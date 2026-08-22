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

/// The [`PlaybackGrid`] for the live analysis, built on demand and cached until
/// the grid changes.
///
/// `None` outside Analysis mode, and for any grid the analysis did not record
/// bucket lengths for (hand-drawn, or a `.lsft` from before version 3) — those
/// keep transposing from the stored grid and the pitch contour, as before.
fn playback_grid(shared_params: &SharedParams) -> Option<Arc<PlaybackGrid>> {
    if shared_params.execution_mode() != ExecutionMode::Analysis {
        return None;
    }
    // One lock for the whole check-and-build, so two keys starting at once
    // cannot both pay for the transform.
    let mut cached = shared_params.playback_grid.lock().unwrap();
    if !shared_params.playback_grid_dirty.swap(false, Ordering::Relaxed) {
        if let Some(grid) = cached.as_ref() {
            return Some(grid.clone());
        }
    }
    let built = build_playback_grid(
        &shared_params.amplitude_data.lock().unwrap(),
        &shared_params.phase_data.lock().unwrap(),
        &shared_params.analysis_bucket_lengths.lock().unwrap(),
        &shared_params.analysis_dc.lock().unwrap(),
        &shared_params.analysis_nyquist.lock().unwrap(),
        &shared_params.bucket_pitch_ratio.lock().unwrap(),
    )
    .map(Arc::new);
    *cached = built.clone();
    built
}

/// This key's clocks over `grid`, in output samples: how long one cycle of each
/// bucket lasts (the pitch) and how long the bucket itself lasts (the source's
/// own duration, the same on every key).
///
/// `base_period` is the key's period and `nominal` the source's, both in their
/// own rate's samples, so their ratio is the transposition.
fn key_timing(
    grid: &PlaybackGrid,
    shared_params: &SharedParams,
    base_period: f32,
) -> Option<(Vec<f32>, Vec<f32>)> {
    let base_freq = *shared_params.analysis_base_freq.lock().unwrap();
    let analysis_rate = *shared_params.analysis_sample_rate.lock().unwrap();
    let out_rate = *shared_params.sample_rate.lock().unwrap();
    if base_freq <= 0.0 || analysis_rate <= 0.0 || out_rate <= 0.0 {
        return None;
    }
    let nominal = analysis_rate / base_freq;
    if !(nominal >= 2.0) {
        return None;
    }
    let rate = out_rate / analysis_rate;
    let periods = grid
        .periods
        .iter()
        .map(|&t| (base_period * t / nominal).max(2.0))
        .collect();
    let spans = grid.spans.iter().map(|&l| (l * rate).max(1.0)).collect();
    Some((periods, spans))
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
#[derive(Default)]
struct IfftBank {
    planner: RealFftPlanner<f32>,
    plans: HashMap<usize, Arc<dyn ComplexToReal<f32>>>,
}

impl IfftBank {
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
/// **Buckets are stepped, not blended.** Their waveforms are already aligned to
/// one phase origin and are one true period each ([`PlaybackGrid`]), so
/// neighbours differ only by what the source did between them — measured at −75
/// dB on a steady tone, where a cross-fade bought 1 dB and would blur the
/// period-to-period variation that on speech is the signal.
///
/// `target_samples`: `0` = Synth timeline, one cycle per bucket. `> 0` =
/// Analysis "preserve seconds", render that many samples and pick each cycle's
/// bucket by position in time, so a note lasts the source's duration at every
/// key. `cancel` lets the background thread bail out and yield.
///
/// `timing` is the source's own clocks ([`BucketTiming`]), present whenever the
/// grid came from an analysis. It replaces both `ratios` *and* the uniform time
/// grid: a cycle is rendered at the bucket's true period and the bucket for a
/// given moment is found by walking the source at wall-clock speed, rather than
/// by dividing the timeline into equal parts. `None` falls back to `ratios`,
/// which is what Synth mode and pre-v3 grids use.
fn render_key_buffer(
    num_harmonics: usize,
    ampl: &[Vec<f32>],
    phase: &[Vec<f32>],
    ampl_enabled: &[bool],
    phase_enabled: &[bool],
    base_period: f32,
    max_harmonic: usize,
    ratios: &[f32],
    timing: Option<&BucketTiming>,
    target_samples: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Vec<f32> {
    let num_buckets = ampl.first().map(|r| r.len()).unwrap_or(0);
    if num_buckets == 0 {
        return Vec::new();
    }
    let drive_by_time = target_samples > 0;
    // The two clocks are gated separately, because only one of them needs a wall
    // clock.
    //
    // `timing` is the **pitch**: each bucket's true period, which is the whole
    // point of the `PlaybackGrid` and is right on any timeline. Gating it on
    // `drive_by_time` — as this did — threw the true periods away whenever
    // `target_samples` was 0 and silently rendered the rounded bucket lengths
    // instead, which is the buzz the grid exists to remove.
    //
    // `walk` is the **wall clock**: following the source's spans at its own
    // speed. That genuinely needs a duration to lay the note on, so a Synth
    // timeline (one cycle per bucket) has no use for it.
    let timing = timing.filter(|t| t.describes(num_buckets));
    let walk = timing.filter(|_| drive_by_time);

    let mut sound: Vec<f32> = Vec::with_capacity(if drive_by_time { target_samples } else { 0 });
    let mut cache = CycleCache::default();

    // Waveform phase, in cycles, and (in source-timed mode) position along the
    // source, in buckets. Both `f64`: they accumulate for the whole note, and at
    // 4 kHz and 44.1 kHz an f32 mantissa would start losing sub-sample
    // resolution within a second, reintroducing the very detuning the fractional
    // accumulator exists to avoid.
    let mut cycles = 0.0f64;
    let mut along = 0.0f64;
    let mut last_cycle = usize::MAX;
    let mut bucket = usize::MAX;
    let mut period = base_period.max(2.0);
    let mut dc = 0.0f32;
    let mut ramp = 0.0f32;
    let mut last_yield = 0usize;

    // Anti-alias cap: harmonic k lands at k / period cycles per sample, so it
    // must stay below Nyquist (k < period / 2). Taken **once**, from the
    // shortest period the note will use, and held for the whole note.
    //
    // Deriving it per bucket instead let vibrato walk `floor(period / 2)` across
    // an integer mid-note, switching the top harmonics off and on at the bucket
    // boundary — full-depth amplitude modulation of the top of the band, gated
    // at the cycle rate, which is a buzz. It survives zeroing the phases,
    // because it is not a phase effect: that is how it was finally noticed,
    // after the phase-domain suspects were ruled out. A note with 3% vibrato
    // moved the cap at a third of its bucket boundaries.
    //
    // Holding the cap costs a harmonic or two of bandwidth on the longest
    // periods, which is inaudible. The flapping was not.
    let min_period = (0..num_buckets)
        .map(|b| match timing {
            Some(t) => t.periods[b].max(2.0),
            None => bucket_period(base_period, ratios, b),
        })
        .fold(f32::INFINITY, f32::min);
    let max_h = num_harmonics
        .min(max_harmonic)
        .min((min_period * 0.5).floor() as usize)
        // Never synthesise past what the source carries: above that the grid
        // holds fitted noise, and rendering noise periodically makes it a tone.
        .min(timing.map(|t| t.usable_harmonics).unwrap_or(usize::MAX));

    // With the source's own periods in hand, synthesise pitch-synchronously:
    // one grain per output period, overlapped at the boundaries. The
    // accumulator below cannot transpose coherently — see [`render_psola`] —
    // and stays for grids that have no true periods to be synchronous with.
    // Which renderer. At the source's own pitch the accumulator below *is* the
    // exact inverse — the two clocks coincide, every bucket is read at the phase
    // its rotation describes, and no cycle is ever spliced — so there is nothing
    // for a resynthesis to improve and a measurable amount for it to lose (a
    // steady tone: -60 dB against -52.7, and with vibrato -53.4 against -25.8,
    // because PSOLA lays its grains on a smooth epoch grid of its own rather
    // than on the recording's boundaries).
    //
    // Off that pitch the accumulator has no coherent answer at all and PSOLA
    // does. The threshold is a cent, far below where either is audible, so the
    // changeover cannot be heard.
    let unity = timing
        .map(|t| {
            let p: f32 = t.periods.iter().sum();
            let s: f32 = t.spans.iter().sum();
            p > 0.0 && (s / p - 1.0).abs() < 6e-4
        })
        .unwrap_or(false);
    // `LESYNTH_NO_PSOLA=1` forces the accumulator for every key: the A/B that
    // shows what the resynthesis is worth on a given source, and the way the
    // numbers in `a_transposed_key_is_not_spliced_out_of_several_buckets` were
    // set. Not a supported setting — a bisection tool.
    let forced_off = std::env::var_os("LESYNTH_NO_PSOLA").is_some();
    // `LESYNTH_FORCE_PSOLA=1` runs the resynthesis at the source's own pitch
    // too, where the accumulator normally wins. Nothing is transposed there, so
    // the output should be the source back again — which makes it the probe for
    // what the grain pipeline itself loses, with transposition out of the way.
    let unity = unity && std::env::var_os("LESYNTH_FORCE_PSOLA").is_none();
    if let Some(t) = timing.filter(|_| !unity && !forced_off) {
        return render_psola(
            ampl,
            phase,
            ampl_enabled,
            phase_enabled,
            t,
            max_h,
            target_samples,
            cancel,
        );
    }

    loop {
        if drive_by_time && sound.len() >= target_samples {
            break;
        }

        // ── Which bucket, and how long is its cycle ──────────────────────────
        let next = match walk {
            // Source-timed: the bucket is where we are *along the source*, and
            // its cycle is its own period. The two advance separately — that is
            // what lets a key hold the source's duration while playing another
            // pitch — and at the source's own pitch they coincide exactly, which
            // is what makes this render the exact inverse there.
            Some(_) => Some((along as usize).min(num_buckets - 1)),
            None => {
                let cycle = cycles as usize;
                if cycle == last_cycle {
                    None
                } else {
                    last_cycle = cycle;
                    if drive_by_time {
                        let t = sound.len() as f64 / target_samples as f64;
                        Some(((t * num_buckets as f64) as usize).min(num_buckets - 1))
                    } else if cycle >= num_buckets {
                        break;
                    } else {
                        Some(cycle)
                    }
                }
            }
        };
        if let Some(b) = next.filter(|&b| b != bucket) {
            bucket = b;
            period = match timing {
                Some(t) => t.periods[b].max(2.0),
                None => bucket_period(base_period, ratios, b),
            };
            // The bucket's own mean. Dropping it (as this renderer used to) puts
            // a step at every bucket boundary — worth 41 dB of the exact
            // inverse's fidelity on a measured tone, and a step at the bucket
            // rate is heard as buzz, not as a level error.
            dc = timing.and_then(|t| t.dc.get(b).copied()).unwrap_or(0.0);
            // The grid's harmonics have the bucket's wrap ramp taken out of
            // them, so it is added back below — see [`PlaybackGrid::ramp`].
            ramp = timing.and_then(|t| t.ramp.get(b).copied()).unwrap_or(0.0);
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

        // Position within the cycle. One running phase for the whole note: the
        // buckets of a `PlaybackGrid` are rotated to share it, so a bucket change
        // needs no re-alignment and lands no step.
        let pos = (cycles - cycles.floor()) as f32;
        let sample =
            cache.sample(ampl, phase, ampl_enabled, phase_enabled, bucket, pos, max_h, 0);
        sound.push((sample + dc + ramp * pos).clamp(-1.0, 1.0));

        cycles += 1.0 / period as f64;
        if let Some(t) = walk {
            // Past the last bucket, hold it: the analysed material can fall a
            // fraction of a period short of the duration, and stopping there
            // would leave the note shorter on some keys than others.
            along += 1.0 / t.spans[bucket].max(1.0) as f64;
        }
    }
    sound
}

/// Synthesise a key by **pitch-synchronous overlap-add**: one grain per output
/// period, each laid on the period boundary it came from, Hann-windowed two
/// periods wide and overlapped at 50%.
///
/// This replaces a phase accumulator that read whichever bucket the wall clock
/// pointed at, sample by sample. That is only coherent while the renderer's
/// phase advances at the source's own rate — its own pitch, on the wall clock —
/// because each bucket's phases are pre-rotated by the phase the *source* had
/// reached there. On any other key the two clocks separate and a single
/// rendered cycle gets spliced out of several buckets (four of them, two
/// octaves down), each read at a phase its rotation does not describe. The
/// result had the right harmonic amplitudes and the wrong harmonic phases,
/// once per cycle, right through the formants: measured 18 dB worse than a
/// linear-interpolation resampler, and audible on every key but the source's
/// own pitch.
///
/// A grain fixes it by being self-contained. It is one bucket's waveform read
/// from *its* phase origin (hence [`PlaybackGrid::rotations`]), so no rotation
/// has to survive a bucket change, and the change itself becomes a cross-fade
/// over one period instead of a splice. Grain spacing is the key's period, so
/// transposing up repeats grains and down skips them — which is what PSOLA
/// does, and it stays coherent at any ratio.
///
/// Hann windows two periods wide at one period's hop sum to 1, but the period
/// moves with the source's pitch, so the window sum is accumulated and divided
/// out rather than assumed.
///
/// `target_samples`: `> 0` walks the source's spans across that many samples
/// ("preserve seconds"); `0` lays one grain per bucket, the Synth timeline.
#[allow(clippy::too_many_arguments)]
fn render_psola(
    ampl: &[Vec<f32>],
    phase: &[Vec<f32>],
    ampl_enabled: &[bool],
    phase_enabled: &[bool],
    timing: &BucketTiming,
    max_h: usize,
    target_samples: usize,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Vec<f32> {
    let nb = timing.periods.len();
    if nb == 0 || max_h == 0 {
        return Vec::new();
    }
    let drive_by_time = target_samples > 0;

    // Where each bucket sits on the source's own clock, in output samples.
    let mut cum = Vec::with_capacity(nb + 1);
    let mut acc = 0.0f64;
    cum.push(0.0f64);
    for &s in timing.spans {
        acc += (s as f64).max(1.0);
        cum.push(acc);
    }
    let total = if drive_by_time {
        target_samples as f64
    } else {
        timing.periods.iter().map(|&p| (p as f64).max(2.0)).sum()
    };
    if !(total >= 2.0) || acc <= 0.0 {
        return Vec::new();
    }
    let n_out = total.ceil() as usize;
    let mut out = vec![0.0f32; n_out];
    let mut wsum = vec![0.0f32; n_out];

    // How much of a period the grains cross-fade over. **None, by default.**
    //
    // A cross-fade is for hiding a join, and there is no longer a join to hide.
    // Consecutive grains are consecutive periods of the source — cut where they
    // are played (`build_playback_grid` advances by the true period) and closed
    // in value (`PlaybackGrid::ramp`) — so grain `b` ends on exactly the sample
    // grain `b+1` starts on. What the fade does instead is average each period
    // with its neighbour, and a voice's periods genuinely differ: that average
    // is a loss, and it is the largest one left.
    //
    // Measured against the render this method is trying to produce (one true
    // source period per output period, read straight off the exact inverse —
    // `tools/psolaref.py` in gemstone-daw), on my_voice.m4a:
    //
    //                     preserve seconds   synth timeline
    //     overlap 0.15        -37.6 dB           -37.8 dB
    //     overlap 0.05        -48.4 dB           -49.8 dB
    //     overlap 0           -64.2 dB           -64.1 dB
    //
    // and independently, against a true band-limited resampling of the same
    // voice (`tools/resampcmp.py`, which knows nothing about grains): -36.9 dB
    // at 0.15 against -42.8 at zero, with the 4-6 kHz residual falling from
    // -8.8 dB relative to the band to -23.0.
    //
    // The earlier figure of 0.15 was measured before the cut was placed where it
    // is played and before the loop was closed, when the joins really did not
    // join and the fade was covering for them.
    const DEFAULT_OVERLAP: f64 = 0.0;
    let overlap: f64 = std::env::var("LESYNTH_OVERLAP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_OVERLAP)
        .clamp(0.0, 1.0);
    // Which bucket the grain laid at `tau` (the `epoch`-th) comes from. Driven by
    // time, the spans are stretched onto the note's length so the material still
    // lasts the source's own seconds; on the Synth timeline it is one grain each.
    let bucket_at = |tau: f64, epoch: usize| -> Option<usize> {
        if drive_by_time {
            let along = tau / total * acc;
            Some(match cum.binary_search_by(|c| c.partial_cmp(&along).unwrap()) {
                Ok(i) => i.min(nb - 1),
                Err(i) => i.saturating_sub(1).min(nb - 1),
            })
        } else if epoch >= nb {
            None
        } else {
            Some(epoch)
        }
    };

    // Where each bucket's waveform starts, relative to the first — the running
    // sum of the wrap ramps, since `ramp[b]` is exactly `h(b+1)(0) - h(b)(0)`.
    // A grain's own ramp is then the difference between where it starts and
    // where the *next* grain starts, which is what makes a splice join.
    let mut origin = Vec::with_capacity(nb + 1);
    let mut acc_ramp = 0.0f32;
    origin.push(0.0f32);
    for b in 0..nb {
        acc_ramp += timing.ramp.get(b).copied().unwrap_or(0.0);
        origin.push(acc_ramp);
    }

    let mut cache = CycleCache::default();
    let mut tau = 0.0f64; // the current epoch, in output samples
    let mut epoch = 0usize;
    let mut last_yield = 0usize;

    while tau < total {
        let Some(b) = bucket_at(tau, epoch) else { break };

        if let Some(c) = cancel {
            if c.load(Ordering::Relaxed) {
                return Vec::new();
            }
            if tau as usize - last_yield >= 8192 {
                thread::sleep(Duration::from_millis(1));
                last_yield = tau as usize;
            }
        }

        let p = (timing.periods[b] as f64).max(2.0);
        let dc = timing.dc.get(b).copied().unwrap_or(0.0);
        // The ramp bridges this grain's start value to the **next grain's**,
        // which is the bucket after this one only while the key is playing the
        // source's own periods in order.
        //
        // It is not, wherever the key's period made the renderer skip a bucket
        // (transposing down) or repeat one (transposing up). Bridging to `b + 1`
        // there leaves the grain ending on material the next grain does not
        // begin with — a step of about one ramp, at a *splice*, and the only
        // joins where a step can appear at all. Measured on my_voice at 65 Hz,
        // where 65% of the joins are splices, against the same cubic-corner
        // measure taken at samples that are not joins at all (`tools/joinstep.py`
        // in gemstone-daw — the control matters, because at a high key a cubic
        // through four samples reads large everywhere):
        //
        //     no join      rms 0.0094      continuation rms 0.0040
        //     splice       rms 0.0383  ->  four times the control
        //
        // Bridging to the bucket actually played next makes every join
        // continuous by construction: a repeat gets no ramp at all (the grain is
        // a closed loop, which is exactly what repeating a period means) and a
        // skip gets the ramp across everything it skipped.
        let next_b = bucket_at(tau + p, epoch + 1).unwrap_or((b + 1).min(nb - 1));
        let ramp = origin[next_b.min(nb)] - origin[b];

        // The grain spans one period either side of its epoch. `x` is the
        // position within it in cycles, zero at the epoch, ±1 at the edges,
        // which is both the Hann argument and the phase to read the bucket at.
        let half = overlap * 0.5;
        let first = (tau - half * p).ceil() as i64;
        let last = (tau + (1.0 + half) * p).floor() as i64;
        for idx in first..=last {
            if idx < 0 || idx as usize >= n_out {
                continue;
            }
            let x = (idx as f64 - tau) / p;
            // Half-open, deliberately: a sample landing exactly on an epoch
            // belongs to the grain that starts there and not to the one that
            // ends there. With `x <= -half` and no fade it belonged to neither,
            // so its window sum stayed zero and it came out as a hole — every
            // grain boundary, whenever the key's period is a whole number.
            if x < -half || x >= 1.0 + half {
                continue;
            }
            // Tukey: raised-cosine ramps of width `overlap`, flat between them.
            // Consecutive grains' ramps are mirror images, so they sum to one.
            let w = if half <= 0.0 {
                1.0
            } else if x < half {
                0.5 * (1.0 - (std::f64::consts::PI * (x + half) / overlap).cos())
            } else if x > 1.0 - half {
                0.5 * (1.0 + (std::f64::consts::PI * (x - 1.0 + half) / overlap).cos())
            } else {
                1.0
            };
            // Read at the grain's own phase. The bucket's phases were baked with
            // its absolute position subtracted (`rot` in `build_playback_grid`),
            // so the table already *is* the source's waveform referenced to a
            // shared origin: reading at zero returns the waveform at absolute
            // phase zero, whatever bucket it came from. That is exactly what a
            // grain wants, and it is why no rotation is undone here — adding
            // `rot` back rotates each grain by a drifting amount and cost 33 dB.
            let pos = x.rem_euclid(1.0) as f32;
            let sample =
                cache.sample(ampl, phase, ampl_enabled, phase_enabled, b, pos, max_h, 0);
            // The ramp uses the *unwrapped* `x`, not `pos`. Where the grain
            // overlaps its neighbour, `x` runs past 1 (or below 0) and the two
            // grains must agree about the material there: bucket `b`'s cut ends
            // exactly where bucket `b+1`'s begins, which in these coordinates is
            // `h_b(0) + ramp_b` — so the ramp has to keep climbing across the
            // join while the waveform wraps. Restarting it with `pos` puts the
            // whole step back into the middle of every cross-fade, and measures
            // as if the ramp were not there at all.
            out[idx as usize] += w as f32 * (sample + dc + ramp * x as f32);
            wsum[idx as usize] += w as f32;
        }

        tau += p;
        epoch += 1;
    }

    // Divide the window sum back out. At a steady pitch it is 1 by construction;
    // it moves where the period does, and at the very ends only one grain has
    // landed, which would otherwise fade the note in and out.
    for (v, w) in out.iter_mut().zip(&wsum) {
        if *w > 1e-3 {
            *v /= *w;
        }
        *v = v.clamp(-1.0, 1.0);
    }
    out
}

/// How a key's render is laid out over the source it came from. Every field is
/// per bucket and in *output* samples.
///
/// Two clocks, deliberately separate:
///
/// * `periods` is the pitch — the bucket's **true** period ([`PlaybackGrid`]),
///   transposed onto the key. Not its recorded length: that is the true period
///   rounded to a whole sample, and a key that renders the rounding hears it as
///   pitch.
/// * `spans` is the clock — how long the bucket occupies, which is the source's
///   own duration and therefore the same on every key ("preserve seconds").
///
/// At the source's own pitch the two run together (they differ only by each
/// bucket's rounding, and not at all on average). Transposed, they separate by
/// the transposition: the render walks the source at wall-clock speed while the
/// waveform runs at the key's pitch. One running phase carries the whole note —
/// the grid's buckets are rotated to share it, so no bucket change re-references
/// it, and it is that re-referencing which used to drop the rounding of every
/// bucket at the bucket rate and buzz where the Original Pitch audition did not.
struct BucketTiming<'a> {
    periods: &'a [f32],
    spans: &'a [f32],
    /// Per-bucket mean (FFT bin 0), on the normalised grid's scale.
    dc: &'a [f32],
    /// Per-bucket wrap ramp — see [`PlaybackGrid::ramp`]. The stored harmonics
    /// have it taken out, so every renderer reading this grid has to put it
    /// back, as `ramp * position-in-cycle`.
    ramp: &'a [f32],
    /// Each bucket's baked phase origin — see [`PlaybackGrid::rotations`].
    rotations: &'a [f32],
    /// The source's own bandwidth — see [`PlaybackGrid::usable_harmonics`].
    usable_harmonics: usize,
}

impl BucketTiming<'_> {
    fn describes(&self, num_buckets: usize) -> bool {
        self.periods.len() == num_buckets
            && self.spans.len() == num_buckets
            && self.rotations.len() == num_buckets
            && self.ramp.len() == num_buckets
    }
}

/// A bucket's waveform, kept between samples so the inverse FFT behind it is
/// paid once per bucket rather than once per sample.
///
/// Two slots, because the source-timed render always reads a bucket and its
/// successor (it morphs between them). Slot 0 holds the current bucket, slot 1
/// the next; advancing one bucket therefore re-uses slot 1's table, so the cost
/// stays at one transform per bucket.
struct CycleCache {
    bank: IfftBank,
    table: [Vec<f32>; 2],
    /// `usize::MAX` until a table is built, so bucket 0 is not mistaken for one
    /// already cached.
    bucket: [usize; 2],
    len: [usize; 2],
}

impl Default for CycleCache {
    fn default() -> Self {
        Self {
            bank: IfftBank::default(),
            table: [Vec::new(), Vec::new()],
            bucket: [usize::MAX; 2],
            len: [0; 2],
        }
    }
}

impl CycleCache {
    /// The bucket's waveform at cycle position `pos`, from `max_h` harmonics.
    #[allow(clippy::too_many_arguments)]
    fn sample(
        &mut self,
        ampl: &[Vec<f32>],
        phase: &[Vec<f32>],
        ampl_enabled: &[bool],
        phase_enabled: &[bool],
        bucket: usize,
        pos: f32,
        max_h: usize,
        slot: usize,
    ) -> f32 {
        if max_h == 0 {
            return 0.0;
        }
        if max_h <= IFFT_MIN_HARMONICS {
            // Direct sinusoid sum — cheaper than a table for few harmonics, and
            // exact (no interpolation).
            let mut acc = 0.0f32;
            for n in 0..max_h {
                if !ampl_enabled[n] {
                    continue;
                }
                let a = ampl[n][bucket];
                if a == 0.0 {
                    continue;
                }
                let ph = if phase_enabled[n] { phase[n][bucket] } else { 0.0 };
                acc += a * (TWO_PI * (n as f32 + 1.0) * pos + ph).sin();
            }
            return acc;
        }
        // Fast path: one inverse real-FFT per bucket, then fractional readout.
        let len = cycle_table_len(max_h);
        if self.bucket[slot] != bucket || self.len[slot] != len {
            // Advancing a bucket makes the old "next" the new "current", so take
            // that table over rather than transforming it again.
            let other = 1 - slot;
            if self.bucket[other] == bucket && self.len[other] == len {
                self.table.swap(0, 1);
                self.bucket.swap(0, 1);
                self.len.swap(0, 1);
            } else {
                build_cycle_table(
                    &mut self.bank,
                    &mut self.table[slot],
                    ampl,
                    phase,
                    ampl_enabled,
                    phase_enabled,
                    bucket,
                    len,
                    max_h,
                );
                self.bucket[slot] = bucket;
                self.len[slot] = len;
            }
        }
        read_cycle(&self.table[slot], pos)
    }
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
///   exact). The harmonic toggles below do not touch them: they are not
///   harmonics and the toggle grid does not list them.
/// * `ampl_enabled` / `phase_enabled` – the per-harmonic checkboxes, indexed by
///   `harmonic - 1` like the grid rows. A disabled amplitude drops that partial;
///   a disabled phase renders it at phase 0 — the same meaning they carry in
///   [`render_key_buffer`], so the audition and the keys agree. An **empty**
///   slice means "all enabled", for callers with no flags of their own (the host
///   bridge, tests).
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
    ampl_enabled: &[bool],
    phase_enabled: &[bool],
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
            // Empty flags = every harmonic enabled, so `unwrap_or(true)`.
            if !ampl_enabled.get(k - 1).copied().unwrap_or(true) {
                continue;
            }
            let a = amplitude[k - 1][b];
            if a == 0.0 {
                continue;
            }
            let ph = if phase_enabled.get(k - 1).copied().unwrap_or(true) {
                phase[k - 1][b]
            } else {
                0.0
            };
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

/// The grid a key transposes from: the same harmonics as the analysis, but
/// measured over the source's **true** period instead of the whole number of
/// samples that period rounds to, and pre-rotated so every bucket shares one
/// phase origin.
///
/// Why it exists. A bucket is a whole number of samples and a period is not
/// (150 samples for a true 149.83), so the bucket's bins are those of a window
/// that does not close. That costs the exact inverse nothing — the same bins put
/// those very samples back — but a key does not play the samples, it plays the
/// bins *as a periodic waveform*, and a window that does not close spreads every
/// harmonic across its neighbours. The spreading depends on where the rounding
/// fell, so it differs from bucket to bucket, and what a key hears is that
/// difference arriving at the bucket rate: a few hundred Hz of roughness, the
/// buzz that the Original Pitch audition of the same grid does not have.
/// Measured on a steady 12-harmonic tone whose period is 149.83 samples, against
/// the exact transposition of it: **−27 dB** from the bucket's own bins, **−74
/// dB** from a true period of the reconstruction. (A tone whose period happened
/// to be a whole 400 samples measured −100 dB either way — the giveaway that the
/// rounding, not the renderer, was the defect.)
///
/// So: reconstruct the source exactly (that reconstruction is what Original
/// Pitch And Gain plays), cut one *fractional* period out of it per bucket, and
/// transform that. Every bucket then holds a waveform that closes, and
/// neighbouring buckets differ only by what the source actually did.
pub struct PlaybackGrid {
    /// `[harmonic][bucket]`, normalised like the playback grid it replaces.
    pub amplitude: Vec<Vec<f32>>,
    /// `[harmonic][bucket]`, with each bucket's rotation against the shared
    /// phase origin already applied, so the renderer reads every bucket at the
    /// same running phase and switching between them is seamless.
    pub phase: Vec<Vec<f32>>,
    /// Per-bucket mean, on the same scale as `amplitude`.
    pub dc: Vec<f32>,
    /// Per-bucket **wrap ramp**, on the same scale as `amplitude`: how far the
    /// bucket's last sample sits from its first, subtracted as a straight line
    /// before the transform and added back as one at render time.
    ///
    /// A bucket is one period cut out of a real recording, and consecutive
    /// periods of a voice are not identical — the cut therefore does not close,
    /// and its *periodic* extension steps by this much at every wrap. A harmonic
    /// series is a periodic basis, so it can only answer that step with Gibbs
    /// ringing, and the ringing lands exactly on the grain boundary: one
    /// impulse, one to two samples wide, of random sign, **once per rendered
    /// period** — a buzz, and the one the exact inverse never shows because it
    /// only ever samples each bucket at the integer points where the ringing is
    /// not evaluated.
    ///
    /// Taking the straight line out first makes the stored waveform genuinely
    /// periodic; putting it back as a line reproduces the material exactly and
    /// costs no ringing at all. It also makes consecutive grains join in value
    /// by construction, since bucket `b`'s line ends where bucket `b+1`'s
    /// material starts. Measured against a true resampling of the same voice at
    /// 110 Hz: the per-period impulse falls from 0.0086 to 0.0013 and the
    /// 4-6 kHz residual from −4.9 to −23.5 dB relative to the band.
    pub ramp: Vec<f32>,
    /// The true period, in source samples — what one cycle of a bucket is.
    pub periods: Vec<f32>,
    /// The bucket's wall-clock span, in source samples: its recorded length.
    pub spans: Vec<f32>,
    /// The highest harmonic the *source* actually carries.
    ///
    /// Above a recording's own bandwidth there is only noise, and the analysis
    /// fits it into harmonics like anything else. One period of noise rendered
    /// as a periodic waveform is not noise any more: it repeats identically
    /// every cycle and adds coherently into a stable comb at the top of the
    /// band, which is heard as a fizz that no amount of work on the renderer can
    /// remove. On my_voice.m4a — lossy, so lowpassed at 9 kHz — the render sat
    /// **16 dB above** a true transposition between 10 and 11.4 kHz, in a band
    /// where the source is 107 dB down.
    pub usable_harmonics: usize,
    /// The phase origin baked into this bucket's phases: the fractional part of
    /// how many fundamental cycles the source had run when the bucket starts.
    ///
    /// Reading the bucket's waveform at this position returns its own phase
    /// zero — the period boundary. PSOLA needs exactly that, to lay each grain
    /// on the boundary it came from.
    pub rotations: Vec<f32>,
    /// What [`normalize_grid_per_bucket`] divided this grid by while building
    /// it. A key plays the normalised grid and does not care, but a caller
    /// reproducing the source's own level has to multiply it back in, and it is
    /// not the analysis grid's divisor — the two grids differ.
    pub norm_divisor: f32,
}

/// The true period of each bucket, in source samples.
///
/// Shape from the pitch contour (`ratios`, the local fundamental over the
/// nominal one — the only unrounded pitch the analysis kept), scale from the
/// lengths, which tile the source exactly: whatever the contour's absolute
/// calibration, the buckets *are* the periods, so their total is the total.
/// A flat contour therefore yields the mean bucket length, which is the true
/// period of a steady source to a small fraction of a sample.
///
/// The last bucket is left out of the scale: it absorbs the subtrack's
/// remainder, so its length is not a period.
fn true_periods(lengths: &[usize], ratios: &[f32]) -> Vec<f32> {
    let n = lengths.len();
    let shape: Vec<f32> = (0..n)
        .map(|b| 1.0 / ratios.get(b).copied().unwrap_or(1.0).max(1e-3))
        .collect();
    let upto = if n > 1 { n - 1 } else { n };
    let total_len: f32 = lengths[..upto].iter().map(|&l| l as f32).sum();
    let total_shape: f32 = shape[..upto].iter().sum();
    let scale = if total_shape > 0.0 { total_len / total_shape } else { 1.0 };
    shape.iter().map(|s| (s * scale).max(2.0)).collect()
}

/// Build the [`PlaybackGrid`] for an analysed grid, or `None` when the analysis
/// did not record what it takes (a hand-drawn grid, or a `.lsft` from before
/// version 3) — the caller then transposes from the stored grid as before.
pub fn build_playback_grid(
    amplitude: &[Vec<f32>],
    phase: &[Vec<f32>],
    lengths: &[usize],
    dc: &[f32],
    nyq: &[f32],
    ratios: &[f32],
) -> Option<PlaybackGrid> {
    let nb = lengths.len();
    let num_harmonics = amplitude.len();
    if nb == 0 || num_harmonics == 0 || amplitude[0].len() != nb || lengths.iter().any(|&l| l < 2) {
        return None;
    }
    // The exact inverse, on the grid's own scale and at the analysis rate: no
    // display gain to undo and no rate to convert, because nothing here leaves
    // the source's own timebase. Toggles are deliberately not applied — they are
    // a per-key render-time edit, and applying them twice would zero a phase
    // that this transform has already folded into a waveform.
    let source = resynthesize_exact(amplitude, phase, lengths, dc, nyq, &[], &[], 0.0, 1.0);
    if source.is_empty() {
        return None;
    }

    let periods = true_periods(lengths, ratios);
    let kernel = resample_kernel();
    let mut planner = RealFftPlanner::<f32>::new();
    let mut out_amp = vec![vec![0.0f32; nb]; num_harmonics];
    let mut out_phase = vec![vec![0.0f32; nb]; num_harmonics];
    let mut out_dc = vec![0.0f32; nb];
    let mut out_ramp = vec![0.0f32; nb];

    let mut start = 0.0f64; // where this bucket begins in the source
    let mut rotations = vec![0.0f32; nb];
    for b in 0..nb {
        let t = periods[b] as f64;
        // One cycle sampled over the true period. Sized to hold every harmonic
        // the period can carry, so nothing is lost before the key's own
        // anti-alias cap gets to choose.
        let top = num_harmonics.min(((t as usize).saturating_sub(1)) / 2).max(1);
        let n = (2 * (top + 1)).max(32).next_power_of_two();
        let fft = planner.plan_fft_forward(n);
        let mut cycle: Vec<f32> = (0..n)
            .map(|i| sinc_read(&source, &kernel, start + (i as f64 / n as f64) * t))
            .collect();
        // Close the loop. The cut runs from one period boundary to the next, and
        // the material at the far end is the *next* bucket's first sample — not
        // this one's, because a voice's periods differ. Left in, that step is a
        // discontinuity in the periodic extension this transform is about to
        // build, and the only thing a harmonic series can do with a step is ring
        // at it. See [`PlaybackGrid::ramp`].
        let ramp = if start + t < source.len() as f64 {
            sinc_read(&source, &kernel, start + t) - cycle[0]
        } else {
            0.0
        };
        if ramp != 0.0 {
            for (i, v) in cycle.iter_mut().enumerate() {
                *v -= ramp * (i as f32 / n as f32);
            }
        }
        out_ramp[b] = ramp;
        let mut spectrum = fft.make_output_vec();
        if fft.process(&mut cycle, &mut spectrum).is_err() {
            return None;
        }
        out_dc[b] = spectrum[0].re / n as f32;
        for k in 1..=top.min(spectrum.len() - 1) {
            let x = spectrum[k];
            out_amp[k - 1][b] = 2.0 * (x.re * x.re + x.im * x.im).sqrt() / n as f32;
            // `analyze_subtrack`'s convention: the DFT's cosine reference turned
            // into the renderer's sine one, then rotated back to the shared phase
            // origin so every bucket reads at the same running phase.
            let ph = x.im.atan2(x.re) + std::f32::consts::FRAC_PI_2;
            // Only the *fraction* of a cycle matters: a whole cycle rotates
            // every harmonic by a multiple of 2π. Keeping the whole part costs
            // precision that grows through the note — `rot` reaches the bucket
            // count (267 on a 2.5 s voice), and `2π·k·rot` for the top harmonic
            // is then a six-figure f32 whose last bits are worth ~0.01 rad. The
            // error is zero at the start and largest at the end, which is heard
            // as a fuzz that comes in partway through and gets worse.
            // `rot` is a whole number of cycles by construction (one period per
            // bucket), and a whole cycle rotates every harmonic by a multiple of
            // 2π, so there is nothing to subtract.
            out_phase[k - 1][b] = ph;
        }
        // Advance by the **true** period, not by the recorded length.
        //
        // The two differ by the rounding the length carries, and the renderer
        // places its grains a true period apart. Cutting them a recorded length
        // apart therefore hands each grain material from a slightly different
        // place than where it is played, by an amount that walks through the
        // note — so consecutive grains no longer join, and every join is a step.
        // Steps at the period rate are broadband: the 4-6 kHz band, where this
        // voice is 36 dB down, came out *above* the source and got worse toward
        // the end of the note, which is where the walk is largest.
        //
        // Advancing by `t` makes the cut and the placement the same thing.
        // Consecutive grains are then consecutive periods of the source, they
        // join exactly, and each bucket begins a whole cycle after the last —
        // so the phase origin below is an integer and the rotation vanishes.
        rotations[b] = 0.0;
        start += t;
    }

    // Where the source's own spectrum ends. A recording rolls off smoothly or
    // falls off a cliff (a codec's lowpass); either way the last harmonic that
    // carries signal is the last one whose mean rises above a floor far below
    // the loudest. 60 dB is deep enough to keep a natural rolloff and shallow
    // enough to catch a cliff.
    let mut mean = vec![0.0f32; num_harmonics];
    for (n, m) in mean.iter_mut().enumerate() {
        *m = out_amp[n].iter().copied().sum::<f32>() / nb as f32;
    }
    let floor = mean.iter().copied().fold(0.0f32, f32::max) * 1e-3;
    // Keep a few harmonics past the edge. A real top harmonic leaks into its
    // neighbours, and cutting flush loses part of it — worth 10 dB against an
    // analytic ideal on a 12-harmonic tone. The noise band this exists to
    // remove is tens of harmonics wide, so a small margin costs nothing there.
    const BANDWIDTH_MARGIN: usize = 4;
    let usable_harmonics = mean
        .iter()
        .rposition(|&m| m > floor)
        .map(|i| i + 1 + BANDWIDTH_MARGIN)
        .unwrap_or(num_harmonics)
        .clamp(1, num_harmonics);

    let divisor = normalize_grid_per_bucket(&mut out_amp);
    if divisor > 0.0 {
        for v in out_dc.iter_mut() {
            *v /= divisor;
        }
        for v in out_ramp.iter_mut() {
            *v /= divisor;
        }
    }
    let spans_out = periods.clone();
    Some(PlaybackGrid {
        amplitude: out_amp,
        phase: out_phase,
        dc: out_dc,
        ramp: out_ramp,
        periods,
        // The bucket now occupies exactly its own true period of the source, so
        // that is its wall-clock span too.
        spans: spans_out,
        rotations,
        usable_harmonics,
        norm_divisor: divisor,
    })
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

/// The Kaiser-windowed sinc itself, sampled on `[0, RESAMPLE_TAPS]` in tap
/// units, with one extra entry so a reader can always interpolate `k + 1`.
/// Shared by [`resample_stream`] and [`sinc_read`].
fn resample_kernel() -> Vec<f64> {
    let beta = 10.0;
    let norm = bessel_i0(beta);
    (0..=RESAMPLE_TAPS * RESAMPLE_KERNEL_STEPS + 1)
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
        .collect()
}

/// Read `input` between its samples at `pos`, band-limited — the same kernel
/// [`resample_stream`] uses, at one point instead of a stream, and at full
/// bandwidth (the sample grid moves, the content does not).
///
/// Used to cut a bucket's true period out of the reconstruction, where a cubic
/// read is not good enough: a period of real material carries content up to its
/// own Nyquist, and Catmull-Rom's error there is broadband — it measured 12 dB
/// worse on a key transposed 19 semitones up.
fn sinc_read(input: &[f32], table: &[f64], pos: f64) -> f32 {
    let half = RESAMPLE_TAPS as f64;
    let first = (pos - half).ceil().max(0.0) as usize;
    let last = ((pos + half).floor() as i64).min(input.len() as i64 - 1);
    let mut acc = 0.0f64;
    for i in first as i64..=last {
        let x = (pos - i as f64).abs() * RESAMPLE_KERNEL_STEPS as f64;
        let k = x as usize;
        if k + 1 >= table.len() {
            continue;
        }
        let f = x - k as f64;
        let w = table[k] + (table[k + 1] - table[k]) * f;
        acc += input[i as usize] as f64 * w;
    }
    acc as f32
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
    let table = resample_kernel();

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
        // The bridge passes a contour, not the source's own lengths — it renders
        // what a host asked for, not a loaded instance's analysis.
        None,
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

/// Render a key **the way the keyboard does** — through a `PlaybackGrid` and the
/// source's own two clocks — with no live engine, so an offline tool measures
/// the path a listener actually hears.
///
/// [`resynthesize_grid`] cannot do this: the host bridge it serves passes a
/// pitch contour and no bucket lengths, so it renders on a uniform time grid
/// from the analysis grid's rounded buckets. That is a different signal from
/// what a key plays, and measuring it is how a keyboard defect stays invisible
/// to an offline dump.
///
/// * `lengths`/`dc`/`nyq` – the analysis's own, exactly as
///   [`resynthesize_exact`] takes them (source samples).
/// * `analysis_rate`/`out_rate` – to place the source's spans on the output
///   clock; equal rates keep them as they are.
/// * `base_period` – the key's period in **output** samples, fractional.
/// * `base_freq` – the analysis fundamental, to scale each bucket's true period
///   onto the key.
///
/// Falls back to [`resynthesize_grid`] when the grid carries no usable lengths,
/// which is exactly when a key does too.
#[allow(clippy::too_many_arguments)]
pub fn resynthesize_key(
    amplitude: &[Vec<f32>],
    phase: &[Vec<f32>],
    lengths: &[usize],
    dc: &[f32],
    nyq: &[f32],
    pitch_ratio: &[f32],
    base_period: f32,
    base_freq: f32,
    analysis_rate: f32,
    out_rate: f32,
    max_harmonic: usize,
    target_samples: usize,
    display_gain: f32,
) -> Vec<f32> {
    let num_harmonics = amplitude.len();
    let num_buckets = amplitude.first().map(|r| r.len()).unwrap_or(0);
    if num_harmonics == 0 || num_buckets == 0 {
        return Vec::new();
    }
    let nominal = analysis_rate / base_freq.max(1e-6);
    let grid = if lengths.len() == num_buckets
        && base_freq > 0.0
        && analysis_rate > 0.0
        && out_rate > 0.0
        && nominal >= 2.0
    {
        build_playback_grid(amplitude, phase, lengths, dc, nyq, pitch_ratio)
    } else {
        None
    };
    let Some(grid) = grid else {
        return resynthesize_grid(
            amplitude,
            phase,
            pitch_ratio,
            base_period,
            max_harmonic,
            target_samples,
            display_gain,
        );
    };

    // The same two clocks `key_timing` derives for a live key, from the same
    // grid: the bucket's true period transposed onto the key, and its span put
    // on the output rate.
    let rate = out_rate / analysis_rate;
    let periods: Vec<f32> =
        grid.periods.iter().map(|&t| (base_period * t / nominal).max(2.0)).collect();
    let spans: Vec<f32> = grid.spans.iter().map(|&l| (l * rate).max(1.0)).collect();

    // `build_playback_grid` has already normalised, so re-normalising here would
    // find nothing to do and lose the divisor that restores the source's level.
    let enabled = vec![true; num_harmonics];
    let mut sound = render_key_buffer(
        num_harmonics,
        &grid.amplitude,
        &grid.phase,
        &enabled,
        &enabled,
        base_period.max(2.0),
        if max_harmonic == 0 { num_harmonics } else { max_harmonic },
        pitch_ratio,
        Some(&BucketTiming {
            periods: &periods,
            spans: &spans,
            dc: &grid.dc,
            ramp: &grid.ramp,
            rotations: &grid.rotations,
            usable_harmonics: grid.usable_harmonics,
        }),
        target_samples,
        None,
    );
    if let Some(scale) = source_level_scale(display_gain, grid.norm_divisor) {
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
        // An analysed grid's width is not a free parameter: it is one bucket per
        // period of the source, and the analysis's own bucket lengths, DC and
        // Nyquist rows are indexed by it. Resampling the rows alone leaves those
        // describing a grid that no longer exists, and `build_playback_grid`
        // answers a width mismatch with `None` — so every key would fall back to
        // the contour renderer, silently, and start buzzing again.
        //
        // The editor already disables the control while input sound is loaded
        // ("Locked: bucket count follows the loaded input sound"). This is the
        // same rule where the invariant actually lives, so no other caller can
        // break it either.
        if !self.shared_params.analysis_bucket_lengths.lock().unwrap().is_empty() {
            log::debug!(
                "set_num_buckets({new_buckets}) ignored: the grid width follows the \
                 analysed source"
            );
            return;
        }
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
        self.assemble_buffer_with_period(
            base_period,
            max_harmonic_for_key(key),
            // A key transposes, so the analysed phases no longer close the cycle
            // (see `SharedParams::zero_key_phases`).
            self.shared_params.zero_key_phases(),
        )
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
                // The per-harmonic checkboxes apply here too: the audition is
                // what the user judges an edit by, so a harmonic switched off in
                // the grid has to be absent from it, exactly as it is on a key.
                let ampl_enabled = self.shared_params.harmonic_ampl_enabled.lock().unwrap();
                let phase_enabled = self.shared_params.harmonic_phase_enabled.lock().unwrap();
                let mut sound = resynthesize_exact(
                    &amp,
                    &phase,
                    &lengths,
                    &dc,
                    &nyq,
                    &ampl_enabled,
                    &phase_enabled,
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
        // `zero_phases: false` — this is the source's own pitch, where the
        // analysed phases are exactly right and are the point of the audition.
        let mut sound =
            self.assemble_buffer_with_period(base_period.max(2.0), NUM_HARMONICS, false);
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
    ///
    /// `zero_phases` renders every bucket as if its phases were zero — the
    /// keyboard's cycle-continuity switch, see
    /// [`SharedParams::zero_key_phases`](crate::engine::shared_params::SharedParams::zero_key_phases).
    fn assemble_buffer_with_period(
        &self,
        base_period: f32,
        max_harmonic: usize,
        zero_phases: bool,
    ) -> Vec<f32> {
        let start_time = std::time::Instant::now();

        if *self.shared_params.normalization_needed.lock().unwrap() {
            self.normalize_amplitude_data();
            *self.shared_params.normalization_needed.lock().unwrap() = false;
        }

        // Before the grid locks below: building it reads the analysis grid.
        let playback = playback_grid(&self.shared_params);

        let num_harmonics = self.shared_params.amplitude_data.lock().unwrap().len();
        let ampl_data_normalized = self.shared_params.amplitude_data_normalized.lock().unwrap();
        let phase_data = self.shared_params.phase_data.lock().unwrap();
        // Per-bucket vibrato ratios apply only in Analysis mode; flat otherwise.
        // They are the fallback for a grid with no `PlaybackGrid` — a hand-drawn
        // one, or a `.lsft` from before version 3.
        let pitch_ratio = bucket_pitch_ratios(&self.shared_params);
        let timing = playback
            .as_ref()
            .and_then(|g| key_timing(g, &self.shared_params, base_period));
        // Hoist the per-harmonic enable flags out of the hot loops — locking
        // them per sample (as before) cost a mutex round-trip for every output
        // sample, making large analysis buffers crawl.
        let harmonic_ampl_enabled = self.shared_params.harmonic_ampl_enabled.lock().unwrap();
        let harmonic_phase_enabled = self.shared_params.harmonic_phase_enabled.lock().unwrap();
        // Zeroing the phases *is* switching every harmonic's phase off: the
        // renderer already substitutes 0.0 for a disabled harmonic's phase, on
        // both its direct-sum and inverse-FFT paths.
        let all_phases_off = vec![false; harmonic_phase_enabled.len()];
        let phase_enabled: &[bool] = if zero_phases {
            &all_phases_off
        } else {
            &harmonic_phase_enabled
        };

        // Synth mode: one period per bucket. Analysis mode: the source duration.
        let target_samples = target_samples_for(&self.shared_params);

        // With a `PlaybackGrid` the key transposes from *its* harmonics, not the
        // analysis grid's — that is the whole point of it.
        let (ampl, phase): (&[Vec<f32>], &[Vec<f32>]) = match (&playback, &timing) {
            (Some(g), Some(_)) => (&g.amplitude, &g.phase),
            _ => (&ampl_data_normalized, &phase_data),
        };
        // Record which of the two it was: falling back is silent, and it sounds
        // like the defect the grid exists to remove. This must be what the
        // *renderer* ends up using, not what is offered to it — reporting the
        // offer showed "true-period grid" while `render_key_buffer` discarded it
        // and rendered the rounded lengths anyway.
        self.shared_params
            .used_playback_grid
            .store(playback.is_some() && timing.is_some(), Ordering::Relaxed);
        let sound = render_key_buffer(
            num_harmonics,
            ampl,
            phase,
            &harmonic_ampl_enabled,
            phase_enabled,
            base_period,
            max_harmonic,
            &pitch_ratio,
            timing.as_ref().zip(playback.as_ref()).map(|((p, s), g)| BucketTiming {
                periods: p,
                spans: s,
                dc: &g.dc,
                ramp: &g.ramp,
                rotations: &g.rotations,
                usable_harmonics: g.usable_harmonics,
            }).as_ref(),
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

        // Before the grid locks below: building it reads the analysis grid. The
        // background thread is where this transform is paid for, most of the time.
        let playback = playback_grid(shared_params);

        // Copy all required data once and release locks immediately to avoid blocking GUI
        let (num_harmonics, ampl_data_copy, phase_data_copy, harmonic_ampl_enabled_copy, harmonic_phase_enabled_copy, base_period, pitch_ratio, timing, target_samples) = {
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
            // This is a key, so the "zero the phases" switch applies (see
            // `SharedParams::zero_key_phases`); switching every harmonic's phase
            // off is how the renderer is told to use 0.0 for all of them.
            let harmonic_phase_enabled_copy: Vec<bool> = if shared_params.zero_key_phases() {
                vec![false; harmonic_phase_enabled.len()]
            } else {
                harmonic_phase_enabled.clone()
            };
            // Per-bucket vibrato ratios (Analysis mode only; empty → flat) —
            // the fallback for a grid with no `PlaybackGrid`. Both are read here
            // so this path renders exactly what the synchronous one does.
            let pitch_ratio = bucket_pitch_ratios(shared_params);
            let timing = playback
                .as_ref()
                .and_then(|g| key_timing(g, shared_params, base_period));
            // Synth mode: one period per bucket. Analysis mode: source duration.
            let target_samples = target_samples_for(shared_params);
            (num_harmonics, ampl_data_copy, phase_data_copy, harmonic_ampl_enabled_copy, harmonic_phase_enabled_copy, base_period, pitch_ratio, timing, target_samples)
        }; // All locks are released here

        // With a `PlaybackGrid` the key transposes from *its* harmonics; the
        // copies above are the fallback for a grid that has none.
        let (ampl, phase): (&[Vec<f32>], &[Vec<f32>]) = match (&playback, &timing) {
            (Some(g), Some(_)) => (&g.amplitude, &g.phase),
            _ => (&ampl_data_copy, &phase_data_copy),
        };
        shared_params
            .used_playback_grid
            .store(playback.is_some() && timing.is_some(), Ordering::Relaxed);
        let sound = render_key_buffer(
            num_harmonics,
            ampl,
            phase,
            &harmonic_ampl_enabled_copy,
            &harmonic_phase_enabled_copy,
            base_period,
            max_harmonic,
            &pitch_ratio,
            timing.as_ref().zip(playback.as_ref()).map(|((p, s), g)| BucketTiming {
                periods: p,
                spans: s,
                dc: &g.dc,
                ramp: &g.ramp,
                rotations: &g.rotations,
                usable_harmonics: g.usable_harmonics,
            }).as_ref(),
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

        // **Only a `Clean` buffer is this grid's.** `Dirty` and `Computing` both
        // mean the grid moved after this buffer was rendered, and handing it to
        // a voice plays the previous sound — which is how loading a source and
        // pressing a key gave the default Synth patch's buzz while every
        // offline render of the analysed grid measured clean. Rendering here
        // costs a pause; playing the wrong instrument costs the user an
        // afternoon deciding the synthesis is broken.
        if buffer_states[key] == BufferState::Clean {
            if let Some(ref buffer) = key_buffers[key] {
                log::debug!("Using pre-computed buffer for key {}", key);
                return buffer.clone();
            }
        }

        drop(buffer_states);
        drop(key_buffers);
        log::debug!("Rendering key {key} synchronously: no buffer for the current grid");
        let sound = self.assemble_buffer_for_key(key);

        // Keep it, so this is paid once per key rather than once per press while
        // the background thread works through the other 87.
        if !sound.is_empty() {
            let mut buffer_states = self.shared_params.buffer_states.lock().unwrap();
            let mut key_buffers = self.shared_params.key_buffers.lock().unwrap();
            if buffer_states[key] != BufferState::Computing {
                key_buffers[key] = Some(sound.clone());
                buffer_states[key] = BufferState::Clean;
            }
        }
        sound
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

            let mut bank = IfftBank::default();
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
    ///
    /// The bridge is handed a grid and a contour and nothing else, so this is
    /// the case where playback has nothing more either: a grid with no recorded
    /// bucket lengths, which is what a hand-drawn grid and any pre-v3 `.lsft`
    /// are. **An analysed grid renders differently on a key** — it follows the
    /// source's own bucket periods ([`source_timing`]), which the bridge's
    /// arguments cannot express; carrying them across the ABI is what it would
    /// take to compare the two on analysed material.
    #[test]
    fn resynthesize_grid_matches_the_playback_path() {
        let engine = create_test_engine();
        let sr = 44_100.0;
        let f = 220.0;
        let samples = tone(sr, f, 0.4);
        engine.analyze_and_load(&samples, sr, f, &[], 0);
        // Drop the lengths, leaving the grid the bridge's arguments describe.
        engine.shared_params.analysis_bucket_lengths.lock().unwrap().clear();
        engine.shared_params.mark_all_buffers_dirty();

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

    /// A harmonic switched off in the grid must be absent from the audition.
    /// The exact-inverse path ignored the checkboxes — only the transposing
    /// renderer read them — so the button played harmonics the editor showed as
    /// disabled. `tone` carries harmonics 1-3, so each one is measurable on its
    /// own.
    #[test]
    fn original_pitch_audition_honours_the_harmonic_toggles() {
        let engine = create_test_engine();
        let sr = 44_100.0;
        let f = 220.0;
        // Like `tone`, but harmonic 2 sits at a phase of its own: buckets start
        // on a period boundary, so a source built from bare sines stores φ ≈ 0
        // everywhere and the phase toggle would have nothing to change.
        let src: Vec<f32> = {
            let n = (sr * 0.4) as usize;
            (0..n)
                .map(|i| {
                    let w = 2.0 * std::f32::consts::PI * f * i as f32 / sr;
                    0.6 * w.sin() + 0.3 * (2.0 * w + 1.2).sin() + 0.15 * (3.0 * w).sin()
                })
                .collect()
        };
        engine.shared_params.update_sample_rate(sr);
        engine.analyze_and_load(&src, sr, f, &[], 0);

        // Magnitude at `k·f`, correlated over a whole number of cycles so the
        // partials next door do not leak into the measurement.
        let level = |buf: &[f32], k: f64| -> f64 {
            let cycles = (buf.len() as f64 * k * f as f64 / sr as f64).floor();
            let n = (cycles * sr as f64 / (k * f as f64)).round() as usize;
            let (mut re, mut im) = (0.0f64, 0.0f64);
            for (i, &v) in buf[..n.min(buf.len())].iter().enumerate() {
                let w = 2.0 * std::f64::consts::PI * k * f as f64 * i as f64 / sr as f64;
                re += v as f64 * w.cos();
                im -= v as f64 * w.sin();
            }
            2.0 * (re * re + im * im).sqrt() / n as f64
        };

        let full = engine.assemble_buffer_at_original_pitch();
        assert!(!full.is_empty(), "the audition must render at all");

        engine.shared_params.harmonic_ampl_enabled.lock().unwrap()[1] = false;
        let muted = engine.assemble_buffer_at_original_pitch();

        let db = 20.0 * (level(&muted, 2.0) / level(&full, 2.0)).log10();
        assert!(db < -40.0, "harmonic 2 is still audible when disabled: {db:.1} dB");
        // Its neighbours are untouched — the toggle silences one row, not a band.
        for k in [1.0, 3.0] {
            let kept = 20.0 * (level(&muted, k) / level(&full, k)).log10();
            assert!(
                kept.abs() < 0.5,
                "disabling harmonic 2 moved harmonic {k} by {kept:+.2} dB"
            );
        }

        // The phase toggle is the other half of the requirement: it renders the
        // harmonic at phase 0, so the waveform changes while the level does not.
        engine.shared_params.harmonic_ampl_enabled.lock().unwrap()[1] = true;
        engine.shared_params.harmonic_phase_enabled.lock().unwrap()[1] = false;
        let flat = engine.assemble_buffer_at_original_pitch();
        let moved = flat
            .iter()
            .zip(&full)
            .fold(0.0f32, |m, (&a, &b)| m.max((a - b).abs()));
        assert!(
            moved > 0.01 * max_abs(&full),
            "disabling harmonic 2's phase changed nothing"
        );
        let kept = 20.0 * (level(&flat, 2.0) / level(&full, 2.0)).log10();
        assert!(
            kept.abs() < 0.5,
            "zeroing a phase must not change its level: {kept:+.2} dB"
        );
    }

    /// Load a one-harmonic grid whose phase alternates between buckets — the
    /// shape that steps at a period border, since a bucket's waveform is
    /// periodic and can only break where the phase *changes*.
    fn engine_with_alternating_phase(buckets: usize) -> SynthComputeEngine {
        let engine = create_test_engine();
        engine.shared_params.update_sample_rate(44_100.0);
        let amplitude = vec![vec![0.5f32; buckets]];
        let phase = vec![(0..buckets)
            .map(|b| if b % 2 == 0 { 0.0 } else { std::f32::consts::FRAC_PI_2 })
            .collect()];
        engine.load_grid(
            amplitude,
            phase,
            vec![1.0; buckets],
            220.0,
            0.2,
            44_100.0,
            0.9,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        );
        engine
    }

    /// Largest jump between neighbouring samples — a period border that does not
    /// join shows up here and nowhere else.
    fn max_step(buf: &[f32]) -> f32 {
        buf.windows(2).fold(0.0f32, |m, w| m.max((w[1] - w[0]).abs()))
    }

    /// The point of the switch: with the phases kept, a key steps at every
    /// bucket change; with them zeroed every harmonic is a sine of the
    /// fundamental, zero at both ends of the cycle, so the periods join.
    #[test]
    fn zeroing_key_phases_joins_the_period_borders() {
        let engine = engine_with_alternating_phase(16);
        let key = 24; // 110 Hz, ~401 samples per period at 44.1 kHz

        engine.shared_params.set_zero_key_phases(false);
        let stepped = engine.assemble_buffer_for_key(key);
        assert!(!stepped.is_empty(), "the key must render at all");
        let jump = max_step(&stepped);

        engine.shared_params.set_zero_key_phases(true);
        let joined = engine.assemble_buffer_for_key(key);
        let smooth = max_step(&joined);

        // A 0.5-amplitude fundamental at ~401 samples per period moves ~0.008 per
        // sample; the phase change is worth ~0.5, i.e. two orders of magnitude.
        assert!(
            jump > 0.3,
            "the phased render should step at the bucket changes, but its worst \
             step is only {jump:.4}"
        );
        assert!(
            smooth < 0.05,
            "zeroed phases must leave no step at a period border: {smooth:.4}"
        );
        // Same spectrum, so the note is still there at the same level.
        assert!(
            (max_abs(&joined) - max_abs(&stepped)).abs() < 0.05,
            "zeroing the phases must not change the level"
        );
    }

    /// The switch belongs to the keyboard alone: Original Pitch And Gain plays at
    /// the pitch the phases were measured at, where they are exactly right.
    #[test]
    fn zeroing_key_phases_leaves_the_original_pitch_audition_alone() {
        let engine = create_test_engine();
        let sr = 44_100.0;
        let f = 220.0;
        let src: Vec<f32> = {
            let n = (sr * 0.3) as usize;
            (0..n)
                .map(|i| {
                    let w = TWO_PI * f * i as f32 / sr;
                    0.6 * w.sin() + 0.3 * (2.0 * w + 1.2).sin()
                })
                .collect()
        };
        engine.shared_params.update_sample_rate(sr);
        engine.analyze_and_load(&src, sr, f, &[], 0);

        engine.shared_params.set_zero_key_phases(false);
        let kept = engine.assemble_buffer_at_original_pitch();
        engine.shared_params.set_zero_key_phases(true);
        let zeroed = engine.assemble_buffer_at_original_pitch();
        assert!(!kept.is_empty(), "the audition must render at all");
        assert_eq!(kept.len(), zeroed.len());
        assert!(
            kept.iter().zip(&zeroed).all(|(a, b)| a == b),
            "the audition must be untouched by the keyboard's phase switch"
        );
    }

    /// The switch is off until the user asks for it, in every mode — loading a
    /// grid must not silently change how the keyboard sounds.
    #[test]
    fn zero_key_phases_defaults_off_everywhere() {
        let fresh = create_test_engine();
        assert!(
            !fresh.shared_params.zero_key_phases(),
            "a fresh Synth instance must keep its phases"
        );

        let imported = engine_with_alternating_phase(8);
        assert!(
            !imported.shared_params.zero_key_phases(),
            "importing a grid must not switch the phases off"
        );

        let analysed = create_test_engine();
        analysed.shared_params.update_sample_rate(44_100.0);
        analysed.analyze_and_load(&tone(44_100.0, 220.0, 0.2), 44_100.0, 220.0, &[], 0);
        assert!(
            !analysed.shared_params.zero_key_phases(),
            "analysing audio must not switch the phases off"
        );

        // And a grid loaded while it is on leaves it on — it is the user's
        // setting, not a property of the grid.
        analysed.shared_params.set_zero_key_phases(true);
        analysed.analyze_and_load(&tone(44_100.0, 330.0, 0.2), 44_100.0, 330.0, &[], 0);
        assert!(
            analysed.shared_params.zero_key_phases(),
            "loading a grid must not undo the user's choice"
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

#[cfg(test)]
mod keyboard_fidelity {
    use super::*;
    use crate::params::LeSynthParams;
    use std::sync::Arc;

    /// A harmonic-rich source with an optional vibrato, plus its contour — the
    /// shape of thing the Resynthesis panel hands over.
    fn source(sr: f32, f0: f32, secs: f32, vib: f32) -> (Vec<f32>, Vec<f32>) {
        let n = (sr * secs) as usize;
        let mut fund = 0.0f32;
        let mut out = Vec::with_capacity(n);
        let mut contour = Vec::new();
        for i in 0..n {
            let t = i as f32 / sr;
            let f = f0 * (1.0 + vib * (2.0 * std::f32::consts::PI * 5.0 * t).sin());
            fund += 2.0 * std::f32::consts::PI * f / sr;
            let s: f32 = (1..=12)
                .map(|k| {
                    let kk = k as f32;
                    (1.0 / kk) * (fund * kk + 0.7 * kk).sin()
                })
                .sum();
            out.push(s * 0.35);
            if i % 256 == 0 {
                contour.push(f);
            }
        }
        (out, contour)
    }

    /// Error of `got` against `want` in dB, after fitting one gain — a key plays
    /// the grid at its normalised level, which is a level difference and not a
    /// distortion.
    fn err_db(got: &[f32], want: &[f32]) -> f64 {
        let n = got.len().min(want.len());
        let (mut num, mut den) = (0.0f64, 0.0f64);
        for i in 0..n {
            num += got[i] as f64 * want[i] as f64;
            den += (want[i] as f64) * (want[i] as f64);
        }
        let g = if den > 0.0 { num / den } else { 1.0 };
        let (mut e, mut r) = (0.0f64, 0.0f64);
        for i in 0..n {
            let d = got[i] as f64 - g * want[i] as f64;
            e += d * d;
            r += (g * want[i] as f64) * (g * want[i] as f64);
        }
        10.0 * (e.max(1e-300) / r.max(1e-300)).log10()
    }

    fn analysed(sr: f32, f0: f32, vib: f32) -> (SynthComputeEngine, Vec<f32>) {
        let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
        let (src, contour) = source(sr, f0, 1.0, vib);
        engine.analyze_and_load(&src, sr, f0, &contour, 0);
        *engine.shared_params.sample_rate.lock().unwrap() = sr;
        (engine, src)
    }

    /// The live path and the offline one must render a key **the same**.
    ///
    /// They share `render_key_buffer`, so any gap is in what each hands it —
    /// and that is exactly where a defect hides from an offline dump: the tool
    /// says the renderer is clean while the plugin buzzes, because the tool was
    /// never rendering what the plugin renders. Prints both sides' inputs, so a
    /// failure names the parameter rather than just the dB.
    #[test]
    fn the_live_key_and_the_offline_key_are_the_same_render() {
        let sr = 24_000.0f32;
        let f0 = 107.63f32;
        let (engine, _src) = analysed(sr, f0, 0.02);
        engine.shared_params.update_sample_rate(sr);
        let key = 24usize;

        let live = engine.assemble_buffer_for_key(key);
        assert!(
            engine.shared_params.used_playback_grid(),
            "the live path fell back to the contour renderer — the offline dump \
             cannot see this, and it is what a key would sound like"
        );

        let base_period = engine.shared_params.piano_periods.lock().unwrap()[key];
        let target = target_samples_for(&engine.shared_params);
        let cap = max_harmonic_for_key(key);
        let amp = engine.shared_params.amplitude_data.lock().unwrap().clone();
        let phase = engine.shared_params.phase_data.lock().unwrap().clone();
        let lengths = engine.shared_params.analysis_bucket_lengths.lock().unwrap().clone();
        let dc = engine.shared_params.analysis_dc.lock().unwrap().clone();
        let nyq = engine.shared_params.analysis_nyquist.lock().unwrap().clone();
        let ratios = engine.shared_params.bucket_pitch_ratio.lock().unwrap().clone();
        let a_rate = *engine.shared_params.analysis_sample_rate.lock().unwrap();
        let b_freq = *engine.shared_params.analysis_base_freq.lock().unwrap();

        println!(
            "live : base_period {base_period:.3}, target {target}, cap {cap}, \
             buckets {}, lengths {}, a_rate {a_rate}, b_freq {b_freq}, out {}",
            amp[0].len(),
            lengths.len(),
            live.len()
        );

        let offline = resynthesize_key(
            &amp, &phase, &lengths, &dc, &nyq, &ratios, base_period, b_freq, a_rate, sr,
            cap, target, 0.0,
        );
        println!("offline: out {}", offline.len());
        assert_eq!(live.len(), offline.len(), "the two paths disagree on length");
        let db = err_db(&live, &offline);
        println!("live vs offline: {db:.1} dB");
        assert!(
            db < -80.0,
            "the live key render and the offline one are {db:.1} dB apart — same \
             renderer, so one of the inputs above differs"
        );
    }

    /// **The requirement**: the key whose period *is* the source's must sound
    /// like the source, not like a rough copy of it — what "Original Pitch And
    /// Gain" plays, on a key.
    ///
    /// It is the same grid either way, so any gap is the renderer's. Before the
    /// source's own bucket periods drove it, this measured −32 dB on a steady
    /// tone and −12 dB with vibrato against a −134 dB audition: the bucket
    /// lengths were rounded to whole samples and then re-imposed as a pitch,
    /// which lands that rounding as a phase jump at the cycle rate. Both figures
    /// are now past −45 dB, and the bound below is set with room for the cycle
    /// table's own error rather than at the measured value.
    #[test]
    fn a_key_at_the_sources_own_pitch_reproduces_the_source() {
        let sr = 44100.0;
        // 440 Hz is key 48 exactly, so this key transposes by 1.0.
        for vib in [0.0f32, 0.03] {
            let (engine, src) = analysed(sr, 440.0, vib);
            let audition = engine.assemble_buffer_at_original_pitch();
            let key = engine.assemble_buffer_for_key(48);
            assert!(
                err_db(&audition, &src) < -100.0,
                "the audition is the reference and must stay exact: {:.1} dB",
                err_db(&audition, &src)
            );
            let got = err_db(&key, &src);
            assert!(
                got < -45.0,
                "key 48 (vib {vib}) is {got:.1} dB from the source it was analysed from"
            );
        }
    }

    /// How long the synchronous fallback in `get_buffer_for_key` actually takes,
    /// since a MIDI note-on can reach it from the audio thread. Reported, not
    /// asserted tightly — it is a budget, not a contract.
    #[test]
    fn the_synchronous_key_render_is_affordable() {
        let sr = 48_000.0f32;
        let (engine, _src) = analysed(24_000.0, 220.0, 0.03);
        engine.shared_params.update_sample_rate(sr);
        let t = std::time::Instant::now();
        let out = engine.assemble_buffer_for_key(48);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        println!("synchronous key render: {ms:.1} ms for {} samples at {sr:.0} Hz", out.len());
        assert!(!out.is_empty());
    }

    /// The true periods are the **pitch**, and a pitch does not need a wall
    /// clock. `render_key_buffer` used to gate the whole `BucketTiming` on
    /// `drive_by_time`, so a note with no recorded duration — a Synth timeline,
    /// or an analysis that never set one — silently rendered each bucket at its
    /// *rounded* length instead, which is the buzz the `PlaybackGrid` exists to
    /// remove. Only the wall-clock walk needs the duration.
    #[test]
    fn the_true_periods_survive_a_note_with_no_duration() {
        let sr = 24_000.0f32;
        let (engine, _src) = analysed(sr, 440.0, 0.03);
        engine.shared_params.update_sample_rate(sr);
        let grid = playback_grid(&engine.shared_params).expect("an analysed grid has one");
        let base_period = engine.shared_params.piano_periods.lock().unwrap()[48];
        let (periods, spans) = key_timing(&grid, &engine.shared_params, base_period).unwrap();
        let enabled = vec![true; grid.amplitude.len()];
        let ratios = engine.shared_params.bucket_pitch_ratio.lock().unwrap().clone();
        let timing =
            BucketTiming {
                periods: &periods,
                spans: &spans,
                dc: &grid.dc,
            ramp: &grid.ramp,
                rotations: &grid.rotations,
                usable_harmonics: grid.usable_harmonics,
            };

        // target_samples = 0: the Synth timeline, one cycle per bucket.
        let with_timing = render_key_buffer(
            grid.amplitude.len(), &grid.amplitude, &grid.phase, &enabled, &enabled,
            base_period, NUM_HARMONICS, &ratios, Some(&timing), 0, None,
        );
        let without = render_key_buffer(
            grid.amplitude.len(), &grid.amplitude, &grid.phase, &enabled, &enabled,
            base_period, NUM_HARMONICS, &ratios, None, 0, None,
        );
        assert!(!with_timing.is_empty());

        // The true periods are fractional and the rounded ones are not, so a
        // note built from them cannot come out the same length. If it does, the
        // timing was thrown away.
        let db = err_db(&with_timing, &without);
        println!(
            "one cycle per bucket: with the true periods {} samples, without {} ({db:.1} dB apart)",
            with_timing.len(),
            without.len()
        );
        assert!(
            db > -60.0,
            "with no duration the renderer produced the same audio with and without \
             the true periods ({db:.1} dB) — it discarded them"
        );
    }

    /// Is the gap at the source's own pitch a *drift* or a distortion? PSOLA
    /// spaces its grains by the bucket's true period, while the source's own
    /// period boundaries sit at those periods **rounded** to whole samples, so
    /// the render walks slowly out of step with the recording it came from —
    /// a fraction of a period over seconds. Sample-wise error counts that as
    /// gross distortion (the trap in [[keyboard-render-source-timing]]), so
    /// measure it per block with the alignment fitted out.
    #[test]
    fn the_sources_own_pitch_is_a_drift_not_a_distortion() {
        for vib in [0.0f32, 0.03] {
            the_sources_own_pitch_probe(vib);
        }
    }

    fn the_sources_own_pitch_probe(vib: f32) {
        let sr = 44_100.0f32;
        let (engine, src) = analysed(sr, 440.0, vib);
        let key = engine.assemble_buffer_for_key(48);
        let n = key.len().min(src.len());
        let block = 4096usize;
        let (mut worst, mut sum, mut count) = (0.0f64, 0.0f64, 0usize);
        let mut lags = Vec::new();
        for start in (0..n.saturating_sub(block)).step_by(block) {
            let a = &key[start..start + block];
            // Best integer lag within a period, then gain-fit, per block.
            let (mut best, mut best_lag) = (f64::INFINITY, 0i64);
            for lag in -120i64..=120 {
                let (mut num, mut den) = (0.0f64, 0.0f64);
                for i in 0..block {
                    let j = start as i64 + i as i64 + lag;
                    if j < 0 || j as usize >= src.len() {
                        continue;
                    }
                    num += a[i] as f64 * src[j as usize] as f64;
                    den += (src[j as usize] as f64).powi(2);
                }
                if den <= 0.0 {
                    continue;
                }
                let g = num / den;
                let (mut e, mut r) = (0.0f64, 0.0f64);
                for i in 0..block {
                    let j = start as i64 + i as i64 + lag;
                    if j < 0 || j as usize >= src.len() {
                        continue;
                    }
                    let d = a[i] as f64 - g * src[j as usize] as f64;
                    e += d * d;
                    r += (g * src[j as usize] as f64).powi(2);
                }
                let db = 10.0 * (e.max(1e-300) / r.max(1e-300)).log10();
                if db < best {
                    best = db;
                    best_lag = lag;
                }
            }
            lags.push(best_lag);
            worst = worst.max(best);
            sum += best;
            count += 1;
        }
        let mean = sum / count.max(1) as f64;
        println!("vib {vib}: per-block, alignment fitted out: mean {mean:.1} dB, worst {worst:.1} dB");
        println!("vib {vib}: block lags (samples): {lags:?}");
        assert!(
            mean < -45.0,
            "at the source's own pitch the render is {mean:.1} dB from it even with the \
             alignment fitted out — that is distortion, not drift"
        );
    }

    /// What bandwidth the grid reports for a source of known extent.
    #[test]
    fn the_usable_bandwidth_matches_the_source() {
        for (f0, harmonics) in [(440.0f32, 12usize), (110.0, 12)] {
            let sr = 44_100.0f32;
            let (engine, _src) = analysed(sr, f0, 0.0);
            let grid = playback_grid(&engine.shared_params).expect("analysed");
            println!(
                "source {f0} Hz with {harmonics} harmonics -> usable_harmonics {}",
                grid.usable_harmonics
            );
            assert!(
                grid.usable_harmonics >= harmonics,
                "the source carries {harmonics} harmonics but only {} were kept",
                grid.usable_harmonics
            );
        }
    }

    /// Do the true periods still tile the source? `true_periods` fits the
    /// contour's *shape* and scales it so the total matches the recorded
    /// lengths, which pins the sum but lets the running total wander in
    /// between. PSOLA places its grains by that running total, so any wander is
    /// a time offset against the recording — and against the bucket the wall
    /// clock is simultaneously pointing at.
    #[test]
    fn the_true_periods_track_the_recorded_boundaries() {
        for vib in [0.0f32, 0.03] {
            let sr = 44_100.0f32;
            let (engine, _src) = analysed(sr, 440.0, vib);
            let grid = playback_grid(&engine.shared_params).expect("analysed");
            let lengths = engine.shared_params.analysis_bucket_lengths.lock().unwrap().clone();
            let (mut cum, mut worst) = (0.0f64, 0.0f64);
            for b in 0..grid.periods.len().min(lengths.len()) {
                cum += grid.periods[b] as f64 - lengths[b] as f64;
                worst = worst.max(cum.abs());
            }
            println!(
                "vib {vib}: true periods vs recorded boundaries — worst running gap {worst:.2} \
                 samples, closing at {cum:.2}"
            );
        }
    }

    /// A key press must never play audio rendered from a **different grid**.
    ///
    /// `get_buffer_for_key` hands back the previous buffer whenever the key is
    /// `Dirty` or `Computing`, to keep the audio thread from rendering. But
    /// loading a source marks every key dirty while its old buffer — rendered
    /// from whatever the grid was before, at startup the default Synth patch —
    /// is still sitting there. So the first press after loading plays that,
    /// which is neither the analysed sound nor anything the renderer is
    /// responsible for, and no offline dump can see it.
    #[test]
    fn a_key_press_never_plays_a_buffer_from_the_previous_grid() {
        let sr = 24_000.0f32;
        let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
        engine.shared_params.update_sample_rate(sr);
        let key = 48usize;

        // Let the default Synth grid render, the way it does while the editor
        // sits open before any audio is loaded.
        let mut stale = Vec::new();
        for _ in 0..600 {
            if engine.shared_params.buffer_states.lock().unwrap()[key] == BufferState::Clean {
                stale = engine.get_buffer_for_key(key);
                if !stale.is_empty() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!stale.is_empty(), "the Synth-mode buffer never rendered");

        // Now load a source, as the Resynthesis panel does.
        let (src, contour) = source(sr, 440.0, 1.0, 0.03);
        engine.analyze_and_load(&src, sr, 440.0, &contour, 0);

        // Press the key straight away.
        let played = engine.get_buffer_for_key(key);
        let analysed = engine.assemble_buffer_for_key(key);
        let db = err_db(&played, &analysed);
        println!(
            "pressed right after load: {} samples vs the analysed {} ({db:.1} dB)",
            played.len(),
            analysed.len()
        );
        assert!(
            db < -40.0,
            "the key played {db:.1} dB from the analysed sound — it handed back the \
             buffer rendered from the previous grid"
        );
    }

    /// Changing the bucket count must not silently send every key back to the
    /// contour renderer.
    ///
    /// `set_num_buckets` resamples the grid rows but leaves the analysis's
    /// bucket lengths at their old count, and `build_playback_grid` refuses a
    /// grid whose width disagrees with them. The refusal is silent — same call,
    /// same signature — so the keyboard just starts buzzing again and nothing
    /// in the offline dump or the other tests can see it.
    #[test]
    fn changing_the_bucket_count_keeps_the_playback_grid() {
        let sr = 24_000.0f32;
        let (engine, _src) = analysed(sr, 440.0, 0.03);
        engine.shared_params.update_sample_rate(sr);
        let before = engine.shared_params.amplitude_data.lock().unwrap()[0].len();

        let _ = engine.assemble_buffer_for_key(48);
        assert!(
            engine.shared_params.used_playback_grid(),
            "a freshly analysed grid must transpose from a PlaybackGrid"
        );

        engine.set_num_buckets(before / 2);
        assert_eq!(
            engine.shared_params.amplitude_data.lock().unwrap()[0].len(),
            before,
            "an analysed grid's width follows the source and must not be resampled"
        );
        let _ = engine.assemble_buffer_for_key(48);
        assert!(
            engine.shared_params.used_playback_grid(),
            "after set_num_buckets({}) the key fell back to the contour renderer — \
             the analysis lengths still describe {before} buckets",
            before / 2
        );
    }

    /// **What a key press actually plays.** `get_buffer_for_key` hands back the
    /// *background* thread's render, which is a second copy of the render setup
    /// — its own grid lookup, its own timing, its own parameter snapshot. Every
    /// other test here, and the whole offline dump, exercises the synchronous
    /// path instead, so a defect that lives only in the background copy is
    /// inaudible to all of them and audible to the user on every key.
    #[test]
    fn what_a_key_press_plays_matches_the_synchronous_render() {
        let sr = 24_000.0f32;
        let (engine, _src) = analysed(sr, 440.0, 0.03);
        engine.shared_params.update_sample_rate(sr);
        let key = 48usize;

        let sync = engine.assemble_buffer_for_key(key);

        // Wait for the background thread to render this key, as the editor does.
        let mut played = Vec::new();
        for _ in 0..600 {
            let state = engine.shared_params.buffer_states.lock().unwrap()[key];
            if state == BufferState::Clean {
                played = engine.get_buffer_for_key(key);
                if !played.is_empty() {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!played.is_empty(), "the background thread never produced key {key}");
        assert!(
            engine.shared_params.used_playback_grid(),
            "the background render fell back to the contour renderer"
        );

        println!("background {} samples, synchronous {}", played.len(), sync.len());
        assert_eq!(played.len(), sync.len(), "the two paths disagree on length");
        let db = err_db(&played, &sync);
        println!("what a key press plays vs the synchronous render: {db:.1} dB");
        assert!(
            db < -80.0,
            "the buffer a key press plays is {db:.1} dB from the synchronous render \
             of the same key — they are supposed to be the same audio"
        );
    }

    /// The same key, with the **device** running at a different rate from the
    /// analysis — which is the normal case in a host and the one every other
    /// test here misses, because `analysed()` leaves the two equal.
    ///
    /// A key's period comes from `piano_periods`, which is in device samples,
    /// while the bucket's true period and span are in the source's. If the
    /// conversion between them is wrong the note is still the right pitch and
    /// the right length — `key_timing` scales both — so nothing obvious breaks;
    /// what changes is how each bucket's cycle lands, which is heard as
    /// roughness at the bucket rate and nowhere else.
    #[test]
    fn a_key_reproduces_the_source_at_a_device_rate_too() {
        let analysis_sr = 24_000.0f32;
        for &device_sr in &[24_000.0f32, 44_100.0, 48_000.0] {
            for vib in [0.0f32, 0.03] {
                // 440 Hz is key 48 exactly, so the key transposes by 1.0 and the
                // render is comparable with the source itself.
                let (engine, src) = analysed(analysis_sr, 440.0, vib);
                engine.shared_params.update_sample_rate(device_sr);

                let key = engine.assemble_buffer_for_key(48);
                assert!(
                    engine.shared_params.used_playback_grid(),
                    "device {device_sr}: the key fell back to the contour renderer"
                );

                // The source at the device's rate is what this key should be.
                let want = if (device_sr - analysis_sr).abs() < 0.5 {
                    src.clone()
                } else {
                    resample_stream(&src, device_sr as f64 / analysis_sr as f64)
                };
                let got = err_db(&key, &want);
                println!(
                    "analysis {analysis_sr:.0} -> device {device_sr:.0}, vib {vib}: {got:.1} dB \
                     ({} samples vs {})",
                    key.len(),
                    want.len()
                );
                assert!(
                    got < -40.0,
                    "device {device_sr:.0} Hz, vib {vib}: a key at the source's own pitch is \
                     {got:.1} dB from it — the analysis rate is {analysis_sr:.0}, and this is \
                     the only thing that changed"
                );
            }
        }
    }

    /// The two clocks a key runs on. `periods` is the pitch — one true period of
    /// the source, transposed — and `spans` is the wall clock, the bucket's own
    /// duration, which is the same on every key ("preserve seconds"). At the
    /// source's own pitch they coincide; below it the cycle outlasts the bucket.
    #[test]
    fn a_key_runs_on_the_sources_period_and_the_sources_clock() {
        let sr = 44100.0;
        let (engine, _) = analysed(sr, 440.0, 0.0);
        let grid = playback_grid(&engine.shared_params).expect("an analysed grid has one");
        let unity = engine.shared_params.piano_periods.lock().unwrap()[48];
        let (p48, s48) = key_timing(&grid, &engine.shared_params, unity).unwrap();
        // At the source's own pitch the two clocks run together: each bucket's
        // cycle is within the rounding of its span (that rounding is precisely
        // what the period no longer inherits), and over the note they agree.
        for (p, s) in p48.iter().zip(&s48).take(s48.len() - 1) {
            assert!(
                (p - s).abs() <= 1.0,
                "at the source's own pitch a bucket's period is its span: {p} vs {s}"
            );
        }
        let n = s48.len() - 1;
        let mean = |v: &[f32]| v[..n].iter().sum::<f32>() / n as f32;
        assert!(
            (mean(&p48) - mean(&s48)).abs() < 0.01,
            "the two clocks must not drift apart: {} vs {}",
            mean(&p48),
            mean(&s48)
        );
        let low = engine.shared_params.piano_periods.lock().unwrap()[36];
        let (p36, s36) = key_timing(&grid, &engine.shared_params, low).unwrap();
        assert!(p36[0] > s36[0], "an octave down the cycle outlasts the bucket");
        // An octave is a factor of two, and the wall clock does not move with it.
        assert!((p36[0] / p48[0] - 2.0).abs() < 0.01);
        assert!((s36[0] - s48[0]).abs() < 1e-3);
    }

    /// A steady source has one period, however its buckets rounded: the grid a
    /// key transposes from must say so, or the rounding is heard as pitch.
    #[test]
    fn the_playback_grid_holds_one_steady_period() {
        let sr = 44100.0;
        let (engine, _) = analysed(sr, 440.0, 0.0);
        let grid = playback_grid(&engine.shared_params).unwrap();
        let lens = engine.shared_params.analysis_bucket_lengths.lock().unwrap().clone();
        // The recorded lengths are whole samples and do vary…
        assert!(lens.iter().any(|&l| l != lens[0]), "the rounding should be visible");
        // …while the periods they stand for do not.
        let inner = &grid.periods[..grid.periods.len() - 1];
        let (min, max) = inner.iter().fold((f32::MAX, 0.0f32), |(a, b), &p| (a.min(p), b.max(p)));
        assert!(
            max - min < 1e-3,
            "a steady source must have one period, got {min}..{max}"
        );
        // And it is the source's: 44100 / 440.
        assert!((min - sr / 440.0).abs() < 0.2, "{min} is not the source's period");
    }

    /// The per-bucket mean is part of the sound: dropping it puts a step at every
    /// bucket boundary. It is the mean of the reconstruction over that bucket's
    /// period, on the same scale as the harmonics beside it — a missing `1/N`
    /// from the transform would land it `N` times too loud.
    #[test]
    fn the_playback_grid_carries_the_mean() {
        let sr = 44100.0;
        // A source with a mean of its own, so there is something to carry.
        let n = (sr * 0.3) as usize;
        let src: Vec<f32> = (0..n)
            .map(|i| 0.2 + 0.5 * (TWO_PI * 440.0 * i as f32 / sr).sin())
            .collect();
        let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
        engine.shared_params.update_sample_rate(sr);
        engine.analyze_and_load(&src, sr, 440.0, &[], 0);
        let grid = playback_grid(&engine.shared_params).unwrap();
        // Against the same mean measured straight off the reconstruction.
        let y = resynthesize_exact(
            &engine.shared_params.amplitude_data.lock().unwrap(),
            &engine.shared_params.phase_data.lock().unwrap(),
            &engine.shared_params.analysis_bucket_lengths.lock().unwrap(),
            &engine.shared_params.analysis_dc.lock().unwrap(),
            &engine.shared_params.analysis_nyquist.lock().unwrap(),
            &[],
            &[],
            0.0,
            1.0,
        );
        let divisor = {
            let mut a = engine.shared_params.amplitude_data.lock().unwrap().clone();
            normalize_grid_per_bucket(&mut a)
        };
        let b = 10usize;
        let start: f32 = engine.shared_params.analysis_bucket_lengths.lock().unwrap()[..b]
            .iter()
            .map(|&l| l as f32)
            .sum();
        let t = grid.periods[b];
        let want: f32 = (0..64)
            .map(|i| y[(start + t * i as f32 / 64.0) as usize])
            .sum::<f32>()
            / 64.0;
        let got = grid.dc[b] * divisor; // the grid is normalised, the source is not
        assert!(
            (got - want).abs() < 0.02 * want.abs().max(0.05),
            "bucket {b}: grid mean {got}, source mean {want}"
        );
        assert!(want.abs() > 0.05, "the test source should have a mean at all");
    }

    /// Synth mode has no source to follow: no playback grid, and the contour
    /// keeps driving the pitch exactly as before.
    #[test]
    fn synth_mode_has_no_playback_grid() {
        let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
        engine.shared_params.set_execution_mode(ExecutionMode::Synth);
        assert!(playback_grid(&engine.shared_params).is_none());
    }

    /// A *steady* harmonic-rich tone: no vibrato, no envelope, so a correct
    /// transposition is a perfectly periodic waveform at the key's period and
    /// everything else the renderer produces is its own error.
    fn steady(sr: f32, f0: f32, secs: f32) -> Vec<f32> {
        let n = (sr * secs) as usize;
        (0..n)
            .map(|i| {
                let w = TWO_PI * f0 * i as f32 / sr;
                0.35 * (1..=12).map(|k| (1.0 / k as f32) * (w * k as f32 + 0.7 * k as f32).sin()).sum::<f32>()
            })
            .collect()
    }

    /// How far the render is from being periodic at `period` samples, in dB
    /// (RMS of `x[i] - x[i-period]` against RMS of `x`), over the steady middle.
    /// A step at a bucket boundary is aperiodic; a correctly transposed steady
    /// tone is not.
    fn aperiodicity_db(x: &[f32], period: usize) -> f64 {
        let a = period * 4;
        let b = x.len().saturating_sub(period * 4);
        if b <= a + period {
            return f64::NAN;
        }
        let (mut e, mut r) = (0.0f64, 0.0f64);
        for i in a..b {
            let d = x[i] as f64 - x[i - period] as f64;
            e += d * d;
            r += (x[i] as f64) * (x[i] as f64);
        }
        10.0 * (e.max(1e-300) / r.max(1e-300)).log10()
    }

    /// Worst sample-to-sample step, over the median one — a click reads here
    /// even when it is too short to move an RMS figure.
    fn step_ratio(x: &[f32]) -> f64 {
        let mut d: Vec<f64> = x.windows(2).map(|w| (w[1] - w[0]).abs() as f64).collect();
        let max = d.iter().cloned().fold(0.0f64, f64::max);
        d.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let med = d[d.len() / 2].max(1e-12);
        max / med
    }

    /// How far a render is from *the* correct transposition of [`steady`], in dB.
    ///
    /// The source is a fixed harmonic series, so its transposition onto a key is
    /// known in closed form: the same series at the key's frequency, with the
    /// same relative phases, at whatever absolute phase and level the renderer
    /// happens to start at. Both of those are fitted out, and what is left is
    /// error — the metric a key's fuzz has to be judged by, since a lag-based
    /// one cannot tell a rendered period apart from the period it assumed.
    fn vs_ideal_db(x: &[f32], sr: f32, f_key: f32) -> f64 {
        let n = x.len();
        let ideal = |psi: f64| -> Vec<f64> {
            (0..n)
                .map(|i| {
                    let w = 2.0 * std::f64::consts::PI * f_key as f64 * i as f64 / sr as f64;
                    (1..=12)
                        .map(|k| (1.0 / k as f64) * (w * k as f64 + 0.7 * k as f64 + k as f64 * psi).sin())
                        .sum::<f64>()
                })
                .collect()
        };
        // The absolute phase is the one free parameter; scan then refine.
        let score = |psi: f64| -> f64 {
            let want = ideal(psi);
            let (mut num, mut den) = (0.0f64, 0.0f64);
            for i in 0..n {
                num += x[i] as f64 * want[i];
                den += want[i] * want[i];
            }
            let g = if den > 0.0 { num / den } else { 0.0 };
            let (mut e, mut r) = (0.0f64, 0.0f64);
            for i in 0..n {
                let d = x[i] as f64 - g * want[i];
                e += d * d;
                r += (g * want[i]) * (g * want[i]);
            }
            10.0 * (e.max(1e-300) / r.max(1e-300)).log10()
        };
        let mut best = (f64::MAX, 0.0f64);
        let steps = 400;
        for i in 0..steps {
            let psi = 2.0 * std::f64::consts::PI * i as f64 / steps as f64;
            let s = score(psi);
            if s < best.0 {
                best = (s, psi);
            }
        }
        let mut step = 2.0 * std::f64::consts::PI / steps as f64;
        for _ in 0..40 {
            step *= 0.5;
            for psi in [best.1 - step, best.1 + step] {
                let s = score(psi);
                if s < best.0 {
                    best = (s, psi);
                }
            }
        }
        best.0
    }

    #[test]
    #[ignore = "measurement, not an assertion: cargo test -- --ignored --nocapture"]
    fn measure_playback_grid_cost() {
        let sr = 44_100.0;
        // 3 s of a 150 Hz source: ~450 buckets, the shape of a real subtrack.
        let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
        engine.shared_params.update_sample_rate(sr);
        let t0 = std::time::Instant::now();
        engine.analyze_and_load(&steady(sr, 150.0, 3.0), sr, 150.0, &[], 0);
        let analysed = t0.elapsed();
        let nb = engine.shared_params.amplitude_data.lock().unwrap()[0].len();
        engine.shared_params.playback_grid_dirty.store(true, Ordering::Relaxed);
        let t1 = std::time::Instant::now();
        let _ = playback_grid(&engine.shared_params).unwrap();
        let built = t1.elapsed();
        let t2 = std::time::Instant::now();
        let _ = playback_grid(&engine.shared_params).unwrap();
        println!("  {nb} buckets: analyse+load {analysed:?}, playback grid {built:?}, cached {:?}", t2.elapsed());
    }

    #[test]
    #[ignore = "measurement, not an assertion: cargo test -- --ignored --nocapture"]
    fn measure_transposed_key_fidelity() {
        let sr = 44_000.0; // keys 36/48/60 land on 200/100/50-sample periods
        for (src_f, name) in [(293.66f32, "D4 source"), (110.0, "A2 source")] {
            let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
            engine.shared_params.update_sample_rate(sr);
            engine.analyze_and_load(&steady(sr, src_f, 0.5), sr, src_f, &[], 0);
            println!("\n{name} ({src_f} Hz), {} buckets", engine.shared_params.amplitude_data.lock().unwrap()[0].len());
            for (key, hz) in [(36usize, 220.0f32), (48, 440.0), (60, 880.0)] {
                let period = (sr / hz) as usize;
                let x = engine.assemble_buffer_for_key(key);
                // The ends carry the note's own fade, so judge the middle.
                let mid = &x[x.len() / 4..x.len() / 2];
                // A fit over one window punishes a constant detune as if it were
                // noise, so report the best over a ±0.1 % pitch scan too: if the
                // gap is there, what is left is tuning, not roughness.
                let mut tuned = (f64::MAX, 0.0f32);
                for i in -60i32..=60 {
                    let off = i as f32 * 1e-6;
                    let s = vs_ideal_db(mid, sr, hz * (1.0 + off));
                    if s < tuned.0 {
                        tuned = (s, off);
                    }
                }
                println!(
                    "  key {key:>2} ({hz:>5.0} Hz, {:+.2} st): vs ideal {:>7.2} dB | aperiodicity {:>7.2} dB | worst step / median {:>6.1}x | tuned {:>7.2} dB at {:+.3} cent",
                    12.0 * (hz / src_f).log2(),
                    vs_ideal_db(mid, sr, hz),
                    aperiodicity_db(&x, period),
                    step_ratio(&x),
                    tuned.0,
                    1200.0 * (1.0 + tuned.1).log2(),
                );
            }
        }
    }

    /// **The requirement.** A source whose period is not a whole number of
    /// samples — which is nearly every source — must transpose onto a key
    /// without the roughness the rounding used to cost.
    ///
    /// The source here is a steady 12-harmonic tone of 149.83 samples, so the
    /// correct render at any key is a perfectly periodic waveform and every
    /// departure from one is the renderer's. Measured against that ideal, and
    /// against itself one period earlier:
    ///
    /// | key | before | after |
    /// |---|---|---|
    /// | 5 semitones down | −32.8 / −29.5 dB | **−62.1 / −73.7 dB** |
    /// | 7 up | −27.1 / −28.1 | **−56.5 / −74.6** |
    /// | 19 up | −21.3 / −27.1 | **−44.3 / −72.4** |
    ///
    /// The bounds below sit well inside those, and the same tone with a period
    /// of a whole 400 samples — which never had the defect — is held to the
    /// figure it always measured, so a fix that only helps the awkward case
    /// cannot regress the easy one.
    ///
    /// (What remains at 19 semitones up is a **0.03-cent** tuning offset, not
    /// roughness: correcting for it takes the same render to −75 dB. It comes
    /// from reading the true period off the bucket lengths, whose total rounds
    /// once — see [`true_periods`].)
    #[test]
    fn a_transposed_key_does_not_inherit_the_bucket_rounding() {
        let sr = 44_000.0; // keys 36/48/60 land on 200/100/50-sample periods
        for (src_f, floor) in [(293.66f32, -40.0f64), (110.0, -80.0)] {
            let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
            engine.shared_params.update_sample_rate(sr);
            engine.analyze_and_load(&steady(sr, src_f, 0.5), sr, src_f, &[], 0);
            for (key, hz) in [(36usize, 220.0f32), (48, 440.0), (60, 880.0)] {
                let x = engine.assemble_buffer_for_key(key);
                assert!(!x.is_empty(), "key {key} must render");
                let mid = &x[x.len() / 4..x.len() / 2];
                let ideal = vs_ideal_db(mid, sr, hz);
                let periodic = aperiodicity_db(&x, (sr / hz) as usize);
                assert!(
                    ideal < floor,
                    "{src_f} Hz on key {key}: {ideal:.1} dB from the ideal transposition"
                );
                assert!(
                    periodic < -60.0,
                    "{src_f} Hz on key {key}: {periodic:.1} dB of aperiodicity — a steady \
                     source must transpose to a steady tone"
                );
            }
        }
    }

    /// The grid a key transposes from must be a clean harmonic series: the
    /// bucket's own bins are not, because its window is a whole number of
    /// samples and its period is not, and that difference is what a key hears.
    #[test]
    fn the_playback_grid_is_a_clean_harmonic_series() {
        let sr = 44_000.0;
        let engine = SynthComputeEngine::new(Arc::new(LeSynthParams::default()));
        engine.shared_params.update_sample_rate(sr);
        // 149.83 samples per period: the awkward case.
        engine.analyze_and_load(&steady(sr, 293.66, 0.5), sr, 293.66, &[], 0);
        let g = playback_grid(&engine.shared_params).unwrap();
        let b = 10usize;
        for k in 1..=12usize {
            let ratio = g.amplitude[k - 1][b] / g.amplitude[0][b];
            assert!(
                (ratio - 1.0 / k as f32).abs() < 1e-4,
                "harmonic {k} is at {ratio} of the fundamental, expected {}",
                1.0 / k as f32
            );
            // The source's partials share one phase, so their phases relative to
            // the fundamental are zero — the shape of the waveform, and what a
            // window that does not close would smear.
            let rel = (g.phase[k - 1][b] - k as f32 * g.phase[0][b]).rem_euclid(TWO_PI);
            let rel = if rel > std::f32::consts::PI { rel - TWO_PI } else { rel };
            assert!(rel.abs() < 1e-3, "harmonic {k}'s relative phase is {rel}");
        }
        // And nothing above the source's own partials.
        for k in 13..=20usize {
            assert!(
                g.amplitude[k - 1][b] < 1e-3 * g.amplitude[0][b],
                "harmonic {k} should be silent, got {}",
                g.amplitude[k - 1][b]
            );
        }
    }

    /// The closed form the renderer is approximating.
    fn ideal_sample(ampl: &[Vec<f32>], phase: &[Vec<f32>], max_h: usize, cycles: f64) -> f64 {
        let mut acc = 0.0f64;
        for n in 0..max_h {
            let a = ampl[n][0] as f64;
            if a == 0.0 {
                continue;
            }
            let k = (n + 1) as f64;
            acc += a * (std::f64::consts::TAU * k * cycles + phase[n][0] as f64).sin();
        }
        acc
    }

    /// Render a flat grid of `harmonics` harmonics and return
    /// `(residual_db, peak_error)` against the closed form.
    fn flat_grid_residual(harmonics: usize, base_period: f32, zero_phase: bool) -> (f64, f64) {
        let nb = 64;
        let mut ampl = vec![vec![0.0f32; nb]; NUM_HARMONICS];
        let mut phase = vec![vec![0.0f32; nb]; NUM_HARMONICS];
        // 1/k amplitudes: a sawtooth-ish spectrum, energy in every harmonic so
        // the top of the band is actually exercised. Scaled so the worst-case
        // in-phase sum stays under 1.0, exactly as `normalize_grid_per_bucket`
        // guarantees for a real grid — otherwise the renderer's output clamp
        // clips the peaks and swamps the measurement.
        let raw: Vec<f32> = (0..harmonics).map(|n| 1.0 / (n as f32 + 1.0)).collect();
        let scale = 0.95 / raw.iter().sum::<f32>();
        for n in 0..harmonics {
            for b in 0..nb {
                ampl[n][b] = raw[n] * scale;
                phase[n][b] = if zero_phase { 0.0 } else { (n as f32) * 0.7 };
            }
        }
        let enabled = vec![true; NUM_HARMONICS];
        let ratios = vec![1.0f32; nb];
        let target = (base_period * nb as f32) as usize;
        let out = render_key_buffer(
            NUM_HARMONICS, &ampl, &phase, &enabled, &enabled,
            base_period, harmonics, &ratios, None, target, None,
        );
        assert!(!out.is_empty(), "renderer produced nothing");

        let max_h = harmonics.min((base_period * 0.5).floor() as usize);
        let (mut se, mut sr, mut peak) = (0.0f64, 0.0f64, 0.0f64);
        // Skip the first cycle: the accumulator starts at an exact boundary and
        // the comparison is about the steady state.
        let skip = base_period.ceil() as usize;
        for n in skip..out.len() {
            let want = ideal_sample(&ampl, &phase, max_h, n as f64 / base_period as f64);
            let got = out[n] as f64;
            let d = got - want;
            peak = peak.max(d.abs());
            se += d * d;
            sr += want * want;
        }
        (10.0 * (se / sr.max(1e-30)).max(1e-30).log10(), peak)
    }

    #[test]
    fn flat_grid_renders_the_closed_form() {
        // Below IFFT_MIN_HARMONICS the renderer sums sinusoids directly, so it
        // *is* the closed form and this pins the convention (phase sign, table
        // scaling) that the fast path is then measured against.
        let (db, peak) = flat_grid_residual(8, 222.987, false);
        println!("direct sum, 8 harmonics: {db:.1} dB residual, peak {peak:.2e}");
        assert!(db < -100.0, "direct sinusoid path should be exact, got {db:.1} dB");
    }

    #[test]
    fn cycle_table_readout_is_accurate() {
        // The fast path builds one inverse FFT per bucket and reads it at
        // fractional positions. On a flat grid the answer must still be the
        // closed form; whatever it falls short by is distortion locked to the
        // cycle rate, i.e. audible as a buzz at f0 rather than as noise.
        for &h in &[16usize, 32, 64, 111] {
            let (db, peak) = flat_grid_residual(h, 222.987, false);
            println!("cycle table, {h:3} harmonics: {db:7.1} dB residual, peak {peak:.2e}");
        }
        for &h in &[16usize, 32, 64, 111] {
            let (db, peak) = flat_grid_residual(h, 222.987, true);
            println!("zero phase,  {h:3} harmonics: {db:7.1} dB residual, peak {peak:.2e}");
        }
        let (db, _) = flat_grid_residual(64, 222.987, false);
        assert!(db < -80.0, "cycle-table readout only reaches {db:.1} dB");
    }

    #[test]
    fn harmonic_content_is_stable_across_vibrato_cycles() {
        // The anti-alias cap is `floor(period / 2)`. Vibrato moves `period`, so
        // recomputing the cap per cycle steps it across an integer mid-note and
        // switches the top harmonics off and on at the cycle boundary — a
        // full-depth amplitude modulation at f0, i.e. a buzz. It survives
        // zeroing the phases, because it is not a phase effect, which is how it
        // was finally noticed.
        //
        // Two harmonics are watched. `k_safe` sits well below the cap for every
        // period the note uses and must be rendered at full amplitude on every
        // cycle — that pins the cap against being fixed by simply throwing away
        // bandwidth. `k_edge` straddles `2 * floor(period / 2)`, so it is the one
        // that used to flap; it must come out *consistent*, in or out for the
        // whole note but never alternating.
        //
        // Synth mode (target_samples = 0) so bucket == cycle and the renderer's
        // accumulator is trivially reproducible here.
        let nb = 64;
        let k_safe = 90usize;
        let k_edge = 100usize;
        let base_period = 200.0f32;
        let mut ampl = vec![vec![0.0f32; nb]; NUM_HARMONICS];
        let phase = vec![vec![0.0f32; nb]; NUM_HARMONICS];
        for b in 0..nb {
            ampl[0][b] = 0.5;
            ampl[k_safe - 1][b] = 0.1;
            ampl[k_edge - 1][b] = 0.1;
        }
        // Periods alternate 199.6 / 200.4, straddling 2 * k_edge: floor(p / 2) is
        // 99 on one and 100 on the other.
        let ratios: Vec<f32> = (0..nb)
            .map(|b| if b % 2 == 0 { base_period / 199.6 } else { base_period / 200.4 })
            .collect();
        let enabled = vec![true; NUM_HARMONICS];

        let out = render_key_buffer(
            NUM_HARMONICS, &ampl, &phase, &enabled, &enabled,
            base_period, NUM_HARMONICS, &ratios, None, 0, None,
        );
        assert!(!out.is_empty());

        // Project each rendered cycle onto sin(2 pi k pos) to recover harmonic
        // k's amplitude in that cycle.
        let per_cycle_amp = |k: usize| -> Vec<f64> {
            let mut cycles = 0.0f64;
            let mut acc: Vec<(f64, usize)> = vec![(0.0, 0); nb];
            for &v in out.iter() {
                let c = cycles as usize;
                if c >= nb {
                    break;
                }
                let pos = cycles - cycles.floor();
                let e = &mut acc[c];
                e.0 += v as f64 * (std::f64::consts::TAU * k as f64 * pos).sin();
                e.1 += 1;
                cycles += 1.0 / (base_period / ratios[c]).max(2.0) as f64;
            }
            acc.iter()
                .filter(|(_, n)| *n > 0)
                .map(|(s, n)| 2.0 * s / *n as f64)
                .collect()
        };
        let range = |a: &[f64]| {
            (
                a.iter().cloned().fold(f64::INFINITY, f64::min),
                a.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            )
        };

        let safe = per_cycle_amp(k_safe);
        let (slo, shi) = range(&safe);
        println!("k={k_safe:3} (safely under the cap): min {slo:.4}, max {shi:.4} of 0.1");
        assert!(
            slo > 0.09 && shi < 0.11,
            "harmonic {k_safe} sits below the cap for every period in this note, so \
             it must render at its grid amplitude on every cycle — got {slo:.4}..{shi:.4}"
        );

        let edge = per_cycle_amp(k_edge);
        let (elo, ehi) = range(&edge);
        println!("k={k_edge:3} (straddles the cap)   : min {elo:.4}, max {ehi:.4} of 0.1");
        assert!(
            ehi - elo < 0.02,
            "harmonic {k_edge} flaps between cycles ({elo:.4}..{ehi:.4}): the anti-alias \
             cap is moving mid-note, which amplitude-modulates the top of the band at f0"
        );
    }
}
