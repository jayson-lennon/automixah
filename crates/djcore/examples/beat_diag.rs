//! Diagnostic: print detected BPM, grid stats, and beat-interval stats
//! for one track. Not part of the shipped pipeline.
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
    eprintln!(
        "bpm={:.3} conf={:.3} stability={:.3} beats={} downbeats={}",
        out.bpm,
        out.bpm_confidence,
        out.grid_stability,
        out.beat_grid.beats.len(),
        out.beat_grid.downbeats.len()
    );
    let beats = &out.beat_grid.beats;
    let iv: Vec<f32> = beats.windows(2).map(|w| w[1] - w[0]).collect();
    let (mn, mx) = iv
        .iter()
        .fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    eprintln!("beat intervals: min={mn:.4}s max={mx:.4}s");
    let s: Vec<String> = beats.iter().take(40).map(|t| format!("{t:.3}")).collect();
    eprintln!("first beats: {}", s.join(" "));
    let s: Vec<String> = out
        .beat_grid
        .downbeats
        .iter()
        .take(12)
        .map(|t| format!("{t:.3}"))
        .collect();
    eprintln!("first downbeats: {}", s.join(" "));
    let mut sorted = iv.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    eprintln!(
        "median interval={:.5}s -> bpm={:.3}",
        sorted[sorted.len() / 2],
        60.0 / sorted[sorted.len() / 2]
    );
}
