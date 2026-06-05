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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChartType {
    Amp,
    Phase,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chart_type_debug() {
        assert_eq!(format!("{:?}", ChartType::Amp), "Amp");
        assert_eq!(format!("{:?}", ChartType::Phase), "Phase");
    }

    #[test]
    fn test_chart_type_clone() {
        let original = ChartType::Amp;
        let cloned = original.clone();
        assert_eq!(original, cloned);
    }

    #[test]
    fn test_chart_type_equality() {
        assert_eq!(ChartType::Amp, ChartType::Amp);
        assert_eq!(ChartType::Phase, ChartType::Phase);
        assert_ne!(ChartType::Amp, ChartType::Phase);
    }
}