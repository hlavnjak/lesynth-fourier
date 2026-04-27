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
use crate::engine::SynthComputeEngine;
use crate::params::{HarmonicParam, NUM_NESTED_FOURIER_HARMONICS};

pub fn draw_nested_fourier_controls(
    ui: &mut nih_plug_egui::egui::Ui,
    harmonic_idx: usize,
    harmonic: &HarmonicParam,
    synth_compute_engine: Arc<SynthComputeEngine>,
    setter: &ParamSetter,
    params_changed_action: &dyn Fn(),
    window_width: f32,
) {
    use nih_plug_egui::egui::{self, Color32, RichText, Stroke};

    let amp_slider_h = 80.0;
    let phase_slider_h = 20.0;
    let col_w = 56.0_f32;
    let gran_max = harmonic.granularity_amp.value().as_f64();

    egui::ScrollArea::horizontal()
        .id_salt(format!("nf_scroll_{}_{}", harmonic_idx, ui.id().value()))
        .max_width(window_width)
        .show(ui, |ui| {
    ui.horizontal(|ui| {
        for sub_idx in 0..NUM_NESTED_FOURIER_HARMONICS {
            let amp_param   = harmonic.nested_fourier_amps.get(sub_idx);
            let phase_param = harmonic.nested_fourier_phases.get(sub_idx);
            let engine = synth_compute_engine.clone();

            ui.vertical(|ui| {
                ui.set_width(col_w);

                // Sub-harmonic caption
                ui.label(
                    RichText::new(format!("H{}", sub_idx + 1))
                        .size(10.0)
                        .strong()
                        .color(Color32::from_rgb(140, 180, 255)),
                );

                // Amp slider (vertical)
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

                let amp_slider = egui::Slider::from_get_set(0.0..=gran_max, move |new_val| {
                    if let Some(v) = new_val {
                        setter.begin_set_parameter(amp_param);
                        setter.set_parameter(amp_param, v as f32);
                        setter.end_set_parameter(amp_param);
                        v
                    } else {
                        amp_param.value() as f64
                    }
                })
                .vertical()
                .show_value(false);

                let amp_resp = ui.add_sized([col_w - 4.0, amp_slider_h], amp_slider);

                // Phase slider (horizontal)
                {
                    let style = ui.style_mut();
                    style.visuals.widgets.inactive.bg_fill = Color32::from_gray(40);
                    style.visuals.widgets.inactive.fg_stroke =
                        Stroke::new(2.0, Color32::from_rgb(180, 100, 60));
                    style.visuals.widgets.hovered.bg_fill = Color32::from_gray(48);
                    style.visuals.widgets.hovered.fg_stroke =
                        Stroke::new(2.0, Color32::from_rgb(210, 130, 80));
                    style.visuals.widgets.active.bg_fill = Color32::from_gray(55);
                    style.visuals.widgets.active.fg_stroke =
                        Stroke::new(2.5, Color32::from_rgb(240, 160, 100));
                    style.visuals.widgets.inactive.expansion = 1.0;
                    style.visuals.widgets.hovered.expansion = 2.0;
                    style.visuals.widgets.active.expansion = 3.0;
                }

                let pi = std::f64::consts::PI;
                let phase_slider = egui::Slider::from_get_set(-pi..=pi, move |new_val| {
                    if let Some(v) = new_val {
                        setter.begin_set_parameter(phase_param);
                        setter.set_parameter(phase_param, v as f32);
                        setter.end_set_parameter(phase_param);
                        v
                    } else {
                        phase_param.value() as f64
                    }
                })
                .show_value(false);

                let engine2 = engine.clone();
                let phase_resp = ui.add_sized([col_w - 4.0, phase_slider_h], phase_slider);

                ui.label(
                    RichText::new(format!(
                        "A{:.2}\nφ{:.2}",
                        amp_param.value(),
                        phase_param.value(),
                    ))
                    .size(9.0)
                    .color(Color32::from_gray(190)),
                );

                if amp_resp.drag_stopped() {
                    engine.fill_nested_fourier_curve(harmonic_idx);
                    params_changed_action();
                }
                if phase_resp.drag_stopped() {
                    engine2.fill_nested_fourier_curve(harmonic_idx);
                    params_changed_action();
                }
            });
        }
    }); // horizontal
    }); // ScrollArea
}
