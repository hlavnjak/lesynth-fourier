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

use std::sync::Arc;
use nih_plug::prelude::ParamSetter;
use crate::engine::{ChartType, SynthComputeEngine};
use crate::params::{
    nested_base_freq_label, CurveType, GranularityLevel, HarmonicParam,
    NESTED_BASE_FREQ_CHOICES,
};

fn style_slider(ui: &mut nih_plug_egui::egui::Ui) {
    use nih_plug_egui::egui::{Color32, Stroke};
    let style = ui.style_mut();
    style.visuals.widgets.inactive.bg_fill = Color32::from_gray(45);
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_gray(25));
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(2.0, Color32::from_rgb(65, 115, 190));
    style.visuals.widgets.hovered.bg_fill = Color32::from_gray(50);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_gray(30));
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(2.0, Color32::from_rgb(85, 140, 220));
    style.visuals.widgets.active.bg_fill = Color32::from_gray(55);
    style.visuals.widgets.active.bg_stroke = Stroke::new(1.5, Color32::from_gray(35));
    style.visuals.widgets.active.fg_stroke = Stroke::new(2.5, Color32::from_rgb(100, 160, 240));
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
    style.visuals.selection.bg_fill = Color32::from_rgb(80, 130, 200);
    style.visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(100, 160, 240));
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
    window_width: f32,
) {
    use nih_plug_egui::egui;

    let (offset, curve, granularity) = match chart_type {
        ChartType::Amp => (
            &harmonic.curve_offset_amp,
            &harmonic.curve_type_amp,
            &harmonic.granularity_amp,
        ),
        ChartType::Phase => (
            &harmonic.curve_offset_phase,
            &harmonic.curve_type_phase,
            &harmonic.granularity_phase,
        ),
    };

    // 5 columns: offset slider | enabled checkbox | granularity combo |
    // curve type combo | nested-Fourier base frequency combo
    let col0_w = window_width * 0.36;
    let col1_w = window_width * 0.12;
    let col2_w = window_width * 0.17;
    let col3_w = window_width * 0.17;
    let col4_w = (window_width - col0_w - col1_w - col2_w - col3_w).max(1.0);

    let x1 = col0_w;
    let x2 = col0_w + col1_w;
    let x3 = col0_w + col1_w + col2_w;
    let x4 = col0_w + col1_w + col2_w + col3_w;

    let line_h = ui.spacing().interact_size.y;
    let vspace = ui.spacing().item_spacing.y;
    let row_h = line_h * 2.0 + vspace * 2.0;

    let (_id, row_rect) = ui.allocate_space(egui::vec2(window_width, row_h));
    let pad = egui::vec2(4.0, 2.0);

    let make_rect = |x_start: f32, w: f32| -> egui::Rect {
        let min = row_rect.min + egui::vec2(x_start, 0.0) + pad;
        let size = egui::vec2(w, row_h) - pad * 2.0;
        egui::Rect::from_min_size(min, size)
    };

    let refill_after_drag = |engine: &SynthComputeEngine, chart_type: &ChartType| {
        match curve.value() {
            CurveType::Constant => engine.fill_constant_curve(idx, offset.value(), *chart_type),
            CurveType::NestedFourier => engine.fill_nested_fourier_curve(idx, *chart_type),
        }
    };

    // ── Col 0: Offset slider ──────────────────────────────────────────────────
    {
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(make_rect(0.0, col0_w))
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
        col_ui.label(
            nih_plug_egui::egui::RichText::new(format!("{:.3} Offset", offset.value()))
                .strong()
                .color(nih_plug_egui::egui::Color32::WHITE),
        );

        if response.drag_stopped() {
            refill_after_drag(&engine, &chart_type_clone);
            params_changed_action();
        }
    }

    // ── Col 1: Enabled checkbox ───────────────────────────────────────────────
    {
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(make_rect(x1, col1_w))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

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
            col_ui
                .checkbox(
                    &mut enabled[idx],
                    nih_plug_egui::egui::RichText::new("Enabled")
                        .color(nih_plug_egui::egui::Color32::WHITE),
                )
                .changed()
        };

        if changed {
            synth_compute_engine.shared_params.mark_all_buffers_dirty();
            synth_compute_engine.update_assembled_chart_with_key24();
            params_changed_action();
        }
    }

    // ── Col 2: Granularity combo ──────────────────────────────────────────────
    {
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(make_rect(x2, col2_w))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        style_other_controls(&mut col_ui);

        let granularity_combo_id = format!("{:?}_granularity_combo_{}", chart_type, idx);
        egui::ComboBox::from_id_salt(granularity_combo_id)
            .width(col2_w - 8.0)
            .selected_text(
                nih_plug_egui::egui::RichText::new(match granularity.value() {
                    GranularityLevel::Micro     => "Max: 0.001",
                    GranularityLevel::UltraLow => "Max: 0.025",
                    GranularityLevel::VeryLow  => "Max: 0.05",
                    GranularityLevel::Low       => "Max: 0.1",
                    GranularityLevel::Medium    => "Max: 0.5",
                    GranularityLevel::High      => "Max: 1.0",
                })
                .color(nih_plug_egui::egui::Color32::WHITE),
            )
            .show_ui(&mut col_ui, |ui| {
                style_other_controls(ui);
                for &variant in GranularityLevel::VARIANTS.iter() {
                    let label = match variant {
                        GranularityLevel::Micro     => "Max: 0.001",
                        GranularityLevel::UltraLow => "Max: 0.025",
                        GranularityLevel::VeryLow  => "Max: 0.05",
                        GranularityLevel::Low       => "Max: 0.1",
                        GranularityLevel::Medium    => "Max: 0.5",
                        GranularityLevel::High      => "Max: 1.0",
                    };
                    if ui.selectable_label(granularity.value() == variant, label).clicked() {
                        setter.begin_set_parameter(granularity);
                        setter.set_parameter(granularity, variant);
                        setter.end_set_parameter(granularity);
                        params_changed_action();
                    }
                }
            });
    }

    // ── Col 3: Curve type combo ───────────────────────────────────────────────
    {
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(make_rect(x3, col3_w))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        style_other_controls(&mut col_ui);

        let combo_id = format!("{:?}_curve_type_combo_{}", chart_type, idx);
        egui::ComboBox::from_id_salt(combo_id)
            .width(col3_w - 8.0)
            .selected_text(
                nih_plug_egui::egui::RichText::new(format!("{:?}", curve.value()))
                    .color(nih_plug_egui::egui::Color32::WHITE),
            )
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
                            CurveType::Constant => {
                                synth_compute_engine
                                    .fill_constant_curve(idx, offset.value(), chart_type);
                            }
                            CurveType::NestedFourier => {
                                synth_compute_engine.fill_nested_fourier_curve(idx, chart_type);
                            }
                        }

                        params_changed_action();
                    }
                }
            });
    }

    // ── Col 4: Nested-Fourier base frequency combo ────────────────────────────
    //
    // Only a Nested Fourier curve has a fundamental to set, so the box is drawn
    // disabled for a Constant one rather than hidden: the row keeps its shape,
    // and the control is where it will be when the type is switched back.
    {
        let mut col_ui = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(make_rect(x4, col4_w))
                .layout(egui::Layout::top_down(egui::Align::Min)),
        );

        style_other_controls(&mut col_ui);

        let is_nested = curve.value() == CurveType::NestedFourier;
        let grid_secs = synth_compute_engine.grid_duration_secs();
        // The chosen frequency, and what it actually draws: Hz is measured
        // against the grid's own duration, so the useful number to show is how
        // many turns the fundamental makes across the chart. Both come off the
        // series so the label cannot drift from what the engine fills.
        let (current, cycles) = {
            let state = harmonic.nested_fourier.read().unwrap();
            let series = state.series(chart_type);
            (series.base_freq_hz, series.cycles_across_grid(grid_secs))
        };

        col_ui.add_enabled_ui(is_nested, |col_ui| {
            // The popup measures itself once per id and keeps that size for
            // good, so the id carries the list length: a build with more choices
            // must not inherit a box sized for fewer.
            let combo_id = format!(
                "{:?}_nf_base_freq_combo_{}_{}",
                chart_type,
                idx,
                NESTED_BASE_FREQ_CHOICES.len()
            );
            let response = egui::ComboBox::from_id_salt(combo_id)
                .width(col4_w - 8.0)
                .height(420.0)
                .selected_text(
                    nih_plug_egui::egui::RichText::new(nested_base_freq_label(current))
                        .color(nih_plug_egui::egui::Color32::WHITE),
                )
                .show_ui(col_ui, |ui| {
                    style_other_controls(ui);
                    for &hz in NESTED_BASE_FREQ_CHOICES.iter() {
                        if ui
                            .selectable_label(current == hz, nested_base_freq_label(hz))
                            .clicked()
                        {
                            harmonic
                                .nested_fourier
                                .write()
                                .unwrap()
                                .series_mut(chart_type)
                                .base_freq_hz = hz;
                            synth_compute_engine.fill_nested_fourier_curve(idx, chart_type);
                            params_changed_action();
                        }
                    }
                });

            response.response.on_hover_text(format!(
                "Fundamental of this chart's nested-Fourier series.\n\
                 The grid spans {grid_secs:.3} s, so this is {cycles:.2} cycle(s) \
                 across the chart (sub-harmonic k makes k times that).\n\
                 \"auto\" is exactly one cycle across the grid."
            ));
            col_ui.label(
                nih_plug_egui::egui::RichText::new(format!("{cycles:.2} cyc/grid"))
                    .strong()
                    .color(nih_plug_egui::egui::Color32::WHITE),
            );
        });
    }
}
