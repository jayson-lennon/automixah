# automixah — Project Record

This file records **current** facts about how the application works — factual, scoped statements about the present state, never future intent.

Entries are added at the end of implementation phases via the "Record Updates" mechanism described in `.plans/auto-dj-music-player/plan.md`.

- The mixer is driven by an addressed, MIDI-shaped control bus; automations are data timelines of control events behind a `ControlSource` trait (future MIDI sources implement the same trait).
- Default UX is zero-config: playlist → play; target BPM and transitions are auto-selected by rules, user overrides optional.
- Playback is forward-only; seeking is permanently out of scope. Skip moves between transition points.
- Track order is user-authored; the engine plans transitions between adjacent tracks only.
- Analysis lives in the shared `djcore` crate (extracted from harmonic-playlist, extended to surface full beat grids); djcore is a workspace member crate of automixah (`crates/djcore`), not a separate repository.
- automixah is a Rust-only terminal application; the Leptos/wasm UI and web build machinery were removed. The primary interface at this stage is offline rendering: the CLI mixes the given tracks and writes a WAV.
- Track inputs are absolute paths passed via repeated `--track` flags in the given order; every invocation hashes, decodes, and analyzes them from scratch — no library, cache, or persistence exists between runs.
- Time-scaling supports pitch-adjusted resampling (default) and pitch-preserving WSOLA; default heuristic: ≤±8% stretch uses pitch-adjusted; decode, stretch, render, and WAV output are stereo (interleaved); analysis uses a mono downmix.
- Transitions overlap: the incoming track cues at the window start (grid-snapped when confident), and the outgoing track's outro plays under the incoming track's intro; session length reflects the overlap.
- Automations are authored as RON `TransitionSpec` pairs addressed by deck role (outgoing/incoming); the default is a 16-bar equal-power fade, and `--automation <file>` loads a custom pair applied to every transition.
