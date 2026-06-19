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
//! Bucket boundaries and the per-bucket DFT frequency follow a pitch *contour*
//! supplied by the host (the per-frame fundamental track of the subtrack),
//! rather than a single global period. In period-synchronous mode
//! (`num_buckets == 0`) the boundaries are walked one local period at a time, so
//! the grid tracks vibrato/drift and the per-harmonic amp/phase curves stay
//! continuous from bucket to bucket — the "most continuous functions" the
//! feature asks for. With an empty contour it falls back to a single global
//! `base_freq` (legacy behaviour).

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
    /// The local period (in samples) actually used for each bucket. With a pitch
    /// contour this follows vibrato/drift; flat otherwise.
    pub bucket_periods: Vec<f32>,
    /// Per-bucket fundamental relative to `base_freq` (`f_local / base_freq`,
    /// ≈1.0 ± a few %). This is the vibrato contour that survives to playback,
    /// where it transposes onto whatever key is pressed. All-ones when analysed
    /// without a contour.
    pub pitch_ratio: Vec<f32>,
}

impl AnalysisResult {
    pub fn num_harmonics(&self) -> usize {
        self.amplitude.len()
    }

    pub fn num_buckets(&self) -> usize {
        self.amplitude.first().map(|r| r.len()).unwrap_or(0)
    }
}

/// Scale an analysis grid so its strongest harmonic reaches `target`, making
/// the curves clearly visible on the 0..1 charts regardless of how quiet the
/// source recording is. Relative harmonic balance (and all phases) are
/// preserved. Grids with no real content (max below `MIN_CONTENT`) are left
/// untouched so pure noise/DC isn't amplified into a fake signal.
pub fn normalize_for_display(result: &mut AnalysisResult, target: f32) {
    const MIN_CONTENT: f32 = 0.01;
    let mut max = 0.0f32;
    for row in &result.amplitude {
        for &v in row {
            if v > max {
                max = v;
            }
        }
    }
    if max < MIN_CONTENT {
        return;
    }
    let gain = target / max;
    for row in &mut result.amplitude {
        for v in row.iter_mut() {
            *v = (*v * gain).min(1.0);
        }
    }
}

/// Absolute noise gate: a harmonic whose raw DFT amplitude is below this is
/// always treated as silence (keeps pure DC/noise from inventing a tone).
const AMP_FLOOR_ABS: f32 = 0.0004;
/// Relative amplitude gate, as a fraction of the *whole grid's* strongest
/// harmonic. Referencing the global maximum — not the local bucket or an
/// absolute level — means genuinely quiet but real *sustained* harmonics
/// survive on quiet recordings (the upper harmonics that an absolute floor used
/// to erase), while near-silent attack/decay buckets stay zeroed.
const AMP_FLOOR_REL: f32 = 0.004;
/// Phase-reliability gate, as a fraction of each *bucket's* strongest harmonic.
/// A harmonic quieter than this still contributes its amplitude, but its phase
/// is left at 0 (cosine-aligned). The phase of a weak harmonic is mostly noise;
/// emitting it would make consecutive buckets start incoherently and buzz, so
/// only the harmonics strong enough to carry a trustworthy phase get one.
const PHASE_REL: f32 = 0.05;

/// Local periods spanned per bucket in period-synchronous mode (`num_buckets ==
/// 0`). The bucket count then falls out as `subtrack_periods / this`, so the
/// grid tracks the source length instead of being a fixed number.
const PERIODS_PER_BUCKET: f32 = 4.0;
/// Analysis window width, in local periods. A little wider than the bucket hop
/// so adjacent buckets overlap → smoother amp/phase curves across buckets.
const WINDOW_PERIODS: f32 = 6.0;

/// One bucket's placement: window centre (source samples), window length, and
/// the local fundamental to run the DFT at.
struct BucketSpec {
    center: f32,
    win_len: usize,
    local_freq: f32,
}

/// Local fundamental (Hz) at source position `pos` in `[0, len)`, read from a
/// uniformly-resampled `contour` of absolute Hz with linear interpolation. An
/// empty contour means "flat" → `base_freq` everywhere (legacy behaviour).
fn local_freq_at(contour: &[f32], base_freq: f32, pos: f32, len: f32) -> f32 {
    match contour.len() {
        0 => base_freq,
        1 => contour[0],
        n => {
            let x = (pos / len.max(1.0) * n as f32).clamp(0.0, (n - 1) as f32);
            let i = x.floor() as usize;
            if i >= n - 1 {
                contour[n - 1]
            } else {
                contour[i] + (contour[i + 1] - contour[i]) * (x - i as f32)
            }
        }
    }
}

/// Lay out the buckets for a subtrack.
///
/// * `num_buckets > 0` – fixed count, centres spread uniformly in time (the
///   preview/host path that pre-allocated `num_buckets`); each window is still
///   sized to the *local* period so vibrato keeps the DFT coherent.
/// * `num_buckets == 0` – period-synchronous: walk the subtrack one local period
///   at a time, grouping [`PERIODS_PER_BUCKET`] periods per bucket. The count is
///   derived (and coarsened if it would exceed `max_buckets`).
fn build_bucket_specs(
    len: usize,
    sample_rate: f32,
    base_freq: f32,
    contour: &[f32],
    num_buckets: usize,
    max_buckets: usize,
) -> Vec<BucketSpec> {
    let lenf = len as f32;
    let win_for = |local_freq: f32| -> usize {
        let p = (sample_rate / local_freq.max(1.0)).max(2.0);
        ((p * WINDOW_PERIODS).round() as usize).clamp(2, len.max(2))
    };

    if num_buckets > 0 {
        let buckets = num_buckets.clamp(1, max_buckets.max(1));
        let hop = (lenf / buckets as f32).max(1.0);
        return (0..buckets)
            .map(|b| {
                let center = (b as f32 + 0.5) * hop;
                let local_freq = local_freq_at(contour, base_freq, center, lenf);
                BucketSpec { center, win_len: win_for(local_freq), local_freq }
            })
            .collect();
    }

    // Period-synchronous. Coarsen the periods-per-bucket if the natural count
    // would blow past the engine grid limit, so the whole subtrack is covered.
    let base_period = (sample_rate / base_freq).max(2.0);
    let natural = (lenf / (base_period * PERIODS_PER_BUCKET)).floor().max(1.0);
    let periods_per_bucket = if natural as usize > max_buckets.max(1) {
        lenf / (base_period * max_buckets.max(1) as f32)
    } else {
        PERIODS_PER_BUCKET
    }
    .max(0.5);

    let mut specs = Vec::new();
    let mut pos = 0.0f32;
    while pos < lenf && specs.len() < max_buckets.max(1) {
        let local_freq = local_freq_at(contour, base_freq, pos, lenf);
        let span = (sample_rate / local_freq.max(1.0) * periods_per_bucket).max(1.0);
        let center = pos + span * 0.5;
        specs.push(BucketSpec { center, win_len: win_for(local_freq), local_freq });
        pos += span;
    }
    if specs.is_empty() {
        let local_freq = local_freq_at(contour, base_freq, lenf * 0.5, lenf);
        specs.push(BucketSpec { center: lenf * 0.5, win_len: win_for(local_freq), local_freq });
    }
    specs
}

/// Analyse one subtrack into an amplitude/phase grid.
///
/// Buckets are laid out by [`build_bucket_specs`] — period-synchronously when
/// `num_buckets == 0`. For each bucket we take a Hann-windowed slice centred on
/// it and run a single-bin DFT at each harmonic's *local* absolute frequency
/// `(h+1) * f_local`, where `f_local` follows the pitch `contour`. Tracking the
/// local fundamental (rather than a single global one) keeps the DFT coherent
/// through vibrato/drift instead of smearing it into amplitude loss; referencing
/// the phase to an absolute sample index keeps it continuous across buckets.
///
/// * `samples`      – mono PCM of the subtrack.
/// * `sample_rate`  – Hz.
/// * `base_freq`    – median fundamental of the subtrack (Hz); transpose ref.
/// * `contour`      – per-position fundamental (absolute Hz), uniformly resampled
///                    across the subtrack; empty → flat at `base_freq` (legacy).
/// * `num_buckets`  – fixed count, or `0` for period-synchronous auto.
/// * `num_harmonics`– number of harmonics to extract per bucket.
/// * `max_buckets`  – upper clamp matching the engine grid limits.
pub fn analyze_subtrack(
    samples: &[f32],
    sample_rate: f32,
    base_freq: f32,
    contour: &[f32],
    num_buckets: usize,
    num_harmonics: usize,
    max_buckets: usize,
) -> AnalysisResult {
    let num_harmonics = num_harmonics.max(1);
    let base_freq = base_freq.max(1.0);
    let nyquist = sample_rate * 0.5;

    let len = samples.len();
    let specs = build_bucket_specs(len, sample_rate, base_freq, contour, num_buckets, max_buckets);
    let buckets = specs.len();

    let mut amplitude = vec![vec![0.0f32; buckets]; num_harmonics];
    let mut phase = vec![vec![0.0f32; buckets]; num_harmonics];
    let bucket_periods: Vec<f32> =
        specs.iter().map(|s| sample_rate / s.local_freq.max(1.0)).collect();
    let pitch_ratio: Vec<f32> = specs.iter().map(|s| s.local_freq / base_freq).collect();

    if len < 2 {
        return AnalysisResult { amplitude, phase, bucket_periods, pitch_ratio };
    }

    // Hann windows are cached per length: in period-synchronous mode every
    // bucket of a steady note shares one length, so this is usually a single
    // entry, but it stays correct when the local period changes.
    let mut hann_cache: std::collections::HashMap<usize, (Vec<f32>, f32)> =
        std::collections::HashMap::new();

    // Pass 1 — raw single-bin DFT per (harmonic, bucket). No gating yet; we need
    // the whole grid's peak before we can decide what counts as silence.
    // `raw_phase` is in the synthesis (`sin`) convention: the DFT angle
    // `atan2(im, re)` is the phase of a *cosine* at that bin while resynthesis
    // renders `sin`, so we add π/2 — a source `A·sin(w·n + ψ)` reads as `ψ − π/2`.
    let mut raw_amp = vec![vec![0.0f32; buckets]; num_harmonics];
    let mut raw_phase = vec![vec![0.0f32; buckets]; num_harmonics]; // ψ (sin convention)
    let mut global_max = 0.0f32;
    for (b, spec) in specs.iter().enumerate() {
        let win_len = spec.win_len.min(len);
        let (hann, wsum) = hann_cache.entry(win_len).or_insert_with(|| {
            let h: Vec<f32> = (0..win_len)
                .map(|i| {
                    let x = 2.0 * PI * i as f32 / win_len as f32;
                    0.5 - 0.5 * x.cos()
                })
                .collect();
            let s = h.iter().sum::<f32>().max(1e-6);
            (h, s)
        });

        let start = (spec.center - win_len as f32 * 0.5)
            .max(0.0)
            .min((len - win_len) as f32) as usize;

        for h in 0..num_harmonics {
            let f = (h + 1) as f32 * spec.local_freq;
            if f >= nyquist {
                continue; // harmonic above Nyquist isn't in the source at all
            }
            let w = 2.0 * PI * f / sample_rate;
            let mut re = 0.0f32;
            let mut im = 0.0f32;
            for i in 0..win_len {
                let s = hann[i] * samples[start + i];
                let theta = w * (start + i) as f32;
                re += s * theta.cos();
                im -= s * theta.sin();
            }
            let amp = (2.0 / *wsum * (re * re + im * im).sqrt()).min(1.0);
            raw_amp[h][b] = amp;
            raw_phase[h][b] = im.atan2(re) + 0.5 * PI;
            if amp > global_max {
                global_max = amp;
            }
        }
    }

    // Grid-relative amplitude gate: quiet but real sustained harmonics survive,
    // near-silent buckets stay zeroed. (Absolute fallback for pure noise/DC.)
    let amp_floor = (AMP_FLOOR_REL * global_max).max(AMP_FLOOR_ABS);

    // Pass 2 — gate and store. Phase is kept *relative to the fundamental*
    // (`ψ_k − k·ψ_1`): the raw DFT phase is tied to the absolute source sample
    // index, so it differs per bucket and — since resynthesis restarts each
    // bucket at t=0 — would make consecutive buckets start incoherently and
    // smear into noise. The relative phase describes the waveform *shape* only,
    // which is position-independent and stays continuous across buckets. A
    // harmonic too weak within its bucket (or sitting on a silent fundamental)
    // gets phase 0 (cosine-aligned, continuous) rather than a noisy one.
    for b in 0..buckets {
        let bucket_max = (0..num_harmonics).fold(0.0f32, |m, h| m.max(raw_amp[h][b]));
        let phase_gate = PHASE_REL * bucket_max;
        let fund = raw_phase[0][b];
        let fund_voiced = raw_amp[0][b] >= amp_floor;
        for h in 0..num_harmonics {
            let a = raw_amp[h][b];
            if a < amp_floor {
                continue; // leave amplitude/phase at 0
            }
            amplitude[h][b] = a;
            if fund_voiced && a >= phase_gate {
                let k = (h + 1) as f32;
                phase[h][b] = (raw_phase[h][b] - k * fund).rem_euclid(2.0 * PI);
            }
        }
    }

    AnalysisResult { amplitude, phase, bucket_periods, pitch_ratio }
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

        let res = analyze_subtrack(&samples, sr, freq, &[], 0, 16, 2000);
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
    fn harmonic_rich_signal_recovers_amplitudes() {
        // Sum of three harmonics with known amplitudes — like a bowed string.
        let sr = 44_100.0;
        let f = 196.0; // ~G3, a typical violin note
        let n = (sr as usize) / 2;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / sr;
                0.5 * (2.0 * PI * f * t).sin()
                    + 0.25 * (2.0 * PI * 2.0 * f * t).sin()
                    + 0.125 * (2.0 * PI * 3.0 * f * t).sin()
            })
            .collect();

        let res = analyze_subtrack(&samples, sr, f, &[], 0, 16, 2000);
        let mid = res.num_buckets() / 2;
        let (a1, a2, a3) = (res.amplitude[0][mid], res.amplitude[1][mid], res.amplitude[2][mid]);

        // Amplitudes must be recovered near their true values (not ~0).
        assert!((a1 - 0.5).abs() < 0.08, "H1 amp {} (want ~0.5)", a1);
        assert!((a2 - 0.25).abs() < 0.06, "H2 amp {} (want ~0.25)", a2);
        assert!((a3 - 0.125).abs() < 0.05, "H3 amp {} (want ~0.125)", a3);
        // Harmonics above the 3rd are silent → phase left at 0 (no noise).
        assert!(res.amplitude[5][mid] < 0.02);
        assert_eq!(res.phase[5][mid], 0.0);
    }

    #[test]
    fn phase_is_recovered_relative_to_fundamental() {
        // x = sin(w n + 0.3) + 0.5·sin(2w n + 1.1). Phases are stored in the
        // synthesis (sin) convention, relative to the fundamental, so:
        //   H1 → 0 (fundamental is the reference)
        //   H2 → ψ_2 − 2·ψ_1 = 1.1 − 2·0.3 = 0.5
        let sr = 44_100.0;
        let f = 440.0;
        let w = 2.0 * PI * f / sr;
        let n = sr as usize / 2;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let x = w * i as f32;
                (x + 0.3).sin() + 0.5 * (2.0 * x + 1.1).sin()
            })
            .collect();

        let res = analyze_subtrack(&samples, sr, f, &[], 0, 8, 2000);
        let mid = res.num_buckets() / 2;

        let dist = |a: f32, b: f32| {
            let d = (a - b).rem_euclid(2.0 * PI);
            d.min(2.0 * PI - d)
        };
        assert!(
            dist(res.phase[0][mid], 0.0) < 0.05,
            "H1 phase should be ~0 (fundamental reference), got {}",
            res.phase[0][mid]
        );
        assert!(
            dist(res.phase[1][mid], 0.5) < 0.05,
            "H2 relative phase should be ~0.5, got {}",
            res.phase[1][mid]
        );
    }

    #[test]
    fn relative_phase_is_stable_across_buckets() {
        // A steady multi-harmonic tone: the stored phase describes the waveform
        // shape, so it must be ~constant across buckets — no per-bucket jumps
        // (those are what smear resynthesis into noise).
        let sr = 44_100.0;
        let f = 587.33; // ~D5
        let w = 2.0 * PI * f / sr;
        let n = sr as usize; // 1 s
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let x = w * i as f32;
                (x + 0.7).sin() + 0.4 * (2.0 * x + 2.0).sin() + 0.2 * (3.0 * x + 1.0).sin()
            })
            .collect();

        let res = analyze_subtrack(&samples, sr, f, &[], 0, 8, 2000);
        let buckets = res.num_buckets();
        assert!(buckets > 8);

        let dist = |a: f32, b: f32| {
            let d = (a - b).rem_euclid(2.0 * PI);
            d.min(2.0 * PI - d)
        };
        // Compare interior buckets (skip the clamped first/last windows) for H2/H3.
        for h in [1usize, 2] {
            let ref_ph = res.phase[h][buckets / 2];
            for b in 2..buckets - 2 {
                if res.amplitude[h][b] <= 0.0 {
                    continue;
                }
                assert!(
                    dist(res.phase[h][b], ref_ph) < 0.2,
                    "H{} phase jumped at bucket {}: {} vs {}",
                    h + 1,
                    b,
                    res.phase[h][b],
                    ref_ph
                );
            }
        }
    }

    #[test]
    fn contour_tracks_vibrato_into_pitch_ratio() {
        // A 5 Hz, ±3% vibrato around 220 Hz. f0(t) = 220·(1 + 0.03·sin(2π·5t)).
        let sr = 44_100.0;
        let base = 220.0f32;
        let depth = 0.03f32;
        let rate = 5.0f32;
        let n = sr as usize; // 1 s
        // Build the signal from the integrated instantaneous phase.
        let mut phase = 0.0f32;
        let mut samples = Vec::with_capacity(n);
        let mut contour = Vec::with_capacity(n / 256);
        for i in 0..n {
            let t = i as f32 / sr;
            let f = base * (1.0 + depth * (2.0 * PI * rate * t).sin());
            phase += 2.0 * PI * f / sr;
            samples.push(phase.sin());
            if i % 256 == 0 {
                contour.push(f); // uniformly-resampled contour, ~one per 256 samples
            }
        }

        // Period-synchronous, with the true contour.
        let res = analyze_subtrack(&samples, sr, base, &contour, 0, 8, 2000);
        assert!(res.num_buckets() > 10);
        // pitch_ratio should swing roughly ±depth and stay centred near 1.
        let max = res.pitch_ratio.iter().cloned().fold(f32::MIN, f32::max);
        let min = res.pitch_ratio.iter().cloned().fold(f32::MAX, f32::min);
        assert!(max > 1.0 + depth * 0.5, "ratio peak too low: {}", max);
        assert!(min < 1.0 - depth * 0.5, "ratio trough too high: {}", min);
        // With the contour tracked, H1 amplitude stays strong throughout (no
        // vibrato→amplitude leakage that a fixed-frequency DFT would suffer).
        let h1_min = res.amplitude[0].iter().cloned().fold(f32::MAX, f32::min);
        assert!(h1_min > 0.4, "H1 collapsed somewhere: {}", h1_min);
    }

    #[test]
    fn empty_input_is_safe() {
        let res = analyze_subtrack(&[], 44100.0, 440.0, &[], 0, 8, 2000);
        assert_eq!(res.num_harmonics(), 8);
        assert!(res.num_buckets() >= 1);
        // No samples → all silent.
        assert!(res.amplitude.iter().all(|row| row.iter().all(|&a| a == 0.0)));
    }
}
