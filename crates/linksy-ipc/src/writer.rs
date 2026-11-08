//! The thread that writes a typing session's frames to the service, so a window never waits
//! on a socket. Jobs go out in order. When the window lets go of the queue, what is still in
//! it is sent first, then the parting frames (the keys to let go of), and only then does the
//! connection close: nothing typed is lost to a window that is closing.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::timed::{DeadlineWrite, write_all_by};

/// Work for the writer.
pub enum Job {
    /// Frames to send, each within the writer's write timeout.
    Write(Vec<Vec<u8>>),
    /// Frames to send before `Instant`, altogether.
    Release(Vec<Vec<u8>>, Instant),
}

/// The frames to send last, after everything queued, and the time they must be sent by.
/// Set once, from any thread, even when the queue is full.
#[derive(Clone, Default)]
pub struct Parting(Arc<Mutex<Option<Last>>>);

/// Frames and the time they must be sent by.
type Last = (Vec<Vec<u8>>, Instant);

impl Parting {
    pub fn set(&self, frames: Vec<Vec<u8>>, deadline: Instant) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((frames, deadline));
    }

    fn take(&self) -> Option<Last> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Starts the writer on `stream`. `on_progress` is called after each job that went out.
/// `on_end(true)` is called once it has sent everything and closed after the sender was
/// dropped; `on_end(false)` if a write failed or ran out of time (the rest is dropped and the
/// connection is cut).
pub fn spawn(stream: UnixStream, queue: usize, write_timeout: Duration, on_progress: impl Fn() + Send + 'static, on_end: impl FnOnce(bool) + Send + 'static) -> (SyncSender<Job>, Parting) {
    let close = |stream: &mut UnixStream, _ok: bool| {
        let _ = stream.shutdown(Shutdown::Both);
    };
    spawn_on(stream, close, queue, write_timeout, on_progress, on_end)
}

/// [`spawn`] on any sink; `close` runs once the writer is finished (`ok`: it wrote it all).
pub fn spawn_on<W: DeadlineWrite + Send + 'static>(
    mut sink: W,
    close: impl FnOnce(&mut W, bool) + Send + 'static,
    queue: usize,
    write_timeout: Duration,
    on_progress: impl Fn() + Send + 'static,
    on_end: impl FnOnce(bool) + Send + 'static,
) -> (SyncSender<Job>, Parting) {
    let (tx, rx) = sync_channel(queue);
    let parting = Parting::default();
    let last = parting.clone();
    std::thread::spawn(move || {
        let ok = write_all_jobs(&mut sink, &rx, write_timeout, &last, &on_progress);
        close(&mut sink, ok);
        on_end(ok);
    });
    (tx, parting)
}

fn send_by<W: DeadlineWrite>(stream: &mut W, frames: &[Vec<u8>], deadline: impl Fn() -> Instant) -> bool {
    frames.iter().all(|f| write_all_by(stream, f, deadline()).is_ok())
}

/// Every job, then (once the sender is gone, which `recv` only says after the queue is
/// empty) the parting frames. False at the first failed write.
fn write_all_jobs<W: DeadlineWrite>(stream: &mut W, jobs: &Receiver<Job>, write_timeout: Duration, parting: &Parting, on_progress: &impl Fn()) -> bool {
    while let Ok(job) = jobs.recv() {
        let sent = match job {
            Job::Write(frames) => send_by(stream, &frames, || Instant::now() + write_timeout),
            Job::Release(frames, deadline) => send_by(stream, &frames, || deadline),
        };
        if !sent {
            return false;
        }
        on_progress();
    }
    parting.take().is_none_or(|(frames, deadline)| send_by(stream, &frames, || deadline))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{Sender, channel};

    fn finished() -> (impl FnOnce(bool) + Send + 'static, std::sync::mpsc::Receiver<bool>) {
        let (tx, rx) = channel();
        (move |ok| tx.send(ok).unwrap(), rx)
    }

    fn read_to_end(mut peer: UnixStream) -> Vec<u8> {
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        let _ = peer.read_to_end(&mut got);
        got
    }

    /// A sink that holds up its first write until the test lets it through, and tells when
    /// it got there: a writer blocked exactly where the test says, whatever a socket's buffers.
    struct Gated {
        entered: Sender<()>,
        open: Option<std::sync::mpsc::Receiver<()>>,
        got: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for Gated {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(open) = self.open.take() {
                self.entered.send(()).unwrap();
                open.recv().unwrap();
            }
            self.got.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl DeadlineWrite for Gated {
        fn set_timeout(&mut self, _: Option<Duration>) -> io::Result<()> {
            Ok(())
        }
    }

    /// A sink that never takes a byte: every write reports the timeout a socket would, and
    /// the limits it was given are noted.
    #[derive(Clone, Default)]
    struct Stalled {
        limits: Arc<Mutex<Vec<Option<Duration>>>>,
        writes: Arc<AtomicUsize>,
    }

    impl Write for Stalled {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.writes.fetch_add(1, Ordering::SeqCst);
            Err(io::ErrorKind::WouldBlock.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl DeadlineWrite for Stalled {
        fn set_timeout(&mut self, limit: Option<Duration>) -> io::Result<()> {
            self.limits.lock().unwrap().push(limit);
            Ok(())
        }
    }

    #[test]
    fn what_is_queued_is_sent_in_order_then_the_parting_frames_then_it_closes() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, parting) = spawn(mine, 2, Duration::from_secs(2), || {}, end);
        tx.send(Job::Write(vec![b"one ".to_vec()])).unwrap();
        tx.send(Job::Write(vec![b"two ".to_vec()])).unwrap();
        parting.set(vec![b"up".to_vec()], Instant::now() + Duration::from_secs(2));
        drop(tx);
        assert_eq!(read_to_end(peer), b"one two up");
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn a_full_queue_does_not_lose_text_when_it_is_let_go() {
        let (entered, blocked) = channel();
        let (let_through, open) = channel();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = Gated { entered, open: Some(open), got: got.clone() };
        let (end, ended) = finished();
        let (tx, parting) = spawn_on(sink, |_, _| {}, 1, Duration::from_secs(2), || {}, end);
        tx.send(Job::Write(vec![b"head ".to_vec()])).unwrap();
        blocked.recv_timeout(Duration::from_secs(5)).unwrap(); // the writer is inside the first write
        tx.send(Job::Write(vec![b"tail ".to_vec()])).unwrap(); // the queue holds it
        assert!(tx.try_send(Job::Write(vec![b"x".to_vec()])).is_err(), "the queue is full");
        parting.set(vec![b"up".to_vec()], Instant::now() + Duration::from_secs(5));
        drop(tx);
        let_through.send(()).unwrap();
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(&*got.lock().unwrap(), b"head tail up", "the text and the release both arrive, in order");
    }

    #[test]
    fn no_parting_frames_is_just_a_close() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, _parting) = spawn(mine, 2, Duration::from_secs(2), || {}, end);
        tx.send(Job::Write(vec![b"hi".to_vec()])).unwrap();
        drop(tx);
        assert_eq!(read_to_end(peer), b"hi");
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn a_finished_writer_closes_only_its_own_side() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let mut reading = mine.try_clone().unwrap();
        let (end, ended) = finished();
        let (tx, _parting) = spawn(mine, 2, Duration::from_secs(2), || {}, end);
        tx.send(Job::Write(vec![b"hi".to_vec()])).unwrap();
        drop(tx);
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
        // the peer sees the end of what was written, and can still answer
        let mut peer = peer;
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = Vec::new();
        peer.read_to_end(&mut got).unwrap();
        assert_eq!(got, b"hi");
        peer.write_all(b"passed on").unwrap();
        drop(peer);
        reading.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut answer = Vec::new();
        reading.read_to_end(&mut answer).unwrap();
        assert_eq!(answer, b"passed on", "our reading half stays open until the peer closes");
    }

    #[test]
    fn a_failed_writer_cuts_both_sides() {
        let closed = Arc::new(Mutex::new(None));
        let note = closed.clone();
        let (end, ended) = finished();
        let (tx, _parting) = spawn_on(Stalled::default(), move |_, ok| *note.lock().unwrap() = Some(ok), 2, Duration::from_secs(2), || {}, end);
        tx.send(Job::Write(vec![b"hi".to_vec()])).unwrap();
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(*closed.lock().unwrap(), Some(false), "close is told it failed, so it cuts everything");
    }

    #[test]
    fn progress_is_reported_after_each_job() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let (tx, _parting) = spawn(mine, 4, Duration::from_secs(2), move || {
            seen.fetch_add(1, Ordering::SeqCst);
        }, end);
        tx.send(Job::Write(vec![b"a".to_vec()])).unwrap();
        tx.send(Job::Release(vec![b"b".to_vec()], Instant::now() + Duration::from_secs(2))).unwrap();
        drop(tx);
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(read_to_end(peer), b"ab");
    }

    #[test]
    fn parting_frames_that_miss_their_deadline_cut_the_connection() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, parting) = spawn(mine, 2, Duration::from_secs(2), || {}, end);
        // stamped when asked for: by the time the writer gets to it, it is late
        parting.set(vec![b"up".to_vec()], Instant::now() - Duration::from_millis(1));
        drop(tx);
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap());
        assert!(read_to_end(peer).is_empty(), "nothing is sent past the deadline");
    }

    #[test]
    fn a_peer_that_never_reads_fails_the_writer_and_the_rest_is_dropped() {
        let sink = Stalled::default();
        let writes = sink.writes.clone();
        let (end, ended) = finished();
        let (tx, _parting) = spawn_on(sink, |_, _| {}, 4, Duration::from_secs(2), || {}, end);
        tx.send(Job::Write(vec![b"first".to_vec()])).unwrap();
        let _ = tx.send(Job::Write(vec![b"second".to_vec()]));
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap());
        assert_eq!(writes.load(Ordering::SeqCst), 1, "nothing is tried after the first failure");
    }

    #[test]
    fn a_release_job_has_its_own_deadline() {
        let sink = Stalled::default();
        let limits = sink.limits.clone();
        let (end, ended) = finished();
        let (tx, _parting) = spawn_on(sink, |_, _| {}, 4, Duration::from_secs(30), || {}, end);
        tx.send(Job::Release(vec![b"up".to_vec()], Instant::now() + Duration::from_millis(200))).unwrap();
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap());
        let limit = limits.lock().unwrap()[0].expect("a limit was set");
        assert!(limit > Duration::ZERO && limit <= Duration::from_millis(200), "the write timeout is 30 s but the release had 200 ms: {limit:?}");
    }

    #[test]
    fn a_write_job_gets_the_write_timeout() {
        let sink = Stalled::default();
        let limits = sink.limits.clone();
        let (end, ended) = finished();
        let (tx, _parting) = spawn_on(sink, |_, _| {}, 4, Duration::from_secs(30), || {}, end);
        tx.send(Job::Write(vec![b"x".to_vec()])).unwrap();
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap());
        let limit = limits.lock().unwrap()[0].expect("a limit was set");
        assert!(limit > Duration::from_secs(20) && limit <= Duration::from_secs(30), "{limit:?}");
    }
}
