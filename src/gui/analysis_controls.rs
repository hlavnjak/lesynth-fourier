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

//! Analysis-mode controls: a grid of per-harmonic enable toggles for the
//! amplitude and phase contributions. In Analysis mode the amp/phase grid is
//! produced by analysing input audio, so the user can no longer "draw" it;
//! instead they sculpt the resynthesis by switching individual harmonics on
//! and off (the requested per-harmonic disable feature).

use std::sync::Arc;
use nih_plug_egui::egui::{self, Color32, RichText};
use crate::engine::{ChartType, HarmonicSource, SynthComputeEngine};
use crate::gui::harmonic_draw::{draw_session, set_draw_session, DrawSession};

/// Top of the "Last harmonic" slider's range.
const LAST_HARMONIC_MAX: usize = 255;

pub fn draw_analysis_controls(
    ui: &mut egui::Ui,
    engine: &Arc<SynthComputeEngine>,
    window_width: f32,
    window_height: f32,
) {
    let shared = &engine.shared_params;

    let num_harmonics = shared.amplitude_data.lock().unwrap().len();

    // Whether analysis data has actually been loaded. When the plugin is opened
    // in plain Synth mode (no subtrack analysed) there is no analysed curve to
    // go back to, so the per-harmonic source boxes do not offer "analysed".
    let has_analysis = *shared.analysis_duration_secs.lock().unwrap() > 0.0;

    ui.label(
        RichText::new("Analysis mode — toggle individual harmonics")
            .strong()
            .size(15.0)
            .color(Color32::from_rgb(180, 220, 255)),
    );
    ui.label(
        RichText::new(
            "The amplitude / phase grid below was extracted from the input audio. \
             Enable or disable harmonics to shape the resynthesis. Each curve can \
             be replaced by your Synth-mode curve or by one you draw on its chart (✏).",
        )
        .size(11.0)
        .color(Color32::from_gray(190)),
    );

    // Report the original tone's pitch range (absolute Hz = base_freq * the
    // per-bucket pitch ratio). Only shown when analysis data is actually loaded.
    if has_analysis {
        let base_freq = *shared.analysis_base_freq.lock().unwrap();
        let ratios = shared.bucket_pitch_ratio.lock().unwrap();
        if base_freq > 0.0 && !ratios.is_empty() {
            let min_ratio = ratios.iter().cloned().fold(f32::INFINITY, f32::min);
            let max_ratio = ratios.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
            let min_pitch = base_freq * min_ratio;
            let max_pitch = base_freq * max_ratio;
            ui.label(
                RichText::new(format!(
                    "Original tone pitch: min {:.1} Hz  ·  max {:.1} Hz",
                    min_pitch, max_pitch
                ))
                .size(12.0)
                .strong()
                .color(Color32::from_rgb(160, 235, 170)),
            );
        }
    }

    ui.add_space(4.0);

    let mut changed = false;

    // Snapshot current enable flags, render toggles, write back any change.
    let mut amp_enabled = shared.harmonic_ampl_enabled.lock().unwrap().clone();
    let mut phase_enabled = shared.harmonic_phase_enabled.lock().unwrap().clone();

    // Bulk controls above the grid: Enable / Disable all, and a slider that
    // sets the last enabled harmonic (harmonics 1..=N on, the rest off, amp
    // and phase alike). The slider reads back the highest harmonic currently
    // enabled, or 0 when none is, so it always agrees with the toggles.
    ui.horizontal(|ui| {
        if ui.button("Enable all").clicked() {
            amp_enabled.iter_mut().for_each(|e| *e = true);
            phase_enabled.iter_mut().for_each(|e| *e = true);
            changed = true;
        }
        if ui.button("Disable all").clicked() {
            amp_enabled.iter_mut().for_each(|e| *e = false);
            phase_enabled.iter_mut().for_each(|e| *e = false);
            changed = true;
        }
        ui.add_space(12.0);
        ui.label("Last harmonic");
        let mut last = (0..num_harmonics)
            .rev()
            .find(|&n| amp_enabled[n] || phase_enabled[n])
            .map_or(0, |n| n + 1);
        // Never clamp: an all-disabled grid shows 0 rather than snapping to 1.
        if ui
            .add(
                egui::Slider::new(&mut last, 1..=LAST_HARMONIC_MAX)
                    .clamping(egui::SliderClamping::Never),
            )
            .on_hover_text("Enable harmonics 1 to N and disable the rest")
            .changed()
        {
            let last = last.min(LAST_HARMONIC_MAX).min(num_harmonics);
            for n in 0..num_harmonics {
                amp_enabled[n] = n < last;
                phase_enabled[n] = n < last;
            }
            changed = true;
        }
    });
    ui.add_space(4.0);

    // Size the toggle grid so the whole control box fills the same
    // window_height * 0.40 region the Synth-mode control box occupies. The
    // header/description labels and the Enable/Disable + slider row take a
    // roughly fixed amount of chrome above the grid; reserve for it so the
    // analysis box matches the Synth box height (and keyboard/charts align).
    const CHROME: f32 = 115.0;
    let grid_height = (window_height * 0.40 - CHROME).max(140.0);
    egui::ScrollArea::both()
        .id_salt("analysis_harmonic_toggles")
        .auto_shrink([false; 2])
        .max_height(grid_height)
        .show(ui, |ui| {
            // Lay the harmonics out in columns. Each entry carries a toggle, a
            // source box and a draw button for amplitude and again for phase.
            let cols = (window_width / 400.0).floor().max(1.0) as usize;
            let per_col = num_harmonics.div_ceil(cols);
            ui.horizontal_top(|ui| {
                for c in 0..cols {
                    ui.vertical(|ui| {
                        for r in 0..per_col {
                            let n = c * per_col + r;
                            if n >= num_harmonics {
                                break;
                            }
                            ui.horizontal(|ui| {
                                // Colour the "H" to match this harmonic's graph
                                // line; keep the number white for readability.
                                let font_id =
                                    egui::TextStyle::Monospace.resolve(ui.style());
                                let mut label = egui::text::LayoutJob::default();
                                label.append(
                                    "H",
                                    0.0,
                                    egui::TextFormat {
                                        font_id: font_id.clone(),
                                        color: crate::gui::harmonic_color(n),
                                        ..Default::default()
                                    },
                                );
                                label.append(
                                    &format!("{:>2}", n + 1),
                                    0.0,
                                    egui::TextFormat {
                                        font_id,
                                        color: Color32::WHITE,
                                        ..Default::default()
                                    },
                                );
                                ui.label(label);
                                if ui
                                    .checkbox(&mut amp_enabled[n], "amp")
                                    .on_hover_text("Include this harmonic's amplitude")
                                    .changed()
                                {
                                    changed = true;
                                }
                                if source_controls(ui, engine, n, ChartType::Amp, has_analysis) {
                                    amp_enabled[n] = true;
                                    changed = true;
                                }
                                if ui
                                    .checkbox(&mut phase_enabled[n], "phase")
                                    .on_hover_text("Apply this harmonic's analysed phase")
                                    .changed()
                                {
                                    changed = true;
                                }
                                if source_controls(ui, engine, n, ChartType::Phase, has_analysis) {
                                    phase_enabled[n] = true;
                                    changed = true;
                                }
                            });
                        }
                    });
                    // Wide gutter between columns so the last "cust" checkbox of
                    // one column is clearly separated from the next column's "H" label.
                    ui.add_space(28.0);
                }
            });
        });

    if changed {
        *shared.harmonic_ampl_enabled.lock().unwrap() = amp_enabled;
        *shared.harmonic_phase_enabled.lock().unwrap() = phase_enabled;
        engine.set_normalization_needed(true);
        shared.mark_all_buffers_dirty();
        engine.update_assembled_chart_with_key24();
    }
}

/// One harmonic's source box and draw button, for its amplitude or its phase.
/// Answers `true` when a drawing was just started — the caller switches the
/// harmonic on, since a curve nobody can see cannot be drawn.
///
/// The box says where the row comes from: the analysis, the harmonic's Synth
/// curve, or a curve drawn by hand. The pencil starts a drawing on the chart
/// (switching the row to "drawn" if it was not), and pressed again stops it.
fn source_controls(
    ui: &mut egui::Ui,
    engine: &Arc<SynthComputeEngine>,
    n: usize,
    chart: ChartType,
    has_analysis: bool,
) -> bool {
    let what = match chart {
        ChartType::Amp => "amplitude",
        ChartType::Phase => "phase",
    };
    let name = |source: HarmonicSource| match source {
        HarmonicSource::Analysed => "analysed",
        HarmonicSource::Synth => "synth",
        HarmonicSource::Drawn => "drawn",
    };
    let current = engine.harmonic_source(n, chart);
    let mut picked = current;
    egui::ComboBox::from_id_salt(("harmonic_source", chart, n))
        .width(74.0)
        .selected_text(name(current))
        .show_ui(ui, |ui| {
            if has_analysis {
                ui.selectable_value(&mut picked, HarmonicSource::Analysed, "analysed")
                    .on_hover_text(format!("The {what} curve measured from the audio"));
            }
            ui.selectable_value(&mut picked, HarmonicSource::Synth, "synth")
                .on_hover_text(format!(
                    "Your Synth-mode {what} curve (Constant / Nested Fourier)"
                ));
            ui.selectable_value(&mut picked, HarmonicSource::Drawn, "drawn")
                .on_hover_text(format!(
                    "A {what} curve drawn by hand — press ✏ to draw it on the chart"
                ));
        })
        .response
        .on_hover_text(format!("Where this harmonic's {what} curve comes from"));
    if picked != current {
        engine.set_harmonic_source(n, chart, picked);
    }

    let session = draw_session(ui.ctx());
    let mine = session.is_some_and(|d| d.chart == chart && d.harmonic == n);
    // A row handed to another source is no longer being drawn.
    if mine && picked != HarmonicSource::Drawn {
        set_draw_session(ui.ctx(), None);
    }
    let pencil = ui
        .add(egui::Button::new("✏").small().selected(mine && picked == HarmonicSource::Drawn))
        .on_hover_text(format!(
            "Draw this harmonic's {what} curve on the chart.\n\nThe pen starts at the \
             left edge. Hold the left mouse button and move right to draw; release it \
             to move the pen up or down without drawing, then press again to carry on \
             from there."
        ));
    if pencil.clicked() {
        if mine {
            engine.commit_drawn();
            set_draw_session(ui.ctx(), None);
        } else {
            engine.set_harmonic_source(n, chart, HarmonicSource::Drawn);
            set_draw_session(ui.ctx(), Some(DrawSession::new(chart, n)));
            return true;
        }
    }
    false
}
