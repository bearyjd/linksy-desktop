//! Typing on the phone (docs/protocol.md "Typing on the phone"). A computer opens an
//! `input` session to a phone that says it takes typing (feature `keyboard`); the phone's app
//! gets the text and key presses from [`KeyboardInput`]. Everything is bounded: a peer can't
//! make the phone buffer or type without limit.

use std::collections::HashSet;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::identity::{DeviceInfo, DeviceKind};
use crate::node::Session;
use crate::protocol::{ControlMsg, SessionKind};

/// The feature a phone announces while its user lets computers type on it. Off unless the
/// app turns it on, so an `input` session to a phone is refused by default.
pub const KEYBOARD_FEATURE: &str = "keyboard";
/// The longest text (bytes, as received) in one `text` frame; a longer one is dropped.
pub const MAX_KEYBOARD_TEXT: usize = 1024;
/// The biggest frame body read; a bigger one ends the typing. Room for the longest text
/// with every byte escaped by JSON (`\u0001`, six bytes), so such a text is only dropped.
pub const MAX_KEYBOARD_FRAME: usize = 6 * MAX_KEYBOARD_TEXT + 64;
/// Once a frame has begun, the rest of it must come within this, or the typing ends. A
/// pause between frames ends nothing.
pub const KEYBOARD_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
/// Text and key presses kept for the app; when it's full, new ones are dropped.
pub const KEYBOARD_QUEUE: usize = 256;
/// Keys that can be held down at once; further presses are ignored. The queue keeps room
/// for this many releases on top of [`KEYBOARD_QUEUE`], so a release is never dropped and
/// the session never waits on the app to read.
pub const MAX_HELD_KEYS: usize = 32;

/// Whether a device with `info` takes `input` sessions: a computer always (its touchpad),
/// a phone or tablet only while it announces [`KEYBOARD_FEATURE`].
pub(crate) fn takes_typing(info: &DeviceInfo) -> bool {
    !is_handheld(info) || info.features.iter().any(|f| f == KEYBOARD_FEATURE)
}

pub(crate) fn is_handheld(info: &DeviceInfo) -> bool {
    matches!(info.kind, DeviceKind::Phone | DeviceKind::Tablet)
}

/// What the computer typed.
#[derive(Clone, PartialEq, Eq)]
pub enum KeyInput {
    /// Never empty. Control characters (Unicode `Cc`: C0, DEL, C1) are taken out, except
    /// tab and newline; Enter, Backspace and the like come as [`KeyInput::Key`].
    Text(String),
    /// `keysym`: an X11 keysym, any of them: the app decides which it types (it should
    /// refuse ones such as Super, power or media keys). While the [`KeyboardInput`] is kept,
    /// every press handed over is followed by its release, also when the session ends with
    /// the key still down.
    Key { keysym: u32, pressed: bool },
}

impl std::fmt::Debug for KeyInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // Never what was typed: it can be a password.
            Self::Text(text) => f.debug_struct("Text").field("len", &text.len()).finish(),
            Self::Key { keysym, pressed } => f.debug_struct("Key").field("keysym", keysym).field("pressed", pressed).finish(),
        }
    }
}

/// The typing of one `input` session, until it ends: the peer goes, a frame is broken, the
/// phone stops announcing [`KEYBOARD_FEATURE`], or this is dropped. Releases for keys still
/// down are queued when it ends; once this is dropped nothing more comes, so an app that
/// stops reading must itself release every key it pressed and didn't release yet.
pub struct KeyboardInput {
    pub id: uuid::Uuid,
    pub peer: DeviceInfo,
    rx: mpsc::Receiver<KeyInput>,
}

impl KeyboardInput {
    /// The next thing typed; `None` once the session is over and everything was read.
    pub async fn next(&mut self) -> Option<KeyInput> {
        self.rx.recv().await
    }
}

impl Session {
    /// Take this `input` session's typing. Pointer, button and scroll frames are ignored.
    /// Needs a running Tokio runtime; dropping the result ends the session.
    ///
    /// This is the only supported way to use a typing session, and it is also what ends one
    /// when typing is turned off. `None` (the session ends) for any session that isn't an
    /// incoming `input` session on a phone or tablet, such as a computer's touchpad.
    pub fn keyboard_input(mut self) -> Option<KeyboardInput> {
        let typing = self.typing.take().filter(|_| self.kind == SessionKind::Input)?;
        let (tx, rx) = mpsc::channel(KEYBOARD_QUEUE + MAX_HELD_KEYS);
        let (id, peer) = (self.id, self.peer.clone());
        tokio::spawn(pump(self, typing, tx));
        Some(KeyboardInput { id, peer, rx })
    }
}

/// The next control message, `Ok(None)` for one that isn't understood. A frame over the
/// limit, one not finished in time or a broken stream is an error.
async fn read_frame(recv: &mut quinn::RecvStream) -> Result<Option<ControlMsg>, ()> {
    let mut len = [0u8; 4];
    // Waiting for the next frame takes as long as it takes; its rest doesn't.
    recv.read_exact(&mut len[..1]).await.map_err(|_| ())?;
    let rest = async move {
        recv.read_exact(&mut len[1..]).await.map_err(|_| ())?;
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_KEYBOARD_FRAME {
            return Err(());
        }
        let mut body = vec![0u8; len];
        recv.read_exact(&mut body).await.map_err(|_| ())?;
        Ok(body)
    };
    let body = tokio::time::timeout(KEYBOARD_FRAME_TIMEOUT, rest).await.map_err(|_| ())??;
    Ok(serde_json::from_slice(&body).ok())
}

/// Never waits on the app: text and presses go only while the room kept for releases is
/// free, and releases always find room (there are never more owed than that room), so the
/// session ends as soon as the peer goes, read or not.
async fn pump(mut session: Session, typing: (watch::Receiver<u64>, u64), tx: mpsc::Sender<KeyInput>) {
    let mut held: HashSet<u32> = HashSet::new();
    let (mut typing, started) = typing;
    loop {
        // Biased: the app going and the kill-switch win over a frame that is ready.
        let msg = tokio::select! {
            biased;
            _ = tx.closed() => break,
            // The phone stopped taking typing at any time since this began, even if it
            // takes it again now (`set_features` without `keyboard`), or is gone.
            _ = typing.wait_for(|generation| *generation != started) => break,
            frame = read_frame(&mut session.recv) => match frame {
                Ok(msg) => msg,
                Err(()) => break,
            },
        };
        let delivered = match msg {
            Some(ControlMsg::Text { text }) if text.len() <= MAX_KEYBOARD_TEXT => {
                let text: String = text.chars().filter(|c| !c.is_control() || matches!(c, '\t' | '\n')).collect();
                if text.is_empty() { Some(false) } else { admit(&tx, KeyInput::Text(text)) }
            }
            Some(ControlMsg::Key { keysym, pressed: true, .. }) if held.contains(&keysym) || held.len() < MAX_HELD_KEYS => {
                let sent = admit(&tx, KeyInput::Key { keysym, pressed: true });
                if sent == Some(true) {
                    held.insert(keysym);
                }
                sent
            }
            // A release is never dropped, or a key sticks.
            Some(ControlMsg::Key { keysym, pressed: false, .. }) if held.remove(&keysym) => try_deliver(&tx, KeyInput::Key { keysym, pressed: false }),
            _ => Some(false),
        };
        if delivered.is_none() {
            break;
        }
    }
    // Whatever is still down when it ends is let go.
    for keysym in held {
        if try_deliver(&tx, KeyInput::Key { keysym, pressed: false }).is_none() {
            break;
        }
    }
}

/// Text or a press: only while more than the room kept for releases is free.
fn admit(tx: &mpsc::Sender<KeyInput>, input: KeyInput) -> Option<bool> {
    if tx.capacity() > MAX_HELD_KEYS {
        try_deliver(tx, input)
    } else if tx.is_closed() {
        None
    } else {
        Some(false)
    }
}

/// `Some(true)` queued, `Some(false)` dropped because the app is behind, `None` the app is gone.
fn try_deliver(tx: &mpsc::Sender<KeyInput>, input: KeyInput) -> Option<bool> {
    match tx.try_send(input) {
        Ok(()) => Some(true),
        Err(mpsc::error::TrySendError::Full(_)) => Some(false),
        Err(mpsc::error::TrySendError::Closed(_)) => None,
    }
}
