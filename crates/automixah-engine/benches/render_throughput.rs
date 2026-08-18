//!
//! AC2 verification record: 2026-08-17, this machine —
//! 3-min synthetic session in 2.28 s ⇒ ~79× realtime (bar: ≥4×). Render-throughput benchmark: the engine must render a session
//! ≥4× faster than realtime (Phase 5 acceptance).

use automixah_engine::render::renderer::{Renderer, TrackFetchError, TrackProvider};
use automixah_engine::timeline::types::{
    PresetName, Segment, SessionPlan, SessionTime, StretchDecision, StretchMode, TrackHash,
    TransitionPlan, TransitionWindow,
};
use criterion::Criterion;

/// Session-rate PCM: 3 minutes of dense-ish material.
const LEN: usize = 44_100 * 180;

struct BenchProvider {
    pcm: Vec<f32>,
}

impl TrackProvider for BenchProvider {
    fn stretched_pcm(&mut self, _hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
        Ok(&self.pcm)
    }

    fn name(&self) -> &'static str {
        "bench"
    }
}

fn pcm() -> Vec<f32> {
    (0..LEN)
        .map(|i| {
            #[expect(clippy::cast_precision_loss, reason = "bench index")]
            let t = i as f32 / 44_100.0;
            ((2.0 * std::f32::consts::PI * 220.0 * t).sin()
                + (2.0 * std::f32::consts::PI * 277.0 * t).sin())
                * 0.25
        })
        .collect()
}

fn plan() -> SessionPlan {
    let half = LEN as u64 / 2;
    SessionPlan {
        session_bpm: 120.0,
        sample_rate: 44_100,
        segments: vec![
            Segment {
                track_hash: TrackHash("a".into()),
                src_start: 0,
                session_start: SessionTime(0),
                len_samples: half + 22_050,
                stretch: StretchDecision {
                    mode: StretchMode::Resample,
                    ratio: 1.0,
                    out_of_comfort_band: false,
                    strategy: automixah_engine::timeline::types::TempoStrategy::SessionBpm,
                },
                transition: Some(TransitionPlan {
                    window: TransitionWindow {
                        start: SessionTime(half - 22_050),
                        end: SessionTime(half + 22_050),
                    },
                    preset: PresetName("Crossfade".into()),
                }),
            },
            Segment {
                track_hash: TrackHash("b".into()),
                src_start: 0,
                session_start: SessionTime(half - 22_050),
                len_samples: half,
                stretch: StretchDecision {
                    mode: StretchMode::Resample,
                    ratio: 1.0,
                    out_of_comfort_band: false,
                    strategy: automixah_engine::timeline::types::TempoStrategy::SessionBpm,
                },
                transition: None,
            },
        ],
    }
}

fn bench_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("render");
    group.throughput(criterion::Throughput::Elements(LEN as u64));
    group.sample_size(10);
    group.bench_function("3min_session", |b| {
        b.iter(|| {
            let mut r = Renderer::new(plan());
            let mut p = BenchProvider { pcm: pcm() };
            let out = r.render_until(&mut p, SessionTime(LEN as u64));
            criterion::black_box(out)
        });
    });
    group.finish();
}

criterion::criterion_group!(benches, bench_render);
criterion::criterion_main!(benches);
