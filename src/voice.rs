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

#[derive(Clone)]
pub struct Voice {
    pub buffer: Vec<f32>,
    pub idx: usize,
    pub fade_in_active: bool,
    pub fade_in_pos: usize,
    pub fade_out_active: bool,
    pub fade_out_pos: usize,
}

impl Voice {
    pub fn new(buffer: Vec<f32>) -> Self {
        Self {
            buffer,
            idx: 0,
            fade_in_active: true,
            fade_in_pos: 0,
            fade_out_active: false,
            fade_out_pos: 0,
        }
    }

    pub fn is_fading(&self) -> bool {
        self.fade_in_active || self.fade_out_active
    }

    pub fn start_fade_out(&mut self) {
        self.fade_out_active = true;
        self.fade_out_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_voice_new() {
        let buffer = vec![0.1, 0.2, 0.3, 0.4];
        let voice = Voice::new(buffer.clone());
        
        assert_eq!(voice.buffer, buffer);
        assert_eq!(voice.idx, 0);
        assert_eq!(voice.fade_in_active, true);
        assert_eq!(voice.fade_in_pos, 0);
        assert_eq!(voice.fade_out_active, false);
        assert_eq!(voice.fade_out_pos, 0);
    }

    #[test]
    fn test_voice_is_fading() {
        let mut voice = Voice::new(vec![0.0; 10]);
        
        // Initially fading in
        assert!(voice.is_fading());
        
        // Stop fade in
        voice.fade_in_active = false;
        assert!(!voice.is_fading());
        
        // Start fade out
        voice.start_fade_out();
        assert!(voice.is_fading());
        assert!(voice.fade_out_active);
        assert_eq!(voice.fade_out_pos, 0);
    }

    #[test]
    fn test_voice_start_fade_out() {
        let mut voice = Voice::new(vec![0.0; 5]);
        
        assert!(!voice.fade_out_active);
        
        voice.start_fade_out();
        
        assert!(voice.fade_out_active);
        assert_eq!(voice.fade_out_pos, 0);
    }

    #[test]
    fn test_voice_clone() {
        let original = Voice::new(vec![1.0, 2.0, 3.0]);
        let cloned = original.clone();
        
        assert_eq!(original.buffer, cloned.buffer);
        assert_eq!(original.idx, cloned.idx);
        assert_eq!(original.fade_in_active, cloned.fade_in_active);
        assert_eq!(original.fade_out_active, cloned.fade_out_active);
    }
}