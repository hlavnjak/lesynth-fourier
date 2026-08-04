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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner};
use crate::constants::{
    max_harmonic_for_key, NUM_HARMONICS, NUM_KEYS, NUM_OF_BUCKETS_DEFAULT, TWO_PI,
    VOICE_MIX_SCALING,
};
use crate::params::{CurveType, LeSynthParams};
use super::{ChartType, ExecutionMode, SharedParams};
use super::shared_params::BufferState;

/// Snapshot the per-bucket pitch ratios for playback — but only in Analysis
/// mode. In Synth mode playback is always flat, so this returns empty and every
/// bucket renders at the key's base period (no behaviour change for synth).
fn bucket_pitch_ratios(shared_params: &SharedParams) -> Vec<f32> {
    if shared_params.execution_mode() == ExecutionMode::Analysis {
        shared_params.bucket_pitch_ratio.lock().unwrap().clone()
    } else {
        Vec::new()
    }
}

/// Resample a per-bucket envelope row to `new_len` buckets by linear
/// interpolation over the normalized position `t = bucket / len` (matching the
/// `t = bucket / num_buckets` convention the curve fills use). Preserves the
/// row's shape at a new time-resolution without regenerating it from params, so
/// an all-zero (untouched) row stays all-zero. Empty source → zeros.
fn resample_row(src: &[f32], new_len: usize) -> Vec<f32> {
    if new_len == 0 {
        return Vec::new();
    }
    let old_len = src.len();
    if old_len == 0 {
        return vec![0.0; new_len];
    }
    if old_len == 1 {
        return vec![src[0]; new_len];
    }
    if old_len == new_len {
        return src.to_vec();
    }
    (0..new_len)
        .map(|i| {
            let pos = i as f32 / new_len as f32 * old_len as f32; // [0, old_len)
            let lo = (pos.floor() as usize).min(old_len - 1);
            let hi = (lo + 1).min(old_len - 1);
            let frac = pos - lo as f32;
            src[lo] * (1.0 - frac) + src[hi] * frac
        })
        .collect()
}

/// Rendered period length (samples, **fractional**) for `bucket`: the key's base
/// period scaled by the bucket's pitch ratio, clamped ≥ 2; empty ratio = flat.
///
/// Fractional on purpose. Rounding to whole samples is a tuning error, not a
/// rounding detail: a D5 at 22 kHz wants 37.33 samples and its neighbours 37/38
/// are 46 cents apart, so the note played sharp and its vibrato collapsed onto
/// two pitches. [`render_key_buffer`] carries a fractional phase accumulator.
fn bucket_period(base_period: f32, ratios: &[f32], bucket: usize) -> f32 {
    let r = ratios.get(bucket).copied().unwrap_or(1.0);
    (base_period / r.max(1e-3)).max(2.0)
}

/// Above this many harmonics, switch from the direct sinusoid sum to a cycle
/// table (one inverse FFT per bucket). Below it the direct sum is both cheaper
/// and exact.
const IFFT_MIN_HARMONICS: usize = 10;

/// How far the cycle table's Nyquist sits above its top harmonic. The cubic
/// kernel's accuracy depends on how much of the *top* harmonic's wavelength its
/// four taps span, so table length scales with harmonic count, not period.
const CYCLE_TABLE_OVERSAMPLE: usize = 4;

/// Cap on the cycle-table length, so a bucket can never ask for an unreasonable
/// transform.
const CYCLE_TABLE_MAX: usize = 1 << 13;

/// Inverse-FFT plans for the fast path, cached by length: one bank per
/// [`render_key_buffer`] call, shared across that key's buckets.
struct IfftBank {
    planner: RealFftPlanner<f32>,
    plans: HashMap<usize, Arc<dyn ComplexToReal<f32>>>,
}

impl IfftBank {
    fn new() -> Self {
        Self { planner: RealFftPlanner::new(), plans: HashMap::new() }
    }

    fn plan(&mut self, len: usize) -> Arc<dyn ComplexToReal<f32>> {
        let planner = &mut self.planner;
        self.plans
            .entry(len)
            .or_insert_with(|| planner.plan_fft_inverse(len))
            .clone()
    }
}

/// Build one cycle of `bucket`'s waveform into `table` with one inverse FFT.
///
/// The table holds exactly one cycle, so harmonic `n` is bin `k = n + 1`, and
/// `A·sin(2πkt/len + φ)` is the coefficient `(A/2)(sin φ − i·cos φ)`. That
/// scaling carries no factor of `len`, so an oversampled table holds the same
/// waveform as one at the exact period.
fn build_cycle_table(
    bank: &mut IfftBank,
    table: &mut Vec<f32>,
    ampl: &[Vec<f32>],
    phase: &[Vec<f32>],
    ampl_enabled: &[bool],
    phase_enabled: &[bool],
    bucket: usize,
    len: usize,
    max_h: usize,
) {
    let fft = bank.plan(len);
    let mut spectrum = fft.make_input_vec(); // length len/2 + 1, zero-filled
    let nyq = len / 2;
    for n in 0..max_h {
        if !ampl_enabled[n] {
            continue;
        }
        let amp = ampl[n][bucket];
        if amp == 0.0 {
            continue;
        }
        let k = n + 1;
        if k >= nyq {
            break; // guarded by the table length; kept for safety
        }
        let ph = if phase_enabled[n] { phase[n][bucket] } else { 0.0 };
        spectrum[k] = Complex { re: 0.5 * amp * ph.sin(), im: -0.5 * amp * ph.cos() };
    }
    *table = fft.make_output_vec();
    // Invariants hold by construction: `spectrum` is the exact input length and
    // its DC (bin 0) and Nyquist bins are zero.
    fft.process(&mut spectrum, table)
        .expect("irfft input length and DC/Nyquist invariants hold");
}

/// Cycle-table length: a power of two whose Nyquist sits
/// [`CYCLE_TABLE_OVERSAMPLE`]× above `max_h`. Sized from `max_h` alone — the
/// read step does not affect accuracy, only the kernel's width relative to the
/// top harmonic, so demanding `4 × period` too just bought a bigger transform.
fn cycle_table_len(max_h: usize) -> usize {
    (2 * CYCLE_TABLE_OVERSAMPLE * (max_h + 1))
        .max(16)
        .next_power_of_two()
        .min(CYCLE_TABLE_MAX)
}

/// Sample `table` at fractional cycle position `pos` in [0, 1), Catmull-Rom
/// with wraparound — the table is periodic, so wrapping (not clamping) is what
/// keeps consecutive cycles seamless.
fn read_cycle(table: &[f32], pos: f32) -> f32 {
    let len = table.len();
    if len == 0 {
        return 0.0;
    }
    let x = pos * len as f32;
    let i = x.floor() as usize;
    let f = x - i as f32;
    let p0 = table[(i + len - 1) % len];
    let p1 = table[i % len];
    let p2 = table[(i + 1) % len];
    let p3 = table[(i + 2) % len];
    let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
    let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c = -0.5 * p0 + 0.5 * p2;
    ((a * f + b) * f + c) * f + p1
}

/// Render a key's waveform from an amp/phase grid — the single render path for
/// Synth and Analysis modes and for both the GUI and background callers, which
/// must not diverge. Transposes, so it resamples; use
/// [`resynthesize_exact`] when the target is the source's own pitch.
///
/// A **fractional phase accumulator** drives it: `cycles` counts fundamental
/// cycles in `f64`, advancing by `1 / bucket_period` per sample, and the
/// waveform is read at the fractional position within the cycle. Emitting whole
/// samples per cycle instead forced a whole-number period and detuned every note
/// (~36 cents resynthesised, ~75 at the top of the keyboard). The bucket is
/// re-selected only at a cycle boundary, so cycles stay phase-continuous.
///
/// **Buckets are stepped, not blended.** One bucket is one period is one
/// rendered cycle, so there is no span to interpolate across. The cross-fade
/// that used to live here hid a bucket-rate modulation (148 Hz on a D5) caused
/// by the analysis not being period-synchronous; it is now, and blending would
/// only blur real period-to-period variation, which on speech is the signal.
///
/// `target_samples`: `0` = Synth timeline, one cycle per bucket. `> 0` =
/// Analysis "preserve seconds", render that many samples and pick each cycle's
/// bucket by position in time, so a note lasts the source's duration at every
/// key. `cancel` lets the background thread bail out and yield.
fn render_key_buffer(
    num_harmonics: usize,
    ampl: &[Vec<f32>],
    phase: &[Vec<f32>],
    ampl_enabled: &[bool],
    phase_enabled: &[bool],
    base_period: f32,
    max_harmonic: usize,
    ratios: &[f32],
    target_samples: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Vec<f32> {
    let num_buckets = ampl.first().map(|r| r.len()).unwrap_or(0);
    if num_buckets == 0 {
        return Vec::new();
    }
    let drive_by_time = target_samples > 0;

    let mut sound: Vec<f32> = Vec::with_capacity(if drive_by_time { target_samples } else { 0 });
    let mut ifft_bank = IfftBank::new();
    // Two tables so a cycle can be rendered *between* buckets (see the blend
    // below); `[0]` holds `bucket`, `[1]` holds `next_bucket`.
    let mut table: [Vec<f32>; 2] = [Vec::new(), Vec::new()];
    let mut table_bucket = [usize::MAX; 2];

    // Fundamental phase, in cycles. `f64` because it accumulates for the whole
    // note: at 4 kHz and 44.1 kHz an f32 mantissa would start losing sub-sample
    // resolution within a second, reintroducing the very detuning this removes.
    let mut cycles = 0.0f64;
    let mut last_cycle = usize::MAX;
    let mut bucket = 0usize;
    let mut next_bucket = 0usize;
    // Weight of `next_bucket` in this cycle's cross-fade; always 0 in Synth mode.
    let mut blend = 0.0f32;
    let mut period = base_period.max(2.0);
    let mut max_h = 0usize;
    let mut last_yield = 0usize;

    loop {
        if drive_by_time && sound.len() >= target_samples {
            break;
        }
        let cycle = cycles as usize;
        if cycle != last_cycle {
            last_cycle = cycle;
            if drive_by_time {
                // A bucket is one period, so it is one rendered cycle: pick it
                // by position in time and use it whole. No fractional position,
                // no neighbour, no blend.
                let x = (sound.len() as f64 / target_samples as f64) * num_buckets as f64;
                bucket = (x as usize).min(num_buckets - 1);
                next_bucket = bucket;
                blend = 0.0;
            } else {
                if cycle >= num_buckets {
                    break;
                }
                bucket = cycle;
                next_bucket = cycle;
                blend = 0.0;
            }
            // Interpolate the pitch too: holding the contour flat for a bucket's
            // whole span steps the pitch every few cycles instead of gliding
            // through the vibrato.
            let p0 = bucket_period(base_period, ratios, bucket);
            let p1 = bucket_period(base_period, ratios, next_bucket);
            period = p0 + (p1 - p0) * blend;
            // Anti-alias cap: harmonic k lands at k / period cycles per sample, so
            // it must stay under Nyquist (k < period / 2).
            max_h = num_harmonics
                .min(max_harmonic)
                .min((period * 0.5).floor() as usize);
        }

        if let Some(c) = cancel {
            if c.load(Ordering::Relaxed) {
                return Vec::new();
            }
            // Yield by produced samples (period count varies wildly with key).
            if sound.len() - last_yield >= 8192 {
                thread::sleep(Duration::from_millis(1));
                last_yield = sound.len();
            }
        }

        let pos = (cycles - cycles.floor()) as f32; // position within the cycle
        let sample = if max_h == 0 {
            0.0
        } else if max_h > IFFT_MIN_HARMONICS {
            // Fast path: one inverse real-FFT per bucket, then fractional readout.
            let len = cycle_table_len(max_h);
            for (slot, b) in [bucket, next_bucket].into_iter().enumerate() {
                if table_bucket[slot] == b && table[slot].len() == len {
                    continue;
                }
                // Advancing a bucket: the new `bucket` is the old `next_bucket`,
                // so take that table over rather than transforming it again -
                // the cost stays at one inverse FFT per bucket, not per cycle.
                if slot == 0 && table_bucket[1] == b && table[1].len() == len {
                    table.swap(0, 1);
                    table_bucket.swap(0, 1);
                    continue;
                }
                build_cycle_table(
                    &mut ifft_bank,
                    &mut table[slot],
                    ampl,
                    phase,
                    ampl_enabled,
                    phase_enabled,
                    b,
                    len,
                    max_h,
                );
                table_bucket[slot] = b;
            }
            let a = read_cycle(&table[0], pos);
            if blend > 0.0 {
                a + (read_cycle(&table[1], pos) - a) * blend
            } else {
                a
            }
        } else {
            // Direct sinusoid sum — cheaper than a table for few harmonics, and
            // exact (no interpolation).
            let mut acc = 0.0f32;
            for n in 0..max_h {
                if !ampl_enabled[n] {
                    continue;
                }
                let (a0, a1) = (ampl[n][bucket], ampl[n][next_bucket]);
                if a0 == 0.0 && a1 == 0.0 {
                    continue;
                }
                let ph = |b: usize| if phase_enabled[n] { phase[n][b] } else { 0.0 };
                let w = TWO_PI * (n as f32 + 1.0) * pos;
                let s0 = a0 * (w + ph(bucket)).sin();
                acc += if blend > 0.0 {
                    let s1 = a1 * (w + ph(next_bucket)).sin();
                    s0 + (s1 - s0) * blend
                } else {
                    s0
                };
            }
            acc
        };
        sound.push(sample.clamp(-1.0, 1.0));
        cycles += 1.0 / period as f64;
    }
    sound
}

/// Scale the grid down so no bucket's harmonics can sum past 1.0 — the rendered
/// sample is their worst-case in-phase sum and would clip.
///
/// The factor is **global**, from the loudest bucket. Per-bucket is equally
/// clip-safe but acts as a compressor (0.57–1.00 across 354 of D5.wav's 473
/// buckets), flattening the note's own dynamics. One function so the sync and
/// background paths cannot normalise differently, as they once did.
///
/// **Returns the divisor** (1.0 if the grid already fit), so a caller
/// reproducing the source at its own level can multiply it back in.
#[must_use]
pub fn normalize_grid_per_bucket(grid: &mut [Vec<f32>]) -> f32 {
    let buckets = grid.first().map(|r| r.len()).unwrap_or(0);
    let mut worst = 0.0f32;
    for b in 0..buckets {
        let sum: f32 = grid.iter().map(|row| row[b]).sum();
        if sum > worst {
            worst = sum;
        }
    }
    if worst <= 1.0 {
        return 1.0;
    }
    for row in grid.iter_mut() {
        for v in row.iter_mut() {
            *v /= worst;
        }
    }
    worst
}

/// Reproduce the analysed source from its grid — the exact inverse of
/// [`analyze_subtrack`](super::analyze_subtrack).
///
/// Bucket `b` is one period of `bucket_lengths[b]` samples whose harmonics are
/// bins `1..N/2` of exactly those samples, so one inverse FFT per bucket returns
/// them and the concatenation returns the subtrack. No interpolation, no
/// cross-fade, no cycle table, no renormalisation: all of those exist to paper
/// over resampling to a different period, and there is none to do here. Kept
/// separate from [`render_key_buffer`], which transposes and therefore must.
///
/// * `dc` / `nyq` – the non-harmonic bins; empty slices omit them (close, not
///   exact).
/// * `display_gain` – divided back out, so the output is at the source's own
///   level; `0` keeps the grid's display-normalised level.
/// * `rate_ratio` – `output_rate / analysis_rate`; `1.0` is the exact case.
///   Applied by [`resample_stream`] to the *finished* reconstruction — see
///   there for why it cannot be folded into the per-bucket transform.
pub fn resynthesize_exact(
    amplitude: &[Vec<f32>],
    phase: &[Vec<f32>],
    bucket_lengths: &[usize],
    dc: &[f32],
    nyq: &[f32],
    display_gain: f32,
    rate_ratio: f32,
) -> Vec<f32> {
    let num_harmonics = amplitude.len();
    if num_harmonics == 0 || bucket_lengths.is_empty() {
        return Vec::new();
    }
    let scale = if display_gain > 0.0 { 1.0 / display_gain } else { 1.0 };

    let mut planner = RealFftPlanner::<f32>::new();
    let mut out: Vec<f32> = Vec::with_capacity(bucket_lengths.iter().sum());
    for (b, &n) in bucket_lengths.iter().enumerate() {
        if n < 2 {
            continue;
        }
        // Inverted at its own length: `n` points in, the same `n` samples out.
        let fft = planner.plan_fft_inverse(n);
        let mut spectrum = fft.make_input_vec(); // n/2 + 1 bins, zeroed
        spectrum[0] = Complex { re: dc.get(b).copied().unwrap_or(0.0), im: 0.0 };
        if n % 2 == 0 {
            spectrum[n / 2] = Complex { re: nyq.get(b).copied().unwrap_or(0.0), im: 0.0 };
        }
        // `A·sin(2πkt + φ)` ↔ `(A/2)(sin φ − i·cos φ)`, the inverse of what
        // `analyze_subtrack` stored.
        let top = ((n - 1) / 2).min(num_harmonics);
        for k in 1..=top {
            let a = amplitude[k - 1][b];
            if a == 0.0 {
                continue;
            }
            let ph = phase[k - 1][b];
            spectrum[k] = Complex { re: 0.5 * a * ph.sin(), im: -0.5 * a * ph.cos() };
        }
        let mut block = fft.make_output_vec();
        if fft.process(&mut spectrum, &mut block).is_err() {
            continue;
        }
        out.extend(block.iter().map(|v| v * scale));
    }
    resample_stream(&out, rate_ratio as f64)
}

/// Half-width of the resampling kernel, in taps. 32 a side measures better than
/// −80 dB on tones from 110 Hz to 7 kHz at every rate pair in use
/// (`resample_stream_is_transparent`).
const RESAMPLE_TAPS: usize = 32;

/// Kernel table resolution, in points per tap, read with linear interpolation so
/// the hot loop costs no `sin` calls. 512 and 2048 measure identically, so this
/// is not what sets the floor — the kernel's length and f32 are.
const RESAMPLE_KERNEL_STEPS: usize = 512;

/// Modified Bessel function of the first kind, order 0 — the Kaiser window's
/// shape term. The series converges quickly for β ≤ 12.
fn bessel_i0(x: f64) -> f64 {
    let mut term = 1.0;
    let mut sum = 1.0;
    let half = x * 0.5;
    for k in 1..40 {
        term *= (half / k as f64) * (half / k as f64);
        sum += term;
        if term < 1e-16 * sum {
            break;
        }
    }
    sum
}

/// Resample a finished signal by `ratio = output_rate / input_rate` with a
/// Kaiser-windowed sinc. Output length `round(len × ratio)`, so the note keeps
/// its duration; downsampling drops the cutoff to `ratio` (widening the support
/// to match) so nothing folds back over Nyquist.
///
/// **Applied to the whole reconstruction, never per bucket.** Rendering a
/// bucket's spectrum into `m` points instead of `n` looks like a free
/// band-limited resample, and is what this replaced. It assumes the period
/// repeats: a quasi-periodic bucket's ends do not join up, so any length but
/// its own evaluates between the samples where that step lives and rings
/// against it — **once per period**, −27.7 dB peak / −50 dB rms on a steady
/// 12-harmonic tone at 22.05 → 44.1 kHz. Rounding each bucket's output length
/// separately also jittered the time base by up to half a sample per period.
pub fn resample_stream(input: &[f32], ratio: f64) -> Vec<f32> {
    if input.is_empty() || !ratio.is_finite() || ratio <= 0.0 {
        return input.to_vec();
    }
    // Rates within a part in a million of each other: nothing to do, and
    // resampling anyway would only add kernel error to an exact signal.
    if (ratio - 1.0).abs() < 1e-6 {
        return input.to_vec();
    }

    let cutoff = ratio.min(1.0);
    // Kernel sampled on [0, RESAMPLE_TAPS] in tap units; one extra entry so the
    // interpolation below can always read `k + 1`.
    let beta = 10.0;
    let norm = bessel_i0(beta);
    let table: Vec<f64> = (0..=RESAMPLE_TAPS * RESAMPLE_KERNEL_STEPS + 1)
        .map(|i| {
            let x = i as f64 / RESAMPLE_KERNEL_STEPS as f64; // taps from centre
            if x >= RESAMPLE_TAPS as f64 {
                return 0.0;
            }
            let t = x / RESAMPLE_TAPS as f64;
            let window = bessel_i0(beta * (1.0 - t * t).max(0.0).sqrt()) / norm;
            let arg = std::f64::consts::PI * x;
            let sinc = if x == 0.0 { 1.0 } else { arg.sin() / arg };
            sinc * window
        })
        .collect();

    let len = input.len();
    let out_len = ((len as f64) * ratio).round() as usize;
    // Support in *input* samples: the kernel is stretched by 1/cutoff when the
    // cutoff drops, so a downsample still averages over the right span.
    let half = RESAMPLE_TAPS as f64 / cutoff;
    let mut out = Vec::with_capacity(out_len);
    for j in 0..out_len {
        let pos = j as f64 / ratio; // where this output sample sits in the input
        let first = (pos - half).ceil() as i64;
        let last = (pos + half).floor() as i64;
        let mut acc = 0.0f64;
        for i in first..=last {
            if i < 0 || i as usize >= len {
                continue;
            }
            let x = (pos - i as f64).abs() * cutoff * RESAMPLE_KERNEL_STEPS as f64;
            let k = x as usize;
            if k + 1 >= table.len() {
                continue;
            }
            let f = x - k as f64;
            let w = table[k] + (table[k + 1] - table[k]) * f;
            acc += input[i as usize] as f64 * w;
        }
        out.push((acc * cutoff) as f32);
    }
    out
}

/// Render a grid the way playback does, with no live engine: normalisation then
/// [`render_key_buffer`], every harmonic enabled. The host bridge
/// (`lesynth_fourier_resynthesize`) goes through here, so a host-side test
/// measures the real playback path rather than a copy of it.
///
/// * `base_period`    – **fractional** period in samples; pass
///   `sample_rate / base_freq` unrounded.
/// * `max_harmonic`   – anti-alias cap; `0` = only the `period / 2` limit.
/// * `target_samples` – `0` = one cycle per bucket, `> 0` = "preserve seconds".
/// * `display_gain`   – non-zero divides it back out, giving the source's own
///   absolute level; `0` keeps the grid's display-normalised level.
pub fn resynthesize_grid(
    amplitude: &[Vec<f32>],
    phase: &[Vec<f32>],
    pitch_ratio: &[f32],
    base_period: f32,
    max_harmonic: usize,
    target_samples: usize,
    display_gain: f32,
) -> Vec<f32> {
    let num_harmonics = amplitude.len();
    if num_harmonics == 0 {
        return Vec::new();
    }
    let mut normalized = amplitude.to_vec();
    let divisor = normalize_grid_per_bucket(&mut normalized);
    let enabled = vec![true; num_harmonics];
    let mut sound = render_key_buffer(
        num_harmonics,
        &normalized,
        phase,
        &enabled,
        &enabled,
        base_period.max(2.0),
        if max_harmonic == 0 { num_harmonics } else { max_harmonic },
        pitch_ratio,
        target_samples,
        None,
    );
    if let Some(scale) = source_level_scale(display_gain, divisor) {
        for v in sound.iter_mut() {
            *v = (*v * scale).clamp(-1.0, 1.0);
        }
    }
    sound
}

/// Factor that returns a render of the stored grid to the source's own absolute
/// level, or `None` when that level isn't known (`display_gain <= 0`: a
/// hand-drawn grid, or a `.lsft` from before the gain was recorded).
///
/// Two deliberate level changes sit between the analysed amplitudes and the
/// audio and both are undone here: `display_gain` (chart legibility, ×15.4 on
/// the quiet D5 sample) and `divisor` (clip safety). Their product is not 1.0 —
/// together they played the D5 resynthesis 18.9 dB hot, which reads as a harsh,
/// noisy version of the source rather than a faithful one.
pub fn source_level_scale(display_gain: f32, divisor: f32) -> Option<f32> {
    if display_gain <= 0.0 || divisor <= 0.0 {
        return None;
    }
    // `is_finite` also rejects the NaN inputs the comparisons above let through.
    let scale = divisor / display_gain;
    scale.is_finite().then_some(scale)
}

/// Playback length in samples for `key`: `0` in Synth mode (caller renders one
/// cycle per bucket), or the source's wall-clock duration at the playback
/// sample rate in Analysis mode ("preserve seconds").
fn target_samples_for(shared_params: &SharedParams) -> usize {
    if shared_params.execution_mode() != ExecutionMode::Analysis {
        return 0;
    }
    let duration = *shared_params.analysis_duration_secs.lock().unwrap();
    let sr = *shared_params.sample_rate.lock().unwrap();
    if duration > 0.0 && sr > 0.0 {
        (duration * sr).round() as usize
    } else {
        0
    }
}

// Deliberately not `Clone`: the engine is always used behind an `Arc` (the
// registry holds `Weak`s to it), and a value-copy would duplicate the analysis
// mailbox and editor registration while the background compute thread kept
// serving only the original.
pub struct SynthComputeEngine {
    synth_params: Arc<LeSynthParams>,
    pub shared_params: Arc<SharedParams>,
    /// Analysis job the host pushed *for this instance*, waiting to be claimed
    /// by this instance's editor. A single slot rather than a queue: the host
    /// pushes at most one subtrack per instance, and a second push supersedes an
    /// unclaimed first.
    ///
    /// Per-instance because several editors can be open at once — a shared inbox
    /// lets whichever editor happens to paint first swallow another instance's
    /// job, leaving that instance with empty charts.
    pending_analysis: Mutex<Option<crate::AnalysisJob>>,
    /// This instance's editor egui context, registered while its editor is open
    /// so off-thread events (host pushes, MIDI) can wake *this* idle editor.
    editor_ctx: Mutex<Option<nih_plug_egui::egui::Context>>,
}

impl SynthComputeEngine {
    pub fn new(synth_params_p: Arc<LeSynthParams>) -> Self {
        let buckets = NUM_OF_BUCKETS_DEFAULT;
        let engine = Self {
            synth_params: synth_params_p,
            shared_params: Arc::new(SharedParams::new(NUM_HARMONICS, buckets)),
            pending_analysis: Mutex::new(None),
            editor_ctx: Mutex::new(None),
        };

        // Start background computation thread
        engine.start_async_computation_thread();

        engine
    }

    /// Register this instance's editor context (replacing any previous), so
    /// [`wake_editor`](Self::wake_editor) can repaint it while it sits idle.
    pub fn set_editor_ctx(&self, ctx: nih_plug_egui::egui::Context) {
        if let Ok(mut g) = self.editor_ctx.lock() {
            *g = Some(ctx);
        }
    }

    /// Repaint this instance's editor to pick up off-thread state. No-op when
    /// this instance has no editor open.
    pub fn wake_editor(&self) {
        if let Ok(g) = self.editor_ctx.lock() {
            if let Some(ctx) = g.as_ref() {
                ctx.request_repaint();
            }
        }
    }

    /// Hand this instance an analysis job, replacing any still unclaimed.
    pub fn push_analysis_job(&self, job: crate::AnalysisJob) {
        if let Ok(mut g) = self.pending_analysis.lock() {
            *g = Some(job);
        }
    }

    /// Take this instance's pending analysis job, if any (called by its editor).
    pub fn take_analysis_job(&self) -> Option<crate::AnalysisJob> {
        self.pending_analysis.lock().ok().and_then(|mut g| g.take())
    }

    /// Whether harmonic `n`'s hand-drawn Synth-mode curve is allowed to
    /// overwrite its live grid row for `chart_type`.
    ///
    /// In plain Synth mode (no analysed audio loaded) the drawn curve always
    /// owns the row — the per-harmonic "cust" override is implicitly on. Once an
    /// analysis is loaded, the row belongs to the data extracted from the source
    /// sound, and a drawn curve must replace it only when the user has ticked
    /// "cust" for that harmonic. Without this gate, drawing in Synth mode would
    /// silently clobber a loaded sound's analysed row even though "cust" was
    /// never selected.
    fn curve_overrides_live(&self, n: usize, chart_type: ChartType) -> bool {
        let has_analysis = *self.shared_params.analysis_duration_secs.lock().unwrap() > 0.0;
        if !has_analysis {
            return true;
        }
        let flags = match chart_type {
            ChartType::Amp => self.shared_params.harmonic_ampl_custom.lock().unwrap(),
            ChartType::Phase => self.shared_params.harmonic_phase_custom.lock().unwrap(),
        };
        flags.get(n).copied().unwrap_or(false)
    }

    pub fn fill_constant_curve(&self, n: usize, value: f32, chart_type: ChartType) {
        // Don't override an analysed row unless "cust" is selected for it.
        if !self.curve_overrides_live(n, chart_type) {
            return;
        }
        let wobble_amp = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].wobble_amp_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].wobble_amp_phase.value(),
        };

        let needs_update = {
            let data = match chart_type {
                ChartType::Amp => self.shared_params.amplitude_data.lock().unwrap(),
                ChartType::Phase => self.shared_params.phase_data.lock().unwrap(),
            };
            data[n][0] != value || wobble_amp > 0.0
        };
        if needs_update {
            self.fill_constant_curve_forced(n, value, chart_type);
        }
    }

    /// Like [`Self::fill_constant_curve`] but always rewrites the whole row,
    /// skipping the "bucket 0 already matches" early-out. Needed when overwriting
    /// an analysed row (where only bucket 0 might coincide with `value`).
    fn fill_constant_curve_forced(&self, n: usize, value: f32, chart_type: ChartType) {
        self.write_constant_row(n, value, chart_type);
        self.set_normalization_needed(true);
        self.shared_params.mark_all_buffers_dirty();
        // Update assembled chart with key 24 for immediate preview
        self.update_assembled_chart_with_key24();
    }

    /// Write harmonic `n`'s constant-curve amplitude/phase row, without the
    /// normalize/dirty/chart side effects. Used both by the public fill (which
    /// adds those) and by bulk operations that batch the side effects once.
    fn write_constant_row(&self, n: usize, value: f32, chart_type: ChartType) {
        let wobble_amp = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].wobble_amp_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].wobble_amp_phase.value(),
        };
        let wobble_freq = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].wobble_freq_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].wobble_freq_phase.value(),
        };

        let mut data = match chart_type {
            ChartType::Amp => self.shared_params.amplitude_data.lock().unwrap(),
            ChartType::Phase => self.shared_params.phase_data.lock().unwrap(),
        };

        for bucket in 0..data[n].len() {
            let wobble = if wobble_amp > 0.0 {
                wobble_amp * (wobble_freq * bucket as f32 * 0.01).sin()
            } else {
                0.0
            };
            let final_value = match chart_type {
                ChartType::Amp => (value + wobble).clamp(0.0, 1.0),
                ChartType::Phase => value + wobble,
            };
            data[n][bucket] = final_value;
        }
    }

    pub fn fill_sin_curve(&self, n: usize, chart_type: ChartType) {
        // Don't override an analysed row unless "cust" is selected for it.
        if !self.curve_overrides_live(n, chart_type) {
            return;
        }
        let a = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].sine_curve_amp_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].sine_curve_amp_phase.value(),
        };
        let b = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].sine_curve_freq_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].sine_curve_freq_phase.value(),
        };
        let amp_off = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].curve_offset_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].curve_offset_phase.value(),
        };
        let wobble_amp = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].wobble_amp_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].wobble_amp_phase.value(),
        };
        let wobble_freq = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].wobble_freq_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].wobble_freq_phase.value(),
        };

        let mut data = match chart_type {
            ChartType::Amp => self.shared_params.amplitude_data.lock().unwrap(),
            ChartType::Phase => self.shared_params.phase_data.lock().unwrap(),
        };
        for bucket in 0..data[n].len() {
            let raw = a * (b as f32 * bucket as f32).sin();
            let wobble = if wobble_amp > 0.0 {
                wobble_amp * (wobble_freq * bucket as f32 * 0.01).sin()
            } else {
                0.0
            };
            let val = match chart_type {
                ChartType::Amp => (raw + amp_off + wobble).clamp(0.0, 1.0),
                ChartType::Phase => raw + amp_off + wobble,
            };
            data[n][bucket] = val;
        }
        self.set_normalization_needed(true);
        // Mark all buffers as dirty since harmonic parameters changed
        drop(data); // Release the lock before calling mark_all_buffers_dirty
        self.shared_params.mark_all_buffers_dirty();
        // Update assembled chart with key 24 for immediate preview
        self.update_assembled_chart_with_key24();
    }

    /// Fill harmonic n's amplitude or phase data using a Fourier series of sub-harmonics.
    /// V(bucket) = offset + Σ_{k=1}^{N} amp_k * sin(2π * k * t + phase_k)
    /// The amplitude chart clamps the result to [0, 1]; the phase chart leaves it unclamped.
    /// Each chart uses its own independent set of sub-harmonic parameters.
    pub fn fill_nested_fourier_curve(&self, n: usize, chart_type: ChartType) {
        // Don't override an analysed row unless "cust" is selected for it. The
        // "cust" toggle enables the flag *before* calling this (via
        // refill_harmonic_curve), so the override path still writes through.
        if !self.curve_overrides_live(n, chart_type) {
            return;
        }
        self.write_nested_fourier_row(n, chart_type);
        self.set_normalization_needed(true);
        self.shared_params.mark_all_buffers_dirty();
        self.update_assembled_chart_with_key24();
    }

    /// Write harmonic `n`'s nested-Fourier amplitude/phase row, without the
    /// normalize/dirty/chart side effects (see [`Self::write_constant_row`]).
    fn write_nested_fourier_row(&self, n: usize, chart_type: ChartType) {
        let harmonic = &self.synth_params.harmonics[n];
        let offset = match chart_type {
            ChartType::Amp => harmonic.curve_offset_amp.value() as f64,
            ChartType::Phase => harmonic.curve_offset_phase.value() as f64,
        };
        let (sub_amps, sub_phases) = {
            let state = harmonic.nested_fourier.read().unwrap();
            let series = state.series(chart_type);
            (series.amps, series.phases)
        };

        let mut data = match chart_type {
            ChartType::Amp => self.shared_params.amplitude_data.lock().unwrap(),
            ChartType::Phase => self.shared_params.phase_data.lock().unwrap(),
        };
        let num_buckets = data[n].len();

        for bucket in 0..num_buckets {
            let t = bucket as f64 / num_buckets as f64;
            let mut value = offset;
            for (k, (&amp, &phase)) in sub_amps.iter().zip(sub_phases.iter()).enumerate() {
                value += amp as f64
                    * (2.0 * std::f64::consts::PI * (k + 1) as f64 * t + phase as f64).sin();
            }
            data[n][bucket] = match chart_type {
                ChartType::Amp => value.clamp(0.0, 1.0) as f32,
                ChartType::Phase => value as f32,
            };
        }
    }

    /// Refill harmonic `n`'s amplitude or phase row from its current Synth-mode
    /// curve type (Constant or Nested Fourier), applying the normalize/dirty/
    /// chart side effects. Used by the per-harmonic "custom" override.
    fn refill_harmonic_curve(&self, n: usize, chart_type: ChartType) {
        match self.curve_type_of(n, chart_type) {
            CurveType::Constant => {
                self.fill_constant_curve_forced(n, self.curve_offset_of(n, chart_type), chart_type);
            }
            CurveType::NestedFourier => self.fill_nested_fourier_curve(n, chart_type),
        }
    }

    fn curve_type_of(&self, n: usize, chart_type: ChartType) -> CurveType {
        match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].curve_type_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].curve_type_phase.value(),
        }
    }

    fn curve_offset_of(&self, n: usize, chart_type: ChartType) -> f32 {
        match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].curve_offset_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].curve_offset_phase.value(),
        }
    }

    /// Current number of buckets (time-resolution of the synthesised envelope).
    pub fn num_buckets(&self) -> usize {
        self.shared_params
            .amplitude_data
            .lock()
            .unwrap()
            .first()
            .map(|r| r.len())
            .unwrap_or(0)
    }

    /// Resize the per-bucket synthesis grid to `new_buckets`, resampling every
    /// harmonic's *existing* amp/phase envelope onto the new grid. This is the
    /// time-resolution of the synthesised envelope; only meaningful in Synth
    /// mode. Analysis mode derives its bucket count from the analysed audio, so
    /// callers must not invoke this while analysed data is loaded. No-op when the
    /// grid is already that size.
    ///
    /// The rows are resampled (not regenerated from each harmonic's params) on
    /// purpose: harmonics the user never shaped still carry non-zero param
    /// defaults, so regenerating would resurrect them as an audible buzz on every
    /// resize. Resampling preserves exactly what is currently on the grid — an
    /// all-zero (untouched) row stays silent, and drawn curves keep their shape.
    pub fn set_num_buckets(&self, new_buckets: usize) {
        let new_buckets = new_buckets.max(1);
        {
            let mut amp = self.shared_params.amplitude_data.lock().unwrap();
            if amp.first().map(|r| r.len()) == Some(new_buckets) {
                return;
            }
            let mut phase = self.shared_params.phase_data.lock().unwrap();
            let mut norm = self.shared_params.amplitude_data_normalized.lock().unwrap();
            for row in amp.iter_mut() {
                *row = resample_row(row, new_buckets);
            }
            for row in phase.iter_mut() {
                *row = resample_row(row, new_buckets);
            }
            *norm = vec![vec![0.0; new_buckets]; amp.len()];
        }
        {
            // Synth mode is flat (no vibrato); keep the ratio grid sized to match.
            let mut ratio = self.shared_params.bucket_pitch_ratio.lock().unwrap();
            ratio.resize(new_buckets, 1.0);
        }

        self.set_normalization_needed(true);
        self.shared_params.mark_all_buffers_dirty();
        self.update_assembled_chart_with_key24();
    }

    pub fn normalize_amplitude_data(&self) {
        let ampl_data = self.shared_params.amplitude_data.lock().unwrap();
        let mut ampl_data_normalized = self.shared_params.amplitude_data_normalized.lock().unwrap();
        // Copy then condition, using the one shared rule (see
        // `normalize_grid_per_bucket`) so this path and the background thread's
        // produce the same audio for the same grid.
        *ampl_data_normalized = ampl_data.clone();
        let divisor = normalize_grid_per_bucket(&mut ampl_data_normalized);
        *self.shared_params.grid_norm_divisor.lock().unwrap() = divisor;
    }

    pub fn assemble_buffer_for_key(&self, key: usize) -> Vec<f32> {
        let base_period = self.shared_params.piano_periods.lock().unwrap()[key];
        // Cap the harmonics this key can carry without aliasing.
        self.assemble_buffer_with_period(base_period, max_harmonic_for_key(key))
    }

    /// Render the grid at the source's own pitch **and its own level** — the
    /// "Original Pitch And Gain" audition, and the only form comparable against
    /// the source file by ear.
    ///
    /// Unlike a key, this buffer is level-restored: both normalisations
    /// ([`source_level_scale`]) and the mixdown's per-voice headroom
    /// ([`VOICE_MIX_SCALING`]) are divided back out, so what the plugin emits is
    /// the source's own waveform amplitude. Without that it played 19 dB hot,
    /// which turns masked bow noise into an obvious buzz.
    ///
    /// Empty outside Analysis mode, or before the fundamental/rate are known.
    pub fn assemble_buffer_at_original_pitch(&self) -> Vec<f32> {
        if self.shared_params.execution_mode() != ExecutionMode::Analysis {
            return Vec::new();
        }
        let base_freq = *self.shared_params.analysis_base_freq.lock().unwrap();
        let sample_rate = *self.shared_params.sample_rate.lock().unwrap();
        if base_freq <= 0.0 || sample_rate <= 0.0 {
            return Vec::new();
        }
        // Prefer the exact inverse: the button means "play me the source", and
        // at the source's own pitch there is nothing to resample, so there is no
        // reason to accept the transposing renderer's error.
        {
            let lengths = self.shared_params.analysis_bucket_lengths.lock().unwrap();
            // Lengths are in the *source's* samples, so they need the analysis
            // rate to describe the right pitch and duration here. Without it,
            // fall through to the renderer, which works from `base_freq` in Hz
            // and is rate-correct by construction.
            let analysis_rate = *self.shared_params.analysis_sample_rate.lock().unwrap();
            if !lengths.is_empty() && analysis_rate > 0.0 && sample_rate > 0.0 {
                let amp = self.shared_params.amplitude_data.lock().unwrap();
                let phase = self.shared_params.phase_data.lock().unwrap();
                let dc = self.shared_params.analysis_dc.lock().unwrap();
                let nyq = self.shared_params.analysis_nyquist.lock().unwrap();
                let display_gain = *self.shared_params.analysis_display_gain.lock().unwrap();
                let mut sound = resynthesize_exact(
                    &amp,
                    &phase,
                    &lengths,
                    &dc,
                    &nyq,
                    display_gain,
                    sample_rate / analysis_rate,
                );
                // Undo the mixer's headroom so the audition lands at the file's
                // own level. **Not clamped**: the mixer reapplies
                // VOICE_MIX_SCALING and clamps the final mix, so peaks above 1.0
                // here are exactly the ones that come back correct there.
                // Clamping flat-topped every source peaking above 0.8 —
                // my_voice.m4a peaks at 0.98, so its loud half was hard-clipped
                // at 0.65 of true, on top of a -128 dB reconstruction.
                for v in sound.iter_mut() {
                    *v /= VOICE_MIX_SCALING;
                }
                return sound;
            }
        }

        let base_period = sample_rate / base_freq;
        // Only the Nyquist limit applies here — the anti-alias cap is a
        // per-*key* quantity and there is no key involved. `render_key_buffer`
        // still clamps to `period / 2`, which is exactly that limit.
        let mut sound = self.assemble_buffer_with_period(base_period.max(2.0), NUM_HARMONICS);
        // Read the divisor *after* rendering: `assemble_buffer_with_period`
        // re-normalises first when the grid changed, and it is that render's
        // divisor we have to undo.
        let display_gain = *self.shared_params.analysis_display_gain.lock().unwrap();
        let divisor = *self.shared_params.grid_norm_divisor.lock().unwrap();
        if let Some(scale) = source_level_scale(display_gain, divisor) {
            let scale = scale / VOICE_MIX_SCALING;
            // Unclamped for the same reason as above: the mixer scales this back
            // down and clamps there.
            for v in sound.iter_mut() {
                *v *= scale;
            }
        }
        sound
    }

    /// Shared body of the synchronous render paths: normalise if needed, then
    /// render the live grid at `base_period` with `max_harmonic` as the
    /// anti-alias cap.
    fn assemble_buffer_with_period(&self, base_period: f32, max_harmonic: usize) -> Vec<f32> {
        let start_time = std::time::Instant::now();

        if *self.shared_params.normalization_needed.lock().unwrap() {
            self.normalize_amplitude_data();
            *self.shared_params.normalization_needed.lock().unwrap() = false;
        }

        let num_harmonics = self.shared_params.amplitude_data.lock().unwrap().len();
        let ampl_data_normalized = self.shared_params.amplitude_data_normalized.lock().unwrap();
        let phase_data = self.shared_params.phase_data.lock().unwrap();
        // Per-bucket vibrato ratios apply only in Analysis mode; flat otherwise.
        let pitch_ratio = bucket_pitch_ratios(&self.shared_params);
        // Hoist the per-harmonic enable flags out of the hot loops — locking
        // them per sample (as before) cost a mutex round-trip for every output
        // sample, making large analysis buffers crawl.
        let harmonic_ampl_enabled = self.shared_params.harmonic_ampl_enabled.lock().unwrap();
        let harmonic_phase_enabled = self.shared_params.harmonic_phase_enabled.lock().unwrap();

        // Synth mode: one period per bucket. Analysis mode: the source duration.
        let target_samples = target_samples_for(&self.shared_params);

        let sound = render_key_buffer(
            num_harmonics,
            &ampl_data_normalized,
            &phase_data,
            &harmonic_ampl_enabled,
            &harmonic_phase_enabled,
            base_period,
            max_harmonic,
            &pitch_ratio,
            target_samples,
            None,
        );

        let elapsed = start_time.elapsed();
        log::trace!("assemble_buffer_with_period(base_period={:.3}) took: {:?} (total_samples={}, max_harmonic={}/{})",
                 base_period, elapsed, sound.len(), max_harmonic, num_harmonics);

        sound
    }

    // Quick mixdown of active voices for plotting
    pub fn update_plotted_mix(&self) {
        let voices = self.shared_params.voices.lock().unwrap();
        // choose a reasonable window length to visualize
        let target_len = voices
            .iter()
            .filter_map(|v| v.as_ref().map(|vv| vv.buffer.len()))
            .max()
            .unwrap_or(0);
        
        if target_len == 0 {
            // No active voices - generate a sample waveform using middle C (key 48) for visualization
            drop(voices); // Release the lock before calling get_buffer_for_key
            let sample_buffer = self.get_buffer_for_key(48); // Middle C
            if !sample_buffer.is_empty() {
                // Clamp the sample buffer for display
                let clamped_buffer: Vec<f32> = sample_buffer.iter().map(|&s| s.clamp(-1.0, 1.0)).collect();
                
                *self.shared_params.assembled_sound_plotted.lock().unwrap() = clamped_buffer;
            } else {
                self.shared_params
                    .assembled_sound_plotted
                    .lock()
                    .unwrap()
                    .clear();
            }
            return;
        }
        let mut mix = vec![0.0f32; target_len];
        for v in voices.iter().filter_map(|o| o.as_ref()) {
            // add unclipped (plotting only); clamp for display later
            for i in 0..v.buffer.len() {
                mix[i] += v.buffer[i];
            }
        }
        for s in &mut mix {
            *s = s.clamp(-1.0, 1.0);
        }
        *self
            .shared_params
            .assembled_sound_plotted
            .lock()
            .unwrap() = mix;
    }

    pub fn set_normalization_needed(&self, normalization_needed: bool) {
        *self
            .shared_params
            .normalization_needed
            .lock()
            .unwrap() = normalization_needed;
    }
    
    /// Update the assembled chart with key 24's waveform for immediate preview
    pub fn update_assembled_chart_with_key24(&self) {
        // Force synchronous recomputation instead of using cached buffer
        let sample_buffer = self.assemble_buffer_for_key(24); // Key 24 (one octave up from key 0)
        if !sample_buffer.is_empty() {
            // Clamp the sample buffer for display
            let clamped_buffer: Vec<f32> = sample_buffer.iter().map(|&s| s.clamp(-1.0, 1.0)).collect();
            
            *self.shared_params.assembled_sound_plotted.lock().unwrap() = clamped_buffer;
            
            // Signal that the chart view should be reset to default range (0-2000)
            self.shared_params.should_reset_chart_view.store(true, std::sync::atomic::Ordering::Relaxed);
            
            log::debug!("Updated assembled chart with key 24 preview (samples: {})", sample_buffer.len());
        } else {
            // If no buffer available, clear the display
            self.shared_params
                .assembled_sound_plotted
                .lock()
                .unwrap()
                .clear();
            log::debug!("Cleared assembled chart (no key 24 buffer available yet)");
        }
    }
    
    /// Start the background thread that continuously computes dirty buffers
    fn start_async_computation_thread(&self) {
        let shared_params = self.shared_params.clone();
        
        thread::spawn(move || {
            loop {
                // Check if we need to cancel and reset
                if shared_params.computation_cancel.load(Ordering::Relaxed) {
                    shared_params.computation_cancel.store(false, Ordering::Relaxed);
                    thread::sleep(Duration::from_millis(10));
                    continue;
                }
                
                // Find the next dirty buffer to compute, prioritizing key 24 first, then lower keys
                let mut next_key = None;
                {
                    let buffer_states = shared_params.buffer_states.lock().unwrap();
                    
                    // First priority: key 24 (for preview)
                    if buffer_states[24] == BufferState::Dirty {
                        next_key = Some(24);
                    } else {
                        // Second priority: lower keys (which take longer)
                        for key in 0..NUM_KEYS {
                            if key != 24 && buffer_states[key] == BufferState::Dirty {
                                next_key = Some(key);
                                break;
                            }
                        }
                    }
                }
                
                if let Some(key) = next_key {
                    // Mark as computing
                    {
                        let mut buffer_states = shared_params.buffer_states.lock().unwrap();
                        if buffer_states[key] == BufferState::Dirty {
                            buffer_states[key] = BufferState::Computing;
                        } else {
                            // State changed while we were acquiring lock, continue
                            continue;
                        }
                    }
                    
                    log::trace!("Starting async computation for key {}", key);
                    
                    // Compute the buffer (this is the expensive operation)
                    let computed_buffer = Self::compute_buffer_for_key_static(&shared_params, key);
                    
                    // Check if we were cancelled during computation
                    if !shared_params.computation_cancel.load(Ordering::Relaxed) {
                        // Store the computed buffer and mark as clean
                        {
                            let mut key_buffers = shared_params.key_buffers.lock().unwrap();
                            let mut buffer_states = shared_params.buffer_states.lock().unwrap();
                            
                            key_buffers[key] = Some(computed_buffer);
                            buffer_states[key] = BufferState::Clean;
                        }
                        log::trace!("Completed async computation for key {}", key);
                    } else {
                        // Computation was cancelled, mark as dirty again
                        let mut buffer_states = shared_params.buffer_states.lock().unwrap();
                        buffer_states[key] = BufferState::Dirty;
                        log::trace!("Cancelled async computation for key {}", key);
                    }
                } else {
                    // No dirty buffers, sleep a bit
                    thread::sleep(Duration::from_millis(50));
                }
            }
        });
    }
    
    /// Static version of assemble_buffer_for_key for use in background thread
    fn compute_buffer_for_key_static(shared_params: &Arc<SharedParams>, key: usize) -> Vec<f32> {
        let start_time = std::time::Instant::now();
        
        if *shared_params.normalization_needed.lock().unwrap() {
            Self::normalize_amplitude_data_static(shared_params);
            *shared_params.normalization_needed.lock().unwrap() = false;
        }
        
        // Calculate maximum usable harmonic for this key to prevent aliasing
        let max_harmonic = max_harmonic_for_key(key);

        // Copy all required data once and release locks immediately to avoid blocking GUI
        let (num_harmonics, ampl_data_copy, phase_data_copy, harmonic_ampl_enabled_copy, harmonic_phase_enabled_copy, base_period, pitch_ratio, target_samples) = {
            let ampl_data_normalized = shared_params.amplitude_data_normalized.lock().unwrap();
            let phase_data = shared_params.phase_data.lock().unwrap();
            let piano_periods = shared_params.piano_periods.lock().unwrap();
            let harmonic_ampl_enabled = shared_params.harmonic_ampl_enabled.lock().unwrap();
            let harmonic_phase_enabled = shared_params.harmonic_phase_enabled.lock().unwrap();

            let num_harmonics = ampl_data_normalized.len();
            let base_period = piano_periods[key];

            // Deep copy the data we need
            let ampl_data_copy: Vec<Vec<f32>> = ampl_data_normalized.clone();
            let phase_data_copy: Vec<Vec<f32>> = phase_data.clone();
            let harmonic_ampl_enabled_copy: Vec<bool> = harmonic_ampl_enabled.clone();
            let harmonic_phase_enabled_copy: Vec<bool> = harmonic_phase_enabled.clone();
            // Per-bucket vibrato ratios (Analysis mode only; empty → flat).
            let pitch_ratio = bucket_pitch_ratios(shared_params);
            // Synth mode: one period per bucket. Analysis mode: source duration.
            let target_samples = target_samples_for(shared_params);

            (num_harmonics, ampl_data_copy, phase_data_copy, harmonic_ampl_enabled_copy, harmonic_phase_enabled_copy, base_period, pitch_ratio, target_samples)
        }; // All locks are released here

        let sound = render_key_buffer(
            num_harmonics,
            &ampl_data_copy,
            &phase_data_copy,
            &harmonic_ampl_enabled_copy,
            &harmonic_phase_enabled_copy,
            base_period,
            max_harmonic,
            &pitch_ratio,
            target_samples,
            Some(&shared_params.computation_cancel),
        );

        let elapsed = start_time.elapsed();
        log::trace!("async compute_buffer_for_key(key={}) took: {:?} (base_period={:.3}, total_samples={}, max_harmonic={}/{})",
                 key, elapsed, base_period, sound.len(), max_harmonic, num_harmonics);
        
        sound
    }
    
    /// Static version of normalize_amplitude_data for use in background thread
    fn normalize_amplitude_data_static(shared_params: &Arc<SharedParams>) {
        let amplitude_data = shared_params.amplitude_data.lock().unwrap();
        let mut ampl_data_normalized = shared_params.amplitude_data_normalized.lock().unwrap();
        *ampl_data_normalized = amplitude_data.clone();
        let divisor = normalize_grid_per_bucket(&mut ampl_data_normalized);
        *shared_params.grid_norm_divisor.lock().unwrap() = divisor;
    }
    
    /// Get a buffer for a key, using pre-computed version if available
    pub fn get_buffer_for_key(&self, key: usize) -> Vec<f32> {
        if key >= NUM_KEYS {
            return Vec::new();
        }
        
        let buffer_states = self.shared_params.buffer_states.lock().unwrap();
        let key_buffers = self.shared_params.key_buffers.lock().unwrap();
        
        match buffer_states[key] {
            BufferState::Clean => {
                if let Some(ref buffer) = key_buffers[key] {
                    log::debug!("Using pre-computed buffer for key {}", key);
                    return buffer.clone();
                }
            }
            BufferState::Computing => {
                // Check if we have an old buffer we can use while waiting
                if let Some(ref buffer) = key_buffers[key] {
                    log::debug!("Using old buffer for key {} while computing new one", key);
                    return buffer.clone();
                }
            }
            BufferState::Dirty => {
                // Check if we have an old buffer we can use
                if let Some(ref buffer) = key_buffers[key] {
                    log::debug!("Using old buffer for key {} (marked dirty)", key);
                    return buffer.clone();
                }
            }
        }
        
        // Fallback to synchronous computation if no buffer available
        drop(buffer_states);
        drop(key_buffers);
        log::warn!("Fallback to synchronous computation for key {}", key);
        self.assemble_buffer_for_key(key)
    }

    /// Replace the amplitude/phase grid with the result of an audio analysis.
    /// Used by the Analysis execution mode. The grid is resized to the
    /// analysis bucket count; harmonics beyond the engine's `NUM_HARMONICS`
    /// are dropped and missing ones are zero-filled.
    pub fn load_analysis(&self, result: &super::AnalysisResult) {
        let buckets = result.num_buckets().max(1);

        {
            let mut amp = self.shared_params.amplitude_data.lock().unwrap();
            let mut phase = self.shared_params.phase_data.lock().unwrap();
            let mut norm = self.shared_params.amplitude_data_normalized.lock().unwrap();
            let n = amp.len();
            for h in 0..n {
                let src_amp = result.amplitude.get(h);
                let src_phase = result.phase.get(h);
                amp[h] = (0..buckets)
                    .map(|b| src_amp.and_then(|r| r.get(b)).copied().unwrap_or(0.0))
                    .collect();
                phase[h] = (0..buckets)
                    .map(|b| src_phase.and_then(|r| r.get(b)).copied().unwrap_or(0.0))
                    .collect();
            }
            // Keep the normalized grid the same shape as the new data.
            *norm = vec![vec![0.0; buckets]; n];

            // Snapshot the pristine analysis grid so a per-harmonic "custom"
            // override can be undone (restoring the analysed row), and clear any
            // existing overrides — freshly loaded data starts fully analysed.
            *self.shared_params.analysis_amplitude_data.lock().unwrap() = amp.clone();
            *self.shared_params.analysis_phase_data.lock().unwrap() = phase.clone();
            self.shared_params
                .harmonic_ampl_custom
                .lock()
                .unwrap()
                .iter_mut()
                .for_each(|c| *c = false);
            self.shared_params
                .harmonic_phase_custom
                .lock()
                .unwrap()
                .iter_mut()
                .for_each(|c| *c = false);
        }

        {
            // Per-bucket pitch ratio drives the playback vibrato. Missing/short
            // → 1.0 (flat) so playback degrades gracefully.
            let mut ratio = self.shared_params.bucket_pitch_ratio.lock().unwrap();
            *ratio = (0..buckets)
                .map(|b| result.pitch_ratio.get(b).copied().unwrap_or(1.0))
                .collect();
        }

        {
            // What the exact inverse needs beyond the grid: per-bucket lengths
            // that tile the subtrack, plus the two non-harmonic bins.
            //
            // The condition is *only* the lengths. It used to also demand
            // `periods_per_bucket == 1` and `!truncated`, and both refusals sent
            // the audition to the transposing renderer — the very fuzz the
            // inverse removes. Grouped buckets still invert exactly
            // (`grouped_buckets_are_still_invertible`), and a truncated grid has
            // lost those bins for the renderer too: measured −15.7 dB inverted
            // against +3.3 dB rendered
            // (`a_truncated_grid_still_inverts_better_than_it_renders`).
            let exact = result.bucket_periods.len() == buckets
                && result.bucket_periods.iter().all(|&p| p >= 2.0);
            *self.shared_params.analysis_bucket_lengths.lock().unwrap() = if exact {
                result.bucket_periods.iter().map(|&p| p as usize).collect()
            } else {
                Vec::new()
            };
            *self.shared_params.analysis_dc.lock().unwrap() =
                if exact { result.dc.clone() } else { Vec::new() };
            *self.shared_params.analysis_nyquist.lock().unwrap() =
                if exact { result.nyquist.clone() } else { Vec::new() };
        }

        self.set_normalization_needed(true);
        self.shared_params.mark_all_buffers_dirty();
        self.update_assembled_chart_with_key24();
        log::info!(
            "Loaded analysis grid: {} harmonics x {} buckets",
            result.num_harmonics(),
            buckets
        );
    }

    /// Toggle the per-harmonic "custom curve" override used in Analysis mode.
    ///
    /// When `custom` is `true`, harmonic `n`'s analysed amplitude/phase row is
    /// overwritten by the user's Synth-mode curve — Constant or Nested Fourier,
    /// per the harmonic's curve-type param — so the user can replace a single
    /// analysed harmonic with one they shaped by hand. When `false`, the row is
    /// restored from the pristine analysis snapshot captured in `load_analysis`.
    pub fn set_harmonic_custom(&self, n: usize, chart_type: ChartType, custom: bool) {
        {
            let mut flags = match chart_type {
                ChartType::Amp => self.shared_params.harmonic_ampl_custom.lock().unwrap(),
                ChartType::Phase => self.shared_params.harmonic_phase_custom.lock().unwrap(),
            };
            if n >= flags.len() {
                return;
            }
            flags[n] = custom;
        }

        if custom {
            self.refill_harmonic_curve(n, chart_type);
        } else {
            {
                let snapshot = match chart_type {
                    ChartType::Amp => self.shared_params.analysis_amplitude_data.lock().unwrap(),
                    ChartType::Phase => self.shared_params.analysis_phase_data.lock().unwrap(),
                };
                let mut data = match chart_type {
                    ChartType::Amp => self.shared_params.amplitude_data.lock().unwrap(),
                    ChartType::Phase => self.shared_params.phase_data.lock().unwrap(),
                };
                if let (Some(src), Some(dst)) = (snapshot.get(n), data.get_mut(n)) {
                    if dst.len() == src.len() {
                        dst.copy_from_slice(src);
                    } else {
                        *dst = src.clone();
                    }
                }
            }
            self.set_normalization_needed(true);
            self.shared_params.mark_all_buffers_dirty();
            self.update_assembled_chart_with_key24();
        }
    }

    /// Analyse a subtrack and load the resulting grid, switching to Analysis
    /// mode. `num_buckets == 0` lets the analyser pick period-synchronous
    /// buckets. `contour` is the host's per-position fundamental (absolute Hz,
    /// uniformly resampled across the subtrack); empty → flat at `base_freq`.
    pub fn analyze_and_load(
        &self,
        samples: &[f32],
        sample_rate: f32,
        base_freq: f32,
        contour: &[f32],
        num_buckets: usize,
    ) {
        // The bucket grid is period-synchronous (num_buckets == 0): its size
        // tracks the source length and is no longer clamped to a small playback
        // cap. Playback length is now decoupled from the bucket count — every
        // key renders the source's wall-clock duration ("preserve seconds", see
        // `render_key_buffer`) — so a fine grid no longer bloats per-note
        // buffers. Only a generous safety bound remains, to keep the charts and
        // the per-bucket DFT sane on very long inputs.
        let max_buckets = (crate::constants::NUM_OF_BUCKETS_MAX as usize).max(num_buckets);
        let mut result = super::analyze_subtrack(
            samples,
            sample_rate,
            base_freq,
            contour,
            num_buckets,
            NUM_HARMONICS,
            max_buckets,
        );
        // Scale the (often very quiet) analysed grid up so the charts are
        // legible; resynthesis re-normalises separately. Keep the gain — it is
        // what the Original Pitch And Gain audition divides out to play the
        // reconstruction at the source file's own level.
        let display_gain = super::normalize_for_display(&mut result, 0.9);
        *self.shared_params.analysis_display_gain.lock().unwrap() = display_gain;
        // Record the source duration so playback lasts the same wall-clock time
        // at every key (pitch-independent), regardless of the played period.
        let duration_secs = if sample_rate > 0.0 {
            samples.len() as f32 / sample_rate
        } else {
            0.0
        };
        *self.shared_params.analysis_duration_secs.lock().unwrap() = duration_secs;
        *self.shared_params.analysis_sample_rate.lock().unwrap() = sample_rate.max(0.0);
        // Remember the source fundamental so the GUI can report the original
        // tone's absolute min/max pitch (base_freq * per-bucket pitch ratio).
        *self.shared_params.analysis_base_freq.lock().unwrap() = base_freq.max(0.0);
        self.shared_params
            .set_execution_mode(super::ExecutionMode::Analysis);
        self.load_analysis(&result);
    }

    /// Load a precomputed harmonic grid directly (from a saved LeSynth track),
    /// bypassing DFT analysis. Mirrors the tail of [`analyze_and_load`]: records
    /// the source duration and fundamental, switches to Analysis mode, and hands
    /// the grid to [`load_analysis`]. `amplitude`/`phase` are `[harmonic][bucket]`;
    /// `pitch_ratio` is one entry per bucket (`f_local / base_freq`).
    ///
    /// The instance's playback sample rate is left untouched (it must stay at the
    /// host device rate), so a note still lasts `duration_secs` of wall-clock time
    /// regardless of the rate the grid was captured at.
    ///
    /// `display_gain` is the [`normalize_for_display`](super::normalize_for_display)
    /// gain the grid was captured with (`.lsft` carries it from version 2 on).
    /// `0.0` means the saved file didn't record it, and the Original Pitch And
    /// Gain audition then falls back to playing at the grid's own level rather than
    /// inventing a source level it cannot know.
    pub fn load_grid(
        &self,
        amplitude: Vec<Vec<f32>>,
        phase: Vec<Vec<f32>>,
        pitch_ratio: Vec<f32>,
        base_freq: f32,
        duration_secs: f32,
        analysis_sample_rate: f32,
        display_gain: f32,
        bucket_lengths: Vec<usize>,
        dc: Vec<f32>,
        nyquist: Vec<f32>,
    ) {
        // `bucket_periods` is informational only (`load_analysis` ignores it);
        // derive it from the current playback rate for a consistent snapshot.
        let sr = *self.shared_params.sample_rate.lock().unwrap();
        let bucket_periods: Vec<f32> = pitch_ratio
            .iter()
            .map(|&r| sr / (base_freq.max(1.0) * r.max(1e-6)))
            .collect();
        let nb = pitch_ratio.len();
        let result = super::AnalysisResult {
            amplitude,
            phase,
            bucket_periods,
            pitch_ratio,
            // A `.lsft` from version 3 on carries the non-harmonic bins and the
            // per-bucket lengths, so a reloaded track can be auditioned exactly.
            // Older files (and hand-drawn grids) supply none, and `load_analysis`
            // then leaves the exact path switched off.
            dc: if dc.len() == nb { dc } else { vec![0.0; nb] },
            nyquist: if nyquist.len() == nb { nyquist } else { vec![0.0; nb] },
            periods_per_bucket: 1.0,
            truncated: false,
        };
        // Hand `load_analysis` the real lengths when we have them and *nothing*
        // when we don't. The rate-derived periods above are this device's
        // periods for the grid's pitch, not the sample counts the source was cut
        // into — a plausible-looking stand-in that inverts to garbage, and
        // nothing downstream can tell the difference.
        let mut result = result;
        if bucket_lengths.len() == nb && bucket_lengths.iter().all(|&n| n >= 2) {
            result.bucket_periods = bucket_lengths.iter().map(|&n| n as f32).collect();
        } else {
            result.bucket_periods = Vec::new();
            result.dc = Vec::new();
            result.nyquist = Vec::new();
        }
        *self.shared_params.analysis_duration_secs.lock().unwrap() = duration_secs.max(0.0);
        *self.shared_params.analysis_sample_rate.lock().unwrap() = analysis_sample_rate.max(0.0);
        *self.shared_params.analysis_base_freq.lock().unwrap() = base_freq.max(0.0);
        *self.shared_params.analysis_display_gain.lock().unwrap() = display_gain.max(0.0);
        self.shared_params
            .set_execution_mode(super::ExecutionMode::Analysis);
        self.load_analysis(&result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::LeSynthParams;
    use std::sync::Arc;

    fn create_test_engine() -> SynthComputeEngine {
        let params = Arc::new(LeSynthParams::default());
        SynthComputeEngine::new(params)
    }

    /// The rate conversion on its own: a tone resampled between the rates real
    /// files and devices use, against the same tone evaluated at the output
    /// rate. Only the interior is pinned — the first/last [`RESAMPLE_TAPS`]
    /// samples are filled partly from the silence outside the note, which is
    /// correct for a finite signal and shorter than the voice's own fade.
    #[test]
    fn resample_stream_is_transparent() {
        for (r_in, r_out) in [
            (22_050.0f64, 44_100.0f64),
            (22_050.0, 48_000.0),
            (48_000.0, 44_100.0),
            (24_000.0, 48_000.0),
        ] {
            for freq in [110.0f64, 1000.0, 3000.0, 7000.0] {
                let n = (r_in * 0.2) as usize;
                let x: Vec<f32> = (0..n)
                    .map(|i| (TWO_PI as f64 * freq * i as f64 / r_in).sin() as f32)
                    .collect();
                let y = resample_stream(&x, r_out / r_in);
                let guard = (2.0 * RESAMPLE_TAPS as f64 * r_out / r_in) as usize;
                let mut worst = 0.0f64;
                for j in guard..y.len() - guard {
                    let want = (TWO_PI as f64 * freq * j as f64 / r_out).sin();
                    worst = worst.max((y[j] as f64 - want).abs());
                }
                let db = 20.0 * worst.max(1e-12).log10();
                assert!(
                    db < -80.0,
                    "{r_in} -> {r_out} at {freq} Hz: {db:.1} dB",
                );
            }
        }
    }

    #[test]
    fn test_engine_creation() {
        let engine = create_test_engine();
        
        // Verify shared params were initialized correctly
        let amp_data = engine.shared_params.amplitude_data.lock().unwrap();
        assert_eq!(amp_data.len(), NUM_HARMONICS);
        assert_eq!(amp_data[0].len(), NUM_OF_BUCKETS_DEFAULT);
    }

    #[test]
    fn resample_row_preserves_silence_and_constants() {
        // An all-zero (untouched) row must stay all-zero at any new resolution —
        // this is what keeps a bucket change from resurrecting default-valued
        // harmonics as an audible buzz.
        assert!(resample_row(&[0.0; 70], 2000).iter().all(|&x| x == 0.0));
        assert!(resample_row(&[0.0; 2000], 30).iter().all(|&x| x == 0.0));
        // A constant row stays that constant (interpolation is exact between
        // equal endpoints).
        assert!(resample_row(&[0.05; 70], 500)
            .iter()
            .all(|&x| (x - 0.05).abs() < 1e-6));
        // Length always matches the request; a single sample broadcasts.
        assert_eq!(resample_row(&[0.3], 40).len(), 40);
        assert!(resample_row(&[0.3], 40).iter().all(|&x| x == 0.3));
        assert_eq!(resample_row(&[0.1, 0.9], 0).len(), 0);
    }

    #[test]
    fn set_num_buckets_keeps_untouched_grid_silent() {
        // Resizing an untouched patch (grid still all zeros) must not introduce
        // any signal, even though the harmonic params default to non-zero
        // amplitudes for higher harmonics.
        let engine = create_test_engine();
        engine.set_num_buckets(500);

        let amp = engine.shared_params.amplitude_data.lock().unwrap();
        assert_eq!(amp[0].len(), 500);
        assert!(amp.iter().all(|row| row.iter().all(|&x| x == 0.0)));
    }

    #[test]
    fn set_num_buckets_resizes_and_preserves_drawn_curve() {
        let engine = create_test_engine();
        // Draw a constant curve on harmonic 0, then resize.
        engine.fill_constant_curve(0, 0.5, ChartType::Amp);
        engine.set_num_buckets(300);

        let amp = engine.shared_params.amplitude_data.lock().unwrap();
        assert_eq!(amp[0].len(), 300);
        // The drawn constant survives the resize.
        assert!(amp[0].iter().all(|&x| (x - 0.5).abs() < 1e-6));
        // Untouched harmonics stay silent.
        assert!(amp[1].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn load_grid_sets_analysis_state() {
        // Loading a precomputed grid (a saved track) must copy amp/phase/ratio
        // into the live state, resize to the file's bucket count, and switch the
        // instance to Analysis mode with the recorded base freq / duration.
        let engine = create_test_engine();
        let nb = 5;
        let mut amplitude = vec![vec![0.0f32; nb]; NUM_HARMONICS];
        let mut phase = vec![vec![0.0f32; nb]; NUM_HARMONICS];
        amplitude[0] = vec![0.5; nb];
        amplitude[1] = vec![0.25; nb];
        phase[1] = vec![1.0; nb];
        let pitch_ratio = vec![1.0, 1.01, 0.99, 1.0, 1.0];

        engine.load_grid(
            amplitude,
            phase,
            pitch_ratio.clone(),
            220.0,
            0.75,
            44_100.0,
            1.0,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(engine.shared_params.execution_mode(), ExecutionMode::Analysis);
        assert_eq!(*engine.shared_params.analysis_base_freq.lock().unwrap(), 220.0);
        assert!(
            (*engine.shared_params.analysis_duration_secs.lock().unwrap() - 0.75).abs() < 1e-6
        );

        let amp = engine.shared_params.amplitude_data.lock().unwrap();
        assert_eq!(amp[0].len(), nb, "grid resized to the file's bucket count");
        assert!(amp[0].iter().all(|&x| (x - 0.5).abs() < 1e-6));
        assert!(amp[1].iter().all(|&x| (x - 0.25).abs() < 1e-6));
        assert!(amp[2].iter().all(|&x| x == 0.0), "untouched harmonics stay silent");
        drop(amp);

        assert!(engine.shared_params.phase_data.lock().unwrap()[1]
            .iter()
            .all(|&x| (x - 1.0).abs() < 1e-6));
        assert_eq!(*engine.shared_params.bucket_pitch_ratio.lock().unwrap(), pitch_ratio);
    }

    #[test]
    fn test_fill_constant_curve_amplitude() {
        let engine = create_test_engine();
        let test_value = 0.75f32;
        
        engine.fill_constant_curve(0, test_value, ChartType::Amp);
        
        let amp_data = engine.shared_params.amplitude_data.lock().unwrap();
        for &value in &amp_data[0] {
            assert_eq!(value, test_value);
        }
    }

    #[test]
    fn test_fill_constant_curve_phase() {
        let engine = create_test_engine();
        let test_value = 3.14f32;
        
        engine.fill_constant_curve(0, test_value, ChartType::Phase);
        
        let phase_data = engine.shared_params.phase_data.lock().unwrap();
        for &value in &phase_data[0] {
            assert_eq!(value, test_value);
        }
    }

    #[test]
    fn test_normalization_needed_flag() {
        let engine = create_test_engine();
        
        // Initially should be false
        assert_eq!(*engine.shared_params.normalization_needed.lock().unwrap(), false);
        
        // Set to true
        engine.set_normalization_needed(true);
        assert_eq!(*engine.shared_params.normalization_needed.lock().unwrap(), true);
        
        // Set back to false
        engine.set_normalization_needed(false);
        assert_eq!(*engine.shared_params.normalization_needed.lock().unwrap(), false);
    }

    #[test]
    fn test_normalize_amplitude_data_empty() {
        let engine = create_test_engine();
        
        // Set some test data
        {
            let mut amp_data = engine.shared_params.amplitude_data.lock().unwrap();
            amp_data[0][0] = 0.5;
            amp_data[1][0] = 0.3;
        }
        
        engine.normalize_amplitude_data();
        
        let normalized = engine.shared_params.amplitude_data_normalized.lock().unwrap();
        // Values should remain the same when sum <= 1.0
        assert_eq!(normalized[0][0], 0.5);
        assert_eq!(normalized[1][0], 0.3);
    }

    #[test]
    fn test_analyze_and_load_changes_bucket_count_without_panic() {
        // Regression: load_analysis used to leave amplitude_data_normalized at
        // the old bucket count, so the next assemble indexed out of bounds.
        let engine = create_test_engine();

        let sr = 44_100.0;
        let freq = 220.0;
        let samples: Vec<f32> = (0..sr as usize)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin())
            .collect();

        // Auto bucket count (≈ number of periods) differs from the default 70.
        engine.analyze_and_load(&samples, sr, freq, &[], 0);

        let buckets = engine.shared_params.amplitude_data.lock().unwrap()[0].len();
        assert_ne!(buckets, NUM_OF_BUCKETS_DEFAULT, "test should exercise a resize");

        // Must not panic and must produce audio.
        let buf = engine.assemble_buffer_for_key(24);
        assert!(!buf.is_empty());
    }

    #[test]
    fn test_normalize_amplitude_data_scaling() {
        let engine = create_test_engine();
        
        // Set test data that requires scaling
        {
            let mut amp_data = engine.shared_params.amplitude_data.lock().unwrap();
            amp_data[0][0] = 1.0;
            amp_data[1][0] = 1.0;
            // Sum of maximums = 2.0, should scale down by factor of 2
        }
        
        engine.normalize_amplitude_data();
        
        let normalized = engine.shared_params.amplitude_data_normalized.lock().unwrap();
        assert_eq!(normalized[0][0], 0.5); // 1.0 / 2.0
        assert_eq!(normalized[1][0], 0.5); // 1.0 / 2.0
    }

    /// A harmonic-rich tone, like a sustained instrument note.
    fn tone(sr: f32, f: f32, secs: f32) -> Vec<f32> {
        let n = (sr * secs) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / sr;
                0.6 * (2.0 * std::f32::consts::PI * f * t).sin()
                    + 0.3 * (2.0 * std::f32::consts::PI * 2.0 * f * t).sin()
                    + 0.15 * (2.0 * std::f32::consts::PI * 3.0 * f * t).sin()
            })
            .collect()
    }

    fn max_abs(buf: &[f32]) -> f32 {
        buf.iter().fold(0.0f32, |m, &x| m.max(x.abs()))
    }

    #[test]
    fn analysis_grid_is_uncapped_and_preserves_seconds() {
        // The old 128-bucket playback cap is gone: a multi-second note keeps a
        // fine, source-tracking grid (playback length is now decoupled from the
        // bucket count). And every key renders the source's wall-clock duration
        // ("preserve seconds"), independent of the played key's period.
        let engine = create_test_engine();
        let sr = 44100.0;
        let secs = 3.0;
        engine.analyze_and_load(&tone(sr, 587.0, secs), sr, 587.0, &[], 0);

        let buckets = engine.shared_params.amplitude_data.lock().unwrap()[0].len();
        assert!(buckets > 128, "grid should no longer be capped at 128, got {}", buckets);

        let target = (secs * sr) as i64;
        for key in [0usize, 24, 48, 72] {
            let len = engine.assemble_buffer_for_key(key).len() as i64;
            let period = engine.shared_params.piano_periods.lock().unwrap()[key] as i64;
            // The render overshoots the target by at most one final period.
            assert!(
                len >= target && len - target <= period,
                "key {} len {} not ~{} (period {})",
                key,
                len,
                target,
                period
            );
        }
    }

    #[test]
    fn analysis_load_populates_assembled_chart() {
        let engine = create_test_engine();
        engine.analyze_and_load(&tone(44100.0, 440.0, 1.0), 44100.0, 440.0, &[], 0);
        let plotted = engine.shared_params.assembled_sound_plotted.lock().unwrap();
        assert!(!plotted.is_empty(), "assembled chart is empty after analysis load");
        assert!(max_abs(&plotted) > 0.01, "assembled chart is silent");
    }

    #[test]
    fn custom_override_toggles_flag_and_restores_analysed_row() {
        let engine = create_test_engine();
        engine.analyze_and_load(&tone(44100.0, 440.0, 1.0), 44100.0, 440.0, &[], 0);
        let h = 1usize;

        // Fresh analysis: override flags default off and the snapshot matches the
        // live grid.
        assert!(!engine.shared_params.harmonic_ampl_custom.lock().unwrap()[h]);
        let snapshot = engine.shared_params.analysis_amplitude_data.lock().unwrap()[h].clone();
        assert_eq!(snapshot, engine.shared_params.amplitude_data.lock().unwrap()[h]);

        // Enabling the override sets the flag and rewrites the row.
        engine.set_harmonic_custom(h, ChartType::Amp, true);
        assert!(engine.shared_params.harmonic_ampl_custom.lock().unwrap()[h]);

        // Scribble over the live row to prove the restore actually rewrites it,
        // then disable the override: the analysed row must come back verbatim.
        engine.shared_params.amplitude_data.lock().unwrap()[h]
            .iter_mut()
            .for_each(|v| *v = 0.123);
        engine.set_harmonic_custom(h, ChartType::Amp, false);
        assert!(!engine.shared_params.harmonic_ampl_custom.lock().unwrap()[h]);
        assert_eq!(snapshot, engine.shared_params.amplitude_data.lock().unwrap()[h]);
    }

    #[test]
    fn drawing_does_not_override_analysed_row_without_cust() {
        // Regression: with an analysis loaded, drawing a Synth-mode curve for a
        // harmonic must NOT touch its live (analysed) row until "cust" is ticked.
        let engine = create_test_engine();
        engine.analyze_and_load(&tone(44100.0, 440.0, 1.0), 44100.0, 440.0, &[], 0);
        let h = 1usize;

        let analysed = engine.shared_params.amplitude_data.lock().unwrap()[h].clone();
        assert!(analysed.iter().any(|&x| x != 0.0), "test needs a non-empty analysed row");

        // Shape a distinctive Synth-mode curve (default curve type is
        // NestedFourier) so an override would be plainly visible on the row.
        engine.synth_params.harmonics[h]
            .nested_fourier
            .write()
            .unwrap()
            .series_mut(ChartType::Amp)
            .amps[0] = 0.5;

        // "cust" is off (fresh analysis) → the drawn curve is ignored on the grid.
        assert!(!engine.shared_params.harmonic_ampl_custom.lock().unwrap()[h]);
        engine.fill_constant_curve(h, 0.9, ChartType::Amp);
        engine.fill_nested_fourier_curve(h, ChartType::Amp);
        assert_eq!(
            analysed,
            engine.shared_params.amplitude_data.lock().unwrap()[h],
            "drawing without cust must not clobber the analysed row"
        );

        // Ticking "cust" applies the drawn Synth-mode curve, so the live row must
        // now depart from the analysed data.
        engine.set_harmonic_custom(h, ChartType::Amp, true);
        assert_ne!(
            analysed,
            engine.shared_params.amplitude_data.lock().unwrap()[h],
            "enabling cust must override the row with the drawn curve"
        );
    }

    #[test]
    fn analysis_playback_buffers_are_audible() {
        let engine = create_test_engine();
        engine.analyze_and_load(&tone(44100.0, 440.0, 1.0), 44100.0, 440.0, &[], 0);
        // Both the synchronous (GUI fallback) and static (async thread) render
        // paths must yield non-empty, non-silent audio for a range of keys.
        for key in [0usize, 24, 48, 60] {
            let inst = engine.assemble_buffer_for_key(key);
            assert!(!inst.is_empty(), "instance buffer empty for key {}", key);
            assert!(max_abs(&inst) > 0.01, "instance buffer silent for key {}", key);

            let stat = SynthComputeEngine::compute_buffer_for_key_static(&engine.shared_params, key);
            assert!(!stat.is_empty(), "static buffer empty for key {}", key);
            assert!(max_abs(&stat) > 0.01, "static buffer silent for key {}", key);
        }
    }

    #[test]
    fn analysis_vibrato_contour_reaches_playback() {
        // End-to-end: a vibrato tone + its contour → analyze_and_load → the
        // per-bucket pitch ratios are stored and playback stays audible. This
        // is the flow the host drives when opening an audio file.
        let sr = 44100.0;
        let base = 440.0f32;
        let (depth, rate) = (0.03f32, 5.0f32);
        let n = (sr * 1.5) as usize;
        let mut phase = 0.0f32;
        let mut samples = Vec::with_capacity(n);
        let mut contour = Vec::new();
        for i in 0..n {
            let t = i as f32 / sr;
            let f = base * (1.0 + depth * (2.0 * std::f32::consts::PI * rate * t).sin());
            phase += 2.0 * std::f32::consts::PI * f / sr;
            samples.push(phase.sin());
            if i % 256 == 0 {
                contour.push(f);
            }
        }

        let engine = create_test_engine();
        engine.analyze_and_load(&samples, sr, base, &contour, 0);
        assert_eq!(engine.shared_params.execution_mode(), ExecutionMode::Analysis);

        // Stored ratios must reflect the vibrato (not all flat).
        let ratios = engine.shared_params.bucket_pitch_ratio.lock().unwrap().clone();
        let hi = ratios.iter().cloned().fold(f32::MIN, f32::max);
        let lo = ratios.iter().cloned().fold(f32::MAX, f32::min);
        assert!(hi - lo > 0.02, "vibrato not reflected in playback ratios: [{lo}, {hi}]");

        // Playback still produces audible audio.
        let buf = engine.assemble_buffer_for_key(48);
        assert!(max_abs(&buf) > 0.01, "vibrato playback is silent");
    }

    #[test]
    fn synth_mode_buffers_unaffected_by_stale_ratios() {
        // Leftover analysis ratios must never bend synth-mode playback.
        let engine = create_test_engine();
        let buckets = engine.shared_params.amplitude_data.lock().unwrap()[0].len();
        {
            let mut r = engine.shared_params.bucket_pitch_ratio.lock().unwrap();
            *r = vec![1.5; buckets];
        }
        engine.shared_params.set_execution_mode(ExecutionMode::Synth);
        let base_period = engine.shared_params.piano_periods.lock().unwrap()[36];
        let len = engine.assemble_buffer_for_key(36).len();
        // One cycle per bucket at the key's own fractional period; within a sample
        // of the flat total, and nowhere near the 1.5× the stale ratios would give.
        let want = buckets as f32 * base_period;
        assert!(
            (len as f32 - want).abs() <= 1.0,
            "synth playback must ignore ratios: {len} vs {want}"
        );
    }


    /// Direct sinusoid sum for a single bucket — the reference the IFFT path
    /// must match. Mirrors the direct branch in `render_key_buffer`.
    fn direct_bucket(
        ampl: &[Vec<f32>],
        phase: &[Vec<f32>],
        ampl_enabled: &[bool],
        phase_enabled: &[bool],
        bucket: usize,
        period: usize,
        max_h: usize,
    ) -> Vec<f32> {
        (0..period)
            .map(|t| {
                let mut sample = 0.0f32;
                for n in 0..max_h {
                    let amp = ampl[n][bucket];
                    if !ampl_enabled[n] || amp == 0.0 {
                        continue;
                    }
                    let ph = if phase_enabled[n] { phase[n][bucket] } else { 0.0 };
                    sample += amp
                        * (TWO_PI * (n as f32 + 1.0) * (t as f32) / (period as f32) + ph).sin();
                }
                sample.clamp(-1.0, 1.0)
            })
            .collect()
    }

    #[test]
    fn cycle_table_matches_direct_sum() {
        // The cycle-table fast path must stay numerically equivalent to the direct
        // sinusoid sum, for a variety of periods and mixed amp/phase/enable flags.
        // The table is oversampled and read with Catmull-Rom, so this also bounds
        // the interpolation error the fractional readout introduces.
        for &period in &[64usize, 65, 100, 128, 129, 512] {
            let max_h = (period / 2).min(40).max(12); // exercise the IFFT branch
            // Deterministic pseudo-random-ish grid, one bucket.
            let mut ampl = vec![vec![0.0f32]; max_h];
            let mut phase = vec![vec![0.0f32]; max_h];
            let mut ampl_enabled = vec![true; max_h];
            let mut phase_enabled = vec![true; max_h];
            for n in 0..max_h {
                ampl[n][0] = 0.02 * ((n * 7 % 11) as f32) + 0.01; // small, avoids clamping
                phase[n][0] = (n as f32 * 1.3).sin() * std::f32::consts::PI;
                if n % 5 == 0 {
                    ampl_enabled[n] = false; // disabled harmonic contributes nothing
                }
                if n % 3 == 0 {
                    phase_enabled[n] = false; // phase forced to 0
                }
            }

            let want = direct_bucket(&ampl, &phase, &ampl_enabled, &phase_enabled, 0, period, max_h);

            let mut bank = IfftBank::new();
            let mut table = Vec::new();
            let len = cycle_table_len(max_h);
            build_cycle_table(
                &mut bank, &mut table, &ampl, &phase, &ampl_enabled, &phase_enabled, 0, len, max_h,
            );
            assert_eq!(table.len(), len);
            // Read one cycle back at the requested period, as the renderer does.
            let got: Vec<f32> = (0..period)
                .map(|t| read_cycle(&table, t as f32 / period as f32).clamp(-1.0, 1.0))
                .collect();

            let max_err = want
                .iter()
                .zip(&got)
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(
                max_err < 2e-3,
                "cycle table diverged from direct sum (period {period}, max_h {max_h}, \
                 table {len}): max_err {max_err}"
            );
        }
    }

    #[test]
    fn bucket_period_scales_with_ratio() {
        let near = |a: f32, b: f32| (a - b).abs() < 1e-4;
        assert!(near(bucket_period(100.0, &[1.0], 0), 100.0)); // flat
        assert!(near(bucket_period(100.0, &[2.0], 0), 50.0)); // sharper → shorter
        assert!(near(bucket_period(100.0, &[0.5], 0), 200.0)); // flatter → longer
        assert!(near(bucket_period(100.0, &[], 5), 100.0)); // missing → flat
        assert!(bucket_period(2.0, &[1000.0], 0) >= 2.0); // clamped ≥ 2
        // The whole point: a period between two whole samples stays between them.
        assert!(near(bucket_period(37.33, &[1.0], 0), 37.33));
        assert!(near(bucket_period(37.33, &[1.01], 0), 37.33 / 1.01));
    }

    #[test]
    fn analysis_pitch_ratio_transposes_playback_period() {
        let engine = create_test_engine();
        let key = 40;
        let buckets = engine.shared_params.amplitude_data.lock().unwrap()[0].len();
        let base_period = engine.shared_params.piano_periods.lock().unwrap()[key];
        *engine.shared_params.normalization_needed.lock().unwrap() = false;

        // A ratio grid is present, but Synth mode must ignore it (flat playback).
        {
            let mut r = engine.shared_params.bucket_pitch_ratio.lock().unwrap();
            *r = vec![2.0; buckets];
        }
        engine.shared_params.set_execution_mode(ExecutionMode::Synth);
        let synth_len = engine.assemble_buffer_for_key(key).len();
        // One cycle per bucket at the key's own (fractional) period. The total is
        // within a sample of `buckets * base_period` — it is no longer an exact
        // integer multiple, because the period no longer is one.
        let want = buckets as f32 * base_period;
        assert!(
            (synth_len as f32 - want).abs() <= 1.0,
            "synth playback must stay flat: {synth_len} vs {want}"
        );

        // Analysis mode applies the ratio: every bucket period scales by 1/ratio.
        engine.shared_params.set_execution_mode(ExecutionMode::Analysis);
        let analysis_len = engine.assemble_buffer_for_key(key).len();
        let ratios = vec![2.0; buckets];
        let expected: f32 = (0..buckets)
            .map(|b| bucket_period(base_period, &ratios, b))
            .sum();
        assert!(
            (analysis_len as f32 - expected).abs() <= 1.0,
            "ratio should transpose each bucket period: {analysis_len} vs {expected}"
        );
        assert!(analysis_len < synth_len, "ratio > 1 shortens the rendered note");
    }

    /// The synchronous fallback and the background thread used to normalise
    /// *differently* (sum of per-harmonic peaks vs. per bucket), so the same grid
    /// rendered on the two paths produced two different sounds. They now share
    /// one rule.
    #[test]
    fn both_normalisation_paths_agree() {
        let engine = create_test_engine();
        {
            let mut amp = engine.shared_params.amplitude_data.lock().unwrap();
            // A grid where the two rules disagree: bucket 0 sums above 1 while
            // bucket 1 stays under, and the per-harmonic peaks live in different
            // buckets.
            amp[0][0] = 0.9;
            amp[1][0] = 0.6;
            amp[2][1] = 0.3;
        }
        engine.normalize_amplitude_data();
        let sync = engine.shared_params.amplitude_data_normalized.lock().unwrap().clone();

        SynthComputeEngine::normalize_amplitude_data_static(&engine.shared_params);
        let background = engine.shared_params.amplitude_data_normalized.lock().unwrap().clone();

        assert_eq!(sync, background, "the two render paths must normalise identically");
        // The factor is global, from the loudest bucket (bucket 0, sum 1.5), so
        // every bucket is scaled by the same 1/1.5 and their *relative* levels
        // survive. Scaling each bucket by its own sum would leave bucket 1 at 0.3
        // and act as a compressor.
        assert!((sync[0][0] - 0.9 / 1.5).abs() < 1e-6, "{}", sync[0][0]);
        assert!((sync[1][0] - 0.6 / 1.5).abs() < 1e-6, "{}", sync[1][0]);
        assert!((sync[2][1] - 0.3 / 1.5).abs() < 1e-6, "{}", sync[2][1]);
        // No bucket can clip: harmonics never sum past 1.
        for b in 0..sync[0].len() {
            let sum: f32 = sync.iter().map(|r| r[b]).sum();
            assert!(sum <= 1.0 + 1e-6, "bucket {b} sums to {sum}");
        }
    }

    /// The host bridge must render exactly what playback renders — a regression
    /// test built on it is worthless if the two can drift apart.
    #[test]
    fn resynthesize_grid_matches_the_playback_path() {
        let engine = create_test_engine();
        let sr = 44_100.0;
        let f = 220.0;
        let samples = tone(sr, f, 0.4);
        engine.analyze_and_load(&samples, sr, f, &[], 0);

        let key = 40;
        let via_engine = engine.assemble_buffer_for_key(key);

        let amplitude = engine.shared_params.amplitude_data.lock().unwrap().clone();
        let phase = engine.shared_params.phase_data.lock().unwrap().clone();
        let ratios = engine.shared_params.bucket_pitch_ratio.lock().unwrap().clone();
        let base_period = engine.shared_params.piano_periods.lock().unwrap()[key];
        let via_bridge = resynthesize_grid(
            &amplitude,
            &phase,
            &ratios,
            base_period,
            max_harmonic_for_key(key),
            via_engine.len().min((0.4 * sr) as usize),
            // Key playback is not level-restored, so neither is the bridge here:
            // the two must stay sample-identical.
            0.0,
        );

        assert!(!via_engine.is_empty());
        assert_eq!(via_engine.len(), via_bridge.len(), "lengths must agree");
        assert_eq!(via_engine, via_bridge, "bridge and playback must be sample-identical");
    }

    #[test]
    fn original_pitch_render_uses_the_source_period_and_duration() {
        let engine = create_test_engine();
        let sr = 44_100.0;
        let f = 220.0;
        let secs = 0.4;
        let samples = tone(sr, f, secs);
        engine.analyze_and_load(&samples, sr, f, &[], 0);

        let buf = engine.assemble_buffer_at_original_pitch();
        assert!(max_abs(&buf) > 0.05, "original-pitch audition must be audible");

        // "Preserve seconds": exactly the source's duration.
        let expected = (secs * sr) as usize;
        let period = sr / f; // fractional, unrounded
        assert!(
            buf.len() >= expected && buf.len() < expected + period.ceil() as usize,
            "length {} not ~{} samples",
            buf.len(),
            expected
        );

        // Every bucket renders the source's own *exact* period (the analysis was
        // flat, so the pitch ratios are ~1) — the source's pitch, not a key's, and
        // not rounded to whole samples.
        let ratios = engine.shared_params.bucket_pitch_ratio.lock().unwrap().clone();
        assert!(
            (0..ratios.len())
                .map(|b| bucket_period(period, &ratios, b))
                .all(|p| (p - period).abs() < 0.05 * period),
            "flat analysis should render the source period everywhere"
        );
    }

    /// The audition must leave the plugin at the source file's own level or an
    /// A/B against that file is meaningless. Deliberately quiet source: that is
    /// where the display normalisation applies its largest gain (×18).
    #[test]
    fn original_pitch_render_is_at_the_source_level() {
        let engine = create_test_engine();
        let sr = 44_100.0;
        let f = 220.0;
        let quiet: Vec<f32> = tone(sr, f, 0.4).iter().map(|v| v * 0.05).collect();
        engine.analyze_and_load(&quiet, sr, f, &[], 0);

        // The gain really was large, so this test is exercising the case that
        // matters rather than a grid that happened to need no scaling.
        let gain = *engine.shared_params.analysis_display_gain.lock().unwrap();
        assert!(gain > 10.0, "display gain {gain} — source is not quiet enough to test");

        let buf = engine.assemble_buffer_at_original_pitch();
        assert!(!buf.is_empty());

        // Compare the steady middle: the first and last buckets are half-covered
        // by the analysis window, so their level is genuinely lower in both.
        let rms = |x: &[f32]| {
            (x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt()
        };
        let mid = |x: &[f32]| {
            let (a, b) = (x.len() / 4, x.len() * 3 / 4);
            rms(&x[a..b])
        };
        // The buffer is pre-compensated for the mixdown's per-voice headroom, so
        // what the plugin *emits* — buffer × VOICE_MIX_SCALING — is the source.
        let emitted = mid(&buf) * VOICE_MIX_SCALING as f64;
        let db = 20.0 * (emitted / mid(&quiet)).log10();
        assert!(
            db.abs() < 1.0,
            "audition plays {db:+.2} dB off the source it reproduces"
        );

        // A key is *not* level-restored: it is an instrument voice, and it keeps
        // the full-scale grid so quiet source material still plays at a usable
        // level across the keyboard.
        let key_buf = engine.assemble_buffer_for_key(40);
        assert!(
            mid(&key_buf) > 4.0 * emitted,
            "key playback should stay at the normalised level"
        );
    }

    /// The requirement in one assertion: what the plugin emits for Original
    /// Pitch And Gain **is** the file it analysed, every sample — not
    /// "correlates with", not "at the same level".
    ///
    /// Two defects hid behind the exact-inverse tests, which measured the
    /// transform rather than the button: the buffer was clamped to ±1 before the
    /// mixer's `VOICE_MIX_SCALING` was reapplied (flat-topping anything above
    /// 0.8 — hence the deliberately loud source here), and the rate change was
    /// done per bucket (see [`resample_stream`]).
    #[test]
    fn original_pitch_audition_reproduces_the_source_sample_for_sample() {
        let engine = create_test_engine();
        let sr = 44_100.0;
        let f = 220.0;
        // Peak just under full scale, like a normalised recording.
        let loud: Vec<f32> = {
            let raw = tone(sr, f, 0.4);
            let peak = max_abs(&raw);
            raw.iter().map(|v| v * 0.98 / peak).collect()
        };
        engine.shared_params.update_sample_rate(sr);
        engine.analyze_and_load(&loud, sr, f, &[], 0);

        let buf = engine.assemble_buffer_at_original_pitch();
        assert_eq!(buf.len(), loud.len(), "the audition must cover the subtrack");

        // What the mixer emits for a single voice is buffer × VOICE_MIX_SCALING.
        let peak = max_abs(&loud);
        let worst = buf
            .iter()
            .zip(&loud)
            .fold(0.0f32, |m, (&got, &want)| {
                m.max((got * VOICE_MIX_SCALING - want).abs())
            })
            / peak;
        let db = 20.0 * worst.max(1e-12).log10();
        assert!(db < -80.0, "the audition is not the source: {db:.1} dB");

        // And it is not clipped: the intermediate buffer genuinely exceeds 1.0,
        // which is the state the old clamp destroyed.
        assert!(
            max_abs(&buf) > 1.0,
            "a source at 0.98 must exceed 1.0 once the mixer's headroom is undone"
        );
    }

    /// The same requirement when the device does not run at the file's rate —
    /// the normal case. At an exact 2× a band-limited interpolation returns the
    /// source samples themselves on the even outputs, so the source stays the
    /// reference rather than another resampler.
    #[test]
    fn original_pitch_audition_survives_the_device_rate() {
        let engine = create_test_engine();
        let analysis_sr = 24_000.0;
        let device_sr = 48_000.0;
        let f = 220.0;
        let src = tone(analysis_sr, f, 0.4);

        engine.shared_params.update_sample_rate(device_sr);
        engine.analyze_and_load(&src, analysis_sr, f, &[], 0);
        let buf = engine.assemble_buffer_at_original_pitch();

        assert!(
            (buf.len() as i64 - 2 * src.len() as i64).abs() <= 2,
            "audition is {} samples, expected ~{}",
            buf.len(),
            2 * src.len()
        );

        // Skip the kernel's edge region at each end, where the interpolation is
        // fed by the silence outside the note.
        let guard = 2 * RESAMPLE_TAPS;
        let peak = max_abs(&src);
        let worst = (guard..src.len() - guard).fold(0.0f32, |m, i| {
            m.max((buf[2 * i] * VOICE_MIX_SCALING - src[i]).abs())
        }) / peak;
        let db = 20.0 * worst.max(1e-12).log10();
        assert!(
            db < -80.0,
            "at {device_sr} Hz the audition is {db:.1} dB from the source"
        );
    }

    /// The exact path turns on from the per-bucket lengths alone, so the one
    /// case that must stay off is a grid that arrived without them (pre-v3
    /// `.lsft`, or hand-drawn). `load_grid` fills `bucket_periods` from the
    /// playback rate for its snapshot, and those look like real lengths while
    /// being the wrong ones.
    #[test]
    fn a_grid_without_bucket_lengths_does_not_claim_the_exact_path() {
        let engine = create_test_engine();
        let buckets = 32;
        let amplitude = vec![vec![0.5f32; buckets]; NUM_HARMONICS];
        let phase = vec![vec![0.0f32; buckets]; NUM_HARMONICS];
        engine.load_grid(
            amplitude,
            phase,
            vec![1.0; buckets],
            220.0,
            0.5,
            44_100.0,
            0.9,
            Vec::new(), // pre-v3: no lengths
            Vec::new(),
            Vec::new(),
        );
        assert!(
            engine.shared_params.analysis_bucket_lengths.lock().unwrap().is_empty(),
            "a grid with no bucket lengths must not be inverted"
        );
        // It still auditions — through the transposing renderer, which is what
        // that grid can honestly support.
        assert!(!engine.assemble_buffer_at_original_pitch().is_empty());
    }

    #[test]
    fn original_pitch_render_is_empty_without_analysis() {
        let engine = create_test_engine();
        // Synth mode: there is no "original" to reproduce.
        assert!(engine.assemble_buffer_at_original_pitch().is_empty());

        // Analysis mode but no known fundamental → still nothing, no panic.
        engine.shared_params.set_execution_mode(ExecutionMode::Analysis);
        *engine.shared_params.analysis_base_freq.lock().unwrap() = 0.0;
        assert!(engine.assemble_buffer_at_original_pitch().is_empty());
    }
}
