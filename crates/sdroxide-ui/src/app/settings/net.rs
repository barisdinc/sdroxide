//! The Spots, Uploads and FreeDV tabs — the network cockpit's settings.
//!
//! All three edit one [`NetworkConfig`], which the engine persists when it
//! applies it, so each has an APPLY button rather than saving as you type:
//! these are credentials and connection details, and half-typed ones would
//! reconnect the feeds on every keystroke.

use eframe::egui::{self, Color32, RichText};

/// A dropdown over an enum's `ALL`, using its `label()`.
/// UI / display preferences: frame rate, waterfall scroll speed, spectrum speed.
/// Section heading for the Spots / Uploads settings tabs.
pub(in crate::app) fn net_heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(text).size(12.0).strong().color(crate::theme::CYAN()));
}

/// A labelled single-line text field for the network settings tabs.
pub(in crate::app) fn net_row(ui: &mut egui::Ui, label: &str, val: &mut String, w: f32) {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 22.0], egui::Label::new(label));
        crate::chrome::field(ui, egui::TextEdit::singleline(val).desired_width(w));
    });
}

/// A labelled password field (masked) for the network settings tabs.
pub(in crate::app) fn net_secret(ui: &mut egui::Ui, label: &str, val: &mut String, w: f32) {
    ui.horizontal(|ui| {
        ui.add_sized([96.0, 22.0], egui::Label::new(label));
        crate::chrome::field(ui, egui::TextEdit::singleline(val).password(true).desired_width(w));
    });
}

/// Where the operator's callsign and grid actually live, for the network tabs
/// that report under them but deliberately do not offer a second copy to edit.
pub(in crate::app) fn operator_identity_note(
    ui: &mut egui::Ui,
    digi: &sdroxide_types::DigiConfig,
    seeded: bool,
) {
    net_heading(ui, "Operator");
    if !seeded {
        ui.label(RichText::new("Callsign and grid are set on the General tab.").weak());
        return;
    }
    let (call, grid) = (digi.my_call.trim(), digi.my_grid.trim());
    if call.is_empty() || grid.is_empty() {
        ui.label(
            RichText::new("⚠ Set your callsign and grid on the General tab.")
                .color(Color32::from_rgb(230, 170, 60)),
        );
    } else {
        ui.label(RichText::new(format!("{call} / {grid}  — set on the General tab")).weak());
    }
}

/// FreeDV Reporter (<https://qso.freedv.org/>): announce this station and show
/// everyone else's as spots.
///
/// `call`/`grid` are the operator identity from the General tab, shown here but
/// not editable: reporting under a second copy would only let the two disagree.
#[allow(clippy::too_many_arguments)]
pub(in crate::app) fn settings_freedv_tab(
    ui: &mut egui::Ui,
    net: &mut sdroxide_types::NetworkConfig,
    call: &str,
    grid: &str,
    digi_seeded: bool,
    status: &Option<String>,
    apply: &mut bool,
) {
    ui.label(RichText::new("FreeDV Reporter").size(14.0).strong().color(crate::theme::CYAN()));
    ui.add_space(6.0);
    crate::chrome::checkbox(ui, &mut net.freedv_reporter.enabled, "Enable").on_hover_text(
        "Connects whenever enabled. Your station is only shown to others while the radio is \
         in RADE — in any other mode you stay connected but hidden.",
    );
    ui.add_space(6.0);

    let enabled = net.freedv_reporter.enabled;

    ui.add_enabled_ui(enabled, |ui| {
        let c = &mut net.freedv_reporter;

        net_heading(ui, "Station");
        net_row(ui, "Message", &mut c.message, 260.0);
        crate::chrome::checkbox(ui, &mut c.rx_only, "Receive only (I cannot transmit)");

        net_heading(ui, "Server");
        net_row(ui, "Host", &mut c.host, 220.0);
        ui.horizontal(|ui| {
            ui.add_sized([96.0, 22.0], egui::Label::new("Port"));
            ui.add(egui::DragValue::new(&mut c.port).range(1..=65535));
        });
        ui.add_enabled_ui(false, |ui| {
            crate::chrome::checkbox(ui, &mut c.tls, "TLS (wss://)")
                .on_hover_text("Not yet implemented — FreeDV GUI uses plain ws:// too.");
        });

        net_heading(ui, "Reporting");
        crate::chrome::checkbox(ui, &mut c.report_rx, "Report stations I decode").on_hover_text(
            "Sends an rx_report for each callsign recovered from a RADE \
                            End-of-Over frame.",
        );
        crate::chrome::checkbox(ui, &mut c.show_spots, "Show other reporter stations as spots")
            .on_hover_text(
                "Adds them to the panadapter overlay, world map and SPOTS window \
                            under the FREEDV filter.",
            );
    });

    ui.add_space(8.0);
    if enabled && digi_seeded && (call.is_empty() || grid.is_empty()) {
        ui.label(
            RichText::new(
                "⚠ Set your callsign and grid on the General tab. Without both, the \
                 connection is view-only: you will see other stations but will not appear \
                 yourself.",
            )
            .color(Color32::from_rgb(230, 170, 60)),
        );
    } else if enabled && digi_seeded {
        ui.label(
            RichText::new(format!(
                "Reporting as {call} / {grid} — SDRoxide {}",
                env!("CARGO_PKG_VERSION")
            ))
            .weak(),
        );
        ui.label(RichText::new("Callsign and grid are set on the General tab.").weak());
    }
    // The status line is shared by every network feed, so only show it here
    // when it is actually ours.
    if let Some(s) = status {
        if s.starts_with("FreeDV Reporter") {
            ui.label(RichText::new(s).weak());
        }
    }

    ui.add_space(8.0);
    if crate::chrome::chip_accent(
        ui,
        false,
        RichText::new(" APPLY ").strong(),
        crate::theme::GREEN(),
        crate::theme::INK_ON_CYAN(),
    )
    .on_hover_text("Persist and (re)connect")
    .clicked()
    {
        *apply = true;
    }
}

/// The broadcast-station block on the Spots settings tab: where the schedule
/// came from, and the three things that can be done to it.
#[cfg(not(target_arch = "wasm32"))]
pub(in crate::app) fn broadcast_stations_settings(
    ui: &mut egui::Ui,
    reload: &mut bool,
    refetch: &mut bool,
    fetching: bool,
    status: Option<&Result<String, String>>,
) {
    let (season, cached) = sdroxide_config::broadcast_schedule_status();
    ui.label(
        RichText::new(
            "The longwave and shortwave stations labelled on the waterfall. The shortwave \
             schedule is downloaded from eibispace.de for the current broadcasting season and \
             re-downloaded when the season changes.",
        )
        .weak(),
    );
    ui.horizontal(|ui| {
        ui.label(RichText::new("Season").size(11.0).color(crate::theme::gray(150)));
        ui.label(
            RichText::new(season.to_uppercase())
                .monospace()
                .size(11.0)
                .color(crate::theme::TEXT_STRONG()),
        );
        let (text, colour) = if fetching {
            ("downloading…".to_string(), crate::theme::YELLOW())
        } else if cached {
            ("downloaded".to_string(), crate::theme::GREEN())
        } else {
            ("using the built-in copy".to_string(), crate::theme::gray(150))
        };
        ui.label(RichText::new(text).size(11.0).color(colour));
    });
    if let Some(Err(e)) = status {
        // Worth showing, not worth alarming over: the built-in schedule is still
        // in use, so the only thing lost is freshness.
        ui.label(
            RichText::new(format!("⚠ Download failed: {e}. Using the built-in schedule."))
                .color(crate::theme::YELLOW())
                .size(11.0),
        );
    }

    ui.add_space(4.0);
    ui.label(
        RichText::new(
            "Your own stations and corrections go in this file. sdroxide never writes it, and \
             an entry here replaces a scheduled one with the same name and frequency.",
        )
        .weak(),
    );
    if let Ok(p) = sdroxide_config::broadcast_stations_path() {
        ui.add(
            egui::Label::new(
                RichText::new(p.display().to_string())
                    .monospace()
                    .size(10.5)
                    .color(crate::theme::gray(150)),
            )
            .truncate(),
        );
    }
    ui.horizontal(|ui| {
        if ui.button("Reload").on_hover_text("Re-read your station file after editing it").clicked()
        {
            *reload = true;
        }
        if ui
            .add_enabled(!fetching, egui::Button::new("Download schedule now"))
            .on_hover_text("Fetch this season's schedule again, replacing the cached copy")
            .clicked()
        {
            *refetch = true;
        }
    });
}

/// The browser client reads the schedule compiled into the wasm bundle, so there
/// is nothing to download and no file to overlay.
#[cfg(target_arch = "wasm32")]
pub(in crate::app) fn broadcast_stations_settings(
    ui: &mut egui::Ui,
    _reload: &mut bool,
    _refetch: &mut bool,
    _fetching: bool,
    _status: Option<&Result<String, String>>,
) {
    ui.label(
        RichText::new(
            "The broadcast stations labelled on the waterfall come from the list built into \
             this build. Editing them needs the desktop app.",
        )
        .weak(),
    );
}

impl crate::app::SdroxideApp {
    /// One "Test" button and its answer, for a logging service's credentials.
    ///
    /// The point of it: today the first sign that a password is wrong is a QSO
    /// that failed to upload, hours after the contact. This asks the service
    /// directly, before anything depends on the answer.
    ///
    /// ⛔ It sends the EDITED config first. The engine tests what it has
    /// applied, so without that a freshly pasted key would be checked against
    /// the old one, and the operator would be told their new credentials were
    /// wrong when they had simply not been saved yet. Pressing Test therefore
    /// also applies the network settings, which is what someone pressing it
    /// means, and the label says so.
    ///
    /// Nothing is cached between sessions: a green tick from an hour ago says
    /// nothing about the password typed since.
    pub(in crate::app) fn login_test_row(
        &self,
        ui: &mut egui::Ui,
        cmds: &mut Vec<sdroxide_types::Command>,
        edited: &sdroxide_types::NetworkConfig,
        target: sdroxide_types::LoginTarget,
    ) {
        ui.horizontal(|ui| {
            let pending = self.login_tests_pending.contains(&target);
            let btn = ui.add_enabled(
                !pending,
                egui::Button::new(RichText::new(format!("Test {}", target.label())).size(11.0)),
            );
            if btn
                .on_hover_text(
                    "Ask the service whether these credentials work. Applies the settings above \
                     first, and logs nothing: it only reads.",
                )
                .clicked()
            {
                cmds.push(sdroxide_types::Command::SetNetworkConfig(edited.clone()));
                cmds.push(sdroxide_types::Command::TestLogin(target));
            }
            if pending {
                ui.label(RichText::new("checking…").size(11.0).weak());
            } else if let Some(r) = self.login_tests.get(&target) {
                let (mark, colour) = if r.ok {
                    ("✔", crate::theme::GREEN())
                } else {
                    ("✖", Color32::from_rgb(255, 120, 120))
                };
                ui.label(RichText::new(format!("{mark} {}", r.message)).size(11.0).color(colour));
            }
        });
    }
}
