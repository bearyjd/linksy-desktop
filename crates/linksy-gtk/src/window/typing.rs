//! "Type on Phone": a small window that sends what is typed (and pasted) to a phone's focused
//! field. What a key means, and what to say as it goes, is `linksy_ipc::typing`; this only
//! wires GTK's keys, input method and clipboard to it.

use std::cell::{Cell, RefCell};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};
use std::sync::mpsc::SyncSender;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib::translate::IntoGlib;
use gtk::{gdk, glib};
use linksy_i18n::t;
use linksy_ipc::linksy_core::protocol::control_frame;
use linksy_ipc::linksy_core::{ControlMsg, Fingerprint, SessionKind, Uuid};
use linksy_ipc::shutdown::{Action, Event, Shutdown as Closing};
use linksy_ipc::typing::{KeyAction, KeyTracker, Mods, Phase, Problem, Step, paste_messages, text_messages};
use linksy_ipc::writer::{Job, Parting, spawn};

use super::Window;

/// How long one write may take, altogether, before the session counts as lost.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long in all letting go of held keys may take, from when it is asked for.
const RELEASE_DEADLINE: Duration = Duration::from_secs(2);
/// How long a closing connection may take to send what is queued and let go of the keys
/// before it is cut and the session ended anyway (the release deadline, and some room).
const CLOSE_FALLBACK: Duration = Duration::from_secs(3);
/// What may wait to be written (a paste is one job); a phone slower than this is lost.
const QUEUE: usize = 64;

impl Window {
    /// Open the typing window for a phone and ask it (it always asks its user).
    pub(super) fn type_on_phone(self: &Rc<Self>, id: Fingerprint) {
        // One window per phone: one still going is brought forward, one that is over is replaced
        let open = self.typers.borrow().get(&id).cloned();
        if let Some(typer) = open {
            if typer.live() {
                typer.window.present();
                return;
            }
            typer.window.close();
        }
        let name = self.device(&id).map(|d| d.device.name).unwrap_or_default();
        let typer = Typer::open(self, id, &name);
        self.typers.borrow_mut().insert(id, typer.clone());
        typer.start();
    }
}

#[derive(Clone)]
pub(super) struct Typer {
    owner: Weak<Window>,
    device: Fingerprint,
    name: String,
    pub(super) window: adw::Window,
    status: adw::StatusPage,
    stop: gtk::Button,
    phase: Rc<RefCell<Phase>>,
    started: Rc<Cell<Option<Instant>>>,
    /// The connection, kept to shut it down at once when the writer can't be trusted to.
    stream: Rc<RefCell<Option<UnixStream>>>,
    /// The writer's queue; letting go of it makes the writer finish what is in it and close.
    writer: Rc<RefCell<Option<SyncSender<Job>>>>,
    /// What the writer sends last (the keys to let go of), set even when the queue is full.
    parting: Rc<RefCell<Option<Parting>>>,
    /// Where the ordered shutdown is: the session ends only after the writer has drained.
    closing: Rc<Cell<Closing>>,
    session: Rc<Cell<Option<Uuid>>>,
    /// Keys the phone holds, so none stays down there.
    keys: Rc<RefCell<KeyTracker>>,
    /// A note ("That was a lot") is showing and goes with the next key.
    notice: Rc<Cell<bool>>,
}

impl Typer {
    fn open(owner: &Rc<Window>, device: Fingerprint, name: &str) -> Self {
        let status = adw::StatusPage::builder().icon_name("input-keyboard-symbolic").vexpand(true).build();
        let stop = gtk::Button::builder().label(t!("Stop")).focusable(false).focus_on_click(false).css_classes(["destructive-action"]).build();
        let header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new(&t!("Typing on {name}", name), "")).build();
        header.pack_start(&stop);
        let toolbar = adw::ToolbarView::builder().content(&status).build();
        toolbar.add_top_bar(&header);
        let window = adw::Window::builder().title(t!("Typing on {name}", name)).default_width(420).default_height(320).content(&toolbar).build();
        if let Some(app) = owner.window.application() {
            window.set_application(Some(&app));
        }
        let typer = Self {
            owner: Rc::downgrade(owner),
            device,
            name: name.to_string(),
            window,
            status,
            stop,
            phase: Rc::new(RefCell::new(Phase::Asking)),
            started: Rc::default(),
            stream: Rc::default(),
            writer: Rc::default(),
            parting: Rc::default(),
            closing: Rc::new(Cell::new(Closing::Open)),
            session: Rc::default(),
            keys: Rc::default(),
            notice: Rc::default(),
        };
        typer.setup_input();
        let t = typer.clone();
        typer.stop.connect_clicked(move |_| t.step(Step::StopPressed));
        let t = typer.clone();
        typer.window.connect_close_request(move |_| {
            t.step(Step::StopPressed);
            if let Some(owner) = t.owner.upgrade() {
                // Only its own entry: a newer window for the phone may have replaced it
                let mut typers = owner.typers.borrow_mut();
                if typers.get(&t.device).is_some_and(|e| e.window == t.window) {
                    typers.remove(&t.device);
                }
            }
            glib::Propagation::Proceed
        });
        typer.show(&Phase::Asking);
        typer.window.present();
        typer
    }

    /// Still asking or typing.
    fn live(&self) -> bool {
        matches!(*self.phase.borrow(), Phase::Asking | Phase::Typing)
    }

    fn start(&self) {
        let Some(owner) = self.owner.upgrade() else { return };
        let Some(c) = owner.client() else { return };
        let device = self.device;
        let call = owner.bridge.call(&c, move |c| async move { c.start_session(device.to_string(), SessionKind::Input).await });
        let t = self.clone();
        glib::spawn_future_local(async move {
            let info = match call.await {
                Ok(info) => info,
                Err(e) => return t.step(Step::StartFailed(e)),
            };
            t.session.set(Some(info.session));
            // The person may have stopped while the phone was being asked
            if !matches!(*t.phase.borrow(), Phase::Asking) {
                return t.release_session();
            }
            match info.socket.as_deref().map(UnixStream::connect) {
                Some(Ok(stream)) => t.connected(stream),
                Some(Err(e)) => t.step(Step::StartFailed(e.to_string())),
                None => t.step(Step::StartFailed(t!("The service sent no stream.").to_string())),
            }
        });
    }

    fn connected(&self, stream: UnixStream) {
        // Writes happen on a thread of their own: a slow service never stops the window
        match stream.try_clone() {
            Ok(out) => {
                let (tx, rx) = async_channel::bounded::<bool>(1);
                let (jobs, parting) = spawn(out, QUEUE, WRITE_TIMEOUT, move |ok| {
                    let _ = tx.send_blocking(ok);
                });
                *self.writer.borrow_mut() = Some(jobs);
                *self.parting.borrow_mut() = Some(parting);
                let t = self.clone();
                glib::spawn_future_local(async move {
                    if let Ok(ok) = rx.recv().await {
                        if !ok {
                            t.write_failed();
                        }
                        // The writer is done, whichever way: the session can end now
                        t.closing_event(Event::Drained);
                    }
                });
            }
            Err(e) => return self.step(Step::StartFailed(e.to_string())),
        }
        // The service closes its end when the phone ends the session: a blocking read in a
        // thread tells. Stopping shuts the socket down, which ends the thread.
        if let Ok(mut reader) = stream.try_clone() {
            let (tx, rx) = async_channel::bounded::<()>(1);
            std::thread::spawn(move || {
                let mut buf = [0u8; 64];
                while matches!(std::io::Read::read(&mut reader, &mut buf), Ok(n) if n > 0) {}
                let _ = tx.send_blocking(());
            });
            let t = self.clone();
            glib::spawn_future_local(async move {
                if rx.recv().await.is_ok() {
                    t.lost();
                }
            });
        }
        *self.stream.borrow_mut() = Some(stream);
        self.started.set(Some(Instant::now()));
        self.step(Step::Started);
    }

    fn lost(&self) {
        let after = self.started.get().map(|s| s.elapsed()).unwrap_or_default();
        self.step(Step::Lost { after });
    }

    fn write_failed(&self) {
        let after = self.started.get().map(|s| s.elapsed()).unwrap_or_default();
        self.step(Step::WriteFailed { after });
    }

    fn step(&self, step: Step) {
        let before = self.phase.borrow().clone();
        let next = before.clone().next(step);
        if next == before {
            return;
        }
        if !matches!(next, Phase::Typing) {
            self.close_session(&next);
        }
        *self.phase.borrow_mut() = next.clone();
        self.show(&next);
    }

    /// The connection ends here. When it is still good, the writer is let go of: it sends
    /// what was typed, lets go of the keys still down and closes, and only then is the
    /// session ended (the service stops relaying the moment it ends). If the writer can't be
    /// trusted, or it takes too long, the connection is cut and the session ended anyway.
    fn close_session(&self, next: &Phase) {
        let good = !matches!(next, Phase::Stopped { by_phone: true } | Phase::Failed(_) | Phase::Disconnected);
        let writer_alive = good && self.writer.borrow().is_some();
        self.closing_event(Event::Begin { writer_alive });
    }

    fn closing_event(&self, event: Event) {
        let (state, actions) = self.closing.get().next(event);
        self.closing.set(state);
        for action in actions {
            match action {
                Action::QueueRelease => {
                    let up = self.keys.borrow_mut().release_all();
                    let frames = up.iter().map(control_frame).collect();
                    if let Some(parting) = self.parting.borrow().as_ref() {
                        parting.set(frames, Instant::now() + RELEASE_DEADLINE);
                    }
                }
                // Dropping the queue's sender: the writer still sends everything in it
                Action::CloseQueue => drop(self.writer.borrow_mut().take()),
                Action::ArmTimeout => {
                    let t = self.clone();
                    glib::spawn_future_local(async move {
                        glib::timeout_future(CLOSE_FALLBACK).await;
                        t.closing_event(Event::TimedOut);
                    });
                }
                Action::ShutSocket => {
                    drop(self.writer.borrow_mut().take());
                    if let Some(stream) = self.stream.borrow_mut().take() {
                        let _ = stream.shutdown(Shutdown::Both);
                    }
                }
                Action::EndSession => self.release_session(),
            }
        }
    }

    fn release_session(&self) {
        if let (Some(owner), Some(session)) = (self.owner.upgrade(), self.session.take()) {
            owner.end_session(session);
        }
    }

    fn show(&self, phase: &Phase) {
        let name = &self.name;
        let (title, description) = match phase {
            Phase::Asking => (t!("Asking {name}…", name), t!("Tap Allow on {name} to let this computer type there.", name)),
            Phase::Typing => (t!("Typing on {name}", name), t!("Click the field you want on {name}, then type here. Paste works too.", name)),
            Phase::Stopped { by_phone: false } => (t!("Stopped").to_string(), t!("Nothing more is sent to {name}.", name)),
            Phase::Stopped { by_phone: true } => (t!("{name} stopped it", name), t!("Close this and choose Type on Phone again to continue.").to_string()),
            Phase::Failed(Problem::Unsupported) => (t!("{name} can't take typing yet", name), t!("Update Linksy on {name}, then try again.", name)),
            Phase::Failed(Problem::NotAllowed | Problem::EndedAtOnce) => (t!("{name} didn't take it", name), not_enabled(name)),
            Phase::Failed(Problem::NoAnswer) => (t!("No answer").to_string(), not_enabled(name)),
            Phase::Disconnected => (t!("Lost connection to {name}", name), t!("Nothing more is sent. Close this and choose Type on Phone again.").to_string()),
            Phase::Failed(Problem::Other(e)) => (t!("Couldn't Connect").to_string(), crate::fmt::capitalize(e)),
        };
        self.status.set_title(&title);
        self.status.set_description(Some(&glib::markup_escape_text(&description)));
        let live = matches!(phase, Phase::Asking | Phase::Typing);
        self.stop.set_visible(live);
        if matches!(phase, Phase::Typing) {
            self.window.grab_focus();
        }
    }

    fn setup_input(&self) {
        // The input method turns key presses into text (accents, compose, IME) and hands
        // over what it composed; the keys it doesn't take come to `key`. It may also take
        // the release of a modifier used to start composing (Ctrl+Shift+U), so nothing here
        // relies on seeing it: the next key event names the modifiers really held, and any
        // the phone holds beyond them are let go (`KeyTracker::release_stale`). Until a next
        // key arrives, such a modifier stays down on the phone; Stop, closing the window or
        // losing focus lets it go. Unverified without a display.
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(&self.window));
        let t = self.clone();
        im.connect_commit(move |_, text| t.send(text_messages(text)));
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.set_im_context(Some(&im));
        let t = self.clone();
        keys.connect_key_pressed(move |_, key, code, state| t.key(key, code, state, true));
        let t = self.clone();
        keys.connect_key_released(move |_, key, code, state| {
            t.key(key, code, state, false);
        });
        self.window.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let i = im.clone();
        focus.connect_enter(move |_| i.focus_in());
        let (t, i) = (self.clone(), im);
        focus.connect_leave(move |_| {
            i.focus_out();
            t.release_held();
        });
        self.window.add_controller(focus);
    }

    fn key(&self, key: gdk::Key, code: u32, state: gdk::ModifierType, pressed: bool) -> glib::Propagation {
        if !matches!(*self.phase.borrow(), Phase::Typing) {
            return glib::Propagation::Proceed;
        }
        let mods = Mods {
            ctrl: state.contains(gdk::ModifierType::CONTROL_MASK),
            alt: state.contains(gdk::ModifierType::ALT_MASK),
            shift: state.contains(gdk::ModifierType::SHIFT_MASK),
            sup: state.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK),
        };
        let mut out = self.keys.borrow_mut().release_stale(code, mods);
        let mut close = false;
        if pressed {
            if self.notice.replace(false) {
                self.show(&Phase::Typing);
            }
            let action = self.keys.borrow_mut().press(code, key.into_glib(), key.to_unicode(), mods);
            match action {
                KeyAction::Send(msgs) => out.extend(msgs),
                KeyAction::Paste => self.paste(),
                KeyAction::Close => close = true,
                KeyAction::Ignore => {}
            }
        } else {
            out.extend(self.keys.borrow_mut().release_event(code, key.into_glib()));
        }
        self.send(out);
        if close {
            self.window.close();
        }
        // Everything is for the phone while typing, nothing triggers a shortcut here
        glib::Propagation::Stop
    }

    fn paste(&self) {
        let t = self.clone();
        self.window.clipboard().read_text_async(gtk::gio::Cancellable::NONE, move |text| match text {
            Ok(Some(text)) => {
                let (msgs, cut) = paste_messages(&text);
                // The phone sees no Ctrl held while the text arrives
                let msgs = t.keys.borrow().around_paste(msgs);
                t.send(msgs);
                if cut {
                    t.status.set_description(Some(t!("That was a lot: only the start was sent.")));
                    t.notice.set(true);
                }
            }
            Ok(None) => {}
            Err(e) => t.status.set_description(Some(&glib::markup_escape_text(&t!("Couldn't paste: {error}", error = e)))),
        });
    }

    /// Queue `msgs` for the writer; a queue that stays full means the connection is lost.
    fn send(&self, msgs: Vec<ControlMsg>) {
        if msgs.is_empty() || !matches!(*self.phase.borrow(), Phase::Typing) {
            return;
        }
        let frames = msgs.iter().map(control_frame).collect();
        let queued = self.writer.borrow().as_ref().map(|tx| tx.try_send(Job::Write(frames)).is_ok());
        if queued == Some(false) {
            self.write_failed();
        }
    }

    /// Let go of every key the phone holds (focus left).
    fn release_held(&self) {
        let up = self.keys.borrow_mut().release_all();
        if up.is_empty() {
            return;
        }
        let frames = up.iter().map(control_frame).collect();
        // The deadline starts now, not when the writer gets to it
        let deadline = Instant::now() + RELEASE_DEADLINE;
        let queued = self.writer.borrow().as_ref().map(|tx| tx.try_send(Job::Release(frames, deadline)).is_ok());
        if queued == Some(false) {
            self.write_failed();
        }
    }
}

/// What to do when a phone declined, didn't answer or closed at once: the usual cause.
fn not_enabled(name: &str) -> String {
    t!("On {name}, open Linksy and turn on Linksy Keyboard, then try again and tap Allow. Older versions of the Linksy app can't take typing.", name)
}
