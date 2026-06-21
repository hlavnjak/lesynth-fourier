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

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use crate::constants::{NUM_HARMONICS, NUM_OF_BUCKETS_DEFAULT, TWO_PI, NUM_KEYS, max_harmonic_for_key};
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

/// Rendered period length (samples) for `bucket`: the key's base period scaled
/// by the bucket's pitch ratio (clamped ≥ 2). A missing/empty ratio means flat.
fn bucket_period(base_period: usize, ratios: &[f32], bucket: usize) -> usize {
    let r = ratios.get(bucket).copied().unwrap_or(1.0);
    ((base_period as f32 / r.max(1e-3)).round() as usize).max(2)
}

/// Render a key's waveform from an amp/phase grid. This is the single render
/// path shared by Synth and Analysis modes — they must not diverge — and by
/// both the synchronous (GUI) and background-thread callers.
///
/// Each rendered chunk is exactly one fundamental cycle of the played key
/// (`bucket_period`), so every harmonic completes an integer number of cycles
/// and consecutive chunks stay phase-aligned regardless of period length.
///
/// `target_samples` selects the timeline:
/// * `0` → **Synth mode**: render one period per bucket, in order
///   (legacy behaviour; total length = `Σ bucket_period`).
/// * `> 0` → **Analysis mode** ("preserve seconds"): render exactly
///   `target_samples` samples and pick each chunk's bucket by its position in
///   time (`produced / target_samples`). The note then lasts the source's
///   wall-clock duration at *every* key — low keys play few long periods, high
///   keys many short ones — so the bucket count no longer drives buffer length.
///
/// When `cancel` is supplied (background thread) the render bails out early on
/// request and periodically yields so the GUI stays responsive.
fn render_key_buffer(
    num_harmonics: usize,
    ampl: &[Vec<f32>],
    phase: &[Vec<f32>],
    ampl_enabled: &[bool],
    phase_enabled: &[bool],
    base_period: usize,
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

    let mut sound: Vec<f32> = Vec::new();
    let mut produced = 0usize;
    let mut chunk = 0usize;
    let mut last_yield = 0usize;
    loop {
        let bucket = if drive_by_time {
            if produced >= target_samples {
                break;
            }
            (((produced as f32 / target_samples as f32) * num_buckets as f32) as usize)
                .min(num_buckets - 1)
        } else {
            if chunk >= num_buckets {
                break;
            }
            chunk
        };

        if let Some(c) = cancel {
            if c.load(Ordering::Relaxed) {
                return Vec::new();
            }
            // Yield by produced samples (period count varies wildly with key).
            if produced - last_yield >= 8192 {
                thread::sleep(Duration::from_millis(1));
                last_yield = produced;
            }
        }

        let period = bucket_period(base_period, ratios, bucket);
        let max_h = num_harmonics.min(max_harmonic).min(period / 2);
        for t in 0..period {
            let mut sample = 0.0;
            for n in 0..max_h {
                let amp = ampl[n][bucket];
                if !ampl_enabled[n] || amp == 0.0 {
                    continue;
                }
                let ph = if phase_enabled[n] {
                    phase[n][bucket]
                } else {
                    0.0
                };
                sample += amp
                    * (TWO_PI * (n as f32 + 1.0) * (t as f32) / (period as f32) + ph).sin();
            }
            sound.push(sample.clamp(-1.0, 1.0));
        }
        produced += period;
        chunk += 1;
    }
    sound
}

/// Playback length in samples for `key`: `0` in Synth mode (caller renders one
/// period per bucket), or the source's wall-clock duration at the playback
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

#[derive(Clone)]
pub struct SynthComputeEngine {
    synth_params: Arc<LeSynthParams>,
    pub shared_params: Arc<SharedParams>,
}

impl SynthComputeEngine {
    pub fn new(synth_params_p: Arc<LeSynthParams>) -> Self {
        let buckets = NUM_OF_BUCKETS_DEFAULT;
        let engine = Self {
            synth_params: synth_params_p,
            shared_params: Arc::new(SharedParams::new(NUM_HARMONICS, buckets)),
        };
        
        // Start background computation thread
        engine.start_async_computation_thread();
        
        engine
    }

    pub fn fill_constant_curve(&self, n: usize, value: f32, chart_type: ChartType) {
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

    /// Like [`Self::refill_harmonic_curve`] but writes the row only — no
    /// normalize/dirty/chart side effects. Used by bulk operations that batch
    /// those once at the end.
    fn refill_harmonic_curve_quiet(&self, n: usize, chart_type: ChartType) {
        match self.curve_type_of(n, chart_type) {
            CurveType::Constant => {
                self.write_constant_row(n, self.curve_offset_of(n, chart_type), chart_type);
            }
            CurveType::NestedFourier => self.write_nested_fourier_row(n, chart_type),
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

    /// Resize the per-bucket synthesis grid to `new_buckets` and refill every
    /// harmonic's curve. This is the time-resolution of the synthesised
    /// envelope; only meaningful in Synth mode. Analysis mode derives its bucket
    /// count from the analysed audio, so callers must not invoke this while
    /// analysed data is loaded. No-op when the grid is already that size.
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
                row.resize(new_buckets, 0.0);
            }
            for row in phase.iter_mut() {
                row.resize(new_buckets, 0.0);
            }
            *norm = vec![vec![0.0; new_buckets]; amp.len()];
        }
        {
            // Synth mode is flat (no vibrato); keep the ratio grid sized to match.
            let mut ratio = self.shared_params.bucket_pitch_ratio.lock().unwrap();
            ratio.resize(new_buckets, 1.0);
        }

        // Re-render every harmonic's amp & phase row onto the new grid, batching
        // the (expensive) normalize/dirty/chart side effects to a single pass.
        let n_harm = self.synth_params.harmonics.len();
        for n in 0..n_harm {
            self.refill_harmonic_curve_quiet(n, ChartType::Amp);
            self.refill_harmonic_curve_quiet(n, ChartType::Phase);
        }

        self.set_normalization_needed(true);
        self.shared_params.mark_all_buffers_dirty();
        self.update_assembled_chart_with_key24();
    }

    pub fn normalize_amplitude_data(&self) {
        let ampl_data = self.shared_params.amplitude_data.lock().unwrap();
        let mut ampl_data_normalized = self.shared_params.amplitude_data_normalized.lock().unwrap();
        let maximums: Vec<f32> = ampl_data
            .iter()
            .map(|row| row.iter().copied().fold(f32::NEG_INFINITY, f32::max))
            .collect();
        let sum: f32 = maximums.iter().copied().sum();

        // Reallocate if the grid shape changed — the bucket count (inner len)
        // changes when an analysis result is loaded, not just the harmonic
        // count (outer len).
        let shape_changed = ampl_data_normalized.len() != ampl_data.len()
            || ampl_data_normalized.first().map(|r| r.len())
                != ampl_data.first().map(|r| r.len());
        if shape_changed {
            let inner = ampl_data.first().map(|r| r.len()).unwrap_or(0);
            *ampl_data_normalized = vec![vec![0.0; inner]; ampl_data.len()];
        }

        for (a, row) in ampl_data.iter().enumerate() {
            for (b, &val) in row.iter().enumerate() {
                ampl_data_normalized[a][b] = if sum > 1.0 { val / sum } else { val };
            }
        }
    }

    pub fn assemble_buffer_for_key(&self, key: usize) -> Vec<f32> {
        let start_time = std::time::Instant::now();
        
        if *self.shared_params.normalization_needed.lock().unwrap() {
            self.normalize_amplitude_data();
            *self.shared_params.normalization_needed.lock().unwrap() = false;
        }

        let num_harmonics = self.shared_params.amplitude_data.lock().unwrap().len();
        let ampl_data_normalized = self.shared_params.amplitude_data_normalized.lock().unwrap();
        let phase_data = self.shared_params.phase_data.lock().unwrap();
        let piano_periods = self.shared_params.piano_periods.lock().unwrap();
        let base_period = piano_periods[key] as usize;
        // Per-bucket vibrato ratios apply only in Analysis mode; flat otherwise.
        let pitch_ratio = bucket_pitch_ratios(&self.shared_params);
        // Hoist the per-harmonic enable flags out of the hot loops — locking
        // them per sample (as before) cost a mutex round-trip for every output
        // sample, making large analysis buffers crawl.
        let harmonic_ampl_enabled = self.shared_params.harmonic_ampl_enabled.lock().unwrap();
        let harmonic_phase_enabled = self.shared_params.harmonic_phase_enabled.lock().unwrap();

        // Calculate maximum usable harmonic for this key to prevent aliasing
        let max_harmonic = max_harmonic_for_key(key);
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
        log::trace!("assemble_buffer_for_key(key={}) took: {:?} (base_period={}, total_samples={}, max_harmonic={}/{})",
                 key, elapsed, base_period, sound.len(), max_harmonic, num_harmonics);
        
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
            let base_period = piano_periods[key] as usize;

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
        log::trace!("async compute_buffer_for_key(key={}) took: {:?} (base_period={}, total_samples={}, max_harmonic={}/{})",
                 key, elapsed, base_period, sound.len(), max_harmonic, num_harmonics);
        
        sound
    }
    
    /// Static version of normalize_amplitude_data for use in background thread
    fn normalize_amplitude_data_static(shared_params: &Arc<SharedParams>) {
        let amplitude_data = shared_params.amplitude_data.lock().unwrap();
        let mut ampl_data_normalized = shared_params.amplitude_data_normalized.lock().unwrap();

        // Match the (possibly changed) grid shape before copying.
        let shape_changed = ampl_data_normalized.len() != amplitude_data.len()
            || ampl_data_normalized.first().map(|r| r.len())
                != amplitude_data.first().map(|r| r.len());
        if shape_changed {
            let inner = amplitude_data.first().map(|r| r.len()).unwrap_or(0);
            *ampl_data_normalized = vec![vec![0.0; inner]; amplitude_data.len()];
        }

        for a in 0..amplitude_data.len() {
            for b in 0..amplitude_data[a].len() {
                ampl_data_normalized[a][b] = amplitude_data[a][b];
            }
        }
        
        for b in 0..ampl_data_normalized[0].len() {
            let sum: f32 = ampl_data_normalized
                .iter()
                .map(|harmonic| harmonic[b])
                .sum();
            if sum > 1.0 {
                for a in 0..ampl_data_normalized.len() {
                    let val = ampl_data_normalized[a][b];
                    ampl_data_normalized[a][b] = val / sum;
                }
            }
        }
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
        // legible; resynthesis re-normalises separately.
        super::normalize_for_display(&mut result, 0.9);
        // Record the source duration so playback lasts the same wall-clock time
        // at every key (pitch-independent), regardless of the played period.
        let duration_secs = if sample_rate > 0.0 {
            samples.len() as f32 / sample_rate
        } else {
            0.0
        };
        *self.shared_params.analysis_duration_secs.lock().unwrap() = duration_secs;
        // Remember the source fundamental so the GUI can report the original
        // tone's absolute min/max pitch (base_freq * per-bucket pitch ratio).
        *self.shared_params.analysis_base_freq.lock().unwrap() = base_freq.max(0.0);
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

    #[test]
    fn test_engine_creation() {
        let engine = create_test_engine();
        
        // Verify shared params were initialized correctly
        let amp_data = engine.shared_params.amplitude_data.lock().unwrap();
        assert_eq!(amp_data.len(), NUM_HARMONICS);
        assert_eq!(amp_data[0].len(), NUM_OF_BUCKETS_DEFAULT);
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
        let base_period = engine.shared_params.piano_periods.lock().unwrap()[36] as usize;
        let len = engine.assemble_buffer_for_key(36).len();
        assert_eq!(len, buckets * base_period, "synth playback must ignore ratios");
    }


    #[test]
    fn bucket_period_scales_with_ratio() {
        assert_eq!(bucket_period(100, &[1.0], 0), 100); // flat
        assert_eq!(bucket_period(100, &[2.0], 0), 50); // sharper → shorter
        assert_eq!(bucket_period(100, &[0.5], 0), 200); // flatter → longer
        assert_eq!(bucket_period(100, &[], 5), 100); // missing → flat
        assert!(bucket_period(2, &[1000.0], 0) >= 2); // clamped ≥ 2
    }

    #[test]
    fn analysis_pitch_ratio_transposes_playback_period() {
        let engine = create_test_engine();
        let key = 40;
        let buckets = engine.shared_params.amplitude_data.lock().unwrap()[0].len();
        let base_period = engine.shared_params.piano_periods.lock().unwrap()[key] as usize;
        *engine.shared_params.normalization_needed.lock().unwrap() = false;

        // A ratio grid is present, but Synth mode must ignore it (flat playback).
        {
            let mut r = engine.shared_params.bucket_pitch_ratio.lock().unwrap();
            *r = vec![2.0; buckets];
        }
        engine.shared_params.set_execution_mode(ExecutionMode::Synth);
        let synth_len = engine.assemble_buffer_for_key(key).len();
        assert_eq!(synth_len, buckets * base_period, "synth playback must stay flat");

        // Analysis mode applies the ratio: every bucket period scales by 1/ratio.
        engine.shared_params.set_execution_mode(ExecutionMode::Analysis);
        let analysis_len = engine.assemble_buffer_for_key(key).len();
        let ratios = vec![2.0; buckets];
        let expected: usize = (0..buckets)
            .map(|b| bucket_period(base_period, &ratios, b))
            .sum();
        assert_eq!(analysis_len, expected, "ratio should transpose each bucket period");
        assert!(analysis_len < synth_len, "ratio > 1 shortens the rendered note");
    }
}
