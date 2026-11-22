//! Windows real-time playback hygiene helpers.
//!
//! - MMCSS `"Pro Audio"` thread elevation (`AvSetMmThreadCharacteristicsW`).
//! - Full-memory RAM page locking (`VirtualLock`) for pre-processed audio buffers.
//! - Configurable Audirvana-style scheduling optimization tiers (Normal / High / Extreme).

/// Thread/process scheduling priority tier inspired by Audirvana's KS optimization levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OptimizationLevel {
    Normal,
    #[default]
    High,
    Extreme,
}
