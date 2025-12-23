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

use nih_plug_egui::egui::{self, Color32, Rect};

pub fn draw_metallic_background(ui: &mut egui::Ui, window_width: f32, window_height: f32) {
    let painter = ui.painter();
    
    // Calculate approximate heights for different UI sections
    let controls_height = window_height  * 0.25 + 10.0;
    let controls_rect = Rect::from_min_size(
        egui::pos2(0.0, 0.0),
        egui::Vec2::new(window_width, controls_height)
    );
    let rest_rect = Rect::from_min_size(
        egui::pos2(0.0, controls_height),
        egui::Vec2::new(window_width, window_height - controls_height)
    );
    
    // Controls area - darker uranium gray for focus
    let controls_color = Color32::from_rgb(32, 35, 39);
    painter.rect_filled(controls_rect, 0.0, controls_color);
    
    // Rest of interface - lighter uranium gray
    let main_color = Color32::from_rgb(42, 45, 49);
    painter.rect_filled(rest_rect, 0.0, main_color);
}
