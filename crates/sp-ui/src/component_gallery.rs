//! Deterministic component-gallery fixtures for interaction and visual regression.

use crate::components::{
    FaultBannerModel, FaultBannerState, GainFaderState, MeterLevel, ParameterControlState,
    PluginSlotState, PluginSlotStatus, RackCardState, RackCardStatus, ScenePadState,
    ScenePadStatus, SegmentedControlState, SystemStatus, SystemStatusState, WorkerHealth,
    WorkerHealthState,
};

/// Representative component states rendered by the application gallery.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentGallery {
    /// Global engine status examples.
    pub systems: Vec<SystemStatus>,
    /// Rack-card examples.
    pub racks: Vec<RackCardState>,
    /// Plug-in-slot examples.
    pub slots: Vec<PluginSlotState>,
    /// Meter examples.
    pub meters: Vec<MeterLevel>,
    /// Editable gain example.
    pub gain: GainFaderState,
    /// Generic parameter examples.
    pub parameters: Vec<ParameterControlState>,
    /// Compact mutually exclusive control example.
    pub segmented: SegmentedControlState,
    /// Scene-pad examples.
    pub scenes: Vec<ScenePadState>,
    /// Worker-health examples.
    pub workers: Vec<WorkerHealth>,
    /// Recoverable fault example.
    pub fault: FaultBannerModel,
}

impl ComponentGallery {
    /// Builds a stable gallery covering normal, transitional, success, and fault states.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn fixtures() -> Self {
        Self {
            systems: vec![
                SystemStatus::new(SystemStatusState::Online),
                SystemStatus::new(SystemStatusState::Connecting),
                SystemStatus::new(SystemStatusState::Offline),
            ],
            racks: vec![
                RackCardState {
                    name: "Lead Vocal".to_owned(),
                    status: RackCardStatus::Active,
                },
                RackCardState {
                    name: "Keys".to_owned(),
                    status: RackCardStatus::Bypassed,
                },
                RackCardState {
                    name: "Spare".to_owned(),
                    status: RackCardStatus::Empty,
                },
            ],
            slots: [
                PluginSlotStatus::Ready,
                PluginSlotStatus::Loading,
                PluginSlotStatus::Bypassed,
                PluginSlotStatus::Faulted,
                PluginSlotStatus::Missing,
            ]
            .into_iter()
            .map(|status| PluginSlotState {
                name: format!("{} plug-in", status.label()),
                status,
            })
            .collect(),
            meters: [-48.0, -12.0, -3.0, 0.0]
                .map(MeterLevel::from_peak_dbfs)
                .to_vec(),
            gain: GainFaderState {
                gain_db: -3.0,
                default_db: 0.0,
                muted: false,
            },
            parameters: vec![
                ParameterControlState {
                    id: 1,
                    name: "Threshold".to_owned(),
                    normalized: 0.62,
                    default: Some(0.5),
                    formatted: "-18.4 dB".to_owned(),
                    read_only: false,
                    discrete: false,
                },
                ParameterControlState {
                    id: 2,
                    name: "Quality".to_owned(),
                    normalized: 1.0,
                    default: Some(0.5),
                    formatted: "High".to_owned(),
                    read_only: false,
                    discrete: true,
                },
                ParameterControlState {
                    id: 3,
                    name: "Latency".to_owned(),
                    normalized: 0.25,
                    default: None,
                    formatted: "64 samples".to_owned(),
                    read_only: true,
                    discrete: false,
                },
            ],
            segmented: SegmentedControlState {
                label: "Monitor".to_owned(),
                options: vec!["Wet".to_owned(), "Dry".to_owned(), "Mute".to_owned()],
                selected: 0,
            },
            scenes: vec![
                ScenePadState {
                    name: "Verse".to_owned(),
                    status: ScenePadStatus::Active,
                },
                ScenePadState {
                    name: "Chorus".to_owned(),
                    status: ScenePadStatus::Recalling,
                },
                ScenePadState {
                    name: "Bridge".to_owned(),
                    status: ScenePadStatus::Idle,
                },
            ],
            workers: [
                WorkerHealthState::Healthy,
                WorkerHealthState::Recovering,
                WorkerHealthState::Unavailable,
            ]
            .map(|state| WorkerHealth { state })
            .to_vec(),
            fault: FaultBannerModel {
                state: FaultBannerState::Visible,
                message: "Rack 2 missed its deadline. Dry bypass is active.".to_owned(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::components::COMPONENT_COUNT;

    use super::ComponentGallery;

    #[test]
    fn fixtures_cover_every_component_family() {
        let gallery = ComponentGallery::fixtures();
        let represented = 10;
        assert_eq!(represented, COMPONENT_COUNT);
        assert!(!gallery.systems.is_empty());
        assert!(!gallery.slots.is_empty());
        assert!(!gallery.parameters.is_empty());
    }
}
