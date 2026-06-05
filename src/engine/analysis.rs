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

//! Audio-analysis ("resynthesis") DSP for the second execution mode.
//!
//! The normal (synth) mode builds the amplitude/phase grid from the
//! user-drawn curves. In analysis mode we go the other way: given a chunk of
//! recorded audio (a "subtrack" of roughly constant pitch, segmented by the
//! host DAW), we divide it into *buckets* that are each as close as possible
//! to one local period, then run a harmonic DFT inside every bucket. The
//! resulting `amplitude[harmonic][bucket]` / `phase[harmonic][bucket]` grids
//! plug straight into the existing charts and the existing resynthesis path.
//!
//! Picking the bucket length per-bucket from the *local* refined period (not a
//! single global period) is what keeps the per-harmonic amp/phase curves
//! continuous from bucket to bucket — exactly the "most continuous functions"
//! the feature asks for.

use std::f32::consts::PI;

/// Which way the compute engine is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Original behaviour: the grid comes from the drawn curves.
    Synth,
    /// New behaviour: the grid is produced by analysing an input subtrack.
    Analysis,
}

impl Default for ExecutionMode {
    fn default() -> Self {
        ExecutionMode::Synth
    }
}

impl ExecutionMode {
    pub fn as_u8(self) -> u8 {
        match self {
            ExecutionMode::Synth => 0,
            ExecutionMode::Analysis => 1,
        }
    }

    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => ExecutionMode::Analysis,
            _ => ExecutionMode::Synth,
        }
    }
}

/// Result of analysing one subtrack: amp/phase per (harmonic, bucket) plus the
/// per-bucket period that was used (in samples), for inspection/plotting.
#[derive(Debug, Clone)]
pub struct AnalysisResult {
    /// `amplitude[harmonic][bucket]`, clamped to [0, 1].
    pub amplitude: Vec<Vec<f32>>,
    /// `phase[harmonic][bucket]`, in radians in [0, 2π).
    pub phase: Vec<Vec<f32>>,
    /// The local period (in samples) used for each bucket.
    pub bucket_periods: Vec<f32>,
}

impl AnalysisResult {
    pub fn num_harmonics(&self) -> usize {
        self.amplitude.len()
    }

    pub fn num_buckets(&self) -> usize {
        self.amplitude.first().map(|r| r.len()).unwrap_or(0)
    }
}

/// Refine an approximate period around `center` using a short normalised
/// autocorrelation search. This lets each bucket track small pitch drift so
/// adjacent buckets line up phase-wise and the charts stay smooth.
fn refine_period(samples: &[f32], start: usize, win: usize, approx: f32) -> f32 {
    if approx < 2.0 {
        return approx.max(2.0);
    }
    let lo = (approx * 0.85).floor().max(2.0) as usize;
    let hi = (approx * 1.15).ceil() as usize;
    let end = (start + win).min(samples.len());
    if end <= start + hi + 2 {
        return approx;
    }

    let mut best_lag = approx.round() as usize;
    let mut best_score = f32::NEG_INFINITY;
    for lag in lo..=hi {
        let mut num = 0.0f32;
        let mut den = 0.0f32;
        let mut n = 0usize;
        let mut i = start;
        while i + lag < end {
            num += samples[i] * samples[i + lag];
            den += samples[i + lag] * samples[i + lag];
            n += 1;
            i += 1;
        }
        if n == 0 || den <= 1e-9 {
            continue;
        }
        let score = num / den.sqrt();
        if score > best_score {
            best_score = score;
            best_lag = lag;
        }
    }
    best_lag as f32
}

/// Analyse one subtrack into an amplitude/phase grid.
///
/// * `samples`      – mono PCM of the subtrack.
/// * `sample_rate`  – Hz.
/// * `base_freq`    – estimated fundamental of the subtrack (Hz).
/// * `num_buckets`  – requested bucket count; `0` means "auto" (one bucket per
///                    fundamental period). Clamped to `max_buckets`.
/// * `num_harmonics`– number of harmonics to extract per bucket.
/// * `max_buckets`  – upper clamp matching the engine grid limits.
pub fn analyze_subtrack(
    samples: &[f32],
    sample_rate: f32,
    base_freq: f32,
    num_buckets: usize,
    num_harmonics: usize,
    max_buckets: usize,
) -> AnalysisResult {
    let num_harmonics = num_harmonics.max(1);
    let base_freq = base_freq.max(1.0);
    let base_period = (sample_rate / base_freq).max(2.0);

    // Decide how many buckets we span. Auto: one fundamental period each.
    let auto = ((samples.len() as f32 / base_period).floor() as usize).max(1);
    let buckets = if num_buckets == 0 { auto } else { num_buckets };
    let buckets = buckets.clamp(1, max_buckets.max(1));

    let mut amplitude = vec![vec![0.0f32; buckets]; num_harmonics];
    let mut phase = vec![vec![0.0f32; buckets]; num_harmonics];
    let mut bucket_periods = vec![base_period; buckets];

    if samples.is_empty() {
        return AnalysisResult { amplitude, phase, bucket_periods };
    }

    let bucket_len = (samples.len() as f32 / buckets as f32).max(2.0);

    for b in 0..buckets {
        let start = (b as f32 * bucket_len).floor() as usize;
        let win = bucket_len.ceil() as usize;
        let start = start.min(samples.len().saturating_sub(1));
        let end = (start + win).min(samples.len());
        let span = end.saturating_sub(start);
        if span < 2 {
            continue;
        }

        // Track the local period for continuity, then analyse one period worth
        // of signal (or the whole bucket if it is shorter than a period).
        let local_period = refine_period(samples, start, win, base_period);
        bucket_periods[b] = local_period;
        let analysis_len = (local_period.round() as usize).clamp(2, span);

        // Per-harmonic DFT at integer multiples of the local fundamental.
        let p = analysis_len as f32;
        for h in 0..num_harmonics {
            let k = (h + 1) as f32;
            // Don't analyse harmonics above Nyquist.
            if k * base_freq >= sample_rate * 0.5 {
                continue;
            }
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            for n in 0..analysis_len {
                let s = samples[start + n];
                let theta = 2.0 * PI * k * (n as f32) / p;
                re += s * theta.cos();
                im -= s * theta.sin();
            }
            let amp = 2.0 / p * (re * re + im * im).sqrt();
            let mut ph = im.atan2(re);
            if ph < 0.0 {
                ph += 2.0 * PI;
            }
            amplitude[h][b] = amp.clamp(0.0, 1.0);
            phase[h][b] = ph;
        }
    }

    AnalysisResult { amplitude, phase, bucket_periods }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_mode_roundtrip() {
        assert_eq!(ExecutionMode::from_u8(ExecutionMode::Synth.as_u8()), ExecutionMode::Synth);
        assert_eq!(ExecutionMode::from_u8(ExecutionMode::Analysis.as_u8()), ExecutionMode::Analysis);
        assert_eq!(ExecutionMode::default(), ExecutionMode::Synth);
    }

    #[test]
    fn pure_sine_lands_in_first_harmonic() {
        let sr = 44100.0;
        let freq = 220.0;
        let n = 44100; // 1 second
        let samples: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * freq * i as f32 / sr).sin())
            .collect();

        let res = analyze_subtrack(&samples, sr, freq, 0, 16, 2000);
        assert_eq!(res.num_harmonics(), 16);
        assert!(res.num_buckets() > 0);

        // The fundamental should carry essentially all of the energy.
        let mid = res.num_buckets() / 2;
        let h1 = res.amplitude[0][mid];
        let h2 = res.amplitude[1][mid];
        assert!(h1 > 0.5, "fundamental amp should be large, got {}", h1);
        assert!(h2 < h1 * 0.25, "2nd harmonic should be small, got {} vs {}", h2, h1);
    }

    #[test]
    fn empty_input_is_safe() {
        let res = analyze_subtrack(&[], 44100.0, 440.0, 0, 8, 2000);
        assert_eq!(res.num_harmonics(), 8);
        assert_eq!(res.num_buckets(), 1);
    }
}
