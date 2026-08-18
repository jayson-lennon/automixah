# Spec: Mixing Fixes — Stereo, Overlapping Transitions, Data-Driven Automation Pairs

## Problem

Two blockers make the current "mix" not a mix:

1. **Mono (left-channel only) output.** `djcore::decoder` was built for analysis and returns mono PCM (channel 0). The CLI routes that mono into the renderer and writes a 1-channel WAV, so the entire mix chain is left-channel-only by inheritance. Analysis is unaffected (mono is standard for BPM/key detection); mix output is wrong.

2. **No actual mixing.** The planner abuts segments — B's `session_start` is exactly A's end (`cursor = cursor + stretched_len`). The transition window *is* planned inside A's tail and automation curves *are* compiled for it, but during the window deck B (the incoming track) is silent because B's segment does not exist yet in the timeline. Audible result: A plays to its natural end (its fade-out curve alone reads as "ending"), then B starts from its beginning. The renderer is capable of a real mix (two deck chains, per-deck EQ/filter curves, `active_segment_indices()` sums co-active segments, `OVERLAP_GAIN` path, soft-knee limiter) — that path never triggers because nothing overlaps.

Additionally: preset selection is hardcoded (`next_transition` always returns `Crossfade`), and although a fully RON-serializable data-driven automation system exists (`PresetSpec`, `CurveSpec`, `SelectionRule` rule table), it is never loaded from files and never selected by rules. The promised foundation for future MIDI control — authoring automations as data files — is absent.

## Solution

1. **Stereo pipeline.** `DecodeAudio` carries interleaved multi-channel PCM plus a `channels` field; djcore adds a mono-downmix helper used internally by analysis callers; stretcher, renderer DSP, and WAV output process interleaved stereo throughout. Frame-aware stretching (L/R resampled in lockstep — see Gotchas).
2. **Overlapping transitions.** The planner cues B at the window start: `session_start = window.start`, `src_start` = B's grid-snapped phrase cue (0 when the grid is unconfident, with a stderr warning), cursor advances to `window.start + B_stretched_len`. A's outro plays under B's intro; the renderer's existing two-deck machinery becomes active as designed.
3. **`TransitionSpec` automation pairs.** A RON-serializable pair (name, beats = 64, curves addressed by **role** — `Outgoing`/`Incoming` — each `{address, from, to, shape}`). Compilation maps roles to literal decks A/B by segment parity. Built-in default: 16-bar (64-beat) equal-power fade pair. CLI `--automation pair.ron` loads and validates a custom pair applied to every transition; no flag → default pair. Rule-table selection is deferred.
4. **The missing test class.** Assertions that *both decks are audible during the window* (RMS of each deck's contribution > 0 at midpoint; A decreasing, B increasing across the window), stereo channel independence, grid-snap correctness, RON round-trip, role→deck parity mapping.

Musical constants: the engine assumes **4/4, 4 beats/bar** (`BEATS_PER_BAR = 4`). **16 bars = 64 beats ≈ 32 s at 120 BPM.** Long windows are intentional: user's genre (extended trance mixes) has 16-bar phrase structure, and a long window makes beat-grid misalignment *more* obvious — the goal of this iteration.

---

## Dialectical Outcomes (Why)

- **16 bars, not 16 beats** — User confirmed bars. Priority this iteration: "the thing plays continuously with serviceable transitions," true trance phrasing. Rejected 16-beat/4-bar (snappier but wrong phrasing for the genre and less revealing of grid misalignment).
- **One RON pair with role-addressed decks** (user's "A, unless decks don't alternate") — Exploration confirmed `compile_preset` translates deck labels *literally* (deck A = segment 0, 3, 5…; deck B = segment 1, 4, 6…; renderer assigns `deck = idx % 2`). A single pair with hardcoded "A fades out" inverts at every odd transition. Roles (`Outgoing`/`Incoming`) mapped by parity at compile time keep ONE authored pair correct at EVERY transition, forever, and future MIDI control maps naturally (a physical knob targets a role). Rejected: separate Outro/Intro specs merged at load (more types, no capability).
- **`--automation pair.ron` applied to every transition** — "Get it working" scope. No rule table, no directory of pairs. Rejected for now: rule-based selection among loaded pairs (later, "when we get fancy with analysis").
- **Unconfident grid → warn + time-based fallback, never refuse** — Real tracks score low sometimes (click fixtures: 0.12–0.13 confidence); refusing blocks the pipeline and proves nothing. Mix completes; stderr says why alignment is off.
- **Overlap changes session length & A's audible span** — Confirmed by user as intended ("that's what a DJ mix is"). Session = sum of stretched lengths minus one window per transition; A's final bars play under B's first bars.
- **Stereo via interleaved PCM, analysis stays mono** — Mono detection is standard and unaffected; interleaved avoids plane-splitting plumbing through the renderer. Frame-awareness is mandatory in the stretchers (see Gotchas).
- **`TransitionSpec` is new; `PresetSpec`/`compile_preset` stay** — Existing golden tests and built-ins keep working; the renderer switches to compiling from `TransitionSpec` with role mapping. No test breakage in the 500-test suite.

## Relevant Files (Where)

**Modified:**
- `crates/djcore/src/decoder/mod.rs` — `DecodeAudio` stereo; `SymphoniaDecoder` multi-channel; downmix helper.
- `crates/djcore/src/analyzer.rs` — analysis input stays mono; document that callers downmix (helper call sites in CLI).
- `crates/automixah-engine/src/timeline/plan.rs` — overlap cursor advance, cue selection, unconfident-grid fallback, window from the pair's beats (64).
- `crates/automixah-engine/src/timeline/placement.rs` — window placement already end-anchored; verify `place_window`/`fallback_window` with new overlap geometry (window *end* = A's session end unchanged; start = end − 64 beats).
- `crates/automixah-engine/src/render/resample.rs` — frame-aware resampling (stereo lockstep).
- `crates/automixah-engine/src/render/wsola.rs` — frame-aware WSOLA (correlate on mono mixdown or L; emit stereo frames).
- `crates/automixah-engine/src/render/renderer.rs` — `compile_session_events` → compile from `TransitionSpec` with parity mapping; stereo frames through `fill_deck_window`/`process_block`.
- `crates/automixah-engine/src/render/dsp.rs` — verify per-channel biquad state pairs consume interleaved frames.
- `crates/automixah-cli/src/lib.rs` — stereo decode plumbing, mono downmix for analysis, `--automation` flag + RON load/validate, stereo WAV write, cue slice in `SessionPcm`.
- `crates/automixah-cli/tests/pipeline.rs` — update expectations (overlap lengths, stereo), new integration tests.

**Created:**
- `crates/automixah-engine/src/automation/transition_spec.rs` — `TransitionSpec`, `RoleSerde`, RON default pair, validation.
- `crates/automixah-engine/tests/mix_audibility.rs` — the "both decks audible" test class.

## Key Code Context (What)

Current mono decode result (`crates/djcore/src/decoder/mod.rs`):

```rust
pub struct DecodeAudio {
    /// Normalized mono samples in the range [-1.0, 1.0].
    pub samples: Vec<f32>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
}
```

The abutting planner loop (`crates/automixah-engine/src/timeline/plan.rs`) — the core bug:

```rust
segments.push(Segment {
    track_hash: TrackHash(track.hash.0.clone()),
    src_start: 0,                      // ← B never cues past its beginning
    session_start: cursor,             // ← cursor = A's end (abutting)
    len_samples: stretched_len.0,
    stretch,
    transition: transition.map(|(window, preset)| TransitionPlan { window, preset }),
});
cursor = SessionTime(cursor.0 + stretched_len.0);   // ← no overlap
```

Hardcoded preset (`next_transition`, same file):

```rust
Some((window, PresetName(DEFAULT_PRESET.into())))  // always "Crossfade"
```

Literal deck translation in `compile_preset` (`crates/automixah-engine/src/automation/presets.rs`):

```rust
events.push(ControlEvent {
    deck: curve.deck.to_deck(),        // ← DeckSerde::A/B taken literally
    address: curve.address.to_address(),
    value: value.clamp(0.0, 1.0),
    time: SessionTime(window.start.0 + offset.min(window.len_samples())),
});
```

Renderer deck assignment and co-active summing (`crates/automixah-engine/src/render/renderer.rs`):

```rust
let deck = idx % 2;                    // seg0→A, seg1→B, seg2→A…
// ...
fn active_segment_indices(&self, t0: SessionTime, t_end: SessionTime) -> Vec<usize> {
    self.plan.segments.iter().enumerate()
        .filter(|(_, seg)| {
            let end = seg.session_start.0 + seg.len_samples;
            seg.session_start.0 < t_end.0 && t0.0 < end
        }).map(|(i, _)| i).collect()
}
// render_block: decks summed, OVERLAP_GAIN when active.len() > 1, soft_knee limiter
```

Note: `fill_deck_window` indexes `pcm[idx]` with `idx` relative to `seg.session_start` — it **ignores `seg.src_start`**. With overlap, the provider must slice stretched PCM at the stretched cue (CLI `SessionPcm`), since the renderer's window fill starts at the segment's session start.

Existing data-driven curve types (`presets.rs`) that `TransitionSpec` reuses for addresses/shapes:

```rust
pub enum AddressSerde { Gain, EqLow, EqMid, EqHigh, HpfCutoff, LpfCutoff }
pub enum Shape { EqualPower, Linear }
pub struct CurveSpec { pub deck: DeckSerde, pub address: AddressSerde, pub from: f32, pub to: f32, pub shape: Shape }
pub struct PresetSpec { pub name: String, pub beats: usize, pub curves: Vec<CurveSpec> }
```

Window placement (`placement.rs`) — end-anchored; window *end* stays at A's session end:

```rust
let end = a_session_end;
let len = requested.0.max(min_len as u64);       // requested = preset_beats beats
let start = SessionTime(end.0.saturating_sub(len));
TransitionWindow { start, end }
```

CLI stereo/WAV write site (`crates/automixah-cli/src/lib.rs`):

```rust
fn write_wav(path: &Path, samples: &[f32], rate: u32) -> Result<(), hound::Error> {
    let spec = hound::WavSpec { channels: 1, sample_rate: rate, bits_per_sample: 32, sample_format: hound::SampleFormat::Float };
    // ...
}
```

## Implementation Algorithm (How)

### Phase 1 — Stereo pipeline

1. `DecodeAudio { samples: Vec<f32>, sample_rate: u32, channels: u16 }`; `samples` becomes interleaved (L,R,L,R…). `SymphoniaDecoder` iterates *all* channels per frame, writes interleaved, sets `channels`.
2. djcore gains `DecodeAudio::to_mono(&self) -> Vec<f32>` (average channels). Analysis call sites downmix (`analyze(&decoded.to_mono(), …)`). Analyzer internals unchanged.
3. Frame-aware resampling: `Resampler` gains a frame size (2). Either (a) split planes → resample each with the same fractional cursor → reinterleave, or (b) interpolate per-frame treating each frame as a unit. Output length exact per channel. **Never interpolate across the flat interleaved stream** — that smears channels.
4. WSOLA: correlation computed on the mono mixdown (or L plane) for alignment offsets; frames copied as stereo units. Same `stretch_all` contract per channel, exact length.
5. Renderer: `fill_deck_window` copies interleaved frames (indices ×2). `process_block` consumes interleaved; biquad pairs `hpf: [Biquad; 2]`/`lpf: [Biquad; 2]` bind per channel (extend if channels > 2 → clamp to stereo downmix). Gains remain per-deck scalars applied per sample.
6. CLI: `write_wav` channels: 2; total session samples now *frames*×2 — every `SessionTime`/length arithmetic in the engine stays in **frames**; only PCM buffers double.

### Phase 2 — Overlapping transitions

1. In `plan_with`, for each transition i→i+1: window = `place_window(...)` end-anchored at `a_session_end` (unchanged), length = pair beats (64) — the window computed BEFORE advancing the cursor for B.
2. B's `session_start = window.start`; `src_start` = cue: if B's grid is confident, the downbeat nearest `0.25 × B_duration` (a phrase-ish entry point), snapped to an actual downbeat sample; else 0 + collect a warning string surfaced by the CLI on stderr.
3. Cursor advance: after pushing B, `cursor = B.session_start + B_stretched_len` (i.e., advance from the *window start*, not A's end).
4. A's `len_samples` stays full stretched length (A plays out completely; window closes exactly at A's end — consistent with end-anchored placement).
5. `SessionPcm` (CLI) slices stretched PCM from the stretched cue: provider must hand the renderer PCM starting at `stretched(src_start)` so `fill_deck_window`'s zero-based indexing lands on the cue. A's PCM (src_start 0) unchanged.
6. Session total = last segment's `session_start + len_samples`. Replan/nav math (`replan.rs`) verified against overlap lengths.

### Phase 3 — TransitionSpec pairs

1. New `transition_spec.rs`:

```rust
pub enum RoleSerde { Outgoing, Incoming }
pub struct TransitionCurve { pub role: RoleSerde, pub address: AddressSerde, pub from: f32, pub to: f32, pub shape: Shape }
pub struct TransitionSpec { pub name: String, pub beats: usize, pub curves: Vec<TransitionCurve> }
```

2. Built-in default: `TransitionSpec::long_crossfade()` — 64 beats, two curves: Outgoing Gain 1→0 EqualPower, Incoming Gain 0→1 EqualPower.
3. `compile_transition(spec, window, session_bpm, sample_rate, outgoing_deck: DeckId)` — same quarter-beat event stepping as `compile_preset`, but each curve's deck = `outgoing_deck` if `Outgoing` else the opposite. `compile_session_events` passes `DeckId` for `idx % 2`.
4. Planner: every transition gets `PresetName(spec.name.clone())` from the active spec; window beats come from `spec.beats` (planner needs the spec's beats for placement — thread it through `PlanOptions` or a `TransitionSpec` parameter).
5. CLI `--automation <path>`: read file → `ron::from_str::<TransitionSpec>` → validate (name nonempty, beats > 0 and ≤ track-safe clamp, curve values finite, gains within [0,1] endpoints sensible) → errors name the file + reason. No flag → default pair.

### Phase 4 — Tests

Per Test Strategies below; the new integration class asserts audible mixing via per-deck RMS.

## Anti-Goals (Out of Scope)

- Rule-table selection among multiple loaded pairs (deferred; one pair for all transitions).
- Multiple/alternative automation files, directories of pairs, per-transition overrides.
- Seeking, reorder, runtime skip (unchanged forward-only stance).
- Audio-device playback (offline render remains the interface).
- Analysis caching/persistence (still analyze every run).
- Metering/EQ-curve authoring beyond `AddressSerde`'s existing six parameters.
- Channels > 2 (downmix to stereo).

## Edge Cases & Gotchas

- **Interleaved resampling smears channels** if the existing per-sample interpolator runs over the flat stream. Frame-awareness is the single most likely stereo regression — test with a left-only file.
- **`fill_deck_window` ignores `src_start`** — cue slicing must live in the provider, or the renderer's fill must map `src_start` into stretched time. Provider-slicing is the minimal change.
- **Parity inversion**: literal A/B labels invert at odd transitions; only role-mapped compilation is correct. Test at both parities.
- **Window longer than a track** (short fixtures): clamp window to `min(spec beats, bar-min, A_len/2)` — `place_window`'s `min_len` bar clamp exists; add an upper clamp so 8 s click fixtures still produce sane windows (tests use synthetic small windows).
- **Confidence gate**: `grid_is_confident` requires stability ≥ threshold AND populated downbeats; real tracks vary. Fallback must log, not fail.
- **Length accounting**: engine time stays in *frames*; only PCM buffers double for stereo. Mixing frames/samples in the renderer's `BLOCK` loop (currently 64 *samples*) must become frames or the block math halves stereo throughput — pick frames, adjust `BLOCK` semantics once.
- **Existing tests** assume abutting lengths and mono PCM — many `assert`s need overlap-aware/stereo updates; golden curve tests for `PresetSpec` remain valid (untouched path).
- **Session length shrinks** by ~one window per transition vs today — tests asserting `total == sum(len)` must switch to `sum − overlaps`.

## Navigation Anchors

- `plan_with` / `next_transition` — `crates/automixah-engine/src/timeline/plan.rs` (overlap + cue + spec threading).
- `compile_preset` — `crates/automixah-engine/src/automation/presets.rs` (model for `compile_transition`).
- `compile_session_events` — `crates/automixah-engine/src/render/renderer.rs` (parity mapping + stereo blocks).
- `SessionPcm::new` / `write_wav` / `run` — `crates/automixah-cli/src/lib.rs` (stereo plumbing, cue slicing, `--automation`).
- `DecodeAudio` — `crates/djcore/src/decoder/mod.rs` (stereo source of truth).

## Dependency Mappings

- `ron` — already a dependency of `automixah-engine` (rule-table ser/de tests); reuse for `TransitionSpec`.
- `hound`, `clap`, `sha2`, `error-stack`, `wherror` — already in the CLI. No new external crates.

## Test Strategies

- **Phase 1**: djcore — decode a stereo fixture → `channels == 2`, `samples.len() % 2 == 0`, L≠R on an asymmetric file; `to_mono` averages. Engine — resampler/WSOLA on interleaved stereo: left-only in → left-only out (right plane all-zero); exact per-channel lengths. CLI pipeline test: WAV header reports 2 channels.
- **Phase 2**: unit — `plan_with` on synthetic analyses: segment[i+1].session_start == window[i].start; session total == sum − overlaps; cue = grid downbeat when confident (±1 beat), 0 + warning when not. Update existing planner tests for overlap arithmetic.
- **Phase 3**: unit — RON round-trip of a pair; invalid file (bad beats / non-finite / empty name) rejected with reason; `compile_transition` at parity 0 and 1 → Outgoing maps to A/B respectively, Incoming to the opposite; equal-power invariant on the default pair's gain curves. Golden fixture for the 64-beat default.
- **Phase 4** (`mix_audibility.rs`, integration): two synthetic tracks (distinct frequencies) → render → at window midpoint both decks' contributions nonzero (isolate per-deck by rendering with one deck's source zeroed, or assert on binned RMS trend of each source's signature frequency via Goertzel); A's signature RMS decreases, B's increases across binned window thirds; no clipping; continuous output (no gap at window edges). End-to-end: `--automation` alters the output envelope vs default (windowed RMS shape differs).
- **Update**: `crates/automixah-cli/tests/pipeline.rs` — duration = planned total (overlap-aware), stereo readback, real-fixture test unchanged in spirit.
- Full suite + `just lint` + `just fmt-check` green before commit; commit per phase.

## Acceptance Criteria

- WAV is 2-channel; a left-only/right-only test file produces distinguishable channels in the mix output.
- Two-track mix: at window midpoint, both tracks audibly present (RMS of each deck contribution > 0); A's RMS decreases, B's increases monotonically (binned) across the window.
- Session length = sum of tracks minus overlapped windows (not abutting sum).
- With confident grids, B's cue lands on a downbeat in source time (±1 beat); with unconfident grids, stderr warning + time-based fallback, mix still completes.
- `--automation custom.ron` changes the transition curves (observable in output envelope); no flag → default 16-bar pair.
- Curve roles map to the correct physical decks at every transition parity (unit-verified).
- All existing suites stay green (adjusted where overlap changes lengths).

## Phases

1. **Stereo pipeline** — `DecodeAudio` interleaved multi-channel + downmix helper; frame-aware resampler/WSOLA; renderer/stretcher/WAV 2-channel; update affected tests.
2. **Overlapping segments** — planner overlap + grid-snapped cue via `src_start` + unconfident fallback warning; provider cue slicing; session-length accounting; overlap-aware test updates.
3. **TransitionSpec pairs** — role-addressed RON spec; parity mapping in `compile_transition`; default 16-bar equal-power pair; `--automation` load/validate; planner threads the spec (window beats, preset name).
4. **Tests** — `mix_audibility.rs` integration class + stereo independence + fallback + RON round-trip + parity mapping + golden default-pair fixture; CLI pipeline updates.
5. **Verify + record** — full suite, lint, fmt; Record updates below.

## Record Updates (end of implementation)

- **Amend** *"Time-scaling supports pitch-adjusted resampling (default) and pitch-preserving WSOLA; default heuristic: ≤±8% stretch uses pitch-adjusted."* → append: "…; decode, stretch, render, and WAV output are stereo (interleaved); analysis uses a mono downmix."
- **Add**: "Transitions overlap: the incoming track cues at the window start (grid-snapped when confident), and the outgoing track's outro plays under the incoming track's intro; session length reflects the overlap."
- **Add**: "Automations are authored as RON `TransitionSpec` pairs addressed by deck role (outgoing/incoming); the default is a 16-bar equal-power fade, and `--automation <file>` loads a custom pair applied to every transition."
