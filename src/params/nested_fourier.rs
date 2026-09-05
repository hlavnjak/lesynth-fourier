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

//! Nested-Fourier sub-harmonic data for the `NestedFourier` curve type.
//!
//! Historically each sub-harmonic amplitude and phase was a separate VST
//! `FloatParam`. With 32 sub-harmonics in four independent sets per harmonic
//! and 32 harmonics, that exposed ~4096 automatable parameters to the host,
//! which some hosts handle poorly. This data is now plain serde state stored
//! via `#[persist]` instead: it is saved and restored with the plugin/project
//! state but is no longer host-automatable.

use serde::{Deserialize, Serialize};

use super::GranularityLevel;

pub const NUM_NESTED_FOURIER_HARMONICS: usize = 32;

/// Default granularity (amplitude-slider max) for a sub-harmonic by index:
/// lower harmonics carry the most energy, so they get a coarser cap while the
/// higher ones default progressively finer. First 8 → 0.5, next 20 → 0.1,
/// the rest → 0.05.
pub fn default_sub_granularity(sub_idx: usize) -> GranularityLevel {
    if sub_idx < 8 {
        GranularityLevel::Medium
    } else if sub_idx < 28 {
        GranularityLevel::Low
    } else {
        GranularityLevel::VeryLow
    }
}

/// The per-slider granularity a series starts on. A separate function because
/// serde needs one to fill the field in from state written before it existed.
fn default_grans() -> [u8; NUM_NESTED_FOURIER_HARMONICS] {
    let mut out = [0u8; NUM_NESTED_FOURIER_HARMONICS];
    for (i, cell) in out.iter_mut().enumerate() {
        *cell = default_sub_granularity(i).to_index();
    }
    out
}

/// One Fourier sub-harmonic series for a single chart.
/// The envelope across buckets is computed as:
///   V(t) = offset + Sum_{k=1}^{N} amps[k] * sin(2*pi * k * cycles * t + phases[k])
/// where t = bucket / num_buckets and `cycles` is how many turns the
/// fundamental makes across the whole grid — `base_freq_hz` times the grid's
/// duration in seconds (see `SynthComputeEngine::grid_duration_secs`).
///
/// Amplitudes are in [0, 1]; phases are in radians [-pi, pi]. The offset lives
/// on the harmonic's `curve_offset_*` parameter, not here.
#[derive(Clone, Serialize, Deserialize)]
pub struct NestedFourierSeries {
    pub amps: [f32; NUM_NESTED_FOURIER_HARMONICS],
    pub phases: [f32; NUM_NESTED_FOURIER_HARMONICS],
    /// Per-slider amplitude cap, as a [`GranularityLevel`] index. It is what the
    /// amp slider's range is, so a series restored without it shows values it
    /// cannot reach — which is why it lives here rather than in egui's
    /// frame-local memory, where it used to.
    #[serde(default = "default_grans")]
    pub grans: [u8; NUM_NESTED_FOURIER_HARMONICS],
    /// Fundamental of this series, in Hz against the grid's own duration.
    /// `0.0` means **auto**: exactly one cycle of the fundamental across the
    /// whole grid, which is what the series did before this was selectable, and
    /// what every file written before it holds.
    #[serde(default)]
    pub base_freq_hz: f32,
}

impl Default for NestedFourierSeries {
    fn default() -> Self {
        NestedFourierSeries {
            amps: [0.0; NUM_NESTED_FOURIER_HARMONICS],
            phases: [0.0; NUM_NESTED_FOURIER_HARMONICS],
            grans: default_grans(),
            base_freq_hz: 0.0,
        }
    }
}

impl NestedFourierSeries {
    /// This slider's amplitude cap, falling back to the index default for a
    /// byte outside the known levels (a file from a build with more of them).
    pub fn granularity(&self, sub_idx: usize) -> GranularityLevel {
        self.grans
            .get(sub_idx)
            .and_then(|&i| GranularityLevel::from_index(i))
            .unwrap_or_else(|| default_sub_granularity(sub_idx))
    }

    /// How many cycles this series' fundamental makes across a grid spanning
    /// `grid_secs` seconds. Auto (`base_freq_hz == 0`) is one.
    pub fn cycles_across_grid(&self, grid_secs: f64) -> f64 {
        if self.base_freq_hz > 0.0 && grid_secs > 0.0 {
            self.base_freq_hz as f64 * grid_secs
        } else {
            1.0
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_written_before_the_new_fields_loads_with_their_defaults() {
        // Exactly what an older build wrote: amps and phases, nothing else.
        let json = format!(
            r#"{{"amps":[{z}],"phases":[{z}]}}"#,
            z = ["0.0"; NUM_NESTED_FOURIER_HARMONICS].join(",")
        );
        let series: NestedFourierSeries = serde_json::from_str(&json).unwrap();
        // Auto — one cycle across the grid, which is what that build drew.
        assert_eq!(series.base_freq_hz, 0.0);
        assert_eq!(series.cycles_across_grid(0.75), 1.0);
        assert_eq!(series.granularity(0), GranularityLevel::Medium);
        assert_eq!(series.granularity(31), GranularityLevel::VeryLow);
    }

    #[test]
    fn base_freq_counts_cycles_against_the_grid_duration() {
        let mut series = NestedFourierSeries::default();
        series.base_freq_hz = 4.0;
        assert!((series.cycles_across_grid(0.75) - 3.0).abs() < 1e-9);
        // A grid with no duration cannot measure Hz; fall back to one cycle.
        assert_eq!(series.cycles_across_grid(0.0), 1.0);
    }

    #[test]
    fn a_round_trip_keeps_every_field() {
        let mut state = NestedFourierState::default();
        {
            let s = state.series_mut(ChartType::Phase);
            s.amps[3] = 0.25;
            s.phases[3] = 1.5;
            s.grans[3] = GranularityLevel::High.to_index();
            s.base_freq_hz = 12.5;
        }
        let json = serde_json::to_string(&state).unwrap();
        let back: NestedFourierState = serde_json::from_str(&json).unwrap();
        let s = back.series(ChartType::Phase);
        assert_eq!(s.amps[3], 0.25);
        assert_eq!(s.phases[3], 1.5);
        assert_eq!(s.granularity(3), GranularityLevel::High);
        assert_eq!(s.base_freq_hz, 12.5);
    }
}
