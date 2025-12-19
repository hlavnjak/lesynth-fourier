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

#[derive(Debug, Clone, Copy, PartialEq, Enum)]
pub enum CurveType {
    Constant,
    Sine,
}

impl CurveType {
    // so we can write `for variant in CurveType::VARIANTS`
    pub const VARIANTS: [CurveType; 2] = [
        CurveType::Constant,
        CurveType::Sine,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Enum)]
pub enum GranularityLevel {
    #[name = "0.025"]
    UltraLow,
    #[name = "0.05"]
    VeryLow,
    #[name = "0.1"]
    Low,
    #[name = "0.5"] 
    Medium,
    #[name = "1.0"]
    High,
}

impl GranularityLevel {
    pub const VARIANTS: [GranularityLevel; 5] = [
        GranularityLevel::UltraLow,
        GranularityLevel::VeryLow,
        GranularityLevel::Low,
        GranularityLevel::Medium,
        GranularityLevel::High,
    ];

    pub fn as_f64(&self) -> f64 {
        match self {
            GranularityLevel::UltraLow => 0.025,
            GranularityLevel::VeryLow => 0.05,
            GranularityLevel::Low => 0.1,
            GranularityLevel::Medium => 0.5,
            GranularityLevel::High => 1.0,
        }
    }

    pub fn as_f32(&self) -> f32 {
        self.as_f64() as f32
    }
}

impl Default for GranularityLevel {
    fn default() -> Self {
        GranularityLevel::High
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_curve_type_variants() {
        assert_eq!(CurveType::VARIANTS.len(), 2);
        assert_eq!(CurveType::VARIANTS[0], CurveType::Constant);
        assert_eq!(CurveType::VARIANTS[1], CurveType::Sine);
    }

    #[test]
    fn test_curve_type_debug() {
        assert_eq!(format!("{:?}", CurveType::Constant), "Constant");
        assert_eq!(format!("{:?}", CurveType::Sine), "Sine");
    }

    #[test]
    fn test_curve_type_clone() {
        let original = CurveType::Sine;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    #[test]
    fn test_curve_type_equality() {
        assert_eq!(CurveType::Constant, CurveType::Constant);
        assert_ne!(CurveType::Constant, CurveType::Sine);
        assert_eq!(CurveType::Sine, CurveType::Sine);
    }

    #[test]
    fn test_granularity_level_variants() {
        assert_eq!(GranularityLevel::VARIANTS.len(), 5);
        assert_eq!(GranularityLevel::VARIANTS[0], GranularityLevel::UltraLow);
        assert_eq!(GranularityLevel::VARIANTS[1], GranularityLevel::VeryLow);
        assert_eq!(GranularityLevel::VARIANTS[2], GranularityLevel::Low);
        assert_eq!(GranularityLevel::VARIANTS[3], GranularityLevel::Medium);
        assert_eq!(GranularityLevel::VARIANTS[4], GranularityLevel::High);
    }

    #[test]
    fn test_granularity_level_values() {
        assert_eq!(GranularityLevel::UltraLow.as_f64(), 0.025);
        assert_eq!(GranularityLevel::VeryLow.as_f64(), 0.05);
        assert_eq!(GranularityLevel::Low.as_f64(), 0.1);
        assert_eq!(GranularityLevel::Medium.as_f64(), 0.5);
        assert_eq!(GranularityLevel::High.as_f64(), 1.0);
        
        assert_eq!(GranularityLevel::UltraLow.as_f32(), 0.025);
        assert_eq!(GranularityLevel::VeryLow.as_f32(), 0.05);
        assert_eq!(GranularityLevel::Low.as_f32(), 0.1);
        assert_eq!(GranularityLevel::Medium.as_f32(), 0.5);
        assert_eq!(GranularityLevel::High.as_f32(), 1.0);
    }

    #[test]
    fn test_granularity_level_default() {
        assert_eq!(GranularityLevel::default(), GranularityLevel::High);
    }
}