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

//! Nested-Fourier sub-harmonic data for the `NestedFourier` curve type.
//!
//! Historically each sub-harmonic amplitude and phase was a separate VST
//! `FloatParam`. With 32 sub-harmonics in four independent sets per harmonic
//! and 32 harmonics, that exposed ~4096 automatable parameters to the host,
//! which some hosts handle poorly. This data is now plain serde state stored
//! via `#[persist]` instead: it is saved and restored with the plugin/project
//! state but is no longer host-automatable.

use serde::{Deserialize, Serialize};

pub const NUM_NESTED_FOURIER_HARMONICS: usize = 32;

/// One Fourier sub-harmonic series for a single chart.
/// The envelope across buckets is computed as:
///   V(t) = offset + Sum_{k=1}^{N} amps[k] * sin(2*pi * k * t + phases[k])
/// where t = bucket / num_buckets.
///
/// Amplitudes are in [0, 1]; phases are in radians [-pi, pi]. The offset lives
/// on the harmonic's `curve_offset_*` parameter, not here.
#[derive(Clone, Serialize, Deserialize)]
pub struct NestedFourierSeries {
    pub amps: [f32; NUM_NESTED_FOURIER_HARMONICS],
    pub phases: [f32; NUM_NESTED_FOURIER_HARMONICS],
}

impl Default for NestedFourierSeries {
    fn default() -> Self {
        NestedFourierSeries {
            amps: [0.0; NUM_NESTED_FOURIER_HARMONICS],
            phases: [0.0; NUM_NESTED_FOURIER_HARMONICS],
        }
    }
}

/// A harmonic's complete nested-Fourier state: one independent series for the
/// amplitude chart and one for the phase chart.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct NestedFourierState {
    pub amp_chart: NestedFourierSeries,
    pub phase_chart: NestedFourierSeries,
}

use crate::engine::ChartType;

impl NestedFourierState {
    /// The sub-harmonic series driving the given chart.
    pub fn series(&self, chart_type: ChartType) -> &NestedFourierSeries {
        match chart_type {
            ChartType::Amp => &self.amp_chart,
            ChartType::Phase => &self.phase_chart,
        }
    }

    /// Mutable access to the sub-harmonic series driving the given chart.
    pub fn series_mut(&mut self, chart_type: ChartType) -> &mut NestedFourierSeries {
        match chart_type {
            ChartType::Amp => &mut self.amp_chart,
            ChartType::Phase => &mut self.phase_chart,
        }
    }
}
