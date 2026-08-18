# automixah — Automatic DJ Music Player: Context-Rich Specification

**Status:** Approved (v2 plan, post-dialectic). This document is the authoritative reference for implementation.

---

## Problem

Build a zero-intervention DJ *player*: the user assembles a playlist and presses play; the app analyzes every track (beatgrid/BPM/key), time-scales tracks to a session tempo, plans beat-aligned transitions from data-defined automations, renders the mix ahead-of-time in Rust/wasm, and streams it continuously. Web-first (wasm/PWA), sideloaded Android APK via Tauri later. Live-DJing and seeking are permanently out of scope.

## Solution

All DSP in pure, browser-free Rust compiled to wasm, running in dedicated Web Workers (render + analysis); the browser does only chunked PCM playback plus a Leptos UI. The mixer is driven by an addressed, MIDI-shaped control bus: automations are data timelines of control events, abstracted behind a `ControlSource` trait so MIDI hardware can drive the same surface later. Time-scaling supports pitch-adjusted resampling (default) and pitch-preserving WSOLA. Analysis lives in a new shared `djcore` crate extracted from `harmonic-playlist` (extended to surface full beat grids). Default UX is zero-config: playlist → play.

---

## Dialectical Outcomes (Why)

Key decisions from the Socratic dialogue, with rejected alternatives:

1. **Web-first PWA; Tauri 2 APK as final phase (not egui).** The core is Rust/wasm either way; Tauri loads the same web UI in a webview with zero code split. Chromium-on-Android testing works day one via install-to-home-screen. egui was rejected: it would discard the Leptos decision and force native audio (cpal/oboe) with no benefit. Note: Tauri does *not* automatically solve background audio — Android pauses backgrounded activities unless a foreground service holds them; Tauri ships none, so a thin Kotlin plugin is required (phase 8).
2. **Offline / just-in-time render in Rust (not real-time WebAudio graph).** User requirement: "plan 60–120s ahead; frontend just plays it." The whole mixer becomes pure Rust, sample-accurately testable without a browser, one code path for web+native. Real-time WebAudio was rejected: `playbackRate` time-scaling is pitch-shifting only (no keylock), scheduling drifts, and automations would live in JS. WASM is also not inherently more battery-efficient than WebAudio — the wins are testability and a single code path; steady-state cost is bounded because rendering is ahead-of-time bursts.
3. **Two workers (render + analysis), never one.** Analysis bursts must never starve the render deadline. `rayon` under `wasm32-unknown-unknown` runs single-threaded, so whole-track analysis takes seconds — analysis must be batch, worker-isolated, and progress-reporting. This also enables "analyze while a mix plays" (user appends a track mid-session).
4. **Dual time-scaling; pitch-adjusted is the default.** User preference: fixed-BPM sessions with turntable-style pitch shift most of the time. Pitch-adjusted resampling is cheap and sample-exact; WSOLA (pitch-preserving) runs only when stretch exceeds ±8% or user opts in. This mostly removes the continuous-WSOLA battery cost of fixed-BPM mode.
5. **Both tempo strategies, session-BPM primary.** Session-wide target BPM (auto: median, octave-normalized; user-overridable) is default = "here's my playlist, deal with it." Pairwise drift-back (incoming matches outgoing during overlap, eases back after) is a per-transition alternative for outlier tracks. ±8% is the comfort band; UI colorizes beyond it (UI specifics out of scope).
6. **`djcore` as a new shared crate in its own repo (option C).** `harmonic-playlist` is small and was built for key analysis + playlist harmony coloring; extract just what's needed, extended to surface full beat grids. Git dependency keeps automixah standalone.
7. **MIDI-shaped control bus, not imperative macros.** The mixer exposes an addressed parameter bus per deck (like knobs on a deck). Automations are data timelines of `(param, value, time)` events — MIDI CC-shaped — produced via a `ControlSource` trait. V1 source: the timeline/preset engine. Future source: `MidiDevice` implementing the same trait, driving the identical bus (drop-in). Preset transitions are data files; rules pick them per transition. Future "dynamic" automations reacting to master-output analysis were explicitly deferred (context only).
8. **Zero-config default.** Create playlist → play. Planner auto-derives target BPM and auto-selects transitions by rules. Planning data (phase 3) exists to drive automations and to inform the user, never as a required step.
9. **Forward-only playback; seek permanently out of scope.** Skip (MediaSession next/prev or UI buttons) jumps between transition points.
10. **Track order is user-authored.** The engine plans transitions between adjacent tracks only — it never reorders.
11. **Firefox is supported.** Verified gaps and mitigations: File System Access pickers will never ship in Firefox (Mozilla: "partly harmful") → `<input type="file" multiple webkitdirectory>` fallback for import; OPFS (incl. sync access handles in workers) supported since FF 111; MediaSession supported (FF 82+); background audio on Firefox Android is a long-standing feature (friendlier than Chrome). No SharedArrayBuffer anywhere → no COOP/COEP header requirement; worker↔main communication is `postMessage` with transferable `ArrayBuffer`s.
12. **Chrome background playback trick still applies:** route the graph through `MediaStreamDestination` → `<audio>` element + MediaSession metadata so Chrome treats the page like a music player instead of suspending the hidden `AudioContext`.

## Relevant Files (Where)

### Existing code to extract from (read-only source: `/mnt/zed/repos/harmonic-playlist`)

| Source | What it contains | Destination |
|---|---|---|
| `crates/harmonic-playlist-core/src/feat/key/mod.rs` (634 lines) | `Key`, `KeyMode`, `KeyFormat`, `harmonic_distance()`, `parse()`, `format_with()`, CAMELOT tables + tests | `djcore/src/key.rs` |
| `crates/harmonic-playlist-core/src/feat/analysis/analyzer.rs` | `AudioAnalyzer` trait, `StratumAnalyzer`, `AnalyzerOutput` | `djcore/src/analyzer.rs` (extended) |
| `crates/harmonic-playlist-core/src/feat/analysis/decoder/mod.rs` | `AudioDecoder` trait, `DecodeAudio`, `DecodeError`, `DecoderRegistry` | `djcore/src/decoder/` |
| `crates/harmonic-playlist-core/src/feat/analysis/decoder/symphonia.rs` | symphonia-based decoder | `djcore/src/decoder/symphonia.rs` |
| `crates/harmonic-playlist-core/src/feat/analysis/mod.rs` | `AnalysisProgress` struct (progress-reporting pattern) | copied into automixah-ui analysis worker |
| `AGENTS.md` | style guide (wherror/error-stack, trait DI, pedantic clippy) | conventions for both new crates |

### New: `djcore` crate (separate repo, sibling of automixah)

```
djcore/
├── Cargo.toml
├── src/
│   ├── lib.rs              # re-exports; docs
│   ├── key.rs              # Key/KeyMode/KeyFormat/harmonic_distance (vendored)
│   ├── analyzer.rs         # AudioAnalyzer trait + StratumAnalyzer → full AnalysisResult
│   └── decoder/
│       ├── mod.rs          # AudioDecoder, DecodeAudio, DecoderRegistry
│       └── symphonia.rs    # symphonia implementation
└── tests/                  # fixture decode tests (mp3/flac/wav/ogg)
```

### New: `automixah` workspace (`/mnt/zed/repos/automixah`)

```
automixah/
├── Cargo.toml                    # workspace: members = ["crates/*"]
├── crates/
│   ├── automixah-engine/         # pure Rust, no wasm/browser deps
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── types.rs          # SessionTime (samples), TrackRef, TrackAnalysis (persisted shape)
│   │       ├── timeline/
│   │       │   ├── mod.rs        # SessionPlan, Segment, TransitionWindow
│   │       │   ├── planner.rs    # zero-config planner (target BPM, stretch modes)
│   │       │   └── rules.rs      # data-driven transition-selection rules
│   │       ├── control/
│   │       │   ├── mod.rs        # DeckId, ParamAddress, ControlEvent, ControlBus
│   │       │   └── source.rs     # ControlSource trait + TimelineSource impl
│   │       ├── automation/
│   │       │   ├── mod.rs        # preset loading (data files), curve generation
│   │       │   └── presets/      # crossfade.ron, low_cut_blend.ron, bass_swap.ron, cut.ron
│   │       └── render/
│   │           ├── mod.rs        # Renderer (pull-based render_until)
│   │           ├── resample.rs   # pitch-adjusted resampler
│   │           ├── wsola.rs      # pitch-preserving stretcher
│   │           ├── deck.rs       # deck DSP: gain, 3-band EQ, HPF/LPF biquads, smoothing
│   │           ├── mixer.rs      # deck sum + master soft-limiter
│   │           └── stretch_select.rs  # ≤±8% → resample; else WSOLA
│   └── automixah-ui/             # wasm crate (csr)
│       ├── Cargo.toml
│       ├── index.html            # Trunk entry, PWA manifest link
│       ├── manifest.webmanifest
│       ├── sw.js                 # service worker (app-shell caching only, v1)
│       └── src/
│           ├── main.rs           # Leptos mount
│           ├── workers/
│           │   ├── analysis_worker.rs   # decode→analyze→persist(OPFS)→progress
│           │   └── render_worker.rs     # owns Renderer; lookahead loop; PCM chunks out
│           ├── audio/
│           │   ├── playback.rs   # AudioContext chunk scheduler, gapless, <audio> sink
│           │   └── mediasession.rs
│           ├── import/
│           │   └── files.rs      # FS Access pickers + <input> fallback → OPFS by content hash
│           ├── bridge.rs         # typed worker messages (serde), transferable buffers
│           └── ui/
│               ├── lib.rs        # router/views
│               ├── library.rs
│               ├── playlist.rs
│               └── now_playing.rs
├── src-tauri/                    # phase 8 only
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   └── plugins/foreground-audio/ # Kotlin foreground-service plugin
├── .agents/RECORD.md             # seeded phase 1; updated end of implementation
└── .plans/auto-dj-music-player/plan.md
```

## Key Code Context (What)

Code that exists today and the implementation depends on. (harmonic-playlist user has not seen its code; these are the verified shapes.)

**1. Key math to vendor verbatim** — `feat/key/mod.rs`:

```rust
pub struct Key { pub root: u8 /* 0–11, C=0 */, pub mode: KeyMode }
pub enum KeyMode { Major, Minor }
pub enum KeyFormat { /* camelot etc. */ }

// Camelot-wheel harmonic distance, 0.0 (identical) … 1.0 (opposite):
pub fn harmonic_distance(&self, other: &Key) -> f32 {
    let self_number = i32::from(match self.mode {
        KeyMode::Major => CAMELOT_MAJOR[self.root as usize % 12],
        KeyMode::Minor => CAMELOT_MINOR[self.root as usize % 12],
    });
    let other_number = i32::from(/* same for other */);
    let same_mode = self.mode == other.mode;
    let wheel_distance = (self_number - other_number).unsigned_abs();
    let wheel_distance = wheel_distance.min(12 - wheel_distance) as f32;
    let raw_distance = if self_number == other_number {
        if same_mode { 0.0 } else { 0.5 }
    } else if same_mode { wheel_distance }
    else { wheel_distance + 0.5 };
    raw_distance / 6.5
}
// Also: Key::parse("Bbm") -> Option<Key>, Key::format_with(KeyFormat)
```

**2. Analyzer today — and the gap.** `feat/analysis/analyzer.rs`:

```rust
pub trait AudioAnalyzer: Send + Sync {
    fn name(&self) -> &'static str;
    fn analyze(&self, samples: &[f32], sample_rate: u32)
        -> Result<AnalyzerOutput, Report<AnalyzerError>>;
}
pub struct AnalyzerOutput { pub bpm: f32, pub key: Key, pub duration: Duration }

impl AudioAnalyzer for StratumAnalyzer {
    fn analyze(&self, samples: &[f32], sample_rate: u32) -> ... {
        let result = stratum_dsp::analyze_audio(samples, sample_rate, AnalysisConfig::default())...;
        Ok(AnalyzerOutput { bpm: result.bpm, key: Key::from(result.key),
                            duration: Duration::from_secs_f32(result.metadata.duration_seconds) })
        // ^ DISCARDS result.beat_grid — djcore must keep it.
    }
}
```

**3. stratum-dsp 1.0.0 public API** (verified from registry source; the reason djcore exists):

```rust
pub fn analyze_audio(samples: &[f32], sample_rate: u32, config: AnalysisConfig)
    -> Result<AnalysisResult, /* ... */>;

pub struct AnalysisResult {
    pub bpm: f32, pub bpm_confidence: f32,
    pub key: Key /* stratum's: Major(u8)|Minor(u8) */, pub key_confidence: f32, pub key_clarity: f32,
    pub beat_grid: BeatGrid, pub grid_stability: f32,
    pub metadata: AnalysisMetadata,
}
pub struct BeatGrid { pub downbeats: Vec<f32>, pub beats: Vec<f32>, pub bars: Vec<f32> } // seconds
pub struct AnalysisMetadata { pub duration_seconds: f32, pub sample_rate: u32, ... }
```

djcore's `AnalyzerOutput` becomes `{ bpm, key, duration, beat_grid: BeatGrid, grid_stability, bpm_confidence }` (all `serde`-serializable for OPFS persistence).

**4. Decoder abstraction to port** — `feat/analysis/decoder/mod.rs`:

```rust
pub struct DecodeAudio { pub samples: Vec<f32> /* mono, normalized */, pub sample_rate: u32 }
pub trait AudioDecoder: Send + Sync {
    fn name(&self) -> &'static str;
    fn supported_extensions(&self) -> &[&str];
    fn decode(&self, path: &CanonicalPath) -> Result<DecodeAudio, Report<DecodeError>>;
}
pub struct DecoderRegistry { /* register(Arc<dyn AudioDecoder>), get(ext), decode(path) */ }
```

djcore adaptation: decode must also work from in-memory bytes (`decode_bytes(&[u8])`) since OPFS gives `ArrayBuffer`s in the worker — keep the trait, add a bytes entry point; `CanonicalPath` dependency drops.

**5. Progress pattern to copy** — `AnalysisProgress { discovered, to_analyze, analyzed, complete, current_file, skipped }`, updated via `ArcSwap`, read by UI. In automixah the worker posts this shape as a message instead.

**6. Style conventions (both new crates).** From harmonic-playlist `AGENTS.md`: `wherror::Error` + `error_stack::Report` for all fallible ops; every external dependency behind a trait (`Send + Sync`, `name(&self) -> &'static str`, `Arc<dyn Trait>` service wrappers); `#[async_trait]` where async; workspace lints with `clippy::pedantic` warn + `missing_docs` warn. automixah-engine is `no_std`-adjacent plain std + serde (no tokio; workers are single-threaded message loops).

## Dependency Mappings

| Crate | Deps | Notes |
|---|---|---|
| `djcore` | `stratum-dsp = "1.0"` (**default features only — never enable `ml`/`ort`**), `symphonia 0.5` (features: mp3, flac, wav, ogg, aac), `error-stack`, `wherror`, `serde` | Verified: stratum-dsp 1.0.0 compiles clean to `wasm32-unknown-unknown` with default features (deps: rustfft, rayon, symphonia, serde, log). |
| `automixah-engine` | `djcore` (git), `serde` + `ron` (preset/plan data), `wherror`, `error-stack` | No tokio, no wasm-bindgen — must compile on any target incl. native for tests. Time-domain WSOLA needs no FFT dep. |
| `automixah-ui` | `leptos` (csr), `wasm-bindgen`, `js-sys`, `web-sys`, `wasm-bindgen-futures`, `serde`/`serde_json`, `gloo-events` (optional) | Built with **Trunk**. Workers as separate entry points (`render_worker.rs`, `analysis_worker.rs` via Trunk worker pipeline or dual `[[bin]]`). |
| `src-tauri` (phase 8) | `tauri 2`, `tauri-build`, `@tauri-apps/cli` (npm), Android SDK/NDK + `rustup target add aarch64-linux-android` | Wraps the existing web `dist/`. Foreground-audio plugin is thin Kotlin. |

## Implementation Algorithm (How)

### A. Analysis pipeline (djcore + analysis worker)

1. Import: pick files → read bytes → content hash (`xxhash64` of full bytes) as identity key → copy raw file to OPFS (`/tracks/<hash>.<ext>`). Dedupe by hash. Request `navigator.storage.persist()`.
2. For each new track (progress posted per file, `AnalysisProgress`-shaped): read bytes → `DecoderRegistry` (symphonia) → mono f32 + sample_rate → `analyze_audio(samples, sr, AnalysisConfig::default())` → serialize `TrackAnalysis { hash, bpm, bpm_confidence, key, duration, beat_grid, grid_stability, sample_rate, channels, format }` to OPFS `/analysis/<hash>.json` → post message → UI/planner update.
3. BPM octave normalization at *plan* time (not analysis time): fold bpm into `[90, 180)` by halving/doubling before any comparisons.

### B. Session planner (pure)

1. Inputs: ordered `Vec<TrackAnalysis>` (user order), optional user target BPM, engine sample rate.
2. Target BPM = user value OR median of octave-normalized BPMs.
3. Per track: `ratio = target / bpm_norm`. Stretch mode: `|ratio - 1| ≤ 0.08` → `Resample` (pitch-adjusted); else `Wsola` (pitch-preserving). Track records both; UI colorizes tracks needing >±8%.
4. Per adjacent pair (A, B): select transition preset via rule table (data, `rules.rs`), roughly:
   - `harmonic_distance(A.key, B.key) ≤ 1/6.5` (≤1 wheel step) and both modes Resample → `Crossfade` (32 beats);
   - distance > 0.5 (clashing) → `BassSwap` or `LowCutBlend`;
   - B needs WSOLA or `grid_stability` low → `Cut` (8 beats) or short `LowCutBlend`;
   - default → `LowCutBlend` (16 beats). Exact thresholds live in the data table.
5. Window placement: transition length L beats ending at A's last usable downbeat anchor (last downbeat minus a bar of margin); B's cue = B's first confident downbeat. Align so B's beat k=0 lands exactly on A's session beat position `(end_of_window - L)`; session times are in **samples (u64)** at engine rate for sample-exactness.
6. Tempo drift correction (variable-BPM tracks): ratio is re-anchored at each bar boundary only when accumulated drift exceeds 5 ms; otherwise hold (prevents pitch wobble). WSOLA mode absorbs drift natively per-window.
7. Fallback: no confident grid (`grid_stability` below threshold or empty beats) → plain time-aligned crossfade using bpm estimate; still plays.
8. Output `SessionPlan { session_bpm, segments: Vec<Segment { track_hash, src_start, session_start_samples, len_samples, stretch { mode, ratio, anchors } , transition: Option<TransitionPlan { window, preset_name, timeline: Vec<ControlEvent>, alignment } }> }`.
9. **Replan-on-append:** if a new track is appended while playing, re-plan only segments whose session start is beyond the render worker's already-rendered watermark; already-audible/ rendered audio is never invalidated.

### C. Control bus & automations (pure)

```rust
enum DeckId { A, B }                          // two decks; renderer assigns per segment
enum ParamAddress { Gain, EqLow, EqMid, EqHigh, HpfCutoff, LpfCutoff } // MIDI-CC-shaped, extensible
struct ControlEvent { deck: DeckId, param: ParamAddress, value: f32, session_time: SessionTime }

pub trait ControlSource: Send + Sync {
    fn name(&self) -> &'static str;
    /// Events with session_time <= until, in time order (pull model; renderer drives the clock).
    fn poll(&mut self, until: SessionTime) -> Vec<ControlEvent>;
}
```

- `TimelineSource` (v1): holds a preset-generated `Vec<ControlEvent>`; `poll` drains in order. Presets are RON data generating event lists, e.g. `Crossfade`: `Gain A: 1.0 → 0.0` and `Gain B: 0.0 → 1.0` over L beats with smooth (cosine-equal-power) curve sampled every 1/4 beat; `LowCutBlend`: plus `HpfCutoff B: 500 Hz → nominal` easing in; `BassSwap`: `EqLow` alternates fully between decks at bar downbeats over the window (the classic swap), gains crossing gently; `Cut`: both gains swap within a 1-beat fade at a downbeat (fade prevents clicks).
- Value semantics: normalized 0–1 for gains/EQ (EQ 0.5 = flat), Hz for cutoffs. Every param is a "knob on a deck" — anything expressible as `(param, value, time)` is legal, which is what makes a future `MidiDevice: ControlSource` a drop-in.

### D. Render engine (pure)

Pull-based; renderer state = plan + per-deck DSP chains + control sources + PCM caches.

```
render_until(session_time) -> Vec<f32>   // interleaved stereo f32, engine sample rate
```

1. Determine active segments (A playing; B once inside its transition window).
2. Per segment, produce stretched PCM: decode source (mono→duplicate to stereo) → time-scale. `Resample`: cubic-interpolated resampling at `ratio × (src_rate / engine_rate)` — exact-duration, pitch shifts. `Wsola`: frame 1536 samples (~32 ms @48k), synthesis hop = frame/2, analysis hop = `synth_hop / ratio`, best alignment by normalized cross-correlation in ±480-sample (~10 ms) search, Hann-weighted overlap-add — duration exact, pitch preserved.
3. Feed each deck's DSP: per-block (64-frame) processing; params smoothed with a one-pole filter (~10 ms) to kill zipper noise; biquads: low-shelf / peaking / high-shelf EQ, 2×2 cascade HPF/LPF (12 dB/oct).
4. Apply `ControlSource::poll` events at their exact sample times (interpolated within block).
5. Sum decks (overlap gains already staged by presets; additional −3 dB headroom during overlap) → master soft limiter (soft-knee polynomial, ceiling 0.99) → append to output.
6. PCM caching: decoded track PCM cached in-memory LRU with byte budget; WSOLA/resample done streaming, not cached (except a small ring of recent output for gapless chunk joins — chunk accounting is exact to the sample).
7. Throughput target ≥4× realtime native; wasm typically similar (bursts).

### E. Playback bridge (automixah-ui, wasm)

1. `AudioContext` (created/resumed on the Play click — autoplay policy). Engine rate = `ctx.sampleRate` (typically 48000), fixed for the session.
2. Render worker loop: keep `min_lookahead = 60 s`, `max_lookahead = 120 s` of session PCM buffered ahead of the playback head; refill on low-water mark (chunk granularity 2–5 s, transferred as `ArrayBuffer`s). Deadline: never let buffer fall below 5 s; render is incremental and resumable.
3. Scheduler: every ~250 ms, schedule the next `AudioBufferSourceNode` at an exact `AudioContext` time (gapless chain); track scheduled-until vs. rendered-until for backpressure.
4. Background sink: `MediaStreamAudioDestinationNode` → `new Audio(srcObject)` element → plays when tab hidden; MediaSession metadata (title = current track, artist = playlist name) + `nexttrack`/`previoustrack` handlers.
5. Skip = jump: send new playback position (next/prev transition start from the plan) to render worker (it may already have it buffered), cancel scheduled nodes at or after the jump, schedule from there. Play/pause = suspend/resume + worker pause. No seek API exists.
6. Import UI: Chromium → `showDirectoryPicker`/`showOpenFilePicker`; Firefox/other → `<input type="file" multiple webkitdirectory>`; both funnel the same bytes → OPFS by hash.

### F. Leptos UI (csr)

Views: **Library** (import, progress list), **Playlist builder** (ordered list, add/remove/reorder, BPM/key badges — Camelot format, optional target-BPM picker, red-ish tint for tracks whose required stretch exceeds ±8%), **Now Playing** (current track, next transition + preset name, timeline strip, play/pause/skip). Reactive state fed by worker messages (progress, plan, playback head).

### G. Tauri Android (phase 8)

Wrap `dist/` in Tauri 2; add Kotlin plugin registering a media foreground service (type `mediaPlayback`) started on play / stopped on pause so Android won't freeze background audio; build `aarch64-linux-android` debug APK for sideloading.

## Phases

1. **Scaffold** — Cargo workspace (`automixah-engine`, `automixah-ui`), wasm32 build check in CI/justfile, `.agents/RECORD.md` seeded, Trunk + Leptos CSR PWA shell (manifest, empty views, sw stub).
2. **Shared `djcore` crate** — new repo: vendor `Key`/`harmonic_distance`/Camelot tables (with tests); port `AudioDecoder`/`DecodeAudio`/`DecoderRegistry` + symphonia decoder (add bytes entry point); extend `AudioAnalyzer` to return full `AnalysisResult` incl. `BeatGrid`, `grid_stability`, `bpm_confidence` (serde). Verify wasm compile. automixah consumes via git dep.
3. **Timeline & planning** — engine types (`SessionPlan`, `Segment`, `TransitionWindow`, `TrackAnalysis`), zero-config planner (octave-normalized median BPM, ±8% stretch-mode decision, phrase-aligned windows from downbeats, drift re-anchoring), no-grid fallback, replan-on-append beyond render watermark. Pure + unit-tested.
4. **Control bus & automation engine** — `DeckId`/`ParamAddress`/`ControlEvent`/`ControlBus`, `ControlSource` trait + `TimelineSource`, four preset timelines as RON data, data-driven selection rules keyed on `harmonic_distance`/BPM gap/grid stability. Golden-curve tests + fake-source equivalence test.
5. **Render engine** — cubic resampler; WSOLA stretcher; deck DSP (biquad EQ/HPF/LPF, param smoothing); pull-based `Renderer::render_until` with deck sum + soft limiter; stretch-mode selection; LRU PCM cache; exact chunk accounting. Invariant + integration tests, throughput benchmark.
6. **Playback & workers** — AudioContext gapless chunk scheduler + `<audio>` background sink; render worker (60–120 s lookahead, low-water refill); analysis worker (decode→analyze→OPFS persist→progress); import (FS Access + `<input>` fallback, hash-dedup, persist request); MediaSession wiring with next/prev = transition jumps.
7. **Leptos UI** — Library/Playlist/Now-Playing views, worker-message reactive state, play/pause/skip controls.
8. **Tauri Android wrapper** — Tauri 2 project over `dist/`, Kotlin foreground-audio plugin, sideloadable debug APK.
9. **Verification** — acceptance criteria (below) verified one by one; Record Updates applied.

## Anti-Goals (Out of Scope)

- **No live-DJing**: no manual decks, jog wheels, manual EQ, hot cues.
- **No seeking** — permanently. Skip only between transition points.
- **No playlist reordering by the engine** — user order is authoritative; transitions only between adjacent tracks.
- **No dynamic/reactive automations** (master-output analysis driving transitions) — future work.
- **No MIDI device support in v1** — only the `ControlSource` abstraction that makes it possible.
- **No streaming services / network audio** — local files only (OPFS).
- **No Multi-Deck >2 simultaneity** — exactly two decks.
- **No Play Store publishing** — sideload only.
- **No recording/export of the rendered mix** — live playback only.
- **No COOP/COEP/SharedArrayBuffer** — by design (portability).

## Edge Cases & Gotchas

- **rayon is single-threaded under wasm32-unknown-unknown** → analysis is seconds-per-track; it must live in the analysis worker with progress, never on main thread, never blocking the render worker.
- **Chrome suspends hidden AudioContexts** → the `MediaStreamDestination` → `<audio>` + MediaSession sink is mandatory, not optional. Firefox Android is friendlier but the same sink is used everywhere (harmless).
- **Autoplay policy**: AudioContext needs a user gesture; create/resume inside the Play handler.
- **BPM octave errors** (170 detected as 85): fold to `[90,180)` before median/stretch; the ±8% rule operates on the folded ratio.
- **Variable-BPM grids**: constant ratio drifts against the beatgrid — re-anchor at bar boundaries only when accumulated drift >5 ms (prevents pitch wobble from continuous micro-adjustment).
- **No confident grid / analysis failure**: fall back to estimated-BPM time-aligned crossfade; never refuse to play. Track the fallback in `TrackAnalysis` so UI can indicate it.
- **Track shorter than transition window**: clamp window to fit between B's cue and A's last downbeat anchor; minimum viable overlap is 1 bar.
- **Zipper noise / clicks**: params smoothed (~10 ms one-pole) per 64-frame block; `Cut` preset uses a 1-beat fade; chunk joins are sample-accounted (no crossfade needed at joins).
- **Clipping during overlap**: presets stage gains; extra −3 dB overlap headroom; final soft-knee limiter at 0.99.
- **Sample-rate mismatch**: tracks decode at native rates; the resampler's effective ratio includes `src_rate/engine_rate`; engine rate is fixed at `AudioContext.sampleRate` for the whole session.
- **Firefox import**: no FS Access pickers ever (`<input>` fallback); OPFS sync handles in workers only.
- **Storage eviction**: request `navigator.storage.persist()` after first import; analysis JSON is tiny — the risk is the track blobs; surface OPFS usage in Library view (simple bytes-used readout).
- **Appending mid-play**: replan only beyond the render watermark; if the appended track is not yet analyzed, planner uses a provisional segment that is replaced when analysis lands (re-plan again beyond watermark; audible audio never changes).
- **MediaSession prev at session start**: no-op; next at end: ends session gracefully.

## Navigation Anchors

- **`djcore/src/analyzer.rs`** — `AudioAnalyzer` trait + `StratumAnalyzer`; the deliberate deviation from harmonic-playlist (keep `beat_grid`).
- **`djcore/src/key.rs`** — vendored Camelot math; parity tests anchor here.
- **`automixah-engine/src/timeline/planner.rs`** — `plan(tracks, options) -> SessionPlan`; the zero-config entry point.
- **`automixah-engine/src/control/source.rs`** — `ControlSource` trait; future MIDI hooks in here.
- **`automixah-engine/src/render/mod.rs`** — `Renderer::render_until(session_time)`; the single pull API everything else serves.
- **`automixah-ui/src/workers/render_worker.rs`** — lookahead loop, watermark tracking, skip-jump handling.
- **`automixah-ui/src/audio/playback.rs`** — gapless scheduler + background sink; the only place `AudioContext` is touched.
- **`automixah-ui/src/import/files.rs`** — browser-branching (FS Access vs `<input>`).

## Test Strategies

| Phase | How to verify |
|---|---|
| 2 (djcore) | Unit: Camelot distance matrix (copy expected values from harmonic-playlist tests); `Key::parse` round-trips. Integration: decode real mp3/flac/wav/ogg fixtures, assert mono f32 non-empty, sample_rate > 0; `analyze` on a click-track fixture (beats at exact 0.5 s intervals @120 BPM) asserts bpm ≈ 120 and `beat_grid.downbeats` populated. `cargo check --target wasm32-unknown-unknown` green. |
| 3 (planner) | Unit with synthetic beatgrids: transition windows land on downbeats; ±8% band selects Resample vs Wsola; median BPM ignores octave outliers; drift re-anchor triggers only past 5 ms; replan-on-append never touches segments before watermark. Integration: mixed-BPM fixture playlist → full `SessionPlan` snapshot test. |
| 4 (control) | Golden-fixture unit tests: each preset → exact `Vec<ControlEvent>` curve (sample every 1/4 beat). Equivalence test: `FakeSource` emitting the same events event-by-event drives an identical bus state to `TimelineSource` (proves trait abstraction). BassSwap: render two synthetic bass-heavy tracks, FFT the output across the boundary, assert low-band energy swaps decks per bar. |
| 5 (render) | Unit: resampler duration exact (`len_out == round(len_in / ratio)`), pitch shift ≈ ratio (spectral-centroid ratio on sine fixture); WSOLA duration exact, sine pitch preserved (dominant bin within 1%); param smoothing removes zipper (max inter-block gain delta bound). Integration: full A→B transition render — decks overlap, curves applied, no sample exceeds limiter ceiling, zero NaN. Bench: ≥4× realtime on a representative 3-min fixture (criterion). |
| 6 (playback) | Soak test (headless chrome / wasm-bindgen-test in Node): simulated slow render (fault injection: worker delays 3× chunk time) — no underrun; skip-jump cancels scheduled nodes correctly (assert no double-audio via scheduler state). 500-file import: main-thread long-task counter stays < 16 ms stalls. Manual E2E on device (below). |
| 7 (UI) | Leptos component tests where cheap; primarily manual review against the phase description. |
| 8 (Tauri) | Manual: sideload APK, screen off 30 min, next/prev respond. |
| 9 (Verification) | Each acceptance criterion exercised as its own task; see below. |

## Acceptance Criteria

1. Transition boundaries land within ±1 sample of planned beat positions (fixture tests with synthetic beatgrids).
2. Pitch-adjusted: duration scales exactly, pitch shifts proportionally (spectral centroid ratio test). WSOLA: duration exact, pitch preserved. Both ≥4× realtime.
3. Automation timelines produce golden-fixture parameter curves on the bus; `BassSwap` measurably swaps low-band energy across the boundary.
4. Zero-config flow: fresh playlist → play with no configuration produces a continuous, correctly planned mix.
5. 20-track continuous playback: no underruns under fault-injected slow renders.
6. Android Chrome + Firefox, screen off: 30+ min playback; MediaSession next/prev < ~200 ms.
7. 500-file import: no main-thread stalls; analysis concurrent with playback.

## Record Updates

Written to `.agents/RECORD.md` at end of implementation (verbatim entries):

- "automixah is a web-first (wasm) auto-DJ player; all mixing DSP runs in Rust/wasm workers; the browser does only PCM playback and UI."
- "The mixer is driven by an addressed, MIDI-shaped control bus; automations are data timelines of control events behind a `ControlSource` trait (future MIDI sources implement the same trait)."
- "Time-scaling supports pitch-adjusted resampling (default) and pitch-preserving WSOLA; default heuristic: ≤±8% stretch uses pitch-adjusted."
- "Default UX is zero-config: playlist → play; target BPM and transitions are auto-selected by rules, user overrides optional."
- "Playback is forward-only; seeking is permanently out of scope. Skip moves between transition points."
- "Track order is user-authored; the engine plans transitions between adjacent tracks only."
- "Analysis lives in the shared `djcore` crate (extracted from harmonic-playlist, extended to surface full beat grids), consumed via git dependency."
- "File import supports both FS Access pickers (Chromium) and `<input>`-based fallback (Firefox); tracks persist to OPFS."
