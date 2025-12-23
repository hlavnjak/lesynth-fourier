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
use crate::constants::*;
use crate::engine::{ChartType, SynthComputeEngine};
use crate::params::{CurveType, GranularityLevel, HarmonicParam};

fn style_slider(ui: &mut nih_plug_egui::egui::Ui) {
    use nih_plug_egui::egui::{Color32, Stroke};

    let style = ui.style_mut();

    // Dark styling with prominent blue borders for sliders
    style.visuals.widgets.inactive.bg_fill = Color32::from_gray(45);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(25));
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(2.0, Color32::from_rgb(65, 115, 190));

    style.visuals.widgets.hovered.bg_fill = Color32::from_gray(50);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(30));
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(2.0, Color32::from_rgb(85, 140, 220));

    style.visuals.widgets.active.bg_fill = Color32::from_gray(55);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.5, Color32::from_gray(35));
    style.visuals.widgets.active.fg_stroke = Stroke::new(2.5, Color32::from_rgb(100, 160, 240));

    // Enhanced slider handle
    style.visuals.widgets.inactive.expansion = 2.0;
    style.visuals.widgets.hovered.expansion = 3.0;
    style.visuals.widgets.active.expansion = 4.0;
}

fn style_other_controls(ui: &mut nih_plug_egui::egui::Ui) {
    use nih_plug_egui::egui::{Color32, Stroke};

    let style = ui.style_mut();

    style.visuals.widgets.inactive.bg_fill = Color32::from_gray(45);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(65, 115, 190));
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_gray(200));

    style.visuals.widgets.hovered.bg_fill = Color32::from_gray(55);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.5, Color32::from_rgb(85, 140, 220));
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::from_gray(220));

    style.visuals.widgets.active.bg_fill = Color32::from_gray(65);
    style.visuals.widgets.active.bg_stroke = Stroke::new(2.0, Color32::from_rgb(100, 160, 240));
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, Color32::from_gray(240));

    style.visuals.widgets.open.bg_fill = Color32::from_gray(60);
    style.visuals.widgets.open.bg_stroke = Stroke::new(2.0, Color32::from_rgb(120, 180, 255));
    style.visuals.widgets.open.fg_stroke = Stroke::new(1.0, Color32::from_gray(240));

    // Selection styling (for combo box items)
    style.visuals.selection.bg_fill = Color32::from_rgb(80, 130, 200);
    style.visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(100, 160, 240));

    // Button styling
    style.visuals.button_frame = true;
}

pub fn draw_curve_controls(
    ui: &mut nih_plug_egui::egui::Ui,
    idx: usize,
    chart_type: ChartType,
    harmonic: &HarmonicParam,
    synth_compute_engine: Arc<SynthComputeEngine>,
    setter: &ParamSetter,
    params_changed_action: &dyn Fn(),
    offset_min: f64,
    offset_max: f64,
    sine_amp_min: f64,
    sine_amp_max: f64,
    window_width: f32,
) {
    use nih_plug_egui::egui;

    let (offset, a, b, curve, granularity, wobble_amp, wobble_freq) = match chart_type {
        ChartType::Amp => (
            &harmonic.curve_offset_amp,
            &harmonic.sine_curve_amp_amp,
            &harmonic.sine_curve_freq_amp,
            &harmonic.curve_type_amp,
            &harmonic.granularity_amp,
            &harmonic.wobble_amp_amp,
            &harmonic.wobble_freq_amp,
        ),
        ChartType::Phase => (
            &harmonic.curve_offset_phase,
            &harmonic.sine_curve_amp_phase,
            &harmonic.sine_curve_freq_phase,
            &harmonic.curve_type_phase,
            &harmonic.granularity_phase,
            &harmonic.wobble_amp_phase,
            &harmonic.wobble_freq_phase,
        ),
    };

    // one allocated row, split into 6 equal rects
    let col_w = (window_width / 6.0).max(1.0);

    let line_h = ui.spacing().interact_size.y;
    let vspace = ui.spacing().item_spacing.y;

    // +1 extra line for the value label under each slider
    let row_h = line_h * 3.0 + vspace * 4.0;

    let (_id, row_rect) = ui.allocate_space(egui::vec2(window_width, row_h));
    let pad = egui::vec2(4.0, 2.0);

    let col_rect = |i: usize| -> egui::Rect {
        let min = row_rect.min + egui::vec2(i as f32 * col_w, 0.0) + pad;
        let size = egui::vec2(col_w, row_h) - pad * 2.0;
        egui::Rect::from_min_size(min, size)
    };

    let refill_after_drag = |engine: &SynthComputeEngine, chart_type: &ChartType| {
        match curve.value() {
            CurveType::Sine => engine.fill_sin_curve(idx, chart_type.clone()),
            CurveType::Constant => engine.fill_constant_curve(idx, offset.value(), chart_type.clone()),
        }
    };

    // Column 0: Offset
    {
        let rect = col_rect(0);
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        let param = offset;
        let engine = synth_compute_engine.clone();
        let chart_type_clone = chart_type.clone();

        let granularity_max = granularity.value().as_f64();
        let actual_max = match chart_type {
            ChartType::Amp => granularity_max.min(offset_max),
            ChartType::Phase => offset_max,
        };

        style_slider(&mut col_ui);

        let slider = egui::Slider::from_get_set(offset_min..=actual_max, move |new_val| {
            if let Some(v) = new_val {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, v as f32);
                setter.end_set_parameter(param);
                v
            } else {
                param.value() as f64
            }
        })
        .show_value(false);

        let response = col_ui.add(slider);
        col_ui.label(format!("{:.3} Offset", offset.value() as f64));

        if response.drag_stopped() {
            refill_after_drag(&engine, &chart_type_clone);
            params_changed_action();
        }
    }

    // Column 1: Sine Amp
    {
        let rect = col_rect(1);
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        let param = a;
        let engine = synth_compute_engine.clone();
        let chart_type_clone = chart_type.clone();

        let granularity_max = granularity.value().as_f64();
        let actual_max = match chart_type {
            ChartType::Amp => granularity_max.min(sine_amp_max),
            ChartType::Phase => sine_amp_max,
        };

        style_slider(&mut col_ui);

        let slider = egui::Slider::from_get_set(sine_amp_min..=actual_max, move |new_val| {
            if let Some(v) = new_val {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, v as f32);
                setter.end_set_parameter(param);
                v
            } else {
                param.value() as f64
            }
        })
        .show_value(false);

        let response = col_ui.add(slider);
        col_ui.label(format!("{:.3} Sine Amp.", a.value() as f64));

        if response.drag_stopped() {
            if curve.value() == CurveType::Sine {
                engine.fill_sin_curve(idx, chart_type_clone.clone());
            }
            params_changed_action();
        }
    }

    // Column 2: Sine Freq
    {
        let rect = col_rect(2);
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        let param = b;
        let engine = synth_compute_engine.clone();
        let chart_type_clone = chart_type.clone();

        style_slider(&mut col_ui);

        let slider = egui::Slider::from_get_set(MIN_SINE_FREQ..=MAX_SINE_FREQ, move |new_val| {
            if let Some(vf) = new_val {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, vf as f32);
                setter.end_set_parameter(param);
                vf as f64
            } else {
                param.value() as f64
            }
        })
        .show_value(false);

        let response = col_ui.add(slider);
        col_ui.label(format!("{:.1} Sine Fq.", b.value() as f64));

        if response.drag_stopped() {
            if curve.value() == CurveType::Sine {
                engine.fill_sin_curve(idx, chart_type_clone.clone());
            }
            params_changed_action();
        }
    }

    // Column 3: Wobble Amp
    {
        let rect = col_rect(3);
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        let param = wobble_amp;
        let engine = synth_compute_engine.clone();
        let chart_type_clone = chart_type.clone();

        let granularity_max = granularity.value().as_f64();
        let wobble_max = granularity_max.min(0.2);

        style_slider(&mut col_ui);

        let slider = egui::Slider::from_get_set(0.0..=wobble_max, move |new_val| {
            if let Some(v) = new_val {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, v as f32);
                setter.end_set_parameter(param);
                v as f64
            } else {
                param.value() as f64
            }
        })
        .show_value(false);

        let response = col_ui.add(slider);
        col_ui.label(format!("{:.3} Wobble Amp.", wobble_amp.value() as f64));

        if response.drag_stopped() {
            refill_after_drag(&engine, &chart_type_clone);
            params_changed_action();
        }
    }

    // Column 4: Wobble Freq
    {
        let rect = col_rect(4);
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        let param = wobble_freq;
        let engine = synth_compute_engine.clone();
        let chart_type_clone = chart_type.clone();

        style_slider(&mut col_ui);

        let slider = egui::Slider::from_get_set(10.0..=200.0, move |new_val| {
            if let Some(vf) = new_val {
                setter.begin_set_parameter(param);
                setter.set_parameter(param, vf as f32);
                setter.end_set_parameter(param);
                vf as f64
            } else {
                param.value() as f64
            }
        })
        .show_value(false);

        let response = col_ui.add(slider);
        col_ui.label(format!("{:.1} Wobble Fq.", wobble_freq.value() as f64));

        if response.drag_stopped() {
            refill_after_drag(&engine, &chart_type_clone);
            params_changed_action();
        }
    }

    // Column 5: Enabled + Granularity + CurveType
    {
        let rect = col_rect(5);
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(rect)
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        // Enabled checkbox
        style_other_controls(&mut col_ui);

        let changed = {
            let mut enabled = match chart_type {
                ChartType::Amp => synth_compute_engine
                    .shared_params
                    .harmonic_ampl_enabled
                    .lock()
                    .unwrap(),
                ChartType::Phase => synth_compute_engine
                    .shared_params
                    .harmonic_phase_enabled
                    .lock()
                    .unwrap(),
            };

            col_ui.checkbox(&mut enabled[idx], "Enabled").changed()
        };

        if changed {
            synth_compute_engine.shared_params.mark_all_buffers_dirty();
            synth_compute_engine.update_assembled_chart_with_key24();
            params_changed_action();
        }

        // Granularity combo
        let granularity_combo_id = format!("{:?}_granularity_combo_{}", chart_type, idx);
        egui::ComboBox::from_id_salt(granularity_combo_id)
            .selected_text(match granularity.value() {
                GranularityLevel::UltraLow => "Max: 0.025",
                GranularityLevel::VeryLow => "Max: 0.05",
                GranularityLevel::Low => "Max: 0.1",
                GranularityLevel::Medium => "Max: 0.5",
                GranularityLevel::High => "Max: 1.0",
            })
            .show_ui(&mut col_ui, |ui| {
                style_other_controls(ui);
                for &variant in GranularityLevel::VARIANTS.iter() {
                    let label = match variant {
                        GranularityLevel::UltraLow => "Max: 0.025",
                        GranularityLevel::VeryLow => "Max: 0.05",
                        GranularityLevel::Low => "Max: 0.1",
                        GranularityLevel::Medium => "Max: 0.5",
                        GranularityLevel::High => "Max: 1.0",
                    };

                    if ui.selectable_label(granularity.value() == variant, label).clicked() {
                        setter.begin_set_parameter(granularity);
                        setter.set_parameter(granularity, variant);
                        setter.end_set_parameter(granularity);
                        params_changed_action();
                    }
                }
            });

        // Curve type combo
        let combo_id = format!("{:?}_curve_type_combo_{}", chart_type, idx);
        egui::ComboBox::from_id_salt(combo_id)
            .selected_text(format!("{:?}", curve.value()))
            .show_ui(&mut col_ui, |ui| {
                style_other_controls(ui);
                for &variant in CurveType::VARIANTS.iter() {
                    if ui
                        .selectable_label(curve.value() == variant, format!("{:?}", variant))
                        .clicked()
                    {
                        setter.begin_set_parameter(curve);
                        setter.set_parameter(curve, variant);
                        setter.end_set_parameter(curve);

                        match variant {
                            CurveType::Sine => synth_compute_engine.fill_sin_curve(idx, chart_type.clone()),
                            CurveType::Constant => {
                                synth_compute_engine
                                    .fill_constant_curve(idx, offset.value(), chart_type.clone());
                            }
                        }

                        params_changed_action();
                    }
                }
            });
    }
}
