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

//! Drawing one harmonic's amplitude or phase curve by hand, on its chart.
//!
//! The pen starts at the chart's origin and only ever travels right. While the
//! left button is held it draws: the curve follows the pointer from where the
//! pen stands to where the pointer is. While the button is up the pen keeps its
//! place along the chart and only rides up and down with the pointer, drawing
//! nothing — so pressing again carries on from the same bucket at a new height.
//! That is what makes a curve with a jump in it as easy to draw as one without.

use nih_plug_egui::egui;
use crate::engine::ChartType;

/// One drawing in progress: which row, and where the pen stands on its chart,
/// in the chart's own coordinates (`x` in buckets, `y` in amplitude or radians).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrawSession {
    pub chart: ChartType,
    pub harmonic: usize,
    pub x: f32,
    pub y: f32,
    /// The button was down at the last step.
    pub down: bool,
}

impl DrawSession {
    pub fn new(chart: ChartType, harmonic: usize) -> Self {
        Self { chart, harmonic, x: 0.0, y: 0.0, down: false }
    }

    /// The pen has reached the last bucket: there is nothing left to draw.
    pub fn finished(&self, last_bucket: usize) -> bool {
        self.x >= last_bucket as f32
    }

    /// Move the pen to where the pointer is and answer the `(bucket, value)`
    /// pairs that drew.
    ///
    /// `pointer` is in chart coordinates; `y_range` is what the chart's y axis
    /// shows right now, so on a zoomed amplitude chart the top of the chart is
    /// the zoomed maximum and nothing above it can be drawn.
    ///
    /// Button up: the pen takes the pointer's height and nothing is written.
    /// Button down: the pen draws a straight line from where it stood to the
    /// pointer — but never backwards. A pointer left of the pen moves it up or
    /// down where it stands.
    pub fn step(
        &mut self,
        pointer: (f32, f32),
        pressed: bool,
        last_bucket: usize,
        y_range: (f32, f32),
    ) -> Vec<(usize, f32)> {
        let y = pointer.1.clamp(y_range.0, y_range.1);
        if !pressed {
            self.y = y;
            self.down = false;
            return Vec::new();
        }
        // A press starts at the pointer's height: the pen was following it.
        let (from_x, from_y) = (self.x, if self.down { self.y } else { y });
        let to_x = pointer.0.clamp(0.0, last_bucket as f32).max(from_x);
        let first = from_x.ceil() as usize;
        let last = (to_x.floor() as usize).min(last_bucket);
        let mut drawn = Vec::new();
        for bucket in first..=last {
            let t = if to_x > from_x { (bucket as f32 - from_x) / (to_x - from_x) } else { 1.0 };
            drawn.push((bucket, from_y + (y - from_y) * t));
        }
        self.x = to_x;
        self.y = y;
        self.down = true;
        drawn
    }
}

fn session_id() -> egui::Id {
    egui::Id::new("harmonic_draw_session")
}

/// The drawing in progress, if there is one.
pub fn draw_session(ctx: &egui::Context) -> Option<DrawSession> {
    ctx.memory(|m| m.data.get_temp::<Option<DrawSession>>(session_id())).flatten()
}

/// Start, update or (with `None`) end the drawing in progress.
pub fn set_draw_session(ctx: &egui::Context, session: Option<DrawSession>) {
    ctx.memory_mut(|m| m.data.insert_temp(session_id(), session));
}

#[cfg(test)]
mod tests {
    use super::*;

    const RANGE: (f32, f32) = (0.0, 1.0);

    #[test]
    fn the_pen_starts_at_the_origin_and_draws_only_while_pressed() {
        let mut pen = DrawSession::new(ChartType::Amp, 3);
        assert_eq!((pen.x, pen.y), (0.0, 0.0));
        // Button up: the pen rides up and down and stays at bucket 0.
        assert!(pen.step((7.0, 0.5), false, 10, RANGE).is_empty());
        assert_eq!((pen.x, pen.y), (0.0, 0.5));
        // Pressed there: bucket 0 takes the pointer's height.
        assert_eq!(pen.step((0.0, 0.5), true, 10, RANGE), vec![(0, 0.5)]);
        // Dragged right: a straight line to the pointer.
        let drawn = pen.step((4.0, 0.9), true, 10, RANGE);
        assert_eq!(drawn.iter().map(|d| d.0).collect::<Vec<_>>(), vec![0, 1, 2, 3, 4]);
        assert!((drawn[2].1 - 0.7).abs() < 1e-6);
        assert!((drawn[4].1 - 0.9).abs() < 1e-6);
        assert_eq!(pen.x, 4.0);
    }

    #[test]
    fn the_pen_never_travels_left() {
        let mut pen = DrawSession::new(ChartType::Amp, 0);
        pen.step((0.0, 0.2), true, 10, RANGE);
        pen.step((5.0, 0.2), true, 10, RANGE);
        // Pulled back to bucket 2 with the button held: it stays at 5 and only
        // the height where it stands changes.
        assert_eq!(pen.step((2.0, 0.6), true, 10, RANGE), vec![(5, 0.6)]);
        assert_eq!(pen.x, 5.0);
    }

    #[test]
    fn releasing_the_button_makes_a_jump() {
        let mut pen = DrawSession::new(ChartType::Amp, 0);
        pen.step((0.0, 0.8), true, 10, RANGE);
        pen.step((3.5, 0.8), true, 10, RANGE);
        // Up, down to 0.1, and on: bucket 3 keeps 0.8 and bucket 4 starts low.
        assert!(pen.step((9.0, 0.1), false, 10, RANGE).is_empty());
        assert_eq!(pen.x, 3.5);
        let drawn = pen.step((6.0, 0.1), true, 10, RANGE);
        assert_eq!(drawn, vec![(4, 0.1), (5, 0.1), (6, 0.1)]);
    }

    #[test]
    fn the_zoomed_axis_is_the_top_of_what_can_be_drawn() {
        let mut pen = DrawSession::new(ChartType::Amp, 0);
        let zoomed = (0.0, 0.25);
        assert_eq!(pen.step((0.0, 0.9), true, 10, zoomed), vec![(0, 0.25)]);
        // (Bucket 0, where the stroke starts from, is written again unchanged.)
        assert_eq!(pen.step((1.0, -3.0), true, 10, zoomed), vec![(0, 0.25), (1, 0.0)]);
    }

    #[test]
    fn the_drawing_ends_at_the_last_bucket() {
        let mut pen = DrawSession::new(ChartType::Phase, 0);
        pen.step((0.0, 1.0), true, 4, (0.0, 6.28));
        assert!(!pen.finished(4));
        let drawn = pen.step((99.0, 1.0), true, 4, (0.0, 6.28));
        assert_eq!(drawn.last(), Some(&(4, 1.0)));
        assert!(pen.finished(4));
    }
}
