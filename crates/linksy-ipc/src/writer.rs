//! The thread that writes a typing session's frames to the service, so a window never waits
//! on a socket. Jobs go out in order. When the window lets go of the queue, what is still in
//! it is sent first, then the parting frames (the keys to let go of), and only then does the
//! connection close: nothing typed is lost to a window that is closing.

use std::os::unix::net::UnixStream;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
pub struct Parting(Arc<Mutex<Option<(Vec<Vec<u8>>, Instant)>>>);

impl Parting {
    pub fn set(&self, frames: Vec<Vec<u8>>, deadline: Instant) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((frames, deadline));
    }

    fn take(&self) -> Option<(Vec<Vec<u8>>, Instant)> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Starts the writer on `stream`. `on_end(true)` is called once it has sent everything and
/// closed after the sender was dropped; `on_end(false)` if a write failed or ran out of
/// time (the rest is dropped and the connection is cut).
pub fn spawn(_stream: UnixStream, queue: usize, _write_timeout: Duration, on_end: impl FnOnce(bool) + Send + 'static) -> (SyncSender<Job>, Parting) {
    let (tx, _rx) = sync_channel(queue);
    std::thread::spawn(move || on_end(true));
    (tx, Parting::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::sync::mpsc::channel;

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

    #[test]
    fn what_is_queued_is_sent_in_order_then_the_parting_frames_then_it_closes() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, parting) = spawn(mine, 2, Duration::from_secs(2), end);
        tx.send(Job::Write(vec![b"one ".to_vec()])).unwrap();
        tx.send(Job::Write(vec![b"two ".to_vec()])).unwrap();
        parting.set(vec![b"up".to_vec()], Instant::now() + Duration::from_secs(2));
        drop(tx);
        assert_eq!(read_to_end(peer), b"one two up");
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn a_full_queue_does_not_lose_text_when_it_is_let_go() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, parting) = spawn(mine, 1, Duration::from_secs(2), end);
        // nobody reads yet: the writer is stuck on the first job, the queue holds the second
        tx.send(Job::Write(vec![vec![b'a'; 400_000]])).unwrap();
        tx.send(Job::Write(vec![b"tail".to_vec()])).unwrap();
        assert!(tx.try_send(Job::Write(vec![b"x".to_vec()])).is_err(), "the queue is full");
        parting.set(vec![b"up".to_vec()], Instant::now() + Duration::from_secs(5));
        drop(tx);
        let got = read_to_end(peer);
        assert!(got.ends_with(b"tailup"), "the text and the release both arrive: {} bytes", got.len());
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn no_parting_frames_is_just_a_close() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, _parting) = spawn(mine, 2, Duration::from_secs(2), end);
        tx.send(Job::Write(vec![b"hi".to_vec()])).unwrap();
        drop(tx);
        assert_eq!(read_to_end(peer), b"hi");
        assert!(ended.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn parting_frames_that_miss_their_deadline_cut_the_connection() {
        let (mine, peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, parting) = spawn(mine, 2, Duration::from_secs(2), end);
        // stamped when asked for: by the time the writer gets to it, it is late
        parting.set(vec![b"up".to_vec()], Instant::now() - Duration::from_millis(1));
        drop(tx);
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap());
        assert!(read_to_end(peer).is_empty(), "nothing is sent past the deadline");
    }

    #[test]
    fn a_peer_that_never_reads_fails_the_writer_and_the_rest_is_dropped() {
        let (mine, _peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, _parting) = spawn(mine, 4, Duration::from_millis(200), end);
        tx.send(Job::Write(vec![vec![b'a'; 4_000_000]])).unwrap();
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap());
    }

    #[test]
    fn a_release_job_has_its_own_deadline() {
        let (mine, _peer) = UnixStream::pair().unwrap();
        let (end, ended) = finished();
        let (tx, _parting) = spawn(mine, 4, Duration::from_secs(30), end);
        tx.send(Job::Release(vec![vec![b'a'; 4_000_000]], Instant::now() + Duration::from_millis(200))).unwrap();
        assert!(!ended.recv_timeout(Duration::from_secs(5)).unwrap(), "the write timeout is 30 s but the release had 200 ms");
    }
}
