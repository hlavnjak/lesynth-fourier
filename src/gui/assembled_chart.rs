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
use egui_plot::{Line, Plot, PlotBounds, PlotPoints};
use crate::engine::SynthComputeEngine;

pub fn draw_assembled_chart(ui: &mut nih_plug_egui::egui::Ui, synth_compute_engine: &Arc<SynthComputeEngine>, window_width : f32, window_height : f32)  {
    // Check if we should reset the view to default range (0-2000)
    let should_reset_view = synth_compute_engine
        .shared_params
        .should_reset_chart_view
        .load(std::sync::atomic::Ordering::Relaxed);
    let chart_width = window_width - 10.0;
    //TODO replace with coeficient*window_height
    let chart_height = window_height * 0.25;

    Plot::new("Assembled Sound Plot")
        .height(chart_height.max(100.0))
        .width(chart_width.max(200.0))
        .include_y(-1.0)
        .include_y(1.0)
        .include_x(1.0)
        .auto_bounds([true, false])
        .allow_zoom([true, false])
        .allow_scroll([true, false])
        .allow_drag([true, false])
        .allow_boxed_zoom(false)
        .show(ui, |plot_ui| {
            let assembled = synth_compute_engine
                .shared_params
                .assembled_sound_plotted
                .lock()
                .unwrap();

            let points: PlotPoints = assembled
                .iter()
                .enumerate()
                .map(|(i, &sample)| [i as f64, sample as f64])
                .collect();

            let bounds = plot_ui.plot_bounds();
            let x_min = bounds.min()[0];
            let x_max = bounds.max()[0];
            let y_min = bounds.min()[1];
            let y_max = bounds.max()[1];

            // Reset view to default range (0-2000) when parameters are edited
            if should_reset_view {
                let default_bounds = PlotBounds::from_min_max([0.0, -1.0], [2000.0, 1.0]);
                plot_ui.set_plot_bounds(default_bounds);
            } else if x_min < 0.0 {
                let clamped_bounds = PlotBounds::from_min_max([0.0, y_min], [x_max, y_max]);
                plot_ui.set_plot_bounds(clamped_bounds);
            }

            plot_ui.line(Line::new(points).name("Sound"));
        });
        
    // Clear the reset flag after use
    if should_reset_view {
        synth_compute_engine
            .shared_params
            .should_reset_chart_view
            .store(false, std::sync::atomic::Ordering::Relaxed);
    }
}
