# Constant-Grid Beatmatching — Implementation Specification

## Problem

The offline auto-DJ produces a proper crossfade, but the beats of the two decks are not aligned. Live diagnostics on the user's two reference OGG tracks (`beat_diag` example) showed the beat *grids* are unusable:

- Track 1: `bpm=139.892 conf=0.149 stability=0.120 beats=761 downbeats=3`, beat intervals from 0.0 s to **96.1 s**.
- Track 2: `bpm=139.735 conf=0.021 stability=0.425 beats=1326 downbeats=19`, doubled beats ~45 ms apart.

Root causes:

1. **The beat tracker is non-functional** (`crates/stratum-dsp/src/features/beat_tracking/hmm.rs`): emission probabilities are evaluated on a uniform frame grid at the nominal BPM; the 5 tempo states are ignored (`let _state_beat_interval = ...` is computed and discarded, ~line 261). Beats are only emitted where a frame has an onset within ~54 ms. With a BPM estimate off by <1%, phase error accumulates ~6 ms/beat until the gate fails and beats vanish (96 s gaps). The Bayesian refinement path then re-tracks segments with inclusive bounds and concatenates them, producing doubled beats and phase discontinuities.
2. **The session planner never aligns phase** (`crates/automixah-engine/src/timeline/plan.rs`, `placement.rs`): the transition window is `[segment_end − preset_beats, segment_end]` computed by length arithmetic only (`place_window` takes anchors but ignores them). The incoming cue (`cue_for`) picks "a downbeat near 25%" with no guarantee the incoming beats coincide with the outgoing deck's beats during the overlap.

The user only cares that fixed-BPM tracks mix properly; sample-level alignment is explicitly not required (WSOLA path retains ±10 ms local jitter by nature).

## Solution

Implement the user's own mental model — *detect beats, mark them, derive BPM, align a constant grid to the marks* — using Mixxx's proven post-processing as the reference:

1. **Mark beats properly**: a dynamic-programming beat tracker (Ellis-style) over an onset novelty envelope, seeded by the existing tempogram BPM. Replaces the fake HMM entirely.
2. **Fit a constant grid** Mixxx-style (`mixxx/src/track/beatutils.cpp`): iron jitter into constant regions → regress BPM over the longest phase-compatible span → snap BPM to a musically plausible value (integer → ½ → ⅓ → 1/12 within regression confidence bounds) → fit a single phase anchor against all beats (mean residual, 25 ms clip) → choose downbeat phase by energy vote on the fitted grid. Output is **one BPM + one anchor**; `beats`/`downbeats`/`bars` arrays become projections of that grid (continuous through breaks).
3. **Align in the engine**: snap transition window boundaries to the outgoing deck's *stretched* grid phase. Because every deck stretches by `grid_bpm/session_bpm`, all decks share one session beat period (`60/session_bpm`), so an incoming cue that is a grid beat lands exactly on the session grid by construction (sample-exact on the resample path, ±10 ms WSOLA jitter out of band).

Mathematical core: a source beat at time `t` lands at session time `t × ratio` where `ratio = grid_bpm/session_bpm`; the stretched beat period is `(60/grid_bpm) × ratio = 60/session_bpm` — identical for every deck. Phase alignment reduces to: (a) window boundaries are session-grid beats, (b) the incoming cue is a source-grid beat.

## Dialectical Outcomes (Why)

- **Scope = grids + session phase (user chose A/A):** fixing grids alone cannot fix the mix — the planner ignores track phase today. Both layers must change.
- **Constant grid, not a variable beat map (user chose 2A):** fixed-BPM tracks only; a constant grid composes *exactly* with stretching (see math above), which a marker-sparse beat map does not. Variable-tempo support is dropped.
- **Replace tracker, keep onsets + tempogram BPM (user chose 3A):** onset detection and the tempogram BPM estimate were verified decent (139.892 vs an expected ~138 on track 1 — well within the tracker's tolerance to fix); the HMM/Bayesian/tempo-variation machinery is the broken part and is deleted rather than preserved behind flags (less dead code; stratum-dsp is in-repo with no external consumers).
- **BPM rounding ladder (user chose 4A):** studio recordings sit on a static metronome at full/half/third/twelfth BPM values. Rounding kills the sub-1% BPM error that currently accumulates into grid destruction, and produces clean stretch ratios.
- **Downbeat by energy vote on the fitted grid (user chose 5A):** trance kicks sit on beat 1; the vote runs on the gapless fitted grid so chain-death cannot break it. Rejected: chain-based `detect_downbeats_with_time_sig` (assumes beat 0 is a downbeat, fragile to gaps) and dropping downbeats entirely (loses phrase alignment recorded in `.agents/RECORD.md`).
- **`BeatGrid` gains explicit `grid_bpm` + `anchor_seconds` (implementer's choice under user's 6):** greenfield with no consumers — explicit fields are the canonical constant-grid contract; arrays remain projections for compatibility with existing consumers.
- **What was *not* chosen:** Beat maps with markers; keeping HMM behind a flag; raw (unrounded) regressed BPM; sample-level alignment guarantees (WSOLA local jitter accepted).

## Relevant Files (Where)

**stratum-dsp (analysis crate):**
- `crates/stratum-dsp/src/features/beat_tracking/mod.rs` — rewrite: new DP tracker entry + constant-grid fit; delete HMM/Bayesian/tempo-variation wiring from the public flow.
- `crates/stratum-dsp/src/features/beat_tracking/dp.rs` — **new**: dynamic-programming beat marker.
- `crates/stratum-dsp/src/features/beat_tracking/grid_fit.rs` — **new**: region ironing, BPM regression + rounding ladder, anchor fit, downbeat vote, array materialization, stability.
- `crates/stratum-dsp/src/features/beat_tracking/hmm.rs`, `bayesian.rs`, `tempo_variation.rs` — **delete**.
- `crates/stratum-dsp/src/features/beat_tracking/time_signature.rs` — keep (4/4 constants); optional dead-code pruning.
- `crates/stratum-dsp/src/features/beat_tracking/mod.rs` re-exports — keep `generate_beat_grid` name/signature stable (lib.rs call site unchanged apart from inputs).
- `crates/stratum-dsp/src/lib.rs` (~lines 920–960, Phase 1C block) — feed the DP tracker a novelty envelope + duration; pass the envelope into `generate_beat_grid`.
- `crates/stratum-dsp/src/analysis/result.rs` (~line 145) — extend `BeatGrid` with `grid_bpm: f32`, `anchor_seconds: f32` (+ `Default`).
- `crates/djcore/src/analyzer.rs` (`BeatGrid` mirror, `From` impl ~line 39) — mirror new fields.

**automixah-engine (planning/rendering):**
- `crates/automixah-engine/src/timeline/placement.rs` — `place_window` actually snaps window boundaries to grid phase.
- `crates/automixah-engine/src/timeline/plan.rs` — clean the duplicated cue/placement block (lines ~90–145; two copies of `cue_for`/`session_start`/`len` logic); pass anchor data through `WindowInputs`.
- `crates/automixah-engine/src/timeline/types.rs` — `BeatGrid` usage; possibly extend `WindowInputs` fields.

**CLI + diagnostics:**
- `crates/djcore/examples/beat_diag.rs` — extend into a grid-quality report (already exists; created during exploration).

**Tests:**
- `crates/stratum-dsp/tests/integration_tests.rs`, `crates/djcore/tests/integration.rs` — adjust expectations (beats now continuous projections).
- `crates/automixah-engine/tests/` — add rendered-mix alignment test (T8); update snapshots (`plan.rs` `full_plan_snapshot_on_mixed_bpm_playlist`, `preset_golden.rs` if geometry shifts).

**Record:**
- `.agents/RECORD.md` — entries applied at end of implementation (see Record Updates).

## Key Code Context (What)

**1. The tracker call site** (`crates/stratum-dsp/src/lib.rs` ~925):

```rust
let (beat_grid, grid_stability) = if bpm > 0.0 && onsets_for_beat_tracking.len() >= 2 {
    let onsets_seconds: Vec<f32> = onsets_for_beat_tracking
        .iter()
        .map(|&sample_idx| sample_idx as f32 / sample_rate as f32)
        .collect();
    use features::beat_tracking::generate_beat_grid;
    match generate_beat_grid(bpm, bpm_confidence, &onsets_seconds, sample_rate) { ... }
```

Note `duration_seconds` is computed from **trimmed** samples (`trimmed_samples.len() as f32 / sample_rate as f32`, ~line 1612) — all beat times are relative to the trimmed buffer, same origin as duration. Keep that invariant.

**2. Novelty envelopes available for the DP tracker** (`crates/stratum-dsp/src/features/period/novelty.rs`): `energy_flux_novelty(&magnitude_spec_frames) -> Vec<f32>` (per-frame, frames → seconds via `hop_size`), plus `combined_novelty`, `spectral_flux_novelty`. The STFT frames (`compute_stft(&trimmed_samples, frame_size, hop_size)`) already exist in scope in `analyze_audio` (~line 174).

**3. `BeatGrid`** (`crates/stratum-dsp/src/analysis/result.rs` ~145):

```rust
pub struct BeatGrid {
    pub downbeats: Vec<f32>,
    pub beats: Vec<f32>,
    pub bars: Vec<f32>,
}
```

becomes (canonical constant-grid contract; arrays are projections):

```rust
pub struct BeatGrid {
    pub grid_bpm: f32,        // rounded, canonical constant tempo
    pub anchor_seconds: f32,  // phase anchor: a downbeat time in [0, bar)
    pub downbeats: Vec<f32>,
    pub beats: Vec<f32>,
    pub bars: Vec<f32>,
}
```

Mirrored in `crates/djcore/src/analyzer.rs` (`pub struct BeatGrid` + `From<stratum_dsp::BeatGrid>`).

**4. The placement function to make real** (`crates/automixah-engine/src/timeline/placement.rs`):

```rust
pub fn place_window(
    _a_anchor: Option<&GridAnchors>,
    _b_anchor: Option<&GridAnchors>,
    inputs: WindowInputs,
) -> TransitionWindow {
    ...
    let end = a_session_end;
    let requested = SessionTime::from_seconds(window_len, sample_rate);
    let max_len = (end.0 / 2).max(min_len as u64);
    let len = requested.0.clamp(min_len as u64, max_len);
    let start = SessionTime(end.0.saturating_sub(len));
    TransitionWindow { start, end }
}
```

`WindowInputs` currently carries `preset_beats`, `a_session_end`, `b_cue_session` (unused), `session_bpm`, `sample_rate`. It needs A's session-grid phase (see Algorithm).

**5. The duplicated block in `plan_with`** (`crates/automixah-engine/src/timeline/plan.rs` lines ~90–145): `cue_for`/`src_start`/`session_start`/`len_samples` computed twice; the dead first copy must go. Keep the live second copy.

**6. Mixxx reference algorithms** (in `/mnt/zed/repos/third-party/mixxx/src/track/beatutils.cpp`):
- `retrieveConstRegions`: two-pointer scan ironing ±12 ms detector jitter; constants `kMaxSecsPhaseError = 0.025`, `kMaxSecsPhaseErrorSum = 0.1`, `kMaxOutliersCount = 1`, `kMinRegionBeatCount = 16`.
- `makeConstBpm`: longest region → extend across phase-compatible start/end regions → `roundBpmWithinRange` (trySnap ladder: fraction 1.0, then 2.0 if center < 85, then 2/3 if center > 127, then 3.0, then 12.0; snap = `round(center × fraction)/fraction`, accepted only strictly inside `[minBpm, maxBpm]`) → `firstBeat = fmod(regionStart, beatLength)`.
- `adjustPhase`: `offset = fmod(beat − startOffset, beatLength)` (wrapped to ±half), mean of residuals within `kMaxSecsPhaseError`, added to firstBeat.
- Grid query model (`beats.h`): 0..N beat markers + final tempo marker; constant tempo = marker at first downbeat only; beats computed as `position + n × beatLengthFrames`.

**7. Stretch/station math already in place:** `decide_stretch(track_bpm, target_bpm, ...)` (`timeline/stretch.rs`) uses `fold_bpm(track_bpm)/target_bpm`; it must switch to `grid_bpm` (already folded by construction — `fold_bpm` is idempotent for in-range values). `SessionPcm::new` in the CLI stretches whole tracks and slices the stretched cue (`cue_frames = round(ratio × src_start)`).

## Implementation Algorithm (How)

### Phase 1 — Beat marking (DP tracker)

New module `features/beat_tracking/dp.rs`:

```
pub struct DpBeatTracker { ... }
impl DpBeatTracker {
    pub fn new(novelty: &[f32], hop_size: u32, sample_rate: u32, bpm_seed: f32) -> Self
    pub fn track(&self) -> Vec<f32>   // beat times in seconds, sorted
}
```

Algorithm (Ellis 2007 "Beat Tracking by Dynamic Programming", as also used by librosa):
1. **Envelope**: use the per-frame novelty already computed in `analyze_audio` (`energy_flux_novelty` or `combined_novelty` over the existing STFT frames). Convert to seconds via `hop_size/sample_rate`.
2. **Tempo prior**: log-Gaussian centered on `60/bpm_seed` (period in seconds), σ ≈ 0.9 octaves equivalent — wide enough to tolerate the observed ~1 BPM seed error.
3. **Backtrack**: `cscore[t] = env[t] + max_{τ ∈ [τmin, τmax]}( cweight(τ) × cscore[t − τ] )` where `cweight = −(log(τ/τ0))²` shaped by the prior; store argmax predecessors. `t` iterates novelty frames.
4. **Path recovery**: start at the frame with max cumulative score; follow predecessors to the beginning. Every path node is a **beat mark**.
5. Edge cases: empty/short envelope (< 8 beats of frames) → return empty vec (caller falls back, grid becomes unconfident). Handle leading silence: the DP naturally starts at the first strong onset.

Output: dense, ordered beat marks (typically every ~0.43 s at 138 BPM, continuous through breaks because the DP path continues on the prior).

### Phase 2 — Constant-grid fit

New module `features/beat_tracking/grid_fit.rs`:

```
pub fn fit_constant_grid(
    beats: &[f32],            // DP marks, seconds
    bpm_seed: f32,
    novelty: &[f32],          // for downbeat energy vote
    hop_size: u32,
    sample_rate: u32,
    duration: f32,
) -> Result<(BeatGrid, f32), AnalysisError>  // grid + stability
```

Steps (all f32 seconds internally, like the rest of the crate):
1. **Guard**: fewer than `kMinRegionBeatCount` (16) marks → `Err` (caller produces unconfident/empty grid).
2. **Iron regions** (port of `retrieveConstRegions`): two-pointer scan; mean beat length from `[left, right]`; walk beats checking phase error vs the projected ironed beat; break region on |error| > 25 ms, error-sum > 100 ms, or > 1 outlier; accept region when the scan completes with border-length sanity (`|first + last − 2·mean| < 12.5 ms`); then `left = right`, `right = last`; repeat. Final zero-length sentinel region marks the end.
3. **Longest span + extension** (port of `makeConstBpm`): find the longest region; compute beat-length min/max bounds widened by `25 ms / numberOfBeats`; scan earlier regions for tempo- and phase-compatible extension (integer beat count unambiguous under both bounds), then later regions; recompute the span.
4. **Rounding ladder** (port of `roundBpmWithinRange`): derive `minBpm/maxBpm/centerBpm` from the span's beat-length bounds; try snap fractions in order 1.0 → (2.0 if center < 85) → (2/3 if center > 127) → 3.0 → 12.0; accept the first snap strictly inside `(minBpm, maxBpm)`; else keep `centerBpm`. This is `grid_bpm`.
5. **Anchor fit**: `beatLength = 60/grid_bpm`; `firstBeat = fmod(spanStart, beatLength)`; then **adjustPhase**: residual `r_i = wrapToHalfBeat(marks_i − firstBeat mod beatLength)`; mean over residuals with |r| ≤ 25 ms; `anchor0 = firstBeat + mean_r`.
6. **Downbeat vote**: for each of 4 bar phases `φ ∈ {0,1,2,3}`: score = Σ over bars of local RMS/onset strength in a ±30 ms window at `anchor0 + φ·beatLength + k·4·beatLength` (sample from the novelty envelope). Pick argmax → `anchor_seconds = anchor0 + φ·beatLength` (reduced mod bar into `[0, bar)`). 4/4 assumed (`BEATS_PER_BAR = 4`).
7. **Materialize projections**: `beats[k] = anchor_seconds + k·beatLength` for `0 ≤ k` while `< duration`; `downbeats = bars = anchor + k·4·beatLength`. Note beats are **gapless by construction** — T4's break test holds because marks stop at `duration`, not at the last onset.
8. **Stability**: `1/(1 + CV)` where CV = std/mean of *residuals* `r_i` (phase consistency of marks against the fitted grid) — replaces the old inter-mark-interval CV. Clamp to [0,1].
9. Fill `BeatGrid { grid_bpm, anchor_seconds, beats, downbeats, bars }`.

Rewired `generate_beat_grid` (same file `mod.rs`, same name/signature contract at the lib.rs call site — extend params to take `novelty`, `hop_size`, `duration`): run DP → fit → return. BPM used downstream (AnalyzerOutput.bpm) should be **replaced by `grid_bpm`** so the planner, stretch decisions, and reporting all see the canonical constant tempo. `bpm_confidence` keeps its tempogram value (still meaningful as detection confidence).

Delete `hmm.rs`, `bayesian.rs`, `tempo_variation.rs` and their wiring; prune now-dead re-exports. `time_signature.rs` stays for 4/4 constants.

### Phase 3 — Session phase alignment (engine)

1. **Compute A's session-grid phase** in `plan_with`: A's segment starts at `session_start` mapping to source time `src_start/rate`; A's session-grid beat times are `session_start + (a·beatLength_a + anchor_a − src_start_seconds) × ratio_a + n × session_beat` for integer `n` (a·beatLength_a + anchor_a = A's first grid beat at/after the source cue; `session_beat = 60/session_bpm`). In practice: `phase = session_start + ((first_beat_after_cue − cue_seconds) × ratio_a) mod session_beat`.
2. **Extend `WindowInputs`**: add `a_grid_phase: SessionTime` (A's session-grid beat phase as above) — or compute inside `place_window` from values already passed; prefer explicit field.
3. **Snap the window** (`place_window`): compute `start`/`end` as today (length arithmetic, clamps), then move **both** boundaries to the nearest A session-grid beat: `start' = start + ((phase − start) mod session_beat)` style snap (nearest, not ceiling), `end'` likewise; keep `end' ≤ a_session_end` (clamp inward) and `len ≥ min_len` (one bar). If no confident grid (`grid_is_confident` false), skip snapping (current behavior preserved — fallback path per RECORD).
4. **B's cue stays grid-derived**: `cue_for` already picks a downbeat; with the constant grid it is exact. Verify (debug assert/log) that `(b_cue_stretched − window.start) mod session_beat` is within ~2 ms on the resample path; log a warning when WSOLA jitter exceeds 10 ms.
5. **Clean the duplicated block** in `plan_with` (delete the dead first copy of cue/session_start/len).
6. `decide_stretch` consumers switch from `track.bpm` to `track.beat_grid.grid_bpm` (fold-idempotent). Keep `bpm` field for reporting.

### Phase 4 — Verification tooling + tests

- Extend `beat_diag` example: print `grid_bpm`, `anchor`, max interval, residual CV, downbeat count/bar, and a PASS/FAIL grid-quality verdict.
- New engine test: render two synthetic click tracks at 138/136 BPM through the full plan→render path; cross-correlate the overlap region; assert peak offset ≤ 10 ms (≤ 2 ms expected on resample path).
- Update existing snapshots where window geometry legitimately shifts (`full_plan_snapshot_on_mixed_bpm_playlist` string, `preset_golden.rs`).
- Re-run the user's exact command on the two reference OGGs; confirm by ear/report.

## Anti-Goals (Out of Scope)

- **Sample-level alignment guarantees** — WSOLA's ±10 ms local nonlinearity remains on the >±8% path.
- **Variable-tempo tracks** — beat maps, tempo drift handling, Bayesian refinement: deleted, not preserved.
- **Changing the ±8% resample/WSOLA comfort-band heuristic** or the resampler/WSOLA implementations themselves.
- **Key detection, onset detectors, tempogram BPM machinery** — untouched (only their consumer changes).
- **Seeking/playback features, library/caching** — per RECORD, out of scope generally.
- **BPM *detection* improvements** beyond consuming its estimate as a seed — the ladder + regression fixes grid quality; detection stays as-is.

## Edge Cases & Gotchas

- **Beat-time origin**: all times are relative to the *trimmed* buffer (`duration_seconds` uses `trimmed_samples.len()`). The DP novelty and the grid must use the same origin — they do if both consume the trimmed samples/STFT.
- **f32 precision**: beat times span ~6 minutes (363 s); f32 has ~1e-5 s precision there — fine. Do anchor/phase math in f64 internally where cheap, store f32 (crate convention allows f32 with `cast_precision_loss` expects).
- **Silence trimming interplay**: trimming shifts audio start; if enabled, `anchor_seconds` is in trimmed time. Duration is trimmed time. Consumers (`cue_for`, `src_stretched`) already operate on the same numbers, so consistency holds — but the *CLI stretch slice* uses `decoded.samples` (untrimmed PCM!) with `src_start` derived from trimmed-time analysis. If trimming is enabled by config there is a latent offset bug; default `AnalysisConfig` has trimming **off** (verified: `enable_silence_trimming` default false), so document this and leave as-is.
- **The `to_samples` hop quantization** in onset consensus multiplies frame indices by `hop_size` — onsets are hop-quantized (~5.8 ms at 1024 hop). The DP tracker working on the novelty envelope is *not* limited to that quantization (per-frame resolution), which is exactly why the DP should consume the envelope rather than the consensus onset list.
- **Doubled marks / near-zero intervals**: the old pipeline produced 0.0 s intervals; the DP path cannot (predecessor τ ≥ τmin), and `fit_constant_grid` should defensively drop marks closer than `0.5 × beatLength` to their predecessor before ironing.
- **Rounding ladder rejection**: if the regression bounds are too wide (few beats), the ladder may snap to a wrong integer; the 16-mark guard plus region-based bounds (25 ms / N) keeps bounds tight enough — this is exactly why Mixxx extends the span across compatible regions *before* rounding.
- **Window snap clamping**: snapping must never push `end` past A's stretched audio end or `start` before session zero; snap inward (toward the window interior) when the nearest beat is outside.
- **B cue = 0 fallback**: when B's grid is unconfident the cue is 0 — snapping still applies to the window (A side only), preserving the RECORD's "session still plays" fallback.
- **`fold_bpm` idempotence**: `grid_bpm` produced by the ladder is already in a sane range but may be < 90 for slow tempo classes; `decide_stretch`'s `fold_bpm` handles it — do not double-fold in the grid itself.
- **Clippy**: stratum-dsp sets `clippy::all/pedantic = allow` (its own lint block) but the workspace treats warnings as errors for `automixah-engine`/`djcore` — keep new engine/djcore code pedantic-clean (doc comments on public items, `#[must_use]`, `#[expect]` casts).

## Navigation Anchors

- `stratum_dsp::analyze_audio` — `crates/stratum-dsp/src/lib.rs:88` (Phase 1C block ~925 is the integration point).
- `features::beat_tracking::generate_beat_grid` — `crates/stratum-dsp/src/features/beat_tracking/mod.rs:108` (rewrite target; keep signature contract).
- `BeatGrid` — `crates/stratum-dsp/src/analysis/result.rs:145`.
- `plan_with` — `crates/automixah-engine/src/timeline/plan.rs:66` (duplicate block ~90–145; cue/transition wiring).
- `place_window` / `WindowInputs` — `crates/automixah-engine/src/timeline/placement.rs` (grid phase snap).
- `decide_stretch` — `crates/automixah-engine/src/timeline/stretch.rs:33`.
- Mixxx reference — `/mnt/zed/repos/third-party/mixxx/src/track/beatutils.cpp` (`retrieveConstRegions`:51, `makeConstBpm`:140, `roundBpmWithinRange`, `adjustPhase`:403) and `beatfactory.cpp` (`makePreferredBeats`).

## Dependency Mappings

- **No new external crates.** Everything (DP tracker, grid fit) is plain Rust over existing data structures; novelty envelopes already exist in-repo.
- Internal: `stratum-dsp::features::period::novelty` (envelope source), `stratum-dsp::features::beat_tracking::time_signature` (4/4 constants), `djcore::analyzer` mirrors the extended `BeatGrid`, `automixah-engine::timeline::{plan, placement, stretch, types}` consume it.
- Dev-only: `djcore` example `beat_diag` (already present); `hound` for WAV test fixtures (already a dev-dependency).

## Test Strategies

| # | Description | Type | Pass criterion |
|---|---|---|---|
| T1 | Synthetic 138 BPM click track → DP + fit | Unit (stratum-dsp) | `grid_bpm = 138.0`; anchor within 10 ms of click phase |
| T2 | Clicks with ±10 ms jitter + dropped beats | Unit | Grid recovers 138.0; anchor within 15 ms; stability > 0.8 |
| T3 | Rounding ladder: regression values near 138.4 / 85.3 / 100.7 | Unit | Snaps to 138.0 / 85.5 / 100⅔ (within computed bounds) |
| T4 | Mid-track break/silence (4 bars) | Unit | Beats continue through break (projection), no gap |
| T5 | Accent every 4th click | Unit | Downbeats land on accented phase |
| T6 | The two reference OGG tracks | Integration (djcore example/tests) | Rounded BPM, continuous beats (no interval > 1.5× median), ~1 downbeat/bar, stability high |
| T7 | Two synthetic tracks (138 & 136) planned | Unit (engine) | Window start/end ≡ A's session-grid beat (mod session beat); B cue lands on grid ≤ 2 ms |
| T8 | Rendered overlap of two click trains | Integration (engine) | Cross-correlation peak offset ≤ 10 ms (≤ 2 ms resample path) |
| T9 | Existing plan/window/preset suites | Unit | Pass; snapshots updated where geometry legitimately changed |
| T10 | Unconfident-grid fallback (empty/short grid) | Integration | Mix still renders via fallback; no panic |

Phase-anchored execution: T1–T5 land with Phase 2 (grid fit); T6 gates Phase 2 exit; T7 with Phase 3; T8–T10 with Phase 4. `just test` / `just lint` must be green throughout.

## Phases

1. **Beat marking** — `dp.rs`: DP tracker over novelty envelope, tempo prior from seed BPM; unit-tested on click fixtures (marks dense and continuous).
2. **Constant-grid fit** — `grid_fit.rs`: ironing, regression, rounding ladder, anchor + downbeat vote, projections, stability; rewire `generate_beat_grid` + `analyze_audio` (BPM output = `grid_bpm`); extend `BeatGrid` (stratum-dsp + djcore mirror); delete HMM/Bayesian/tempo-variation. Exit gate: T1–T6.
3. **Session phase alignment** — `place_window` grid-phase snap, `WindowInputs` extension, duplicate-block cleanup in `plan_with`, `decide_stretch` on `grid_bpm`, cue verification logging. Exit gate: T7, T9.
4. **Verification** — `beat_diag` quality report, T8 rendered-alignment test, snapshot updates, full `just test`/`just lint`, re-run the user's exact command on the reference tracks.

## Acceptance Criteria

- Both reference tracks analyze to a single constant grid: rounded BPM (e.g. 138.0), continuous beats (no interval > 1.5× median), one downbeat per bar, high stability.
- The user's exact command produces a mix where deck beats coincide within ≤ 10 ms across the 64-beat overlap (typically sample-adjacent on the resample path).
- Zero-config/fallback behavior unchanged for unconfident grids; all existing suites pass (snapshots updated where window geometry legitimately changes).

## Record Updates

Applied at the **end of implementation** (not at plan approval):

- **Amend** the existing transitions entry to: *"Transitions overlap: the incoming track cues at a grid downbeat at the window start, the window is phase-snapped to the outgoing track's stretched beat grid so both decks' beats coincide during the overlap, and the outgoing track's outro plays under the incoming track's intro; session length reflects the overlap."*
- **Add**: *"Beat grids are constant-tempo: one rounded BPM and one phase anchor per track; beats/downbeats/bars arrays are projections of that grid, with the downbeat phase chosen by energy. Fixed-BPM tracks are the only supported input."*
- **Amend** the analysis entry (currently "…extended to surface full beat grids") to: *"Analysis lives in the shared `djcore` crate (extracted from harmonic-playlist); djcore wraps stratum-dsp, whose beat grids are constant-tempo (one rounded BPM + one phase anchor; arrays are projections). Analysis uses a mono downmix."*
