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
use nih_plug::prelude::*;
use nih_plug_egui::EguiState;

use crate::constants::*;
use super::{CurveType, GranularityLevel, HarmonicParam};

#[derive(Params)]
pub struct LeSynthParams {
    #[persist = "editor-state"]
    pub editor_state: Arc<EguiState>,

    #[id = "points_per_period"]
    pub points_per_period: IntParam,

    #[id = "num_buckets"]
    pub num_buckets: IntParam,

    #[nested(array, group = "harmonics")]
    pub harmonics: [HarmonicParam; NUM_HARMONICS],
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

        let default_curve = CurveType::Constant;

        let default_a = 0.0;
        let default_b = 0.1;
        let ab_range = FloatRange::Linear { min: 0.0, max: 1.0 };
        let b_range = FloatRange::Linear { min: 0.0, max: 0.35 };

        let default_wobble_amp = 0.0;
        let default_wobble_freq = 50.0;
        let wobble_amp_range = FloatRange::Linear { min: 0.0, max: 0.2 };
        let wobble_freq_range = FloatRange::Linear { min: 10.0, max: 200.0 };

        let harmonics = std::array::from_fn(|i| {
            let idx = i + 1;
            HarmonicParam {
                curve_offset_amp: FloatParam::new(
                    &format!("Harmonic {} Curve Offset For Amplitude", idx),
                    default_amp,
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
                    default_curve,
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
                    GranularityLevel::default(),
                ),
                granularity_phase: EnumParam::new(
                    &format!("Harmonic {} Granularity For Phase", idx),
                    GranularityLevel::default(),
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
            }
        });

        Self {
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
