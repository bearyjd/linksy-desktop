//! Ending a typing connection in order, without losing what is still queued: what was typed
//! is sent, the keys still down are let go, the connection closes, and only then does the
//! service end the session (which stops relaying at once). The window follows the
//! actions this says; it only supplies the facts.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shutdown {
    Open,
    /// The writer is working through its queue, then the release, then closing.
    Draining,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The window is done with the connection. `writer_alive`: it is good and the writer can
    /// still take the rest of the queue; otherwise nothing more can be sent.
    Begin { writer_alive: bool },
    /// The writer has sent everything and closed.
    Drained,
    /// It took too long.
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Hand the writer the keys to let go of, with their deadline starting now.
    QueueRelease,
    /// Let go of the queue: the writer finishes what is in it and closes.
    CloseQueue,
    /// Start the clock for [`Event::TimedOut`].
    ArmTimeout,
    /// Cut the connection now.
    ShutSocket,
    /// Tell the service the session is over.
    EndSession,
}

impl Shutdown {
    pub fn next(self, _event: Event) -> (Shutdown, Vec<Action>) {
        (self, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    #[test]
    fn a_good_connection_drains_before_the_session_ends() {
        let (s, acts) = Shutdown::Open.next(Event::Begin { writer_alive: true });
        assert_eq!((s, acts), (Shutdown::Draining, vec![QueueRelease, CloseQueue, ArmTimeout]));
        let (s, acts) = s.next(Event::Drained);
        assert_eq!((s, acts), (Shutdown::Done, vec![EndSession]));
    }

    #[test]
    fn the_session_does_not_end_while_the_writer_is_still_sending() {
        let (s, _) = Shutdown::Open.next(Event::Begin { writer_alive: true });
        assert!(!s.next(Event::Begin { writer_alive: true }).1.contains(&EndSession));
    }

    #[test]
    fn a_writer_that_never_finishes_is_cut_off_and_the_session_ends() {
        let (s, _) = Shutdown::Open.next(Event::Begin { writer_alive: true });
        assert_eq!(s.next(Event::TimedOut), (Shutdown::Done, vec![ShutSocket, EndSession]));
    }

    #[test]
    fn a_connection_that_cannot_send_more_is_cut_and_the_session_ends_at_once() {
        assert_eq!(Shutdown::Open.next(Event::Begin { writer_alive: false }), (Shutdown::Done, vec![ShutSocket, EndSession]));
    }

    #[test]
    fn the_session_ends_once() {
        for late in [Event::Drained, Event::TimedOut, Event::Begin { writer_alive: true }, Event::Begin { writer_alive: false }] {
            assert_eq!(Shutdown::Done.next(late), (Shutdown::Done, Vec::new()), "{late:?}");
        }
    }

    #[test]
    fn nothing_happens_before_the_window_begins() {
        assert_eq!(Shutdown::Open.next(Event::Drained), (Shutdown::Open, Vec::new()));
        assert_eq!(Shutdown::Open.next(Event::TimedOut), (Shutdown::Open, Vec::new()));
    }
}
