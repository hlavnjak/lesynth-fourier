// Copyright 2026 Jakub Hlavnicka
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The Modulation tab: what is done to a voice on top of the harmonic grid.
//! For now one amplitude envelope; see [`crate::modulation`].

use egui_plot::{Line, LineStyle, Plot, PlotBounds, PlotPoints, VLine};
use nih_plug::prelude::{ParamSetter};
use nih_plug_egui::egui::{self, Color32, RichText};
use nih_plug_egui::widgets::ParamSlider;

use crate::modulation::Envelope;
use crate::params::LeSynthParams;

/// Colour of the envelope wherever it is drawn (here and over the charts).
pub const ENVELOPE_COLOR: Color32 = Color32::from_rgb(255, 196, 92);

pub fn draw_modulation_controls(
    ui: &mut egui::Ui,
    params: &LeSynthParams,
    setter: &ParamSetter,
    width: f32,
    height: f32,
) {
    let mut enabled = params.env_enabled.value();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Amplitude envelope")
                .strong()
                .size(15.0)
                .color(Color32::from_rgb(180, 220, 255)),
        );
        ui.add_space(8.0);
        if ui
            .checkbox(&mut enabled, "Enabled")
            .on_hover_text(
                "Shape every key's level from its press to its release. \
                 Off: keys play the grid as it is.",
            )
            .changed()
        {
            setter.begin_set_parameter(&params.env_enabled);
            setter.set_parameter(&params.env_enabled, enabled);
            setter.end_set_parameter(&params.env_enabled);
        }
    });
    ui.label(
        RichText::new(
            "Applied to each key as it plays, on top of the harmonic grid: attack, hold \
             and decay run from the key press, release from the key coming back up. \
             The grid itself is not changed. While enabled, the charts below show the \
             sound with the envelope applied, for a key held over the whole sound.",
        )
        .size(11.0)
        .color(Color32::from_gray(190)),
    );
    ui.add_space(4.0);

    const CURVE_HINT: &str = "0 = straight line. Positive = exponential-like \
        (attack starts slowly; decay and release drop fast, then tail off). \
        Negative = the mirror shape.";
    let rows: [(&str, &str); 8] = [
        ("Attack", "Time from the key press to full level"),
        ("Attack curve", CURVE_HINT),
        ("Decay", "Time from full level down to the sustain level"),
        ("Decay curve", CURVE_HINT),
        ("Release", "Time to silence once the key comes back up"),
        ("Release curve", CURVE_HINT),
        ("Hold", "Time kept at full level before the decay"),
        ("Sustain", "Level kept while the key stays down (0..1)"),
    ];

    // Sliders on the left, the envelope's shape on the right.
    const CONTROLS_W: f32 = 530.0;
    ui.horizontal_top(|ui| {
        ui.add_enabled_ui(enabled, |ui| {
            egui::Grid::new("envelope_sliders")
                .num_columns(4)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    for (i, (label, hint)) in rows.iter().enumerate() {
                        ui.label(RichText::new(*label).color(Color32::WHITE))
                            .on_hover_text(*hint);
                        // `ParamSlider` wants the concrete param type.
                        match i {
                            0 => ui.add(ParamSlider::for_param(&params.env_attack, setter)),
                            1 => ui.add(ParamSlider::for_param(&params.env_attack_curve, setter)),
                            2 => ui.add(ParamSlider::for_param(&params.env_decay, setter)),
                            3 => ui.add(ParamSlider::for_param(&params.env_decay_curve, setter)),
                            4 => ui.add(ParamSlider::for_param(&params.env_release, setter)),
                            5 => ui.add(ParamSlider::for_param(&params.env_release_curve, setter)),
                            6 => ui.add(ParamSlider::for_param(&params.env_hold, setter)),
                            _ => ui.add(ParamSlider::for_param(&params.env_sustain, setter)),
                        };
                        if i % 2 == 1 {
                            ui.end_row();
                        }
                    }
                });
        });

        let plot_w = (width - CONTROLS_W).max(200.0);
        let plot_h = height.max(90.0);
        // Shown even while disabled (greyed), so the shape can be set up
        // before it is switched on.
        let env = Envelope::from_sliders(params);
        draw_envelope_shape(ui, &env, enabled, plot_w, plot_h);
    });
}

/// The envelope over one note: the key held until a little after the sustain
/// level is reached, then released. Time in seconds.
fn draw_envelope_shape(ui: &mut egui::Ui, env: &Envelope, enabled: bool, w: f32, h: f32) {
    let to_sustain = env.time_to_sustain();
    // How long the key is drawn held at the sustain level: enough to read as
    // a plateau next to the stages around it.
    let plateau = ((to_sustain + env.release) * 0.25).max(0.05);
    let gate = to_sustain + plateau;
    let total = gate + env.release;

    const POINTS: usize = 400;
    let points: PlotPoints = (0..=POINTS)
        .map(|i| {
            let t = total * i as f32 / POINTS as f32;
            [t as f64, env.level_at(t, gate) as f64]
        })
        .collect();
    let color = if enabled { ENVELOPE_COLOR } else { Color32::from_gray(120) };

    Plot::new("Envelope Shape Plot")
        .width(w)
        .height(h)
        .allow_zoom(false)
        .allow_scroll(false)
        .allow_drag(false)
        .allow_boxed_zoom(false)
        .x_axis_label("seconds")
        .show(ui, |plot_ui| {
            plot_ui.set_plot_bounds(PlotBounds::from_min_max([0.0, 0.0], [total as f64, 1.05]));
            plot_ui.vline(
                VLine::new(gate as f64)
                    .color(Color32::from_gray(140))
                    .style(LineStyle::dashed_dense())
                    .name("Key released"),
            );
            plot_ui.line(Line::new(points).color(color).width(2.0).name("Envelope"));
        });
}
