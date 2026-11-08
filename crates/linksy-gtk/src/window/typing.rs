//! "Type on Phone": a small window that sends what is typed (and pasted) to a phone's focused
//! field. What a key means, and what to say as it goes, is `linksy_ipc::typing`; this only
//! wires GTK's keys, input method and clipboard to it.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib::translate::IntoGlib;
use gtk::{gdk, glib};
use linksy_i18n::t;
use linksy_ipc::linksy_core::protocol::control_frame;
use linksy_ipc::linksy_core::{ControlMsg, Fingerprint, SessionKind, Uuid};
use linksy_ipc::typing::{KeyAction, Mods, Phase, Problem, Step, map_key, paste_messages, text_messages};

use super::Window;

/// How long a write may wait for the service before the session counts as lost.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

impl Window {
    /// Open the typing window for a phone and ask it (it always asks its user).
    pub(super) fn type_on_phone(self: &Rc<Self>, id: Fingerprint) {
        let name = self.device(&id).map(|d| d.device.name).unwrap_or_default();
        let typer = Typer::open(self, id, &name);
        typer.start();
    }
}

#[derive(Clone)]
struct Typer {
    owner: Weak<Window>,
    device: Fingerprint,
    name: String,
    window: adw::Window,
    status: adw::StatusPage,
    stop: gtk::Button,
    phase: Rc<RefCell<Phase>>,
    started: Rc<Cell<Option<Instant>>>,
    stream: Rc<RefCell<Option<UnixStream>>>,
    session: Rc<Cell<Option<Uuid>>>,
    /// Keys sent as pressed and not yet as released, so none stays down on the phone.
    held: Rc<RefCell<HashSet<u32>>>,
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
            session: Rc::default(),
            held: Rc::default(),
        };
        typer.setup_input();
        let t = typer.clone();
        typer.stop.connect_clicked(move |_| t.step(Step::StopPressed));
        let t = typer.clone();
        typer.window.connect_close_request(move |_| {
            t.step(Step::StopPressed);
            glib::Propagation::Proceed
        });
        typer.show(&Phase::Asking);
        typer.window.present();
        typer
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
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
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

    /// The connection ends here: keys still down are let go first, while it can be told.
    fn close_session(&self, next: &Phase) {
        if !matches!(next, Phase::Stopped { by_phone: true } | Phase::Failed(_)) {
            self.release_held();
        }
        if let Some(stream) = self.stream.borrow_mut().take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.release_session();
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
        // over what it composed; the keys it doesn't take come to `key_pressed`.
        let im = gtk::IMMulticontext::new();
        im.set_client_widget(Some(&self.window));
        let t = self.clone();
        im.connect_commit(move |_, text| t.send(text_messages(text)));
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        keys.set_im_context(Some(&im));
        let t = self.clone();
        keys.connect_key_pressed(move |_, key, _, state| t.key(key, state, true));
        let t = self.clone();
        keys.connect_key_released(move |_, key, _, state| {
            t.key(key, state, false);
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

    fn key(&self, key: gdk::Key, state: gdk::ModifierType, pressed: bool) -> glib::Propagation {
        if !matches!(*self.phase.borrow(), Phase::Typing) {
            return glib::Propagation::Proceed;
        }
        let mods = Mods {
            ctrl: state.contains(gdk::ModifierType::CONTROL_MASK),
            alt: state.contains(gdk::ModifierType::ALT_MASK),
            shift: state.contains(gdk::ModifierType::SHIFT_MASK),
            sup: state.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::META_MASK),
        };
        match map_key(key.into_glib(), key.to_unicode(), mods, pressed) {
            KeyAction::Send(msgs) => self.send(msgs),
            KeyAction::Paste => self.paste(),
            KeyAction::Ignore => {}
        }
        // Everything is for the phone while typing, nothing triggers a shortcut here
        glib::Propagation::Stop
    }

    fn paste(&self) {
        let t = self.clone();
        self.window.clipboard().read_text_async(gtk::gio::Cancellable::NONE, move |text| match text {
            Ok(Some(text)) => {
                let (msgs, cut) = paste_messages(&text);
                t.send(msgs);
                if cut {
                    t.status.set_description(Some(t!("That was a lot: only the start was sent.")));
                }
            }
            Ok(None) => {}
            Err(e) => t.status.set_description(Some(&glib::markup_escape_text(&t!("Couldn't paste: {error}", error = e)))),
        });
    }

    fn send(&self, msgs: Vec<ControlMsg>) {
        if msgs.is_empty() || !matches!(*self.phase.borrow(), Phase::Typing) {
            return;
        }
        for msg in &msgs {
            if let ControlMsg::Key { keysym, pressed, .. } = msg {
                if *pressed {
                    self.held.borrow_mut().insert(*keysym);
                } else {
                    self.held.borrow_mut().remove(keysym);
                }
            }
        }
        let failed = {
            let mut stream = self.stream.borrow_mut();
            let Some(out) = stream.as_mut() else { return };
            msgs.iter().any(|m| out.write_all(&control_frame(m)).is_err())
        };
        if failed {
            self.lost();
        }
    }

    fn release_held(&self) {
        let held: Vec<u32> = self.held.borrow_mut().drain().collect();
        if held.is_empty() {
            return;
        }
        let mut stream = self.stream.borrow_mut();
        if let Some(out) = stream.as_mut() {
            for keysym in held {
                let _ = out.write_all(&control_frame(&ControlMsg::Key { keysym, pressed: false, code: None }));
            }
        }
    }
}

/// What to do when a phone declined, didn't answer or closed at once: the usual cause.
fn not_enabled(name: &str) -> String {
    t!("On {name}, open Linksy and turn on Linksy Keyboard, then try again and tap Allow. Older versions of the Linksy app can't take typing.", name)
}
