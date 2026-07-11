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
use nih_plug_egui::egui::{self, RichText};
use egui_plot::{Line, Plot, PlotBounds, PlotPoints};
use crate::constants::TWO_PI;
use crate::engine::{ChartType, SynthComputeEngine};

pub fn draw_harmonic_plot(
    ui: &mut nih_plug_egui::egui::Ui,
    title: &str,
    chart_type: ChartType,
    chart_w: f32,
    chart_h: f32,
    synth_compute_engine: &Arc<SynthComputeEngine>,
) {
    let is_amp = matches!(chart_type, ChartType::Amp);

    // The Amplitude chart carries a compact Y-axis "zoom" slider that sets the
    // axis maximum: a smaller max magnifies the curves, a larger max zooms out.
    // Persist it in egui memory so it survives the per-frame redraw.
    let ymax_id = egui::Id::new("live_harmonics_amp_ymax");
    let mut amp_ymax: f32 = ui
        .ctx()
        .memory(|m| m.data.get_temp(ymax_id))
        .unwrap_or(1.0);

    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().size(16.0));
        if is_amp {
            // Push the zoom control away from the main "Amplitude" caption.
            ui.add_space(24.0);
            ui.label(RichText::new("y-axis max").size(12.0));
            ui.add_space(4.0);
            // Keep the slider short so it sits within the label row without
            // crowding or overlapping the chart below.
            ui.spacing_mut().slider_width = 70.0;
            ui.add(egui::Slider::new(&mut amp_ymax, 0.05..=1.0).show_value(false))
                .on_hover_text("Amplitude axis max (zoom)");
        }
    });
    if is_amp {
        ui.ctx().memory_mut(|m| m.data.insert_temp(ymax_id, amp_ymax));
    }

    let plot_id = match chart_type {
        ChartType::Amp => "Amplitude Plot",
        ChartType::Phase => "Phase Plot",
    };

    let mut plot = Plot::new(plot_id)
        .height(chart_h)
        .width(chart_w)
        .allow_zoom([false, false])
        .allow_scroll([false, false])
        .allow_drag([false, false])
        .include_y(0.0);

    // Set different y-axis ranges based on chart type
    plot = match chart_type {
        ChartType::Amp => plot.include_y(amp_ymax as f64),
        ChartType::Phase => plot.include_y(TWO_PI as f64),
    };

    plot.show(ui, |plot_ui| {
            let (data, enabled_flags) = match chart_type {
                ChartType::Amp => (
                    synth_compute_engine
                        .shared_params
                        .amplitude_data
                        .lock()
                        .unwrap(),
                    synth_compute_engine
                        .shared_params
                        .harmonic_ampl_enabled
                        .lock()
                        .unwrap(),
                ),
                ChartType::Phase => (
                    synth_compute_engine
                        .shared_params
                        .phase_data
                        .lock()
                        .unwrap(),
                    synth_compute_engine
                        .shared_params
                        .harmonic_phase_enabled
                        .lock()
                        .unwrap(),
                ),
            };

            for (n, line_data) in data.iter().enumerate() {
                if !enabled_flags[n] || line_data.iter().all(|&x| x.abs() < 1e-10) {
                    continue;
                }

                let points: PlotPoints = line_data
                    .iter()
                    .enumerate()
                    .map(|(i, &val)| [i as f64, val as f64])
                    .collect();

                plot_ui.line(
                    Line::new(points)
                        .color(crate::gui::harmonic_color(n))
                        .name(format!("Harmonic {}", n + 1)),
                );
            }

            // Pin the amplitude axis to exactly [0, amp_ymax] so the slider is a
            // hard zoom. Derive the x-range from the bucket count (all curves
            // share the same length) rather than the current plot bounds, so the
            // x-axis keeps spanning the full number of buckets.
            if is_amp {
                let x_max = data
                    .iter()
                    .map(|d| d.len())
                    .max()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .max(1) as f64;
                plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                    [0.0, 0.0],
                    [x_max, amp_ymax as f64],
                ));
            }
        });
}
