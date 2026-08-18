//! Session planning: target-BPM selection, stretch decisions, and
//! transition-window placement on phrase boundaries.

pub mod placement;
pub mod plan;
pub mod replan;
pub mod stretch;
pub mod tempo;
pub mod types;

pub use types::{
    PresetName, Segment, SessionPlan, SessionTime, StretchDecision, StretchMode, TrackAnalysis,
    TrackHash, TransitionPlan, TransitionWindow,
};
