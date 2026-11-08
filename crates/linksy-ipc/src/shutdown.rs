//! Ending a typing connection in order, without losing what is still queued: what was typed
//! is sent, the keys still down are let go, our side of the connection is half-closed, the
//! service closes its side once it has passed everything on, and only then does the
//! window end the session (which stops relaying at once). The window follows the actions
//! this says; it only supplies the facts.
//!
//! The fallback is tied to progress: the window ticks every few seconds ([`Event::TimedOut`]),
//! and a tick lets the wait go on if the writer got somewhere since the last one, up to
//! [`MAX_EXTENSIONS`] times. A writer or a service that gets nowhere is cut off at the first
//! tick without progress.

/// How many ticks the wait goes on past the first, at most, while the writer makes progress.
pub const MAX_EXTENSIONS: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shutdown {
    Open,
    /// The writer is working through its queue, then the release, then closing its side;
    /// then the service passes it on and closes its own.
    Draining { writer_done: bool, progress: bool, extended: u8 },
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The window is done with the connection. `writer_alive`: it is good and the writer can
    /// still take the rest of the queue; otherwise nothing more can be sent.
    Begin { writer_alive: bool },
    /// The writer got somewhere (a job went out).
    Progress,
    /// The writer is finished. `ok`: it sent everything and half-closed our side; otherwise a
    /// write failed and the connection was cut.
    Drained { ok: bool },
    /// The service closed its side: it has passed everything on, or the phone ended the session.
    ServiceClosed,
    /// The tick of the fallback clock.
    TimedOut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Hand the writer the keys to let go of, with their deadline starting now.
    QueueRelease,
    /// Let go of the queue: the writer finishes what is in it and half-closes.
    CloseQueue,
    /// Start the clock for the next [`Event::TimedOut`].
    ArmTimeout,
    /// Cut the connection now.
    ShutSocket,
    /// Tell the service the session is over.
    EndSession,
}

const CUT: [Action; 2] = [Action::ShutSocket, Action::EndSession];

impl Shutdown {
    pub fn next(self, event: Event) -> (Shutdown, Vec<Action>) {
        use Shutdown::{Done, Draining, Open};
        match (self, event) {
            (Open, Event::Begin { writer_alive: true }) => (Draining { writer_done: false, progress: false, extended: 0 }, vec![Action::QueueRelease, Action::CloseQueue, Action::ArmTimeout]),
            (Open, Event::Begin { writer_alive: false }) => (Done, CUT.to_vec()),
            (Draining { writer_done, extended, .. }, Event::Progress) => (Draining { writer_done, progress: true, extended }, Vec::new()),
            // finishing is progress too: the service gets a full period to pass it on
            (Draining { extended, .. }, Event::Drained { ok: true }) => (Draining { writer_done: true, progress: true, extended }, Vec::new()),
            (Draining { .. }, Event::Drained { ok: false }) => (Done, CUT.to_vec()),
            // everything is written and passed on
            (Draining { writer_done: true, .. }, Event::ServiceClosed) => (Done, vec![Action::EndSession]),
            // the service closed before our end did: the phone ended the session
            (Draining { writer_done: false, .. }, Event::ServiceClosed) => (Done, CUT.to_vec()),
            (Draining { writer_done, progress: true, extended }, Event::TimedOut) if extended < MAX_EXTENSIONS => (Draining { writer_done, progress: false, extended: extended + 1 }, vec![Action::ArmTimeout]),
            (Draining { .. }, Event::TimedOut) => (Done, CUT.to_vec()),
            (state, _) => (state, Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    const BEGIN: Event = Event::Begin { writer_alive: true };
    const DONE_OK: Event = Event::Drained { ok: true };

    fn draining(writer_done: bool, progress: bool, extended: u8) -> Shutdown {
        Shutdown::Draining { writer_done, progress, extended }
    }

    fn begun() -> Shutdown {
        Shutdown::Open.next(BEGIN).0
    }

    #[test]
    fn beginning_hands_over_the_release_lets_go_of_the_queue_and_arms_the_clock() {
        assert_eq!(Shutdown::Open.next(BEGIN), (draining(false, false, 0), vec![QueueRelease, CloseQueue, ArmTimeout]));
    }

    #[test]
    fn drained_but_the_service_not_closed_does_not_end_the_session() {
        let (s, acts) = begun().next(DONE_OK);
        assert_eq!((s, acts), (draining(true, true, 0), Vec::new()), "finishing is progress, and nothing else happens");
    }

    #[test]
    fn the_session_ends_when_the_service_has_closed_after_the_writer_drained() {
        let (s, _) = begun().next(DONE_OK);
        assert_eq!(s.next(Event::ServiceClosed), (Shutdown::Done, vec![EndSession]));
    }

    #[test]
    fn the_service_closing_before_the_writer_drained_is_the_phone_ending_it() {
        assert_eq!(begun().next(Event::ServiceClosed), (Shutdown::Done, vec![ShutSocket, EndSession]));
    }

    #[test]
    fn a_writer_that_failed_cuts_the_connection_and_ends_the_session() {
        assert_eq!(begun().next(Event::Drained { ok: false }), (Shutdown::Done, vec![ShutSocket, EndSession]));
    }

    #[test]
    fn a_wait_without_progress_is_cut_off_at_the_tick() {
        assert_eq!(begun().next(Event::TimedOut), (Shutdown::Done, vec![ShutSocket, EndSession]));
        // the service never closes: it gets one full period after the writer finished, no more
        let (waiting, _) = begun().next(DONE_OK);
        let (waiting, _) = waiting.next(Event::TimedOut);
        assert_eq!(waiting.next(Event::TimedOut), (Shutdown::Done, vec![ShutSocket, EndSession]));
    }

    #[test]
    fn progress_since_the_last_tick_lets_the_wait_go_on() {
        let (s, acts) = begun().next(Event::Progress);
        assert_eq!((s, acts), (draining(false, true, 0), Vec::new()));
        assert_eq!(s.next(Event::TimedOut), (draining(false, false, 1), vec![ArmTimeout]));
    }

    #[test]
    fn the_writer_finishing_counts_as_progress_so_the_service_gets_a_full_period() {
        let (s, _) = begun().next(Event::Progress).0.next(DONE_OK);
        assert_eq!(s.next(Event::TimedOut), (draining(true, false, 1), vec![ArmTimeout]));
    }

    #[test]
    fn the_wait_has_a_hard_cap_however_much_progress_there_is() {
        let mut s = begun();
        for n in 0..MAX_EXTENSIONS {
            s = s.next(Event::Progress).0;
            let (next, acts) = s.next(Event::TimedOut);
            assert_eq!((next, acts), (draining(false, false, n + 1), vec![ArmTimeout]), "tick {n}");
            s = next;
        }
        s = s.next(Event::Progress).0;
        assert_eq!(s.next(Event::TimedOut), (Shutdown::Done, vec![ShutSocket, EndSession]));
    }

    #[test]
    fn a_connection_that_cannot_send_more_is_cut_and_the_session_ends_at_once() {
        assert_eq!(Shutdown::Open.next(Event::Begin { writer_alive: false }), (Shutdown::Done, vec![ShutSocket, EndSession]));
    }

    #[test]
    fn the_session_ends_once_and_late_events_do_nothing() {
        for late in [Event::Drained { ok: true }, Event::Drained { ok: false }, Event::ServiceClosed, Event::Progress, Event::TimedOut, BEGIN, Event::Begin { writer_alive: false }] {
            assert_eq!(Shutdown::Done.next(late), (Shutdown::Done, Vec::new()), "{late:?}");
        }
    }

    #[test]
    fn beginning_again_while_draining_changes_nothing() {
        let s = begun().next(Event::Progress).0;
        assert_eq!(s.next(BEGIN), (s, Vec::new()));
        assert_eq!(s.next(Event::Begin { writer_alive: false }), (s, Vec::new()));
    }

    #[test]
    fn nothing_happens_before_the_window_begins() {
        for early in [DONE_OK, Event::ServiceClosed, Event::Progress, Event::TimedOut] {
            assert_eq!(Shutdown::Open.next(early), (Shutdown::Open, Vec::new()), "{early:?}");
        }
    }
}
