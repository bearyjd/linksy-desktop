//! Typing on a phone from this computer (docs/protocol.md §4.26), without any toolkit: which
//! devices to offer it for, what a key press or a paste becomes on the wire, and what to tell
//! the person as the session goes. The GTK window and `linksy type` only wire this up.

use std::time::Duration;

use linksy_core::keyboard::{KEYBOARD_FEATURE, MAX_KEYBOARD_TEXT};
use linksy_core::{ControlMsg, DeviceInfo, DeviceKind};

/// The most a single paste sends (bytes); more is left out. Well under what a phone queues
/// (256 frames of up to [`MAX_KEYBOARD_TEXT`]), so a paste is never dropped half way.
pub const MAX_PASTE: usize = 100 * MAX_KEYBOARD_TEXT;
/// A typing session that ends sooner than this after it began never really started: older
/// phones just close it.
pub const INSTANT_END: Duration = Duration::from_secs(2);

/// Whether to offer "Type on Phone" for `device`: a phone or tablet that announces `keyboard`.
/// Unlike other features, an empty list never counts: older phones say nothing.
pub fn phone_takes_typing(device: &DeviceInfo) -> bool {
    let _ = (device, KEYBOARD_FEATURE, DeviceKind::Phone);
    todo!()
}

/// `text` as `text` messages of at most [`MAX_KEYBOARD_TEXT`] bytes each, split between
/// characters. Line ends become `\n`; other control characters except tab are dropped (the
/// phone would anyway). Nothing for empty text.
pub fn text_messages(text: &str) -> Vec<ControlMsg> {
    let _ = text;
    todo!()
}

/// A paste: [`text_messages`] of the first [`MAX_PASTE`] bytes, and whether some was left out.
pub fn paste_messages(text: &str) -> (Vec<ControlMsg>, bool) {
    let _ = text;
    todo!()
}

/// Modifier keys held while a key goes down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub sup: bool,
}

/// What a key event on the capture window means.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyAction {
    Send(Vec<ControlMsg>),
    /// Read the clipboard and send it as text.
    Paste,
    Ignore,
}

/// `keysym` (X11) pressed or released with `mods` held, and `ch` the character it types, if
/// any. Text is sent on the press only; the input method (compose, IME) delivers its own
/// text, which doesn't come through here.
pub fn map_key(keysym: u32, ch: Option<char>, mods: Mods, pressed: bool) -> KeyAction {
    let _ = (keysym, ch, mods, pressed);
    todo!()
}

/// Why typing on a phone didn't work, for the person.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The phone says it can't (an old Linksy app).
    Unsupported,
    /// Said no, or didn't say: its Linksy Keyboard may be off.
    NotAllowed,
    NoAnswer,
    /// Opened, then closed at once.
    EndedAtOnce,
    /// Anything else, as the service worded it.
    Other(String),
}

/// What the service said when it couldn't start the session.
pub fn start_problem(error: &str) -> Problem {
    let _ = error;
    todo!()
}

/// Where a typing session is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Asking,
    Typing,
    /// Over, by the person or by the phone, after a real start.
    Stopped { by_phone: bool },
    Failed(Problem),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Started,
    StartFailed(String),
    /// The connection closed or a write failed, `after` it started.
    Lost { after: Duration },
    StopPressed,
}

impl Phase {
    pub fn next(self, step: Step) -> Phase {
        let _ = step;
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linksy_core::Fingerprint;

    fn info(kind: DeviceKind, features: &[&str]) -> DeviceInfo {
        DeviceInfo { id: Fingerprint([0; 32]), name: "Pixel".into(), kind, os: "android".into(), features: features.iter().map(|f| f.to_string()).collect(), away: None }
    }

    fn text(t: &str) -> ControlMsg {
        ControlMsg::Text { text: t.into() }
    }

    fn key(keysym: u32, pressed: bool) -> ControlMsg {
        ControlMsg::Key { keysym, pressed, code: None }
    }

    const NONE: Mods = Mods { ctrl: false, alt: false, shift: false, sup: false };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };

    #[test]
    fn only_phones_that_announce_the_keyboard_get_the_action() {
        assert!(phone_takes_typing(&info(DeviceKind::Phone, &["files", "keyboard"])));
        assert!(phone_takes_typing(&info(DeviceKind::Tablet, &["keyboard"])));
        assert!(!phone_takes_typing(&info(DeviceKind::Phone, &["files"])));
        assert!(!phone_takes_typing(&info(DeviceKind::Phone, &[])), "older phones say nothing");
        assert!(!phone_takes_typing(&info(DeviceKind::Laptop, &["keyboard"])), "a computer's keyboard is not typed on");
    }

    #[test]
    fn short_text_is_one_message() {
        assert_eq!(text_messages("héllo"), vec![text("héllo")]);
        assert!(text_messages("").is_empty());
    }

    #[test]
    fn long_text_is_cut_between_characters_within_the_limit() {
        let long = "é".repeat(MAX_KEYBOARD_TEXT); // two bytes each
        let msgs = text_messages(&long);
        assert_eq!(msgs.len(), 2);
        let mut joined = String::new();
        for m in &msgs {
            let ControlMsg::Text { text } = m else { panic!("not text: {m:?}") };
            assert!(text.len() <= MAX_KEYBOARD_TEXT, "{} bytes", text.len());
            joined.push_str(text);
        }
        assert_eq!(joined, long);
    }

    #[test]
    fn line_ends_are_newlines_and_other_controls_go() {
        assert_eq!(text_messages("a\r\nb\rc\x07\td\u{7f}"), vec![text("a\nb\nc\td")]);
        assert!(text_messages("\x07\x1b").is_empty());
    }

    #[test]
    fn a_big_paste_is_cut_and_says_so() {
        let (msgs, cut) = paste_messages(&"a".repeat(MAX_PASTE + 5));
        assert!(cut);
        let total: usize = msgs.iter().map(|m| if let ControlMsg::Text { text } = m { text.len() } else { 0 }).sum();
        assert_eq!(total, MAX_PASTE);
        let (msgs, cut) = paste_messages("short");
        assert!(!cut);
        assert_eq!(msgs, vec![text("short")]);
    }

    #[test]
    fn a_paste_cut_never_splits_a_character() {
        let (msgs, cut) = paste_messages(&"€".repeat(MAX_PASTE)); // three bytes each
        assert!(cut);
        assert!(msgs.iter().all(|m| matches!(m, ControlMsg::Text { text } if text.chars().all(|c| c == '€'))));
    }

    #[test]
    fn printable_keys_are_text_on_press_only() {
        assert_eq!(map_key(0x61, Some('a'), NONE, true), KeyAction::Send(vec![text("a")]));
        assert_eq!(map_key(0x41, Some('A'), Mods { shift: true, ..NONE }, true), KeyAction::Send(vec![text("A")]));
        assert_eq!(map_key(0x61, Some('a'), NONE, false), KeyAction::Ignore);
        assert_eq!(map_key(0xffb5, Some('5'), NONE, true), KeyAction::Send(vec![text("5")]), "keypad digit");
    }

    #[test]
    fn named_keys_go_as_keys_both_ways() {
        for keysym in [0xff0d, 0xff08, 0xff09, 0xff1b, 0xff51, 0xffff, 0xff8d, 0xffbe] {
            let ch = match keysym { 0xff0d | 0xff8d => Some('\r'), 0xff08 => Some('\x08'), 0xff09 => Some('\t'), 0xff1b => Some('\x1b'), _ => None };
            assert_eq!(map_key(keysym, ch, NONE, true), KeyAction::Send(vec![key(keysym, true)]));
            assert_eq!(map_key(keysym, ch, NONE, false), KeyAction::Send(vec![key(keysym, false)]));
        }
        assert_eq!(map_key(0xfe20, None, Mods { shift: true, ..NONE }, true), KeyAction::Send(vec![key(0xff09, true)]), "Shift+Tab is Tab with Shift held");
    }

    #[test]
    fn shift_ctrl_and_alt_go_as_held_keys_but_nothing_else_modifier_like() {
        for keysym in [0xffe1, 0xffe2, 0xffe3, 0xffe4, 0xffe9, 0xffea] {
            assert_eq!(map_key(keysym, None, NONE, true), KeyAction::Send(vec![key(keysym, true)]));
            assert_eq!(map_key(keysym, None, NONE, false), KeyAction::Send(vec![key(keysym, false)]));
        }
        // Caps Lock, Num Lock, Super, Meta, Hyper, AltGr: kept here
        for keysym in [0xffe5, 0xff7f, 0xffeb, 0xffec, 0xffe7, 0xffed, 0xfe03] {
            assert_eq!(map_key(keysym, None, NONE, true), KeyAction::Ignore, "{keysym:#x}");
        }
    }

    #[test]
    fn shortcuts_go_as_keys_not_text() {
        assert_eq!(map_key(0x61, Some('a'), CTRL, true), KeyAction::Send(vec![key(0x61, true)]));
        assert_eq!(map_key(0x61, Some('a'), CTRL, false), KeyAction::Send(vec![key(0x61, false)]));
        assert_eq!(map_key(0x78, Some('x'), Mods { alt: true, ..NONE }, true), KeyAction::Send(vec![key(0x78, true)]));
    }

    #[test]
    fn super_held_sends_nothing() {
        assert_eq!(map_key(0x61, Some('a'), Mods { sup: true, ..NONE }, true), KeyAction::Ignore);
        assert_eq!(map_key(0xff0d, None, Mods { sup: true, ..NONE }, true), KeyAction::Ignore);
    }

    #[test]
    fn ctrl_v_and_shift_insert_paste() {
        assert_eq!(map_key(0x76, Some('v'), CTRL, true), KeyAction::Paste);
        assert_eq!(map_key(0x56, Some('V'), Mods { ctrl: true, shift: true, ..NONE }, true), KeyAction::Paste);
        assert_eq!(map_key(0xff63, None, Mods { shift: true, ..NONE }, true), KeyAction::Paste);
        assert_eq!(map_key(0x76, Some('v'), CTRL, false), KeyAction::Ignore, "the release sends nothing");
        assert_eq!(map_key(0x76, Some('v'), NONE, true), KeyAction::Send(vec![text("v")]));
    }

    #[test]
    fn keys_nothing_maps_are_ignored() {
        assert_eq!(map_key(0x1008ff11, None, NONE, true), KeyAction::Ignore, "media key");
        assert_eq!(map_key(0x61, None, NONE, true), KeyAction::Ignore);
    }

    #[test]
    fn the_services_words_become_problems() {
        assert_eq!(start_problem("the other device can't do this yet"), Problem::Unsupported);
        assert_eq!(start_problem("request was declined"), Problem::NotAllowed);
        assert_eq!(start_problem("timed out waiting for an answer"), Problem::NoAnswer);
        assert_eq!(start_problem("device isn't reachable right now"), Problem::Other("device isn't reachable right now".into()));
    }

    #[test]
    fn a_session_runs_asking_typing_stopped() {
        let p = Phase::Asking.next(Step::Started);
        assert_eq!(p, Phase::Typing);
        assert_eq!(p.clone().next(Step::StopPressed), Phase::Stopped { by_phone: false });
        assert_eq!(p.next(Step::Lost { after: Duration::from_secs(30) }), Phase::Stopped { by_phone: true });
    }

    #[test]
    fn closing_at_once_means_not_supported() {
        let p = Phase::Typing.next(Step::Lost { after: Duration::from_millis(200) });
        assert_eq!(p, Phase::Failed(Problem::EndedAtOnce));
        assert_eq!(Phase::Typing.next(Step::Lost { after: INSTANT_END }), Phase::Stopped { by_phone: true });
    }

    #[test]
    fn failing_to_start_is_a_problem_and_over_stays_over() {
        assert_eq!(Phase::Asking.next(Step::StartFailed("request was declined".into())), Phase::Failed(Problem::NotAllowed));
        assert_eq!(Phase::Asking.next(Step::StopPressed), Phase::Stopped { by_phone: false });
        let over = Phase::Stopped { by_phone: false };
        assert_eq!(over.clone().next(Step::Started), over);
        let failed = Phase::Failed(Problem::NoAnswer);
        assert_eq!(failed.clone().next(Step::StopPressed), failed);
    }
}
