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
use crate::params::LeSynthParams;
use super::{ChartType, SharedParams};
use super::shared_params::BufferState;

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
        let wobble_freq = match chart_type {
            ChartType::Amp => self.synth_params.harmonics[n].wobble_freq_amp.value(),
            ChartType::Phase => self.synth_params.harmonics[n].wobble_freq_phase.value(),
        };

        let mut data = match chart_type {
            ChartType::Amp => self.shared_params.amplitude_data.lock().unwrap(),
            ChartType::Phase => self.shared_params.phase_data.lock().unwrap(),
        };
        
        let needs_update = data[n][0] != value || wobble_amp > 0.0;
        if needs_update {
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
            self.set_normalization_needed(true);
            // Mark all buffers as dirty since harmonic parameters changed
            drop(data); // Release the lock before calling mark_all_buffers_dirty
            self.shared_params.mark_all_buffers_dirty();
            // Update assembled chart with key 24 for immediate preview
            self.update_assembled_chart_with_key24();
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

    pub fn normalize_amplitude_data(&self) {
        let ampl_data = self.shared_params.amplitude_data.lock().unwrap();
        let mut ampl_data_normalized = self.shared_params.amplitude_data_normalized.lock().unwrap();
        let maximums: Vec<f32> = ampl_data
            .iter()
            .map(|row| row.iter().copied().fold(f32::NEG_INFINITY, f32::max))
            .collect();
        let sum: f32 = maximums.iter().copied().sum();

        if ampl_data_normalized.len() != ampl_data.len() {
            *ampl_data_normalized = vec![vec![0.0; ampl_data[0].len()]; ampl_data.len()];
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
        let period = piano_periods[key] as usize;

        // Calculate maximum usable harmonic for this key to prevent aliasing
        let max_harmonic = max_harmonic_for_key(key);

        let mut sound = Vec::new();
        for bucket in 0..ampl_data_normalized[0].len() {
            for t in 0..period {
                let mut sample = 0.0;
                let harmonic_ampl_enabled = self.shared_params.harmonic_ampl_enabled.lock().unwrap();
                let harmonic_phase_enabled = self.shared_params.harmonic_phase_enabled.lock().unwrap();
                for n in 0..num_harmonics.min(max_harmonic) {
                    let amp = ampl_data_normalized[n][bucket];
                    if !harmonic_ampl_enabled[n] || amp == 0.0 {
                        continue;
                    }
                    let phase = if harmonic_phase_enabled[n] {
                        phase_data[n][bucket]
                    } else {
                        0.0
                    };
                    sample += amp
                        * (TWO_PI * (n as f32 + 1.0) * (t as f32) / (period as f32) + phase).sin();
                }
                sound.push(sample.clamp(-1.0, 1.0));
            }
        }
        
        let elapsed = start_time.elapsed();
        log::trace!("assemble_buffer_for_key(key={}) took: {:?} (period={}, total_samples={}, max_harmonic={}/{})",
                 key, elapsed, piano_periods[key], sound.len(), max_harmonic, num_harmonics);
        
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
        let (num_harmonics, ampl_data_copy, phase_data_copy, harmonic_ampl_enabled_copy, harmonic_phase_enabled_copy, period) = {
            let ampl_data_normalized = shared_params.amplitude_data_normalized.lock().unwrap();
            let phase_data = shared_params.phase_data.lock().unwrap();
            let piano_periods = shared_params.piano_periods.lock().unwrap();
            let harmonic_ampl_enabled = shared_params.harmonic_ampl_enabled.lock().unwrap();
            let harmonic_phase_enabled = shared_params.harmonic_phase_enabled.lock().unwrap();
            
            let num_harmonics = ampl_data_normalized.len();
            let period = piano_periods[key] as usize;
            
            // Deep copy the data we need
            let ampl_data_copy: Vec<Vec<f32>> = ampl_data_normalized.clone();
            let phase_data_copy: Vec<Vec<f32>> = phase_data.clone();
            let harmonic_ampl_enabled_copy: Vec<bool> = harmonic_ampl_enabled.clone();
            let harmonic_phase_enabled_copy: Vec<bool> = harmonic_phase_enabled.clone();
            
            (num_harmonics, ampl_data_copy, phase_data_copy, harmonic_ampl_enabled_copy, harmonic_phase_enabled_copy, period)
        }; // All locks are released here
        
        let mut sound = Vec::new();
        for bucket in 0..ampl_data_copy[0].len() {
            // Check for cancellation periodically
            if shared_params.computation_cancel.load(Ordering::Relaxed) {
                log::debug!("Computation cancelled for key {} during bucket {}", key, bucket);
                return Vec::new(); // Return empty buffer on cancellation
            }
            
            // Yield to other threads every few buckets to keep GUI responsive
            if bucket % 10 == 0 && bucket > 0 {
                thread::sleep(Duration::from_millis(1));
            }
            
            for t in 0..period {
                let mut sample = 0.0;
                for n in 0..num_harmonics.min(max_harmonic) {
                    let amp = ampl_data_copy[n][bucket];
                    if !harmonic_ampl_enabled_copy[n] || amp == 0.0 {
                        continue;
                    }
                    let phase = if harmonic_phase_enabled_copy[n] {
                        phase_data_copy[n][bucket]
                    } else {
                        0.0
                    };
                    sample += amp
                        * (TWO_PI * (n as f32 + 1.0) * (t as f32) / (period as f32) + phase).sin();
                }
                sound.push(sample.clamp(-1.0, 1.0));
            }
        }
        
        let elapsed = start_time.elapsed();
        log::trace!("async compute_buffer_for_key(key={}) took: {:?} (period={}, total_samples={}, max_harmonic={}/{})",
                 key, elapsed, period, sound.len(), max_harmonic, num_harmonics);
        
        sound
    }
    
    /// Static version of normalize_amplitude_data for use in background thread
    fn normalize_amplitude_data_static(shared_params: &Arc<SharedParams>) {
        let amplitude_data = shared_params.amplitude_data.lock().unwrap();
        let mut ampl_data_normalized = shared_params.amplitude_data_normalized.lock().unwrap();
        
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
}
