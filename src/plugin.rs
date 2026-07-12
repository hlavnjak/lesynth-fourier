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

use std::num::NonZeroU32;
use std::sync::Arc;
use nih_plug::prelude::*;
use nih_plug_egui::{
    create_egui_editor,
    egui::{self},
    widgets::ParamSlider,
};

use crate::constants::*;
use crate::engine::{ChartType, ExecutionMode, SynthComputeEngine};
use crate::gui::{draw_analysis_controls, draw_assembled_chart, draw_curve_controls, draw_harmonic_plot, draw_nested_fourier_controls, draw_piano_keyboard, draw_metallic_background, section, section_with_header};
use crate::params::LeSynthParams;
use crate::voice::Voice;

pub struct LeSynth {
    synth_params: Arc<LeSynthParams>,
    pub synth_compute_engine: Arc<SynthComputeEngine>,
}

impl Default for LeSynth {
    fn default() -> Self {
        crate::init_logging();
        
        let synth_params = Arc::new(LeSynthParams::default());
        Self {
            synth_params: synth_params.clone(),
            synth_compute_engine: Arc::new(SynthComputeEngine::new(synth_params)),
        }
    }
}

impl Plugin for LeSynth {
    const NAME: &'static str = "LeSynth";
    const VENDOR: &'static str = "Jakub Hlavnicka";
    const URL: &'static str = "https://donothaveany.com";
    const EMAIL: &'static str = "hlavnickajakub@gmail.com";
    const VERSION: &'static str = "1.2.0";
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: Some(NonZeroU32::new(2).unwrap()),
        aux_input_ports: &[],
        aux_output_ports: &[],
        ..AudioIOLayout::const_default()
    }];

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.synth_params.clone()
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.synth_compute_engine
            .shared_params
            .update_sample_rate(buffer_config.sample_rate);
        self.synth_compute_engine
            .shared_params
            .mark_all_buffers_dirty();
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let shared = &self.synth_compute_engine.shared_params;
        let fade_duration = shared.fade_duration;
        let repeat_playback = shared.repeat_playback();

        // --- Handle incoming MIDI events (build/stop voices) ---
        while let Some(event) = context.next_event() {
            match event {
                NoteEvent::NoteOn { note, .. } => {
                    // MIDI A0 = note 21, our key 0 = A0, so subtract 21
                    let key_idx = (note as usize).saturating_sub(21);
                    if key_idx < NUM_KEYS {
                        // Get pre-computed buffer or compute synchronously as fallback
                        let buf = self.synth_compute_engine.get_buffer_for_key(key_idx);
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
                }
                NoteEvent::NoteOff { note, .. } => {
                    let key_idx = (note as usize).saturating_sub(21);
                    if key_idx < NUM_KEYS {
                        let mut voices = shared.voices.lock().unwrap();
                        if let Some(v) = voices[key_idx].as_mut() {
                            v.fade_out_active = true;
                            v.fade_out_pos = 0;
                        }
                    }
                }
                _ => {}
            }
        }

        // --- Mixdown all active voices into the output buffer with headroom ---
        {
            let mut voices = shared.voices.lock().unwrap();

            for mut frame in buffer.iter_samples() {
                // Count active voices this frame (cheap; keeps headroom stable)
                let active_count = voices.iter().filter(|o| o.is_some()).count();
                
                // Per-voice scaling with safe loudness compensation
                // Scale each voice down, then boost final mix carefully to avoid clipping
                let (voice_gain, master_gain) = if active_count > 0 {
                    let n = active_count as f32;
                    // Each voice gets 1/N scaling to prevent clipping
                    let voice_scaling = 0.8 / n;  // More conservative base scaling
                    // Safer loudness compensation that won't exceed ±1.0
                    let loudness_compensation = match active_count {
                        1 => 1.0,   // Single voice: 0.8 * 1.0 = 0.8
                        2 => 1.5,   // 2 voices: 0.4 * 1.5 = 0.6  
                        3 => 2.0,   // 3 voices: 0.267 * 2.0 = 0.53
                        4 => 2.4,   // 4 voices: 0.2 * 2.4 = 0.48
                        5 => 2.8,   // 5 voices: 0.16 * 2.8 = 0.45
                        _ => 3.0,   // 6+ voices: 0.133 * 3.0 = 0.4 max
                    };
                    (voice_scaling, loudness_compensation)
                } else {
                    (1.0, 1.0)
                };

                let mut mixed = 0.0f32;

                for opt in voices.iter_mut() {
                    if let Some(v) = opt.as_mut() {
                        let len = v.buffer.len();
                        if len == 0 {
                            continue;
                        }

                        // One-shot playback: once the whole buffer has played,
                        // begin a clean fade-out (holding the last sample) rather
                        // than looping. Repeat mode keeps wrapping as before.
                        if !repeat_playback && v.idx >= len && !v.fade_out_active {
                            v.fade_out_active = true;
                            v.fade_out_pos = 0;
                        }

                        let sample_idx = if repeat_playback {
                            v.idx % len
                        } else {
                            v.idx.min(len - 1)
                        };
                        let mut s = v.buffer[sample_idx];

                        // Apply per-voice scaling FIRST to prevent intermediate clipping
                        s *= voice_gain;

                        // Fade in
                        if v.fade_in_active && v.fade_in_pos < fade_duration {
                            let g = v.fade_in_pos as f32 / fade_duration as f32;
                            s *= g;
                            v.fade_in_pos += 1;
                        } else {
                            v.fade_in_active = false;
                        }

                        // Fade out
                        if v.fade_out_active {
                            if v.fade_out_pos < fade_duration {
                                let g = 1.0 - (v.fade_out_pos as f32 / fade_duration as f32);
                                s *= g;
                                v.fade_out_pos += 1;
                            } else {
                                // Voice finished after fade; remove it
                                *opt = None;
                                continue;
                            }
                        }

                        mixed += s;
                        v.idx = v.idx.wrapping_add(1);
                    }
                }

                // Apply loudness compensation
                mixed *= master_gain;

                // Final clamp - should rarely trigger now
                mixed = mixed.clamp(-1.0, 1.0);

                for (_, sample) in frame.iter_mut().enumerate() {
                    *sample = mixed;
                }
            }
        }

        ProcessStatus::Normal
    }

    // Create an editor using the provided helper.
    fn editor(&mut self, _executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        let synth_params = self.synth_params.clone();
        let synth_compute_engine = self.synth_compute_engine.clone();
        create_egui_editor(
            synth_params.editor_state.clone(),
            (),
            |_, _| {},
            move |egui_ctx, setter, _state| {
                let last_key_id = egui::Id::new("last_pressed_key");
                let last_key_id_persist = egui::Id::new("last_pressed_key_persist");

                let mut last_pressed_key: Option<usize> = None;
                let mut last_pressed_key_persist: Option<usize> = Some(15);

                // The following params are changed when the window is resized
                let (win_w, win_h) = synth_params.editor_state.size();
                let window_width = win_w as f32;
                let window_height = win_h as f32;
                // Only ask baseview to resize when the size actually changed.
                // Sending InnerSize every frame forces a window.resize each
                // frame, which bounces back as a repaint and stops the editor
                // from ever idling.
                let last_size_id = egui::Id::new("last_sent_inner_size");
                let last_sent = egui_ctx.memory(|m| m.data.get_temp::<(u32, u32)>(last_size_id));
                if last_sent != Some((win_w, win_h)) {
                    egui_ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                        egui::Vec2::new(window_width, window_height),
                    ));
                    egui_ctx.memory_mut(|m| m.data.insert_temp(last_size_id, (win_w, win_h)));
                }

                egui_ctx.memory_mut(|mem| {
                    last_pressed_key =
                        *mem.data.get_temp_mut_or_insert_with(last_key_id, || None);
                    last_pressed_key_persist = *mem
                        .data
                        .get_temp_mut_or_insert_with(last_key_id_persist, || Some(15));
                });
                egui::CentralPanel::default().show(egui_ctx, |ui| {
                        // Draw metallic background
                        draw_metallic_background(ui, window_width, window_height);

                        // ── Host-pushed analysis jobs ─────────────────────────────
                        // If the host DAW pushed a subtrack to analyse, claim it,
                        // run the analysis and flip into Analysis mode.
                        if let Some(job) = crate::claim_analysis_job() {
                            synth_compute_engine.analyze_and_load(
                                &job.samples,
                                job.sample_rate,
                                job.base_freq,
                                &job.contour,
                                0,
                            );
                        }

                        // Width available to section content once the card's
                        // horizontal inner margin is subtracted, so nothing
                        // overflows the consistent section borders.
                        let pad = 10.0;
                        let content_w = window_width - 2.0 * pad;

                        // ── Execution-mode switch ─────────────────────────────────
                        let mut mode = synth_compute_engine.shared_params.execution_mode();
                        section(ui, "Mode", |ui| {
                            ui.horizontal(|ui| {
                                if ui
                                    .selectable_label(mode == ExecutionMode::Synth, "Synth")
                                    .clicked()
                                {
                                    mode = ExecutionMode::Synth;
                                }
                                if ui
                                    .selectable_label(mode == ExecutionMode::Analysis, "Analysis")
                                    .clicked()
                                {
                                    mode = ExecutionMode::Analysis;
                                }
                            });
                        });
                        synth_compute_engine.shared_params.set_execution_mode(mode);
                        ui.add_space(10.0);

                        // Whether analysed input audio is loaded. When it is, the grid
                        // resolution comes from the source and the bucket count must not
                        // be overridden, so the Buckets slider (drawn inside the Harmonic
                        // Editor below) is disabled. `analysis_duration_secs > 0` is the
                        // "analysis data present" flag.
                        let has_analysis = *synth_compute_engine
                            .shared_params
                            .analysis_duration_secs
                            .lock()
                            .unwrap()
                            > 0.0;

                        let params_changed_action = || {
                            synth_compute_engine.set_normalization_needed(true);

                            // Rebuild buffers for currently active voices so changes are audible immediately
                            {
                                let shared = &synth_compute_engine.shared_params;
                                let mut voices = shared.voices.lock().unwrap();
                                for (key_idx, slot) in voices.iter_mut().enumerate() {
                                    if let Some(v) = slot.as_mut() {
                                        let buf = synth_compute_engine
                                            .get_buffer_for_key(key_idx);
                                        v.buffer = buf;
                                        // keep current idx and fade states
                                    }
                                }
                            }

                            // Update assembled chart with key 24 for immediate preview
                            synth_compute_engine.update_assembled_chart_with_key24();
                        };

                        // Keep original structure but make it responsive, wrapped
                        // in a consistent bordered section card.
                        let editor_title = if mode == ExecutionMode::Synth {
                            "Harmonic Editor"
                        } else {
                            "Analysis"
                        };
                        section(ui, editor_title, |ui| {
                        if mode == ExecutionMode::Synth {
                        // The harmonic list is NUM_HARMONICS (256) rows, each a heavy
                        // block of sliders/combos/nested-Fourier controls. egui is
                        // immediate-mode and baseview re-runs this whole closure ~66x/sec,
                        // so building all 256 rows every frame pegs a CPU core even when
                        // the editor is idle. Virtualize with `show_rows` so only the rows
                        // scrolled into view are built. The rows are uniform height: learn
                        // it at runtime from the per-row stride and cache it in egui memory
                        // (converges after one frame; never changes afterwards).
                        let row_h_id = egui::Id::new("harmonic_row_height");
                        let cached_row_h: f32 = egui_ctx
                            .memory(|m| m.data.get_temp(row_h_id))
                            .unwrap_or(500.0);
                        let mut measured_row_h: Option<f32> = None;
                        let mut prev_row_y: Option<f32> = None;

                        egui::ScrollArea::vertical()
                            .auto_shrink([false; 2])
                            .max_height(window_height * 0.24)
                            .max_width(content_w)
                            .show_rows(
                                ui,
                                cached_row_h,
                                synth_params.harmonics.len(),
                                |ui, row_range| {
                                let spacing_y = ui.spacing().item_spacing.y;
                                for idx in row_range {
                                    let harmonic = &synth_params.harmonics[idx];

                                    // Learn the true (uniform) row height from the vertical
                                    // stride between consecutive rows, minus the inter-row
                                    // spacing egui inserts.
                                    let row_y = ui.cursor().min.y;
                                    if let (Some(p), None) = (prev_row_y, measured_row_h) {
                                        measured_row_h = Some((row_y - p - spacing_y).max(1.0));
                                    }
                                    prev_row_y = Some(row_y);

                                    // ── Harmonic header ───────────────────────────────────────
                                    egui::Frame::new()
                                        .fill(egui::Color32::from_gray(58))
                                        .inner_margin(egui::Margin::same(6i8))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new(format!("Parameters for {}th harmonic:", idx + 1))
                                                    .strong()
                                                    .size(16.0)
                                                    .color(egui::Color32::WHITE),
                                            );
                                        });

                                    // ── Amplitude Chart ───────────────────────────────────────
                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(35, 52, 46))
                                        .inner_margin(egui::Margin::same(4i8))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new("Amplitude Chart:")
                                                    .strong()
                                                    .size(13.0)
                                                    .color(egui::Color32::WHITE),
                                            );
                                        });

                                    egui::Frame::new()
                                        .fill(egui::Color32::from_gray(30))
                                        .inner_margin(egui::Margin::same(4i8))
                                        .show(ui, |ui| {
                                            draw_curve_controls(
                                                ui,
                                                idx,
                                                ChartType::Amp,
                                                harmonic,
                                                synth_compute_engine.clone(),
                                                setter,
                                                &params_changed_action,
                                                MIN_OFFSET_AMP,
                                                MAX_OFFSET_AMP,
                                                content_w - 8.0,
                                            );
                                        });

                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(18, 25, 45))
                                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 80, 140)))
                                        .inner_margin(egui::Margin::same(6i8))
                                        .show(ui, |ui| {
                                            draw_nested_fourier_controls(
                                                ui,
                                                idx,
                                                ChartType::Amp,
                                                harmonic,
                                                synth_compute_engine.clone(),
                                                &params_changed_action,
                                                content_w - 12.0,
                                            );
                                        });

                                    // ── Phase Chart ───────────────────────────────────────────
                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(48, 35, 55))
                                        .inner_margin(egui::Margin::same(4i8))
                                        .show(ui, |ui| {
                                            ui.label(
                                                egui::RichText::new("Phase Chart:")
                                                    .strong()
                                                    .size(13.0)
                                                    .color(egui::Color32::WHITE),
                                            );
                                        });

                                    egui::Frame::new()
                                        .fill(egui::Color32::from_gray(30))
                                        .inner_margin(egui::Margin::same(4i8))
                                        .show(ui, |ui| {
                                            draw_curve_controls(
                                                ui,
                                                idx,
                                                ChartType::Phase,
                                                harmonic,
                                                synth_compute_engine.clone(),
                                                setter,
                                                &params_changed_action,
                                                MIN_OFFSET_PHASE,
                                                MAX_OFFSET_PHASE,
                                                content_w - 8.0,
                                            );
                                        });

                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(18, 25, 45))
                                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(50, 80, 140)))
                                        .inner_margin(egui::Margin::same(6i8))
                                        .show(ui, |ui| {
                                            draw_nested_fourier_controls(
                                                ui,
                                                idx,
                                                ChartType::Phase,
                                                harmonic,
                                                synth_compute_engine.clone(),
                                                &params_changed_action,
                                                content_w - 12.0,
                                            );
                                        });

                                    ui.add_space(4.0);
                                }
                                },
                            );

                        // Persist the measured row height so the next frame's
                        // virtualization math is exact.
                        if let Some(h) = measured_row_h {
                            if (h - cached_row_h).abs() > 0.5 {
                                egui_ctx.memory_mut(|m| m.data.insert_temp(row_h_id, h));
                                egui_ctx.request_repaint();
                            }
                        }
                        } else {
                            // Analysis mode: per-harmonic enable/disable grid.
                            // Reuse the exact same fixed-height scroll area as Synth
                            // mode so the control box occupies identical space and the
                            // keyboard + charts below line up in both modes.
                            egui::ScrollArea::vertical()
                                .auto_shrink([false; 2])
                                .max_height(window_height * 0.24)
                                .max_width(content_w)
                                .show(ui, |ui| {
                                    egui::Frame::new()
                                        .fill(egui::Color32::from_rgb(18, 25, 45))
                                        .inner_margin(egui::Margin::same(6i8))
                                        .show(ui, |ui| {
                                            draw_analysis_controls(
                                                ui,
                                                &synth_compute_engine,
                                                content_w - 12.0,
                                                window_height,
                                            );
                                        });
                                });
                        }
                        });
                        ui.add_space(10.0);

                        // ── Keyboard ──────────────────────────────────────────────
                        section(ui, "Keyboard", |ui| {
                            let input = ui.input(|i| i.clone());
                            let gutter = 10.0;
                            draw_piano_keyboard(
                                egui_ctx,
                                ui,
                                &input,
                                last_key_id,
                                last_key_id_persist,
                                &synth_compute_engine,
                                content_w - 1.5 * gutter,
                                window_height,
                                1.0,
                            );
                        });
                        ui.add_space(10.0);

                        // ── Live harmonics ────────────────────────────────────────
                        // The Buckets control (envelope time-resolution) lives in this
                        // section's caption row, roughly centred, so it shares the
                        // caption's height and never grows the section. It is disabled
                        // when input sound is loaded (the grid resolution then follows
                        // the source and must not be overridden).
                        let buckets_header = |ui: &mut egui::Ui| {
                            let applied_id = egui::Id::new("applied_num_buckets");
                            // Apply a restored param value to the grid once on open so
                            // the grid matches the param. Never while input sound is
                            // loaded (it would clobber the analysed grid).
                            if !has_analysis
                                && ui.data(|d| d.get_temp::<i32>(applied_id)).is_none()
                            {
                                let v = synth_params.num_buckets.value();
                                if synth_compute_engine.num_buckets() != v as usize {
                                    synth_compute_engine.set_num_buckets(v as usize);
                                }
                                ui.data_mut(|d| d.insert_temp(applied_id, v));
                            }

                            // Push the control group toward the window's horizontal
                            // centre (approx: the group is ~230 px wide).
                            let middle = ui.min_rect().left() + content_w * 0.5;
                            let space = (middle - 115.0 - ui.cursor().min.x).max(8.0);
                            ui.add_space(space);

                            ui.label(
                                egui::RichText::new("Buckets:")
                                    .strong()
                                    .color(egui::Color32::WHITE),
                            );
                            let resp = ui.add_enabled(
                                !has_analysis,
                                ParamSlider::for_param(&synth_params.num_buckets, setter),
                            );
                            if has_analysis {
                                resp.on_hover_text(
                                    "Locked: bucket count follows the loaded input sound",
                                );
                            } else {
                                // Applying a new bucket count resizes the grid and
                                // invalidates all key buffers — too heavy to run per
                                // frame. Commit only on drag release or a typed value,
                                // never mid-drag.
                                let committed =
                                    resp.drag_stopped() || (resp.changed() && !resp.dragged());
                                if committed {
                                    let v = synth_params.num_buckets.value();
                                    if ui.data(|d| d.get_temp::<i32>(applied_id)) != Some(v) {
                                        synth_compute_engine.set_num_buckets(v as usize);
                                        ui.data_mut(|d| d.insert_temp(applied_id, v));
                                    }
                                }
                            }
                        };
                        section_with_header(ui, "Live Harmonics", buckets_header, |ui| {
                            let gutter = 10.0;
                            let chart_w = (content_w - gutter) * 0.5;
                            let chart_h = (window_height * 0.23).max(160.0);
                            // Anchor the harmonic plots to the live flow cursor (inside
                            // this card) so the absolute rects below line up with the
                            // space reserved by allocate_space() and stay within the
                            // section border.
                            let plot_start_point =
                                egui::pos2(ui.cursor().min.x, ui.cursor().min.y);
                            let right_w = chart_w - gutter;

                            let left_rect = egui::Rect::from_min_size(
                                plot_start_point,
                                egui::vec2(chart_w, chart_h),
                            );

                            let right_rect = egui::Rect::from_min_size(
                                plot_start_point + egui::vec2(chart_w + gutter, 0.0),
                                egui::vec2(right_w, chart_h),
                            );

                            ui.allocate_space(egui::vec2(content_w, chart_h));

                            ui.allocate_new_ui(
                                egui::UiBuilder::new()
                                    .max_rect(left_rect)
                                    .layout(*ui.layout()),
                                |ui| {
                                    draw_harmonic_plot(
                                        ui,
                                        "Amplitude",
                                        ChartType::Amp,
                                        chart_w,
                                        chart_h,
                                        &synth_compute_engine,
                                    );
                                },
                            );

                            ui.allocate_new_ui(
                                egui::UiBuilder::new()
                                    .max_rect(right_rect)
                                    .layout(*ui.layout()),
                                |ui| {
                                    draw_harmonic_plot(
                                        ui,
                                        "Phase",
                                        ChartType::Phase,
                                        right_w,
                                        chart_h,
                                        &synth_compute_engine,
                                    );
                                },
                            );
                        });
                        ui.add_space(10.0);

                        // ── Assembled sound ───────────────────────────────────────
                        section(ui, "Assembled Sound", |ui| {
                            draw_assembled_chart(
                                ui,
                                &synth_compute_engine,
                                content_w,
                                window_height,
                            );
                        });
                });
            },
        )
    }
}

impl ClapPlugin for LeSynth {
    const CLAP_ID: &'static str = "com.hlavnicka.lesynth";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("A LeSynth plugin");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[];
}

impl Vst3Plugin for LeSynth {
    const VST3_CLASS_ID: [u8; 16] = *b"LeSynthFourier01";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Synth,
        Vst3SubCategory::Instrument,
        Vst3SubCategory::Tools,
    ];
}
