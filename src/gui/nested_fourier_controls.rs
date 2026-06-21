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
use crate::engine::{ChartType, SynthComputeEngine};
use crate::params::{GranularityLevel, HarmonicParam, NUM_NESTED_FOURIER_HARMONICS};

fn gran_label(g: GranularityLevel) -> &'static str {
    match g {
        GranularityLevel::Micro    => "0.001",
        GranularityLevel::UltraLow => "0.025",
        GranularityLevel::VeryLow  => "0.05",
        GranularityLevel::Low      => "0.1",
        GranularityLevel::Medium   => "0.5",
        GranularityLevel::High     => "1.0",
    }
}

pub fn draw_nested_fourier_controls(
    ui: &mut nih_plug_egui::egui::Ui,
    harmonic_idx: usize,
    chart_type: ChartType,
    harmonic: &HarmonicParam,
    synth_compute_engine: Arc<SynthComputeEngine>,
    params_changed_action: &dyn Fn(),
    window_width: f32,
) {
    use nih_plug_egui::egui::{self, Color32, RichText, Stroke};

    // Each chart (amplitude / phase) drives its own independent Fourier series,
    // stored as persisted (non-automatable) serde state behind an RwLock.
    let nf = &harmonic.nested_fourier;

    let amp_slider_h = 80.0;
    let phase_slider_h = 20.0;
    let col_w = 56.0_f32;

    egui::ScrollArea::horizontal()
        .id_salt(format!("nf_scroll_{:?}_{}_{}", chart_type, harmonic_idx, ui.id().value()))
        .max_width(window_width)
        .show(ui, |ui| {
    ui.horizontal(|ui| {
        for sub_idx in 0..NUM_NESTED_FOURIER_HARMONICS {
            let engine = synth_compute_engine.clone();

            // Snapshot this sub-harmonic's current amp/phase for the frame.
            let (cur_amp, cur_phase) = {
                let state = nf.read().unwrap();
                let series = state.series(chart_type);
                (series.amps[sub_idx], series.phases[sub_idx])
            };

            // Per-slider granularity caps the amplitude slider's range. This is
            // GUI-only state (kept in egui memory): it persists across frames but
            // resets when the editor is reopened. The slider VALUE persists normally.
            let gran_id = egui::Id::new(("nf_gran", chart_type, harmonic_idx, sub_idx));
            let mut gran: GranularityLevel =
                ui.data_mut(|d| d.get_temp(gran_id)).unwrap_or(GranularityLevel::High);
            let gran_max = gran.as_f64();

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

                // Bind the slider directly to a local value (read once per frame) and
                // push changes to the parameter on change. This avoids the inverted /
                // jumpy behaviour that `Slider::from_get_set` exhibits for vertical
                // sliders.
                let mut amp_val = cur_amp as f64;
                let amp_slider = egui::Slider::new(&mut amp_val, 0.0..=gran_max)
                    .vertical()
                    .show_value(false);
                let amp_resp = ui.add_sized([col_w - 4.0, amp_slider_h], amp_slider);
                if amp_resp.changed() {
                    nf.write().unwrap().series_mut(chart_type).amps[sub_idx] = amp_val as f32;
                }

                // Breathing room between the amp slider and the granularity box.
                ui.add_space(8.0);

                // Per-slider granularity selector (sets this slider's amplitude max).
                {
                    let style = ui.style_mut();
                    style.visuals.widgets.inactive.bg_fill = Color32::from_gray(40);
                    style.visuals.widgets.hovered.bg_fill = Color32::from_gray(48);
                    style.visuals.widgets.active.bg_fill = Color32::from_gray(55);
                }
                egui::ComboBox::from_id_salt(("nf_gran_combo", chart_type, harmonic_idx, sub_idx))
                    .width(col_w - 8.0)
                    .selected_text(
                        RichText::new(gran_label(gran))
                            .size(9.0)
                            .color(Color32::from_gray(200)),
                    )
                    .show_ui(ui, |ui| {
                        for &variant in GranularityLevel::VARIANTS.iter() {
                            if ui
                                .selectable_label(gran == variant, gran_label(variant))
                                .clicked()
                            {
                                gran = variant;
                                ui.data_mut(|d| d.insert_temp(gran_id, variant));
                                // Clamp the stored value down if it now exceeds the new max.
                                let new_max = variant.as_f64() as f32;
                                let clamped = {
                                    let mut state = nf.write().unwrap();
                                    let amp = &mut state.series_mut(chart_type).amps[sub_idx];
                                    if *amp > new_max {
                                        *amp = new_max;
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if clamped {
                                    engine.fill_nested_fourier_curve(harmonic_idx, chart_type);
                                    params_changed_action();
                                }
                            }
                        }
                    });

                // Breathing room between the granularity box and the phase slider.
                ui.add_space(8.0);

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
                let mut ph_val = cur_phase as f64;
                let phase_slider = egui::Slider::new(&mut ph_val, -pi..=pi).show_value(false);
                let phase_resp = ui.add_sized([col_w - 4.0, phase_slider_h], phase_slider);
                if phase_resp.changed() {
                    nf.write().unwrap().series_mut(chart_type).phases[sub_idx] = ph_val as f32;
                }

                ui.label(
                    RichText::new(format!("A{:.3}\nφ{:.2}", amp_val, ph_val))
                        .size(9.0)
                        .color(Color32::from_gray(190)),
                );

                if amp_resp.drag_stopped() || phase_resp.drag_stopped() {
                    engine.fill_nested_fourier_curve(harmonic_idx, chart_type);
                    params_changed_action();
                }
            });
        }
    }); // horizontal
    }); // ScrollArea
}
