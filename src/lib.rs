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
    match ANALYSIS_INBOX.lock() {
        Ok(mut q) => {
            q.push_back(job);
            q.len() as u64
        }
        Err(_) => 0,
    }
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

#[cfg(all(debug_assertions, feature = "debug-logging"))]
use std::sync::Once;
#[cfg(all(debug_assertions, feature = "debug-logging"))]
static INIT_LOGGER: Once = Once::new();

#[cfg(all(debug_assertions, feature = "debug-logging"))]
pub fn init_logging() {
    INIT_LOGGER.call_once(|| {
        use std::fs::OpenOptions;
        use std::io::Write;
        
        let log_file = std::env::temp_dir().join("lesynth.log");
        
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Debug)
            .format(|buf, record| {
                use std::io::Write;
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                writeln!(buf, "[{}] [{}] [{}:{}] {}", 
                    timestamp,
                    record.level(),
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    record.args()
                )
            })
            .init();
            
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file) 
        {
            let _ = writeln!(file, "\n=== LeSynth Debug Session Started ===");
        }
        
        log::info!("LeSynth logging initialized. Log file: {:?}", log_file);
    });
}

#[cfg(not(all(debug_assertions, feature = "debug-logging")))]
pub fn init_logging() {
    // No-op when not in debug build with debug-logging feature
}

nih_plug::nih_export_vst3!(LeSynth);
nih_plug::nih_export_clap!(LeSynth);
