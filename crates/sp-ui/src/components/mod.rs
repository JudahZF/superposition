//! Renderer-independent UI component state models.

/// Number of component model families exported by this module.
pub const COMPONENT_COUNT: usize = 10;

/// Explicit accessibility semantics for every component model.
mod accessibility;
/// Gain, parameter, and segmented control state.
pub mod controls;
/// Deterministic text truncation that preserves accessible names.
pub mod label;

/// Fault banner state.
pub mod fault_banner;
/// Audio meter state.
pub mod meter;
/// Plug-in slot state.
pub mod plugin_slot;
/// Shared explicit interaction and focus presentation wrapper.
pub mod presentation;
/// Rack card state.
pub mod rack_card;
/// Scene pad state.
pub mod scene_pad;
/// System status state.
pub mod system_status;
/// Worker health state.
pub mod worker_health;

pub use controls::{GainFaderState, ParameterControlState, SegmentedControlState};
pub use fault_banner::{FaultBannerModel, FaultBannerState};
pub use label::TruncatedLabel;
pub use meter::{
    METER_REPAINT_HZ, METER_REPAINT_INTERVAL, MeterLevel, MeterRepaintScheduler, MeterState,
    StereoMeter,
};
pub use plugin_slot::{PluginSlotState, PluginSlotStatus};
pub use presentation::ComponentPresentation;
pub use rack_card::{RackCardState, RackCardStatus};
pub use scene_pad::{ScenePadState, ScenePadStatus};
pub use system_status::{SystemStatus, SystemStatusState};
pub use worker_health::{WorkerHealth, WorkerHealthState};
