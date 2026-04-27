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

use std::f32::consts::PI;

// Audio Constants
pub const NUM_HARMONICS: usize = 64;
pub const NUM_KEYS: usize = 88;

// Parameter Defaults and Ranges
pub static NUM_OF_BUCKETS_DEFAULT: usize = 70;
pub static NUM_OF_BUCKETS_MIN: i32 = 30;
pub static NUM_OF_BUCKETS_MAX: i32 = 2000;

// Amplitude Parameter Ranges
pub static MIN_OFFSET_AMP: f64 = 0.0;
pub static MAX_OFFSET_AMP: f64 = 1.0;

// Phase Parameter Ranges
pub static MIN_OFFSET_PHASE: f64 = 0.0;
pub static MAX_OFFSET_PHASE: f64 = 6.28;

// GUI Constants
pub static LABEL_FONT_SIZE: f32 = 12.0;

// Audio Processing Constants
pub const TWO_PI: f32 = 2.0 * PI;
pub const SAMPLE_RATE: f64 = 44100.0;
pub const NYQUIST_FREQUENCY: f64 = SAMPLE_RATE / 2.0;

/// Calculate the maximum usable harmonic number for a given piano key
/// to prevent aliasing (harmonic frequency must be below Nyquist frequency)
pub fn max_harmonic_for_key(key: usize) -> usize {
    if key >= NUM_KEYS {
        return 0;
    }

    // Calculate the fundamental frequency for the given key
    // A0 (key 0) is 27.5 Hz and each key increases by the factor 2^(1/12)
    let fundamental_freq = 27.5 * 2f64.powf(key as f64 / 12.0);

    // Calculate maximum harmonic number that stays below Nyquist frequency
    let max_harmonic = (NYQUIST_FREQUENCY / fundamental_freq).floor() as usize;

    // Clamp to available harmonics
    max_harmonic.min(NUM_HARMONICS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_constants() {
        assert_eq!(NUM_HARMONICS, 64);
        assert_eq!(NUM_KEYS, 88);
    }

    #[test]
    fn test_parameter_defaults_and_ranges() {
        assert_eq!(NUM_OF_BUCKETS_DEFAULT, 70);
        assert_eq!(NUM_OF_BUCKETS_MIN, 30);
        assert_eq!(NUM_OF_BUCKETS_MAX, 2000);
        assert!(NUM_OF_BUCKETS_MIN < NUM_OF_BUCKETS_DEFAULT as i32);
        assert!(NUM_OF_BUCKETS_DEFAULT < NUM_OF_BUCKETS_MAX as usize);
    }

    #[test]
    fn test_amplitude_ranges() {
        assert_eq!(MIN_OFFSET_AMP, 0.0);
        assert_eq!(MAX_OFFSET_AMP, 1.0);
        assert!(MIN_OFFSET_AMP < MAX_OFFSET_AMP);
    }

    #[test]
    fn test_phase_ranges() {
        assert_eq!(MIN_OFFSET_PHASE, 0.0);
        assert_eq!(MAX_OFFSET_PHASE, 6.28);
        assert!(MIN_OFFSET_PHASE < MAX_OFFSET_PHASE);

        // Should be approximately 2*PI
        assert!((MAX_OFFSET_PHASE - 2.0 * PI as f64).abs() < 0.01);
    }

    #[test]
    fn test_two_pi_constant() {
        assert_eq!(TWO_PI, 2.0 * PI);
        assert!((TWO_PI - 6.28318530717959).abs() < 1e-6);
    }

    #[test]
    fn test_gui_constants() {
        assert_eq!(LABEL_FONT_SIZE, 12.0);
        assert!(LABEL_FONT_SIZE > 0.0);
    }

    #[test]
    fn test_max_harmonic_for_key() {
        // Test lower keys - should allow many harmonics
        let low_key_max = max_harmonic_for_key(0); // A0 = 27.5 Hz
        assert!(low_key_max > 50, "Low keys should allow many harmonics, got {}", low_key_max);

        // Test high keys - should limit harmonics
        let high_key_max = max_harmonic_for_key(87); // C8 = ~4186 Hz
        assert!(high_key_max < 10, "High keys should limit harmonics to prevent aliasing, got {}", high_key_max);

        // Test that higher keys have fewer allowed harmonics
        let mid_key_max = max_harmonic_for_key(48); // C4 = ~261 Hz
        assert!(mid_key_max < low_key_max, "Higher keys should have fewer allowed harmonics");
        assert!(high_key_max < mid_key_max, "Highest keys should have the fewest allowed harmonics");

        // Test boundary condition
        assert_eq!(max_harmonic_for_key(NUM_KEYS), 0, "Invalid key should return 0");
    }

    #[test]
    fn test_sample_rate_constants() {
        assert_eq!(SAMPLE_RATE, 44100.0);
        assert_eq!(NYQUIST_FREQUENCY, 22050.0);
    }
}
