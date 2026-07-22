//! Deterministic component-gallery fixtures for interaction and visual regression.

use crate::{
    components::{
        ComponentPresentation, FaultBannerModel, FaultBannerState, GainFaderState, MeterLevel,
        MeterRepaintScheduler, ParameterControlState, PluginSlotState, PluginSlotStatus,
        RackCardState, RackCardStatus, ScenePadState, ScenePadStatus, SegmentedControlState,
        StereoMeter, SystemStatus, SystemStatusState, TruncatedLabel, WorkerHealth,
        WorkerHealthState,
    },
    design::{FocusOrder, Interaction, MotionPreference},
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

/// Pixel density used for a deterministic visual snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotScale {
    /// Standard-density 1x rendering.
    OneX,
    /// Retina 2x rendering.
    TwoX,
}

impl SnapshotScale {
    /// Returns the scale factor expected by a snapshot renderer.
    #[must_use]
    pub const fn factor(self) -> u8 {
        match self {
            Self::OneX => 1,
            Self::TwoX => 2,
        }
    }
}

/// Required non-color visual review modes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotReview {
    /// Normal alpha palette view.
    FullColor,
    /// Grayscale review proves labels, icons, and boundaries carry state without hue.
    Grayscale,
    /// Deterministic color-deficiency simulation review.
    ColorDeficiency,
}

/// A stable gallery snapshot request that contains no wall-clock or font/logo asset dependency.
#[derive(Clone, Debug, PartialEq)]
pub struct GallerySnapshotFixture {
    /// Stable snapshot file stem, suitable for a renderer harness.
    pub id: &'static str,
    /// Logical viewport width.
    pub width: u16,
    /// Logical viewport height.
    pub height: u16,
    /// Device pixel density.
    pub scale: SnapshotScale,
    /// Non-color review treatment.
    pub review: SnapshotReview,
    /// Reduced motion is on for snapshots so capture never contains a transient frame.
    pub motion: MotionPreference,
    /// Stable gallery data.
    pub gallery: ComponentGallery,
    /// Long visual name with an untruncated accessible name.
    pub long_name: TruncatedLabel,
    /// Explicit interaction coverage for all actionable gallery component families.
    pub interactions: Vec<ComponentPresentation<GalleryInteractionTarget>>,
    /// Stereo meter sample used by the renderer to exercise the 30 Hz meter contract.
    pub stereo_meter: StereoMeter,
}

/// A component target used solely to make gallery interaction snapshots deterministic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GalleryInteractionTarget {
    /// Rack-card interaction sample.
    RackCard,
    /// Plug-in-slot interaction sample.
    PluginSlot,
    /// Gain control interaction sample.
    GainFader,
    /// Parameter control interaction sample.
    ParameterControl,
    /// Segmented-control interaction sample.
    SegmentedControl,
    /// Scene-pad interaction sample.
    ScenePad,
    /// Dismissible fault-banner action sample.
    FaultBanner,
}

impl crate::design::AccessibleComponent for GalleryInteractionTarget {
    fn accessibility(&self, focus_order: FocusOrder) -> crate::design::AccessibilityNode {
        let name = match self {
            Self::RackCard => "Rack card",
            Self::PluginSlot => "Plug-in slot",
            Self::GainFader => "Gain fader",
            Self::ParameterControl => "Parameter control",
            Self::SegmentedControl => "Segmented control",
            Self::ScenePad => "Scene pad",
            Self::FaultBanner => "Fault banner",
        };
        let mut node =
            crate::design::AccessibilityNode::named(crate::design::AccessibilityRole::Button, name);
        node.focus_order = Some(focus_order);
        node.keyboard_actions = &[
            crate::design::KeyboardAction::Toggle,
            crate::design::KeyboardAction::Open,
            crate::design::KeyboardAction::Close,
        ];
        node
    }
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

    /// Returns 1x/2x, full-color/grayscale/color-deficiency fixture requests.
    ///
    /// Renderers must snapshot these exact data models using the bundled OFL Sora/Space Mono
    /// families and retain the text-mark alpha fallback until production logo assets are supplied.
    #[must_use]
    pub fn snapshot_fixtures() -> Vec<GallerySnapshotFixture> {
        [SnapshotScale::OneX, SnapshotScale::TwoX]
            .into_iter()
            .flat_map(|scale| {
                [
                    ("full-color", SnapshotReview::FullColor),
                    ("grayscale", SnapshotReview::Grayscale),
                    ("color-deficiency", SnapshotReview::ColorDeficiency),
                ]
                .into_iter()
                .map(move |(review_name, review)| GallerySnapshotFixture {
                    id: match (scale, review_name) {
                        (SnapshotScale::OneX, "full-color") => "gallery-1x-full-color",
                        (SnapshotScale::OneX, "grayscale") => "gallery-1x-grayscale",
                        (SnapshotScale::OneX, "color-deficiency") => "gallery-1x-color-deficiency",
                        (SnapshotScale::TwoX, "full-color") => "gallery-2x-full-color",
                        (SnapshotScale::TwoX, "grayscale") => "gallery-2x-grayscale",
                        (SnapshotScale::TwoX, "color-deficiency") => "gallery-2x-color-deficiency",
                        _ => unreachable!("the fixture inventory has fixed variants"),
                    },
                    width: 1180,
                    height: 720,
                    scale,
                    review,
                    motion: MotionPreference::Reduced,
                    gallery: Self::fixtures(),
                    long_name: TruncatedLabel::new(
                        "Lead Vocal Parallel Compression and Saturation Return",
                        24,
                    ),
                    interactions: gallery_interactions(),
                    stereo_meter: StereoMeter {
                        left: MeterLevel::from_peak_dbfs(-3.0),
                        right: MeterLevel::from_peak_dbfs(-1.0),
                        repaint: MeterRepaintScheduler::default(),
                    },
                })
            })
            .collect()
    }
}

fn gallery_interactions() -> Vec<ComponentPresentation<GalleryInteractionTarget>> {
    use GalleryInteractionTarget::{
        FaultBanner, GainFader, ParameterControl, PluginSlot, RackCard, ScenePad, SegmentedControl,
    };
    [
        (RackCard, Interaction::Default),
        (RackCard, Interaction::Hover),
        (PluginSlot, Interaction::Pressed),
        (GainFader, Interaction::Focused),
        (ParameterControl, Interaction::Disabled),
        (SegmentedControl, Interaction::Selected),
        (ScenePad, Interaction::Loading),
        (ScenePad, Interaction::Success),
        (FaultBanner, Interaction::Fault),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, (component, interaction))| {
        let mut presentation = ComponentPresentation::new(
            component,
            FocusOrder(u16::try_from(index).unwrap_or(u16::MAX)),
        );
        presentation.set_interaction(interaction);
        presentation
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use crate::{
        components::COMPONENT_COUNT,
        design::{Interaction, MotionPreference},
    };

    use super::{ComponentGallery, SnapshotReview, SnapshotScale};

    #[test]
    fn fixtures_cover_every_component_family() {
        let gallery = ComponentGallery::fixtures();
        let represented = 10;
        assert_eq!(represented, COMPONENT_COUNT);
        assert!(!gallery.systems.is_empty());
        assert!(!gallery.slots.is_empty());
        assert!(!gallery.parameters.is_empty());
    }

    #[test]
    fn deterministic_snapshots_cover_scale_review_and_all_interaction_states() {
        let fixtures = ComponentGallery::snapshot_fixtures();
        assert_eq!(fixtures.len(), 6);
        assert!(
            fixtures
                .iter()
                .any(|fixture| fixture.scale == SnapshotScale::OneX)
        );
        assert!(
            fixtures
                .iter()
                .any(|fixture| fixture.scale == SnapshotScale::TwoX)
        );
        assert!(
            fixtures
                .iter()
                .any(|fixture| fixture.review == SnapshotReview::Grayscale)
        );
        assert!(
            fixtures
                .iter()
                .any(|fixture| fixture.review == SnapshotReview::ColorDeficiency)
        );
        assert!(
            fixtures
                .iter()
                .all(|fixture| fixture.motion == MotionPreference::Reduced)
        );
        let interactions = &fixtures[0].interactions;
        for state in [
            Interaction::Default,
            Interaction::Hover,
            Interaction::Pressed,
            Interaction::Focused,
            Interaction::Disabled,
            Interaction::Selected,
            Interaction::Loading,
            Interaction::Success,
            Interaction::Fault,
        ] {
            assert!(
                interactions
                    .iter()
                    .any(|fixture| fixture.interaction == state)
            );
        }
    }
}
