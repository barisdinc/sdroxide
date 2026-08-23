//! The DRM window: what a Digital Radio Mondiale broadcast says about itself.
//!
//! Two things are worth looking at, and they answer different questions. The
//! **sync row** is for tuning one in: it shows how far up the chain the decoder
//! has got, so a station that is present but not decoding says *where* it
//! stopped rather than just staying silent. Everything below it is for once
//! that has succeeded — who is broadcasting, how, and what they are saying
//! about the programme.
//!
//! DRM's own latency is seconds — 400 ms or 2 s of time interleaving before
//! the decoder even starts — so nothing here reacts as quickly as an analog
//! S-meter does. That is the transmission, not the display.

use eframe::egui::{self, Color32, RichText};
use sdroxide_types::{Command, DrmStatus, DrmSync, Mode};

use crate::app::SdroxideApp;

/// Field labels and anything the reader is not meant to look at first.
fn dim_ink() -> Color32 {
    crate::theme::gray(110)
}

/// The colour of one stage's indicator. Deliberately not a red/green pair:
/// "arriving with errors" is the interesting middle state while tuning, and it
/// is the one that says the signal is real but not yet good enough.
fn sync_ink(s: DrmSync) -> Color32 {
    match s {
        DrmSync::Absent => crate::theme::gray(90),
        DrmSync::CrcError => Color32::from_rgb(220, 90, 70),
        DrmSync::DataError => Color32::from_rgb(220, 180, 70),
        DrmSync::Ok => Color32::from_rgb(90, 200, 120),
    }
}

impl SdroxideApp {
    pub(in crate::app) fn on_drm(&mut self, data: DrmStatus) {
        self.drm = Some(data);
    }

    pub(in crate::app) fn drm_window(&mut self, ctx: &egui::Context, cmds: &mut Vec<Command>) {
        if !self.show_drm {
            return;
        }
        let mut open = self.show_drm;
        let resp = egui::Window::new("DRM")
            .id(crate::layout::salted_id(ctx, "DRM"))
            .open(&mut open)
            .frame(crate::chrome::window_frame())
            .resizable(true)
            .default_width(crate::layout::window_w(ctx, 420.0))
            .default_height(crate::layout::window_h(ctx, 380.0))
            .show(ctx, |ui| {
                crate::chrome::window_body_bg(ui);
                self.drm_body(ui)
            });
        if let Some(r) = &resp {
            cmds.extend(r.inner.clone().unwrap_or_default());
        }
        if let Some(r) = &resp {
            crate::chrome::paint_window_border(ctx, &r.response);
        }
        self.show_drm = open;
    }

    fn drm_body(&mut self, ui: &mut egui::Ui) -> Vec<Command> {
        let mut cmds = Vec::new();
        let dim = |s: &str| RichText::new(s).size(9.5).color(dim_ink());

        let Some(d) = self.drm.clone() else {
            ui.label(dim("waiting for the receiver…"));
            return cmds;
        };

        if self.state.rx[0].mode != Mode::Drm {
            ui.label(dim(
                "Not in DRM. Set the mode to DRM on a digital shortwave broadcast — the \
                 dial goes on the channel centre, not on a sideband.",
            ));
            ui.add_space(6.0);
        }

        self.drm_sync_row(ui, &d);
        ui.add_space(8.0);

        if !d.locked {
            ui.label(dim(
                "No DRM signal locked. The decoder needs a few seconds on a clean carrier: \
                 DRM interleaves over 400 ms or 2 s before any of it can be read.",
            ));
            return cmds;
        }

        self.drm_signal(ui, &d);
        ui.add_space(8.0);
        self.drm_service(ui, &d, &mut cmds);
        cmds
    }

    /// How far up the chain the decoder has got, left to right in the order the
    /// stages lock.
    fn drm_sync_row(&self, ui: &mut egui::Ui, d: &DrmStatus) {
        let stages = [
            ("IO", d.io, "Samples reaching the decoder"),
            ("TIME", d.time_sync, "Symbol timing recovered"),
            ("FRAME", d.frame_sync, "Transmission frames found"),
            ("FAC", d.fac, "Fast Access Channel — what the transmission is"),
            ("SDC", d.sdc, "Service Description Channel — what the services are"),
            ("AUDIO", d.audio, "Audio frames decoding"),
        ];
        ui.horizontal_wrapped(|ui| {
            for (label, state, hover) in stages {
                let dot = RichText::new("\u{25cf}").size(11.0).color(sync_ink(state));
                let text = RichText::new(label).size(9.5).color(if state.is_ok() {
                    crate::theme::gray(200)
                } else {
                    dim_ink()
                });
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    ui.label(dot);
                    ui.label(text);
                })
                .response
                .on_hover_text(hover);
                ui.add_space(6.0);
            }
        });
    }

    fn drm_signal(&self, ui: &mut egui::Ui, d: &DrmStatus) {
        let dim = |s: &str| RichText::new(s).size(9.5).color(dim_ink());
        let val = |s: String| RichText::new(s).size(11.0);

        egui::Grid::new("drm-signal").num_columns(4).spacing([14.0, 3.0]).show(ui, |ui| {
            ui.label(dim("SNR"));
            ui.label(val(format!("{:.1} dB", d.snr_db)));
            ui.label(dim("MER"));
            ui.label(val(format!("{:.1} dB", d.wmer_db)));
            ui.end_row();

            ui.label(dim("MODE"));
            ui.label(val(format!(
                "{} / {}",
                d.robustness.map(|r| r.label()).unwrap_or("?"),
                d.bandwidth_khz.map(|b| format!("{b} kHz")).unwrap_or_else(|| "?".into()),
            )))
            .on_hover_text(
                "Robustness mode and channel width. A is for a ground-wave path and \
                 carries the most; D is for a badly scattered sky-wave one and carries \
                 the least.",
            );
            ui.label(dim("INTERLEAVE"));
            ui.label(val(if d.interleaver_long { "2 s".into() } else { "400 ms".into() }))
                .on_hover_text(
                    "How far the transmission spreads each frame in time. Long rides out \
                     deeper fades and takes correspondingly longer to acquire.",
                );
            ui.end_row();

            ui.label(dim("PROTECTION"));
            ui.label(val(format!("B {} / A {}", d.protection_b, d.protection_a)));
            ui.label(dim("OFFSET"));
            ui.label(val(format!("{:+.0} Hz", d.sample_offset_hz))).on_hover_text(
                "Residual sample-clock error against the transmitter. Large and steady \
                 means the receiver's reference is off, not the broadcast.",
            );
            ui.end_row();

            if let Some(dop) = d.doppler_hz {
                ui.label(dim("DOPPLER"));
                ui.label(val(format!("{dop:.1} Hz")));
                ui.label(dim("DELAY"));
                ui.label(val(format!("{:.1} ms", d.delay_ms))).on_hover_text(
                    "Doppler and delay spread of the path — how fast it is moving and how \
                     far apart its echoes arrive.",
                );
                ui.end_row();
            }
        });
    }

    fn drm_service(&self, ui: &mut egui::Ui, d: &DrmStatus, cmds: &mut Vec<Command>) {
        let dim = |s: &str| RichText::new(s).size(9.5).color(dim_ink());

        if !d.service.label.is_empty() {
            ui.label(RichText::new(&d.service.label).size(15.0).strong());
        }

        let mut line = Vec::new();
        if !d.service.country.is_empty() {
            line.push(d.service.country.to_uppercase());
        }
        if !d.service.language.is_empty() {
            line.push(d.service.language.clone());
        }
        if let Some(c) = d.service.codec {
            line.push(c.label().to_string());
        }
        if d.service.bitrate_kbps > 0.0 {
            line.push(format!("{:.1} kbps", d.service.bitrate_kbps));
        }
        line.push(if d.service.stereo { "stereo".into() } else { "mono".into() });
        if !line.is_empty() {
            ui.label(dim(&line.join(" \u{00b7} ")));
        }

        // Only worth a control when the multiplex actually carries a choice.
        if d.audio_services > 1 {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.label(dim("SERVICE"));
                for i in 0..d.audio_services {
                    let on = i == d.current_service;
                    if crate::chrome::chip(ui, on, &format!("{}", i + 1))
                        .on_hover_text("Decode this service of the multiplex")
                        .clicked()
                        && !on
                    {
                        cmds.push(Command::SetDrmService { service: i });
                    }
                }
            });
        }

        if !d.service.text.is_empty() {
            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);
            // The broadcaster's own message, whatever they put in it. Wrapped
            // rather than scrolled: it is a couple of lines at most, and the
            // standard caps it at 128 characters.
            ui.label(RichText::new(&d.service.text).size(11.0));
        }

        if let Some(t) = d.time {
            ui.add_space(6.0);
            ui.label(dim(&format!(
                "broadcaster's clock  {:04}-{:02}-{:02} {:02}:{:02} UTC",
                t.year, t.month, t.day, t.hour, t.minute
            )));
        }
    }
}
