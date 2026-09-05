// Copyright 2026 Jakub Hlavnicka
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
    #[name = "Nested Fourier"]
    NestedFourier,
}

impl CurveType {
    // so we can write `for variant in CurveType::VARIANTS`
    pub const VARIANTS: [CurveType; 2] = [
        CurveType::Constant,
        CurveType::NestedFourier,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Enum)]
pub enum GranularityLevel {
    #[name = "0.001"]
    Micro,
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
    pub const VARIANTS: [GranularityLevel; 6] = [
        GranularityLevel::Micro,
        GranularityLevel::UltraLow,
        GranularityLevel::VeryLow,
        GranularityLevel::Low,
        GranularityLevel::Medium,
        GranularityLevel::High,
    ];

    pub fn as_f64(&self) -> f64 {
        match self {
            GranularityLevel::Micro => 0.001,
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

    /// Position in [`Self::VARIANTS`] — how a level is stored in the persisted
    /// nested-Fourier state, which needs a plain byte rather than an enum whose
    /// name could be renamed out from under an old file.
    pub fn to_index(self) -> u8 {
        Self::VARIANTS
            .iter()
            .position(|&v| v == self)
            .unwrap_or(0) as u8
    }

    /// The level at `index`, or `None` when a file names one this build has no
    /// variant for.
    pub fn from_index(index: u8) -> Option<Self> {
        Self::VARIANTS.get(index as usize).copied()
    }
}

/// Selectable fundamentals (Hz) for a `NestedFourier` curve, `0.0` first for
/// **auto** — one cycle of the fundamental across the whole grid, which is the
/// only shape the series could make before this was selectable.
///
/// Hz here is measured against the grid's own duration
/// (`SynthComputeEngine::grid_duration_secs`): a 4 Hz fundamental on a grid
/// spanning 0.75 s turns three times across the chart. The list runs from well
/// under one turn per grid up to a couple of hundred, which is where a chart of
/// a few dozen buckets stops resolving the wave at all.
pub const NESTED_BASE_FREQ_CHOICES: [f32; 16] = [
    0.0, 0.25, 0.5, 1.0, 2.0, 3.0, 5.0, 8.0, 12.0, 20.0, 30.0, 50.0, 80.0, 120.0, 160.0, 200.0,
];

/// How a nested-Fourier base frequency reads in the combo box.
pub fn nested_base_freq_label(hz: f32) -> String {
    if hz <= 0.0 {
        "Base: auto".to_string()
    } else if hz < 1.0 {
        format!("Base: {hz:.2} Hz")
    } else {
        format!("Base: {hz:.0} Hz")
    }
}

impl Default for CurveType {
    fn default() -> Self {
        CurveType::NestedFourier
    }
}

impl Default for GranularityLevel {
    fn default() -> Self {
        GranularityLevel::Low
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_curve_type_variants() {
        assert_eq!(CurveType::VARIANTS.len(), 2);
        assert_eq!(CurveType::VARIANTS[0], CurveType::Constant);
        assert_eq!(CurveType::VARIANTS[1], CurveType::NestedFourier);
    }

    #[test]
    fn test_curve_type_debug() {
        assert_eq!(format!("{:?}", CurveType::Constant), "Constant");
        assert_eq!(format!("{:?}", CurveType::NestedFourier), "NestedFourier");
    }

    #[test]
    fn test_curve_type_clone() {
        let original = CurveType::NestedFourier;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    #[test]
    fn test_curve_type_equality() {
        assert_eq!(CurveType::Constant, CurveType::Constant);
        assert_ne!(CurveType::Constant, CurveType::NestedFourier);
        assert_eq!(CurveType::NestedFourier, CurveType::NestedFourier);
    }

    #[test]
    fn test_granularity_level_variants() {
        assert_eq!(GranularityLevel::VARIANTS.len(), 6);
        assert_eq!(GranularityLevel::VARIANTS[0], GranularityLevel::Micro);
        assert_eq!(GranularityLevel::VARIANTS[1], GranularityLevel::UltraLow);
        assert_eq!(GranularityLevel::VARIANTS[2], GranularityLevel::VeryLow);
        assert_eq!(GranularityLevel::VARIANTS[3], GranularityLevel::Low);
        assert_eq!(GranularityLevel::VARIANTS[4], GranularityLevel::Medium);
        assert_eq!(GranularityLevel::VARIANTS[5], GranularityLevel::High);
    }

    #[test]
    fn test_granularity_level_values() {
        assert_eq!(GranularityLevel::Micro.as_f64(), 0.001);
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
        assert_eq!(GranularityLevel::default(), GranularityLevel::Low);
    }
}
