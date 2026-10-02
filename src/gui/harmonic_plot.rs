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

use std::sync::Arc;
use nih_plug_egui::egui::{self, RichText};
use egui_plot::{Line, Plot, PlotBounds, PlotPoints, Points};
use crate::constants::TWO_PI;
use crate::engine::{ChartType, SynthComputeEngine};

/// A harmonic quieter than this fraction of its bucket's loudest harmonic
/// (−40 dB) has no phase worth drawing: on a real recording it is analysis
/// noise, spread over the whole 0..2π range, burying the curves that matter.
/// (D5.wav, 64 harmonics: at −60 dB 17% of adjacent buckets wrap 0↔2π, at
/// −40 dB 12.5%; the wraps that remain are what [`wrap_runs`] breaks.)
const QUIET_REL: f32 = 1e-2;

/// How the Phase chart draws each harmonic's phase.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PhaseView {
    /// The stored absolute phase, 0..2π.
    Raw,
    /// φₖ − k·φ₁, wrapped to −π..π: the waveform's shape, independent of
    /// where each bucket starts. Not the default: an analysed bucket is
    /// already one period, so a loud harmonic's raw phase is steady (median
    /// 0.06–0.3 rad per bucket on D5.wav) and subtracting k·φ₁ only adds the
    /// fundamental's own wobble, k times over.
    Relative,
}

/// Split one harmonic's `(bucket, phase)` points into runs to draw as
/// separate lines: at a gap (a hidden bucket) and wherever the phase wraps —
/// a step of more than π is a crossing of the 0/2π seam, and joining it
/// would draw a full-height vertical line through the chart.
fn wrap_runs(points: &[(usize, f32)]) -> Vec<Vec<[f64; 2]>> {
    let mut runs: Vec<Vec<[f64; 2]>> = Vec::new();
    let mut prev: Option<(usize, f32)> = None;
    for &(b, v) in points {
        let joined = matches!(prev, Some((pb, pv)) if pb + 1 == b && (v - pv).abs() <= std::f32::consts::PI);
        if !joined {
            runs.push(Vec::new());
        }
        runs.last_mut().unwrap().push([b as f64, v as f64]);
        prev = Some((b, v));
    }
    runs
}

/// `phase[n][b]` relative to the fundamental, wrapped to −π..π.
fn relative_phase(phase: &[Vec<f32>], n: usize, b: usize) -> f32 {
    let k = (n + 1) as f32;
    let rel = (phase[n][b] - k * phase[0][b]).rem_euclid(TWO_PI);
    if rel > std::f32::consts::PI { rel - TWO_PI } else { rel }
}

/// `audible[n][b]`: harmonic `n` is at least [`QUIET_REL`] of bucket `b`'s
/// loudest harmonic. A silent bucket has no audible harmonic at all.
fn audible_mask(amplitude: &[Vec<f32>]) -> Vec<Vec<bool>> {
    let buckets = amplitude.iter().map(|d| d.len()).max().unwrap_or(0);
    let loudest: Vec<f32> = (0..buckets)
        .map(|b| amplitude.iter().filter_map(|d| d.get(b)).fold(0.0f32, |m, &a| m.max(a.abs())))
        .collect();
    amplitude
        .iter()
        .map(|d| {
            d.iter()
                .enumerate()
                .map(|(b, &a)| loudest[b] > 0.0 && a.abs() >= QUIET_REL * loudest[b])
                .collect()
        })
        .collect()
}

pub fn draw_harmonic_plot(
    ui: &mut nih_plug_egui::egui::Ui,
    title: &str,
    chart_type: ChartType,
    chart_w: f32,
    chart_h: f32,
    synth_compute_engine: &Arc<SynthComputeEngine>,
) {
    let is_amp = matches!(chart_type, ChartType::Amp);

    // The Amplitude chart carries a compact Y-axis "zoom" slider that sets the
    // axis maximum: a smaller max magnifies the curves, a larger max zooms out.
    // Persist it in egui memory so it survives the per-frame redraw.
    let ymax_id = egui::Id::new("live_harmonics_amp_ymax");
    let mut amp_ymax: f32 = ui
        .ctx()
        .memory(|m| m.data.get_temp(ymax_id))
        .unwrap_or(1.0);

    // The Phase chart's view (relative to the fundamental, or raw), likewise
    // persisted in egui memory.
    let view_id = egui::Id::new("live_harmonics_phase_view");
    let mut phase_view: PhaseView = ui
        .ctx()
        .memory(|m| m.data.get_temp(view_id))
        .unwrap_or(PhaseView::Raw);

    ui.horizontal(|ui| {
        ui.label(RichText::new(title).strong().size(16.0));
        if is_amp {
            // Push the zoom control away from the main "Amplitude" caption.
            ui.add_space(24.0);
            ui.label(RichText::new("y-axis max").size(12.0));
            ui.add_space(4.0);
            // Keep the slider short so it sits within the label row without
            // crowding or overlapping the chart below.
            ui.spacing_mut().slider_width = 70.0;
            ui.add(egui::Slider::new(&mut amp_ymax, 0.05..=1.0).show_value(false))
                .on_hover_text("Amplitude axis max (zoom)");
        } else {
            ui.add_space(24.0);
            ui.selectable_value(&mut phase_view, PhaseView::Raw, RichText::new("raw").size(12.0))
                .on_hover_text("The stored absolute phase, 0..2π");
            ui.selectable_value(&mut phase_view, PhaseView::Relative, RichText::new("relative").size(12.0))
                .on_hover_text("Phase relative to the fundamental (φₖ − k·φ₁), −π..π: the waveform's shape");
        }
    });
    if is_amp {
        ui.ctx().memory_mut(|m| m.data.insert_temp(ymax_id, amp_ymax));
    } else {
        ui.ctx().memory_mut(|m| m.data.insert_temp(view_id, phase_view));
    }

    let plot_id = match chart_type {
        ChartType::Amp => "Amplitude Plot",
        ChartType::Phase => "Phase Plot",
    };

    let (y_min, y_max) = match (chart_type, phase_view) {
        (ChartType::Amp, _) => (0.0, amp_ymax as f64),
        (ChartType::Phase, PhaseView::Relative) => (-std::f64::consts::PI, std::f64::consts::PI),
        (ChartType::Phase, PhaseView::Raw) => (0.0, TWO_PI as f64),
    };

    let plot = Plot::new(plot_id)
        .height(chart_h)
        .width(chart_w)
        .allow_zoom([false, false])
        .allow_scroll([false, false])
        .allow_drag([false, false])
        .include_y(y_min)
        .include_y(y_max);

    // Phase is only drawn where its harmonic is audible. Taken (and the lock
    // dropped) before the phase lock below.
    let audible = if is_amp {
        None
    } else {
        Some(audible_mask(&synth_compute_engine.shared_params.amplitude_data.lock().unwrap()))
    };

    plot.show(ui, |plot_ui| {
            let (data, enabled_flags) = match chart_type {
                ChartType::Amp => (
                    synth_compute_engine
                        .shared_params
                        .amplitude_data
                        .lock()
                        .unwrap(),
                    synth_compute_engine
                        .shared_params
                        .harmonic_ampl_enabled
                        .lock()
                        .unwrap(),
                ),
                ChartType::Phase => (
                    synth_compute_engine
                        .shared_params
                        .phase_data
                        .lock()
                        .unwrap(),
                    synth_compute_engine
                        .shared_params
                        .harmonic_phase_enabled
                        .lock()
                        .unwrap(),
                ),
            };

            for (n, line_data) in data.iter().enumerate() {
                if !enabled_flags[n] || line_data.iter().all(|&x| x.abs() < 1e-10) {
                    continue;
                }
                let color = crate::gui::harmonic_color(n);
                let name = format!("Harmonic {}", n + 1);

                let Some(audible) = &audible else {
                    let points: PlotPoints = line_data
                        .iter()
                        .enumerate()
                        .map(|(i, &val)| [i as f64, val as f64])
                        .collect();
                    plot_ui.line(Line::new(points).color(color).name(name));
                    continue;
                };

                // Phase: only the audible buckets (a relative phase also needs
                // the fundamental audible, or φ₁ is noise too), one line per run.
                let shown = |b: usize| {
                    audible.get(n).and_then(|m| m.get(b)).copied().unwrap_or(false)
                        && (phase_view == PhaseView::Raw
                            || audible.first().and_then(|m| m.get(b)).copied().unwrap_or(false))
                };
                let points: Vec<(usize, f32)> = (0..line_data.len())
                    .filter(|&b| shown(b))
                    .map(|b| match phase_view {
                        PhaseView::Raw => (b, line_data[b]),
                        PhaseView::Relative => (b, relative_phase(&data, n, b)),
                    })
                    .collect();
                for run in wrap_runs(&points) {
                    if run.len() == 1 {
                        // A lone bucket has no segment to draw.
                        plot_ui.points(Points::new(run).color(color).radius(1.5).name(&name));
                    } else {
                        plot_ui.line(Line::new(PlotPoints::from(run)).color(color).name(&name));
                    }
                }
            }

            // Pin both axes so the x-axis always spans the full bucket count,
            // even when every curve is skipped (all-zero, disabled or quiet)
            // and egui_plot has no data to auto-fit to. Derive the x-range from
            // the bucket count (all curves share the same length) rather than
            // the current plot bounds. The amplitude y-axis is a hard zoom to
            // [0, amp_ymax]; the phase y-axis spans its view's range.
            let x_max = data
                .iter()
                .map(|d| d.len())
                .max()
                .unwrap_or(1)
                .saturating_sub(1)
                .max(1) as f64;
            plot_ui.set_plot_bounds(PlotBounds::from_min_max(
                [0.0, y_min],
                [x_max, y_max],
            ));
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_shift_cancels_in_the_relative_phase() {
        // Waveform shape: harmonic k sits at 0.3·k rad relative to the
        // fundamental. Shift the window by δ of the fundamental's phase: each
        // harmonic's absolute phase turns by k·δ and wraps around 2π.
        let shape = |k: f32| 0.3 * k;
        for &delta in &[0.0f32, 1.0, 2.5, 6.0] {
            let phase: Vec<Vec<f32>> = (1..=12)
                .map(|k| vec![(shape(k as f32) + k as f32 * delta).rem_euclid(TWO_PI)])
                .collect();
            for n in 0..12 {
                let k = (n + 1) as f32;
                let want = (shape(k) - k * shape(1.0)).rem_euclid(TWO_PI);
                let want = if want > std::f32::consts::PI { want - TWO_PI } else { want };
                let got = relative_phase(&phase, n, 0);
                assert!((got - want).abs() < 1e-3, "δ={delta} harmonic {k}: {got} vs {want}");
            }
        }
    }

    #[test]
    fn a_relative_phase_near_zero_does_not_flip_to_two_pi() {
        // Fundamental at 0, harmonic 2 just below 0 (absolute ≈ 2π).
        let phase = vec![vec![0.0], vec![TWO_PI - 0.01]];
        assert!((relative_phase(&phase, 1, 0) + 0.01).abs() < 1e-4);
    }

    #[test]
    fn a_wrap_or_a_gap_breaks_the_line() {
        let pts = [(0, 6.2), (1, 6.25), (2, 0.05), (3, 0.1), (5, 0.12), (6, 3.0)];
        let lens: Vec<usize> = wrap_runs(&pts).iter().map(|r| r.len()).collect();
        // 6.25 → 0.05 wraps; 3 → 5 skips a bucket; 0.12 → 3.0 is a real move.
        assert_eq!(lens, vec![2, 2, 2]);
    }

    #[test]
    fn quiet_harmonics_and_silent_buckets_are_not_audible() {
        let amplitude = vec![vec![1.0, 0.0], vec![1e-1, 0.0], vec![1e-3, 0.0]];
        let m = audible_mask(&amplitude);
        assert_eq!(m[0], vec![true, false]);
        assert_eq!(m[1], vec![true, false]);
        assert_eq!(m[2], vec![false, false]);
    }
}
