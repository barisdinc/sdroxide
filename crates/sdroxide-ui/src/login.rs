//! The sign-in screen a server that asks for a password puts up.
//!
//! Drawn *instead of* the radio rather than over it. There is nothing to draw
//! behind it — no capabilities, no state and no spectrum arrive until the
//! server accepts — and a screen full of empty meters with a dialog on top
//! would only invite the belief that the radio is there and unresponsive.
//!
//! Shared by both clients: the main window and, in the browser, the
//! solar-system tab, which is challenged by the same server for the same
//! reason.
//!
//! A station serves each of its radios on a connection of its own, and asks
//! each one for a password — so a client holding a whole station would face
//! one sign-in per radio. It does not: an answer a station has *accepted* is
//! kept for as long as the program runs ([`SESSION_LOGINS`]) and offered to
//! every other connection to that same station, so the operator signs in once
//! and the rest of the tabs let themselves in. Ticking "remember" is the
//! separate, longer-lived decision to keep it on this device between runs.

use std::collections::BTreeMap;
use std::sync::{LazyLock, Mutex};

use eframe::egui::{self, RichText};
use sdroxide_types::{AuthPhase, RemoteAccess};

/// Sign-ins accepted this run, by station. Memory only, and never written
/// anywhere: what survives a restart is the "remember" store at the bottom of
/// this file, which is the operator's decision and not ours.
static SESSION_LOGINS: LazyLock<Mutex<BTreeMap<String, RemoteAccess>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// Which station an address belongs to: everything up to the endpoint, so the
/// several radios of one station (`/ws`, `/ws/1`, …) and its solar feed
/// (`/solar-ws`) come out the same, and a *different* station never does.
///
/// A password is answered to the station that asked for it and to nothing
/// else: two stations on one screen are two doors, however alike their
/// addresses look.
pub fn station_key(url: &str) -> String {
    let rest = url.trim_end_matches('/');
    match (rest.rfind("/ws"), rest.rfind("/solar-ws")) {
        (_, Some(i)) => rest[..i].to_string(),
        (Some(i), None) => rest[..i].to_string(),
        (None, None) => rest.to_string(),
    }
}

/// What the operator has typed, and what to do with it.
#[derive(Default)]
pub struct LoginForm {
    username: String,
    password: String,
    /// Whether to keep this sign-in on this device. Off by default: it writes
    /// a password to disk (or to the browser's local storage), which is the
    /// operator's call to make, not ours to make for them.
    remember: bool,
    /// Whether a stored sign-in has already been offered on this connection.
    /// Once only, so a stored password the server no longer accepts asks the
    /// operator instead of being posted back for ever.
    offered_stored: bool,
    /// Likewise for the sign-in another tab of this station made — offered
    /// once per connection, so a station that turns it down (the operator's
    /// access was withdrawn mid-session) asks this tab rather than looping.
    offered_session: bool,
    /// Which station this form belongs to, remembered from the last frame it
    /// was told: [`LoginForm::settle`] runs on the frame the answer is
    /// accepted, which is the frame that has to record it.
    station: String,
    /// Whether the attempt now being judged came out of storage, so a
    /// rejection can throw it away rather than leave a password behind that is
    /// known not to work.
    submitted_stored: bool,
    /// Whether the challenge was outstanding last frame, so the moment the
    /// server accepts can be noticed — that is when a sign-in is worth keeping,
    /// and not before.
    was_pending: bool,
    /// Whether the fields have been given the keyboard yet.
    focused: bool,
    /// The backdrop's state and the frame time it started at, on the screens
    /// that draw one. `None` on a phone, and until the first frame.
    sky: Option<(crate::login_globe::Sky, f64)>,
}

impl LoginForm {
    /// Notice the server's verdict. Call every frame, whatever the phase.
    ///
    /// The sign-in is written out on acceptance rather than on submission:
    /// storing what the operator typed before the server has agreed with it
    /// would leave a wrong password behind to be offered again next time.
    pub fn settle(&mut self, phase: &AuthPhase, station: &str) {
        if !station.is_empty() {
            self.station = station.to_string();
        }
        if phase.is_pending() {
            self.was_pending = true;
            return;
        }
        if !std::mem::take(&mut self.was_pending) {
            return;
        }
        // Accepted. Every other tab of this station may use this answer for as
        // long as the program runs — including tabs that have not been asked
        // yet, which is most of them at the moment the first one gets in.
        if !self.station.is_empty() && !self.password.is_empty() {
            SESSION_LOGINS.lock().unwrap_or_else(|e| e.into_inner()).insert(
                self.station.clone(),
                RemoteAccess { username: self.username.clone(), password: self.password.clone() },
            );
        }
        if self.remember {
            store(&RemoteAccess {
                username: self.username.clone(),
                password: self.password.clone(),
            });
        } else {
            // Unticking it is an instruction, not an omission.
            forget();
        }
        self.password.clear();
        self.focused = false;
    }

    /// The turning globe behind the card.
    ///
    /// Tablets and desktops only. A phone is the one screen where the card is
    /// nearly the whole window, so there would be little of the globe to see —
    /// and the most likely device to be running on a battery, over a link that
    /// is already the reason this screen is up. There it stays flat.
    fn backdrop(
        &mut self,
        ui: &mut egui::Ui,
        tier: crate::layout::Tier,
        render_state: Option<&eframe::egui_wgpu::RenderState>,
    ) {
        if tier == crate::layout::Tier::Phone {
            self.sky = None;
            return;
        }
        let Some(rs) = render_state else { return };
        crate::login_globe::ensure(rs);

        let rect = ui.max_rect();
        if rect.width() < 1.0 || rect.height() < 1.0 {
            return;
        }
        let now = ui.input(|i| i.time);
        let sky = self.sky.get_or_insert_with(|| (crate::login_globe::Sky::default(), now));
        sky.0.step(now);
        // Faded up over the first second: the pipelines and the two maps are
        // built on the frame this is first asked for, so without it the screen
        // cuts from flat black to a lit globe.
        let alpha = (((now - sky.1) / 1.0) as f32).clamp(0.0, 1.0);

        ui.painter().add(crate::egui_wgpu::Callback::new_paint_callback(
            rect,
            crate::login_globe::callback(&sky.0, [rect.width(), rect.height()], alpha),
        ));
        // Nothing else drives this screen's repaints — a socket that is waiting
        // for a password sends nothing — so the globe has to ask for its own.
        ui.ctx().request_repaint();
    }

    /// An answer this connection can give without asking anybody: the sign-in
    /// another tab of this station has already had accepted, or the one kept
    /// on this device. `None` means the operator has to be asked.
    ///
    /// Public because a tab that is not on screen never draws the sign-in and
    /// still has to be able to let itself in — see `SdroxideApp::poll_auth`.
    pub fn answer_without_asking(&mut self, phase: &AuthPhase) -> Option<RemoteAccess> {
        self.session_answer(phase).or_else(|| self.stored_answer(phase))
    }

    /// The answer another tab of this station has already had accepted, if
    /// there is one. Offered once per connection, and ahead of the stored
    /// sign-in: it is the one this station is known to be taking *now*.
    fn session_answer(&mut self, phase: &AuthPhase) -> Option<RemoteAccess> {
        if !matches!(phase, AuthPhase::Prompt(None)) || self.offered_session {
            return None;
        }
        let known =
            SESSION_LOGINS.lock().unwrap_or_else(|e| e.into_inner()).get(&self.station).cloned()?;
        self.offered_session = true;
        self.username = known.username.clone();
        self.password = known.password.clone();
        // Not `submitted_stored`: this did not come out of storage, so a
        // refusal must not erase the operator's saved sign-in.
        Some(known)
    }

    /// What was stored, offered once against the first challenge of a
    /// connection.
    fn stored_answer(&mut self, phase: &AuthPhase) -> Option<RemoteAccess> {
        if !matches!(phase, AuthPhase::Prompt(None)) || self.offered_stored {
            return None;
        }
        self.offered_stored = true;
        let stored = load_stored()?;
        self.username = stored.username.clone();
        self.password = stored.password.clone();
        self.remember = true;
        self.submitted_stored = true;
        Some(stored)
    }
}

/// Draw the sign-in for one frame. `Some` is the answer to send to the server.
///
/// Call only while [`AuthPhase::is_pending`]; the caller draws nothing else.
/// `render_state` is what the backdrop is built from; pass `None` and the
/// screen is drawn flat.
pub fn screen(
    ui: &mut egui::Ui,
    form: &mut LoginForm,
    phase: &AuthPhase,
    render_state: Option<&eframe::egui_wgpu::RenderState>,
) -> Option<RemoteAccess> {
    // A tab of a station somebody has already signed in to — or a sign-in
    // kept on this device — lets itself in without troubling the operator.
    if let Some(known) = form.answer_without_asking(phase) {
        return Some(known);
    }
    let refused = match phase {
        AuthPhase::Prompt(why) => why.as_deref(),
        _ => None,
    };
    if refused.is_some() && std::mem::take(&mut form.submitted_stored) {
        // The stored sign-in was turned down, so it is no use to anybody —
        // least of all next time, when it would be offered again and fail
        // again before the operator was ever asked.
        forget();
    }
    let checking = matches!(phase, AuthPhase::Checking);
    let tier = crate::layout::tier(ui.ctx());
    let touch = tier.touch();

    form.backdrop(ui, tier, render_state);

    // The card is sized to the screen it is on, not to a number: 360 pt of
    // phone leaves 328 once the margins are off, and a fixed 320-wide card
    // would sit in a 4 pt gutter — or overflow the moment the browser's own
    // chrome takes a few points more.
    let avail = ui.available_size();
    let card_w = (avail.x - 24.0).clamp(200.0, 360.0);
    // A little above centre, which is where a sign-in belongs and where the
    // globe's open sky is. On a screen too short to centre anything the
    // arithmetic goes negative and the floor puts the card at the top, which is
    // what keeps the fields clear of a phone's on-screen keyboard.
    let lead = ((avail.y - card_height(touch)) * 0.40).max(8.0);

    let mut submit = false;
    // A landscape phone is 393 pt tall and this card is most of that; the
    // scroll area is what stops the button being somewhere the finger cannot
    // reach when the keyboard is up.
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(lead);
            egui::Frame::new()
                .fill(crate::theme::PANEL().gamma_multiply(0.94))
                .stroke(egui::Stroke::new(1.0, crate::theme::LINE_LIT()))
                .inner_margin(egui::Margin::symmetric(if touch { 16 } else { 20 }, 16))
                .show(ui, |ui| {
                    ui.set_width(card_w);
                    ui.vertical(|ui| {
                        card(ui, form, refused, checking, touch, &mut submit);
                    });
                });
            ui.add_space(lead);
        });
    });

    // An empty password is never worth a round trip: the server would refuse
    // it and shut the door for three seconds, which the operator would then
    // have to sit out for the attempt they meant to make.
    if !submit || checking || form.password.is_empty() {
        return None;
    }
    form.submitted_stored = false;
    Some(RemoteAccess { username: form.username.clone(), password: form.password.clone() })
}

/// Roughly how tall the card comes out, for placing it before it is laid out.
/// Only used to centre it, so being a few points out costs nothing.
fn card_height(touch: bool) -> f32 {
    if touch { 300.0 } else { 260.0 }
}

/// The card's contents.
fn card(
    ui: &mut egui::Ui,
    form: &mut LoginForm,
    refused: Option<&str>,
    checking: bool,
    touch: bool,
    submit: &mut bool,
) {
    let field_h = if touch { 30.0 } else { 22.0 };

    ui.label(RichText::new("SDR OXIDE").size(17.0).strong().color(crate::theme::CYAN()));
    ui.add_space(12.0);

    ui.add_enabled_ui(!checking, |ui| {
        ui.label(RichText::new("Username").size(11.5).weak());
        let user = crate::chrome::field_sized(
            ui,
            [ui.available_width(), field_h],
            egui::TextEdit::singleline(&mut form.username),
        );
        ui.add_space(8.0);
        ui.label(RichText::new("Password").size(11.5).weak());
        let pass = crate::chrome::field_sized(
            ui,
            [ui.available_width(), field_h],
            egui::TextEdit::singleline(&mut form.password).password(true),
        );
        // The keyboard lands in the first empty box, so a remembered username
        // can be typed past without reaching for the mouse. Not done on a
        // touched layout: focusing a field there raises the on-screen keyboard
        // over the card before the operator has asked for it.
        if !form.focused && !touch {
            form.focused = true;
            if form.username.is_empty() {
                user.request_focus();
            } else {
                pass.request_focus();
            }
        }
        // Enter in either box is the button.
        let entered = (user.lost_focus() || pass.lost_focus())
            && ui.input(|i| i.key_pressed(egui::Key::Enter));
        *submit |= entered;
    });

    ui.add_space(10.0);
    // The wording is the hint on a touched layout: there is no pointer to hover
    // with, so a tooltip there is a sentence nobody will ever read.
    let remember = crate::chrome::checkbox(ui, &mut form.remember, "Remember on this device");
    if !touch {
        remember.on_hover_text(
            "Signs in without asking next time, and lets the 3D view's tab in without asking at \
             all. The password is kept in the clear, so leave this off on a machine other people \
             use.",
        );
    } else {
        ui.label(
            RichText::new("Keeps the password on this device, in the clear.")
                .size(10.5)
                .color(crate::theme::gray(140)),
        );
    }

    ui.add_space(12.0);
    ui.horizontal(|ui| {
        let label = if checking { " CHECKING… " } else { " SIGN IN " };
        let button = crate::chrome::chip_accent(
            ui,
            false,
            RichText::new(label).strong().size(if touch { 15.0 } else { 13.0 }),
            crate::theme::GREEN(),
            crate::theme::INK_ON_CYAN(),
        );
        *submit |= button.clicked() && !checking;
    });

    if let Some(why) = refused {
        ui.add_space(10.0);
        ui.label(RichText::new(why).size(12.0).color(crate::theme::ALERT()));
    } else if checking {
        ui.add_space(10.0);
        ui.label(
            RichText::new(
                "A wrong password shuts the server's door for a few seconds, so this can take a \
                 moment.",
            )
            .size(11.0)
            .color(crate::theme::gray(140)),
        );
    }
}

// ── Where a remembered sign-in lives ─────────────────────────────────────────
//
// Native: `remote_login.json` in the config directory, beside the rest of this
// client's own state. Browser: local storage, which the solar tab shares
// because it shares an origin — that is what makes one sign-in do for both.
//
// In the clear, both of them. So is every other credential sdroxide stores (the
// cluster login, the QRZ and eQSL passwords), the box that writes it says so,
// and the manual says so; encrypting it would need a key kept beside it, which
// is not an improvement worth the impression of one.

#[cfg(not(target_arch = "wasm32"))]
fn load_stored() -> Option<RemoteAccess> {
    sdroxide_config::load_remote_login()
}

#[cfg(not(target_arch = "wasm32"))]
fn store(login: &RemoteAccess) {
    if let Err(e) = sdroxide_config::save_remote_login(Some(login)) {
        eprintln!("failed to remember the sign-in: {e}");
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn forget() {
    if let Err(e) = sdroxide_config::save_remote_login(None) {
        eprintln!("failed to forget the sign-in: {e}");
    }
}

#[cfg(target_arch = "wasm32")]
const USER_KEY: &str = "sdroxide_login_user";
#[cfg(target_arch = "wasm32")]
const PASS_KEY: &str = "sdroxide_login_pass";

/// Two keys rather than one encoded value: nothing to escape, and nothing that
/// can be mis-split by a password that happens to contain the separator.
#[cfg(target_arch = "wasm32")]
fn local_storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

#[cfg(target_arch = "wasm32")]
fn load_stored() -> Option<RemoteAccess> {
    let store = local_storage()?;
    let login = RemoteAccess {
        username: store.get_item(USER_KEY).ok().flatten().unwrap_or_default(),
        password: store.get_item(PASS_KEY).ok().flatten().unwrap_or_default(),
    };
    login.is_enforced().then_some(login)
}

#[cfg(target_arch = "wasm32")]
fn store(login: &RemoteAccess) {
    let Some(store) = local_storage() else { return };
    let _ = store.set_item(USER_KEY, &login.username);
    let _ = store.set_item(PASS_KEY, &login.password);
}

#[cfg(target_arch = "wasm32")]
fn forget() {
    let Some(store) = local_storage() else { return };
    let _ = store.remove_item(USER_KEY);
    let _ = store.remove_item(PASS_KEY);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that asks for nothing must never make the form do anything —
    /// in particular it must not write or erase a stored sign-in belonging to
    /// some other server the operator uses.
    #[test]
    fn a_local_engine_settles_to_nothing() {
        let mut form = LoginForm { remember: true, ..LoginForm::default() };
        for _ in 0..5 {
            form.settle(&AuthPhase::Open, "");
        }
        assert!(!form.was_pending, "nothing was ever pending, so nothing was decided");
    }

    /// The transition that matters: pending, then accepted. Only then is there
    /// a sign-in worth keeping.
    #[test]
    fn acceptance_is_what_ends_the_exchange() {
        let mut form = LoginForm::default();
        form.settle(&AuthPhase::Prompt(None), "");
        assert!(form.was_pending);
        form.settle(&AuthPhase::Checking, "");
        assert!(form.was_pending);
        form.settle(&AuthPhase::Open, "");
        assert!(!form.was_pending, "the verdict was acted on exactly once");
        // ...and not again on every subsequent frame.
        form.settle(&AuthPhase::Open, "");
        assert!(!form.was_pending);
    }

    /// A station asks each of its radios' connections for the password
    /// separately. The operator answers once: the tab that got in leaves the
    /// answer where the others find it.
    #[test]
    fn one_sign_in_gets_the_stations_other_tabs_in() {
        let station = "ws://one-sign-in.test:4950";
        let mut first = LoginForm {
            username: "oe3jjs".into(),
            password: "hunter2".into(),
            ..LoginForm::default()
        };
        first.settle(&AuthPhase::Prompt(None), station);
        first.settle(&AuthPhase::Open, station);

        // The second radio's tab, challenged afterwards, answers by itself.
        let mut second = LoginForm { station: station.to_string(), ..LoginForm::default() };
        let answer = second
            .session_answer(&AuthPhase::Prompt(None))
            .expect("the station's sign-in was not offered to its other tab");
        assert_eq!(answer.username, "oe3jjs");
        assert_eq!(answer.password, "hunter2");
        // Once only: a station that turns it down asks this tab rather than
        // posting the same answer back for ever.
        assert!(second.session_answer(&AuthPhase::Prompt(None)).is_none());

        // ...and a *different* station is a different door.
        let mut elsewhere =
            LoginForm { station: "ws://elsewhere.test:4950".into(), ..LoginForm::default() };
        assert!(
            elsewhere.session_answer(&AuthPhase::Prompt(None)).is_none(),
            "one station's password was offered to another"
        );
    }

    /// Every connection to one station — each radio, and the solar feed —
    /// resolves to the same door, and no other station's does.
    #[test]
    fn a_stations_addresses_share_one_key() {
        let station = "ws://shack.test:4950";
        assert_eq!(station_key("ws://shack.test:4950/ws"), station);
        assert_eq!(station_key("ws://shack.test:4950/ws/10"), station);
        assert_eq!(station_key("ws://shack.test:4950/solar-ws"), station);
        assert_eq!(station_key("ws://shack.test:4950/"), station);
        assert_eq!(station_key("ws://shack.test:4950"), station);
        // A path prefix belongs to the address, so two stations behind one
        // reverse proxy stay two stations.
        assert_eq!(station_key("wss://host/a/ws/1"), "wss://host/a");
        assert_ne!(station_key("wss://host/a/ws/1"), station_key("wss://host/b/ws/1"));
    }

    /// The password is dropped once the server has taken it: keeping it in the
    /// form would leave it in memory for the rest of the session, and put it
    /// back in the box if the link dropped and the dialog returned.
    #[test]
    fn the_password_does_not_linger_after_a_successful_sign_in() {
        let mut form = LoginForm { password: "hunter2".into(), ..LoginForm::default() };
        form.settle(&AuthPhase::Prompt(None), "ws://lingering.test:4950");
        form.settle(&AuthPhase::Open, "ws://lingering.test:4950");
        assert!(form.password.is_empty());
    }

    /// A stored sign-in is offered against the first challenge and only the
    /// first: if the server turns it down, the operator is asked.
    #[test]
    fn a_stored_sign_in_is_offered_once_per_connection() {
        let mut form = LoginForm::default();
        // Whatever is (or is not) on this machine, the second ask must not
        // produce an answer — that is the loop this guards against.
        form.stored_answer(&AuthPhase::Prompt(None));
        assert!(form.offered_stored);
        assert!(form.stored_answer(&AuthPhase::Prompt(None)).is_none());
        // Nor does a rejection re-open it.
        assert!(form.stored_answer(&AuthPhase::Prompt(Some("no".into()))).is_none());
    }
}
