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
// component instances live in *this* shared object's address space, a global
// inbox here is shared with them: the host pushes a job, the running editor
// claims it and runs the analysis on its own engine.
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

static ANALYSIS_INBOX: Mutex<VecDeque<AnalysisJob>> = Mutex::new(VecDeque::new());

/// Editor egui context, registered so background threads can wake the idle
/// editor (it blocks its event loop when idle) via [`wake_editor`].
static EDITOR_WAKER: Mutex<Option<nih_plug_egui::egui::Context>> = Mutex::new(None);

/// Register the editor's egui context (replacing any previous).
pub(crate) fn register_editor_waker(ctx: nih_plug_egui::egui::Context) {
    if let Ok(mut g) = EDITOR_WAKER.lock() {
        *g = Some(ctx);
    }
}

/// Repaint the registered editor to pick up off-thread state. No-op if none.
pub(crate) fn wake_editor() {
    if let Ok(g) = EDITOR_WAKER.lock() {
        if let Some(ctx) = g.as_ref() {
            ctx.request_repaint();
        }
    }
}

/// Claim the oldest pending analysis job (called by a plugin editor).
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
/// `(token, weak engine)` for every host-tagged live instance. Small (one entry
/// per open editor), pruned of dead entries on every access; a linear scan is fine.
static INSTANCE_REGISTRY: Mutex<Vec<(u64, Weak<SynthComputeEngine>)>> = Mutex::new(Vec::new());

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

/// Register a newly created engine under the pending token, if the host set one.
/// No-op when the plugin is instantiated by a plain host (no token) — e.g. as a
/// normal VST3 in a DAW.
pub(crate) fn register_new_instance(engine: &Arc<SynthComputeEngine>) {
    if let Some(token) = take_pending_token() {
        if let Ok(mut reg) = INSTANCE_REGISTRY.lock() {
            reg.retain(|(_, w)| w.strong_count() > 0);
            reg.push((token, Arc::downgrade(engine)));
        }
    }
}

/// Resolve a token to its live engine, pruning any dead entries en route.
fn lookup_instance(token: u64) -> Option<Arc<SynthComputeEngine>> {
    let mut reg = INSTANCE_REGISTRY.lock().ok()?;
    reg.retain(|(_, w)| w.strong_count() > 0);
    reg.iter()
        .find(|(t, _)| *t == token)
        .and_then(|(_, w)| w.upgrade())
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
        *out_sample_rate = *sp.sample_rate.lock().unwrap();
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
    _sample_rate: f32,
    amp: *const f32,
    phase: *const f32,
    pitch_ratio: *const f32,
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

    engine.load_grid(amplitude, phase_v, ratio.to_vec(), base_freq, duration_secs);
    // Repaint the idle editor so the loaded grid appears immediately.
    wake_editor();
    0
}

/// Push a subtrack to be analysed by the next available plugin instance.
/// Returns the new queue depth (0 on invalid input).
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
    // Wake the idle editor to claim and render the job.
    wake_editor();
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
    // Match what the plugin's charts show (see analyze_and_load).
    engine::normalize_for_display(&mut result, 0.9);
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

#[cfg(test)]
mod ffi_tests {
    use super::*;

    #[test]
    fn push_analysis_round_trips_contour() {
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

        let rc = unsafe {
            lesynth_fourier_import_grid(
                token,
                nh as u32,
                nb as u32,
                220.0,
                0.75,
                44_100.0,
                amp_in.as_ptr(),
                phase_in.as_ptr(),
                ratio_in.as_ptr(),
            )
        };
        assert_eq!(rc, 0);

        // Dimensions + metadata come back as loaded.
        let (mut o_nh, mut o_nb, mut o_base, mut o_dur, mut o_sr) = (0u32, 0u32, 0f32, 0f32, 0f32);
        let dc = unsafe {
            lesynth_fourier_export_dims(
                token, &mut o_nh, &mut o_nb, &mut o_base, &mut o_dur, &mut o_sr,
            )
        };
        assert_eq!(dc, 0);
        assert_eq!(o_nh, nh as u32);
        assert_eq!(o_nb, nb as u32);
        assert_eq!(o_base, 220.0);
        assert!((o_dur - 0.75).abs() < 1e-6);

        // The grid itself round-trips byte-for-byte (load copies rows verbatim).
        let mut amp_out = vec![0.0f32; nh * nb];
        let mut phase_out = vec![0.0f32; nh * nb];
        let mut ratio_out = vec![0.0f32; nb];
        let gc = unsafe {
            lesynth_fourier_export_grid(
                token,
                nh as u32,
                nb as u32,
                amp_out.as_mut_ptr(),
                phase_out.as_mut_ptr(),
                ratio_out.as_mut_ptr(),
            )
        };
        assert_eq!(gc, nb as i64);
        assert_eq!(amp_out, amp_in);
        assert_eq!(phase_out, phase_in);
        assert_eq!(ratio_out, ratio_in);

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
