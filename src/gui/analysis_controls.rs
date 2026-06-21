// Copyright 2025 Jakub Hlavnicka
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
use crate::engine::{ChartType, SynthComputeEngine};

pub fn draw_analysis_controls(
    ui: &mut egui::Ui,
    engine: &Arc<SynthComputeEngine>,
    window_width: f32,
    window_height: f32,
) {
    let shared = &engine.shared_params;

    let num_harmonics = shared.amplitude_data.lock().unwrap().len();

    ui.label(
        RichText::new("Analysis mode — toggle individual harmonics")
            .strong()
            .size(15.0)
            .color(Color32::from_rgb(180, 220, 255)),
    );
    ui.label(
        RichText::new(
            "The amplitude / phase grid below was extracted from the input audio. \
             Enable or disable harmonics to shape the resynthesis. Tick \"cust\" to \
             override a harmonic's analysed curve with your Synth-mode curve.",
        )
        .size(11.0)
        .color(Color32::from_gray(190)),
    );
    ui.add_space(4.0);

    let mut changed = false;

    // Snapshot current enable flags, render toggles, write back any change.
    let mut amp_enabled = shared.harmonic_ampl_enabled.lock().unwrap().clone();
    let mut phase_enabled = shared.harmonic_phase_enabled.lock().unwrap().clone();
    // Custom-override flags are applied through the engine (which rewrites /
    // restores the harmonic's row), so toggle them immediately on change.
    let mut amp_custom = shared.harmonic_ampl_custom.lock().unwrap().clone();
    let mut phase_custom = shared.harmonic_phase_custom.lock().unwrap().clone();

    // Size the toggle grid so the whole control box fills the same
    // window_height * 0.40 region the Synth-mode control box occupies. The
    // header/description labels and the Enable/Disable buttons take a roughly
    // fixed amount of chrome above and below the grid; reserve for it so the
    // analysis box matches the Synth box height (and keyboard/charts align).
    const CHROME: f32 = 115.0;
    let grid_height = (window_height * 0.40 - CHROME).max(140.0);
    egui::ScrollArea::vertical()
        .id_salt("analysis_harmonic_toggles")
        .auto_shrink([false; 2])
        .max_height(grid_height)
        .show(ui, |ui| {
            // Lay the harmonics out in columns. Each entry now carries four
            // toggles (amp / amp-custom / phase / phase-custom), so widen them.
            let cols = (window_width / 240.0).floor().max(1.0) as usize;
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
                                ui.label(
                                    RichText::new(format!("H{:>2}", n + 1))
                                        .monospace()
                                        .color(Color32::WHITE),
                                );
                                if ui
                                    .checkbox(&mut amp_enabled[n], "amp")
                                    .on_hover_text("Include this harmonic's amplitude")
                                    .changed()
                                {
                                    changed = true;
                                }
                                if ui
                                    .checkbox(&mut amp_custom[n], "cust")
                                    .on_hover_text(
                                        "Override the analysed amplitude curve with your \
                                         Synth-mode curve (Constant / Nested Fourier)",
                                    )
                                    .changed()
                                {
                                    engine.set_harmonic_custom(n, ChartType::Amp, amp_custom[n]);
                                }
                                if ui
                                    .checkbox(&mut phase_enabled[n], "phase")
                                    .on_hover_text("Apply this harmonic's analysed phase")
                                    .changed()
                                {
                                    changed = true;
                                }
                                if ui
                                    .checkbox(&mut phase_custom[n], "cust")
                                    .on_hover_text(
                                        "Override the analysed phase curve with your \
                                         Synth-mode curve (Constant / Nested Fourier)",
                                    )
                                    .changed()
                                {
                                    engine.set_harmonic_custom(
                                        n,
                                        ChartType::Phase,
                                        phase_custom[n],
                                    );
                                }
                            });
                        }
                    });
                    ui.add_space(8.0);
                }
            });
        });

    ui.add_space(4.0);
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
    });

    if changed {
        *shared.harmonic_ampl_enabled.lock().unwrap() = amp_enabled;
        *shared.harmonic_phase_enabled.lock().unwrap() = phase_enabled;
        engine.set_normalization_needed(true);
        shared.mark_all_buffers_dirty();
        engine.update_assembled_chart_with_key24();
    }
}
