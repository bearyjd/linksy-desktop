//! Typing on a phone from this computer (docs/protocol.md §4.26), without any toolkit: which
//! devices to offer it for, what a key press or a paste becomes on the wire, and what to tell
//! the person as the session goes. The GTK window and `linksy type` only wire this up.

use std::io::Read;
use std::time::Duration;

use linksy_core::keyboard::{KEYBOARD_FEATURE, MAX_KEYBOARD_TEXT};
use linksy_core::{ControlMsg, DeviceInfo, DeviceKind, DeviceView, Fingerprint};

/// The most a single paste sends (bytes); more is left out. Well under what a phone queues
/// (256 frames of up to [`MAX_KEYBOARD_TEXT`]), so a paste is never dropped half way.
pub const MAX_PASTE: usize = 100 * MAX_KEYBOARD_TEXT;
/// A typing session that ends sooner than this after it began never really started: older
/// phones just close it.
pub const INSTANT_END: Duration = Duration::from_secs(2);

/// Whether to offer "Type on Phone" for `device`: a phone or tablet that announces `keyboard`.
/// Unlike other features, an empty list never counts: older phones say nothing.
pub fn phone_takes_typing(device: &DeviceInfo) -> bool {
    matches!(device.kind, DeviceKind::Phone | DeviceKind::Tablet) && device.features.iter().any(|f| f == KEYBOARD_FEATURE)
}

/// `text` as `text` messages of at most [`MAX_KEYBOARD_TEXT`] bytes each, split between
/// characters. Line ends become `\n`; other control characters except tab are dropped (the
/// phone would anyway). Nothing for empty text.
pub fn text_messages(text: &str) -> Vec<ControlMsg> {
    let mut clean = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                chars.next_if_eq(&'\n');
                clean.push('\n');
            }
            '\n' | '\t' => clean.push(c),
            c if c.is_control() => {}
            c => clean.push(c),
        }
    }
    let mut out = Vec::new();
    let mut rest = clean.as_str();
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_KEYBOARD_TEXT);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (head, tail) = rest.split_at(end);
        out.push(ControlMsg::Text { text: head.to_string() });
        rest = tail;
    }
    out
}

/// A paste: [`text_messages`] of the first [`MAX_PASTE`] bytes, and whether some was left out.
pub fn paste_messages(text: &str) -> (Vec<ControlMsg>, bool) {
    let mut end = text.len().min(MAX_PASTE);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text_messages(&text[..end]), end < text.len())
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
    /// Alt+F4: close the typing window instead of typing it on the phone.
    Close,
    Ignore,
}

/// `keysym` (X11) pressed or released with `mods` held, and `ch` the character it types, if
/// any. Text is sent on the press only; the input method (compose, IME) delivers its own
/// text, which doesn't come through here.
pub fn map_key(keysym: u32, ch: Option<char>, mods: Mods, pressed: bool) -> KeyAction {
    const SHIFT_L: u32 = 0xffe1;
    const SHIFT_R: u32 = 0xffe2;
    const CONTROL_L: u32 = 0xffe3;
    const CONTROL_R: u32 = 0xffe4;
    const ALT_L: u32 = 0xffe9;
    const ALT_R: u32 = 0xffea;
    const TAB: u32 = 0xff09;
    const ISO_LEFT_TAB: u32 = 0xfe20;
    const INSERT: u32 = 0xff63;
    const F4: u32 = 0xffc1;
    let key = |keysym| KeyAction::Send(vec![ControlMsg::Key { keysym, pressed, code: None }]);
    if mods.sup {
        return KeyAction::Ignore;
    }
    if keysym == F4 && mods.alt {
        return if pressed { KeyAction::Close } else { KeyAction::Ignore };
    }
    match keysym {
        SHIFT_L | SHIFT_R | CONTROL_L | CONTROL_R | ALT_L | ALT_R => return key(keysym),
        // Caps/Num Lock, Meta, Super, Hyper, mode switch, AltGr: not sent
        0xff7e | 0xff7f | 0xffe5..=0xffee | 0xfe03 => return KeyAction::Ignore,
        ISO_LEFT_TAB => return key(TAB),
        _ => {}
    }
    let paste = (mods.ctrl && !mods.alt && matches!(keysym, 0x76 | 0x56)) || (mods.shift && !mods.ctrl && !mods.alt && keysym == INSERT);
    if paste {
        return if pressed { KeyAction::Paste } else { KeyAction::Ignore };
    }
    if mods.ctrl || mods.alt {
        return key(keysym);
    }
    match ch {
        Some(c) if !c.is_control() => {
            if pressed { KeyAction::Send(vec![ControlMsg::Text { text: c.to_string() }]) } else { KeyAction::Ignore }
        }
        _ if (0xff00..=0xffff).contains(&keysym) => key(keysym),
        _ => KeyAction::Ignore,
    }
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
    if error.contains("can't do this yet") {
        Problem::Unsupported
    } else if error.contains("declined") {
        Problem::NotAllowed
    } else if error.contains("timed out") {
        Problem::NoAnswer
    } else {
        Problem::Other(error.to_string())
    }
}

/// Where a typing session is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Asking,
    Typing,
    /// Over, by the person or by the phone, after a real start.
    Stopped { by_phone: bool },
    Failed(Problem),
    /// A write to the phone failed after a real start: the connection is gone.
    Disconnected,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Started,
    StartFailed(String),
    /// The connection closed or a write failed, `after` it started.
    Lost { after: Duration },
    /// Sending to the phone failed, `after` it started.
    WriteFailed { after: Duration },
    StopPressed,
}

impl Phase {
    pub fn next(self, step: Step) -> Phase {
        match (self, step) {
            (Phase::Asking, Step::Started) => Phase::Typing,
            (Phase::Asking, Step::StartFailed(error)) => Phase::Failed(start_problem(&error)),
            (Phase::Asking | Phase::Typing, Step::StopPressed) => Phase::Stopped { by_phone: false },
            (Phase::Asking | Phase::Typing, Step::Lost { after }) if after < INSTANT_END => Phase::Failed(Problem::EndedAtOnce),
            (Phase::Typing, Step::Lost { .. }) => Phase::Stopped { by_phone: true },
            (Phase::Typing, Step::WriteFailed { after }) if after < INSTANT_END => Phase::Failed(Problem::EndedAtOnce),
            (Phase::Typing, Step::WriteFailed { .. }) => Phase::Disconnected,
            (phase, _) => phase,
        }
    }
}

/// Which keys the phone has been told are down, by physical key code. A release is always
/// the opposite of what that key's press sent, whatever is held by then, so no key stays
/// down on the phone when Ctrl or Super went up first.
#[derive(Clone, Debug, Default)]
pub struct KeyTracker {
    /// (physical key code, keysym sent as pressed)
    down: Vec<(u32, u32)>,
}

const SHIFTS: [u32; 2] = [0xffe1, 0xffe2];
const CONTROLS: [u32; 2] = [0xffe3, 0xffe4];
const ALTS: [u32; 2] = [0xffe9, 0xffea];

fn key_up(keysym: u32) -> ControlMsg {
    ControlMsg::Key { keysym, pressed: false, code: None }
}

fn is_modifier(keysym: u32) -> bool {
    modifier_class(keysym).is_some()
}

/// Shift, Ctrl and Alt are 0, 1 and 2; either side of the keyboard is the same class.
fn modifier_class(keysym: u32) -> Option<u8> {
    [&SHIFTS, &CONTROLS, &ALTS].iter().position(|keys| keys.contains(&keysym)).map(|i| i as u8)
}

/// Marks a tracked press the phone was sent on behalf of another key, not a key of its own.
const SYNTHETIC: u32 = 1 << 31;

fn is_synthetic(code: u32) -> bool {
    code & SYNTHETIC != 0
}

impl KeyTracker {
    /// A key went down: what to send, remembering which keys the phone now holds. A key
    /// that repeats under a different keysym (Shift joined while it was held) lets go of the
    /// first one before the new one goes down, so nothing is stranded. A key sent with a
    /// modifier the phone hasn't been told about (it was held before the window had focus, or
    /// before the session began) gets that modifier's press first.
    pub fn press(&mut self, code: u32, keysym: u32, ch: Option<char>, mods: Mods) -> KeyAction {
        let action = map_key(keysym, ch, mods, true);
        let KeyAction::Send(msgs) = action else { return action };
        let mut out = Vec::with_capacity(msgs.len());
        for msg in msgs {
            if let ControlMsg::Key { keysym, pressed: true, .. } = &msg {
                if let Some(i) = self.down.iter().position(|(c, _)| *c == code) {
                    let old = self.down.remove(i).1;
                    if old != *keysym {
                        out.push(key_up(old));
                    }
                }
                match modifier_class(*keysym) {
                    // the key itself takes over from one sent on its behalf
                    Some(class) => self.down.retain(|(c, k)| !(is_synthetic(*c) && modifier_class(*k) == Some(class))),
                    None => out.extend(self.missing_modifiers(mods)),
                }
                self.down.push((code, *keysym));
            }
            out.push(msg);
        }
        KeyAction::Send(out)
    }

    /// Presses for the modifiers in `mods` the phone holds none of, remembered as held.
    fn missing_modifiers(&mut self, mods: Mods) -> Vec<ControlMsg> {
        let mut out = Vec::new();
        for (held, class, keysym) in [(mods.ctrl, 1u32, CONTROLS[0]), (mods.alt, 2, ALTS[0]), (mods.shift, 0, SHIFTS[0])] {
            if held && !self.down.iter().any(|(_, k)| modifier_class(*k) == Some(class as u8)) {
                self.down.push((SYNTHETIC | class, keysym));
                out.push(ControlMsg::Key { keysym, pressed: true, code: None });
            }
        }
        out
    }

    /// A key came up: let go of what its press sent, if anything, however the modifiers are now.
    pub fn release(&mut self, code: u32) -> Vec<ControlMsg> {
        match self.down.iter().position(|(c, _)| *c == code) {
            Some(i) => vec![key_up(self.down.remove(i).1)],
            None => Vec::new(),
        }
    }

    /// Another key's event shows which modifiers are really held (`mods`): let go of any the
    /// phone holds that aren't. An input method can swallow a modifier's release (Ctrl+Shift+U),
    /// so nothing else would. `code` is the key the event is for, whose own state lags.
    pub fn release_stale(&mut self, code: u32, mods: Mods) -> Vec<ControlMsg> {
        let stale = |k: u32| (SHIFTS.contains(&k) && !mods.shift) || (CONTROLS.contains(&k) && !mods.ctrl) || (ALTS.contains(&k) && !mods.alt);
        let (gone, kept): (Vec<_>, Vec<_>) = self.down.iter().partition(|(c, k)| *c != code && stale(*k));
        let up = gone.iter().map(|(_, k)| key_up(*k)).collect();
        self.down = kept;
        up
    }

        /// A key came up and its keysym is `keysym`: like [`release`](Self::release), and a
    /// modifier sent on its behalf (it was held before the window saw it) goes up too.
    pub fn release_event(&mut self, code: u32, keysym: u32) -> Vec<ControlMsg> {
        let up = self.release(code);
        let Some(class) = modifier_class(keysym).filter(|_| up.is_empty()) else { return up };
        let (gone, kept): (Vec<_>, Vec<_>) = self.down.iter().partition(|(c, k)| is_synthetic(*c) && modifier_class(*k) == Some(class));
        let up = gone.iter().map(|(_, k)| key_up(*k)).collect();
        self.down = kept;
        up
    }

    /// Everything the phone holds, let go (focus left, the session ends).
    pub fn release_all(&mut self) -> Vec<ControlMsg> {
        self.down.drain(..).map(|(_, k)| key_up(k)).collect()
    }

    /// `text` as pasted: the phone sees no modifier held while it arrives, and gets the ones
    /// still held back afterwards.
    pub fn around_paste(&self, text: Vec<ControlMsg>) -> Vec<ControlMsg> {
        let held: Vec<u32> = self.down.iter().map(|(_, k)| *k).filter(|k| is_modifier(*k)).collect();
        let mut out: Vec<ControlMsg> = held.iter().map(|k| key_up(*k)).collect();
        out.extend(text);
        out.extend(held.iter().map(|k| ControlMsg::Key { keysym: *k, pressed: true, code: None }));
        out
    }
}

/// Why `linksy type` can't use a target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetError {
    /// No such device, or several (the service's words).
    Resolve(String),
    /// It isn't a phone or tablet that announces `keyboard`.
    NotTyping,
}

/// The one device `target` names, if it takes typing. A fingerprint nobody has announced is
/// left to the service, which can't reach it either.
pub fn check_target(devices: &[DeviceView], target: &str) -> Result<Fingerprint, TargetError> {
    let id = crate::resolve::resolve_device(devices, target).map_err(TargetError::Resolve)?;
    match devices.iter().find(|d| d.device.id == id) {
        Some(d) if !phone_takes_typing(&d.device) => Err(TargetError::NotTyping),
        _ => Ok(id),
    }
}

/// All of `reader` as text, at most [`MAX_PASTE`] bytes: more is an error, not a cut, and
/// never read in full.
pub fn read_bounded(reader: impl Read) -> Result<String, String> {
    let mut buf = Vec::new();
    reader.take(MAX_PASTE as u64 + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() > MAX_PASTE {
        return Err(format!("That is over {} KiB. Type less at a time.", MAX_PASTE / 1024));
    }
    String::from_utf8(buf).map_err(|_| "The input isn't text.".to_string())
}

/// Where the phone closed the session in a `linksy type` run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Closed {
    NotYet,
    /// Before all the text was written and flushed: it never took it.
    WhileWriting,
    /// After: `after` the session began (the text was on its way either way).
    AfterWriting { after: Duration },
}

/// Whether a run is a failure given when the phone closed the session: closing before the
/// text was written means it never took it (the same as in the window); closing after is not
/// a failure, or people retype what was already typed.
pub fn delivery_outcome(closed: Closed, instant_end: Duration) -> Result<(), Problem> {
    let _ = instant_end;
    match closed {
        Closed::WhileWriting => Err(Problem::EndedAtOnce),
        Closed::NotYet | Closed::AfterWriting { .. } => Ok(()),
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
        let lens: Vec<usize> = msgs.iter().map(|m| if let ControlMsg::Text { text } = m { text.len() } else { 0 }).collect();
        assert_eq!(lens, vec![MAX_KEYBOARD_TEXT, MAX_KEYBOARD_TEXT], "cut exactly at the limit, on a character boundary");
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

    const SUPER: Mods = Mods { sup: true, ..NONE };

    fn sent(a: KeyAction) -> Vec<ControlMsg> {
        match a {
            KeyAction::Send(m) => m,
            other => panic!("not sent: {other:?}"),
        }
    }

    #[test]
    fn a_key_pressed_under_ctrl_is_let_go_even_if_ctrl_went_first() {
        let mut k = KeyTracker::default();
        assert_eq!(sent(k.press(37, 0xffe3, None, NONE)), vec![key(0xffe3, true)]);
        assert_eq!(sent(k.press(54, 0x63, Some('c'), CTRL)), vec![key(0x63, true)]);
        assert_eq!(k.release(37), vec![key(0xffe3, false)]);
        assert_eq!(k.release(54), vec![key(0x63, false)], "c must come up although Ctrl is already up");
        assert!(k.release_all().is_empty());
    }

    #[test]
    fn the_release_uses_the_keysym_of_the_press_not_the_current_one() {
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        assert_eq!(sent(k.press(38, 0x61, Some('a'), CTRL)), vec![key(0x61, true)]);
        // the layout or Shift changed meanwhile; the release reports a different keysym
        assert_eq!(k.release(38), vec![key(0x61, false)]);
    }

    #[test]
    fn super_pressed_after_the_key_does_not_strand_it() {
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        k.press(54, 0x63, Some('c'), CTRL);
        // Super goes down while c is held, and c repeats with Super in the state
        let with_super = Mods { ctrl: true, sup: true, ..NONE };
        assert_eq!(k.press(133, 0xffeb, None, CTRL), KeyAction::Ignore, "Super itself is not sent");
        assert_eq!(k.press(54, 0x63, Some('c'), with_super), KeyAction::Ignore, "nothing is sent with Super held");
        assert_eq!(k.release(133), Vec::new());
        assert_eq!(k.release(54), vec![key(0x63, false)], "c comes up although Super was in the way");
        assert_eq!(k.release(37), vec![key(0xffe3, false)]);
        assert!(k.release_all().is_empty());
    }

    #[test]
    fn a_repeat_with_another_keysym_lets_go_of_the_first() {
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        assert_eq!(sent(k.press(38, 0x61, Some('a'), CTRL)), vec![key(0x61, true)]);
        k.press(50, 0xffe1, None, CTRL);
        // Shift joined while a repeats: the same physical key now reports A
        let both = Mods { ctrl: true, shift: true, ..NONE };
        assert_eq!(sent(k.press(38, 0x41, Some('A'), both)), vec![key(0x61, false), key(0x41, true)]);
        assert_eq!(k.release(38), vec![key(0x41, false)]);
        let mut rest = k.release_all();
        rest.sort_by_key(|m| if let ControlMsg::Key { keysym, .. } = m { *keysym } else { 0 });
        assert_eq!(rest, vec![key(0xffe1, false), key(0xffe3, false)], "a is not held any more");
    }

    #[test]
    fn release_all_covers_a_key_whose_keysym_changed() {
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        k.press(38, 0x61, Some('a'), CTRL);
        k.press(38, 0x41, Some('A'), Mods { ctrl: true, shift: true, ..NONE });
        let up = k.release_all();
        assert!(up.contains(&key(0x41, false)) && !up.contains(&key(0x61, false)), "{up:?}");
    }

    #[test]
    fn a_modifier_held_before_the_window_saw_it_is_sent_before_the_key() {
        // focus came back, or the session began, with Ctrl already down
        let mut k = KeyTracker::default();
        assert_eq!(sent(k.press(54, 0x61, Some('a'), CTRL)), vec![key(0xffe3, true), key(0x61, true)]);
        assert_eq!(sent(k.press(55, 0x62, Some('b'), CTRL)), vec![key(0x62, true)], "Ctrl is down on the phone now");
        // it comes up when the key does, or at the next event if the window never sees that
        assert_eq!(k.release_event(37, 0xffe3), vec![key(0xffe3, false)]);
        assert!(k.release_stale(60, NONE).is_empty());
    }

    #[test]
    fn missing_modifiers_come_in_a_fixed_order_and_only_for_keys() {
        let mut k = KeyTracker::default();
        let all = Mods { ctrl: true, alt: true, shift: true, sup: false };
        assert_eq!(sent(k.press(23, 0xff09, None, all)), vec![key(0xffe3, true), key(0xffe9, true), key(0xffe1, true), key(0xff09, true)]);
        let mut k = KeyTracker::default();
        assert_eq!(sent(k.press(23, 0xfe20, None, Mods { shift: true, ..NONE })), vec![key(0xffe1, true), key(0xff09, true)], "Shift+Tab");
        let mut k = KeyTracker::default();
        assert_eq!(sent(k.press(50, 0xffe1, None, CTRL)), vec![key(0xffe1, true)], "a modifier key is only itself");
        assert_eq!(sent(k.press(38, 0x41, Some('A'), Mods { shift: true, ..NONE })), vec![text("A")], "text carries its own Shift");
    }

    #[test]
    fn a_modifier_the_window_does_see_is_not_sent_twice() {
        let mut k = KeyTracker::default();
        k.press(54, 0x61, Some('a'), CTRL); // Control_L sent on its behalf
        assert!(sent(k.press(37, 0xffe3, None, CTRL)).is_empty(), "the same keysym is handed over, not pressed again");
        assert_eq!(k.release(37), vec![key(0xffe3, false)]);
        let mut rest = k.release_all();
        rest.sort_by_key(|m| if let ControlMsg::Key { keysym, .. } = m { *keysym } else { 0 });
        assert_eq!(rest, vec![key(0x61, false)], "one Ctrl, and it is up");
    }

    #[test]
    fn the_other_side_of_a_modifier_sent_on_a_keys_behalf_lets_go_of_that_one_first() {
        // Left Ctrl was held before the window saw it; then Right Ctrl goes down
        let mut k = KeyTracker::default();
        k.press(54, 0x61, Some('a'), CTRL); // Control_L sent on its behalf
        assert_eq!(sent(k.press(105, 0xffe4, None, CTRL)), vec![key(0xffe3, false), key(0xffe4, true)]);
        assert_eq!(k.release(105), vec![key(0xffe4, false)]);
        let mut rest = k.release_all();
        rest.sort_by_key(|m| if let ControlMsg::Key { keysym, .. } = m { *keysym } else { 0 });
        assert_eq!(rest, vec![key(0x61, false)], "Control_L is not left down");
    }

    #[test]
    fn release_all_after_a_mix_of_sides_covers_everything() {
        let mut k = KeyTracker::default();
        k.press(54, 0x61, Some('a'), CTRL);
        k.press(105, 0xffe4, None, CTRL);
        let mut up = k.release_all();
        up.sort_by_key(|m| if let ControlMsg::Key { keysym, .. } = m { *keysym } else { 0 });
        assert_eq!(up, vec![key(0x61, false), key(0xffe4, false)], "Control_L went up when Right Ctrl took over");
    }

    #[test]
    fn a_key_the_phone_never_saw_is_not_released() {
        let mut k = KeyTracker::default();
        assert_eq!(k.press(54, 0x63, Some('c'), SUPER), KeyAction::Ignore);
        assert!(k.release(54).is_empty(), "Super+c was never sent");
        assert!(k.release(99).is_empty(), "unknown key");
        k.press(55, 0x61, Some('a'), NONE);
        assert!(k.release(55).is_empty(), "text keys have no key down on the phone");
    }

    #[test]
    fn a_repeated_press_is_one_release() {
        let mut k = KeyTracker::default();
        for _ in 0..3 {
            assert_eq!(sent(k.press(113, 0xff51, None, NONE)), vec![key(0xff51, true)]);
        }
        assert_eq!(k.release(113), vec![key(0xff51, false)]);
        assert!(k.release(113).is_empty());
    }

    #[test]
    fn losing_focus_lets_everything_go() {
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        k.press(50, 0xffe1, None, CTRL);
        k.press(54, 0x63, Some('c'), Mods { ctrl: true, shift: true, ..NONE });
        let mut up = k.release_all();
        up.sort_by_key(|m| if let ControlMsg::Key { keysym, .. } = m { *keysym } else { 0 });
        assert_eq!(up, vec![key(0x63, false), key(0xffe1, false), key(0xffe3, false)]);
        assert!(k.release_all().is_empty());
        assert!(k.release(54).is_empty());
    }

    #[test]
    fn a_modifier_the_window_never_saw_released_is_let_go_on_the_next_key() {
        // Ctrl+Shift+U in an input method can eat the releases
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        k.press(50, 0xffe1, None, CTRL);
        assert!(k.release_stale(60, Mods { ctrl: true, shift: true, ..NONE }).is_empty(), "both still held");
        assert_eq!(k.release_stale(61, Mods { ctrl: true, ..NONE }), vec![key(0xffe1, false)], "Shift is up now");
        assert_eq!(k.release_stale(61, NONE), vec![key(0xffe3, false)]);
        assert!(k.release_all().is_empty());
    }

    #[test]
    fn a_modifier_is_not_stale_on_its_own_event() {
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        assert!(k.release_stale(37, NONE).is_empty(), "the state before its own press/release doesn't count");
    }

    #[test]
    fn pasted_text_goes_without_modifiers_and_they_come_back_if_still_held() {
        let mut k = KeyTracker::default();
        k.press(37, 0xffe3, None, NONE);
        k.press(55, 0x76, Some('v'), CTRL); // paste: nothing sent, nothing tracked
        let out = k.around_paste(vec![text("hi")]);
        assert_eq!(out, vec![key(0xffe3, false), text("hi"), key(0xffe3, true)]);
        assert_eq!(k.release(37), vec![key(0xffe3, false)], "Ctrl is still tracked as down");
        assert_eq!(k.around_paste(vec![text("x")]), vec![text("x")], "nothing held, nothing around");
    }

    #[test]
    fn alt_f4_closes_the_window_and_is_not_typed() {
        assert_eq!(map_key(0xffc1, None, Mods { alt: true, ..NONE }, true), KeyAction::Close);
        assert_eq!(map_key(0xffc1, None, NONE, true), KeyAction::Send(vec![key(0xffc1, true)]), "F4 alone is a key");
        let mut k = KeyTracker::default();
        assert_eq!(k.press(70, 0xffc1, None, Mods { alt: true, ..NONE }), KeyAction::Close);
        assert!(k.release(70).is_empty());
    }

    #[test]
    fn a_write_failing_is_a_lost_connection_unless_it_was_at_once() {
        assert_eq!(Phase::Typing.next(Step::WriteFailed { after: Duration::from_secs(30) }), Phase::Disconnected);
        assert_eq!(Phase::Typing.next(Step::WriteFailed { after: Duration::from_millis(100) }), Phase::Failed(Problem::EndedAtOnce));
        let over = Phase::Stopped { by_phone: true };
        assert_eq!(over.clone().next(Step::WriteFailed { after: Duration::from_secs(30) }), over);
        assert_eq!(Phase::Disconnected.next(Step::StopPressed), Phase::Disconnected);
    }

    #[test]
    fn the_services_real_errors_become_the_right_problems() {
        use linksy_core::Error;
        assert_eq!(start_problem(&Error::Unsupported.to_string()), Problem::Unsupported);
        assert_eq!(start_problem(&Error::Declined.to_string()), Problem::NotAllowed);
        assert_eq!(start_problem(&Error::Timeout.to_string()), Problem::NoAnswer);
        assert!(matches!(start_problem(&Error::Unreachable.to_string()), Problem::Other(_)));
    }

    fn view(n: u8, name: &str, kind: DeviceKind, features: &[&str]) -> DeviceView {
        let mut d = info(kind, features);
        d.id = Fingerprint([n; 32]);
        d.name = name.into();
        serde_json::from_value(serde_json::json!({ "device": d, "trusted": true, "connected": true, "nearby": true })).unwrap()
    }

    #[test]
    fn the_target_is_resolved_the_way_the_service_does_before_the_check() {
        let devices = [view(1, "Pixel", DeviceKind::Phone, &["keyboard"]), view(2, "Old", DeviceKind::Phone, &["files"]), view(3, "Desk", DeviceKind::Desktop, &["keyboard"])];
        let pixel = Fingerprint([1; 32]);
        assert_eq!(check_target(&devices, "Pixel"), Ok(pixel));
        assert_eq!(check_target(&devices, " pixel "), Ok(pixel), "padded name");
        assert_eq!(check_target(&devices, &pixel.to_string()), Ok(pixel), "full fingerprint");
        assert_eq!(check_target(&devices, " old "), Err(TargetError::NotTyping), "padded name still checked");
        assert_eq!(check_target(&devices, &Fingerprint([2; 32]).to_string()), Err(TargetError::NotTyping), "full fingerprint still checked");
        assert_eq!(check_target(&devices, &Fingerprint([3; 32]).short()), Err(TargetError::NotTyping), "a computer");
        assert!(matches!(check_target(&devices, "Nokia"), Err(TargetError::Resolve(e)) if e.starts_with("No device called")));
        assert_eq!(check_target(&devices, &Fingerprint([9; 32]).to_string()), Ok(Fingerprint([9; 32])), "unknown fingerprint: the service decides");
    }

    #[test]
    fn stdin_is_read_up_to_the_paste_limit_and_no_further() {
        assert_eq!(read_bounded("héllo\n".as_bytes()), Ok("héllo\n".to_string()));
        let exact = "a".repeat(MAX_PASTE);
        assert_eq!(read_bounded(exact.as_bytes()).map(|s| s.len()), Ok(MAX_PASTE));
        let over = "a".repeat(MAX_PASTE + 1);
        assert!(read_bounded(over.as_bytes()).unwrap_err().contains("100 KiB"));
        assert!(read_bounded(&[0xff, 0xfe][..]).is_err(), "not text");
    }

    #[test]
    fn stdin_never_reads_much_past_the_limit() {
        struct Endless(usize);
        impl Read for Endless {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.0 += buf.len();
                buf.fill(b'a');
                Ok(buf.len())
            }
        }
        let mut r = Endless(0);
        assert!(read_bounded(&mut r).is_err());
        assert!(r.0 <= MAX_PASTE + 64 * 1024, "read {}", r.0);
    }

    #[test]
    fn a_phone_that_closes_the_session_at_once_did_not_take_the_text() {
        let window = Duration::from_secs(2);
        assert_eq!(delivery_outcome(Closed::NotYet, window), Ok(()));
        assert_eq!(delivery_outcome(Closed::WhileWriting, window), Err(Problem::EndedAtOnce));
        // a short text is written before the close arrives, which looks the same on the wire
        assert_eq!(delivery_outcome(Closed::AfterWriting { after: Duration::from_millis(50) }, window), Err(Problem::EndedAtOnce));
        assert_eq!(delivery_outcome(Closed::AfterWriting { after: window - Duration::from_millis(1) }, window), Err(Problem::EndedAtOnce));
        assert_eq!(delivery_outcome(Closed::AfterWriting { after: window }, window), Ok(()), "the same edge as the window");
        assert_eq!(delivery_outcome(Closed::AfterWriting { after: Duration::from_secs(30) }, window), Ok(()));
    }
}
