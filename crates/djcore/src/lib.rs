//! djcore — shared DJ analysis core.
//!
//! Extracted from `harmonic-playlist` and extended to surface full beat
//! grids. Provides:
//!
//! - [`key`] — musical key representation, Camelot formatting, and
//!   harmonic distance.
//! - [`decoder`] — trait-abstracted audio decoding ([`decoder::AudioDecoder`])
//!   with a symphonia backend and an extension-based
//!   [`decoder::DecoderRegistry`].
//! - [`analyzer`] — BPM, key, and beat-grid analysis via
//!   [`analyzer::AudioAnalyzer`] / [`analyzer::StratumAnalyzer`].
//!
//! All output types are `serde`-serializable so analyses can be persisted
//! (e.g. to OPFS in the automixah web app).

pub mod analyzer;
pub mod decoder;
pub mod key;

pub use key::{Key, KeyMode};
