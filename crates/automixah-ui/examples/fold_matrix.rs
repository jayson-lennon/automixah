//! Simulates the output-callback math across rate/channel combinations,
//! asserting the src buffer always covers RateFolder consumption (the
//! crackle was fold running dry and repeating its last frame).
use automixah_ui_lib::audio::output::RateFolder;
use automixah_ui_lib::audio::scrub::ScrubCore;
use std::f32::consts::TAU;

fn main() {
    let combos = [
        (48_000u32, 1usize, 44_100u32, 2usize), // opus mono on 44.1k stereo DAC (the bug)
        (48_000, 2, 44_100, 2),
        (48_000, 2, 48_000, 2), // passthrough
        (44_100, 2, 48_000, 2), // opposite direction
        (44_100, 1, 44_100, 2), // mono passthrough rate, channel fold
        (44_100, 2, 44_100, 2),
        (48_000, 1, 48_000, 2),
        (22_050, 1, 44_100, 2), // 2x upsample
    ];
    for &(src_rate, src_ch, dev_rate, dev_ch) in &combos {
        let hz = 440.0f32;
        let seconds = 2.0f32;
        #[allow(clippy::cast_possible_truncation)]
        let src_frames = (src_rate as f32 * seconds) as usize;
        let pcm: Vec<f32> = (0..src_frames)
            .flat_map(|i| {
                let v = (TAU * hz * i as f32 / src_rate as f32).sin() * 0.8;
                vec![v; src_ch]
            })
            .collect();

        let mut scrub = ScrubCore::new(src_ch, 0.0);
        scrub.set_speed(1.0);
        let mut folder = RateFolder::new(dev_ch, src_rate, dev_rate);
        let callback_frames = 512usize;
        let mut full: Vec<f32> = Vec::new();

        for _ in 0..(dev_rate as usize * seconds as usize / callback_frames) {
            // Exactly the callback math from output.rs.
            let device_frames = callback_frames;
            #[allow(clippy::cast_possible_truncation)]
            let needed =
                (device_frames as f32 * src_rate as f32 / dev_rate as f32).ceil() as usize + 4;
            let mut src = vec![0.0_f32; needed * src_ch];
            scrub.read(&pcm, &mut src);

            // Channel fold mirrors the production callback: same frame
            // count as src, then fold consumes at the rate ratio.
            let src_frame_count = src.len() / src_ch;
            let mut chan = vec![0.0_f32; src_frame_count * dev_ch];
            for f in 0..src_frame_count {
                for oc in 0..dev_ch {
                    let c = if src_ch == 1 { 0 } else { oc.min(src_ch - 1) };
                    chan[f * dev_ch + oc] = src[f * src_ch + c];
                }
            }

            let mut folded = vec![0.0_f32; device_frames * dev_ch];
            let consumed = folder.fold(&chan, &mut folded);
            let consumed_needed =
                (device_frames as f32 * src_rate as f32 / dev_rate as f32).ceil() as usize;
            assert!(
                consumed >= consumed_needed.saturating_sub(2),
                "src_rate={src_rate} src_ch={src_ch} dev_rate={dev_rate}: consumed {consumed} < needed {consumed_needed} — DRY FOLD"
            );

            full.extend(folded.iter().step_by(dev_ch));
        }
        // Frequency measured over the WHOLE stream — per-callback windows
        // hold too few cycles (~5) and quantize the crossing count.
        let mut crossings = 0usize;
        let mut first = None;
        let mut last = None;
        for w in full.windows(2) {
            if w[0] < 0.0 && w[1] >= 0.0 {
                crossings += 1;
                if first.is_none() {
                    first = Some(crossings);
                }
                last = Some(crossings);
            }
        }
        let avg = match (first, last) {
            (Some(f), Some(l)) if l > f => dev_rate as f32 * (l - f) as f32 / full.len() as f32,
            _ => 0.0,
        };
        println!("src={src_rate}/{src_ch}ch dev={dev_rate}/{dev_ch}ch → measured {avg:.1} Hz",);
        assert!(
            (avg - 440.0).abs() < 15.0,
            "src={src_rate}/{src_ch} dev={dev_rate}/{dev_ch}: tone broken ({avg:.1} Hz) — crackle"
        );
    }
    println!("ALL COMBOS CLEAN");
}
