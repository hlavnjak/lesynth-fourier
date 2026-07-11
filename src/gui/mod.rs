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

pub mod analysis_controls;
pub mod piano_keyboard;
pub mod harmonic_plot;
pub mod assembled_chart;
pub mod curve_controls;
pub mod metallic_background;
pub mod nested_fourier_controls;

pub use analysis_controls::draw_analysis_controls;
pub use piano_keyboard::draw_piano_keyboard;
pub use harmonic_plot::draw_harmonic_plot;
pub use assembled_chart::draw_assembled_chart;
pub use curve_controls::draw_curve_controls;
pub use metallic_background::draw_metallic_background;
pub use nested_fourier_controls::draw_nested_fourier_controls;

use nih_plug_egui::egui;

/// Consistent bordered "section card" used to separate the top-level areas of
/// the editor, mirroring the framed sections in the Gemstone DAW host UI: a
/// rounded, bordered, dark-translucent card with an accent heading and a
/// separator. `add_contents` receives the inner `Ui` and its result is
/// returned.
pub fn section<R>(
    ui: &mut egui::Ui,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(16, 20, 28, 205))
        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(72, 96, 132)))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                egui::RichText::new(title)
                    .strong()
                    .size(15.0)
                    .color(egui::Color32::from_rgb(150, 200, 255)),
            );
            ui.separator();
            ui.add_space(2.0);
            add_contents(ui)
        })
        .inner
}