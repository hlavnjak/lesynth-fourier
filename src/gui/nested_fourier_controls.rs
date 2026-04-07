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

use std::sync::Arc;
use nih_plug::prelude::ParamSetter;
use crate::constants::NUM_HARMONICS;
use crate::engine::SynthComputeEngine;
use crate::params::{LeSynthParams, NUM_NESTED_FOURIER_HARMONICS};

pub fn draw_nested_fourier_controls(
    ui: &mut nih_plug_egui::egui::Ui,
    synth_params: &LeSynthParams,
    synth_compute_engine: Arc<SynthComputeEngine>,
    setter: &ParamSetter,
    params_changed_action: &dyn Fn(),
    window_width: f32,
) {
    use nih_plug_egui::egui::{self, Color32, RichText, Stroke};

    let selected_id = egui::Id::new("nf_selected_harmonic");
    let mut selected: usize = ui.ctx().memory_mut(|mem| {
        *mem.data.get_temp_mut_or_insert_with(selected_id, || 0usize)
    });

    // ── Header row: label + target-harmonic combo ──────────────────────────
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Nested Fourier Sub-Harmonics  |  Target:")
                .strong()
                .size(13.0)
                .color(Color32::WHITE),
        );

        // Style the combo box
        {
            let style = ui.style_mut();
            style.visuals.widgets.inactive.bg_fill = Color32::from_gray(45);
            style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(65, 115, 190));
            style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_gray(200));
            style.visuals.widgets.hovered.bg_fill = Color32::from_gray(55);
            style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.5, Color32::from_rgb(85, 140, 220));
            style.visuals.widgets.open.bg_fill = Color32::from_gray(60);
            style.visuals.widgets.open.bg_stroke = Stroke::new(2.0, Color32::from_rgb(120, 180, 255));
            style.visuals.selection.bg_fill = Color32::from_rgb(80, 130, 200);
        }

        egui::ComboBox::from_id_salt("nf_target_harmonic_combo")
            .selected_text(
                RichText::new(format!("Harmonic {}", selected + 1)).color(Color32::WHITE),
            )
            .show_ui(ui, |ui| {
                for i in 0..NUM_HARMONICS {
                    if ui
                        .selectable_label(selected == i, format!("Harmonic {}", i + 1))
                        .clicked()
                    {
                        selected = i;
                    }
                }
            });
    });

    // Persist selection
    ui.ctx().memory_mut(|mem| {
        mem.data.insert_temp(selected_id, selected);
    });

    // ── 16 vertical sliders ────────────────────────────────────────────────
    let slider_h = 90.0;
    let col_w = (window_width / NUM_NESTED_FOURIER_HARMONICS as f32).max(28.0);

    ui.horizontal(|ui| {
        for sub_idx in 0..NUM_NESTED_FOURIER_HARMONICS {
            let param = synth_params.harmonics[selected].nested_fourier_amps.get(sub_idx);
            let engine = synth_compute_engine.clone();
            let harmonic_idx = selected;

            ui.vertical(|ui| {
                ui.set_width(col_w);

                // Style sliders dark/blue
                {
                    let style = ui.style_mut();
                    style.visuals.widgets.inactive.bg_fill = Color32::from_gray(45);
                    style.visuals.widgets.inactive.fg_stroke =
                        Stroke::new(2.0, Color32::from_rgb(65, 115, 190));
                    style.visuals.widgets.hovered.bg_fill = Color32::from_gray(50);
                    style.visuals.widgets.hovered.fg_stroke =
                        Stroke::new(2.0, Color32::from_rgb(85, 140, 220));
                    style.visuals.widgets.active.bg_fill = Color32::from_gray(55);
                    style.visuals.widgets.active.fg_stroke =
                        Stroke::new(2.5, Color32::from_rgb(100, 160, 240));
                    style.visuals.widgets.inactive.expansion = 2.0;
                    style.visuals.widgets.hovered.expansion = 3.0;
                    style.visuals.widgets.active.expansion = 4.0;
                }

                let slider =
                    egui::Slider::from_get_set(-1.0..=1.0, move |new_val| {
                        if let Some(v) = new_val {
                            setter.begin_set_parameter(param);
                            setter.set_parameter(param, v as f32);
                            setter.end_set_parameter(param);
                            v
                        } else {
                            param.value() as f64
                        }
                    })
                    .vertical()
                    .show_value(false);

                let response = ui.add_sized([col_w - 4.0, slider_h], slider);

                ui.label(
                    RichText::new(format!("{:.2}\nH{}", param.value(), sub_idx + 1))
                        .size(9.0)
                        .color(Color32::WHITE),
                );

                if response.drag_stopped() {
                    engine.fill_nested_fourier_curve(harmonic_idx);
                    params_changed_action();
                }
            });
        }
    });
}
