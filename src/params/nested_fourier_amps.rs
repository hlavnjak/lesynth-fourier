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

use nih_plug::prelude::*;

pub const NUM_NESTED_FOURIER_HARMONICS: usize = 16;

/// 16 sub-harmonic amplitude parameters used by the NestedFourier curve type.
/// The amplitude envelope across buckets is computed as:
///   A(t) = clamp( Σ_{k=1}^{16} sub_k * sin(2π * k * t), 0, 1 )
/// where t = bucket / num_buckets.
#[derive(Params)]
pub struct NestedFourierAmps {
    #[id = "nf_1"]  pub sub_1:  FloatParam,
    #[id = "nf_2"]  pub sub_2:  FloatParam,
    #[id = "nf_3"]  pub sub_3:  FloatParam,
    #[id = "nf_4"]  pub sub_4:  FloatParam,
    #[id = "nf_5"]  pub sub_5:  FloatParam,
    #[id = "nf_6"]  pub sub_6:  FloatParam,
    #[id = "nf_7"]  pub sub_7:  FloatParam,
    #[id = "nf_8"]  pub sub_8:  FloatParam,
    #[id = "nf_9"]  pub sub_9:  FloatParam,
    #[id = "nf_10"] pub sub_10: FloatParam,
    #[id = "nf_11"] pub sub_11: FloatParam,
    #[id = "nf_12"] pub sub_12: FloatParam,
    #[id = "nf_13"] pub sub_13: FloatParam,
    #[id = "nf_14"] pub sub_14: FloatParam,
    #[id = "nf_15"] pub sub_15: FloatParam,
    #[id = "nf_16"] pub sub_16: FloatParam,
}

impl NestedFourierAmps {
    pub fn new(harmonic_idx: usize) -> Self {
        let h = harmonic_idx + 1;
        let range = FloatRange::Linear { min: 0.0, max: 1.0 };
        NestedFourierAmps {
            sub_1:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 1"),  0.0, range),
            sub_2:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 2"),  0.0, range),
            sub_3:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 3"),  0.0, range),
            sub_4:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 4"),  0.0, range),
            sub_5:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 5"),  0.0, range),
            sub_6:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 6"),  0.0, range),
            sub_7:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 7"),  0.0, range),
            sub_8:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 8"),  0.0, range),
            sub_9:  FloatParam::new(&format!("H{h} NF Sub-Harmonic 9"),  0.0, range),
            sub_10: FloatParam::new(&format!("H{h} NF Sub-Harmonic 10"), 0.0, range),
            sub_11: FloatParam::new(&format!("H{h} NF Sub-Harmonic 11"), 0.0, range),
            sub_12: FloatParam::new(&format!("H{h} NF Sub-Harmonic 12"), 0.0, range),
            sub_13: FloatParam::new(&format!("H{h} NF Sub-Harmonic 13"), 0.0, range),
            sub_14: FloatParam::new(&format!("H{h} NF Sub-Harmonic 14"), 0.0, range),
            sub_15: FloatParam::new(&format!("H{h} NF Sub-Harmonic 15"), 0.0, range),
            sub_16: FloatParam::new(&format!("H{h} NF Sub-Harmonic 16"), 0.0, range),
        }
    }

    /// Get a reference to sub-harmonic parameter by index (0-based).
    pub fn get(&self, i: usize) -> &FloatParam {
        match i {
            0  => &self.sub_1,
            1  => &self.sub_2,
            2  => &self.sub_3,
            3  => &self.sub_4,
            4  => &self.sub_5,
            5  => &self.sub_6,
            6  => &self.sub_7,
            7  => &self.sub_8,
            8  => &self.sub_9,
            9  => &self.sub_10,
            10 => &self.sub_11,
            11 => &self.sub_12,
            12 => &self.sub_13,
            13 => &self.sub_14,
            14 => &self.sub_15,
            15 => &self.sub_16,
            _  => panic!("NestedFourierAmps index out of bounds: {}", i),
        }
    }

    /// Return all 16 current values as an array.
    pub fn values(&self) -> [f32; NUM_NESTED_FOURIER_HARMONICS] {
        [
            self.sub_1.value(),
            self.sub_2.value(),
            self.sub_3.value(),
            self.sub_4.value(),
            self.sub_5.value(),
            self.sub_6.value(),
            self.sub_7.value(),
            self.sub_8.value(),
            self.sub_9.value(),
            self.sub_10.value(),
            self.sub_11.value(),
            self.sub_12.value(),
            self.sub_13.value(),
            self.sub_14.value(),
            self.sub_15.value(),
            self.sub_16.value(),
        ]
    }
}
