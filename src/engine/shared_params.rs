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

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use crate::constants::NUM_KEYS;
use crate::voice::Voice;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BufferState {
    Clean,     // Buffer is ready to use
    Dirty,     // Buffer needs recomputation
    Computing, // Buffer is currently being computed
}

#[derive(Clone)]
pub struct SharedParams {
    pub amplitude_data: Arc<Mutex<Vec<Vec<f32>>>>,
    pub amplitude_data_normalized: Arc<Mutex<Vec<Vec<f32>>>>,
    pub phase_data: Arc<Mutex<Vec<Vec<f32>>>>,
    pub voices: Arc<Mutex<Vec<Option<Voice>>>>,
    pub assembled_sound_plotted: Arc<Mutex<Vec<f32>>>,
    pub piano_periods: Arc<Mutex<Vec<u32>>>,
    pub normalization_needed: Arc<Mutex<bool>>,
    pub harmonic_ampl_enabled: Arc<Mutex<Vec<bool>>>,
    pub harmonic_phase_enabled: Arc<Mutex<Vec<bool>>>,
    pub fade_duration: usize,
    
    // Async buffer computation
    pub key_buffers: Arc<Mutex<Vec<Option<Vec<f32>>>>>,
    pub buffer_states: Arc<Mutex<Vec<BufferState>>>,
    pub computation_cancel: Arc<AtomicBool>,
    
    // Chart view control
    pub should_reset_chart_view: Arc<AtomicBool>,
}

impl SharedParams {
    pub fn new(num_harmonics: usize, buckets: usize) -> Self {
        Self {
            // 2D arrays for amplitude and phase data:
            // dimensions: [points_per_period/2] x [num_buckets]
            amplitude_data: Arc::new(Mutex::new(vec![vec![0.0; buckets]; num_harmonics])),
            amplitude_data_normalized: Arc::new(Mutex::new(vec![vec![0.0; buckets]; num_harmonics])),
            phase_data: Arc::new(Mutex::new(vec![vec![0.0; buckets]; num_harmonics])),
            voices: Arc::new(Mutex::new(vec![None; NUM_KEYS])),
            assembled_sound_plotted: Arc::new(Mutex::new(Vec::new())),
            piano_periods: Arc::new(Mutex::new(Self::populate_piano_periods())),
            normalization_needed: Arc::new(Mutex::new(false)),
            harmonic_ampl_enabled: Arc::new(Mutex::new(vec![true; num_harmonics])),
            harmonic_phase_enabled: Arc::new(Mutex::new(vec![true; num_harmonics])),
            fade_duration: 128,
            
            // Async buffer computation - initialize all buffers as dirty
            key_buffers: Arc::new(Mutex::new(vec![None; NUM_KEYS])),
            buffer_states: Arc::new(Mutex::new(vec![BufferState::Dirty; NUM_KEYS])),
            computation_cancel: Arc::new(AtomicBool::new(false)),
            
            // Chart view control
            should_reset_chart_view: Arc::new(AtomicBool::new(false)),
        }
    }

    fn populate_piano_periods() -> Vec<u32> {
        Self::compute_piano_periods(44100.0)
    }

    pub fn update_sample_rate(&self, sample_rate: f32) {
        let new_periods = Self::compute_piano_periods(sample_rate as f64);
        let mut piano_periods = self.piano_periods.lock().unwrap();
        *piano_periods = new_periods;
    }

    fn compute_piano_periods(sample_rate: f64) -> Vec<u32> {
        let mut piano_periods = Vec::with_capacity(NUM_KEYS);
        for key in 0..NUM_KEYS {
            // Calculate the frequency for the given key.
            // A0 (key 0) is 27.5 Hz and each key increases by the factor 2^(1/12).
            let frequency = 27.5 * 2f64.powf(key as f64 / 12.0);
            let period = (sample_rate / frequency).round() as u32;
            piano_periods.push(period);
        }
        piano_periods
    }
    
    /// Mark all buffers as dirty and cancel any ongoing computations
    pub fn mark_all_buffers_dirty(&self) {
        self.computation_cancel.store(true, Ordering::Relaxed);
        
        let mut buffer_states = self.buffer_states.lock().unwrap();
        for state in buffer_states.iter_mut() {
            if *state != BufferState::Dirty {
                *state = BufferState::Dirty;
            }
        }
    }
    
    /// Mark a specific buffer as dirty
    pub fn mark_buffer_dirty(&self, key: usize) {
        if key < NUM_KEYS {
            let mut buffer_states = self.buffer_states.lock().unwrap();
            if buffer_states[key] != BufferState::Dirty {
                buffer_states[key] = BufferState::Dirty;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_shared_params_new() {
        let params = SharedParams::new(8, 50);
        
        // Test amplitude data initialization
        let amp_data = params.amplitude_data.lock().unwrap();
        assert_eq!(amp_data.len(), 8);
        assert_eq!(amp_data[0].len(), 50);
        
        // Test phase data initialization
        let phase_data = params.phase_data.lock().unwrap();
        assert_eq!(phase_data.len(), 8);
        assert_eq!(phase_data[0].len(), 50);
        
        // Test voices initialization
        let voices = params.voices.lock().unwrap();
        assert_eq!(voices.len(), NUM_KEYS);
        assert!(voices.iter().all(|v| v.is_none()));
        
        // Test harmonic enabled flags
        let amp_enabled = params.harmonic_ampl_enabled.lock().unwrap();
        let phase_enabled = params.harmonic_phase_enabled.lock().unwrap();
        assert_eq!(amp_enabled.len(), 8);
        assert_eq!(phase_enabled.len(), 8);
        assert!(amp_enabled.iter().all(|&enabled| enabled));
        assert!(phase_enabled.iter().all(|&enabled| enabled));
        
        // Test fade duration
        assert_eq!(params.fade_duration, 128);
    }

    #[test]
    fn test_populate_piano_periods() {
        let periods = SharedParams::populate_piano_periods();
        
        assert_eq!(periods.len(), NUM_KEYS);
        
        // Test first key (A0 = 27.5 Hz)
        // Period should be around 44100 / 27.5 ≈ 1603 samples
        let first_period = periods[0];
        assert!(first_period > 1600 && first_period < 1610);
        
        // Test that periods decrease as we go up in pitch
        // (higher frequency = smaller period)
        assert!(periods[0] > periods[12]); // One octave higher should have half the period
        assert!(periods[12] > periods[24]); // Another octave higher
        
        // Test middle A (A4, key 48) should be around 220Hz
        // Period should be around 44100 / 440 ≈ 100 samples (for A4)
        let middle_a_idx = 48; // A4
        if middle_a_idx < NUM_KEYS {
            let middle_a_period = periods[middle_a_idx];
            assert!(middle_a_period > 90 && middle_a_period < 110);
        }
    }

    #[test]
    fn test_piano_periods_mathematical_relationship() {
        let periods = SharedParams::populate_piano_periods();
        
        // Test that each octave (12 keys) doubles the period (halves frequency)
        for i in 0..NUM_KEYS-12 {
            let ratio = periods[i] as f64 / periods[i + 12] as f64;
            // Should be approximately 2.0 (since frequency doubles each octave)
            assert!((ratio - 2.0).abs() < 0.1, "Period ratio should be ~2.0, got {}", ratio);
        }
    }

    #[test]
    fn test_shared_params_thread_safety() {
        let params = SharedParams::new(4, 10);
        
        // Test that we can lock and modify different mutexes independently
        {
            let mut amp_data = params.amplitude_data.lock().unwrap();
            amp_data[0][0] = 1.0;
        }
        
        {
            let mut phase_data = params.phase_data.lock().unwrap();
            phase_data[0][0] = 2.0;
        }
        
        {
            let mut normalization = params.normalization_needed.lock().unwrap();
            *normalization = true;
        }
        
        // Verify changes were applied
        assert_eq!(params.amplitude_data.lock().unwrap()[0][0], 1.0);
        assert_eq!(params.phase_data.lock().unwrap()[0][0], 2.0);
        assert_eq!(*params.normalization_needed.lock().unwrap(), true);
    }
}
