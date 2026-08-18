//! Grid-quality report for one track: BPM/anchor, interval and
//! residual-CV continuity, downbeat density, and a PASS/FAIL verdict.
//! Not part of the shipped pipeline.
use djcore::analyzer::{AudioAnalyzer, StratumAnalyzer};
use djcore::decoder::{DecodeAudio, DecoderRegistry};

fn main() {
    let path = std::env::args().nth(1).expect("usage: beat_diag <file>");
    let bytes = std::fs::read(&path).expect("read");
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    let registry = DecoderRegistry::with_symphonia();
    let decoded = registry.decode(&bytes, &ext).expect("decode");
    let d = DecodeAudio {
        samples: decoded.samples.clone(),
        sample_rate: decoded.sample_rate,
        channels: decoded.channels,
    };
    let mono = d.to_mono();
    eprintln!("decoded {} frames @ {} Hz", d.frames(), d.sample_rate);
    let out = StratumAnalyzer
        .analyze(&mono, d.sample_rate)
        .expect("analyze");

    let grid = &out.beat_grid;
    eprintln!(
        "grid_bpm={:.3} anchor={:.4}s conf={:.3} stability={:.3}",
        grid.grid_bpm, grid.anchor_seconds, out.bpm_confidence, out.grid_stability
    );
    eprintln!(
        "beats={} downbeats={} duration={:.1}s",
        grid.beats.len(),
        grid.downbeats.len(),
        out.duration_seconds
    );

    let beat_len = 60.0 / f64::from(grid.grid_bpm);
    let beats = &grid.beats;

    // Interval continuity: no gap larger than 1.5x the grid period.
    let max_interval = beats
        .windows(2)
        .map(|w| f64::from(w[1] - w[0]))
        .fold(0.0_f64, f64::max);
    eprintln!("max interval={max_interval:.4}s (grid period {beat_len:.4}s)");

    // Residual CV: phase consistency of marks against the fitted grid.
    let anchor = f64::from(grid.anchor_seconds);
    let residuals: Vec<f64> = beats
        .iter()
        .map(|&b| {
            let n = ((f64::from(b) - anchor) / beat_len).round();
            (f64::from(b) - anchor - n * beat_len).abs()
        })
        .collect();
    #[expect(clippy::cast_precision_loss, reason = "diagnostic: small counts")]
    let rms = (residuals.iter().map(|r| r * r).sum::<f64>() / residuals.len() as f64).sqrt();
    let max_r = residuals.iter().copied().fold(0.0_f64, f64::max);
    eprintln!(
        "residual rms={:.2}ms max={:.2}ms",
        rms * 1000.0,
        max_r * 1000.0
    );

    // Downbeat density: one per bar on a constant grid.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "diagnostic: bar count is small"
    )]
    let bars = (f64::from(out.duration_seconds) / (beat_len * 4.0)) as usize;
    #[expect(clippy::cast_precision_loss, reason = "diagnostic: small counts")]
    let db_per_bar = if bars > 0 {
        grid.downbeats.len() as f64 / bars as f64
    } else {
        0.0
    };
    eprintln!("downbeats/bar={db_per_bar:.3} ({bars} bars)");

    let checks = [
        (
            "near-musical bpm",
            nearest_musical_delta(f64::from(grid.grid_bpm)) < 0.25,
        ),
        ("continuous beats", max_interval <= beat_len * 1.5),
        ("residuals < 25ms", max_r < 0.025),
        ("one downbeat/bar", (db_per_bar - 1.0).abs() < 0.05),
        ("high stability", out.grid_stability > 0.5),
    ];
    for (name, ok) in checks {
        eprintln!("  [{}] {name}", if ok { "PASS" } else { "FAIL" });
    }
    let verdict = checks.iter().all(|&(_, ok)| ok);
    eprintln!("VERDICT: {}", if verdict { "PASS" } else { "FAIL" });
    let s: Vec<String> = beats.iter().take(24).map(|t| format!("{t:.3}")).collect();
    eprintln!("first beats: {}", s.join(" "));
}

/// Distance in BPM from the nearest musically plausible value
/// (integer / half / third / twelfth grid).
fn nearest_musical_delta(bpm: f64) -> f64 {
    [1.0, 2.0, 3.0, 12.0]
        .iter()
        .map(|&f| ((bpm * f).round() / f - bpm).abs())
        .fold(f64::INFINITY, f64::min)
}
