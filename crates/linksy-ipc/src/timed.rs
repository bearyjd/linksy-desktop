//! Writing with a deadline for the whole write, not for each piece of it: a socket's write
//! timeout starts over with every partial write, so a peer that takes a byte at a time never
//! trips it.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// A sink whose next write can be given a time limit.
pub trait DeadlineWrite: Write {
    fn set_timeout(&mut self, limit: Option<Duration>) -> io::Result<()>;
}

impl DeadlineWrite for UnixStream {
    fn set_timeout(&mut self, limit: Option<Duration>) -> io::Result<()> {
        self.set_write_timeout(limit)
    }
}

/// All of `buf`, or `TimedOut` once `deadline` has passed.
pub fn write_all_by<W: DeadlineWrite>(w: &mut W, mut buf: &[u8], deadline: Instant) -> io::Result<()> {
    while !buf.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out"));
        }
        w.set_timeout(Some(left))?;
        match w.write(buf) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => buf = &buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // what a socket's write timeout reports
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Err(io::Error::new(io::ErrorKind::TimedOut, "write timed out")),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes `per_call` bytes a call, taking `pause` each, and notes the limit it was given.
    struct Slow {
        taken: Vec<u8>,
        limits: Vec<Option<Duration>>,
        per_call: usize,
        pause: Duration,
    }

    impl Write for Slow {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(self.pause);
            let n = buf.len().min(self.per_call);
            self.taken.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl DeadlineWrite for Slow {
        fn set_timeout(&mut self, limit: Option<Duration>) -> io::Result<()> {
            self.limits.push(limit);
            Ok(())
        }
    }

    fn slow(per_call: usize, pause_ms: u64) -> Slow {
        Slow { taken: Vec::new(), limits: Vec::new(), per_call, pause: Duration::from_millis(pause_ms) }
    }

    #[test]
    fn every_partial_write_gets_what_is_left_of_the_deadline() {
        let mut w = slow(1, 2);
        write_all_by(&mut w, b"hello", Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(w.taken, b"hello");
        assert_eq!(w.limits.len(), 5, "the limit is set before each write");
        assert!(w.limits.windows(2).all(|p| p[1] <= p[0]), "never more than before: {:?}", w.limits);
        assert!(w.limits.iter().all(|l| l.is_some_and(|l| l > Duration::ZERO && l <= Duration::from_secs(5))));
    }

    #[test]
    fn a_peer_taking_a_byte_at_a_time_runs_out_the_whole_deadline() {
        let mut w = slow(1, 10);
        let started = Instant::now();
        let e = write_all_by(&mut w, &[b'x'; 100], started + Duration::from_millis(60)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(w.taken.len() < 100, "stopped part way: {}", w.taken.len());
        assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
    }

    #[test]
    fn a_deadline_already_past_writes_nothing() {
        let mut w = slow(10, 0);
        let e = write_all_by(&mut w, b"late", Instant::now() - Duration::from_millis(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(w.taken.is_empty());
    }
}
