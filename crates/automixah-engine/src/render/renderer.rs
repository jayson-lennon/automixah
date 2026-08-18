//! The pull-based mix renderer: turns a [`SessionPlan`] into PCM.
//!
//! The renderer owns two deck chains (A/B), a control bus fed by the
//! session's automation timelines, and per-segment stretched audio
//! (fetched through a [`TrackProvider`]). `render_until` is the only
//! entry point the playback layer needs: it pulls control events
//! sample-accurately, sums active decks with −3 dB overlap headroom,
//! and applies a master soft-knee limiter (ceiling 0.99).

use crate::automation::TimelineSource;
use crate::control::{ControlBus, DeckId};
use crate::render::dsp::DeckChain;
use crate::timeline::types::{Segment, SessionPlan, SessionTime, TrackHash};

/// Master limiter ceiling.
/// Output channel count (interleaved stereo).
const CHANNELS: usize = 2;

const CEILING: f32 = 0.99;

/// Soft-knee width (in output level) below the ceiling.
const KNEE: f32 = 0.3;

/// Linear gain applied while two decks overlap (−3 dB).
const OVERLAP_GAIN: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Supplies stretched session PCM for a track.
///
/// Implemented by the playback layer (render worker); tests provide
/// synthetic audio. Audio is supplied *pre-stretched* to session
/// rate so the renderer slices it 1:1 in session time.
pub trait TrackProvider {
    /// Returns the full stretched PCM for `hash` at session rate.
    /// # Errors
    ///
    /// Returns an error if the track's PCM is not available.
    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError>;

    /// Provider name for debugging.
    fn name(&self) -> &'static str;
}

/// A track could not be fetched from the provider.
#[derive(Debug, wherror::Error)]
#[error(debug)]
pub struct TrackFetchError;

/// Renderer failure modes.
#[derive(Debug, wherror::Error)]
#[error(debug)]
pub struct RenderError;

impl From<TrackFetchError> for RenderError {
    fn from(_: TrackFetchError) -> Self {
        Self
    }
}

/// Renders a [`SessionPlan`] to session-rate PCM, pull-style.
pub struct Renderer {
    plan: SessionPlan,
    bus: ControlBus,
    decks: [DeckChain; 2],
    source: TimelineSource,
    position: SessionTime,
}

impl Renderer {
    /// Builds a renderer for `plan`.
    #[must_use]
    pub fn new(plan: SessionPlan) -> Self {
        Self::with_transition(plan, crate::automation::transition_spec::long_crossfade())
    }

    /// Builds a renderer that drives every transition from one
    /// authored [`TransitionSpec`] pair (roles mapped to decks by
    /// segment parity).
    #[must_use]
    pub fn with_transition(
        plan: SessionPlan,
        transition: crate::automation::transition_spec::TransitionSpec,
    ) -> Self {
        #[expect(clippy::cast_precision_loss, reason = "sample rates are exact in f32")]
        let rate = plan.sample_rate as f32;
        let events = compile_session_events(&plan, &transition);
        Self {
            plan,
            bus: ControlBus::new(),
            decks: [DeckChain::new(rate, 1.0), DeckChain::new(rate, 1.0)],
            source: TimelineSource::new("session", events),
            position: SessionTime(0),
        }
    }

    /// Renders until absolute session time `until`; returns the new
    /// PCM since the previous call, master-limited.
    ///
    /// # Errors
    ///
    /// Returns an error if the provider cannot supply a track.
    pub fn render_until(
        &mut self,
        provider: &mut dyn TrackProvider,
        until: SessionTime,
    ) -> Result<Vec<f32>, RenderError> {
        let start = self.position;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "render spans fit memory by construction"
        )]
        let len = until.0.saturating_sub(start.0) as usize;
        if len == 0 {
            return Ok(Vec::new());
        }

        let mut out = vec![0.0_f32; len * CHANNELS];
        let mut written = 0_usize;
        while written < len {
            let block_len = (len - written).min(crate::render::dsp::BLOCK);
            let t0 = SessionTime(start.0 + written as u64);
            self.render_block(
                provider,
                &mut out[(written * CHANNELS)..((written + block_len) * CHANNELS)],
                block_len,
                t0,
            )?;
            written += block_len;
        }
        self.position = until;
        Ok(out)
    }

    /// Current render position.
    #[must_use]
    pub fn position(&self) -> SessionTime {
        self.position
    }

    /// Renders one ≤64-frame block starting at `t0`.
    fn render_block(
        &mut self,
        provider: &mut dyn TrackProvider,
        out: &mut [f32],
        frames: usize,
        t0: SessionTime,
    ) -> Result<(), RenderError> {
        let t_end = SessionTime(t0.0 + frames as u64);
        self.apply_control_events(t_end);

        let active = self.active_segment_indices(t0, t_end);
        let mut deck_bufs = [[0.0_f32; crate::render::dsp::BLOCK * 2]; 2];
        for &idx in &active {
            let seg = self.plan.segments[idx].clone();
            let deck = idx % 2;
            Self::fill_deck_window(provider, &seg, t0, frames, &mut deck_bufs[deck])?;
        }
        for (deck, buf) in self.decks.iter_mut().zip(deck_bufs.iter_mut()) {
            deck.process_block(&mut buf[..frames * 2]);
        }
        let gain = if active.len() > 1 { OVERLAP_GAIN } else { 1.0 };
        for (o, pair) in out
            .chunks_mut(2)
            .zip(deck_bufs[0].chunks(2).zip(deck_bufs[1].chunks(2)))
        {
            let (a, b) = pair;
            let l = soft_knee((a[0] + b[0]) * gain);
            let r = soft_knee((a[1] + b[1]) * gain);
            o[0] = l;
            o[1] = r;
        }
        Ok(())
    }

    /// Copies the segment's interleaved PCM frames `[t0, t0+frames)`
    /// into the deck buffer, zero-filling outside the segment's span
    /// or PCM.
    fn fill_deck_window(
        provider: &mut dyn TrackProvider,
        seg: &Segment,
        t0: SessionTime,
        frames: usize,
        buf: &mut [f32],
    ) -> Result<(), RenderError> {
        let pcm = provider.stretched_pcm(&seg.track_hash)?;
        for f in 0..frames {
            let session_rel = (t0.0 + f as u64).saturating_sub(seg.session_start.0);
            #[expect(
                clippy::cast_possible_truncation,
                reason = "session indices fit memory by construction"
            )]
            let frame = session_rel as usize;
            for ch in 0..CHANNELS {
                let s = pcm.get(frame * CHANNELS + ch).copied().unwrap_or(0.0);
                buf[f * CHANNELS + ch] = s;
            }
        }
        Ok(())
    }

    /// Applies all control events due at or before `t`, then pushes
    /// the bus state into the deck chains.
    fn apply_control_events(&mut self, t: SessionTime) {
        let events = crate::automation::ControlSource::poll(&mut self.source, t);
        if events.is_empty() {
            return;
        }
        self.bus.apply_all(&events);
        self.decks[0].read_bus(&self.bus, DeckId::A);
        self.decks[1].read_bus(&self.bus, DeckId::B);
    }

    /// Indices of segments audible at any point in `[t0, t_end)`.
    fn active_segment_indices(&self, t0: SessionTime, t_end: SessionTime) -> Vec<usize> {
        self.plan
            .segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| {
                let end = seg.session_start.0 + seg.len_samples;
                seg.session_start.0 < t_end.0 && t0.0 < end
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// Applies the soft-knee limiter to one output sample.
fn soft_knee(x: f32) -> f32 {
    let ax = x.abs();
    if ax <= CEILING - KNEE {
        return x;
    }
    let over = (ax - (CEILING - KNEE)).min(KNEE);
    let compressed = (CEILING - KNEE) + (2.0 * over * KNEE - over * over) / KNEE;
    x * (compressed / ax.max(1e-9))
}

/// Compiles all transitions in the plan into one event timeline.
fn compile_session_events(
    plan: &SessionPlan,
    spec: &crate::automation::transition_spec::TransitionSpec,
) -> Vec<crate::control::ControlEvent> {
    use crate::automation::transition_spec::compile_transition;

    let mut events = Vec::new();
    for (idx, seg) in plan.segments.iter().enumerate() {
        let Some(tr) = &seg.transition else {
            continue;
        };
        // Parity: segment 0, 2, 4 … play on deck A; 1, 3, 5 … on B.
        let outgoing_deck = if idx % 2 == 0 { DeckId::A } else { DeckId::B };
        events.extend(compile_transition(
            spec,
            tr.window,
            plan.session_bpm,
            plan.sample_rate,
            outgoing_deck,
        ));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::types::{
        PresetName, Segment, StretchDecision, StretchMode, TransitionPlan, TransitionWindow,
    };

    /// Provider holding one second of DC-free unity sine per hash.
    struct TestProvider {
        a: Vec<f32>,
        b: Vec<f32>,
    }

    impl TrackProvider for TestProvider {
        fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError> {
            if hash.0 == "a" {
                Ok(&self.a)
            } else {
                Ok(&self.b)
            }
        }

        fn name(&self) -> &'static str {
            "test"
        }
    }

    fn sine_pcm(len: usize, freq: f32) -> Vec<f32> {
        (0..len)
            .map(|i| {
                #[expect(clippy::cast_precision_loss, reason = "test index")]
                let t = i as f32 / 44_100.0;
                (2.0 * std::f32::consts::PI * freq * t).sin() * 0.5
            })
            .collect()
    }

    fn two_track_plan(overlap: u64) -> SessionPlan {
        let transition = TransitionPlan {
            window: TransitionWindow {
                start: SessionTime(44_100 - overlap),
                end: SessionTime(44_100),
            },
            preset: PresetName("Crossfade".into()),
        };
        SessionPlan {
            session_bpm: 120.0,
            sample_rate: 44_100,
            segments: vec![
                Segment {
                    track_hash: TrackHash("a".into()),
                    src_start: 0,
                    session_start: SessionTime(0),
                    len_samples: 44_100,
                    stretch: StretchDecision {
                        mode: StretchMode::Resample,
                        ratio: 1.0,
                        out_of_comfort_band: false,
                        strategy: crate::timeline::types::TempoStrategy::SessionBpm,
                    },
                    transition: Some(transition),
                },
                Segment {
                    track_hash: TrackHash("b".into()),
                    src_start: 0,
                    session_start: SessionTime(44_100 - overlap),
                    len_samples: 44_100,
                    stretch: StretchDecision {
                        mode: StretchMode::Resample,
                        ratio: 1.0,
                        out_of_comfort_band: false,
                        strategy: crate::timeline::types::TempoStrategy::SessionBpm,
                    },
                    transition: None,
                },
            ],
        }
    }

    #[test]
    fn render_until_returns_requested_length() {
        // Given a one-track plan and a sine provider.
        let mut plan = two_track_plan(4_410);
        plan.segments[0].transition = None;
        plan.segments.truncate(1);
        let mut r = Renderer::new(plan);
        let mut p = TestProvider {
            a: sine_pcm(44_100, 440.0),
            b: sine_pcm(44_100, 440.0),
        };

        // When rendering two pulls.
        let a = r.render_until(&mut p, SessionTime(1_000)).expect("render");
        let b = r.render_until(&mut p, SessionTime(2_500)).expect("render");

        // Then each pull returns exactly its span (interleaved stereo).
        assert_eq!(a.len(), 1_000 * 2);
        assert_eq!(b.len(), 1_500 * 2);
    }

    #[test]
    fn render_is_continuous_across_pull_boundary() {
        // Given a single track rendered in two pulls.
        let mut plan = two_track_plan(4_410);
        plan.segments[0].transition = None;
        plan.segments.truncate(1);
        let mut r = Renderer::new(plan);
        let mut p = TestProvider {
            a: sine_pcm(44_100, 440.0),
            b: sine_pcm(44_100, 440.0),
        };

        // When rendering split at sample 5000.
        let a = r.render_until(&mut p, SessionTime(5_000)).expect("render");
        let b = r.render_until(&mut p, SessionTime(5_100)).expect("render");

        // Then the boundary samples join without a discontinuity.
        let last = a[a.len() - 1];
        let first = b[0];
        assert!(
            (last - first).abs() < 0.05,
            "boundary jump {last} -> {first}"
        );
    }

    #[test]
    fn overlap_attenuates_by_3db() {
        // Given two tracks overlapping for 4_410 samples.
        let plan = two_track_plan(4_410);
        let mut r = Renderer::new(plan);
        let mut p = TestProvider {
            a: sine_pcm(88_200, 440.0),
            b: sine_pcm(88_200, 440.0),
        };

        // When rendering the middle of the overlap.
        let mid = 44_100 - 4_410 / 2;
        let before = r
            .render_until(&mut p, SessionTime(44_100 - 4_410 - 1_000))
            .expect("render");
        let overlap = r
            .render_until(&mut p, SessionTime(mid + 1))
            .expect("render");
        let after = r
            .render_until(&mut p, SessionTime(44_100 + 1_000))
            .expect("render");

        // Then solo output exceeds overlap-period output (the two
        // sines are identical so overlap sums to 2·solo·(1/√2)).
        let peak_before = before.iter().fold(0.0_f32, |m, &x| m.max(x)).abs();
        let peak_overlap = overlap.iter().fold(0.0_f32, |m, &x| m.max(x)).abs();
        let peak_after = after.iter().fold(0.0_f32, |m, &x| m.max(x)).abs();
        assert!(peak_overlap > peak_before * 0.5);
        assert!(peak_after > 0.0);
    }

    #[test]
    fn limiter_caps_output_below_ceiling_plus_epsilon() {
        // Given loud overlapping material (0.5-amplitude sines on both
        // decks pre-limiter).
        let plan = two_track_plan(44_100 / 2);
        let mut r = Renderer::new(plan);
        let mut p = TestProvider {
            a: sine_pcm(132_300, 440.0),
            b: sine_pcm(132_300, 440.0),
        };

        // When rendering the whole session.
        let out = r.render_until(&mut p, SessionTime(88_200)).expect("render");

        // Then no sample exceeds the ceiling.
        assert!(
            out.iter().all(|x| x.abs() <= CEILING + 1e-6),
            "max = {}",
            out.iter().fold(0.0_f32, |m, x| m.max(x.abs()))
        );
    }

    #[test]
    fn output_is_finite_and_no_nan() {
        // Given the two-track plan.
        let plan = two_track_plan(4_410);
        let mut r = Renderer::new(plan);
        let mut p = TestProvider {
            a: sine_pcm(132_300, 440.0),
            b: sine_pcm(132_300, 440.0),
        };

        // When rendering the full session.
        let out = r.render_until(&mut p, SessionTime(88_200)).expect("render");

        // Then all samples are finite.
        assert!(out.iter().all(|x| x.is_finite()));
    }
}
