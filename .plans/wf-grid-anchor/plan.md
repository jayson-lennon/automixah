# Spec: 48 kHz waveform timeline + grid visibility + anchor accuracy

Task slug: `wf-grid-anchor`

## Problem

1. **Waveform drifts vs audio/grid on 48 kHz tracks** (the real "Thousand Pieces" bug): `peaks.rs` compares an integer frame counter against the fractional stride `48000/441 = 108.8435…`; the counter can only reach 109, so every visual slot swallows 109 frames. The picture stretches ~86 ms/min → ~0.6 s (≈1.4 beats at 138 BPM) error by the end of a 7-minute track. Audio playback and the grid both run on true time (the f64 scrub/playhead merge is intact and NOT involved), so listening matches the grid while the render doesn't. 44.1 kHz tracks (stride exactly 100.0) are unaffected — which is why only the 48 kHz opus tracks look broken. Side effect: the waveform's right edge is truncated (~1 frame per slot, ~0.6 s over the track).
2. **Beat lines unreadable**: they are blue `(70,130,255)`, nearly identical to the blue high-band waveform columns `(70,110,255)` — the user saw band columns and mistook them for non-equidistant gridlines. Additionally at far zoom-out beats are ~1 px apart, so the overlay becomes a wall of lines.
3. **Auto-detected anchor ~150 ms off** on offbeat-heavy trance: the phase-anchor fit (`grid_fit.rs` `phase_anchor`) averages wrapped residuals of *all* DP marks, so offbeat bass drags the grid off the kicks. Measured on the reference problem file: true tempo 137.9994 vs grid 138.000 (no real tempo error), but anchor lands ~154 ms off the kick phase; a single global shift takes median kick-to-grid error 153 ms → 1 ms.

## Solution

1. **Fractional stride accumulation in `Peaks::build`** so slot *k* covers source frames `[k·stride, (k+1)·stride)` exactly.
2. **Grid overlay polish** in `view/grid.rs`: beat lines become thin translucent white, downbeats stay heavy opaque white; beat lines hide when spacing < ~4 px.
3. **Post-fit anchor refinement in `stratum-dsp`**: snap the anchor within ±half a beat to the phase maximizing low-band onset energy at grid beat positions (kick comb). BPM, rounding ladder, DP tracker untouched.

## Acceptance Criteria

- 48 kHz synthetic fixture: impulses at known frames map to the exact expected visual slot; final slot's coverage ends at the true last frame (no truncation).
- 44.1 kHz behavior unchanged (existing peaks tests pass untouched).
- Beat/downbeat colors differ from every band color by a clear hue/value margin (test enforces).
- At overview zoom on a 138 BPM track: beat lines hidden, downbeats visible; zoomed to ≥ 4 px/beat: all lines visible.
- Auto grid on the 48 kHz opus reference locks < 15 ms median kick error with **no** manual anchor shift (via `align_check`).
- Existing `grid_fit` suite passes unchanged (BPM ladder, stability, materialization untouched).
- `just check`, `just test`, `just lint` green.

---

## Dialectical Outcomes (Why)

- **User's original "grid drifts" report was three stacked issues.** Investigation (regression of kick events vs beat index; whole-track tempo fit 137.9994 vs 138.000) ruled out true tempo error. The final "audio aligned, waveform not" observation pinpointed the render path: the wave/pixel mapping is built from the peaks summary, not raw PCM, and the summary's stride loop is the only place frame→time diverges on 48 kHz sources. The f64 merge (audio-cut branch, scrub position/`Playhead`) is explicitly NOT implicated — audio runs on true time.
- **44.1 kHz stride is exactly 100.0**, which is why all 44.1 kHz .ogg tracks render fine and only 48 kHz .opus tracks (Thousand Pieces, Escape from Reality) misbehave. Verified in the user's live library DB.
- **Rejected: rebuild waveform from raw PCM per frame.** Peaks summary (Mixxx scheme, 441 visual samples/s, u8 quartets) is the right storage; only the slot-boundary accumulation is wrong.
- **Rejected: earlier "aliasing/phantom beats" theory** for "non-equidistant lines" — user confirmed the true cause was beat-line color colliding with the blue high band. Color fix + zoom-out hiding (user explicitly asked to keep the hiding rule as polish: "when I zoom way out then all I see are grid lines").
- **Anchor refinement approach chosen (post-fit snap, option B)** over reworking the DP tracker or the phase-anchor math (higher blast radius, affects all tracks). The snap is validated on the reference file already via the `align_check` example's manual-shift emulation (153 ms → 1 ms). Self-contained: runs *after* `fit_constant_grid` produces BPM + anchor; cannot corrupt BPM or the ladder.
- **No DB/schema changes.** The user will click the existing "re-analyze" button once on the affected track to discard the stale stored grid after the analyzer fix lands.

## Relevant Files (Where)

| File | Action |
|---|---|
| `crates/automixah-ui/src/audio/peaks.rs` | Modify: fractional stride accumulation in `Peaks::build`; new tests |
| `crates/automixah-ui/src/view/grid.rs` | Modify: `BEAT_COLOR`, visibility rule in `paint`; extract pure helper; tests |
| `crates/stratum-dsp/src/features/beat_tracking/grid_fit.rs` | Modify: add `refine_anchor` post-fit step + low-band comb scoring; tests |
| `crates/stratum-dsp/src/features/beat_tracking/mod.rs` | Possibly modify: thread low-band envelope/magnitudes into `fit_constant_grid` call (see Dependency Mappings) |
| `crates/automixah-ui/examples/align_check.rs` | Modify: assert auto (pre-manual) lock < 15 ms; keep manual-shift emulation as fallback print |
| `crates/automixah-ui/examples/wave_check.rs` | Run for verification (no change expected; totals shift slightly at 48 kHz — fixture is 44.1 kHz OGG so unchanged) |

No new files elsewhere; no schema/migration changes.

## Key Code Context (What)

### peaks.rs — the bug (current code to replace)

```rust
pub fn build(samples: &[f32], sample_rate: u32) -> Self {
    #[expect(clippy::cast_precision_loss, reason = "sample rate fits f32")]
    let stride = sample_rate as f32 / VISUAL_RATE;          // 108.8435… at 48k
    ...
    let mut stride_frames_consumed = 0;
    for frame in samples.chunks_exact(2) {
        let (l, r) = (frame[0], frame[1]);
        let bands = splitter.process_frame(l, r);
        running.absorb(bands, l, r);
        stride_frames_consumed += 1;                          // integer
        #[expect(clippy::cast_precision_loss, reason = "counter fits f32")]
        if stride_frames_consumed as f32 >= stride {          // 108.84 unreachable → every slot = 109 frames
            data.push(PeakQuartet::from_running(&running));
            running = RunningPeak::default();
            stride_frames_consumed = 0;
        }
    }
    if stride_frames_consumed > 0 {
        data.push(PeakQuartet::from_running(&running));
    }
    Self { data, stride_frames: stride }
}
```

Consumers of the summary (do not change their contract): `waveform.rs` `total_frames()` = `data.len() as f32 * stride_frames`, and `aggregate()` maps pixel frame ranges → visual indices via `/ stride`. Honest `stride_frames` (f32) makes those exact.

### view/grid.rs — paint loop + colors (current)

```rust
const BEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(70, 130, 255, 170); // ≈ high band color!
const DOWNBEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 220);

pub fn paint(painter, grid, rect, seconds_per_pixel, time_at_left, track_end) {
    let beat = grid.beat_seconds();
    ...
    let is_downbeat = k.rem_euclid(BEATS_PER_BAR as i64) == i64::from(grid.downbeat_phase);
    let (color, half_w) = if is_downbeat { (DOWNBEAT_COLOR, 1.0) } else { (BEAT_COLOR, 0.5) };
    ...
}
```

Band colors in `waveform.rs` (must stay visually distinct from grid colors): low `(180,30,30,160)`, mid `(30,180,60,140)`, high `(70,110,255,120)`.

### grid_fit.rs — fit entry point (post-fit refinement slots in here)

```rust
pub fn fit_constant_grid(
    marks: &[f32], bpm_seed: f32, novelty: &[f32], hop_seconds: f64, duration: f32,
) -> Result<(BeatGrid, f32), AnalysisError> {
    ...
    let (grid_bpm, beat_length) = rounded_bpm(span);
    let anchor0 = phase_anchor(&cleaned, span.start, beat_length);
    let anchor = downbeat_phase(anchor0, beat_length, novelty, hop_seconds, f64::from(duration));
    let stability = residual_stability(&cleaned, anchor, beat_length);
    let grid = materialize(grid_bpm, anchor, beat_length, f64::from(duration));
    ...
}
```

Constants already defined there: `MAX_PHASE_ERROR = 0.025`, `BEATS_PER_BAR = 4`, `DOWNBEAT_VOTE_WINDOW = 0.03`. The novelty passed in is the **energy-flux (full-band)** envelope — that is what the offbeat bass corrupts; the refinement needs a **low-band** envelope instead (see How).

### lib.rs — where generate_beat_grid is called (input plumbing)

```rust
let (beat_grid, grid_stability) = if bpm > 0.0 && !novelty_envelope.is_empty() {
    match generate_beat_grid(bpm, bpm_confidence, &novelty_envelope, config.hop_size as u32, sample_rate, duration_seconds) { ... }
};
```

`generate_beat_grid(bpm, confidence, novelty, hop_size, sample_rate, duration)` lives in `features/beat_tracking/mod.rs` and calls `grid_fit::fit_constant_grid(marks, bpm, novelty, hop_seconds, duration)`.

## Implementation Algorithm (How)

### Phase 1 — fractional stride (peaks.rs)

Replace integer counting with a fractional accumulator in `f64` (frame indices reach 2·10⁷; f64 is exact there and keeps the visual-domain math consistent with the audio-cut f64 merge):

```
let stride = f64::from(sample_rate) / f64::from(VISUAL_RATE);   // 108.8435… at 48k, exactly 100 at 44.1k
let mut next_boundary = stride;                                  // end frame of slot 0
for (frame_idx, frame) in samples.chunks_exact(2).enumerate() {
    ... absorb ...
    if (frame_idx + 1) as f64 >= next_boundary {                 // slot k covers [k·stride, (k+1)·stride)
        push quartet; reset running; next_boundary += stride;
    }
}
if last frame not covered by a pushed slot { push final partial quartet; }
```

- Slot *k* boundaries at `k·stride` in f64 — no cumulative drift.
- 44.1 kHz: boundaries at exact multiples of 100 — behavior identical to today (existing tests must pass unchanged).
- Keep `stride_frames: f32` in `Peaks` (cast once); `waveform.rs` consumers are already exact with an honest f32 stride (ulp error ≤ 1 frame at 2·10⁷ — invisible at any zoom ≥ 4 fpp).
- Expected visible effect: at 48 kHz, `data.len()` ≈ frames/108.84 (was frames/109), `total_frames()` ≈ true frame count.

### Phase 2 — grid overlay polish (view/grid.rs)

1. `BEAT_COLOR` → thin translucent white, e.g. `Color32::from_rgba_premultiplied(255, 255, 255, 90)`; `DOWNBEAT_COLOR` unchanged (heavy 220). Tune alpha so beats read as a sub-tier of the downbeat (Mixxx convention).
2. Extract a pure visibility predicate:

```rust
/// Beat lines are drawn only when at least this many pixels separate
/// adjacent beats; below it the overlay would read as a solid band.
const MIN_BEAT_SPACING_PX: f32 = 4.0;
fn beat_lines_visible(beat_seconds: f32, seconds_per_pixel: f32) -> bool {
    beat_seconds / seconds_per_pixel >= MIN_BEAT_SPACING_PX
}
```

3. In `paint`, when `!beat_lines_visible(...)` skip painting non-downbeat lines only (downbeats always paint). The `is_downbeat` classification is already computed per line — invert the condition into the existing `if (rect...)` paint guard.
4. Add hue/value-margin test comparing both grid colors against the three band colors (e.g. assert channel-wise max-abs diff ≥ threshold and, for the beat color vs high band, that the white lines dominate on the G/R channels where the high band is dark).

### Phase 3 — anchor refinement (stratum-dsp grid_fit.rs)

1. **Low-band envelope input.** The energy-flux novelty is full-band. Derive a low-band (<600 Hz) onset envelope from the STFT magnitudes the pipeline already computes (bin frequency = `k · sample_rate / fft_size`; sum magnitudes for bins < 600 Hz, then first-difference / half-wave rectify the running energy, same hop as `novelty`). Plumb it as a new optional slice parameter: `fit_constant_grid(..., low_novelty: &[f32])` — `generate_beat_grid` passes it through; `lib.rs` computes it next to `energy_flux_novelty`. Keep it additive: empty slice ⇒ refinement is a no-op (protects existing tests/callers).
2. **Refinement (post `downbeat_phase`):**

```
fn refine_anchor(anchor, beat_length, low_novelty, hop_seconds, duration) -> f64 {
    // Scan phase offsets in ±half a beat around the fitted anchor,
    // step = hop_seconds (≈23 ms at 1024/44.1k — finer than the 25 ms target).
    best = anchor; best_score = -inf;
    for delta in (-beat_length/2 ..= +beat_length/2).step_by(hop_seconds) {
        cand = anchor + delta;
        score = Σ over beats b in [0, duration): Σ low_novelty[frames within ±DOWNBEAT_VOTE_WINDOW of cand + b·beat_length];
        if score > best_score { best_score = score; best = cand; }
    }
    return best.rem_euclid(beat_length) as the new anchor (pre-downbeat-vote semantics: anchor is a BEAT time, then re-run the existing downbeat_phase vote against the refined anchor);
}
```

   - This is the same comb-energy idea the existing `downbeat_phase` vote uses (reuse `phase_energy` with `bar`→`beat_length`), now applied to beats against the low band. Factor `phase_energy(anchor, period, novelty, hop, duration)` (already general) — call it with `period = beat_length` and the low-band envelope.
   - Order in `fit_constant_grid`: `rounded_bpm` → `phase_anchor` → `refine_anchor` (beat alignment) → `downbeat_phase` (bar phase vote) → stability/materialize. Downbeat vote runs on the refined anchor so bar classification follows the corrected beat phase.
3. **Bounded:** candidate range is one full beat window (−½..+½ beat); refinement cannot move the anchor more than half a beat and cannot touch `grid_bpm`, span, or ladder output.
4. Stability metric (`residual_stability`) continues to use the DP marks — unchanged.

### Phase 4 — real-audio verification

- Extend `align_check.rs`: before the manual-shift emulation, compute and assert the *auto* grid's median kick error < 15 ms; on failure print both. Keep the manual-shift block (harmless; demonstrates headroom) but the assertion must pass on the auto grid alone.
- Run `wave_check` (44.1 kHz reference OGG — geometry math unchanged).
- Run `just check && just test && just lint`.

### Phase 5 — Record updates

Write the three entries from the approved plan into `.agents/RECORD.md` (see task list for divergence handling).

## Anti-Goals (Out of Scope)

- No change to `ScrubCore`, `Playhead`, `RateFolder`, or any audio-callback f64 code (the merge is correct).
- No change to the DP tracker, tempogram, BPM fusion, or the rounding ladder.
- No schema/migration/DB changes; no auto-invalidation of stored grids (user clicks "re-analyze" once on affected tracks).
- No variable-tempo support (fixed-BPM is a recorded global boundary).
- No rendering architecture change (keep u8 peak quartets, 441 Hz visual rate, Bessel bands).
- No detected-onset overlay/debug toggle (rejected in dialogue).
- No per-band waveform color redesign beyond what grid-line distinctness requires.

## Edge Cases & Gotchas

- **Integer-boundary stall**: the new accumulator must compare `frame_idx+1 ≥ next_boundary` with f64 doubles; at 44.1 kHz boundaries are exact integers — ensure `>=` (not `>`) so slot length stays exactly 100 (no off-by-one reintroducing 101/99 alternating).
- **Trailing partial slot**: flush only when ≥ 1 frame is uncovered; a slot covering 0 frames must not be pushed (would shift `total_frames` past the true length).
- **Anchor semantic**: `fit_constant_grid`'s anchor is a *beat* time; `downbeat_phase` shifts it by whole beats into `[0, bar)` afterwards. The refinement output must stay a beat-aligned value (mod beat_length, not mod bar) before the vote, or bar phase corrupts.
- **Empty low-band envelope** (silence/short input): refinement returns the input anchor unchanged — the no-op guard on empty slice.
- **hop step vs range**: at 138 BPM, ±½ beat ≈ ±217 ms; hop 1024/44100 ≈ 23.2 ms → ~19 candidates; ensure the scan is inclusive of both endpoints (`-½..=+½`) so a half-beat-late offbeat bass can still be escaped.
- **f32 stride reporting**: `stride_frames` stays f32; `total_frames = len · stride_f32` at 48 kHz must round to within ±2 frames of the true count — assert in test.
- **Beat-color alpha on premultiplied egui colors**: use `from_rgba_premultiplied` consistently; test compares premultiplied channel triples, so pick the beat alpha so G and R channels (255) clearly exceed the high band's (70/110) — that's the margin the test enforces.
- **align_check regression risk**: the two 44.1 kHz reference OGGs already lock ≤ ~2 ms after manual shift; after the analyzer change their *auto* anchors must also pass < 15 ms — if a reference regresses, the refinement step/score needs revisiting (surface, don't tune blindly).
- **Existing `grid_fit` tests** construct grids via `fit_constant_grid` with synthetic novelty envs; the new parameter must default (empty slice overload or `Option`) so those tests compile unchanged.

## Navigation Anchors

- `crates/automixah-ui/src/audio/peaks.rs` → `Peaks::build` (the stride loop).
- `crates/automixah-ui/src/view/grid.rs` → `paint` + `BEAT_COLOR` (top of file).
- `crates/stratum-dsp/src/features/beat_tracking/grid_fit.rs` → `fit_constant_grid`, `phase_energy` (reuse), `downbeat_phase`, `phase_anchor`.
- `crates/stratum-dsp/src/features/beat_tracking/mod.rs` → `generate_beat_grid` (signature extension).
- `crates/stratum-dsp/src/lib.rs` → the `generate_beat_grid(...)` call site (novelty plumbing; ~line 926).
- `crates/automixah-ui/examples/align_check.rs` → `phase_err`, manual-shift emulation block.

## Dependency Mappings

- **No new external crates.** Everything uses existing types (`Color32`, STFT magnitudes, `BandSplitter` untouched).
- Internal additions:
  - `grid_fit::refine_anchor(anchor, beat_length, low_novelty, hop_seconds, duration) -> f64` (new fn in grid_fit.rs).
  - `generate_beat_grid` gains a `low_novelty: &[f32]` parameter (empty = no-op).
  - Low-band novelty helper in `features/period/novelty.rs` or inline in `lib.rs` (sum <600 Hz bins of `magnitude_spec_frames`, half-wave-rectified first difference) — decide at implementation; keep it colocated with `energy_flux_novelty` per the style guide.
- Test-only: fixture builders for 48 kHz impulse trains (peaks) and offbeat-bass click envs (grid_fit).

## Test Strategies

- **Phase 1**: new tests in `peaks.rs`:
  - `peaks_slot_k_covers_k_stride_at_48k` — place impulses at frames `⌊k·108.8435…⌋` for k = 0..N at 48 kHz; assert each impulse's quartet index equals k exactly (old code drifts 1 slot per ~76 slots).
  - `peaks_total_frames_matches_source_at_48k` — assert `|total_frames(peaks) − true_frames| ≤ 2`.
  - Existing 44.1 kHz tests pass untouched (regression).
- **Phase 2**: tests in `view/grid.rs`:
  - `beat_color_distinct_from_band_colors` — margin check vs the three band colors (band colors imported or duplicated as consts in the test to avoid API churn).
  - `beat_lines_hidden_below_min_pixel_spacing` — `beat_lines_visible(beat_seconds, spp)` false/true across the 4 px threshold; downbeats unconditionally painted (paint-guard logic mirrored in a pure helper).
- **Phase 3**: tests in `grid_fit.rs`:
  - `refine_anchor_snaps_to_kicks_despite_offbeat_energy` — clicks at beat 0 phase, louder offbeat events at +half-beat; assert refined anchor within 15 ms of kick phase while the pre-refinement `phase_anchor` result lands near the offbeat.
  - `refine_anchor_keeps_locked_grid` — clean click train; refinement delta < 5 ms.
  - Existing `t1..t5` fixtures pass unchanged (they pass empty low-band slices or aligned envs).
- **Phase 4**: run `align_check` on both reference OGGs + the 48 kHz opus; auto-lock < 15 ms each. `wave_check` on reference OGG. `just check/test/lint`.

## Phases (implementation order)

1. **Fractional stride fix** — `peaks.rs` accumulator rewrite + 48 kHz tests + regression green.
2. **Grid overlay polish** — color change + visibility rule + tests.
3. **Anchor refinement** — low-band envelope plumbing + `refine_anchor` + unit tests.
4. **Real-audio verification** — `align_check` auto assertion, `wave_check`, full `just` gates.
5. **Record updates** — three entries (with divergence check).
