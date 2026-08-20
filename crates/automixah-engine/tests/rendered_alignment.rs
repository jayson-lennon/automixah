//! T8: rendered-mix beat alignment — two click trains at 138/136 BPM
//! planned at 138 and rendered end to end; the deck clicks in the
//! overlap must coincide within 10 ms (2 ms on the resample path).

use std::collections::HashMap;

use automixah_engine::render::renderer::{Renderer, TrackFetchError};
use automixah_engine::timeline::plan::{PlanOptions, plan_with};
use automixah_engine::timeline::types::{SessionTime, TrackAnalysis, TrackHash};
use djcore::analyzer::BeatGrid;
use djcore::{Key, KeyMode};

/// Builds a click-train analysis: impulse every beat at `bpm`,
/// downbeats every 4 beats (louder clicks carry no information here).
fn click_track(hash: &str, bpm: f32, duration_s: f32) -> TrackAnalysis {
    let beat = 60.0 / bpm;
    let beats = (duration_s / beat).floor() as usize;
    let beat_times = (0..beats).map(|i| i as f32 * beat).collect();
    let downbeats = (0..beats / 4).map(|i| i as f32 * beat * 4.0).collect();
    TrackAnalysis {
        hash: TrackHash(hash.into()),
        bpm,
        bpm_confidence: 0.99,
        key: Key {
            root: 7,
            mode: KeyMode::Minor,
        },
        duration: duration_s,
        beat_grid: BeatGrid {
            grid_bpm: bpm,
            anchor_seconds: 0.0,
            downbeats,
            beats: beat_times,
            bars: Vec::new(),
        },
        grid_stability: 0.99,
        sample_rate: 44_100,
        channels: 2,
        format: "wav".into(),
    }
}

/// Renders click PCM the way the CLI does: stretch native clicks to
/// the session tempo (resample path: position mapping `t → t·ratio`),
/// then slice off the segment's cue so the renderer's
/// segment-relative indexing starts at the audible span.
struct ClickPcm {
    pcms: HashMap<String, Vec<f32>>,
}

impl ClickPcm {
    fn new(
        tracks: &[TrackAnalysis],
        session_bpm: f32,
        plan: &automixah_engine::timeline::types::SessionPlan,
    ) -> Self {
        Self {
            pcms: tracks
                .iter()
                .zip(&plan.segments)
                .map(|(t, seg)| {
                    let ratio = f64::from(t.bpm / session_bpm);
                    let len =
                        ((f64::from(t.duration) * ratio * f64::from(t.sample_rate)) * 2.0) as usize;
                    let mut pcm = vec![0.0_f32; len];
                    for &b in &t.beat_grid.beats {
                        #[expect(
                            clippy::cast_possible_truncation,
                            clippy::cast_sign_loss,
                            reason = "beat samples fit usize"
                        )]
                        let idx = (f64::from(b) * ratio * f64::from(t.sample_rate)) as usize * 2;
                        if idx + 1 < len {
                            pcm[idx] = 1.0;
                            pcm[idx + 1] = 1.0;
                        }
                    }
                    #[expect(
                        clippy::cast_possible_truncation,
                        clippy::cast_sign_loss,
                        reason = "cue bounded by stretched length"
                    )]
                    let cue_frames = ((f64::from(seg.stretch.ratio) * seg.src_start as f64).round()
                        as usize)
                        .min(len / 2);
                    (t.hash.0.clone(), pcm[cue_frames * 2..].to_vec())
                })
                .collect(),
        }
    }
}

impl automixah_engine::render::renderer::TrackProvider for ClickPcm {
    fn name(&self) -> &'static str {
        "click-pcm"
    }

    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
        self.pcms
            .get(&hash.0)
            .map(Vec::as_slice)
            .ok_or(TrackFetchError)
    }
}

/// Finds the dominant click period offsets in the overlap: for each
/// click peak position, records `pos % beat_samples`.
fn click_phases(mix: &[f32], beat_samples: usize) -> Vec<usize> {
    mix.iter()
        .enumerate()
        .step_by(2) // mono view
        .filter(|&(_, &s)| s > 0.05)
        .map(|(i, _)| i / 2 % beat_samples)
        .collect()
}

#[test]
fn overlap_clicks_coincide_within_tolerance() {
    // Given two click trains at 138/136 BPM, 8 bars each, session 138
    // (bar-aligned durations keep the window start on the grid).
    let tracks = vec![
        click_track("a", 138.0, 8.0 * 4.0 * 60.0 / 138.0),
        click_track("b", 136.0, 8.0 * 4.0 * 60.0 / 136.0),
    ];
    let plan = plan_with(
        &tracks,
        PlanOptions {
            target_bpm: Some(138.0),
            transition_beats: 4,
            ..PlanOptions::default()
        },
    );

    let mut provider = ClickPcm::new(&tracks, 138.0, &plan);
    let mut renderer = Renderer::new(plan.clone());
    let mix = renderer
        .render_until(&mut provider, SessionTime(plan.total_len_samples()))
        .expect("render");
    let mono_len = mix.len() / 2;

    // When extracting click phases over the overlap window.
    let window = plan.segments[0]
        .transition
        .as_ref()
        .expect("transition")
        .window;
    #[expect(clippy::cast_possible_truncation, reason = "test: mix bounds")]
    let start = (window.start.0 * 2).min(mix.len() as u64) as usize;
    #[expect(clippy::cast_possible_truncation, reason = "test: mix bounds")]
    let end = (window.end.0 * 2).min(mix.len() as u64) as usize;
    let overlap = &mix[start..end];
    let beat_samples = (60.0 / 138.0 * 44_100.0) as usize;
    let phases = click_phases(overlap, beat_samples);
    if std::env::var("DBG2").is_ok() {
        // first 12 click positions in the overlap (mono samples)
        let pos: Vec<usize> = overlap
            .iter()
            .enumerate()
            .step_by(2)
            .filter(|&(_, &v)| v > 0.5)
            .take(12)
            .map(|(i, _)| i / 2)
            .collect();
        eprintln!(
            "window {}..{} first clicks (rel): {:?}",
            window.start.0, window.end.0, pos
        );
    }

    // Then clicks cluster at ONE phase modulo the session beat (both
    // decks' clicks coincide); the spread is the alignment error.
    assert!(!phases.is_empty(), "no clicks in overlap");
    assert!(
        phases.len() >= 4,
        "expected a click on every beat of the window, got {}",
        phases.len()
    );
    if std::env::var("DBG").is_ok() {
        let mut ph = phases.clone();
        ph.sort_unstable();
        eprintln!(
            "phase histogram (first 60 sorted): {:?}",
            &ph[..ph.len().min(60)]
        );
    }
    let max_phase = phases.iter().copied().max().expect("max");
    let min_phase = phases.iter().copied().min().expect("min");
    let spread = (max_phase - min_phase).min(beat_samples - (max_phase - min_phase));
    let ms = spread as f64 * 1000.0 / 44_100.0;
    assert!(
        ms <= 10.0,
        "click spread {ms:.2} ms exceeds 10 ms tolerance (phases {min_phase}..{max_phase})"
    );
    // On the resample path (in-band 138 vs 136) the expectation is
    // sample-adjacent: assert the tighter 2 ms bound.
    assert!(
        ms <= 2.0,
        "click spread {ms:.2} ms exceeds 2 ms resample-path expectation"
    );
    assert!(mono_len > 0, "mix rendered empty");
}
