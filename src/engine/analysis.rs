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

//! Analysis mode: build the amp/phase grid *from* recorded audio.
//!
//! One bucket = one local pitch period of `N` samples = a real FFT of exactly
//! those `N` samples, harmonics `k < N/2` stored with **absolute** phase. That
//! is an invertible transform: the inverse FFT returns the samples, and
//! concatenating the buckets returns the subtrack ([`super::resynthesize_exact`]).
//! No window, no overlap, no decimation, no gating — each would destroy it.
//!
//! Absolute phase works because one period advances harmonic `k` by exactly
//! `2πk`, so bucket starts are automatically phase-continuous. Bucket
//! boundaries follow the host's pitch `contour`, so they stay locked to the
//! waveform through vibrato; an empty contour means flat at `base_freq`.

use std::f32::consts::PI;

use realfft::RealFftPlanner;

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
    /// Each bucket's length in samples — a whole number, and the length of the
    /// inverse FFT that reproduces it. These sum to the subtrack length.
    pub bucket_periods: Vec<f32>,
    /// Per-bucket fundamental relative to `base_freq` (`f_local / base_freq`,
    /// ≈1.0 ± a few %). This is the vibrato contour that survives to playback,
    /// where it transposes onto whatever key is pressed.
    pub pitch_ratio: Vec<f32>,
    /// Per-bucket DC (bin 0). Not a harmonic, so it has no grid row; dropping
    /// it steps the waveform at every period boundary (~120 dB of the error).
    pub dc: Vec<f32>,
    /// Per-bucket Nyquist term (bin `N/2`, even `N` only), real-valued. Kept
    /// for the same reason as [`Self::dc`] (~93 dB).
    pub nyquist: Vec<f32>,
    /// Periods folded into each bucket; `1.0` unless one per period would
    /// exceed the grid's column limit.
    pub periods_per_bucket: f32,
    /// A bucket held more harmonics than the grid has rows, so its top bins
    /// were dropped.
    pub truncated: bool,
}

impl AnalysisResult {
    pub fn num_harmonics(&self) -> usize {
        self.amplitude.len()
    }

    pub fn num_buckets(&self) -> usize {
        self.amplitude.first().map(|r| r.len()).unwrap_or(0)
    }
}

/// Scale the grid so its strongest harmonic reaches `target`, for chart
/// legibility. Balance and phase are preserved; a grid with no real content
/// (below `MIN_CONTENT`) is left alone.
///
/// **Returns the gain**, so `grid_amplitude = source_amplitude × gain`. A
/// display decision, not an analysis one: reproducing the source divides it
/// back out. Dropping it played resynthesis ~19 dB hot.
#[must_use]
pub fn normalize_for_display(result: &mut AnalysisResult, target: f32) -> f32 {
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
        return 1.0;
    }
    let gain = target / max;
    for row in &mut result.amplitude {
        for v in row.iter_mut() {
            *v *= gain;
        }
    }
    // DC and Nyquist are part of the same transform and must move with it: the
    // inverse divides the whole reconstruction by this gain, so a bin that
    // missed it comes back wrong by exactly that factor (-22 dB, not -126).
    for v in result.dc.iter_mut().chain(result.nyquist.iter_mut()) {
        *v *= gain;
    }
    gain
}

/// Deliberately **no amplitude or phase gates in this file.** Three used to
/// live here; each threw away data the inverse needs (the phase gate misplaced
/// audible harmonics by over 100°), and noise became tone. A floor for chart
/// legibility or denoising belongs downstream, not in the transform.

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

/// Cut `len` samples into contiguous, non-overlapping buckets of one local
/// period each, returned as `(start, length)` in samples.
///
/// The walk carries a **fractional** position and rounds only the boundaries,
/// so lengths track the true period (222/223 alternating for 222.4) instead of
/// drifting off the waveform. Buckets tile exactly — `start[b] + len[b] ==
/// start[b+1]`, last ends at `len` — which is what makes the concatenated
/// inverse reproduce the input.
///
/// Past `max_buckets`, periods are grouped uniformly instead
/// ([`AnalysisResult::periods_per_bucket`]); still invertible, coarser.
fn build_period_bounds(
    len: usize,
    sample_rate: f32,
    base_freq: f32,
    contour: &[f32],
    max_buckets: usize,
) -> (Vec<(usize, usize)>, f32) {
    let lenf = len as f32;
    let max_buckets = max_buckets.max(1);

    // How many periods fit, and therefore whether we can afford one per bucket.
    let base_period = (sample_rate / base_freq).max(2.0) as f64;
    let natural = (lenf as f64 / base_period).ceil().max(1.0);
    let periods_per_bucket = (natural / max_buckets as f64).ceil().max(1.0);

    let mut bounds = Vec::new();
    let mut pos = 0.0f64; // fractional start of the current bucket
    while (pos as usize) < len && bounds.len() < max_buckets {
        let f = local_freq_at(contour, base_freq, pos as f32, lenf);
        let span = (sample_rate / f.max(1.0)) as f64 * periods_per_bucket;
        let start = pos.round() as usize;
        let mut end = (pos + span).round() as usize;
        // The final bucket absorbs whatever is left, so the tiling is exact and
        // no samples are dropped.
        if end >= len || bounds.len() + 1 == max_buckets {
            end = len;
        }
        if end <= start {
            break;
        }
        bounds.push((start, end - start));
        pos += span;
        if end == len {
            break;
        }
    }
    if bounds.is_empty() && len > 0 {
        bounds.push((0, len));
    }
    (bounds, periods_per_bucket as f32)
}

/// Analyse one subtrack into an amplitude/phase grid, invertibly.
///
/// Each bucket is one local pitch period of `N` samples, transformed by a real
/// FFT of exactly those samples. Harmonic `k` is DFT bin `k`, stored as
///
/// ```text
/// amplitude[k-1][b] = 2·|X_k| / N        phase[k-1][b] = arg(X_k) + π/2
/// ```
///
/// The `+ π/2` converts the DFT's cosine reference to the sine convention the
/// renderer uses: `A·sin(2πkn/N + φ)` ↔ `X_k = (A·N/2)·e^{i(φ − π/2)}`. Phase
/// is absolute (the angle at the bucket's first sample) — what the inverse
/// needs. DC and, for even `N`, Nyquist are not harmonics and so have no grid
/// row; they come back in [`AnalysisResult::dc`] / [`AnalysisResult::nyquist`].
///
/// * `base_freq`    – median fundamental (Hz); the transpose reference.
/// * `contour`      – per-position fundamental (Hz); empty → flat.
/// * `num_buckets`  – `0` = one bucket per period, the invertible layout.
///                    `> 0` averages that grid down for the preview chart, so
///                    the two cannot drift apart. **Only `0` is invertible.**
/// * `num_harmonics`– grid height; `N/2` above it is dropped
///                    ([`AnalysisResult::truncated`]).
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
    let len = samples.len();

    let (bounds, periods_per_bucket) =
        build_period_bounds(len, sample_rate, base_freq, contour, max_buckets);
    let buckets = bounds.len().max(1);

    let mut amplitude = vec![vec![0.0f32; buckets]; num_harmonics];
    let mut phase = vec![vec![0.0f32; buckets]; num_harmonics];
    let mut dc = vec![0.0f32; buckets];
    let mut nyquist = vec![0.0f32; buckets];
    // Not the same quantity: `bucket_periods` is the inverse-FFT length,
    // `pitch_ratio` the local pitch to transpose from. The last bucket absorbs
    // the remainder, so deriving its ratio from its length reports a pitch jump
    // that is not in the source.
    let bucket_periods: Vec<f32> = bounds.iter().map(|&(_, n)| n as f32).collect();
    let lenf = len as f32;
    let pitch_ratio: Vec<f32> = bounds
        .iter()
        .map(|&(start, n)| {
            let centre = start as f32 + n as f32 * 0.5;
            local_freq_at(contour, base_freq, centre, lenf) / base_freq
        })
        .collect();
    let mut truncated = false;

    if len < 2 || bounds.is_empty() {
        return AnalysisResult {
            amplitude,
            phase,
            bucket_periods,
            pitch_ratio,
            dc,
            nyquist,
            periods_per_bucket,
            truncated,
        };
    }

    // Plans are cached by length; a steady note uses one or two distinct
    // lengths, vibrato a handful.
    let mut planner = RealFftPlanner::<f32>::new();

    for (b, &(start, n)) in bounds.iter().enumerate() {
        if n < 2 {
            continue;
        }
        let fft = planner.plan_fft_forward(n);
        let mut input = fft.make_input_vec();
        input.copy_from_slice(&samples[start..start + n]);
        let mut spectrum = fft.make_output_vec(); // n/2 + 1 bins
        if fft.process(&mut input, &mut spectrum).is_err() {
            continue;
        }

        let inv_n = 1.0 / n as f32;
        dc[b] = spectrum[0].re * inv_n;
        // Highest harmonic genuinely present: bin k is a harmonic while k < n/2.
        // For even `n`, bin n/2 is the real-valued Nyquist term, kept aside.
        let top = (n - 1) / 2;
        if n % 2 == 0 {
            nyquist[b] = spectrum[n / 2].re * inv_n;
        }
        if top > num_harmonics {
            truncated = true;
        }
        for k in 1..=top.min(num_harmonics) {
            let c = spectrum[k];
            amplitude[k - 1][b] = 2.0 * inv_n * (c.re * c.re + c.im * c.im).sqrt();
            phase[k - 1][b] = (c.im.atan2(c.re) + 0.5 * PI).rem_euclid(2.0 * PI);
        }
    }

    let result = AnalysisResult {
        amplitude,
        phase,
        bucket_periods,
        pitch_ratio,
        dc,
        nyquist,
        periods_per_bucket,
        truncated,
    };

    // A fixed column count is a *view* of the per-period grid, never a different
    // analysis: averaging the invertible result keeps the preview chart and the
    // audible render describing the same thing.
    if num_buckets > 0 && num_buckets != result.num_buckets() {
        return resample_buckets(&result, num_buckets.min(max_buckets.max(1)));
    }
    result
}

/// Average an analysis grid down (or spread it up) to `target` columns, for the
/// preview chart. Amplitudes average linearly; phases average as unit vectors so
/// the wrap at 2π does not pull the mean towards π. The result is **not**
/// invertible — it is a picture of one, and nothing that has to reproduce audio
/// should use it.
fn resample_buckets(src: &AnalysisResult, target: usize) -> AnalysisResult {
    let target = target.max(1);
    let nb = src.num_buckets();
    let nh = src.num_harmonics();
    let mut amplitude = vec![vec![0.0f32; target]; nh];
    let mut phase = vec![vec![0.0f32; target]; nh];
    let mut bucket_periods = vec![0.0f32; target];
    let mut pitch_ratio = vec![1.0f32; target];
    let mut dc = vec![0.0f32; target];
    let mut nyquist = vec![0.0f32; target];

    for t in 0..target {
        let from = t * nb / target;
        let to = (((t + 1) * nb) / target).max(from + 1).min(nb);
        let count = (to - from) as f32;
        for h in 0..nh {
            let a: f32 = (from..to).map(|b| src.amplitude[h][b]).sum::<f32>() / count;
            amplitude[h][t] = a;
            let (mut x, mut y) = (0.0f32, 0.0f32);
            for b in from..to {
                let w = src.amplitude[h][b];
                x += w * src.phase[h][b].cos();
                y += w * src.phase[h][b].sin();
            }
            phase[h][t] = y.atan2(x).rem_euclid(2.0 * PI);
        }
        bucket_periods[t] =
            (from..to).map(|b| src.bucket_periods[b]).sum::<f32>() / count;
        pitch_ratio[t] = (from..to).map(|b| src.pitch_ratio[b]).sum::<f32>() / count;
        dc[t] = (from..to).map(|b| src.dc[b]).sum::<f32>() / count;
        nyquist[t] = (from..to).map(|b| src.nyquist[b]).sum::<f32>() / count;
    }

    AnalysisResult {
        amplitude,
        phase,
        bucket_periods,
        pitch_ratio,
        dc,
        nyquist,
        periods_per_bucket: src.periods_per_bucket * nb as f32 / target as f32,
        truncated: src.truncated,
    }
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
        // Harmonics above the 3rd are silent. Their *amplitude* is what says so;
        // their phase is now left as whatever the transform reports, because
        // forcing it to 0 would make the transform non-invertible. (The old
        // `assert_eq!(phase, 0.0)` here was asserting the gate that is gone.)
        assert!(res.amplitude[5][mid] < 0.02);
    }

    /// Phase is absolute (the angle at each bucket's first sample), not relative
    /// to the fundamental: `ψ_k − k·ψ_1` discarded ψ₁ and with it the inverse.
    #[test]
    fn phase_is_absolute_at_the_bucket_start() {
        // x = sin(w n + 0.3) + 0.5·sin(2w n + 1.1), with a period of exactly 100
        // samples so bucket b starts at sample 100b and every harmonic returns to
        // the same angle there.
        let sr = 44_100.0;
        let f = 441.0; // period = 100 samples exactly
        let w = 2.0 * PI * f / sr;
        let n = sr as usize / 2;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let x = w * i as f32;
                (x + 0.3).sin() + 0.5 * (2.0 * x + 1.1).sin()
            })
            .collect();

        let res = analyze_subtrack(&samples, sr, f, &[], 0, 8, 4000);
        let mid = res.num_buckets() / 2;

        let dist = |a: f32, b: f32| {
            let d = (a - b).rem_euclid(2.0 * PI);
            d.min(2.0 * PI - d)
        };
        assert!(
            dist(res.phase[0][mid], 0.3) < 0.05,
            "H1 absolute phase should be ~0.3, got {}",
            res.phase[0][mid]
        );
        assert!(
            dist(res.phase[1][mid], 1.1) < 0.05,
            "H2 absolute phase should be ~1.1, got {}",
            res.phase[1][mid]
        );
        // And it is genuinely continuous across buckets without any relative
        // encoding: one period advances harmonic k by exactly 2πk.
        for b in 1..res.num_buckets() - 1 {
            assert!(
                dist(res.phase[1][b], 1.1) < 0.05,
                "H2 phase drifted at bucket {b}: {}",
                res.phase[1][b]
            );
        }
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

#[cfg(test)]
mod invertibility_tests {
    use super::*;
    use crate::engine::resynthesize_exact;

    /// Reconstruction error as a fraction of the source's own peak.
    fn max_dev(src: &[f32], rec: &[f32]) -> f32 {
        let peak = src.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
        let n = src.len().min(rec.len());
        (0..n).fold(0.0f32, |m, i| m.max((src[i] - rec[i]).abs())) / peak
    }

    fn roundtrip(samples: &[f32], sr: f32, f0: f32, contour: &[f32]) -> Vec<f32> {
        let res = analyze_subtrack(samples, sr, f0, contour, 0, 256, 4000);
        let lens: Vec<usize> = res.bucket_periods.iter().map(|&p| p as usize).collect();
        resynthesize_exact(&res.amplitude, &res.phase, &lens, &res.dc, &res.nyquist, &[], &[], 0.0, 1.0)
    }

    /// The whole point: analysis followed by its inverse *returns* the input —
    /// not "correlates well with", not "matches per period".
    #[test]
    fn analysis_is_invertible_on_a_steady_tone() {
        let sr = 44_100.0;
        let f = 587.33;
        let w = 2.0 * PI * f / sr;
        let n = sr as usize / 2;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let x = w * i as f32;
                0.6 * (x + 0.7).sin() + 0.3 * (2.0 * x + 2.0).sin() + 0.1 * (7.0 * x + 1.0).sin()
            })
            .collect();
        let rec = roundtrip(&samples, sr, f, &[]);
        assert_eq!(rec.len(), samples.len(), "reconstruction changed length");
        let dev = max_dev(&samples, &rec);
        assert!(dev < 1e-4, "steady tone did not round trip: {:.3e} of peak", dev);
    }

    /// Noise is signal, and must come back too. Every amplitude/phase gate
    /// failed here, which is why speech resynthesised worse than a violin.
    #[test]
    fn analysis_is_invertible_on_noise() {
        let sr = 24_000.0;
        let f = 107.0;
        let n = 12_000;
        let mut state = 0x12345678u32;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / 8_388_608.0 - 1.0;
                let tone = (2.0 * PI * f * i as f32 / sr).sin();
                0.7 * tone + 0.3 * noise
            })
            .collect();
        let rec = roundtrip(&samples, sr, f, &[]);
        let dev = max_dev(&samples, &rec);
        assert!(dev < 1e-4, "noisy signal did not round trip: {:.3e} of peak", dev);
    }

    /// A low fundamental (long bucket, many harmonics) must cost nothing — the
    /// symptom that made low pitch look like it needed special handling.
    #[test]
    fn low_fundamental_round_trips_as_well_as_a_high_one() {
        let sr = 48_000.0;
        let n = 24_000;
        let build = |f: f32| -> Vec<f32> {
            let w = 2.0 * PI * f / sr;
            (0..n)
                .map(|i| {
                    let x = w * i as f32;
                    (1..=12)
                        .map(|k| (1.0 / k as f32) * (k as f32 * x + k as f32 * 0.3).sin())
                        .sum::<f32>()
                        * 0.2
                })
                .collect()
        };
        for f in [98.0f32, 587.33] {
            let s = build(f);
            let dev = max_dev(&s, &roundtrip(&s, sr, f, &[]));
            assert!(dev < 1e-4, "f0 {f} Hz did not round trip: {:.3e} of peak", dev);
        }
    }

    /// Vibrato moves the bucket boundaries; the tiling must stay exact.
    #[test]
    fn analysis_is_invertible_through_vibrato() {
        let sr = 44_100.0;
        let base = 220.0f32;
        let n = sr as usize / 2;
        let mut ph = 0.0f32;
        let mut samples = Vec::with_capacity(n);
        let mut contour = Vec::new();
        for i in 0..n {
            let t = i as f32 / sr;
            let f = base * (1.0 + 0.03 * (2.0 * PI * 5.0 * t).sin());
            ph += 2.0 * PI * f / sr;
            samples.push(0.8 * ph.sin() + 0.2 * (3.0 * ph + 0.4).sin());
            if i % 256 == 0 {
                contour.push(f);
            }
        }
        let dev = max_dev(&samples, &roundtrip(&samples, sr, base, &contour));
        assert!(dev < 1e-4, "vibrato did not round trip: {:.3e} of peak", dev);
    }

    /// The buckets must tile the subtrack exactly — no gaps, no overlap, no
    /// dropped tail. Everything above depends on this.
    #[test]
    fn buckets_tile_the_subtrack_exactly() {
        let (bounds, ppb) = build_period_bounds(10_000, 44_100.0, 220.0, &[], 4000);
        assert_eq!(ppb, 1.0, "should afford one period per bucket here");
        assert_eq!(bounds[0].0, 0, "first bucket must start at 0");
        for w in bounds.windows(2) {
            assert_eq!(w[0].0 + w[0].1, w[1].0, "buckets must be contiguous");
        }
        let last = bounds.last().unwrap();
        assert_eq!(last.0 + last.1, 10_000, "buckets must cover the whole subtrack");
        // ~200.45 samples per period at 220 Hz / 44.1 kHz, so buckets alternate
        // 200/201 and stay locked to the waveform. The last one is excluded: it
        // absorbs the remainder so the tiling covers the subtrack exactly.
        assert!(
            bounds[..bounds.len() - 1].iter().all(|&(_, n)| (200..=201).contains(&n)),
            "period drifted off the waveform"
        );
    }

    /// Grouping costs resolution, not invertibility: a bucket is still `n`
    /// contiguous samples with harmonics in bins `1..n/2`, so the inverse still
    /// returns them. ("Harmonic k" then means the k-th of the group, which only
    /// matters when transposing.) Hence the audition no longer refuses a grouped
    /// grid — it used to fall back to the fuzzy renderer past 2000 periods,
    /// i.e. after 3.4 s of a violin D5.
    #[test]
    fn grouped_buckets_are_still_invertible() {
        let sr = 44_100.0;
        let f = 587.33;
        let n = 88_200; // 2 s → ~1175 periods
        let w = 2.0 * PI * f / sr;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let x = w * i as f32;
                0.5 * (x + 0.7).sin() + 0.25 * (2.0 * x + 2.0).sin() + 0.1 * (5.0 * x + 1.0).sin()
            })
            .collect();
        // 200 buckets for ~1175 periods forces six periods into each.
        let res = analyze_subtrack(&samples, sr, f, &[], 0, 256, 200);
        assert!(res.periods_per_bucket > 1.0, "test did not force grouping");
        assert!(!res.truncated, "grouping should not truncate at this pitch");
        let lens: Vec<usize> = res.bucket_periods.iter().map(|&p| p as usize).collect();
        let rec =
            resynthesize_exact(&res.amplitude, &res.phase, &lens, &res.dc, &res.nyquist, &[], &[], 0.0, 1.0);
        assert_eq!(rec.len(), samples.len());
        let dev = max_dev(&samples, &rec);
        assert!(dev < 1e-4, "grouped buckets did not round trip: {:.3e} of peak", dev);
    }

    /// `truncated` (bucket Nyquist past row 256, i.e. f0 under ~90 Hz) is a
    /// real loss: a fractional period in a whole-sample bucket leaks across the
    /// spectrum, and the leakage above row 256 goes with the rest — 4.9% of
    /// peak on a 39 Hz sine. That is the grid's row limit and no renderer can
    /// undo it. What the audition *can* pick is which path renders the rows
    /// that survived, which is what this measures.
    #[test]
    fn a_truncated_grid_still_inverts_better_than_it_renders() {
        use crate::engine::synth_compute_engine::resynthesize_grid;

        let sr = 48_000.0;
        let f = 39.0; // period 1230.8 samples: fractional, and past row 256
        let n = 24_000;
        let w = 2.0 * PI * f / sr;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let x = w * i as f32;
                0.5 * (x + 0.7).sin() + 0.2 * (3.0 * x + 2.0).sin()
            })
            .collect();
        let res = analyze_subtrack(&samples, sr, f, &[], 0, 256, 4000);
        assert!(res.truncated, "test did not reach the grid's harmonic limit");

        let lens: Vec<usize> = res.bucket_periods.iter().map(|&p| p as usize).collect();
        let inverted =
            resynthesize_exact(&res.amplitude, &res.phase, &lens, &res.dc, &res.nyquist, &[], &[], 0.0, 1.0);
        // The path the gate used to force. `display_gain = 1.0` makes
        // `source_level_scale` undo exactly the clip-safety divisor that
        // `resynthesize_grid` applies, so both renders sit at the source's level
        // and the comparison is about waveform, not gain.
        let rendered = resynthesize_grid(
            &res.amplitude,
            &res.phase,
            &res.pitch_ratio,
            sr / f,
            0,
            samples.len(),
            1.0,
        );

        let inv = max_dev(&samples, &inverted);
        let ren = max_dev(&samples, &rendered);
        println!(
            "truncated grid: exact inverse {:.1} dB, transposing renderer {:.1} dB",
            20.0 * inv.log10(),
            20.0 * ren.log10()
        );
        assert!(
            inv < ren,
            "the exact inverse ({inv:.3e}) is not better than the renderer ({ren:.3e}); \
             the gate that preferred the renderer would be justified"
        );
    }

    /// When one bucket per period would exceed the grid, periods are grouped —
    /// uniformly, and the tiling still has to hold.
    #[test]
    fn grouping_kicks_in_only_at_the_grid_limit() {
        let (bounds, ppb) = build_period_bounds(200_000, 44_100.0, 587.33, &[], 100);
        assert!(ppb > 1.0, "should have grouped periods to fit 100 buckets");
        assert!(bounds.len() <= 100, "exceeded max_buckets: {}", bounds.len());
        for w in bounds.windows(2) {
            assert_eq!(w[0].0 + w[0].1, w[1].0);
        }
        assert_eq!(bounds.last().unwrap().0 + bounds.last().unwrap().1, 200_000);
    }
}

#[cfg(test)]
mod non_harmonic_bin_tests {
    use super::*;
    use crate::engine::resynthesize_exact;

    /// DC and Nyquist each cost an ABI field and a file-format field, so:
    /// how much do they carry? Reported in dB relative to the source's peak.
    #[test]
    fn report_cost_of_dropping_dc_and_nyquist() {
        let sr = 24_000.0;
        let f = 107.0;
        let n = 12_000;
        let mut state = 0xC0FFEEu32;
        // Tone + noise + a deliberate DC offset, i.e. the awkward case.
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / 8_388_608.0 - 1.0;
                0.6 * (2.0 * PI * f * i as f32 / sr).sin() + 0.25 * noise + 0.05
            })
            .collect();

        let res = analyze_subtrack(&samples, sr, f, &[], 0, 256, 4000);
        let lens: Vec<usize> = res.bucket_periods.iter().map(|&p| p as usize).collect();
        let peak = samples.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
        let err = |rec: &[f32]| {
            let n = samples.len().min(rec.len());
            let e = (0..n).fold(0.0f32, |m, i| m.max((samples[i] - rec[i]).abs())) / peak;
            20.0 * e.max(1e-12).log10()
        };

        let full = resynthesize_exact(&res.amplitude, &res.phase, &lens, &res.dc, &res.nyquist, &[], &[], 0.0, 1.0);
        let no_dc = resynthesize_exact(&res.amplitude, &res.phase, &lens, &[], &res.nyquist, &[], &[], 0.0, 1.0);
        let no_nyq = resynthesize_exact(&res.amplitude, &res.phase, &lens, &res.dc, &[], &[], &[], 0.0, 1.0);
        let neither = resynthesize_exact(&res.amplitude, &res.phase, &lens, &[], &[], &[], &[], 0.0, 1.0);

        println!("peak reconstruction error, dB relative to source peak:");
        println!("  harmonics + DC + Nyquist : {:.1} dB", err(&full));
        println!("  without DC               : {:.1} dB", err(&no_dc));
        println!("  without Nyquist          : {:.1} dB", err(&no_nyq));
        println!("  harmonics only           : {:.1} dB", err(&neither));
        assert!(err(&full) < -80.0, "the full inverse must be essentially exact");
    }
}

#[cfg(test)]
mod rate_conversion_tests {
    use super::*;
    use crate::engine::resynthesize_exact;

    /// Dominant frequency by plain DFT peak — deliberately independent of the
    /// analysis code under test.
    fn dominant_freq(x: &[f32], sr: f32) -> f32 {
        let n = x.len().min(8192);
        let bin = |f: f32| -> f64 {
            let w = 2.0 * std::f64::consts::PI * f as f64 / sr as f64;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for i in 0..n {
                let h = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos();
                let s = x[i] as f64 * h;
                re += s * (w * i as f64).cos();
                im -= s * (w * i as f64).sin();
            }
            re * re + im * im
        };
        let (mut best_f, mut best) = (0.0f32, -1.0f64);
        let mut f = 50.0f32;
        while f < 1000.0 {
            let p = bin(f);
            if p > best {
                best = p;
                best_f = f;
            }
            f += 0.5;
        }
        best_f
    }

    /// The audition plays into the device's stream, but bucket lengths are in
    /// source samples: emitting them unscaled played a 24 kHz recording an
    /// octave sharp and half as long on a 48 kHz device.
    #[test]
    fn a_rate_change_preserves_pitch_and_duration() {
        let src_sr = 24_000.0f32;
        let f0 = 107.0f32;
        let n = 12_000; // 0.5 s
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                let x = 2.0 * PI * f0 * i as f32 / src_sr;
                0.7 * x.sin() + 0.2 * (2.0 * x + 0.5).sin() + 0.1 * (3.0 * x).sin()
            })
            .collect();

        let res = analyze_subtrack(&samples, src_sr, f0, &[], 0, 256, 4000);
        let lens: Vec<usize> = res.bucket_periods.iter().map(|&p| p as usize).collect();

        for out_sr in [24_000.0f32, 44_100.0, 48_000.0] {
            let rec = resynthesize_exact(
                &res.amplitude,
                &res.phase,
                &lens,
                &res.dc,
                &res.nyquist,
                &[],
                &[],
                0.0,
                out_sr / src_sr,
            );
            let want_len = (n as f32 * out_sr / src_sr).round() as usize;
            assert!(
                (rec.len() as i64 - want_len as i64).abs() <= 2,
                "at {out_sr} Hz the note is {} samples, want {want_len} — duration \
                 does not survive the rate change",
                rec.len()
            );
            let got = dominant_freq(&rec, out_sr);
            let cents = 1200.0 * (got / f0).log2();
            println!("{src_sr} Hz -> {out_sr} Hz: {} samples, f0 {got:.1} Hz ({cents:+.1} cents)", rec.len());
            assert!(
                cents.abs() < 10.0,
                "at {out_sr} Hz the note plays at {got:.1} Hz, not {f0:.1} ({cents:+.1} cents)"
            );
        }
    }

    /// Rate ratio 1.0 must remain bit-for-bit the exact case — the resampling
    /// generalisation must not cost anything when there is nothing to resample.
    #[test]
    fn ratio_of_one_is_still_exact() {
        let sr = 24_000.0;
        let f = 107.0;
        let n = 12_000;
        let mut state = 0xBEEFu32;
        let samples: Vec<f32> = (0..n)
            .map(|i| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let noise = (state >> 8) as f32 / 8_388_608.0 - 1.0;
                0.7 * (2.0 * PI * f * i as f32 / sr).sin() + 0.3 * noise
            })
            .collect();
        let res = analyze_subtrack(&samples, sr, f, &[], 0, 256, 4000);
        let lens: Vec<usize> = res.bucket_periods.iter().map(|&p| p as usize).collect();
        let rec = resynthesize_exact(
            &res.amplitude,
            &res.phase,
            &lens,
            &res.dc,
            &res.nyquist,
            &[],
            &[],
            0.0,
            1.0,
        );
        let peak = samples.iter().fold(0.0f32, |m, &v| m.max(v.abs())).max(1e-9);
        let m = samples.len().min(rec.len());
        let dev = (0..m).fold(0.0f32, |a, i| a.max((samples[i] - rec[i]).abs())) / peak;
        assert!(dev < 1e-4, "ratio 1.0 is no longer exact: {dev:.3e} of peak");
    }
}
