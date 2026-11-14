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

//! Modulation of a sounding voice, applied on top of the harmonic grid.
//!
//! The grid says what a note *is*: each harmonic's amplitude and phase across
//! the buckets. It cannot say when a key comes back up, so anything that
//! follows the key rather than the grid — an attack from the press, a release
//! from the note-off — lives here and is applied per voice as it plays. The
//! grid, its buffers and the files that carry them are untouched by it.
//!
//! For now that is one amplitude envelope (attack, hold, decay, sustain,
//! release). It is off by default, and off means off: the audio path is then
//! exactly what it was before this module existed.

use crate::params::LeSynthParams;

/// Shortest release, in seconds. A release of nothing is a click.
pub const MIN_RELEASE_SECS: f32 = 0.001;

/// An amplitude envelope, times in seconds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Envelope {
    pub attack: f32,
    pub hold: f32,
    pub decay: f32,
    /// Level held after the decay, 0..1.
    pub sustain: f32,
    pub release: f32,
    /// Bend of each moving stage, −1..1: 0 is a straight line, positive the
    /// exponential-like shape (an attack that starts slowly, a decay or
    /// release that drops fast and then tails off), negative its mirror.
    pub attack_curve: f32,
    pub decay_curve: f32,
    pub release_curve: f32,
}

impl Envelope {
    /// The envelope the params describe, or `None` while it is switched off.
    pub fn from_params(params: &LeSynthParams) -> Option<Self> {
        params.env_enabled.value().then(|| Self::from_sliders(params))
    }

    /// The envelope the sliders describe, whether or not it is switched on.
    pub fn from_sliders(params: &LeSynthParams) -> Self {
        Self {
            attack: params.env_attack.value() * 1e-3,
            hold: params.env_hold.value() * 1e-3,
            decay: params.env_decay.value() * 1e-3,
            sustain: params.env_sustain.value(),
            release: (params.env_release.value() * 1e-3).max(MIN_RELEASE_SECS),
            attack_curve: params.env_attack_curve.value(),
            decay_curve: params.env_decay_curve.value(),
            release_curve: params.env_release_curve.value(),
        }
    }

    /// Level `t` seconds after the key went down, for as long as it stays down.
    pub fn held_level(&self, t: f32) -> f32 {
        if t < self.attack {
            return bend(t / self.attack, self.attack_curve);
        }
        let t = t - self.attack;
        if t < self.hold {
            return 1.0;
        }
        let t = t - self.hold;
        if t < self.decay {
            let left = bend(1.0 - t / self.decay, self.decay_curve);
            return self.sustain + (1.0 - self.sustain) * left;
        }
        self.sustain
    }

    /// What is left, 1..0, of the level the key was released at, `t` seconds
    /// after the release. The release always starts from wherever the envelope
    /// was, so a key let go mid-attack does not jump to the sustain level.
    pub fn release_gain(&self, t: f32) -> f32 {
        if t >= self.release {
            return 0.0;
        }
        bend(1.0 - t / self.release, self.release_curve)
    }

    /// Level over a whole note: held for `gate` seconds, then released.
    pub fn level_at(&self, t: f32, gate: f32) -> f32 {
        if t < gate {
            self.held_level(t)
        } else {
            self.held_level(gate) * self.release_gain(t - gate)
        }
    }

    /// When the charts that preview a whole sound of `total` seconds let the
    /// key go: early enough that the release finishes as the sound does, so
    /// the release and its curve are on screen with the other stages. A
    /// release longer than half the sound still leaves the key held for the
    /// first half, and is then cut off by the end of the chart.
    pub fn preview_gate(&self, total: f32) -> f32 {
        (total - self.release).max(total * 0.5)
    }

    /// The level `t` seconds into that preview — see [`Self::preview_gate`].
    pub fn preview_level(&self, t: f32, total: f32) -> f32 {
        self.level_at(t, self.preview_gate(total))
    }

    /// Seconds from the press to the sustain level.
    pub fn time_to_sustain(&self) -> f32 {
        self.attack + self.hold + self.decay
    }
}

/// A 0..1 ramp bent by `curve` (−1..1) and still running 0..1. Rational
/// rather than a power: no `powf` per sample beyond the one that turns the
/// curve into a slope, and exactly the identity at `curve == 0`.
fn bend(x: f32, curve: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if curve == 0.0 {
        return x;
    }
    let s = (4.0 * curve).exp2();
    x / (x + s * (1.0 - x))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The preview the Live Harmonics and Assembled Sound charts draw must
    /// show the release: it ends at silence with the sound, and both the
    /// release time and its curve move it.
    #[test]
    fn the_whole_sound_preview_shows_the_release() {
        let e = env();
        let total = 2.0;
        assert!(e.preview_level(total, total) < 1e-6, "silent as the sound ends");
        let mid_release = total - e.release * 0.5;
        let level = e.preview_level(mid_release, total);
        assert!(level > 0.0 && level < e.sustain, "part-way down mid-release");

        let longer = Envelope { release: e.release * 2.0, ..e };
        assert_ne!(longer.preview_level(mid_release, total), level, "release time shows");
        let bent = Envelope { release_curve: 0.8, ..e };
        assert_ne!(bent.preview_level(mid_release, total), level, "release curve shows");

        // A release longer than the sound still leaves the key held first.
        let endless = Envelope { release: 10.0, ..e };
        assert_eq!(endless.preview_gate(total), 1.0);
    }

    fn env() -> Envelope {
        Envelope {
            attack: 0.1,
            hold: 0.05,
            decay: 0.2,
            sustain: 0.4,
            release: 0.3,
            attack_curve: 0.0,
            decay_curve: 0.0,
            release_curve: 0.0,
        }
    }

    #[test]
    fn the_stages_meet_without_a_step() {
        let e = env();
        assert_eq!(e.held_level(0.0), 0.0);
        assert!((e.held_level(0.05) - 0.5).abs() < 1e-6);
        assert!((e.held_level(0.1) - 1.0).abs() < 1e-6);
        assert_eq!(e.held_level(0.12), 1.0);
        assert!((e.held_level(0.25) - 0.7).abs() < 1e-5);
        assert!((e.held_level(0.35) - 0.4).abs() < 1e-5);
        assert_eq!(e.held_level(10.0), 0.4);
        // No step anywhere along the way, bent or straight.
        for curve in [-1.0, -0.3, 0.0, 0.6, 1.0] {
            let e = Envelope { attack_curve: curve, decay_curve: curve, release_curve: curve, ..env() };
            let dt = 1e-4;
            let mut prev = e.level_at(0.0, 0.5);
            for i in 1..10_000 {
                let v = e.level_at(i as f32 * dt, 0.5);
                assert!((v - prev).abs() < 0.02, "curve {curve}: step at {i}");
                assert!((0.0..=1.0).contains(&v));
                prev = v;
            }
            assert_eq!(e.level_at(0.8, 0.5), 0.0);
        }
    }

    #[test]
    fn a_release_starts_from_where_the_envelope_was() {
        let e = env();
        // Let go half way up the attack: the release leaves from 0.5, not
        // from the sustain level or from full scale.
        assert!((e.level_at(0.05, 0.05) - 0.5).abs() < 1e-6);
        assert!((e.level_at(0.05 + 0.15, 0.05) - 0.25).abs() < 1e-5);
    }

    #[test]
    fn zero_length_stages_are_skipped_not_divided_by() {
        let e = Envelope { attack: 0.0, hold: 0.0, decay: 0.0, ..env() };
        assert_eq!(e.held_level(0.0), 0.4);
        let e = Envelope { attack: 0.0, ..env() };
        assert_eq!(e.held_level(0.0), 1.0);
    }

    #[test]
    fn a_positive_curve_is_the_exponential_shape() {
        // Attack starts slowly; decay and release drop fast.
        let e = Envelope { attack_curve: 0.5, decay_curve: 0.5, release_curve: 0.5, ..env() };
        assert!(e.held_level(0.05) < 0.5);
        assert!(e.held_level(0.25) < 0.7);
        assert!(e.release_gain(0.15) < 0.5);
        assert_eq!(bend(0.0, 0.7), 0.0);
        assert_eq!(bend(1.0, 0.7), 1.0);
    }

    #[test]
    fn it_is_off_by_default() {
        assert_eq!(Envelope::from_params(&LeSynthParams::default()), None);
    }
}
