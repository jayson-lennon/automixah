//! Golden-fixture and behavioral tests for compiled preset curves.
//!
//! - Golden fixtures: each preset compiles to a deterministic curve
//!   set at fixed inputs; changes are caught against committed goldens
//!   (regenerate with `AUTOUPDATE_GOLDEN=1 cargo test`).
//! - BassSwap energy swap: low-band energy measured with Goertzel
//!   band filters moves from A to B across the boundary, driven
//!   purely by bus state (the automation contract, ahead of the
//!   Phase 5 DSP).

use automixah_engine::automation::ControlSource;
use automixah_engine::automation::TimelineSource;
use automixah_engine::automation::presets::{PresetSpec, compile_preset, preset_specs};
use automixah_engine::control::{ControlBus, DeckId, ParamAddress};
use automixah_engine::timeline::types::{SessionTime, TransitionWindow};

const WINDOW_LEN: u64 = 705_600; // 16 s at 44.1 kHz
const BPM: f32 = 120.0;
const RATE: u32 = 44_100;

fn window() -> TransitionWindow {
    TransitionWindow {
        start: SessionTime(1000),
        end: SessionTime(1000 + WINDOW_LEN),
    }
}

/// Samples a deck's curve for one address across a compiled timeline.
fn curve(
    events: &[automixah_engine::control::ControlEvent],
    deck: DeckId,
    address: ParamAddress,
) -> Vec<f32> {
    let mut out: Vec<f32> = events
        .iter()
        .filter(|e| e.deck == deck && e.address == address)
        .map(|e| e.value)
        .collect();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Stable text digest of a preset's compiled curves.
fn digest(spec: &PresetSpec) -> String {
    let events = compile_preset(spec, window(), BPM, RATE);
    let mut lines = vec![format!("preset {} beats {}", spec.name, spec.beats)];
    for deck in [DeckId::A, DeckId::B] {
        for address in [
            ParamAddress::Gain,
            ParamAddress::EqLow,
            ParamAddress::EqMid,
            ParamAddress::EqHigh,
            ParamAddress::HpfCutoff,
            ParamAddress::LpfCutoff,
        ] {
            let values = curve(&events, deck, address);
            if !values.is_empty() {
                let rendered: Vec<String> = values.iter().map(|v| format!("{v:.4}")).collect();
                lines.push(format!("{deck:?}/{address:?}: {}", rendered.join(",")));
            }
        }
    }
    lines.join("\n")
}

#[test]
fn compiled_curves_match_golden_fixtures() {
    // Given the four presets and their golden digests.
    let golden_path = "tests/golden/presets.txt";

    // When compiling digests.
    let digests: Vec<String> = preset_specs().iter().map(digest).collect();

    if std::env::var("AUTOUPDATE_GOLDEN").is_ok() {
        std::fs::create_dir_all("tests/golden").expect("mkdir");
        std::fs::write(golden_path, digests.join("\n\n")).expect("write golden");
    }

    // Then they match the committed goldens byte-for-byte.
    let golden = std::fs::read_to_string(golden_path).unwrap_or_default();
    assert_eq!(
        digests.join("\n\n"),
        golden,
        "golden mismatch — if intentional, regenerate with AUTOUPDATE_GOLDEN=1"
    );
}

/// Goertzel magnitude of `samples` at `freq`.
fn goertzel(samples: &[f32], freq: f32, rate: u32) -> f32 {
    #[expect(clippy::cast_precision_loss, reason = "sample rates are exact in f32")]
    let k = 2.0 * std::f32::consts::PI * freq / rate as f32;
    let (mut s_prev, mut s_prev2) = (0.0_f32, 0.0_f32);
    let coeff = 2.0 * k.cos();
    for &x in samples {
        let s = x + coeff * s_prev - s_prev2;
        s_prev2 = s_prev;
        s_prev = s;
    }
    let power = s_prev * s_prev + s_prev2 * s_prev2 - coeff * s_prev * s_prev2;
    power.abs().sqrt()
}

/// A windowed slice of a sine.
fn sine(freq: f32, rate: u32, len: usize, t0: usize) -> Vec<f32> {
    (0..len)
        .map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "test sample indices are small")]
            let t = (t0 + i) as f32 / rate as f32;
            (t * freq * 2.0 * std::f32::consts::PI).sin() * 0.5
        })
        .collect()
}

/// EQ-low normalized value → linear low-band gain.
/// 0.5 = unity (0 dB); ±12 dB across [0, 1].
fn low_band_gain(eq_low: f32) -> f32 {
    let db = (eq_low - 0.5) * 24.0;
    10.0_f32.powf(db / 20.0)
}

#[test]
fn bass_swap_moves_low_band_energy_from_a_to_b() {
    // Given a compiled BassSwap timeline and two synthetic decks:
    // A carries only 100 Hz; B carries 100 Hz + 1 kHz.
    let spec = PresetSpec::bass_swap();

    let frame = 4096;
    let low_hz = 100.0_f32;
    let high_hz = 1000.0_f32;
    let rate = RATE;

    let a_signal = |t0: usize| sine(low_hz, rate, frame, t0);
    let b_signal = |t0: usize| {
        let low = sine(low_hz, rate, frame, t0);
        let high = sine(high_hz, rate, frame, t0);
        let combined: Vec<f32> = low.iter().zip(&high).map(|(l, h)| l + h).collect();
        combined
    };

    // When measuring low-band energy near the window start and end.
    let measure = |at_start: bool| -> (f32, f32) {
        #[expect(clippy::cast_possible_truncation, reason = "test window fits usize")]
        let t0 = if at_start {
            2000
        } else {
            1000 + WINDOW_LEN as usize - frame - 10
        };
        // Drive the bus to that point from scratch (source polls are
        // cumulative; recompile for the second pass).
        let events2 = compile_preset(&spec, window(), BPM, RATE);
        let mut src = TimelineSource::new("bass", events2);
        let mut b2 = ControlBus::new();
        let until = SessionTime(t0 as u64 + 1000);
        b2.apply_all(&src.poll(until));

        let a_gain = b2.get(DeckId::A, ParamAddress::Gain);
        let b_gain = b2.get(DeckId::B, ParamAddress::Gain);
        let a_low = low_band_gain(b2.get(DeckId::A, ParamAddress::EqLow));
        let b_low = low_band_gain(b2.get(DeckId::B, ParamAddress::EqLow));

        let a: Vec<f32> = a_signal(t0).iter().map(|x| x * a_gain * a_low).collect();
        let b: Vec<f32> = b_signal(t0).iter().map(|x| x * b_gain * b_low).collect();

        let a_low_energy = goertzel(&a, low_hz, rate);
        let b_low_energy = goertzel(&b, low_hz, rate);
        (a_low_energy, b_low_energy)
    };

    let (a_start, b_start) = measure(true);
    let (a_end, b_end) = measure(false);

    // Then A's low-band energy collapses and B's takes over.
    assert!(
        a_start > a_end * 10.0,
        "A low-band did not collapse: start={a_start} end={a_end}"
    );
    assert!(
        b_end > b_start * 10.0,
        "B low-band did not take over: start={b_start} end={b_end}"
    );
}
