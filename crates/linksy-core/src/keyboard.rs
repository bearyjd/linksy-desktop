//! Typing on the phone (docs/protocol.md "Typing on the phone"). A computer opens an
//! `input` session to a phone that says it takes typing (feature `keyboard`); the phone's app
//! gets the text and key presses from [`KeyboardInput`]. Everything is bounded: a peer can't
//! make the phone buffer or type without limit.

use std::collections::HashSet;

use tokio::sync::mpsc;

use crate::identity::DeviceInfo;
use crate::node::Session;
use crate::protocol::ControlMsg;

/// The feature a phone announces while its user lets computers type on it. Off unless the
/// app turns it on, so an `input` session to a phone is refused by default.
pub const KEYBOARD_FEATURE: &str = "keyboard";
/// The biggest frame body read; a bigger one ends the typing.
pub const MAX_KEYBOARD_FRAME: usize = 4096;
/// The longest text (bytes) in one `text` frame; a longer one is dropped.
pub const MAX_KEYBOARD_TEXT: usize = 1024;
/// Events kept for the app; when it's full, new text and key presses are dropped.
pub const KEYBOARD_QUEUE: usize = 256;
/// Keys that can be held down at once; further presses are ignored.
pub const MAX_HELD_KEYS: usize = 32;

/// What the computer typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyInput {
    Text(String),
    /// `keysym`: an X11 keysym. Every press is followed by its release, also when the
    /// session ends with the key still down.
    Key { keysym: u32, pressed: bool },
}

/// The typing of one `input` session, until it ends.
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
    pub fn keyboard_input(self) -> KeyboardInput {
        let (tx, rx) = mpsc::channel(KEYBOARD_QUEUE);
        let (id, peer) = (self.id, self.peer.clone());
        tokio::spawn(pump(self, tx));
        KeyboardInput { id, peer, rx }
    }
}

/// The next control message, `Ok(None)` for one that isn't understood. A frame over the
/// limit or a broken stream is an error.
async fn read_frame(recv: &mut quinn::RecvStream) -> Result<Option<ControlMsg>, ()> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(|_| ())?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_KEYBOARD_FRAME {
        return Err(());
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.map_err(|_| ())?;
    Ok(serde_json::from_slice(&body).ok())
}

async fn pump(mut session: Session, tx: mpsc::Sender<KeyInput>) {
    let mut held: HashSet<u32> = HashSet::new();
    loop {
        let msg = tokio::select! {
            _ = tx.closed() => break,
            frame = read_frame(&mut session.recv) => match frame {
                Ok(msg) => msg,
                Err(()) => break,
            },
        };
        let delivered = match msg {
            Some(ControlMsg::Text { text }) if !text.is_empty() && text.len() <= MAX_KEYBOARD_TEXT => try_deliver(&tx, KeyInput::Text(text)),
            Some(ControlMsg::Key { keysym, pressed: true, .. }) if held.contains(&keysym) || held.len() < MAX_HELD_KEYS => {
                let sent = try_deliver(&tx, KeyInput::Key { keysym, pressed: true });
                if sent == Some(true) {
                    held.insert(keysym);
                }
                sent
            }
            // A release is never dropped (there are few: only held keys), or a key sticks.
            Some(ControlMsg::Key { keysym, pressed: false, .. }) if held.remove(&keysym) => {
                tx.send(KeyInput::Key { keysym, pressed: false }).await.ok().map(|_| true)
            }
            _ => Some(false),
        };
        if delivered.is_none() {
            break;
        }
    }
    // Whatever is still down when it ends is let go.
    for keysym in held {
        if tx.send(KeyInput::Key { keysym, pressed: false }).await.is_err() {
            break;
        }
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
