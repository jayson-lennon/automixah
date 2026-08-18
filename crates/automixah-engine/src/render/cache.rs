//! Decoded-PCM cache: a byte-budgeted LRU over stretched session PCM.
//!
//! The render worker decodes a track once, stretches it to session
//! rate per its [`StretchDecision`], and stores the result here.
//! The cache enforces a byte budget by evicting least-recently-used
//! tracks; exact chunk accounting lets the caller know how many
//! bytes a cache hit "cost" (zero) versus a miss (full stretched
//! PCM), so lookahead logic can reason about render-time budgets.

use std::collections::HashMap;

use crate::timeline::types::{StretchDecision, TempoStrategy, TrackHash};

/// An entry's bookkeeping after a cache operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkAccounting {
    /// Bytes of PCM the cache holds after the operation.
    pub bytes_held: usize,
    /// Whether this operation was a hit (no decode/stretch needed).
    pub hit: bool,
}

/// LRU cache of stretched PCM keyed by track hash.
///
/// `f32` samples at session rate; the budget counts sample bytes.
pub struct PcmCache {
    entries: HashMap<TrackHash, (Vec<f32>, u64)>,
    /// Monotonic use counter for LRU ordering.
    clock: u64,
    budget_bytes: usize,
    bytes_held: usize,
}

impl PcmCache {
    /// Creates a cache holding at most `budget_bytes` of PCM.
    #[must_use]
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            clock: 0,
            budget_bytes,
            bytes_held: 0,
        }
    }

    /// Looks up pre-stretched PCM, refreshing recency.
    /// True if `hash` is resident (no LRU accounting side effect).
    #[must_use]
    pub fn is_resident(&self, hash: &TrackHash) -> bool {
        self.entries.contains_key(hash)
    }

    pub fn get(&mut self, hash: &TrackHash) -> Option<&[f32]> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.entries.get_mut(hash)?;
        entry.1 = clock;
        Some(&entry.0)
    }

    /// Inserts stretched PCM, evicting LRU entries until the budget
    /// is satisfied. Returns the post-insert accounting.
    ///
    /// A single track larger than the whole budget is never inserted
    /// (returns accounting with `hit: false` and no insert).
    pub fn insert(&mut self, hash: TrackHash, pcm: Vec<f32>) -> ChunkAccounting {
        let bytes = pcm.len() * 4;
        if bytes > self.budget_bytes {
            return ChunkAccounting {
                bytes_held: self.bytes_held,
                hit: false,
            };
        }
        if let Some((old, _)) = self.entries.insert(hash, (pcm, self.clock)) {
            self.bytes_held -= old.len() * 4;
        } else {
            self.bytes_held += bytes;
        }
        self.evict_to_budget();
        ChunkAccounting {
            bytes_held: self.bytes_held,
            hit: false,
        }
    }

    /// Records a hit's accounting (no state change beyond recency).
    pub fn accounting_for_hit(&self) -> ChunkAccounting {
        ChunkAccounting {
            bytes_held: self.bytes_held,
            hit: true,
        }
    }

    /// Bytes currently held.
    #[must_use]
    pub fn bytes_held(&self) -> usize {
        self.bytes_held
    }

    /// Evicts least-recently-used entries until within budget.
    fn evict_to_budget(&mut self) {
        while self.bytes_held > self.budget_bytes {
            let Some((victim, (_, _))) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, v)| (k.clone(), (v.0.clone(), v.1)))
            else {
                break;
            };
            let bytes = self.entries.remove(&victim).map(|(p, _)| p.len() * 4);
            if let Some(b) = bytes {
                self.bytes_held = self.bytes_held.saturating_sub(b);
            }
        }
    }
}

/// Chooses the stretch mode for a track given its folded BPM and the
/// session target: within ±8% → resample (pitch-adjusted), else
/// WSOLA (pitch-preserving).
///
/// This mirrors the planner's decision; the render worker uses it to
/// select the stretcher for a cache miss.
#[must_use]
pub fn stretch_mode_for(folded_bpm: f32, session_bpm: f32) -> StretchDecision {
    let ratio = session_bpm / folded_bpm;
    let out_of_band = (ratio - 1.0).abs() > 0.08;
    StretchDecision {
        strategy: TempoStrategy::SessionBpm,
        mode: if out_of_band {
            crate::timeline::types::StretchMode::Wsola
        } else {
            crate::timeline::types::StretchMode::Resample
        },
        ratio,
        out_of_comfort_band: out_of_band,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(n: u8) -> TrackHash {
        TrackHash(format!("h{n}"))
    }

    fn pcm(n_samples: usize) -> Vec<f32> {
        vec![0.5; n_samples]
    }

    #[test]
    fn insert_then_get_is_a_hit() {
        // Given a cache with room.
        let mut c = PcmCache::new(1_000_000);

        // When inserting and getting.
        c.insert(hash(1), pcm(100));
        let got = c.get(&hash(1));

        // Then the PCM is returned.
        assert_eq!(got.expect("present").len(), 100);
    }

    #[test]
    fn lru_entry_is_evicted_first() {
        // Given a budget fitting two small tracks.
        let mut c = PcmCache::new(100 * 4 * 2);

        // When inserting three, touching the first, then inserting
        // a third that forces eviction.
        c.insert(hash(1), pcm(100));
        c.insert(hash(2), pcm(100));
        c.get(&hash(1)); // refresh 1
        c.insert(hash(3), pcm(100));

        // Then track 2 (least recently used) was evicted, 1 remains.
        assert!(c.get(&hash(2)).is_none());
        assert!(c.get(&hash(1)).is_some());
    }

    #[test]
    fn oversized_track_is_not_inserted() {
        // Given a tiny budget.
        let mut c = PcmCache::new(10);

        // When inserting a larger track.
        let acc = c.insert(hash(1), pcm(100));

        // Then nothing is held and the accounting says miss.
        assert_eq!(acc.bytes_held, 0);
        assert!(!acc.hit);
        assert!(c.get(&hash(1)).is_none());
    }

    #[test]
    fn bytes_held_reflects_entries() {
        // Given a cache and one track.
        let mut c = PcmCache::new(1_000_000);
        c.insert(hash(1), pcm(250));

        // Then bytes held counts f32 sample bytes.
        assert_eq!(c.bytes_held(), 250 * 4);
    }

    #[test]
    fn within_band_uses_resample() {
        // Given BPMs 4% apart.
        let d = stretch_mode_for(120.0, 124.8);

        // Then the mode is resample (pitch-adjusted), in band.
        assert_eq!(d.mode, crate::timeline::types::StretchMode::Resample);
        assert!(!d.out_of_comfort_band);
    }

    #[test]
    fn out_of_band_uses_wsola() {
        // Given BPMs 20% apart.
        let d = stretch_mode_for(100.0, 120.0);

        // Then the mode is WSOLA (pitch-preserving), out of band.
        assert_eq!(d.mode, crate::timeline::types::StretchMode::Wsola);
        assert!(d.out_of_comfort_band);
    }
}
