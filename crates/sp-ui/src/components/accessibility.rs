//! Accessibility contracts for all renderer-independent component models.

use crate::{
    components::{
        FaultBannerModel, FaultBannerState, GainFaderState, ParameterControlState, PluginSlotState,
        PluginSlotStatus, RackCardState, ScenePadState, ScenePadStatus, SegmentedControlState,
        SystemStatus, SystemStatusState, WorkerHealth, WorkerHealthState,
    },
    design::{
        AccessibilityNode, AccessibilityRange, AccessibilityRole, AccessibleComponent, FocusOrder,
        KeyboardAction,
    },
};

const COLLECTION_ACTIONS: &[KeyboardAction] = &[
    KeyboardAction::Previous,
    KeyboardAction::Next,
    KeyboardAction::Toggle,
    KeyboardAction::Open,
];
const ADJUSTABLE_ACTIONS: &[KeyboardAction] = &[
    KeyboardAction::Decrease,
    KeyboardAction::Increase,
    KeyboardAction::FineDecrease,
    KeyboardAction::FineIncrease,
    KeyboardAction::Minimum,
    KeyboardAction::Maximum,
    KeyboardAction::Reset,
];

impl AccessibleComponent for SystemStatus {
    fn accessibility(&self, _focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::Status, "Audio engine");
        node.value = Some(self.state.label().to_owned());
        node.state.busy = self.state == SystemStatusState::Connecting;
        node.state.fault = self.state == SystemStatusState::Offline;
        node
    }
}

impl AccessibleComponent for RackCardState {
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::Button, &self.name);
        node.value = Some(self.status.label().to_owned());
        node.state.selected = self.status == crate::components::RackCardStatus::Active;
        node.focus_order = Some(focus_order);
        node.keyboard_actions = COLLECTION_ACTIONS;
        node
    }
}

impl AccessibleComponent for PluginSlotState {
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::Button, &self.name);
        node.value = Some(self.status.label().to_owned());
        node.state.busy = self.status == PluginSlotStatus::Loading;
        node.state.fault = matches!(
            self.status,
            PluginSlotStatus::Faulted | PluginSlotStatus::Missing
        );
        node.focus_order = Some(focus_order);
        node.keyboard_actions = COLLECTION_ACTIONS;
        node
    }
}

impl AccessibleComponent for GainFaderState {
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::Slider, "Rack gain");
        node.value = Some(format!("{:.1} dB", self.gain_db));
        node.range = Some(AccessibilityRange {
            minimum: -120.0,
            maximum: 24.0,
            step: 1.0,
        });
        node.state.pressed = self.muted;
        node.focus_order = Some(focus_order);
        node.keyboard_actions = ADJUSTABLE_ACTIONS;
        node
    }
}

impl AccessibleComponent for ParameterControlState {
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let role = if self.discrete {
            AccessibilityRole::RadioGroup
        } else {
            AccessibilityRole::Slider
        };
        let mut node = AccessibilityNode::named(role, &self.name);
        node.value = Some(self.formatted.clone());
        node.range = Some(AccessibilityRange {
            minimum: 0.0,
            maximum: 1.0,
            step: if self.discrete { 1.0 } else { 0.01 },
        });
        node.state.disabled = self.read_only;
        if !self.read_only {
            node.focus_order = Some(focus_order);
            node.keyboard_actions = if self.default.is_some() {
                ADJUSTABLE_ACTIONS
            } else {
                &ADJUSTABLE_ACTIONS[..6]
            };
        }
        node
    }
}

impl AccessibleComponent for SegmentedControlState {
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let selected = self.options.get(self.selected).cloned().unwrap_or_default();
        let mut node = AccessibilityNode::named(AccessibilityRole::RadioGroup, &self.label);
        node.value = Some(selected);
        node.range = Some(AccessibilityRange {
            minimum: 0.0,
            maximum: f64::from(
                u32::try_from(self.options.len().saturating_sub(1)).unwrap_or(u32::MAX),
            ),
            step: 1.0,
        });
        node.focus_order = Some(focus_order);
        node.keyboard_actions = COLLECTION_ACTIONS;
        node
    }
}

impl AccessibleComponent for ScenePadState {
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::Button, &self.name);
        node.value = Some(self.status.label().to_owned());
        node.state.selected = self.status == ScenePadStatus::Active;
        node.state.busy = self.status == ScenePadStatus::Recalling;
        node.focus_order = Some(focus_order);
        node.keyboard_actions = &[KeyboardAction::Toggle, KeyboardAction::Open];
        node
    }
}

impl AccessibleComponent for WorkerHealth {
    fn accessibility(&self, _focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::Status, "Rack worker health");
        node.value = Some(self.state.label().to_owned());
        node.state.busy = self.state == WorkerHealthState::Recovering;
        node.state.fault = self.state == WorkerHealthState::Unavailable;
        node
    }
}

impl AccessibleComponent for FaultBannerModel {
    fn accessibility(&self, focus_order: FocusOrder) -> AccessibilityNode {
        let mut node = AccessibilityNode::named(AccessibilityRole::Alert, "Rack fault");
        node.value = Some(self.message.clone());
        node.state.fault = self.state == FaultBannerState::Visible;
        if self.state == FaultBannerState::Visible {
            node.focus_order = Some(focus_order);
            node.keyboard_actions = &[KeyboardAction::Close];
        }
        node
    }
}
