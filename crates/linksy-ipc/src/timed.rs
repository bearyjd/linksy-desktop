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
pub fn write_all_by<W: DeadlineWrite>(w: &mut W, buf: &[u8], deadline: Instant) -> io::Result<()> {
    write_all_by_clock(w, buf, deadline, Instant::now)
}

/// [`write_all_by`] with the clock it reads supplied, so a test controls time.
fn write_all_by_clock<W: DeadlineWrite>(w: &mut W, mut buf: &[u8], deadline: Instant, now: impl Fn() -> Instant) -> io::Result<()> {
    while !buf.is_empty() {
        let left = deadline.saturating_duration_since(now());
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
    use std::cell::Cell;
    use std::rc::Rc;

    /// A clock that only moves when told to.
    #[derive(Clone)]
    struct Clock(Rc<Cell<Duration>>, Instant);

    impl Clock {
        fn new() -> Self {
            Clock(Rc::default(), Instant::now())
        }
        fn now(&self) -> Instant {
            self.1 + self.0.get()
        }
    }

    /// Takes `per_call` bytes a call, taking `pause` of the (fake) time each, and notes the
    /// limit it was given.
    struct Slow {
        taken: Vec<u8>,
        limits: Vec<Option<Duration>>,
        per_call: usize,
        pause: Duration,
        clock: Clock,
    }

    impl Write for Slow {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.clock.0.set(self.clock.0.get() + self.pause);
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
        Slow { taken: Vec::new(), limits: Vec::new(), per_call, pause: Duration::from_millis(pause_ms), clock: Clock::new() }
    }

    #[test]
    fn every_partial_write_gets_what_is_left_of_the_deadline() {
        let mut w = slow(1, 2);
        let clock = w.clock.clone();
        write_all_by_clock(&mut w, b"hello", clock.now() + Duration::from_secs(5), || clock.now()).unwrap();
        assert_eq!(w.taken, b"hello");
        assert_eq!(w.limits.len(), 5, "the limit is set before each write");
        assert!(w.limits.windows(2).all(|p| p[1] <= p[0]), "never more than before: {:?}", w.limits);
        assert!(w.limits.iter().all(|l| l.is_some_and(|l| l > Duration::ZERO && l <= Duration::from_secs(5))));
    }

    #[test]
    fn a_peer_taking_a_byte_at_a_time_runs_out_the_whole_deadline() {
        let mut w = slow(1, 10);
        let clock = w.clock.clone();
        let e = write_all_by_clock(&mut w, &[b'x'; 100], clock.now() + Duration::from_millis(60), || clock.now()).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert_eq!(w.taken.len(), 6, "a byte every 10 ms for 60 ms, then out of time");
        assert_eq!(clock.0.get(), Duration::from_millis(60), "it stopped at the deadline, not later");
    }

    #[test]
    fn a_deadline_already_past_writes_nothing() {
        let mut w = slow(10, 0);
        let e = write_all_by(&mut w, b"late", Instant::now() - Duration::from_millis(1)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(w.taken.is_empty());
    }
}
