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

mod constants;
mod engine;
mod gui;
mod params;
mod plugin;
mod voice;

pub use plugin::LeSynth;

// ───────────────────────────────────────────────────────────────────────────
// Host-facing C ABI bridge (Analysis execution mode)
//
// The host DAW loads this same shared object (it is both the VST3 plugin and a
// plain cdylib). These exported functions let the host feed recorded audio
// "subtracks" to the plugin for Fourier analysis. Because the host's VST3
// component instances live in *this* shared object's address space, the host can
// hand a job straight to one of them: it addresses a job to the instance it
// tagged (see the registry below), the job waits in that instance, and that
// instance's editor claims it and runs the analysis on its own engine.
//
// Everything here is keyed per instance rather than global, because a host can
// have several editors open at once (one per track). A shared "active editor"
// or shared inbox lets whichever editor happens to paint first swallow another
// instance's job, so the track the host meant to fill comes up with empty charts.
// ───────────────────────────────────────────────────────────────────────────

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};

use crate::engine::SynthComputeEngine;

/// A pending analysis request handed from the host to a plugin instance.
pub struct AnalysisJob {
    pub samples: Vec<f32>,
    pub sample_rate: f32,
    pub base_freq: f32,
    /// Per-position fundamental (absolute Hz), uniformly resampled across the
    /// subtrack. Empty → flat at `base_freq` (legacy). Drives period-synchronous
    /// bucketing and the per-bucket DFT frequency.
    pub contour: Vec<f32>,
}

/// Untargeted analysis jobs, from hosts that push without naming an instance
/// (the legacy [`lesynth_fourier_push_analysis`] entry point). Whichever editor
/// paints first claims the oldest — fine when a single editor is open, which is
/// the only case this path can serve unambiguously. Hosts that open several
/// editors must use [`lesynth_fourier_push_analysis_to`] instead.
static ANALYSIS_INBOX: Mutex<VecDeque<AnalysisJob>> = Mutex::new(VecDeque::new());

/// Claim the oldest *untargeted* analysis job (called by a plugin editor once it
/// has found nothing addressed to its own instance).
pub(crate) fn claim_analysis_job() -> Option<AnalysisJob> {
    ANALYSIS_INBOX.lock().ok().and_then(|mut q| q.pop_front())
}

// ───────────────────────────────────────────────────────────────────────────
// Per-instance registry (state save/load)
//
// The host loads a saved LeSynth track, or exports the live grid a user edited,
// against a *specific* plugin instance. Because several editors can be open at
// once, the global "active editor" model isn't enough. Instead the host tags an
// instance before creating it (`lesynth_fourier_prepare_instance`); the plugin's
// `Default::default()` claims the pending token and registers a weak handle to
// its compute engine here, so the host can later address that exact instance.
// ───────────────────────────────────────────────────────────────────────────

/// Token the host set for the next instance to be created; taken by `default()`.
static PENDING_TOKEN: Mutex<Option<u64>> = Mutex::new(None);
/// `(token, weak engine)` for every live instance — `None` token for instances a
/// plain host created without tagging them. Small (one entry per instance),
/// pruned of dead entries on every access; a linear scan is fine.
static INSTANCE_REGISTRY: Mutex<Vec<(Option<u64>, Weak<SynthComputeEngine>)>> =
    Mutex::new(Vec::new());

/// Record the token the next-created instance should register under.
pub(crate) fn set_pending_token(token: u64) {
    if let Ok(mut g) = PENDING_TOKEN.lock() {
        *g = Some(token);
    }
}

/// Take (and clear) any pending token — called by a freshly created instance.
fn take_pending_token() -> Option<u64> {
    PENDING_TOKEN.lock().ok().and_then(|mut g| g.take())
}

/// Register a newly created engine, under the pending token if the host set one.
/// Instances created by a plain host (no token) are still registered — untagged
/// — so [`wake_all_editors`] can reach them.
pub(crate) fn register_new_instance(engine: &Arc<SynthComputeEngine>) {
    let token = take_pending_token();
    if let Ok(mut reg) = INSTANCE_REGISTRY.lock() {
        reg.retain(|(_, w)| w.strong_count() > 0);
        reg.push((token, Arc::downgrade(engine)));
    }
}

/// Resolve a token to its live engine, pruning any dead entries en route.
fn lookup_instance(token: u64) -> Option<Arc<SynthComputeEngine>> {
    let mut reg = INSTANCE_REGISTRY.lock().ok()?;
    reg.retain(|(_, w)| w.strong_count() > 0);
    reg.iter()
        .find(|(t, _)| *t == Some(token))
        .and_then(|(_, w)| w.upgrade())
}

/// Repaint every live instance's editor. Used for events that aren't addressed
/// to a particular instance (a legacy untargeted push); anything that *is*
/// addressed should wake only its own editor via
/// [`SynthComputeEngine::wake_editor`].
pub(crate) fn wake_all_editors() {
    let engines: Vec<Arc<SynthComputeEngine>> = match INSTANCE_REGISTRY.lock() {
        Ok(mut reg) => {
            reg.retain(|(_, w)| w.strong_count() > 0);
            reg.iter().filter_map(|(_, w)| w.upgrade()).collect()
        }
        Err(_) => return,
    };
    // Wake outside the registry lock: `request_repaint` calls into egui.
    for engine in engines {
        engine.wake_editor();
    }
}

/// Tag the next instance the host creates with `token`, so it can later be
/// addressed by [`lesynth_fourier_export_dims`] / `_export_grid` / `_import_grid`.
/// Call this immediately before instantiating the plugin.
#[no_mangle]
pub extern "C" fn lesynth_fourier_prepare_instance(token: u64) {
    set_pending_token(token);
}

/// Report the dimensions and metadata of a tagged instance's current grid, so
/// the host can size its buffers before calling [`lesynth_fourier_export_grid`].
/// `out_display_gain` is the display normalisation the grid carries (`0.0` =
/// unknown); save it with the grid or the source's absolute level is lost and a
/// reloaded track can only be auditioned at the grid's own level.
/// Returns 0 on success, or a negative value if the token is unknown/dead. Any
/// out pointer may be null (that field is then skipped).
///
/// # Safety
/// Each non-null out pointer must be valid for a single write of its type.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_export_dims(
    token: u64,
    out_num_harmonics: *mut u32,
    out_num_buckets: *mut u32,
    out_base_freq: *mut f32,
    out_duration: *mut f32,
    out_sample_rate: *mut f32,
    out_display_gain: *mut f32,
) -> i64 {
    let Some(engine) = lookup_instance(token) else {
        return -1;
    };
    let sp = &engine.shared_params;
    let (nh, nb) = {
        let amp = sp.amplitude_data.lock().unwrap();
        (amp.len(), amp.first().map(|r| r.len()).unwrap_or(0))
    };
    if !out_num_harmonics.is_null() {
        *out_num_harmonics = nh as u32;
    }
    if !out_num_buckets.is_null() {
        *out_num_buckets = nb as u32;
    }
    if !out_base_freq.is_null() {
        *out_base_freq = *sp.analysis_base_freq.lock().unwrap();
    }
    if !out_duration.is_null() {
        *out_duration = *sp.analysis_duration_secs.lock().unwrap();
    }
    if !out_sample_rate.is_null() {
        // The rate the audio was *analysed* at, which is the source file's — not
        // the playback device's, which is what this used to report. The exact
        // inverse's bucket lengths are in these samples, so a saved track that
        // recorded the wrong rate here reloads playing at the wrong pitch.
        let analysed = *sp.analysis_sample_rate.lock().unwrap();
        *out_sample_rate = if analysed > 0.0 {
            analysed
        } else {
            *sp.sample_rate.lock().unwrap()
        };
    }
    if !out_display_gain.is_null() {
        *out_display_gain = *sp.analysis_display_gain.lock().unwrap();
    }
    0
}

/// Copy a tagged instance's live grid into host buffers sized for `nh * nb`
/// (amp/phase) and `nb` (pitch ratio) — the `nh`/`nb` returned by
/// [`lesynth_fourier_export_dims`]. Values outside the current grid are written
/// as 0 (amp/phase) or 1.0 (ratio), so a grid that shrank between the two calls
/// never overflows the host buffers. Returns `nb`, or negative on error.
///
/// # Safety
/// `out_amp`/`out_phase` must each be valid for `nh * nb` writes and
/// `out_pitch_ratio` for `nb` writes.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_export_grid(
    token: u64,
    nh: u32,
    nb: u32,
    out_amp: *mut f32,
    out_phase: *mut f32,
    out_pitch_ratio: *mut f32,
    out_bucket_lengths: *mut u32,
    out_dc: *mut f32,
    out_nyquist: *mut f32,
) -> i64 {
    if out_amp.is_null() || out_phase.is_null() || out_pitch_ratio.is_null() {
        return -1;
    }
    let Some(engine) = lookup_instance(token) else {
        return -2;
    };
    let (nh, nb) = (nh as usize, nb as usize);
    let sp = &engine.shared_params;
    let amp = sp.amplitude_data.lock().unwrap();
    let phase = sp.phase_data.lock().unwrap();
    let ratio = sp.bucket_pitch_ratio.lock().unwrap();

    let amp_out = std::slice::from_raw_parts_mut(out_amp, nh * nb);
    let phase_out = std::slice::from_raw_parts_mut(out_phase, nh * nb);
    for h in 0..nh {
        for b in 0..nb {
            amp_out[h * nb + b] = amp.get(h).and_then(|r| r.get(b)).copied().unwrap_or(0.0);
            phase_out[h * nb + b] = phase.get(h).and_then(|r| r.get(b)).copied().unwrap_or(0.0);
        }
    }
    // The exact inverse's extra state. All three are written together or not at
    // all: a grid with lengths but no DC would claim an exactness it cannot
    // deliver. Zero lengths mean "this grid was not analysed period-synchronously"
    // and the reader must fall back to the transposing renderer.
    if !out_bucket_lengths.is_null() && !out_dc.is_null() && !out_nyquist.is_null() {
        let lens = sp.analysis_bucket_lengths.lock().unwrap();
        let dc = sp.analysis_dc.lock().unwrap();
        let nyq = sp.analysis_nyquist.lock().unwrap();
        let exact = lens.len() >= nb && dc.len() >= nb && nyq.len() >= nb;
        let len_out = std::slice::from_raw_parts_mut(out_bucket_lengths, nb);
        let dc_out = std::slice::from_raw_parts_mut(out_dc, nb);
        let nyq_out = std::slice::from_raw_parts_mut(out_nyquist, nb);
        for b in 0..nb {
            len_out[b] = if exact { lens[b] as u32 } else { 0 };
            dc_out[b] = if exact { dc[b] } else { 0.0 };
            nyq_out[b] = if exact { nyq[b] } else { 0.0 };
        }
    }
    let ratio_out = std::slice::from_raw_parts_mut(out_pitch_ratio, nb);
    for b in 0..nb {
        ratio_out[b] = ratio.get(b).copied().unwrap_or(1.0);
    }
    nb as i64
}

/// Load a saved grid into a tagged instance (Analysis mode), bypassing DFT
/// analysis. `amp`/`phase` are row-major `[h*nb + b]`; `pitch_ratio` is `nb`
/// long. `sample_rate` is accepted for format completeness but not applied — the
/// instance keeps the host device rate so playback duration stays correct.
/// `display_gain` is the value [`lesynth_fourier_export_dims`] reported when the
/// grid was saved; `0.0` (unknown) leaves the Original Pitch And Gain audition
/// at the grid's own level instead of restoring the source's.
/// Returns 0 on success, negative on error.
///
/// # Safety
/// `amp`/`phase` must point to `nh * nb` valid `f32`s and `pitch_ratio` to `nb`.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_import_grid(
    token: u64,
    nh: u32,
    nb: u32,
    base_freq: f32,
    duration_secs: f32,
    sample_rate: f32,
    display_gain: f32,
    amp: *const f32,
    phase: *const f32,
    pitch_ratio: *const f32,
    bucket_lengths: *const u32,
    dc: *const f32,
    nyquist: *const f32,
) -> i64 {
    if amp.is_null() || phase.is_null() || pitch_ratio.is_null() {
        return -1;
    }
    let Some(engine) = lookup_instance(token) else {
        return -2;
    };
    let (nh, nb) = (nh as usize, nb as usize);
    if nh == 0 || nb == 0 {
        return -3;
    }
    let amp = std::slice::from_raw_parts(amp, nh * nb);
    let phase = std::slice::from_raw_parts(phase, nh * nb);
    let ratio = std::slice::from_raw_parts(pitch_ratio, nb);

    let amplitude: Vec<Vec<f32>> = (0..nh).map(|h| amp[h * nb..(h + 1) * nb].to_vec()).collect();
    let phase_v: Vec<Vec<f32>> = (0..nh).map(|h| phase[h * nb..(h + 1) * nb].to_vec()).collect();

    // Restoring the exact inverse's state needs all three, and needs the lengths
    // to be real. A file saved before version 3, or a hand-drawn grid, supplies
    // none of it (or zeroed lengths) and simply loads without exactness.
    let exact = !bucket_lengths.is_null() && !dc.is_null() && !nyquist.is_null();
    let lens: Vec<usize> = if exact {
        std::slice::from_raw_parts(bucket_lengths, nb)
            .iter()
            .map(|&v| v as usize)
            .collect()
    } else {
        Vec::new()
    };
    let usable = exact && lens.iter().all(|&n| n >= 2);
    let (lens, dc_v, nyq_v) = if usable {
        (
            lens,
            std::slice::from_raw_parts(dc, nb).to_vec(),
            std::slice::from_raw_parts(nyquist, nb).to_vec(),
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new())
    };

    engine.load_grid(
        amplitude,
        phase_v,
        ratio.to_vec(),
        base_freq,
        duration_secs,
        // The rate the grid was *analysed* at. It used to be ignored on import
        // (`_sample_rate`), which was harmless while nothing depended on it;
        // the exact inverse does, because its bucket lengths are in those
        // samples and mean the wrong pitch at any other rate.
        sample_rate,
        display_gain,
        lens,
        dc_v,
        nyq_v,
    );
    // Repaint that instance's idle editor so the loaded grid appears immediately.
    engine.wake_editor();
    0
}

/// Push a subtrack to be analysed by the instance tagged with `token` (see
/// [`lesynth_fourier_prepare_instance`]). The job waits in that instance alone,
/// so it is still there when its editor opens and cannot be swallowed by another
/// open editor. Returns 0 on success, or a negative value on bad input (-1) or
/// an unknown/dead token (-2).
///
/// `contour`/`contour_len` are the host's per-position fundamental (absolute Hz,
/// uniformly resampled across the subtrack); pass `null`/`0` for flat (legacy).
///
/// # Safety
/// `samples` must point to `len` valid `f32`s; `contour`, if non-null, to
/// `contour_len` valid `f32`s.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_push_analysis_to(
    token: u64,
    samples: *const f32,
    len: usize,
    sample_rate: f32,
    base_freq: f32,
    contour: *const f32,
    contour_len: usize,
) -> i64 {
    if samples.is_null() || len == 0 {
        return -1;
    }
    let Some(engine) = lookup_instance(token) else {
        return -2;
    };
    let slice = std::slice::from_raw_parts(samples, len);
    let contour = if contour.is_null() || contour_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(contour, contour_len).to_vec()
    };
    engine.push_analysis_job(AnalysisJob {
        samples: slice.to_vec(),
        sample_rate,
        base_freq,
        contour,
    });
    // Wake only this instance's editor — it is the only one that can claim the
    // job, and waking the others would just burn frames.
    engine.wake_editor();
    0
}

/// Push a subtrack to be analysed by the next available plugin instance.
/// Returns the new queue depth (0 on invalid input).
///
/// **Legacy — prefer [`lesynth_fourier_push_analysis_to`].** The job is not
/// addressed to any instance, so with several editors open whichever paints
/// first claims it, and the instance the host meant to fill stays empty.
///
/// `contour`/`contour_len` are the host's per-position fundamental (absolute Hz,
/// uniformly resampled across the subtrack); pass `null`/`0` for flat (legacy).
///
/// # Safety
/// `samples` must point to `len` valid `f32`s; `contour`, if non-null, to
/// `contour_len` valid `f32`s.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_push_analysis(
    samples: *const f32,
    len: usize,
    sample_rate: f32,
    base_freq: f32,
    contour: *const f32,
    contour_len: usize,
) -> u64 {
    if samples.is_null() || len == 0 {
        return 0;
    }
    let slice = std::slice::from_raw_parts(samples, len);
    let contour = if contour.is_null() || contour_len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(contour, contour_len).to_vec()
    };
    let job = AnalysisJob {
        samples: slice.to_vec(),
        sample_rate,
        base_freq,
        contour,
    };
    let depth = match ANALYSIS_INBOX.lock() {
        Ok(mut q) => {
            q.push_back(job);
            q.len() as u64
        }
        Err(_) => 0,
    };
    // No instance is named, so wake every editor — any of them may claim it.
    wake_all_editors();
    depth
}

/// Stateless harmonic analysis, for the host's own preview plotting.
///
/// Writes `num_harmonics * num_buckets` floats (row-major, `[h*num_buckets+b]`)
/// into `out_amp` and `out_phase`. Returns the number of buckets written, or a
/// negative value on bad arguments.
///
/// `contour`/`contour_len` are the host's per-position fundamental (absolute Hz,
/// uniformly resampled across the subtrack); pass `null`/`0` for flat (legacy).
/// `num_buckets` is the fixed grid the caller allocated for (must be > 0 here,
/// since the output buffers are sized to it).
///
/// # Safety
/// `samples` must point to `len` valid `f32`s; `contour`, if non-null, to
/// `contour_len` valid `f32`s; `out_amp`/`out_phase` must each have room for
/// `num_harmonics * num_buckets` `f32`s.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_analyze(
    samples: *const f32,
    len: usize,
    sample_rate: f32,
    base_freq: f32,
    contour: *const f32,
    contour_len: usize,
    num_buckets: usize,
    num_harmonics: usize,
    out_amp: *mut f32,
    out_phase: *mut f32,
) -> i64 {
    if samples.is_null() || out_amp.is_null() || out_phase.is_null() || num_buckets == 0 {
        return -1;
    }
    let slice = std::slice::from_raw_parts(samples, len);
    let contour = if contour.is_null() || contour_len == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(contour, contour_len)
    };
    let mut result = engine::analyze_subtrack(
        slice,
        sample_rate,
        base_freq,
        contour,
        num_buckets,
        num_harmonics,
        num_buckets,
    );
    // Match what the plugin's charts show (see analyze_and_load). This entry
    // point feeds the host's *preview* chart only, so the gain is genuinely not
    // needed here — `lesynth_fourier_analyze_full` is the one that reports it.
    let _ = engine::normalize_for_display(&mut result, 0.9);
    let nb = result.num_buckets();
    let nh = result.num_harmonics();
    let amp_out = std::slice::from_raw_parts_mut(out_amp, num_harmonics * num_buckets);
    let phase_out = std::slice::from_raw_parts_mut(out_phase, num_harmonics * num_buckets);
    for h in 0..num_harmonics.min(nh) {
        for b in 0..num_buckets.min(nb) {
            amp_out[h * num_buckets + b] = result.amplitude[h][b];
            phase_out[h * num_buckets + b] = result.phase[h][b];
        }
    }
    nb as i64
}

/// Full harmonic analysis: the amp/phase grids *plus* the per-bucket pitch
/// ratio, bucket period and non-harmonic bins that [`lesynth_fourier_analyze`]
/// drops. Exactly what `SynthComputeEngine::analyze_and_load` performs, display
/// normalisation included, so a host can reproduce the plugin's grid.
///
/// `out_display_gain` receives that normalisation's gain
/// (`grid_amplitude = source_amplitude × gain`); pass it back to reproduce the
/// source's own absolute level, or a quiet recording plays ~19 dB hot.
///
/// The bucket count is derived from the source, so use the **two-call
/// protocol**: call with the grid out pointers null to get `nb`, allocate, then
/// call again with `cap_buckets = nb`. The analysis is deterministic.
///
/// Returns `nb`, or a negative value: `-1` bad arguments, `-3` `cap_buckets`
/// smaller than the grid the analysis produced (nothing is written).
///
/// * `num_buckets` – fixed bucket count, or `0` for period-synchronous (what the
///   plugin itself uses).
/// * `max_buckets` – upper bound on the derived count; `0` → the engine default.
///
/// # Safety
/// `samples` must point to `len` valid `f32`s; `contour`, if non-null, to
/// `contour_len` valid `f32`s. When non-null, `out_amp`/`out_phase` must each
/// have room for `num_harmonics * cap_buckets` `f32`s and
/// `out_pitch_ratio`/`out_bucket_periods` for `cap_buckets` each.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_analyze_full(
    samples: *const f32,
    len: usize,
    sample_rate: f32,
    base_freq: f32,
    contour: *const f32,
    contour_len: usize,
    num_buckets: usize,
    num_harmonics: usize,
    max_buckets: usize,
    cap_buckets: usize,
    out_amp: *mut f32,
    out_phase: *mut f32,
    out_pitch_ratio: *mut f32,
    out_bucket_periods: *mut f32,
    out_display_gain: *mut f32,
    out_dc: *mut f32,
    out_nyquist: *mut f32,
) -> i64 {
    if samples.is_null() || num_harmonics == 0 {
        return -1;
    }
    let slice = std::slice::from_raw_parts(samples, len);
    let contour = if contour.is_null() || contour_len == 0 {
        &[][..]
    } else {
        std::slice::from_raw_parts(contour, contour_len)
    };
    let max_buckets = if max_buckets == 0 {
        (crate::constants::NUM_OF_BUCKETS_MAX as usize).max(num_buckets)
    } else {
        max_buckets
    };
    let mut result = engine::analyze_subtrack(
        slice,
        sample_rate,
        base_freq,
        contour,
        num_buckets,
        num_harmonics,
        max_buckets,
    );
    // Same display scaling `analyze_and_load` applies before the grid reaches
    // the charts and playback — reported, not hidden, so the caller can undo it.
    let display_gain = engine::normalize_for_display(&mut result, 0.9);
    if !out_display_gain.is_null() {
        *out_display_gain = display_gain;
    }
    let nb = result.num_buckets();

    let probing = out_amp.is_null()
        && out_phase.is_null()
        && out_pitch_ratio.is_null()
        && out_bucket_periods.is_null();
    if probing {
        return nb as i64;
    }
    // Amp and phase share one allocation decision; a half-filled pair is a
    // caller bug, not something to silently skip.
    if out_amp.is_null() != out_phase.is_null() {
        return -1;
    }
    if nb > cap_buckets {
        return -3;
    }

    if !out_amp.is_null() {
        let amp_out = std::slice::from_raw_parts_mut(out_amp, num_harmonics * nb);
        let phase_out = std::slice::from_raw_parts_mut(out_phase, num_harmonics * nb);
        for h in 0..num_harmonics {
            for b in 0..nb {
                amp_out[h * nb + b] = result.amplitude[h][b];
                phase_out[h * nb + b] = result.phase[h][b];
            }
        }
    }
    if !out_pitch_ratio.is_null() {
        std::slice::from_raw_parts_mut(out_pitch_ratio, nb)
            .copy_from_slice(&result.pitch_ratio[..nb]);
    }
    if !out_bucket_periods.is_null() {
        std::slice::from_raw_parts_mut(out_bucket_periods, nb)
            .copy_from_slice(&result.bucket_periods[..nb]);
    }
    // The two non-harmonic bins. They have no row in the grid and no place on
    // the charts, but the inverse transform is not exact without them: dropping
    // DC alone takes the reconstruction from -127 dB to -6.5 dB of error,
    // because one pitch period of real audio does not have zero mean and the
    // omission puts a step at every period boundary.
    if !out_dc.is_null() {
        std::slice::from_raw_parts_mut(out_dc, nb).copy_from_slice(&result.dc[..nb]);
    }
    if !out_nyquist.is_null() {
        std::slice::from_raw_parts_mut(out_nyquist, nb).copy_from_slice(&result.nyquist[..nb]);
    }
    nb as i64
}

/// Reproduce the analysed source exactly, inverting the grid bucket by bucket
/// ([`engine::resynthesize_exact`]) — the counterpart of
/// [`lesynth_fourier_analyze_full`] and the one to use for "play the source
/// back". Output length is `Σ bucket_lengths × rate_ratio`. Use
/// [`lesynth_fourier_resynthesize`] to hear the grid *transposed* onto a key,
/// which has to resample and cannot be exact.
///
/// `amp`/`phase` are row-major `[h * num_buckets + b]`; `dc`/`nyquist` may be
/// null, at the accuracy cost. `display_gain` is divided back out for the
/// source's own absolute level (`0` = leave the grid's level alone).
///
/// `rate_ratio` is `output_rate / analysis_rate`; `1.0` reproduces the source
/// exactly. `bucket_lengths` are in the *file's* sample rate, so a stream at any
/// other rate needs this or the note plays at the wrong pitch and length.
///
/// # Safety
/// `amp`/`phase` must point to `num_harmonics * num_buckets` valid `f32`s;
/// `bucket_lengths` to `num_buckets` valid `u32`s; `dc`/`nyquist`, if non-null,
/// to `num_buckets` valid `f32`s; `out`, if non-null, to `out_cap` writable ones.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_resynthesize_exact(
    num_harmonics: usize,
    num_buckets: usize,
    amp: *const f32,
    phase: *const f32,
    bucket_lengths: *const u32,
    dc: *const f32,
    nyquist: *const f32,
    display_gain: f32,
    rate_ratio: f32,
    out: *mut f32,
    out_cap: usize,
) -> i64 {
    if amp.is_null() || phase.is_null() || bucket_lengths.is_null() {
        return -1;
    }
    if num_harmonics == 0 || num_buckets == 0 {
        return -2;
    }
    let amp = std::slice::from_raw_parts(amp, num_harmonics * num_buckets);
    let phase = std::slice::from_raw_parts(phase, num_harmonics * num_buckets);
    let lens: Vec<usize> = std::slice::from_raw_parts(bucket_lengths, num_buckets)
        .iter()
        .map(|&v| v as usize)
        .collect();
    let dc = if dc.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(dc, num_buckets).to_vec()
    };
    let nyq = if nyquist.is_null() {
        Vec::new()
    } else {
        std::slice::from_raw_parts(nyquist, num_buckets).to_vec()
    };

    let amplitude: Vec<Vec<f32>> = (0..num_harmonics)
        .map(|h| amp[h * num_buckets..(h + 1) * num_buckets].to_vec())
        .collect();
    let phase_v: Vec<Vec<f32>> = (0..num_harmonics)
        .map(|h| phase[h * num_buckets..(h + 1) * num_buckets].to_vec())
        .collect();

    // Every harmonic enabled: the grid crossing this ABI comes from a file or a
    // host-side analysis, neither of which carries the editor's per-harmonic
    // checkboxes.
    let sound = engine::resynthesize_exact(
        &amplitude, &phase_v, &lens, &dc, &nyq, &[], &[], display_gain, rate_ratio,
    );
    if !out.is_null() {
        let n = sound.len().min(out_cap);
        std::slice::from_raw_parts_mut(out, n).copy_from_slice(&sound[..n]);
    }
    sound.len() as i64
}

/// Render a grid back to audio through the plugin's own playback path
/// ([`engine::resynthesize_grid`] → `render_key_buffer`), with no instance
/// involved, so a host can regression-test what a note actually sounds like.
///
/// `amp`/`phase` are row-major `[h * num_buckets + b]`; `pitch_ratio` is
/// `num_buckets` long. Pass `base_period = sample_rate / base_freq` **unrounded**
/// — the renderer carries a fractional phase accumulator, and rounding here is
/// the tuning error it exists to avoid.
///
/// Returns the sample count produced: with `out` null it renders without
/// writing, so a caller can size its buffer; otherwise it writes
/// `min(produced, out_cap)` and still returns the full length. Negative on bad
/// arguments.
///
/// * `max_harmonic`   – anti-alias cap; `0` → only the `period / 2` limit.
/// * `target_samples` – `0` = one period per bucket; `> 0` = "preserve seconds".
/// * `display_gain`   – non-zero renders at the source's own absolute level
///   (what Original Pitch And Gain plays); `0` at the grid's own level.
///
/// # Safety
/// `amp`/`phase` must point to `num_harmonics * num_buckets` valid `f32`s and
/// `pitch_ratio` to `num_buckets`; `out`, if non-null, to `out_cap` writable ones.
#[no_mangle]
pub unsafe extern "C" fn lesynth_fourier_resynthesize(
    num_harmonics: usize,
    num_buckets: usize,
    amp: *const f32,
    phase: *const f32,
    pitch_ratio: *const f32,
    base_period: f32,
    max_harmonic: usize,
    target_samples: usize,
    display_gain: f32,
    out: *mut f32,
    out_cap: usize,
) -> i64 {
    if amp.is_null() || phase.is_null() || pitch_ratio.is_null() {
        return -1;
    }
    if num_harmonics == 0 || num_buckets == 0 || !(base_period >= 2.0) {
        return -2;
    }
    let amp = std::slice::from_raw_parts(amp, num_harmonics * num_buckets);
    let phase = std::slice::from_raw_parts(phase, num_harmonics * num_buckets);
    let ratio = std::slice::from_raw_parts(pitch_ratio, num_buckets);

    let amplitude: Vec<Vec<f32>> = (0..num_harmonics)
        .map(|h| amp[h * num_buckets..(h + 1) * num_buckets].to_vec())
        .collect();
    let phase_v: Vec<Vec<f32>> = (0..num_harmonics)
        .map(|h| phase[h * num_buckets..(h + 1) * num_buckets].to_vec())
        .collect();

    let sound = engine::resynthesize_grid(
        &amplitude,
        &phase_v,
        ratio,
        base_period,
        max_harmonic,
        target_samples,
        display_gain,
    );

    if !out.is_null() {
        let n = sound.len().min(out_cap);
        std::slice::from_raw_parts_mut(out, n).copy_from_slice(&sound[..n]);
    }
    sound.len() as i64
}

/// Render a grid **the way a key on the keyboard does** — through the plugin's
/// `PlaybackGrid` and the source's own two clocks.
///
/// This is what [`lesynth_fourier_resynthesize`] cannot be: that one takes a
/// pitch contour and no bucket lengths, so it renders the analysis grid's
/// *rounded* buckets on a uniform time grid. A key does neither, so an offline
/// dump made through it measures a signal nobody listens to — which is how a
/// keyboard defect stays invisible to the buzz tooling. Feed this one to
/// `tools/buzzscan.py` instead when the question is "why does a key buzz".
///
/// Inputs are [`lesynth_fourier_resynthesize_exact`]'s, plus the key: `amp` and
/// `phase` row-major `[h * num_buckets + b]`, `bucket_lengths` in the file's own
/// samples, `dc`/`nyquist` optional. `base_period` is the key's period in
/// **output** samples, fractional; `base_freq` and `analysis_rate` describe the
/// analysis, `out_rate` the render.
///
/// Returns the sample count, writing `min(produced, out_cap)` when `out` is
/// non-null; negative on bad arguments. Falls back to the contour path when the
/// lengths are missing, which is when a key does too.
///
/// # Safety
/// `amp`/`phase` must point to `num_harmonics * num_buckets` valid `f32`s;
/// `bucket_lengths` to `num_buckets` valid `u32`s; `pitch_ratio`, `dc` and
/// `nyquist`, if non-null, to `num_buckets` valid `f32`s; `out`, if non-null, to
/// `out_cap` writable ones.
#[no_mangle]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn lesynth_fourier_resynthesize_key(
    num_harmonics: usize,
    num_buckets: usize,
    amp: *const f32,
    phase: *const f32,
    bucket_lengths: *const u32,
    dc: *const f32,
    nyquist: *const f32,
    pitch_ratio: *const f32,
    base_period: f32,
    base_freq: f32,
    analysis_rate: f32,
    out_rate: f32,
    max_harmonic: usize,
    target_samples: usize,
    display_gain: f32,
    out: *mut f32,
    out_cap: usize,
) -> i64 {
    if amp.is_null() || phase.is_null() || bucket_lengths.is_null() {
        return -1;
    }
    if num_harmonics == 0 || num_buckets == 0 || !(base_period >= 2.0) {
        return -2;
    }
    let amp = std::slice::from_raw_parts(amp, num_harmonics * num_buckets);
    let phase = std::slice::from_raw_parts(phase, num_harmonics * num_buckets);
    let lens: Vec<usize> = std::slice::from_raw_parts(bucket_lengths, num_buckets)
        .iter()
        .map(|&v| v as usize)
        .collect();
    let opt = |p: *const f32| -> Vec<f32> {
        if p.is_null() {
            Vec::new()
        } else {
            std::slice::from_raw_parts(p, num_buckets).to_vec()
        }
    };
    let dc = opt(dc);
    let nyq = opt(nyquist);
    // A flat contour is the honest default: the bucket's own true period already
    // carries the source's pitch movement.
    let ratio = if pitch_ratio.is_null() {
        vec![1.0f32; num_buckets]
    } else {
        std::slice::from_raw_parts(pitch_ratio, num_buckets).to_vec()
    };

    let amplitude: Vec<Vec<f32>> = (0..num_harmonics)
        .map(|h| amp[h * num_buckets..(h + 1) * num_buckets].to_vec())
        .collect();
    let phase_v: Vec<Vec<f32>> = (0..num_harmonics)
        .map(|h| phase[h * num_buckets..(h + 1) * num_buckets].to_vec())
        .collect();

    let sound = engine::resynthesize_key(
        &amplitude, &phase_v, &lens, &dc, &nyq, &ratio, base_period, base_freq,
        analysis_rate, out_rate, max_harmonic, target_samples, display_gain,
    );

    if !out.is_null() {
        let n = sound.len().min(out_cap);
        std::slice::from_raw_parts_mut(out, n).copy_from_slice(&sound[..n]);
    }
    sound.len() as i64
}

/// Serialises tests that touch process-global bridge state — the untargeted
/// inbox and `PENDING_TOKEN` (where a concurrent `prepare_instance` would
/// otherwise be claimed by the wrong test's instance). Poison is ignored: a
/// panicking test shouldn't cascade into unrelated failures.
#[cfg(test)]
static GLOBAL_STATE_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
fn lock_global_state() -> std::sync::MutexGuard<'static, ()> {
    GLOBAL_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod ffi_tests {
    use super::*;

    #[test]
    fn push_analysis_round_trips_contour() {
        let _guard = lock_global_state();
        let samples = vec![0.1f32, 0.2, 0.3, 0.4];
        let contour = vec![440.0f32, 441.0, 439.0];

        // With a contour pointer.
        let depth = unsafe {
            lesynth_fourier_push_analysis(
                samples.as_ptr(),
                samples.len(),
                44_100.0,
                440.0,
                contour.as_ptr(),
                contour.len(),
            )
        };
        assert!(depth >= 1);
        let job = claim_analysis_job().expect("queued job");
        assert_eq!(job.samples, samples);
        assert_eq!(job.base_freq, 440.0);
        assert_eq!(job.contour, contour, "contour must survive the FFI boundary");

        // Null contour → flat (legacy), no crash.
        let depth2 = unsafe {
            lesynth_fourier_push_analysis(
                samples.as_ptr(),
                samples.len(),
                44_100.0,
                440.0,
                std::ptr::null(),
                0,
            )
        };
        assert!(depth2 >= 1);
        let job2 = claim_analysis_job().expect("queued job");
        assert!(job2.contour.is_empty(), "null contour → empty");
    }

    /// A harmonic tone through `analyze_full` and back through `resynthesize`:
    /// the two-call protocol, the per-bucket extras `lesynth_fourier_analyze`
    /// drops, and a reconstruction that carries the same harmonics.
    fn tone(sr: f32, f: f32, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let x = 2.0 * std::f32::consts::PI * f * i as f32 / sr;
                0.5 * (0.6 * x.sin() + 0.3 * (2.0 * x + 1.0).sin() + 0.15 * (3.0 * x).sin())
            })
            .collect()
    }

    #[test]
    fn analyze_full_two_call_protocol_and_resynthesis() {
        let sr = 22_050.0f32;
        let period = 37usize;
        let f0 = sr / period as f32;
        let nh = 32usize;
        let sig = tone(sr, f0, sr as usize / 2);

        // Probe: all grid out pointers null → the derived bucket count.
        let mut display_gain = 0.0f32;
        let mut probe = |cap, amp: *mut f32, ph: *mut f32, pr: *mut f32, bp: *mut f32| unsafe {
            lesynth_fourier_analyze_full(
                sig.as_ptr(),
                sig.len(),
                sr,
                f0,
                std::ptr::null(),
                0,
                0, // period-synchronous
                nh,
                0,
                cap,
                amp,
                ph,
                pr,
                bp,
                &mut display_gain,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        let null = std::ptr::null_mut();
        let nb = probe(0, null, null, null, null);
        assert!(nb > 10, "probe returned {nb}");
        let nb = nb as usize;

        let mut amp = vec![0.0f32; nh * nb];
        let mut phase = vec![0.0f32; nh * nb];
        let mut ratio = vec![0.0f32; nb];
        let mut periods = vec![0.0f32; nb];

        // A capacity below the derived count must be refused, not overrun.
        assert_eq!(
            probe(
                nb - 1,
                amp.as_mut_ptr(),
                phase.as_mut_ptr(),
                ratio.as_mut_ptr(),
                periods.as_mut_ptr()
            ),
            -3
        );
        // Half a pair is a caller bug.
        assert_eq!(probe(nb, amp.as_mut_ptr(), null, null, null), -1);

        assert_eq!(
            probe(
                nb,
                amp.as_mut_ptr(),
                phase.as_mut_ptr(),
                ratio.as_mut_ptr(),
                periods.as_mut_ptr()
            ),
            nb as i64
        );

        // Flat analysis → ratios of 1 everywhere. `pitch_ratio` tracks pitch, so
        // it stays flat even where a bucket's *length* does not.
        assert!(ratio.iter().all(|&r| (r - 1.0).abs() < 1e-6), "{ratio:?}");
        // `bucket_periods` is each bucket's inverse-FFT length in whole samples,
        // so it lands on the period (rounded) for every full bucket. The last one
        // absorbs the remainder and is short by design — that is what makes the
        // buckets tile the subtrack exactly, which is what makes the transform
        // invertible.
        assert!(
            periods[..periods.len() - 1]
                .iter()
                .all(|&p| (p - period as f32).abs() < 1.0),
            "{periods:?}"
        );
        assert_eq!(
            periods.iter().sum::<f32>() as usize,
            sig.len(),
            "buckets must account for every sample of the subtrack"
        );
        // The 3 harmonics that are there, in the right proportions; nothing above.
        let mid = nb / 2;
        let h1 = amp[mid];
        assert!(h1 > 0.1, "fundamental {h1}");
        assert!((amp[nb + mid] / h1 - 0.5).abs() < 0.02);
        assert!((amp[2 * nb + mid] / h1 - 0.25).abs() < 0.02);
        // H4 is not in the source, so it reads at the transform's noise floor
        // rather than an enforced 0.0 — the amplitude gate that used to round it
        // down is gone, because rounding is exactly what breaks invertibility.
        assert!(amp[3 * nb + mid] < 1e-4, "H4 leaked: {}", amp[3 * nb + mid]);

        // The display gain is reported, and it is the real one: the source's
        // strongest harmonic is 0.3, scaled to the chart target of 0.9.
        assert!(
            (display_gain - 3.0).abs() < 0.05,
            "display gain {display_gain} should be 0.9 / 0.3"
        );

        // Resynthesis: probe the length, then render.
        let render = |gain: f32, out: *mut f32, cap: usize| unsafe {
            lesynth_fourier_resynthesize(
                nh,
                nb,
                amp.as_ptr(),
                phase.as_ptr(),
                ratio.as_ptr(),
                period as f32,
                0,
                sig.len(),
                gain,
                out,
                cap,
            )
        };
        let len = render(0.0, std::ptr::null_mut(), 0);
        assert!(len >= sig.len() as i64, "produced {len}, source {}", sig.len());
        let mut out = vec![0.0f32; len as usize];
        assert_eq!(render(0.0, out.as_mut_ptr(), out.len()), len);
        assert!(
            out.iter().fold(0.0f32, |m, &x| m.max(x.abs())) > 0.05,
            "reconstruction is silent"
        );

        // Passing the reported gain back reproduces the source's own level. The
        // grid is normalised for chart legibility, so without this the
        // reconstruction plays back several times too loud — the defect that
        // made the Original Pitch And Gain audition sound harsh next to its source.
        let rms = |x: &[f32]| {
            (x.iter().map(|&v| (v as f64) * (v as f64)).sum::<f64>() / x.len().max(1) as f64).sqrt()
        };
        let mut restored = vec![0.0f32; len as usize];
        assert_eq!(
            render(display_gain, restored.as_mut_ptr(), restored.len()),
            len
        );
        let n = sig.len().min(restored.len());
        let db = 20.0 * (rms(&restored[..n]) / rms(&sig[..n])).log10();
        assert!(
            db.abs() < 0.5,
            "level-restored resynthesis is {db:+.2} dB off the source"
        );
        // …and the uncompensated render really is the loud one, so the test
        // would notice if the parameter silently stopped doing anything.
        // (+5.6 dB on this synthetic tone: gain 3.0 against a clip-guard divisor
        // of ~1.6. A real recording is far quieter and the offset far larger —
        // 18.9 dB on D5.wav.)
        let raw_db = 20.0 * (rms(&out[..n]) / rms(&sig[..n])).log10();
        assert!(raw_db > 4.0, "display-normalised render only {raw_db:+.2} dB hot");

        // Bad arguments are rejected rather than trusted.
        assert!(render(0.0, std::ptr::null_mut(), 0) > 0);
        assert_eq!(
            unsafe {
                lesynth_fourier_resynthesize(
                    nh,
                    nb,
                    std::ptr::null(),
                    phase.as_ptr(),
                    ratio.as_ptr(),
                    period as f32,
                    0,
                    0,
                    0.0,
                    std::ptr::null_mut(),
                    0,
                )
            },
            -1
        );
        assert_eq!(
            unsafe {
                lesynth_fourier_resynthesize(
                    nh,
                    nb,
                    amp.as_ptr(),
                    phase.as_ptr(),
                    ratio.as_ptr(),
                    1.0, // period < 2
                    0,
                    0,
                    0.0,
                    std::ptr::null_mut(),
                    0,
                )
            },
            -2
        );
    }
}

#[cfg(test)]
mod state_registry_tests {
    use super::*;
    use crate::constants::NUM_HARMONICS;
    use crate::params::LeSynthParams;

    fn new_engine() -> Arc<SynthComputeEngine> {
        Arc::new(SynthComputeEngine::new(Arc::new(LeSynthParams::default())))
    }

    #[test]
    fn prepare_register_lookup_and_prune() {
        let _guard = lock_global_state();
        let engine = new_engine();
        lesynth_fourier_prepare_instance(4242);
        register_new_instance(&engine);

        assert!(lookup_instance(4242).is_some(), "tagged instance resolves");
        assert!(lookup_instance(9999).is_none(), "unknown token → none");

        // Dropping the last strong ref makes the weak entry resolve to none and
        // get pruned (the detached compute thread only holds SharedParams).
        drop(engine);
        assert!(lookup_instance(4242).is_none(), "dead instance pruned");
    }

    #[test]
    fn import_then_export_round_trips_grid() {
        let _guard = lock_global_state();
        let engine = new_engine();
        let token = 7;
        lesynth_fourier_prepare_instance(token);
        register_new_instance(&engine);

        let nh = NUM_HARMONICS;
        let nb = 4usize;
        let mut amp_in = vec![0.0f32; nh * nb];
        let mut phase_in = vec![0.0f32; nh * nb];
        for b in 0..nb {
            amp_in[b] = 0.5; // harmonic 0
            amp_in[nb + b] = 0.25; // harmonic 1
            phase_in[nb + b] = 1.0;
        }
        let ratio_in = vec![1.0f32, 1.01, 0.99, 1.0];
        // The exact-inverse state a version 3 `.lsft` carries.
        let lens_in = vec![200u32, 198, 202, 200];
        let dc_in = vec![0.01f32, -0.02, 0.03, 0.0];
        let nyq_in = vec![0.001f32, 0.0, -0.002, 0.0];

        let rc = unsafe {
            lesynth_fourier_import_grid(
                token,
                nh as u32,
                nb as u32,
                220.0,
                0.75,
                44_100.0,
                2.5, // display gain the grid was captured with
                amp_in.as_ptr(),
                phase_in.as_ptr(),
                ratio_in.as_ptr(),
                lens_in.as_ptr(),
                dc_in.as_ptr(),
                nyq_in.as_ptr(),
            )
        };
        assert_eq!(rc, 0);

        // Dimensions + metadata come back as loaded.
        let (mut o_nh, mut o_nb, mut o_base, mut o_dur, mut o_sr) = (0u32, 0u32, 0f32, 0f32, 0f32);
        let mut o_gain = 0f32;
        let dc = unsafe {
            lesynth_fourier_export_dims(
                token, &mut o_nh, &mut o_nb, &mut o_base, &mut o_dur, &mut o_sr, &mut o_gain,
            )
        };
        assert_eq!(dc, 0);
        assert_eq!(o_nh, nh as u32);
        assert_eq!(o_nb, nb as u32);
        assert_eq!(o_base, 220.0);
        assert!((o_dur - 0.75).abs() < 1e-6);
        // The source level travels with the grid: an imported track can still be
        // auditioned at the level it was analysed at.
        assert_eq!(o_gain, 2.5);

        // The grid itself round-trips byte-for-byte (load copies rows verbatim).
        let mut amp_out = vec![0.0f32; nh * nb];
        let mut phase_out = vec![0.0f32; nh * nb];
        let mut ratio_out = vec![0.0f32; nb];
        let mut lens_out = vec![0u32; nb];
        let mut dc_out = vec![0.0f32; nb];
        let mut nyq_out = vec![0.0f32; nb];
        let gc = unsafe {
            lesynth_fourier_export_grid(
                token,
                nh as u32,
                nb as u32,
                amp_out.as_mut_ptr(),
                phase_out.as_mut_ptr(),
                ratio_out.as_mut_ptr(),
                lens_out.as_mut_ptr(),
                dc_out.as_mut_ptr(),
                nyq_out.as_mut_ptr(),
            )
        };
        assert_eq!(gc, nb as i64);
        assert_eq!(amp_out, amp_in);
        assert_eq!(phase_out, phase_in);
        assert_eq!(ratio_out, ratio_in);
        // …including everything the exact inverse needs, or a saved track could
        // only ever be reloaded into the approximate render path.
        assert_eq!(lens_out, lens_in);
        assert_eq!(dc_out, dc_in);
        assert_eq!(nyq_out, nyq_in);

        drop(engine);
    }

    /// A job pushed for one instance must stay in that instance — a second live
    /// instance (another open track editor) must not be able to claim it.
    #[test]
    fn targeted_push_reaches_only_its_own_instance() {
        let _guard = lock_global_state();
        let target = new_engine();
        let bystander = new_engine();
        lesynth_fourier_prepare_instance(31);
        register_new_instance(&target);
        lesynth_fourier_prepare_instance(32);
        register_new_instance(&bystander);

        let samples = vec![0.1f32, 0.2, 0.3, 0.4];
        let contour = vec![440.0f32, 441.0];
        let rc = unsafe {
            lesynth_fourier_push_analysis_to(
                31,
                samples.as_ptr(),
                samples.len(),
                44_100.0,
                440.0,
                contour.as_ptr(),
                contour.len(),
            )
        };
        assert_eq!(rc, 0);

        // The bystander's editor finds nothing — neither in its own mailbox nor
        // in the untargeted inbox (a targeted push must never land there).
        assert!(bystander.take_analysis_job().is_none());
        assert!(claim_analysis_job().is_none());

        let job = target.take_analysis_job().expect("job waits in its instance");
        assert_eq!(job.samples, samples);
        assert_eq!(job.contour, contour);
        assert!(target.take_analysis_job().is_none(), "claimed exactly once");

        drop((target, bystander));
    }

    #[test]
    fn targeted_push_rejects_unknown_token_and_bad_input() {
        let _guard = lock_global_state();
        let samples = vec![0.1f32, 0.2];
        let unknown = unsafe {
            lesynth_fourier_push_analysis_to(
                654_321,
                samples.as_ptr(),
                samples.len(),
                44_100.0,
                440.0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(unknown, -2, "unknown token must not fall back to the inbox");
        assert!(claim_analysis_job().is_none());

        let engine = new_engine();
        lesynth_fourier_prepare_instance(33);
        register_new_instance(&engine);
        let empty = unsafe {
            lesynth_fourier_push_analysis_to(
                33,
                samples.as_ptr(),
                0,
                44_100.0,
                440.0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(empty, -1, "empty sample slice is rejected");
        assert!(engine.take_analysis_job().is_none());
        drop(engine);
    }

    #[test]
    fn export_unknown_token_errors() {
        let (mut nh, mut nb) = (0u32, 0u32);
        let rc = unsafe {
            lesynth_fourier_export_dims(
                123456,
                &mut nh,
                &mut nb,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert!(rc < 0, "unknown token must error");
    }
}

use std::sync::Once;
static INIT_LOGGER: Once = Once::new();

/// Initialise the plugin's own logger.
///
/// The plugin is a `cdylib` loaded into the host process, but Rust statically
/// links a *private* copy of the `log` crate (and its global logger) into every
/// dynamic library. So the host's logger is unreachable from here — the plugin
/// has to install its own. We route records to `<tmpdir>/lesynth.log` (e.g.
/// `/tmp/lesynth.log`) so they're readable regardless of how the host was
/// launched, and default to `Info` so this works in release builds. `RUST_LOG`
/// can still override the level/filters if set.
pub fn init_logging() {
    INIT_LOGGER.call_once(|| {
        use std::fs::OpenOptions;
        use std::io::Write;

        let log_path = std::env::temp_dir().join("lesynth.log");

        let Ok(file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        else {
            // Nowhere to log to — leave the no-op logger in place rather than
            // crash the host on plugin instantiation.
            return;
        };

        // Session separator, written directly so it isn't prefixed like a record.
        {
            let mut file = &file;
            let _ = writeln!(file, "\n=== LeSynth session started ===");
        }

        let _ = env_logger::Builder::new()
            .filter_level(log::LevelFilter::Info)
            .parse_default_env() // honour RUST_LOG when present
            .format(|buf, record| {
                use std::io::Write;
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                writeln!(
                    buf,
                    "[{}] [{}] [{}:{}] {}",
                    timestamp,
                    record.level(),
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    record.args()
                )
            })
            .target(env_logger::Target::Pipe(Box::new(file)))
            .try_init();

        log::info!("LeSynth logging initialized. Log file: {:?}", log_path);
    });
}

nih_plug::nih_export_vst3!(LeSynth);
nih_plug::nih_export_clap!(LeSynth);
