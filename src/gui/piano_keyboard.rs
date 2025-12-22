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
use nih_plug_egui::egui::{Color32, CornerRadius, StrokeKind, Stroke, Vec2, Rect, pos2};
use crate::constants::NUM_KEYS;
use crate::engine::SynthComputeEngine;
use crate::engine::shared_params::BufferState;
use crate::voice::Voice;

fn is_black_key(key_index: usize) -> bool {
    let octave_pos = key_index % 12;
    matches!(octave_pos, 1 | 3 | 6 | 8 | 10)
}

fn get_white_key_index(key_index: usize) -> usize {
    let octave = key_index / 12;
    let octave_pos = key_index % 12;
    let white_keys_in_octave = [0, 0, 1, 1, 2, 3, 3, 4, 4, 5, 5, 6];
    octave * 7 + white_keys_in_octave[octave_pos]
}

fn get_black_key_offset(key_index: usize) -> f32 {
    let octave_pos = key_index % 12;
    match octave_pos {
        1 => 0.7,   // C#
        3 => 1.7,   // D#
        6 => 3.3,   // F#
        8 => 4.3,   // G#
        10 => 5.3,  // A#
        _ => 0.0,
    }
}

pub fn draw_piano_keyboard(
    egui_ctx: &nih_plug_egui::egui::Context,
    ui: &mut nih_plug_egui::egui::Ui,
    input: &nih_plug_egui::egui::InputState,
    last_key_id: nih_plug_egui::egui::Id,
    last_key_id_persist: nih_plug_egui::egui::Id,
    synth_compute_engine: &Arc<SynthComputeEngine>,
    window_width: f32
) {
    let mut last_pressed_key = egui_ctx
        .memory(|mem| mem.data.get_temp::<Option<usize>>(last_key_id).unwrap_or(None));

    let mut last_pressed_key_persist = egui_ctx
        .memory(|mem| mem.data.get_temp::<Option<usize>>(last_key_id_persist).unwrap_or(Some(15)));

    let keyboard_height = 80.0;
    let white_key_height = keyboard_height;
    let black_key_height = keyboard_height * 0.6;
    
    // Calculate number of white keys for proper spacing
    let actual_white_keys = (0..NUM_KEYS).filter(|&i| !is_black_key(i)).count();
    let white_key_width = window_width / actual_white_keys as f32;
    let black_key_width = white_key_width * 0.6;

    let (kb_rect, _kb_resp) = ui.allocate_exact_size(
        Vec2::new(window_width, keyboard_height),
        nih_plug_egui::egui::Sense::hover(),
    );

    let mut pressed_this_frame: Option<usize> = None;

    // Check if any voice is currently active for visual feedback
    let active_voices = {
        let shared = &synth_compute_engine.shared_params;
        let voices = shared.voices.lock().unwrap();
        (0..NUM_KEYS).filter(|&i| voices[i].is_some()).collect::<Vec<_>>()
    };

    // Get buffer states for visual feedback
    let buffer_states = {
        let shared = &synth_compute_engine.shared_params;
        let states = shared.buffer_states.lock().unwrap();
        states.clone()
    };

    // Determine overall computation status
    let (computing_count, dirty_count) = buffer_states.iter().fold((0, 0), |(computing, dirty), state| {
        match state {
            BufferState::Computing => (computing + 1, dirty),
            BufferState::Dirty => (computing, dirty + 1),
            BufferState::Clean => (computing, dirty),
        }
    });

    let status_text = if computing_count > 0 {
        format!("Recomputing the final sound ({} keys remaining)", computing_count + dirty_count)
    } else if dirty_count > 0 {
        format!("Recomputing the final sound ({} keys pending)", dirty_count)
    } else {
        "Synthesis finished".to_string()
    };

    let status_color = if computing_count > 0 || dirty_count > 0 {
        Color32::from_rgb(200, 100, 50) // Orange for computing/pending
    } else {
        Color32::from_rgb(50, 150, 50) // Green for finished
    };

    // Draw status label above keyboard
    ui.horizontal(|ui| {
        ui.add_space(10.0);
        ui.colored_label(status_color, &status_text);
    });
    ui.add_space(5.0);

    // Draw white keys first
    for key_idx in 0..NUM_KEYS {
        if is_black_key(key_idx) {
            continue;
        }

        let white_key_idx = get_white_key_index(key_idx);
        let x = kb_rect.left() + white_key_idx as f32 * white_key_width;
        let key_rect = Rect::from_min_size(
            pos2(x, kb_rect.top()),
            Vec2::new(white_key_width - 1.0, white_key_height),
        );

        let resp = ui.interact(
            key_rect,
            nih_plug_egui::egui::Id::new(format!("white_key_{}", key_idx)),
            nih_plug_egui::egui::Sense::click(),
        );

        // Determine key color based on state
        let key_color = if active_voices.contains(&key_idx) {
            Color32::from_rgb(200, 220, 255) // Light blue for active
        } else if resp.hovered() {
            Color32::from_rgb(245, 245, 245) // Light gray for hover
        } else {
            match buffer_states[key_idx] {
                BufferState::Clean => Color32::WHITE, // Normal - buffer ready
                BufferState::Dirty => Color32::from_rgb(230, 230, 230), // Light shadow - needs recomputation
                BufferState::Computing => Color32::from_rgb(255, 255, 200), // Light yellow - currently computing
            }
        };

        // Draw white key with rounded corners
        ui.painter().rect_filled(
            key_rect,
            CornerRadius::same(3),
            key_color,
        );
        
        // Add subtle shadow/border
        ui.painter().rect_stroke(
            key_rect,
            CornerRadius::same(3),
            Stroke::new(1.0, Color32::from_rgb(180, 180, 180)),
            StrokeKind::Outside,
        );

        if resp.is_pointer_button_down_on() && input.pointer.any_pressed() {
            pressed_this_frame = Some(key_idx);
        }
    }

    // Draw black keys on top
    for key_idx in 0..NUM_KEYS {
        if !is_black_key(key_idx) {
            continue;
        }

        let octave = key_idx / 12;
        let black_key_offset = get_black_key_offset(key_idx);
        let x = kb_rect.left() + (octave as f32 * 7.0 + black_key_offset) * white_key_width - black_key_width / 2.0;
        let key_rect = Rect::from_min_size(
            pos2(x, kb_rect.top()),
            Vec2::new(black_key_width, black_key_height),
        );

        let resp = ui.interact(
            key_rect,
            nih_plug_egui::egui::Id::new(format!("black_key_{}", key_idx)),
            nih_plug_egui::egui::Sense::click(),
        );

        // Determine key color based on state
        let key_color = if active_voices.contains(&key_idx) {
            Color32::from_rgb(100, 120, 180) // Darker blue for active black key
        } else if resp.hovered() {
            Color32::from_rgb(60, 60, 60) // Lighter black for hover
        } else {
            match buffer_states[key_idx] {
                BufferState::Clean => Color32::from_rgb(30, 30, 30), // Normal - buffer ready
                BufferState::Dirty => Color32::from_rgb(60, 60, 60), // Lighter shadow - needs recomputation
                BufferState::Computing => Color32::from_rgb(80, 80, 40), // Darker yellow - currently computing
            }
        };

        // Draw black key with rounded corners
        ui.painter().rect_filled(
            key_rect,
            CornerRadius::same(2),
            key_color,
        );
        
        // Add subtle highlight on top edge
        let highlight_rect = Rect::from_min_size(
            pos2(x + 2.0, kb_rect.top() + 2.0),
            Vec2::new(black_key_width - 4.0, 3.0),
        );
        ui.painter().rect_filled(
            highlight_rect,
            CornerRadius::same(1),
            Color32::from_rgb(80, 80, 80),
        );

        if resp.is_pointer_button_down_on() && input.pointer.any_pressed() {
            pressed_this_frame = Some(key_idx);
        }
    }

    let released = input.pointer.any_released();
    
    // Handle computer keyboard shortcuts
    let mut keyboard_pressed_key: Option<usize> = None;
    let mut keyboard_released_key: Option<usize> = None;
    
    // Map computer keyboard keys to piano keys (starting from C4 = key 48)
    let base_key = 48; // C4
    for event in &input.events {
        if let nih_plug_egui::egui::Event::Key { key, pressed, .. } = event {
            let piano_key = match key {
                // White keys: ASDFGHJK (C, D, E, F, G, A, B)
                nih_plug_egui::egui::Key::A => Some(base_key + 0),      // C
                nih_plug_egui::egui::Key::S => Some(base_key + 2),      // D
                nih_plug_egui::egui::Key::D => Some(base_key + 4),      // E
                nih_plug_egui::egui::Key::F => Some(base_key + 5),      // F
                nih_plug_egui::egui::Key::G => Some(base_key + 7),      // G
                nih_plug_egui::egui::Key::H => Some(base_key + 9),      // A
                nih_plug_egui::egui::Key::J => Some(base_key + 11),     // B
                nih_plug_egui::egui::Key::K => Some(base_key + 12),     // C (next octave)
                
                // Black keys: WETYUI (C#, D#, F#, G#, A#)
                nih_plug_egui::egui::Key::W => Some(base_key + 1),      // C#
                nih_plug_egui::egui::Key::E => Some(base_key + 3),      // D#
                nih_plug_egui::egui::Key::T => Some(base_key + 6),      // F#
                nih_plug_egui::egui::Key::Y => Some(base_key + 8),      // G#
                nih_plug_egui::egui::Key::U => Some(base_key + 10),     // A#
                nih_plug_egui::egui::Key::I => Some(base_key + 13),     // C# (next octave)
                
                _ => None,
            };
            
            if let Some(key_idx) = piano_key {
                if key_idx < NUM_KEYS {
                    if *pressed {
                        keyboard_pressed_key = Some(key_idx);
                    } else {
                        keyboard_released_key = Some(key_idx);
                    }
                }
            }
        }
    }

    if let Some(key_idx) = pressed_this_frame.or(keyboard_pressed_key) {
        if Some(key_idx) != last_pressed_key {
            log::debug!("Key {} clicked", key_idx);
            {
                let shared = &synth_compute_engine.shared_params;
                let buf = synth_compute_engine.get_buffer_for_key(key_idx);
                let mut voices = shared.voices.lock().unwrap();
                voices[key_idx] = Some(Voice {
                    buffer: buf,
                    idx: 0,
                    fade_in_active: true,
                    fade_in_pos: 0,
                    fade_out_active: false,
                    fade_out_pos: 0,
                });
            }
            synth_compute_engine.update_plotted_mix();
            last_pressed_key = Some(key_idx);
            last_pressed_key_persist = Some(key_idx);
        }
    } else if released || keyboard_released_key.is_some() {
        let release_key = if let Some(kb_key) = keyboard_released_key {
            Some(kb_key)
        } else {
            last_pressed_key
        };
        
        if let Some(prev_key) = release_key {
            log::debug!("Key {} released", prev_key);
            {
                let shared = &synth_compute_engine.shared_params;
                let mut voices = shared.voices.lock().unwrap();
                if let Some(v) = voices[prev_key].as_mut() {
                    v.fade_out_active = true;
                    v.fade_out_pos = 0;
                }
            }

            synth_compute_engine.update_plotted_mix();
            last_pressed_key = None;
        }
    }

    // Persist the updated values back into memory
    egui_ctx.memory_mut(|mem| {
        mem.data.insert_temp(last_key_id, last_pressed_key);
        mem.data
            .insert_temp(last_key_id_persist, last_pressed_key_persist);
    });
}
