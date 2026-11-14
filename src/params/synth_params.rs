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

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use nih_plug::prelude::*;
use nih_plug_egui::EguiState;

use crate::constants::*;
use super::{CurveType, GranularityLevel, HarmonicParam, NestedFourierState};

#[derive(Params)]
pub struct LeSynthParams {
    #[persist = "editor-state"]
    pub editor_state: Arc<EguiState>,

    /// The keyboard's "Zero phases on keys" checkbox. Not an automatable
    /// parameter — it is a render setting, read by the engine through the same
    /// `Arc` (see `SharedParams::zero_key_phases`) — but it changes how every
    /// key sounds, so it is saved and restored with the rest of the state.
    #[persist = "zero_key_phases"]
    pub zero_key_phases: Arc<AtomicBool>,

    /// The keyboard's "Repeat" checkbox — whether a held note loops. Saved
    /// with the state for the same reason, and shared with the engine the same
    /// way (`SharedParams::repeat_playback`).
    #[persist = "repeat_playback"]
    pub repeat_playback: Arc<AtomicBool>,

    #[id = "points_per_period"]
    pub points_per_period: IntParam,

    #[id = "num_buckets"]
    pub num_buckets: IntParam,

    // ── Modulation: the amplitude envelope (see `crate::modulation`) ──
    #[id = "env_enabled"]
    pub env_enabled: BoolParam,
    #[id = "env_attack"]
    pub env_attack: FloatParam,
    #[id = "env_hold"]
    pub env_hold: FloatParam,
    #[id = "env_decay"]
    pub env_decay: FloatParam,
    #[id = "env_sustain"]
    pub env_sustain: FloatParam,
    #[id = "env_release"]
    pub env_release: FloatParam,
    #[id = "env_attack_curve"]
    pub env_attack_curve: FloatParam,
    #[id = "env_decay_curve"]
    pub env_decay_curve: FloatParam,
    #[id = "env_release_curve"]
    pub env_release_curve: FloatParam,

    // Heap-allocated (not an inline `[HarmonicParam; NUM_HARMONICS]`): each
    // `HarmonicParam` is ~3 KB, so at NUM_HARMONICS = 256 an inline array makes
    // `LeSynthParams` ~700 KB and constructing it by value (default → Arc::new)
    // overflows smaller (e.g. test) thread stacks. A `Vec` keeps the struct
    // pointer-sized; `#[nested(array)]` only needs `.iter()`, which `Vec` has.
    #[nested(array, group = "harmonics")]
    pub harmonics: Vec<HarmonicParam>,
}

impl Default for LeSynthParams {
    fn default() -> Self {
        // your old "make_param" defaults for amplitude:
        let default_amp = 0.0;
        let amp_range = FloatRange::Linear {
            min: MIN_OFFSET_AMP as f32,
            max: MAX_OFFSET_AMP as f32,
        };

        // new defaults for the other fields:
        let default_phase = 0.0;
        let phase_range = FloatRange::Linear {
            min: MIN_OFFSET_PHASE as f32,
            max: MAX_OFFSET_PHASE as f32,
        };

        let default_curve = CurveType::NestedFourier;

        let default_a = 0.0;
        let default_b = 0.1;
        let ab_range = FloatRange::Linear { min: 0.0, max: 1.0 };
        let b_range = FloatRange::Linear { min: 0.0, max: 0.35 };

        let default_wobble_amp = 0.0;
        let default_wobble_freq = 50.0;
        let wobble_amp_range = FloatRange::Linear { min: 0.0, max: 0.2 };
        let wobble_freq_range = FloatRange::Linear { min: 10.0, max: 200.0 };

        let harmonics: Vec<HarmonicParam> = (0..NUM_HARMONICS).map(|i| {
            let idx = i + 1;
            let amp = if idx > 10 { 0.05 } else { default_amp };
            // Default offset granularity (amplitude-slider cap) by harmonic index:
            // lower harmonics carry the most energy so they default coarser.
            // Harmonics 1-8 → 0.5, 9-28 → 0.1, 29+ → 0.05.
            let default_gran = if i < 8 {
                GranularityLevel::Medium
            } else if i < 28 {
                GranularityLevel::Low
            } else {
                GranularityLevel::VeryLow
            };
            HarmonicParam {
                curve_offset_amp: FloatParam::new(
                    &format!("Harmonic {} Curve Offset For Amplitude", idx),
                    amp,
                    amp_range,
                ),
                curve_offset_phase: FloatParam::new(
                    &format!("Harmonic {} Curve Offset For Phase", idx),
                    default_phase,
                    phase_range,
                ),
                curve_type_amp: EnumParam::new(
                    &format!("Harmonic {} Curve Type For Amplitude", idx),
                    default_curve,
                ),
                curve_type_phase: EnumParam::new(
                    &format!("Harmonic {} Curve Type Phase", idx),
                    CurveType::Constant,
                ),
                sine_curve_amp_amp: FloatParam::new(
                    &format!("Harmonic {} Amplitude Of Sine Curve For Amplitude", idx),
                    default_a,
                    ab_range,
                ),
                sine_curve_freq_amp: FloatParam::new(
                    &format!("Harmonic {} Frequency Of Sine Curve For Amplitude", idx),
                    default_b,
                    b_range,
                ),
                sine_curve_amp_phase: FloatParam::new(
                    &format!("Harmonic {} Amplitude Of Sine Curve For Phase", idx),
                    default_a,
                    ab_range,
                ),
                sine_curve_freq_phase: FloatParam::new(
                    &format!("Harmonic {} Frequency Of Sine Curve For Phase", idx),
                    default_b,
                    b_range,
                ),
                granularity_amp: EnumParam::new(
                    &format!("Harmonic {} Granularity For Amplitude", idx),
                    default_gran,
                ),
                granularity_phase: EnumParam::new(
                    &format!("Harmonic {} Granularity For Phase", idx),
                    default_gran,
                ),
                wobble_amp_amp: FloatParam::new(
                    &format!("Harmonic {} Wobble Amplitude For Amplitude", idx),
                    default_wobble_amp,
                    wobble_amp_range,
                ),
                wobble_freq_amp: FloatParam::new(
                    &format!("Harmonic {} Wobble Frequency For Amplitude", idx),
                    default_wobble_freq,
                    wobble_freq_range,
                ),
                wobble_amp_phase: FloatParam::new(
                    &format!("Harmonic {} Wobble Amplitude For Phase", idx),
                    default_wobble_amp,
                    wobble_amp_range,
                ),
                wobble_freq_phase: FloatParam::new(
                    &format!("Harmonic {} Wobble Frequency For Phase", idx),
                    default_wobble_freq,
                    wobble_freq_range,
                ),
                nested_fourier: Arc::new(RwLock::new(NestedFourierState::default())),
            }
        }).collect();

        // Envelope times in milliseconds, skewed so the short end — where an
        // attack is actually tuned — takes most of the slider's travel.
        let env_time = |name: &str, default: f32, min: f32, max: f32| {
            FloatParam::new(
                name,
                default,
                FloatRange::Skewed { min, max, factor: FloatRange::skew_factor(-2.0) },
            )
            .with_unit(" ms")
            .with_value_to_string(formatters::v2s_f32_rounded(1))
        };
        let env_curve = |name: &str| {
            FloatParam::new(name, 0.0, FloatRange::Linear { min: -1.0, max: 1.0 })
                .with_value_to_string(formatters::v2s_f32_rounded(2))
        };

        Self {
            // Off until the user asks for it; a state saved before this was
            // kept has no entry and leaves it off.
            zero_key_phases: Arc::new(AtomicBool::new(false)),
            // Looping a held note is the default, as it always was.
            repeat_playback: Arc::new(AtomicBool::new(true)),
            // Off by default: a voice then plays exactly as it did before the
            // envelope existed.
            env_enabled: BoolParam::new("Envelope Enabled", false),
            env_attack: env_time("Envelope Attack", 10.0, 0.0, 10_000.0),
            env_hold: env_time("Envelope Hold", 0.0, 0.0, 5_000.0),
            env_decay: env_time("Envelope Decay", 200.0, 0.0, 10_000.0),
            env_sustain: FloatParam::new(
                "Envelope Sustain",
                0.7,
                FloatRange::Linear { min: 0.0, max: 1.0 },
            )
            .with_value_to_string(formatters::v2s_f32_rounded(2)),
            env_release: env_time("Envelope Release", 300.0, 1.0, 10_000.0),
            env_attack_curve: env_curve("Envelope Attack Curve"),
            env_decay_curve: env_curve("Envelope Decay Curve"),
            env_release_curve: env_curve("Envelope Release Curve"),
            // These dimensions are overriden by actual window size
            editor_state: EguiState::from_size(1000, 1000),
            points_per_period: IntParam::new(
                "Points Per Period",
                64,
                IntRange::Linear { min: 16, max: 512 },
            ),
            num_buckets: IntParam::new(
                "Number of Buckets",
                NUM_OF_BUCKETS_DEFAULT as i32,
                IntRange::Linear {
                    min: NUM_OF_BUCKETS_MIN,
                    max: NUM_OF_BUCKETS_MAX,
                },
            ),
            harmonics,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::ChartType;

    /// The whole point of keeping the nested-Fourier sliders as `#[persist]`
    /// state: they have to come back through the plugin's own save/load, which
    /// is the only channel a host has for them. Without this a saved project
    /// reloads its grid but every slider under it reads zero.
    #[test]
    fn the_nested_fourier_sliders_survive_the_plugins_own_state() {
        let params = LeSynthParams::default();
        {
            let mut st = params.harmonics[3].nested_fourier.write().unwrap();
            let series = st.series_mut(ChartType::Amp);
            series.amps[5] = 0.42;
            series.phases[5] = -1.25;
            series.grans[5] = GranularityLevel::High.to_index();
            series.base_freq_hz = 12.0;
        }
        let saved = params.serialize_fields();

        let restored = LeSynthParams::default();
        restored.deserialize_fields(&saved);
        let st = restored.harmonics[3].nested_fourier.read().unwrap();
        let series = st.series(ChartType::Amp);
        assert_eq!(series.amps[5], 0.42);
        assert_eq!(series.phases[5], -1.25);
        assert_eq!(series.granularity(5), GranularityLevel::High);
        assert_eq!(series.base_freq_hz, 12.0);
        // And a harmonic nobody touched is still at its defaults.
        let other = restored.harmonics[4].nested_fourier.read().unwrap();
        assert_eq!(other.series(ChartType::Amp).base_freq_hz, 0.0);
    }
}
