//! On-the-fly upsampling & noise-shaping algorithm selection driven by source attributes.
//!
//! Switches cleanly between curated audiophile filter families depending on input rate
//! (1x 44.1/48 kHz vs Nx HiRes 88.2/96/192 kHz), bit depth, source kind, and optional
//! apodization / fake-HiRes ultrasonic analysis (finalized in INV-03).

use ap_core::format::{StreamSpec, TargetDspRate};

/// Curated audiophile PCM filter archetypes (shortlisted in INV-02, formalized in INV-03).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterArchetype {
    GaussApod4M,
    SincSteep4M,
    GaussLongLp,
    GaussHiresIp,
    GaussHiresMp,
    ExtSharpLp,
}

/// Curated 8fS (`352.8 / 384 kHz`) noise shapers & dithers (finalized in INV-04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseShaperProfile {
    Ns5Multibit,
    Lns15Smooth,
    PggbOptimal,
    TpdfDither,
}

/// Chosen DSP profile for the current track/stream.
#[derive(Debug, Clone, Copy)]
pub struct SelectedDspProfile {
    pub target_rate: TargetDspRate,
    pub filter: FilterArchetype,
    pub shaper: NoiseShaperProfile,
}

/// Select the optimal target rate, filter, and noise shaper on the fly for a source stream.
pub fn select_dsp_profile(_spec: StreamSpec) -> SelectedDspProfile {
    todo!("Implement source-aware on-the-fly DSP selection policy (INV-03 / INV-04)")
}
