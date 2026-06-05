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
use crate::params::NUM_NESTED_FOURIER_HARMONICS;

/// Sub-harmonic phase parameters used by the NestedFourier curve type.
/// Phase is in radians: range [-pi, pi].
#[derive(Params)]
pub struct NestedFourierPhases {
    #[id = "nf_ph_1"] pub sub_1: FloatParam,
    #[id = "nf_ph_2"] pub sub_2: FloatParam,
    #[id = "nf_ph_3"] pub sub_3: FloatParam,
    #[id = "nf_ph_4"] pub sub_4: FloatParam,
    #[id = "nf_ph_5"] pub sub_5: FloatParam,
    #[id = "nf_ph_6"] pub sub_6: FloatParam,
    #[id = "nf_ph_7"] pub sub_7: FloatParam,
    #[id = "nf_ph_8"] pub sub_8: FloatParam,
    #[id = "nf_ph_9"] pub sub_9: FloatParam,
    #[id = "nf_ph_10"] pub sub_10: FloatParam,
    #[id = "nf_ph_11"] pub sub_11: FloatParam,
    #[id = "nf_ph_12"] pub sub_12: FloatParam,
    #[id = "nf_ph_13"] pub sub_13: FloatParam,
    #[id = "nf_ph_14"] pub sub_14: FloatParam,
    #[id = "nf_ph_15"] pub sub_15: FloatParam,
    #[id = "nf_ph_16"] pub sub_16: FloatParam,
    #[id = "nf_ph_17"] pub sub_17: FloatParam,
    #[id = "nf_ph_18"] pub sub_18: FloatParam,
    #[id = "nf_ph_19"] pub sub_19: FloatParam,
    #[id = "nf_ph_20"] pub sub_20: FloatParam,
    #[id = "nf_ph_21"] pub sub_21: FloatParam,
    #[id = "nf_ph_22"] pub sub_22: FloatParam,
    #[id = "nf_ph_23"] pub sub_23: FloatParam,
    #[id = "nf_ph_24"] pub sub_24: FloatParam,
    #[id = "nf_ph_25"] pub sub_25: FloatParam,
    #[id = "nf_ph_26"] pub sub_26: FloatParam,
    #[id = "nf_ph_27"] pub sub_27: FloatParam,
    #[id = "nf_ph_28"] pub sub_28: FloatParam,
    #[id = "nf_ph_29"] pub sub_29: FloatParam,
    #[id = "nf_ph_30"] pub sub_30: FloatParam,
    #[id = "nf_ph_31"] pub sub_31: FloatParam,
    #[id = "nf_ph_32"] pub sub_32: FloatParam,
}

impl NestedFourierPhases {
    pub fn new(harmonic_idx: usize) -> Self {
        let h = harmonic_idx + 1;
        let range = FloatRange::Linear { min: -std::f32::consts::PI, max: std::f32::consts::PI };
        NestedFourierPhases {
            sub_1: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 1"), 0.0, range),
            sub_2: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 2"), 0.0, range),
            sub_3: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 3"), 0.0, range),
            sub_4: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 4"), 0.0, range),
            sub_5: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 5"), 0.0, range),
            sub_6: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 6"), 0.0, range),
            sub_7: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 7"), 0.0, range),
            sub_8: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 8"), 0.0, range),
            sub_9: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 9"), 0.0, range),
            sub_10: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 10"), 0.0, range),
            sub_11: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 11"), 0.0, range),
            sub_12: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 12"), 0.0, range),
            sub_13: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 13"), 0.0, range),
            sub_14: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 14"), 0.0, range),
            sub_15: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 15"), 0.0, range),
            sub_16: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 16"), 0.0, range),
            sub_17: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 17"), 0.0, range),
            sub_18: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 18"), 0.0, range),
            sub_19: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 19"), 0.0, range),
            sub_20: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 20"), 0.0, range),
            sub_21: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 21"), 0.0, range),
            sub_22: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 22"), 0.0, range),
            sub_23: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 23"), 0.0, range),
            sub_24: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 24"), 0.0, range),
            sub_25: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 25"), 0.0, range),
            sub_26: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 26"), 0.0, range),
            sub_27: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 27"), 0.0, range),
            sub_28: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 28"), 0.0, range),
            sub_29: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 29"), 0.0, range),
            sub_30: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 30"), 0.0, range),
            sub_31: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 31"), 0.0, range),
            sub_32: FloatParam::new(&format!("H{h} NF Sub-Harmonic Phase 32"), 0.0, range),
        }
    }

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
            16 => &self.sub_17,
            17 => &self.sub_18,
            18 => &self.sub_19,
            19 => &self.sub_20,
            20 => &self.sub_21,
            21 => &self.sub_22,
            22 => &self.sub_23,
            23 => &self.sub_24,
            24 => &self.sub_25,
            25 => &self.sub_26,
            26 => &self.sub_27,
            27 => &self.sub_28,
            28 => &self.sub_29,
            29 => &self.sub_30,
            30 => &self.sub_31,
            31 => &self.sub_32,
            _  => panic!("NestedFourierPhases index out of bounds: {}", i),
        }
    }

    pub fn values(&self) -> [f32; NUM_NESTED_FOURIER_HARMONICS] {
        [
            self.sub_1.value(), self.sub_2.value(), self.sub_3.value(), self.sub_4.value(),
            self.sub_5.value(), self.sub_6.value(), self.sub_7.value(), self.sub_8.value(),
            self.sub_9.value(), self.sub_10.value(), self.sub_11.value(), self.sub_12.value(),
            self.sub_13.value(), self.sub_14.value(), self.sub_15.value(), self.sub_16.value(),
            self.sub_17.value(), self.sub_18.value(), self.sub_19.value(), self.sub_20.value(),
            self.sub_21.value(), self.sub_22.value(), self.sub_23.value(), self.sub_24.value(),
            self.sub_25.value(), self.sub_26.value(), self.sub_27.value(), self.sub_28.value(),
            self.sub_29.value(), self.sub_30.value(), self.sub_31.value(), self.sub_32.value(),
        ]
    }
}
