//! Grid-lock verification: kick transients (low-band Bessel envelope) vs the
//! UI's `EditableGrid` projection — the exact model a user edits and saves.
//!
//! Trance kicks sit on beats, so the median |kick − nearest beat| is the
//! honest lock metric. We also emulate a user's manual anchor correction
//! (global shift search through `EditableGrid`) and verify it locks.

use automixah_ui_lib::audio::bands::Cascade;
use automixah_ui_lib::grid::EditableGrid;
use djcore::analyzer::{AudioAnalyzer, StratumAnalyzer};
use djcore::decoder::DecoderRegistry;

fn main() {
    let path = std::env::args().nth(1).expect("usage: align_check <file>");
    let bytes = std::fs::read(&path).expect("read");
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    let decoded = DecoderRegistry::with_symphonia()
        .decode(&bytes, &ext)
        .expect("decode");
    let mono = decoded.to_mono();
    let out = StratumAnalyzer::new()
        .analyze(&mono, decoded.sample_rate)
        .expect("analyze");
    let eg = EditableGrid::from_grid(&out.beat_grid);
    let beats = eg.project_to(600.0).beats;

    // Kick ground truth: low-band (<600 Hz Bessel, same as the waveform
    // view) envelope peaks. The loudest event per beat-sized window is the
    // kick; its phase vs the grid anchor is the alignment error.
    let events = low_band_events(&mono, decoded.sample_rate);
    let period = 60.0 / eg.grid_bpm;
    let first = beats[4];
    let last = *beats.last().unwrap() - 2.0;
    let body: Vec<(f32, f32)> = events
        .iter()
        .filter(|(t, _)| *t > first && *t < last)
        .copied()
        .collect();
    let kicks = loudest_per_window(&body, period, &body_amp_total(&body));

    // Median phase error of kicks vs the grid (mod beat) — the number a
    // human would measure dragging the anchor slider.
    let phase_err = |anchor: f32| -> f32 {
        let mut ph: Vec<f32> = kicks
            .iter()
            .map(|(t, _)| {
                let phase = (t - anchor).rem_euclid(period);
                phase.min(period - phase)
            })
            .collect();
        ph.sort_by(f32::total_cmp);
        ph[ph.len() / 2]
    };

    let before = phase_err(eg.anchor_seconds);

    // Emulate the user's manual correction: scan anchor shifts, take the
    // minimum-median-error lag, apply through EditableGrid (the UI path).
    let mut best = (eg.anchor_seconds, before);
    let mut lag = -250.0f32;
    while lag <= 250.0 {
        let cand = eg.anchor_seconds + lag / 1000.0;
        let err = phase_err(cand);
        if err < best.1 {
            best = (cand, err);
        }
        lag += 2.0;
    }
    let manual = EditableGrid {
        grid_bpm: eg.grid_bpm,
        anchor_seconds: best.0,
        downbeat_phase: eg.downbeat_phase,
    };
    let locked = manual.project_to(600.0);
    let after = phase_err(manual.anchor_seconds);

    println!("kicks={} (of {} body events)", kicks.len(), body.len());
    println!(
        "auto:   median kick-grid phase error {:.0} ms",
        before * 1000.0
    );
    println!(
        "manual: median kick-grid phase error {:.0} ms (anchor {:+.0}ms → {:.3}s)",
        after * 1000.0,
        (best.0 - eg.anchor_seconds) * 1000.0,
        manual.anchor_seconds
    );
    assert_eq!(locked.beats.len(), beats.len(), "projection stable");
    assert!(after < 0.015, "manual alignment failed: {after:.0}ms");
    println!("LOCKED");
}

/// Total amplitude of body events (normalizer for loudest-per-window).
fn body_amp_total(body: &[(f32, f32)]) -> f32 {
    body.iter().map(|(_, a)| a).sum::<f32>().max(1e-9)
}

/// The loudest event in each `period`-sized window advanced by phase scan:
/// returns (time, amp) of the winning event per window at the best phase.
fn loudest_per_window(body: &[(f32, f32)], period: f32, _norm: &f32) -> Vec<(f32, f32)> {
    // Coarse phase scan: for each candidate phase in [0, period), sum the
    // max amplitude within each window; keep the phase with the highest
    // total. Then one kick per window at that phase.
    let t0 = body.first().map_or(0.0, |(t, _)| *t);
    let steps = 40;
    let mut best_phase = 0.0f32;
    let mut best_total = -1.0f32;
    for s in 0..steps {
        let phase = period * s as f32 / steps as f32;
        let mut total = 0.0f32;
        let mut w0 = t0 + phase;
        while w0 < body.last().map_or(0.0, |(t, _)| *t) {
            let loudest = body
                .iter()
                .filter(|(t, _)| *t >= w0 && *t < w0 + period)
                .map(|(_, a)| a)
                .copied()
                .fold(0.0f32, f32::max);
            total += loudest;
            w0 += period;
        }
        if total > best_total {
            best_total = total;
            best_phase = phase;
        }
    }

    let mut out = Vec::new();
    let mut w0 = t0 + best_phase;
    let end = body.last().map_or(0.0, |(t, _)| *t);
    while w0 < end {
        let winner = body
            .iter()
            .filter(|(t, _)| *t >= w0 && *t < w0 + period)
            .min_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(w) = winner {
            out.push(*w);
        }
        w0 += period;
    }
    out
}

/// Low-band events: (time, amplitude) envelope peaks.
fn low_band_events(samples: &[f32], rate: u32) -> Vec<(f32, f32)> {
    let mut low = Cascade::lowpass(f64::from(rate), 600.0);
    let mut env = Vec::with_capacity(samples.len() / 64);
    let mut acc = 0.0f32;
    for (i, s) in samples.iter().enumerate() {
        let y = low.process(f64::from(*s)).abs() as f32;
        acc = acc.max(y);
        if i % 64 == 63 {
            env.push(acc);
            acc = 0.0;
        }
    }
    let mut out = Vec::new();
    for i in 1..env.len().saturating_sub(1) {
        let t = i as f32 * 64.0 / rate as f32;
        if env[i] > env[i - 1] && env[i] >= env[i + 1] {
            out.push((t, env[i]));
        }
    }
    out
}
