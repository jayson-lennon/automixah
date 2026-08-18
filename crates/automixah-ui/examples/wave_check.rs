//! Headless waveform verification: decode a track, build peaks, verify
//! render geometry across the zoom range (no GPU needed — we exercise the
//! same math the egui painter uses).

use automixah_ui_lib::audio::peaks::Peaks;
use automixah_ui_lib::view::waveform::{
    FRAMES_PER_PIXEL_MAX, FRAMES_PER_PIXEL_MIN, WaveformView, total_frames,
};
use djcore::analyzer::AudioAnalyzer as _;
use djcore::analyzer::StratumAnalyzer;
use djcore::decoder::DecoderRegistry;

fn main() {
    let path = std::env::args().nth(1).expect("usage: wave_check <audio>");
    let bytes = std::fs::read(&path).expect("read");
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    let registry = DecoderRegistry::with_symphonia();
    let audio = registry.decode(&bytes, &ext).expect("decode");
    let peaks = Peaks::build(&audio.samples, audio.sample_rate);

    println!(
        "decoded: {:.1}s, {} frames, {} peak quartets ({:.1} per second)",
        audio.frames() as f32 / audio.sample_rate as f32,
        audio.frames(),
        peaks.data.len(),
        peaks.data.len() as f32 / (audio.frames() as f32 / audio.sample_rate as f32)
    );

    let total = total_frames(&peaks);
    let mut view = WaveformView::default();
    for fpp in [FRAMES_PER_PIXEL_MAX, 2048.0, FRAMES_PER_PIXEL_MIN] {
        view.frames_per_pixel = fpp;
        view.clamp_pan(total, 1920.0);
        let left = view.left_frame;
        let right = left + fpp * 1920.0;
        // Per-pixel aggregation must stay in bounds at every zoom.
        let px = ((right - left) / fpp) as usize;
        for i in 0..px {
            let lo = left + i as f32 * fpp;
            let hi = lo + fpp;
            if hi <= 0.0 || lo >= total {
                continue;
            }
            let a = (lo.max(0.0) / peaks.stride_frames).floor() as usize;
            let b = (((hi.min(total) / peaks.stride_frames).ceil() as usize).max(a + 1))
                .min(peaks.data.len());
            assert!(a <= peaks.data.len(), "index overflow at zoom {fpp}");
            let _ = &peaks.data[a..b];
        }
        println!("zoom {fpp:>8.1} fpp: left {left:.0} right {right:.0} OK");
    }

    let out = StratumAnalyzer::new()
        .analyze(&audio.to_mono(), audio.sample_rate)
        .expect("analyze");
    println!(
        "grid: {:.3} BPM, anchor {:.3}s, {} beats",
        out.beat_grid.grid_bpm,
        out.beat_grid.anchor_seconds,
        out.beat_grid.beats.len()
    );
    println!("PASS");
}
