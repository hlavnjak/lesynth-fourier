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
use nih_plug_egui::egui::{Color32, CornerRadius, StrokeKind, Stroke, Vec2, Pos2, Rect, pos2};
use crate::constants::NUM_KEYS;
use crate::engine::SynthComputeEngine;
use crate::engine::shared_params::{
    BufferState, KEYBOARD_GAIN_MAX_DB, KEYBOARD_GAIN_MIN_DB, ORIGINAL_PITCH_VOICE,
};
use crate::voice::Voice;

/// Where a key is drawn, and therefore where it is clicked. One function for
/// both, so the two can never disagree about which key is under the pointer.
fn key_rect(key_idx: usize, kb_rect: Rect, white_key_width: f32, keyboard_height: f32) -> Rect {
    if is_black_key(key_idx) {
        let black_key_width = white_key_width * 0.6;
        let x = kb_rect.left() + get_black_key_x_pos(key_idx) * white_key_width
            - black_key_width / 2.0;
        Rect::from_min_size(
            pos2(x, kb_rect.top()),
            Vec2::new(black_key_width, keyboard_height * 0.6),
        )
    } else {
        let x = kb_rect.left() + get_white_key_index(key_idx) as f32 * white_key_width;
        Rect::from_min_size(
            pos2(x, kb_rect.top()),
            Vec2::new(white_key_width - 1.0, keyboard_height),
        )
    }
}

/// Which key a press at `pos` lands on, or `None` for a press off the keys.
///
/// `keys` is in **paint order** — white keys first, then the black ones that are
/// drawn over them — and the search runs backwards, so a black key wins wherever
/// the two overlap, exactly as the drawing does.
fn key_at(pos: Pos2, keys: &[(usize, Rect)]) -> Option<usize> {
    keys.iter().rev().find(|(_, r)| r.contains(pos)).map(|&(i, _)| i)
}

fn is_black_key(key_index: usize) -> bool {
    // Piano starts at A0. Offset by 9 to align with C-based chromatic scale
    // where black keys are C#(1), D#(3), F#(6), G#(8), A#(10)
    let octave_pos = (key_index + 9) % 12;
    matches!(octave_pos, 1 | 3 | 6 | 8 | 10)
}

fn get_white_key_index(key_index: usize) -> usize {
    // Count white keys from A0 up to (but not including) key_index
    (0..key_index).filter(|&i| !is_black_key(i)).count()
}

fn get_black_key_x_pos(key_index: usize) -> f32 {
    // Returns absolute position in white-key-width units from the left edge.
    // Use the C-based adjusted octave so the standard offsets apply.
    let adjusted = key_index + 9;
    let c_octave = adjusted / 12;
    let c_octave_pos = adjusted % 12;
    let within_octave = match c_octave_pos {
        1 => 0.7,   // C#
        3 => 1.7,   // D#
        6 => 3.7,   // F#
        8 => 4.7,   // G#
        10 => 5.7,  // A#
        _ => 0.0,
    };
    // Subtract 5.0 because A0 is the 6th white key (index 5) in the C0-relative system
    c_octave as f32 * 7.0 + within_octave - 5.0
}

pub fn draw_piano_keyboard(
    egui_ctx: &nih_plug_egui::egui::Context,
    ui: &mut nih_plug_egui::egui::Ui,
    input: &nih_plug_egui::egui::InputState,
    last_key_id: nih_plug_egui::egui::Id,
    last_key_id_persist: nih_plug_egui::egui::Id,
    synth_compute_engine: &Arc<SynthComputeEngine>,
    window_width: f32,
    window_height: f32,
    y_offset: f32,
) {
    let mut last_pressed_key = egui_ctx
        .memory(|mem| mem.data.get_temp::<Option<usize>>(last_key_id).unwrap_or(None));

    let mut last_pressed_key_persist = egui_ctx
        .memory(|mem| mem.data.get_temp::<Option<usize>>(last_key_id_persist).unwrap_or(Some(15)));

    // A key whose release arrived in the same frame as its own press, carried
    // over so it can be let go on the next one — see the dispatch below.
    let pending_release_id = last_key_id.with("pending_release");
    let mut pending_release = egui_ctx
        .memory(|mem| mem.data.get_temp::<Option<usize>>(pending_release_id).unwrap_or(None));

    let keyboard_height = window_height * 0.055;

    // Calculate number of white keys for proper spacing
    let actual_white_keys = (0..NUM_KEYS).filter(|&i| !is_black_key(i)).count();
    let white_key_width = window_width / actual_white_keys as f32;
    let black_key_width = white_key_width * 0.6;

    // Every key's rect, in paint order, for the hit test below.
    let mut key_rects: Vec<(usize, Rect)> = Vec::with_capacity(NUM_KEYS);

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

    // Draw status label above keyboard, shifted down by y_offset
    ui.add_space(y_offset);
    // Wrapped, because the row carries the audition button's caption as well as
    // the status text: a narrow window has to fold it onto a second line rather
    // than clip it.
    ui.horizontal_wrapped(|ui| {
        ui.add_space(10.0);
        ui.colored_label(status_color, &status_text);
        ui.separator();
        // Playback mode: loop a held note, or play it once.
        let mut repeat = synth_compute_engine.shared_params.repeat_playback();
        if ui.checkbox(&mut repeat, "Repeat").changed() {
            synth_compute_engine.shared_params.set_repeat_playback(repeat);
        }

        // Audition at the source's own pitch rather than transposed onto a key
        // — the reference for judging a resynthesis by ear.
        let has_analysis = *synth_compute_engine
            .shared_params
            .analysis_duration_secs
            .lock()
            .unwrap()
            > 0.0;
        let playing = synth_compute_engine
            .shared_params
            .voices
            .lock()
            .unwrap()
            .get(ORIGINAL_PITCH_VOICE)
            .map(|v| v.as_ref().is_some_and(|v| !v.fade_out_active))
            .unwrap_or(false);
        let base_freq = *synth_compute_engine
            .shared_params
            .analysis_base_freq
            .lock()
            .unwrap();

        let label = if playing {
            "■ Original Pitch And Gain"
        } else {
            "▶ Original Pitch And Gain"
        };
        let resp = ui
            .add_enabled(has_analysis, nih_plug_egui::egui::Button::new(label).small())
            .on_hover_text(if has_analysis {
                format!(
                    "Play the analysed sound at its original pitch ({:.1} Hz) and at the \
                     source's own level — the reference for comparing a resynthesis by \
                     ear, A/B-able against the source file directly.\n\n\
                     It also keeps the original pitch *per bucket*: every bucket sounds \
                     at the pitch it was analysed at, so the source's own vibrato, \
                     glide and drift survive. A key on the keyboard instead plays every \
                     bucket at one constant pitch — the key's — which is what the pitch \
                     contour is traded away for when the sound is transposed.",
                    base_freq
                )
            } else {
                "Analyse some audio first (Analysis mode)".to_string()
            })
            .on_disabled_hover_text("Analyse some audio first (Analysis mode)");
        // Caption: the difference from a key is not guessable from the button's
        // name, and it is the reason to reach for this button at all.
        ui.label(
            nih_plug_egui::egui::RichText::new(
                "keeps each bucket's original pitch — a key plays one constant pitch",
            )
            .small()
            .color(Color32::from_gray(140)),
        );

        // Sits next to the audition button because it is the other half of the
        // same trade-off: the audition keeps the phases, a key cannot. Separated
        // so it does not read as a second caption for the button.
        ui.separator();
        let mut zero_phases = synth_compute_engine.shared_params.zero_key_phases();
        let zero_resp = ui
            .checkbox(&mut zero_phases, "Zero phases on keys")
            .on_hover_text(
                "Render notes played from the keyboard (and the assembled-sound \
                 chart) with every bucket's phases set to zero.\n\n\
                 A bucket is one period, and its phases are the ones the source \
                 had at *its* pitch. On a key the period is a different length, \
                 so the waveform no longer meets itself at the cycle boundary \
                 and each bucket change steps the signal — the clipping heard at \
                 the period borders. With the phases zeroed every harmonic is a \
                 sine of the fundamental, zero at both ends of the cycle, so the \
                 periods join cleanly. The spectrum is unchanged; the source's \
                 waveform shape is not preserved.\n\n\
                 Never affects Original Pitch And Gain, which plays at the pitch \
                 the phases belong to.",
            );
        if zero_resp.changed() {
            let shared = &synth_compute_engine.shared_params;
            shared.set_zero_key_phases(zero_phases);
            // Every key buffer was rendered with the old setting.
            shared.mark_all_buffers_dirty();
            synth_compute_engine.update_assembled_chart_with_key24();
        }

        // How loud the keyboard plays. A mixdown gain, not a render setting:
        // nothing is recomputed and no key buffer goes dirty, so it takes
        // effect on the note already sounding.
        ui.separator();
        let mut gain_db = synth_compute_engine.shared_params.keyboard_gain_db();
        // Narrower than egui's default: this row is a strip of small controls
        // and a full-width slider would push the status text onto a second line
        // on any window that is not wide.
        let slider_width = ui.spacing().slider_width;
        ui.spacing_mut().slider_width = 90.0;
        let gain_resp = ui
            .add(
                nih_plug_egui::egui::Slider::new(
                    &mut gain_db,
                    KEYBOARD_GAIN_MIN_DB..=KEYBOARD_GAIN_MAX_DB,
                )
                .suffix(" dB")
                .text("Keyboard gain")
                .custom_formatter(|v, _| {
                    if v as f32 <= KEYBOARD_GAIN_MIN_DB {
                        "-∞".to_string()
                    } else {
                        format!("{v:+.1}")
                    }
                }),
            )
            .on_hover_text(
                "How loud notes played from the keyboard are in the mix.                  0 dB is unity — what the keyboard has always played — and the                  bottom of the slider is silence, not a very quiet note.

                 Never applied to Original Pitch And Gain: that audition plays                  at the source's own level so it can be compared with the                  source file directly, and a gain on it would make it a                  different reference every time this moved.

                 Nothing is re-rendered, so it takes effect on the note already                  sounding.",
            );
        ui.spacing_mut().slider_width = slider_width;
        if gain_resp.changed() {
            synth_compute_engine.shared_params.set_keyboard_gain_db(gain_db);
        }

        // How a key is rendering, right now. `build_playback_grid` answers
        // anything it cannot use with `None` and the renderer falls back to the
        // contour path — same call, same signature, and the buzz the true-period
        // grid exists to remove comes straight back. Invisible from outside,
        // which is how it went unnoticed while offline dumps measured clean.
        if has_analysis {
            let on_grid = synth_compute_engine.shared_params.used_playback_grid();
            let (text, colour, hover) = if on_grid {
                (
                    "true-period grid",
                    Color32::from_rgb(50, 150, 50),
                    "Keys are transposing from the source's own true periods — the \
                     render path that matches Original Pitch And Gain.",
                )
            } else {
                (
                    "⚠ contour fallback",
                    Color32::from_rgb(200, 100, 50),
                    "Keys are NOT using the source's true periods: the analysis is \
                     missing the per-bucket lengths (an imported or pre-v3 grid), or \
                     the grid's width no longer matches them. The renderer is then \
                     working from bucket lengths rounded to whole samples, which is \
                     heard as roughness at the bucket rate — the buzz. Re-analyse \
                     the source to get it back.",
                )
            };
            ui.separator();
            ui.colored_label(colour, nih_plug_egui::egui::RichText::new(text).small())
                .on_hover_text(hover);
        }

        if resp.clicked() {
            let shared = &synth_compute_engine.shared_params;
            if playing {
                // Second click stops it, matching the button's ■ state.
                if let Some(v) = shared.voices.lock().unwrap()[ORIGINAL_PITCH_VOICE].as_mut() {
                    v.start_fade_out();
                }
            } else {
                let buf = synth_compute_engine.assemble_buffer_at_original_pitch();
                if !buf.is_empty() {
                    shared.voices.lock().unwrap()[ORIGINAL_PITCH_VOICE] = Some(Voice::new(buf));
                }
            }
            synth_compute_engine.update_plotted_mix();
        }
    });
    ui.add_space(5.0);

    let (kb_rect, _kb_resp) = ui.allocate_exact_size(
        Vec2::new(window_width, keyboard_height),
        nih_plug_egui::egui::Sense::hover(),
    );

    // Draw white keys first
    for key_idx in 0..NUM_KEYS {
        if is_black_key(key_idx) {
            continue;
        }

        let key_rect = key_rect(key_idx, kb_rect, white_key_width, keyboard_height);

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

        key_rects.push((key_idx, key_rect));
    }

    // Draw black keys on top
    for key_idx in 0..NUM_KEYS {
        if !is_black_key(key_idx) {
            continue;
        }

        let key_rect = key_rect(key_idx, kb_rect, white_key_width, keyboard_height);
        let x = key_rect.left();

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

        key_rects.push((key_idx, key_rect));
    }

    // Which key was pressed, from the raw press events rather than a `Response`.
    //
    // `is_pointer_button_down_on()` is the wrong instrument: egui clears that
    // flag for any widget that also saw a *release* in the same frame
    // (`context.rs`, `PointerEvent::Released`), so a click shorter than one
    // repaint was invisible to it and the note was dropped — no voice, and no
    // blue key either, which is how it was spotted. egui says as much on
    // `any_pressed`. `press_origin()` is no help either, being cleared on
    // release; the raw `Event::PointerButton` carries the press position and is
    // never retracted.
    let mut pressed_this_frame: Option<usize> = None;
    for event in &input.events {
        if let nih_plug_egui::egui::Event::PointerButton {
            pos,
            button: nih_plug_egui::egui::PointerButton::Primary,
            pressed: true,
            ..
        } = event
        {
            if let Some(key_idx) = key_at(*pos, &key_rects) {
                pressed_this_frame = Some(key_idx);
            }
        }
    }

    let released = input.pointer.any_released();
    
    // Handle computer keyboard shortcuts
    let mut keyboard_pressed_key: Option<usize> = None;
    let mut keyboard_released_key: Option<usize> = None;
    
    // Map computer keyboard keys to piano keys (starting from C4 = key 39)
    // key 0 = A0 (27.5 Hz), so C4 = A0 + 39 semitones = key 39
    let base_key = 39; // C4
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
        // Let go of whatever was still sounding first. A release and the next
        // press land in the same frame often enough — release, move, click —
        // and while this was an `if`/`else if` the press swallowed the release,
        // so the previous key was never faded out and went on ringing under the
        // new one, still lit blue.
        if let Some(prev_key) = last_pressed_key.filter(|&p| p != key_idx) {
            log::debug!("Key {} released (a new key took over)", prev_key);
            let shared = &synth_compute_engine.shared_params;
            let mut voices = shared.voices.lock().unwrap();
            if let Some(v) = voices[prev_key].as_mut() {
                v.fade_out_active = true;
                v.fade_out_pos = 0;
            }
        }
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
        // A click already over before this frame was drawn still has to be let
        // go, or the note it started would never stop. Carrying it to the next
        // frame also gives it a frame of sound, the least a two-frame click gets.
        // The repaint must be asked for: the editor redraws only on input.
        pending_release = if released { Some(key_idx) } else { None };
        if pending_release.is_some() {
            egui_ctx.request_repaint();
        }
    } else if released || keyboard_released_key.is_some() || pending_release.is_some() {
        let release_key = keyboard_released_key
            .or_else(|| if released { last_pressed_key } else { None })
            .or(pending_release);

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
        pending_release = None;
    }

    // Persist the updated values back into memory
    egui_ctx.memory_mut(|mem| {
        mem.data.insert_temp(last_key_id, last_pressed_key);
        mem.data
            .insert_temp(last_key_id_persist, last_pressed_key_persist);
        mem.data.insert_temp(pending_release_id, pending_release);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keyboard as `draw_piano_keyboard` lays it out, in paint order: white
    /// keys first, then the black keys drawn over them.
    fn laid_out(width: f32, height: f32) -> (Rect, Vec<(usize, Rect)>, f32) {
        let kb = Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(width, height));
        let whites = (0..NUM_KEYS).filter(|&i| !is_black_key(i)).count();
        let w = width / whites as f32;
        let mut keys: Vec<(usize, Rect)> = (0..NUM_KEYS)
            .filter(|&i| !is_black_key(i))
            .map(|i| (i, key_rect(i, kb, w, height)))
            .collect();
        keys.extend(
            (0..NUM_KEYS)
                .filter(|&i| is_black_key(i))
                .map(|i| (i, key_rect(i, kb, w, height))),
        );
        (kb, keys, w)
    }

    /// A press anywhere on a white key that no black key covers must find that
    /// white key, and every white key must be reachable — a hit test that
    /// silently misses is the defect this exists to catch.
    #[test]
    fn every_white_key_is_hit_at_its_own_bottom_edge() {
        let (kb, keys, _) = laid_out(1200.0, 60.0);
        for i in (0..NUM_KEYS).filter(|&i| !is_black_key(i)) {
            let r = key_rect(i, kb, 1200.0 / 52.0, 60.0);
            // Below the black keys (they stop at 60% of the height), so this
            // point belongs to the white key alone.
            let p = pos2(r.center().x, kb.top() + 55.0);
            assert_eq!(key_at(p, &keys), Some(i), "white key {i} missed at {p:?}");
        }
    }

    /// A black key is drawn over its neighbours, so a press on the overlap is
    /// the black key's — the search runs in reverse paint order for exactly
    /// this. Getting it backwards lights the wrong key, which is how a missed
    /// click first shows itself.
    #[test]
    fn a_black_key_wins_the_overlap_it_is_drawn_over() {
        let (kb, keys, _) = laid_out(1200.0, 60.0);
        for i in (0..NUM_KEYS).filter(|&i| is_black_key(i)) {
            let r = key_rect(i, kb, 1200.0 / 52.0, 60.0);
            let p = pos2(r.center().x, kb.top() + 5.0);
            assert_eq!(key_at(p, &keys), Some(i), "black key {i} missed at {p:?}");
            // The same column, below where the black key ends, is white again.
            let below = key_at(pos2(r.center().x, kb.top() + 55.0), &keys);
            assert!(
                below.is_some_and(|k| !is_black_key(k)),
                "below black key {i} should be a white key, got {below:?}"
            );
        }
    }

    #[test]
    fn a_press_off_the_keys_hits_nothing() {
        let (kb, keys, _) = laid_out(1200.0, 60.0);
        assert_eq!(key_at(pos2(-5.0, kb.top() + 10.0), &keys), None);
        assert_eq!(key_at(pos2(600.0, kb.bottom() + 10.0), &keys), None);
        assert_eq!(key_at(pos2(1300.0, kb.top() + 10.0), &keys), None);
    }
}
