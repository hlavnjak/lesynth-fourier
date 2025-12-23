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
use nih_plug_egui::egui::{Align2, Color32, RichText};
use egui_plot::{Line, Plot, PlotPoints, Text};
use crate::constants::{LABEL_FONT_SIZE, TWO_PI};
use crate::engine::{ChartType, SynthComputeEngine};

pub fn draw_harmonic_plot(
    ui: &mut nih_plug_egui::egui::Ui,
    title: &str,
    chart_type: ChartType,
    chart_w: f32,
    chart_h: f32,
    synth_compute_engine: &Arc<SynthComputeEngine>,
) {
    ui.label(
        RichText::new(title)
        .strong()
        .size(16.0)
    );

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
        ChartType::Amp => plot.include_y(1.0),
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

                if let Some(last_point) = points.points().last() {
                    let text_widget = RichText::new(format!("H{}", n + 1))
                        .size(LABEL_FONT_SIZE)
                        .color(Color32::WHITE);

                    plot_ui.text(
                        Text::new(*last_point, text_widget)
                            .color(Color32::WHITE)
                            .anchor(Align2::LEFT_CENTER),
                    );
                }

                plot_ui.line(Line::new(points).name(format!("Harmonic {}", n + 1)));
            }
        });
}
