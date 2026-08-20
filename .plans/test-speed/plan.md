# Test Suite Speed: 110s → ≤10s wall

## Problem

`just test` takes ~110s wall (nextest-measured; full log saved at `/tmp/nextest.out` during planning). Three causes:

1. **Oversized synthetic render tests.** Several engine integration tests render multi-minute synthetic sessions in debug builds (≈20–27× slower than realtime): the soak renders 3,000s of audio (20 × 150s tracks), `mix_audibility` renders two 240s tracks (one test renders twice), `ac1_boundaries` renders 3 × 120s, `rendered_alignment` 2 × 180s, `stereo_channel_independence` 2 × 60 bars. Tests use long tracks because the default transition window is 64 beats (~31s at 124 BPM).
2. **Real-audio tests that cannot shrink.** CLI pipeline / djcore / stratum-dsp integration tests decode + analyze real click WAV fixtures (4 bars ≈ 8s — already minimal viable input); their cost is the debug-mode DSP constant factor.
3. **Sequential runner.** `just test` runs `cargo test`, which executes test binaries one at a time — a multi-second floor independent of test speed.

Measured baseline (slowest first):

| Test | Time | Disposition |
|---|---|---|
| `engine::ac5_soak ac5_twenty_track_session_renders_gap_free` | 109.9s | shrink + restructure (4 tracks) |
| `engine::mix_audibility custom_pair_changes_the_output_envelope` | 36.6s | shrink |
| `engine::mix_audibility crossfade_is_audible_on_both_decks…` | 21.7s | shrink |
| `cli::pipeline target_bpm_override_changes_plan` | 18.6s | `#[ignore]` |
| `engine::ac1_boundaries ac1_rendered_mix_reaches_every_planned_boundary` | 14.6s | shrink |
| `engine::rendered_alignment overlap_clicks_coincide_within_tolerance` | 9.4s | shrink |
| `cli::pipeline two_track_render_has_planned_duration…` | 9.4s | `#[ignore]` |
| `cli::pipeline forty_eight_k_first_track_writes_forty_eight_k_wav` | 9.2s | `#[ignore]` |
| `engine::stereo_channel_independence left_only_and_right_only…` | 9.1s | shrink |
| `djcore::integration click_track_analysis_detects_120bpm…` | 3.7s | `#[ignore]` |
| `stratum-dsp::integration_tests test_analyze_120bpm_kick` | 3.6s | `#[ignore]` |
| `stratum-dsp::integration_tests test_analyze_128bpm_kick` | 3.3s | `#[ignore]` |
| `engine::stereo_stretch wsola_frames_keeps_left_only_left` | 2.6s | leave (watch item) |
| `stratum-dsp::integration_tests test_silence_detection_and_trimming` | 2.3s | `#[ignore]` |
| `stratum-dsp::integration_tests test_analyze_cmajor_scale` | 1.9s | `#[ignore]` |
| everything else | < 1.8s | leave |

## Solution

Shrink synthetic render tests to a few seconds of audio each by shortening the transition window (4 beats) via the existing `PlanOptions.transition_beats` / `TransitionSpec.beats` — **product code untouched**. Reduce the soak from 20 to 4 tracks. `#[ignore]` the unreducible real-audio tests behind a `just test-heavy` recipe. Switch `just test` to `cargo nextest run` (parallel, per-test timing) plus a doc-test step. Budget: **≤ 10s wall** for `just test` (user-relaxed from 2s), warm build.

## Acceptance Criteria

1. `just test` completes in ≤ 10s wall on this machine (warm build), all tests passing.
2. `just test-heavy` runs every `#[ignore]`d test, all passing.
3. No tests deleted; every shrunk test keeps its original assertions (durations and window lengths change; asserts do not).
4. The ignored set is exactly the real-audio tests — no synthetic test silently ignored, no fast test dropped.

## Dialectical Outcomes (Why)

- **Shrink vs optimize profiles.** Proposed `[profile.test]/[profile.dev] opt-level = 1`; **user rejected**: "running tests in release mode just shifts the wall time from running the test to compiling the test." No profile changes.
- **20 → 4 tracks in the soak.** User: "20 is just wasting CPU." The structural claim (gap-free render across many chained transitions) survives with 4 tracks / 3 transitions.
- **Real-audio: ignore, don't shrink.** Fixtures are already minimal (8s click WAVs are the floor for BPM/grid detection); cost is decode+analyze DSP in debug. AGENTS §4 explicitly blesses real-audio integration tests as `#[ignore]`-style heavy tests. User: "we should have another command to run these slow tests" → `just test-heavy`.
- **nextest vs cargo test.** `cargo test` runs binaries sequentially (multi-second floor). User already uses nextest for its timing output → switch the recipe.
- **Budget 10s** (user set; 2s deemed too strict). Math: debug render ≈ 20–27× slower than realtime, so each render test should render ≲ 30s of audio to stay ~1–1.5s; nextest parallelism makes wall time ≈ slowest test + startup.
- **`ac5_soak.rs` → `soak.rs`.** "AC5" is Acceptance Criterion #5 of `.plans/auto-dj-music-player/plan.md` (the removed wasm/web plan). User didn't recognize it; grep shows no external references to the AC numbering or to a `soak.rs` model (the header's "existing 3× slow-render model (soak.rs)" is stale — that file no longer exists). Rename approved. `ac1_boundaries.rs` keeps its name (out of approved scope).
- **"Sixty minute session" test is not slow.** `ac5_scheduler_simulation_covers_sixty_minute_session` is pure arithmetic (a watermark loop over 3,100s × 0.25s ticks) — instant. Only its sibling full-render test was the 110s offender. The simulation stays as-is.

## Relevant Files (Where)

| File | Change |
|---|---|
| `crates/automixah-engine/tests/ac5_soak.rs` | **git mv** → `crates/automixah-engine/tests/soak.rs`; restructure to 4 tracks, short window, rename tests, fix docs |
| `crates/automixah-engine/tests/mix_audibility.rs` | durations 240s → 12s; 4-beat plan + specs |
| `crates/automixah-engine/tests/ac1_boundaries.rs` | render test only: durations 120s → 12s; 4-beat plan |
| `crates/automixah-engine/tests/rendered_alignment.rs` | durations 180s → 12s; 4-beat plan |
| `crates/automixah-engine/tests/stereo_channel_independence.rs` | bars 60 → 8; 4-beat plan if it plans with defaults |
| `crates/automixah-cli/tests/pipeline.rs` | `#[ignore]` on 4 named tests |
| `crates/djcore/tests/integration.rs` | `#[ignore]` on `click_track_analysis_detects_120bpm_and_populates_grid` |
| `crates/stratum-dsp/tests/integration_tests.rs` | `#[ignore]` on 4 named tests |
| `justfile` | `test` → nextest + doctests; new `test-heavy` recipe |
| `AGENTS.md` | §7 tooling table: update `test` row, add `test-heavy` row |
| `.agents/RECORD.md` | Record Updates entry at end of implementation (see Test Strategies) |

## Key Code Context (What)

Window length is already test-controllable — this is the core lever, no product changes needed:

```rust
// crates/automixah-engine/src/timeline/plan.rs
pub struct PlanOptions {
    pub target_bpm: Option<f32>,
    pub force_drift_back: bool,
    pub transition_beats: usize,   // default: DEFAULT_PRESET_BEATS
    pub transition_name: String,
}
const DEFAULT_PRESET_BEATS: usize = 64;  // 16 bars — why tests grew long tracks

pub fn plan_session(tracks: &[TrackAnalysis], user_bpm_override: Option<f32>) -> SessionPlan
pub fn plan_with(tracks: &[TrackAnalysis], options: PlanOptions) -> SessionPlan
```

Window geometry (placement floored at session start — the reason window and duration must shrink *together*):

```rust
// crates/automixah-engine/src/timeline/placement.rs (~line 163)
// The window is `[end - preset_beats, end]`, floored at the session start.
```

Render entry points as used by the tests:

```rust
// crates/automixah-engine/src/render/renderer.rs
Renderer::new(plan.clone())
Renderer::with_transition(plan.clone(), long_crossfade())  // TransitionSpec drives automation
renderer.render_until(&mut provider, SessionTime(total)).expect("render")

pub trait TrackProvider {
    fn name(&self) -> &'static str;
    fn stretched_pcm(&mut self, hash: &TrackHash) -> Result<&[f32], TrackFetchError>;
}
```

Shortening a spec (existing pattern from `custom_pair_changes_the_output_envelope`):

```rust
let mut snappy = long_crossfade();
snappy.beats = 16;
for c in &mut snappy.curves { c.shape = Shape::Linear; }
```

Plan override pattern to use everywhere a test currently calls `plan_session`:

```rust
let plan = plan_with(&tracks, PlanOptions {
    transition_beats: 4,
    ..PlanOptions::default()          // preserves target_bpm: None / zero-config
});
// or with a session BPM: plan_with(&tracks, PlanOptions {
//     target_bpm: Some(120.0), transition_beats: 4, ..Default::default() })
```

Cast-lint pattern required in test synth code (clippy runs warnings-as-errors):

```rust
#[expect(clippy::cast_precision_loss, reason = "test index")]
let t = i as f32 / 44_100.0;
```

## Implementation Algorithm (How)

### Phase 1 — Shrink render tests (all assertions unchanged)

> **Divergence (annotated during implementation):** `rendered_alignment` required two deviations from the flat "durations → 12s" instruction. (1) Durations must be **bar-aligned**, not a flat 12s: 12s at 138 BPM is 27.6 beats, the window anchors at the track end, and an off-grid end pushes the phase-snapped window start off the beat grid — the decks then start 0.6 beat apart and the click-spread assertion (10 ms) fails legitimately. Fixed with 8-bar durations (`8 * 4 * 60 / bpm`). (2) The click-density floor `phases.len() >= 40` encoded the old 64-beat window's click budget; at 4 beats the window physically contains at most one click per beat, so the floor was scaled to `>= 4` (same purpose: prevent the spread assertion passing vacuously; 5 clicks observed, spread 3 samples).

**`ac5_soak.rs` → `soak.rs`**
1. `git mv crates/automixah-engine/tests/ac5_soak.rs crates/automixah-engine/tests/soak.rs`.
2. Rewrite module docs: drop AC5/20-track/60-minute references and the stale "existing 3× slow-render model (soak.rs)" line. New docs: engine soak (fully render a small multi-track session; gap-free proves the pipeline never structurally starves) + scheduler simulation (pure arithmetic).
3. `twenty_tracks()` → `four_tracks()`: 4 tracks, 8s each (same formula `bpm = 120 + (i % 5) * 2` over `0..4` → 120/122/124/126). If the plan drops a transition (one-bar anchor margin crowds the 4-beat window), bump durations to 12s — assertions demand exactly 4 segments / 3 transitions.
4. `plan_session(&tracks, None)` → `plan_with(&tracks, PlanOptions { transition_beats: 4, ..Default::default() })` (stays zero-config).
5. Rename `ac5_twenty_track_session_renders_gap_free` → `four_track_session_renders_gap_free`; update assertions 20→4 segments, 19→3 transitions; update inline comments (~45 min → ~30 s of audio).
6. Scheduler simulation: rename to drop the `ac5_` prefix; body unchanged (already instant).

**`mix_audibility.rs`** (both tests)
1. `synth("a", 120.0, 220.0, 240.0)` / `synth("b", 121.0, 330.0, 240.0)` → durations `12.0`.
2. `plan_with(..., PlanOptions { transition_beats: 4, ..Default::default() })`.
3. Both render specs: `long_crossfade()` with `.beats = 4`. In `custom_pair…`, keep the shape contrast (default equal-power vs snappy `Shape::Linear`), both at 4 beats — the RMS-at-midpoint assertion (`> 1e-4` difference) is satisfied by shape alone (equal-power mid ≈ 0.707 vs linear 0.5).
4. Keep the cue-slicing step (it reads `plan.segments[1]`, so it follows the new plan automatically). Assertions unchanged.

**`ac1_boundaries.rs`** — only `ac1_rendered_mix_reaches_every_planned_boundary`: durations `120.0 → 12.0`; `plan_session(&tracks, Some(120.0))` → `plan_with` with `target_bpm: Some(120.0), transition_beats: 4`. The planning-only sibling test is untouched (already instant).

**`rendered_alignment.rs`** — durations `180.0 → 12.0`; `plan_session(&tracks, Some(138.0))` → `plan_with` with `target_bpm: Some(138.0), transition_beats: 4`. `ClickPcm` derives lengths from the plan, so it follows.

**`stereo_channel_independence.rs`** — `bars = 60 → 8`; if the test plans with the default 64-beat window (16 bars), switch to `plan_with` with `transition_beats: 4` so the window fits without flooring (8 bars = 16s at 120 BPM; a 64-beat window would floor at session start and degenerate the overlap).

### Phase 2 — `#[ignore]` the real-audio tests

Use `#[ignore = "real-audio (decode+analyze in debug); run via just test-heavy"]`.

- `crates/automixah-cli/tests/pipeline.rs`: `two_track_render_has_planned_duration_no_gaps_no_clip`, `forty_eight_k_first_track_writes_forty_eight_k_wav`, `target_bpm_override_changes_plan`, `real_fixture_end_to_end_when_present`. **Do not** ignore `missing_file_fails_loudly` (fails before decode) or `driftback_strategy_changes_plan` (`plan_only` with synthetic analyses — fast).
- `crates/djcore/tests/integration.rs`: `click_track_analysis_detects_120bpm_and_populates_grid`.
- `crates/stratum-dsp/tests/integration_tests.rs`: `test_analyze_120bpm_kick`, `test_analyze_128bpm_kick`, `test_silence_detection_and_trimming`, `test_analyze_cmajor_scale`.
- Audit a fresh nextest run (`cargo nextest run --workspace > /tmp/nextest-after.out 2>&1`, then grep timings); if any other *real-audio* test pushes the suite over budget, ignore it too. Synthetic tests must be shrunk, never ignored.

### Phase 3 — Runner switch

Read the `justfile` first; preserve recipe style. Then:

```just
test:
    cargo nextest run --workspace
    cargo test --workspace --doc   # nextest does not run doctests

test-heavy:
    cargo nextest run --workspace --run-ignored only
```

Update `AGENTS.md` §7 tooling table: `test` row description (nextest fast suite + doctests) and a new `test-heavy` row. AGENTS §0: "When this document and the code disagree, fix the document in the same change."

### Phase 4 — Verify budget

Warm the build (one throwaway `cargo nextest run`), then time `just test`. Straggler rule: any test > ~2.5s → shrink further (synthetic) or ignore (real-audio only). `wsola_frames_keeps_left_only_left` (2.6s today) is the known watch item — leave unless the budget busts.

## Anti-Goals (Out of Scope)

- **No `[profile.*` changes** — rejected by the user (shifts cost to compile time).
- **No product-code changes** — engine/plan/placement/renderer/dsp sources untouched; tests, justfile, and docs only.
- **No test deletions or weakened assertions** — durations and windows change; asserts stay verbatim.
- **No new dependencies** (crate-level); cargo-nextest is already installed.
- **No CI changes** beyond the justfile recipes.
- Not touching the ~1–1.7s wsola unit tests or the 1–1.5s ui `track::tests` unless the budget busts.
- No renaming of `ac1_boundaries.rs` / its tests (only the soak rename was approved).

## Edge Cases & Gotchas

- **nextest does not run doctests.** The workspace has doc tests (e.g. `stratum-dsp/src/preprocessing/*`). `just test` must chain `cargo test --workspace --doc` or coverage silently disappears. The doc step is compile-bound on cold runs; measure warm.
- **Window flooring.** The transition window is `[end − preset_beats, end]` floored at session start. If the window (in beats) approaches the track length, placement degenerates — always shrink window *and* durations together, and keep durations ≥ ~6 bars.
- **Goertzel probe floor.** `mix_audibility` probes ¼s windows and asserts trend over window *thirds*; a 4-beat window at ~120 BPM = 2s → thirds ≈ 0.67s > 0.25s probe. Don't go below 4 beats.
- **`custom_pair` contrast.** With both specs at 4 beats, the measurable difference comes from curve *shape* (equal-power vs Linear), not length. That satisfies the existing RMS assertion.
- **Cue slicing.** `mix_audibility` slices deck B's PCM from its stretched cue — the code reads `plan.segments[1]`, so it adapts; don't delete the step (it matches CLI provider behavior).
- **Clippy is warnings-as-errors.** New/edited test math with float→int casts needs `#[expect(clippy::cast_precision_loss, reason = …)]` like the existing code.
- **Stale comments.** File headers and inline comments encode old facts (20 tracks, 150s, "~45 min", "60-minute", "AC5", "soak.rs model") — update them in the same change.
- **Soak assertion coupling.** `four_tracks` must yield exactly 4 segments / 3 transitions; if the one-bar anchor margin crowds a 4-beat window in an 8s (4-bar) track, raise durations to 12s rather than weakening the assert.
- **nextest terminal rewriting.** Always pipe nextest output to a file and grep it (`> /tmp/….out 2>&1`), per user workflow.
- **Measure warm.** The 10s budget excludes compile time; run once to build, then time.

## Navigation Anchors

- `PlanOptions` / `plan_with` / `DEFAULT_PRESET_BEATS` — `crates/automixah-engine/src/timeline/plan.rs:24,47,71`
- Window geometry `[end − preset_beats, end]` — `crates/automixah-engine/src/timeline/placement.rs:163–193`
- `Renderer::new` / `with_transition` / `render_until` / `TrackProvider` — `crates/automixah-engine/src/render/renderer.rs`
- `long_crossfade()` / `TransitionSpec.beats` / `Shape` — `crates/automixah-engine/src/automation/transition_spec.rs`, `…/presets.rs`
- `test` recipe — `justfile`
- Baseline timings — `/tmp/nextest.out` (regenerate with `cargo nextest run --workspace > /tmp/nextest.out 2>&1` if stale)

## Dependency Mappings

- **No new crate dependencies.** No `Cargo.toml` changes.
- `cargo-nextest` binary — already installed (used for all measurements in this plan).
- Existing dev-deps cover everything touched: `tempfile`, `hound` (CLI tests), `rstest`.

## Test Strategies

- **Per-file, during Phase 1:** run just the touched target, e.g. `cargo nextest run -p automixah-engine --test mix_audibility > /tmp/mix.out 2>&1` — must pass with time collapsed from tens of seconds to ≲1.5s.
- **Phase 2 audit:** `cargo nextest run --workspace > /tmp/nextest-after.out 2>&1`; grep slowest tests; confirm the only >2.5s entries are known watch items.
- **Budget:** warm build, then `time just test` — ≤ 10s wall, exit 0.
- **Heavy lane:** `just test-heavy` — every ignored test runs (nextest `--run-ignored only`) and passes.
- **Ignored-set audit:** `grep -rn "#\[ignore" crates/*/tests/` — the set must equal the real-audio tests listed in Phase 2 (plus any budget-forced real-audio additions); no synthetic tests.
- **Hygiene:** `just check` and `just lint` pass (clippy warnings-as-errors).
- **Record Updates (end of implementation):** the approved entry, written verbatim to `.agents/RECORD.md` only if the implementation matches the plan:
  `- (workflow) just test runs the fast suite via nextest; slow real-audio tests are #[ignore]d and run via just test-heavy.`
  If the implementation diverged (e.g. budget forced extra ignores beyond real-audio, or doctest handling changed), do **not** write a wrong entry — surface the divergence in the final summary for the user to resolve.
