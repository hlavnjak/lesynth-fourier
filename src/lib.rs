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
use std::sync::Mutex;

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
