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
};

use crate::constants::*;
use crate::engine::{ChartType, SynthComputeEngine};
use crate::gui::{draw_assembled_chart, draw_curve_controls, draw_harmonic_plot, draw_piano_keyboard, draw_metallic_background};
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

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let shared = &self.synth_compute_engine.shared_params;
        let fade_duration = shared.fade_duration;

        // --- Handle incoming MIDI events (build/stop voices) ---
        while let Some(event) = context.next_event() {
            match event {
                NoteEvent::NoteOn { note, .. } => {
                    let key_idx = note as usize;
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
                    let key_idx = note as usize;
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

                        let mut s = v.buffer[v.idx % len];

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
                let (window_width, window_height) = synth_params.editor_state.size();
                let window_width = window_width as f32;
                let window_height = window_height as f32;
                egui_ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::Vec2::new(window_width, window_height)));

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

                        // Keep original structure but make it responsive
                        egui::ScrollArea::vertical()
                            .auto_shrink([false; 2])
                            .max_height(window_height * 0.3)
                            .max_width(window_width)
                            .show(ui, |ui| {
                                for (idx, harmonic) in synth_params.harmonics.iter().enumerate() {
                                    ui.label(format!("Parameters for {}th harmonic:", idx + 1));
                                    ui.add_space(15.0);
                                    ui.label(format!("Amplitude Chart:"));
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
                                        MIN_AMP_SINE_AMP,
                                        MAX_AMP_SINE_AMP,
                                        window_width,
                                    );

                                    ui.label(format!("Phase Chart:"));
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
                                        MIN_PHASE_SINE_AMP,
                                        MAX_PHASE_SINE_AMP,
                                        window_width,
                                    );

                                    ui.separator();
                                }
                            });


                        ui.add_space(40.0);

                        let input = ui.input(|i| i.clone());
                        let gutter = 10.0;

                        draw_piano_keyboard(
                            egui_ctx,
                            ui,
                            &input,
                            last_key_id,
                            last_key_id_persist,
                            &synth_compute_engine,
                            window_width - 1.5*gutter,
                            window_height
                        );


                        let chart_w = (window_width - gutter) * 0.5;
                        let chart_h = (window_height * 0.3).max(200.0);
                        let plot_start_point = egui::pos2(0.0, 0.43 * window_height);
                        let right_w = chart_w - gutter;

                        let left_rect = egui::Rect::from_min_size(
                            plot_start_point,
                            egui::vec2(chart_w, chart_h),
                        );

                        let right_rect = egui::Rect::from_min_size(
                            plot_start_point + egui::vec2(chart_w + gutter, 0.0),
                            egui::vec2(right_w, chart_h),
                        );

                        ui.allocate_space(egui::vec2(window_width, chart_h));


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

                        ui.add_space(10.0);

                        draw_assembled_chart(ui, &synth_compute_engine, window_width, window_height);
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
