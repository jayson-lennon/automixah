# file-formats: import support for opus, flac, mp3, m4a, wav

## Problem

`djcore`'s decoder registry currently supports mp3, flac, wav, ogg, and raw aac. Two requested formats fail: **m4a** (the MP4 demuxer feature is simply not enabled in the symphonia dependency) and **opus** (symphonia 0.5 demuxes Opus-in-OGG but has no Opus decoder — native Opus decoding does not exist upstream in 0.5 or 0.6). Additionally, the CLI hardcodes 44 100 Hz when writing the output WAV while the session rate is actually taken from the first track (`plan.rs:72`) — a 48 kHz opus-first mix would produce a mislabeled, wrong-speed output file.

## Solution

1. **m4a**: enable symphonia's `isomp4` + `alac` features; add `m4a` to `SUPPORTED_EXTENSIONS`. AAC in m4a decodes with the already-enabled `aac` codec; ALAC adds lossless m4a support. Tag/duration probing in `probe_metadata` works for free (the isomp4 demuxer reads iTunes `ilst` tags).
2. **opus**: add `symphonia-adapter-libopus` 0.2 (bundled libopus, C) as a **native-only** dependency of `djcore`. `SymphoniaDecoder::decode_bytes` builds one custom `CodecRegistry` (symphonia's enabled codecs plus the Opus adapter) instead of calling `symphonia::default::get_codecs()`; `opus` joins the extension list. The OGG demuxer already maps the `opus` extension and tags tracks `CODEC_TYPE_OPUS`.
3. **CLI rate fix**: `write_wav` and the progress log lines use `plan.sample_rate` instead of the hardcoded `44_100`.
4. **Fixtures**: ffmpeg-generated `tone440.opus` (48 kHz), `tone440.m4a` (AAC, tagged), `tone440.alac.m4a` committed to `djcore/tests/fixtures/`; a 48 kHz click fixture for the CLI pipeline test.

All consumers (CLI `--track`, UI file dialog, playlist queue) derive their extension lists from `DecoderRegistry::supported_extensions()`, so the new formats propagate with no call-site changes.

## Acceptance Criteria

- `djcore` decodes `.opus`, `.m4a` (AAC and ALAC), `.flac`, `.mp3`, `.wav` fixtures to interleaved stereo f32 with correct sample rate (opus at 48 kHz).
- `probe_metadata` returns title/artist/duration for a tagged m4a.
- `DecoderRegistry::supported_extensions()` includes `opus` and `m4a`; the UI file dialog and CLI accept both with no further changes.
- CLI mix with a 48 kHz first track writes a WAV whose header reports 48 kHz.
~~Workspace still clippy-checks clean for `wasm32-unknown-unknown` (opus dependency excluded there).~~
  *Divergence (user-approved): the wasm job was already broken on main (pre-existing tokio feature error in automixah-ui) and nothing builds for wasm; the CI wasm step and target were removed instead. djcore itself clippy-checks clean for wasm32.*

## Dialectical Outcomes (Why)

- **Opus decoder route — adapter on symphonia 0.5 (chosen) vs upgrade to 0.6 vs skip.** Symphonia has no native Opus decoder in 0.5 *or* 0.6, so `symphonia-adapter-libopus` (libopus C behind a symphonia `AudioDecoder`) is the standard solution. Upgrading to 0.6 + adapter 0.3 was rejected: 0.6 still lacks native Opus, so the migration cost buys zero format gain. The 0.2.x adapter series targets symphonia-core 0.5 and matches the workspace's symphonia 0.5.5.
- **libopus linking — bundled (chosen) vs system.** The adapter's default feature builds and bundles libopus via `cc`; self-contained builds, no system dependency, no new environment requirement.
- **m4a codec coverage — AAC-only vs AAC + ALAC (chosen).** The `alac` feature is a tiny additional crate and covers lossless m4a rips; the isomp4 demuxer already parses the ALAC sample entry.
- **CLI 48 kHz bug — fix now (chosen) vs defer.** Opus input is almost always 48 kHz, so the bug is hit immediately by this feature's headline format. Fix is ~4 lines.
- **wasm gate (discovered during verification).** CI (`.github/workflows/check.yml`) runs `cargo clippy --workspace --target wasm32-unknown-unknown`. Bundled libopus cannot build for `wasm32-unknown-unknown`, so the adapter must be a target-gated dependency and the registry/extension code cfg-gated. Nothing wasm consumes djcore anymore (the Leptos UI was removed; record entry `(cli)`), so gating is safe.
- **Custom codec registry instead of default.** The default `CODEC_REGISTRY` from `symphonia::default::get_codecs()` is feature-driven and cannot include third-party codecs. Symphonia exposes `symphonia::default::register_enabled_codecs(&mut registry)` to populate a fresh registry with all feature-enabled codecs, onto which the Opus adapter is registered. The format probe side (`get_probe()`) needs no change — format readers are enabled by feature flags alone.

## Relevant Files (Where)

| File | Action |
| --- | --- |
| `Cargo.toml` (workspace root) | Modify: add `isomp4`, `alac` to symphonia features; add `symphonia-adapter-libopus` to workspace deps |
| `crates/djcore/Cargo.toml` | Modify: add native-gated `symphonia-adapter-libopus` dependency |
| `crates/djcore/src/decoder/symphonia.rs` | Modify: extension list, custom codec registry |
| `crates/djcore/tests/fixtures/` | Add: `tone440.opus`, `tone440.m4a`, `tone440.alac.m4a` |
| `crates/djcore/tests/integration.rs` | Modify: new decode/probe tests; update extension-coverage test |
| `crates/automixah-cli/src/lib.rs` | Modify: `write_wav` call + log lines use `plan.sample_rate` |
| `crates/automixah-cli/tests/fixtures/` | Add: `120bpm_4bar_48k.wav` |
| `crates/automixah-cli/tests/pipeline.rs` | Modify: 48 kHz output-header test |

Not modified (verified no call-site changes needed): `crates/djcore/src/decoder/mod.rs` (registry is generic over extensions), `meta.rs` (probe works through the demuxer for free), `crates/automixah-ui/src/app.rs` (file dialog filters come from `registry.supported_extensions()`), `crates/automixah-ui/src/playlist/queue.rs`, `crates/automixah-cli` decode path, engine crates.

## Key Code Context (What)

Workspace symphonia pin (root `Cargo.toml`) — add `isomp4`, `alac`:

```toml
symphonia = { version = "0.5", features = ["mp3", "flac", "wav", "ogg", "aac"] }
```

`crates/djcore/src/decoder/symphonia.rs` — the constants and registry call to change:

```rust
/// File extensions supported by the symphonia decoder (lowercase, without dot).
const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "aac"];
```

```rust
let mut decoder = symphonia::default::get_codecs()
    .make(&codec_params, &decoder_opts)
    .change_context(DecodeError)
    .attach("failed to create audio decoder")?;
```

Adapter usage pattern (from symphonia-adapter-libopus 0.2 docs; `register_all` is also what symphonia itself uses internally):

```rust
use symphonia::core::codecs::CodecRegistry;
let mut codec_registry = CodecRegistry::new();
symphonia::default::register_enabled_codecs(&mut codec_registry); // feature-enabled codecs
codec_registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
```

`crates/automixah-engine/src/timeline/types.rs` — the session rate source:

```rust
pub struct SessionPlan {
    /// The session-wide target tempo.
    pub session_bpm: f32,
    /// Engine sample rate all session times are expressed at.
    pub sample_rate: u32,
    /// Ordered segments; adjacent segments overlap during transitions.
    pub segments: Vec<Segment>,
}
```

`crates/automixah-cli/src/lib.rs` — the hardcoded 44 100 sites to fix (`write_wav` call near line 112; log division lines in `log_plan`/`log_transition` near lines 286, 296, 304–305):

```rust
write_wav(&config.out, &mix, 44_100).change_context(CliError)?;
```

```rust
fn log_transition(t: &TransitionPlan) {
    eprintln!(
        "    transition @ {:.1}s→{:.1}s preset {}",
        t.window.start.0 as f64 / 44_100.0,
        t.window.end.0 as f64 / 44_100.0,
        t.preset.0
    );
}
```

Existing djcore integration test to update (`crates/djcore/tests/integration.rs`):

```rust
#[test]
fn symphonia_decoder_name_and_extensions() {
    let decoder = SymphoniaDecoder::new();
    assert_eq!(decoder.name(), "symphonia");
    assert!(decoder.supported_extensions().contains(&"mp3"));
    // ... flac, wav, ogg, aac
}
```

CLI pipeline test helpers (`crates/automixah-cli/tests/pipeline.rs`) — `read_wav` already returns the header rate, and configs take fixture `PathBuf`s:

```rust
fn read_wav(path: &std::path::Path) -> (u32, Vec<f32>, u16) { ... }
fn click_config(out: &std::path::Path) -> Config { ... }
```

## Implementation Algorithm (How)

### Phase 1 — Decoder support

1. Root `Cargo.toml`: `symphonia = { version = "0.5", features = ["mp3", "flac", "wav", "ogg", "aac", "isomp4", "alac"] }`. Add `symphonia-adapter-libopus = "0.2"` to `[workspace.dependencies]` (default features = bundled libopus).
2. `crates/djcore/Cargo.toml`:

   ```toml
   [target.'cfg(not(target_arch = "wasm32"))'.dependencies]
   symphonia-adapter-libopus.workspace = true
   ```

3. `crates/djcore/src/decoder/symphonia.rs`:
   - cfg-split the extension const:

     ```rust
     #[cfg(not(target_arch = "wasm32"))]
     const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "aac", "opus", "m4a"];
     #[cfg(target_arch = "wasm32")]
     const SUPPORTED_EXTENSIONS: &[&str] = &["mp3", "flac", "wav", "ogg", "aac", "m4a"];
     ```

   - add a lazily-built shared registry accessor (native registers opus; wasm falls back to the default registry):

     ```rust
     #[cfg(not(target_arch = "wasm32"))]
     fn codec_registry() -> &'static symphonia::core::codecs::CodecRegistry {
         use symphonia::core::codecs::CodecRegistry;
         static REGISTRY: std::sync::OnceLock<CodecRegistry> = std::sync::OnceLock::new();
         REGISTRY.get_or_init(|| {
             let mut registry = CodecRegistry::new();
             symphonia::default::register_enabled_codecs(&mut registry);
             registry.register_all::<symphonia_adapter_libopus::OpusDecoder>();
             registry
         })
     }

     #[cfg(target_arch = "wasm32")]
     fn codec_registry() -> &'static symphonia::core::codecs::CodecRegistry {
         symphonia::default::get_codecs()
     }
     ```

   - in `decode_bytes`, replace `symphonia::default::get_codecs().make(...)` with `codec_registry().make(...)`. Everything else (probe via `get_probe()`, packet loop, `interleave_channels`) is untouched.
4. `just check` must pass.

### Phase 2 — Fixtures & djcore tests

1. Generate fixtures with the system ffmpeg (n9.0.1 present) into `crates/djcore/tests/fixtures/`:

   ```sh
   ffmpeg -f lavfi -i sine=frequency=440:duration=1 -ar 48000 -c:a libopus tone440.opus
   ffmpeg -f lavfi -i sine=frequency=440:duration=1 -ar 44100 -c:a aac \
     -metadata title="Tone Four Forty" -metadata artist="Test Artist" tone440.m4a
   ffmpeg -f lavfi -i sine=frequency=440:duration=1 -ar 44100 -c:a alac tone440.alac.m4a
   ```

2. Add tests to `crates/djcore/tests/integration.rs` following the existing BDD style (`decode_tone_fixture` helper exists; ALAC needs a variant that maps a friendly name to the `.alac.m4a` file):
   - opus: sample rate 48 000, length within 5% of 48 000 (covers encoder padding + pre-skip).
   - m4a AAC: length within 5% of 44 100.
   - m4a ALAC: exact length, equal to the WAV decode length (lossless).
   - `probe_metadata` on tagged m4a: title and artist present, duration present.
   - garbage bytes with extension `opus`: `Err`, no panic.
   - Update `symphonia_decoder_name_and_extensions` to also assert `opus` and `m4a`.

### Phase 3 — CLI session-rate fix

1. `crates/automixah-cli/src/lib.rs`: replace the three hardcoded `44_100` divisions and the `write_wav` call with `plan.sample_rate`. `log_plan` already receives `&SessionPlan`; pass `plan.sample_rate` into `log_transition` (change its signature to take the rate). `run()`'s final `eprintln` duration line uses `plan.sample_rate` too.
2. Fixture: resample the existing stratum-dsp click to 48 kHz into `crates/automixah-cli/tests/fixtures/120bpm_4bar_48k.wav` (`ffmpeg -i ../stratum-dsp/tests/fixtures/120bpm_4bar.wav -ar 48000 ...`). Beat structure survives resampling; analysis still detects ~120 BPM.
3. Pipeline test: config with the 48 kHz click first and `click128()` second; `run(...)`; assert `read_wav(out).0 == 48_000`. This also exercises mixed-rate planning (48k first track defines the session rate; the 44.1k second track stretches across rates via `decide_stretch`).

### Phase 4 — Verification

- `just check`, `just test`, `just lint` all green.
~~`cargo clippy --workspace --all-targets --target wasm32-unknown-unknown -- -D warnings` green (mirrors CI).~~
  *Removed with the CI wasm step (user-approved; the job was broken on main before this task).*
- Walk every acceptance criterion; then handle Record Updates (below).

## Anti-Goals (Out of Scope)

- No symphonia 0.5 → 0.6 upgrade.
- No system-libopus/pkg-config linking (bundled only).
- No new UI code, CLI flags, or call-site changes — extension propagation through the registry is automatic.
- No resampling to a canonical session rate at decode time; mixed-rate sessions are the engine's existing concern.
- No variable-BPM/tempo support change; no seeking; no metadata editing.
- No MP4 video, no ALAC-in-CAF or other exotic containers beyond what the enabled demuxers already parse.
- No dependency on ffmpeg at runtime (ffmpeg is a development-time fixture generator only).

## Edge Cases & Gotchas

- **wasm32 CI clippy pass**: the adapter must live under `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`, and every reference to it (`SUPPORTED_EXTENSIONS`, `codec_registry`) must be cfg-gated symmetrically. A plain dependency breaks the wasm clippy job because `cc` cannot build libopus for `wasm32-unknown-unknown`.
- **Opus pre-skip/padding**: opus-in-OGG carries a pre-skip (~312 samples at 48 kHz) and encoder padding; symphonia's OGG opus mapping handles timestamps, but decoded length tolerance must be 5% (same convention as the existing mp3/ogg/aac tests).
- **Opus is 48 kHz**: libopus only accepts 48/24/16/12/8 kHz. Fixtures and the rate assertion must expect 48 000, not 44 100.
- **`SessionPlan::sample_rate` comes from the first track** (`plan.rs:72`) — that is the value the WAV header must carry; do not invent a fixed rate.
- **Registry last-write-wins**: `DecoderRegistry::register` maps each extension to one decoder; all seven extensions land on the single `SymphoniaDecoder`, so no conflict.
- **m4a with video track / data tracks**: `default_track()` picks the audio track; non-audio packets are skipped by the existing `track_id` filter. Pure-video m4a/mp4 still errors with "no audio track" — acceptable.
- **>2-channel m4a** (5.1 AAC): existing `interleave_channels` keeps the front L/R pair — unchanged behavior.
- **Workspace lints**: `missing_docs` is warn and pedantic clippy is deny-in-CI; any new public item needs a doc comment, and internal fns (`codec_registry`) are private so they need none.
- **Fixture gitignore**: `automixah-engine/tests/fixtures/music/` is gitignored for real music; the new fixtures are synthetic tones and must be *committed* (the djcore `tone440.*` set already is).
- **fdk/opus licensing note**: libopus is BSD-3; bundling is fine for this experimental project (license field is MIT — no action needed, but be aware).

## Navigation Anchors

- `SymphoniaDecoder::decode_bytes` (`crates/djcore/src/decoder/symphonia.rs`) — primary edit point for the codec registry swap.
- `SUPPORTED_EXTENSIONS` (same file) — extension list.
- `DecoderRegistry::with_symphonia` / `DecoderRegistry::supported_extensions` (`crates/djcore/src/decoder/mod.rs`) — unchanged, but the propagation mechanism (UI dialog filter, playlist queue) that makes call-site edits unnecessary.
- `probe_metadata` (`crates/djcore/src/decoder/meta.rs`) — works for m4a once `isomp4` is enabled; no edit.
- `run` / `log_plan` / `log_transition` / `write_wav` (`crates/automixah-cli/src/lib.rs`) — the 44 100 hardcodes.
- `plan_with` (`crates/automixah-engine/src/timeline/plan.rs:72`) — where `SessionPlan::sample_rate` originates.
- `decode_tone_fixture` / `fixture` helpers (`crates/djcore/tests/integration.rs`) — fixture test entry points.
- `read_wav` / `click_config` (`crates/automixah-cli/tests/pipeline.rs`) — CLI test entry points.

## Dependency Mappings

- **New external**: `symphonia-adapter-libopus = "0.2"` (0.2.x series is symphonia-0.5-compatible; default features bundle libopus via `opusic-sys` + `cc`; native targets only). Workspace dep + djcore target-gated inheritance.
- **Feature additions (no new crates)**: `symphonia/isomp4` (pulls `symphonia-format-isomp4`), `symphonia/alac` (pulls `symphonia-codec-alac`). Both crates already appear in the lockfile resolution space for symphonia 0.5.5.
- **Internal**: none — `djcore` already re-exports the decoder module; consumers are untouched.

## Test Strategies

- **djcore unit/integration** (`crates/djcore/tests/integration.rs`): mirror the existing tone-fixture pattern; one behavior per test, BDD comments, 5% tolerance for lossy, exact for lossless (flac precedent: `decodes_flac_fixture_exactly`). Garbage-bytes test follows `probe_metadata_rejects_garbage` precedent.
- **djcore probe** (`meta.rs` tests or integration): tagged m4a title/artist/duration — via `probe_metadata` directly, in integration tests (fixture-based).
- **CLI pipeline** (`crates/automixah-cli/tests/pipeline.rs`): extend with the 48 kHz first-track config; assert WAV header rate and that logged session length equals WAV duration (rate consistency). Keep runtime reasonable — the existing two-fixture configs are the model.
~~**Manual smoke**: `just mix out.wav <some .opus> <some .m4a>` on real files if available; `rfd` dialog in the UI shows opus/m4a in the filter (manual, not automated).~~
  *Removed by user decision: no manual tests are required for this work. (The equivalent command was run once during review and passed; tracks must be passed as repeated `--track` flags.)*
- **Full gate**: `just check && just test && just lint`. ~~plus the wasm clippy invocation mirroring CI~~ *(wasm step removed from CI).*

## Phases (summary)

1. **Decoder support** — Cargo feature/dependency wiring, extension list, custom codec registry with opus adapter.
2. **Fixtures & djcore tests** — ffmpeg fixtures, decode/probe/extension tests.
3. **CLI session-rate fix** — thread `plan.sample_rate` through write/log; 48 kHz pipeline test.
4. **Verification** — full gate (check/test/lint + wasm clippy), acceptance criteria walk, Record Updates.

## Record Updates

Applied at end of implementation (human-approved here; written only if implementation matches):

- `- (analysis) djcore decodes mp3, flac, wav, ogg, aac, opus, and m4a (AAC or ALAC) input via symphonia; opus uses a bundled libopus adapter available on native targets only.`
- `- (cli) The CLI writes the output WAV at the session sample rate (the first track's rate) rather than a fixed 44.1 kHz.`

No existing entries are contradicted.
