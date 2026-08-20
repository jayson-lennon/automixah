//! automixah-engine — the pure-Rust auto-DJ engine.
//!
//! Browser-free planning, automation, and rendering logic:
//!
//! - [`timeline`] — session planning: target-BPM selection, stretch
//!   decisions, and transition-window placement on phrase boundaries.
//! - [`control`] — the addressed, MIDI-shaped parameter bus that drives
//!   the mixer.
//! - [`automation`] — data-driven control-event timelines (presets).
//! - [`render`] — the pull-based mix render engine.
//! - [`mixdown`] — the offline mixdown pipeline: playlist snapshot in,
//!   WAV out, atomic output, cooperative progress and cancellation.
//!
//! Analysis and decoding come from the workspace `djcore` crate
//! (`djcore::analyzer`, `djcore::decoder`), re-exported here for
//! downstream consumers.

pub mod automation;
pub mod control;
pub mod mixdown;
pub mod render;
pub mod timeline;

pub use djcore;
