//! Token bridge and the painted primitives every screen shares.
//!
//! Every colour, size, and font the renderer uses resolves through this module from
//! `sp_ui::design`. The Cue style is a calm console: Inter text on a flat window, filled controls
//! with soft radii, hairline dividers, and the teal accent reserved for selection and focus.
//! Popovers are the one place a shadow appears.

use eframe::egui::{
    self, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, InnerResponse, Pos2,
    Rect, Response, Sense, Stroke, Ui, Vec2, WidgetInfo, WidgetType, text::LayoutJob,
};
use sp_ui::design::{ColorToken, FontWeight, Layout, Spacing, TypographyRole, fonts};

const INTER_MEDIUM_FAMILY: &str = "inter-medium";
const INTER_SEMIBOLD_FAMILY: &str = "inter-semibold";

/// Resolves a chrome colour token. The only egui colour constructor in the renderer.
pub(super) fn color(token: ColorToken) -> Color32 {
    let [red, green, blue] = token.color().channels();
    Color32::from_rgb(red, green, blue)
}

pub(super) fn px(layout: Layout) -> f32 {
    f32::from(layout.pixels())
}

pub(super) fn sp(spacing: Spacing) -> f32 {
    f32::from(spacing.pixels())
}

pub(super) fn font(role: TypographyRole) -> FontId {
    let typography = role.typography();
    FontId::new(
        f32::from(typography.size),
        match typography.weight {
            FontWeight::Regular => FontFamily::Proportional,
            FontWeight::Medium => FontFamily::Name(INTER_MEDIUM_FAMILY.into()),
            FontWeight::SemiBold => FontFamily::Name(INTER_SEMIBOLD_FAMILY.into()),
        },
    )
}

pub(super) fn line() -> f32 {
    px(Layout::Line)
}

/// The line height of a typography role.
pub(super) fn line_height(role: TypographyRole) -> f32 {
    f32::from(role.typography().line_height)
}

/// The corner radius of controls, preview slots, and popovers.
pub(super) fn radius() -> f32 {
    px(Layout::Radius)
}

/// The corner radius of meter bars, the gain marker, and load bars.
pub(super) fn small_radius() -> f32 {
    px(Layout::SmallRadius)
}

fn corner_radius() -> egui::CornerRadius {
    egui::CornerRadius::same(u8::try_from(Layout::Radius.pixels()).unwrap_or(u8::MAX))
}

pub(super) fn hairline(token: ColorToken) -> Stroke {
    Stroke::new(px(Layout::Hairline), color(token))
}

/// Installs Inter, IBM Plex Mono, and the Cue egui style.
pub fn install_style(ctx: &egui::Context) {
    install_fonts(ctx);

    let mut style = (*ctx.style()).clone();
    let visuals = &mut style.visuals;
    *visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(color(ColorToken::Text));
    visuals.panel_fill = color(ColorToken::Base);
    visuals.window_fill = color(ColorToken::Base);
    visuals.extreme_bg_color = color(ColorToken::Well);
    visuals.faint_bg_color = color(ColorToken::HoverFill);
    visuals.code_bg_color = color(ColorToken::Well);
    visuals.window_stroke = hairline(ColorToken::Outline);
    visuals.window_corner_radius = corner_radius();
    visuals.menu_corner_radius = corner_radius();
    visuals.window_shadow = egui::Shadow::NONE;
    visuals.popup_shadow = egui::Shadow::NONE;
    visuals.selection.bg_fill = color(ColorToken::AccentFill);
    visuals.selection.stroke = hairline(ColorToken::Accent);
    visuals.text_cursor.stroke = hairline(ColorToken::Strong);
    install_widget_states(visuals);
    // Only meters animate: no hover, scroll, or popover transitions.
    style.animation_time = 0.0;
    style.spacing.item_spacing = Vec2::new(sp(Spacing::S8), 0.0);
    style.spacing.button_padding = Vec2::new(sp(Spacing::S6), 0.0);
    style.spacing.interact_size.y = px(Layout::Control);
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    for (text_style, role) in [
        (egui::TextStyle::Body, TypographyRole::Body),
        (egui::TextStyle::Button, TypographyRole::Body),
        (egui::TextStyle::Small, TypographyRole::Label),
        (egui::TextStyle::Heading, TypographyRole::Title),
    ] {
        style.text_styles.insert(text_style, font(role));
    }
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        FontId::new(
            f32::from(TypographyRole::Body.typography().size),
            FontFamily::Monospace,
        ),
    );
    ctx.set_style(style);
}

fn install_fonts(ctx: &egui::Context) {
    let mut definitions = FontDefinitions::default();
    for (name, bytes) in [
        ("inter-regular", fonts::INTER_REGULAR),
        (INTER_MEDIUM_FAMILY, fonts::INTER_MEDIUM),
        (INTER_SEMIBOLD_FAMILY, fonts::INTER_SEMIBOLD),
        ("plex-mono", fonts::PLEX_MONO_REGULAR),
    ] {
        definitions.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(FontData::from_static(bytes)),
        );
    }
    // Inter and Plex Mono lack ⌘ and ⇧; egui's bundled Hack and icon font supply them.
    let fallbacks = ["Hack", "emoji-icon-font", "NotoEmoji-Regular"].map(str::to_owned);
    let family = |first: &str| {
        std::iter::once(first.to_owned())
            .chain(fallbacks.iter().cloned())
            .collect::<Vec<_>>()
    };
    definitions
        .families
        .insert(FontFamily::Proportional, family("inter-regular"));
    definitions
        .families
        .insert(FontFamily::Monospace, family("plex-mono"));
    definitions.families.insert(
        FontFamily::Name(INTER_MEDIUM_FAMILY.into()),
        family(INTER_MEDIUM_FAMILY),
    );
    definitions.families.insert(
        FontFamily::Name(INTER_SEMIBOLD_FAMILY.into()),
        family(INTER_SEMIBOLD_FAMILY),
    );
    ctx.set_fonts(definitions);
}

fn install_widget_states(visuals: &mut egui::Visuals) {
    for (widget, fill, border, text) in [
        (
            &mut visuals.widgets.noninteractive,
            ColorToken::Base,
            ColorToken::Hairline,
            ColorToken::Text,
        ),
        (
            &mut visuals.widgets.inactive,
            ColorToken::Raised,
            ColorToken::Outline,
            ColorToken::Text,
        ),
        (
            &mut visuals.widgets.hovered,
            ColorToken::HoverFill,
            ColorToken::Faint,
            ColorToken::Strong,
        ),
        (
            &mut visuals.widgets.active,
            ColorToken::HoverFill,
            ColorToken::Accent,
            ColorToken::Strong,
        ),
        (
            &mut visuals.widgets.open,
            ColorToken::Raised,
            ColorToken::Accent,
            ColorToken::Text,
        ),
    ] {
        widget.corner_radius = corner_radius();
        widget.bg_fill = color(fill);
        widget.weak_bg_fill = color(fill);
        widget.bg_stroke = hairline(border);
        widget.fg_stroke = hairline(text);
        widget.expansion = 0.0;
    }
}

/// Lays out one line of text, truncated with an ellipsis to `max_width`.
pub(super) fn galley(
    ui: &Ui,
    text: impl Into<String>,
    role: TypographyRole,
    token: ColorToken,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.into(), font(role), color(token));
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_width);
    ui.fonts(|fonts| fonts.layout_job(job))
}

/// Paints one line of text anchored at `pos`, truncated to `max_width`. Returns its rect.
pub(super) fn paint_text(
    ui: &Ui,
    pos: Pos2,
    anchor: Align2,
    text: impl Into<String>,
    role: TypographyRole,
    token: ColorToken,
    max_width: f32,
) -> Rect {
    let galley = galley(ui, text, role, token, max_width);
    let rect = anchor.anchor_size(pos, galley.size());
    ui.painter().galley(rect.min, galley, color(token));
    rect
}

/// Plain text that acts as a button. Hover underlines it in Strong and sets the label Strong; a
/// key hint follows in Dim. Disabled actions are faint.
pub(super) fn action(ui: &mut Ui, label: &str, key: Option<&str>, enabled: bool) -> Response {
    action_with(ui, label, key, enabled, false)
}

/// [`action`] that can draw a selected chip. `inverted` fills it with the accent fill, sets the label
/// Strong and the key hint Text, and pads it 6 px on each side.
pub(super) fn action_with(
    ui: &mut Ui,
    label: &str,
    key: Option<&str>,
    enabled: bool,
    inverted: bool,
) -> Response {
    let label_width = galley(
        ui,
        label,
        TypographyRole::Body,
        ColorToken::Text,
        f32::INFINITY,
    )
    .size()
    .x;
    let key_width = key.map_or(0.0, |key| {
        sp(Spacing::S6)
            + galley(
                ui,
                key,
                TypographyRole::Body,
                ColorToken::Dim,
                f32::INFINITY,
            )
            .size()
            .x
    });
    let pad = if inverted { sp(Spacing::S6) } else { 0.0 };
    let size = Vec2::new(pad * 2.0 + label_width + key_width, line());
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        let (text_token, key_token) = match (enabled, inverted, hovered) {
            (false, _, _) => (ColorToken::Faint, ColorToken::Faint),
            (true, true, _) => (ColorToken::Strong, ColorToken::Text),
            (true, false, true) => (ColorToken::Strong, ColorToken::Dim),
            (true, false, false) => (ColorToken::Text, ColorToken::Dim),
        };
        let painter = ui.painter();
        if inverted {
            painter.rect_filled(rect, radius(), color(ColorToken::AccentFill));
        }
        let label_rect = paint_text(
            ui,
            Pos2::new(rect.left() + pad, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            TypographyRole::Body,
            text_token,
            f32::INFINITY,
        );
        if hovered && !inverted {
            painter.hline(
                label_rect.x_range(),
                rect.bottom() - sp(Spacing::S2),
                hairline(ColorToken::Strong),
            );
        }
        if let Some(key) = key {
            paint_text(
                ui,
                Pos2::new(label_rect.right() + sp(Spacing::S6), rect.center().y),
                Align2::LEFT_CENTER,
                key,
                TypographyRole::Body,
                key_token,
                f32::INFINITY,
            );
        }
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response
}

/// A command button at the control height. See [`button_with`].
pub(super) fn button(ui: &mut Ui, label: &str, key: Option<&str>, enabled: bool) -> Response {
    button_with(ui, label, key, enabled, false, px(Layout::Control))
}

/// A bordered command button with an optional key hint in Dim after a 6 px gap. Hover fills it
/// with the hover fill and sets the label Strong. `active` marks an open or pressed state with an
/// Accent border. Disabled buttons are inert and faint.
pub(super) fn button_with(
    ui: &mut Ui,
    label: &str,
    key: Option<&str>,
    enabled: bool,
    active: bool,
    height: f32,
) -> Response {
    let label_width = galley(
        ui,
        label,
        TypographyRole::Body,
        ColorToken::Text,
        f32::INFINITY,
    )
    .size()
    .x;
    let key_width = key.map_or(0.0, |key| {
        sp(Spacing::S6)
            + galley(
                ui,
                key,
                TypographyRole::Body,
                ColorToken::Dim,
                f32::INFINITY,
            )
            .size()
            .x
    });
    let size = Vec2::new(sp(Spacing::S12) * 2.0 + label_width + key_width, height);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let (label_token, key_token) = paint_button(ui, rect, &response, enabled, active);
        let label_rect = paint_text(
            ui,
            Pos2::new(rect.left() + sp(Spacing::S12), rect.center().y),
            Align2::LEFT_CENTER,
            label,
            TypographyRole::Body,
            label_token,
            f32::INFINITY,
        );
        if let Some(key) = key {
            paint_text(
                ui,
                Pos2::new(label_rect.right() + sp(Spacing::S6), rect.center().y),
                Align2::LEFT_CENTER,
                key,
                TypographyRole::Body,
                key_token,
                f32::INFINITY,
            );
        }
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response
}

/// A square button for fade − and +: the control height on each side, with its label centred.
pub(super) fn square_button(ui: &mut Ui, label: &str, enabled: bool) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(px(Layout::Control)), sense);
    if ui.is_rect_visible(rect) {
        let (label_token, _) = paint_button(ui, rect, &response, enabled, false);
        paint_text(
            ui,
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            TypographyRole::Body,
            label_token,
            rect.width(),
        );
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response
}

/// Paints the fill and border of a button in its state, and returns the label and key colours.
fn paint_button(
    ui: &Ui,
    rect: Rect,
    response: &Response,
    enabled: bool,
    active: bool,
) -> (ColorToken, ColorToken) {
    let (fill, border, label, key) = if enabled {
        let hovered = response.hovered();
        let fill = if hovered {
            ColorToken::HoverFill
        } else {
            ColorToken::Raised
        };
        let border = if active {
            ColorToken::Accent
        } else if hovered {
            ColorToken::Faint
        } else {
            ColorToken::Outline
        };
        let label = if hovered || active {
            ColorToken::Strong
        } else {
            ColorToken::Text
        };
        let key = if hovered {
            ColorToken::Text
        } else {
            ColorToken::Dim
        };
        (fill, border, label, key)
    } else {
        (
            ColorToken::Base,
            ColorToken::Hairline,
            ColorToken::Faint,
            ColorToken::Faint,
        )
    };
    let painter = ui.painter();
    painter.rect_filled(rect, radius(), color(fill));
    painter.rect_stroke(rect, radius(), hairline(border), egui::StrokeKind::Inside);
    (label, key)
}

/// A bordered toggle at the control height, `width` wide, with a centred label. Lit fills with
/// `lit` and sets the label in Base; off is a button with a Raised fill.
pub(super) fn toggle(ui: &mut Ui, label: &str, on: bool, lit: ColorToken, width: f32) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, px(Layout::Control)), Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = response.hovered();
        let (fill, border, text_token) = if on {
            (lit, lit, ColorToken::Base)
        } else if hovered {
            (ColorToken::HoverFill, ColorToken::Faint, ColorToken::Strong)
        } else {
            (ColorToken::Raised, ColorToken::Outline, ColorToken::Text)
        };
        let painter = ui.painter();
        painter.rect_filled(rect, radius(), color(fill));
        painter.rect_stroke(rect, radius(), hairline(border), egui::StrokeKind::Inside);
        paint_text(
            ui,
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            TypographyRole::Body,
            text_token,
            rect.width(),
        );
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, on, label));
    response
}

/// A readout cell: a 1 px Hairline rule at its left edge, then a Label-role name in Dim above a
/// Value-role figure in `token`. The Text token draws as Strong, the readout default.
pub(super) fn readout(ui: &mut Ui, label: &str, value: &str, token: ColorToken, max_width: f32) {
    let value_token = if token == ColorToken::Text {
        ColorToken::Strong
    } else {
        token
    };
    let label_galley = galley(
        ui,
        label,
        TypographyRole::Label,
        ColorToken::Dim,
        f32::INFINITY,
    );
    let value_galley = galley(ui, value, TypographyRole::Value, value_token, max_width);
    let rule = px(Layout::Hairline);
    let text_width = label_galley.size().x.max(value_galley.size().x);
    let size = Vec2::new(
        rule + sp(Spacing::S14) * 2.0 + text_width,
        line_height(TypographyRole::Label) + line_height(TypographyRole::Value),
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.vline(
        rect.left() + rule / 2.0,
        rect.y_range(),
        hairline(ColorToken::Hairline),
    );
    let text_left = rect.left() + rule + sp(Spacing::S14);
    painter.galley(
        Pos2::new(text_left, rect.top()),
        label_galley,
        color(ColorToken::Dim),
    );
    painter.galley(
        Pos2::new(text_left, rect.top() + line_height(TypographyRole::Label)),
        value_galley,
        color(value_token),
    );
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Label, true, label);
        info.current_text_value = Some(value.to_owned());
        info
    });
}

/// A 1 px Accent outline with the control's radius, 2 px outside a keyboard-focused control.
pub(super) fn focus_outline(ui: &Ui, response: &Response, rect: Rect) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(sp(Spacing::S2)),
            radius(),
            hairline(ColorToken::Accent),
            egui::StrokeKind::Outside,
        );
    }
}

/// A route or sidechain jack: a channel label in a radius-cornered box. Selected jacks fill with
/// the accent fill and an Accent border; unavailable channels have a Hairline border and no fill.
pub(super) fn jack(ui: &mut Ui, label: &str, on: bool, available: bool, width: f32) -> Response {
    let size = Vec2::new(width, px(Layout::JackHeight));
    let sense = if available {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let hovered = available && response.hovered();
        let (fill, border, text) = match (available, on, hovered) {
            (false, _, _) => (None, ColorToken::Hairline, ColorToken::Faint),
            (true, true, _) => (
                Some(ColorToken::AccentFill),
                ColorToken::Accent,
                ColorToken::Strong,
            ),
            (true, false, true) => (
                Some(ColorToken::Raised),
                ColorToken::Faint,
                ColorToken::Strong,
            ),
            (true, false, false) => (
                Some(ColorToken::Raised),
                ColorToken::Outline,
                ColorToken::Dim,
            ),
        };
        let painter = ui.painter();
        if let Some(fill) = fill {
            painter.rect_filled(rect, radius(), color(fill));
        }
        painter.rect_stroke(rect, radius(), hairline(border), egui::StrokeKind::Inside);
        paint_text(
            ui,
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            TypographyRole::Body,
            text,
            rect.width(),
        );
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::RadioButton, available, on, label));
    response
}

/// The mark drawn before a choice row.
#[derive(Clone, Copy)]
pub(super) enum Mark {
    /// ● / ○ for one choice among several.
    Radio,
    /// ■ / □ for independent choices.
    Check,
}

/// A full-width choice row: mark, label, and a right-aligned value. Hover fills the row with
/// the hover fill, sets the label Strong, and promotes the value to Text so it keeps AA contrast.
pub(super) fn choice_row(
    ui: &mut Ui,
    mark: Mark,
    on: bool,
    label: &str,
    value: &str,
    enabled: bool,
) -> Response {
    let width = ui.available_width();
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, px(Layout::RouteRow)), sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        let painter = ui.painter();
        if hovered {
            painter.rect_filled(rect, radius(), color(ColorToken::HoverFill));
        }
        let (text, secondary) = match (enabled, hovered, on) {
            (false, _, _) => (ColorToken::Faint, ColorToken::Faint),
            (true, true, _) => (ColorToken::Strong, ColorToken::Text),
            (true, false, true) => (ColorToken::Text, ColorToken::Text),
            (true, false, false) => (ColorToken::Text, ColorToken::Dim),
        };
        let mark_token = match (enabled, on) {
            (false, _) => ColorToken::Faint,
            (true, true) => ColorToken::Accent,
            (true, false) => ColorToken::Outline,
        };
        let inset = sp(Spacing::S6);
        let mark_size = sp(Spacing::S10);
        let center = Pos2::new(rect.left() + inset + mark_size / 2.0, rect.center().y);
        let square = Rect::from_center_size(center, Vec2::splat(mark_size));
        let stroke = hairline(mark_token);
        match (mark, on) {
            (Mark::Radio, true) => {
                painter.circle_filled(center, mark_size / 2.0, color(mark_token));
            }
            (Mark::Radio, false) => {
                painter.circle_stroke(center, mark_size / 2.0, stroke);
            }
            (Mark::Check, true) => {
                painter.rect_filled(square, sp(Spacing::S2), color(mark_token));
            }
            (Mark::Check, false) => {
                painter.rect_stroke(square, sp(Spacing::S2), stroke, egui::StrokeKind::Inside);
            }
        }
        let value_rect = paint_text(
            ui,
            Pos2::new(rect.right() - inset, rect.center().y),
            Align2::RIGHT_CENTER,
            value,
            TypographyRole::Body,
            secondary,
            rect.width() / 2.0,
        );
        let label_left = rect.left() + inset + mark_size + sp(Spacing::S12);
        paint_text(
            ui,
            Pos2::new(label_left, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            TypographyRole::Body,
            text,
            (value_rect.left() - label_left - sp(Spacing::S8)).max(0.0),
        );
        focus_outline(ui, &response, rect);
    }
    let kind = match mark {
        Mark::Radio => WidgetType::RadioButton,
        Mark::Check => WidgetType::Checkbox,
    };
    response.widget_info(|| WidgetInfo::selected(kind, enabled, on, label));
    response
}

/// A full-width list row that highlights on hover or when it is the keyboard cursor. Returns the
/// response; disabled rows are faint and inert.
pub(super) fn list_row(
    ui: &mut Ui,
    label: &str,
    detail: &str,
    key: Option<&str>,
    current: bool,
    enabled: bool,
) -> Response {
    let width = ui.available_width();
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, px(Layout::RouteRow)), sense);
    if ui.is_rect_visible(rect) {
        let highlighted = enabled && (current || response.hovered());
        let (text, secondary) = match (enabled, highlighted) {
            (false, _) => (ColorToken::Faint, ColorToken::Faint),
            (true, true) => (ColorToken::Strong, ColorToken::Text),
            (true, false) => (ColorToken::Text, ColorToken::Dim),
        };
        if highlighted {
            ui.painter()
                .rect_filled(rect, radius(), color(ColorToken::HoverFill));
        }
        let inset = sp(Spacing::S12);
        let right = key.unwrap_or(detail);
        let right_rect = paint_text(
            ui,
            Pos2::new(rect.right() - inset, rect.center().y),
            Align2::RIGHT_CENTER,
            right,
            TypographyRole::Body,
            secondary,
            rect.width() / 2.0,
        );
        paint_text(
            ui,
            Pos2::new(rect.left() + inset, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            TypographyRole::Body,
            text,
            (right_rect.left() - rect.left() - inset * 2.0).max(0.0),
        );
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response
}

/// One line of text in the given role and colour.
pub(super) fn text(ui: &mut Ui, text: impl Into<String>, role: TypographyRole, token: ColorToken) {
    let text = text.into();
    let galley = galley(ui, text, role, token, ui.available_width());
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(galley.size().x, galley.size().y.max(line())),
        Sense::hover(),
    );
    ui.painter().galley(
        Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0),
        galley,
        color(token),
    );
}

/// Wrapped paragraph text for notes.
pub(super) fn note(ui: &mut Ui, text: &str, token: ColorToken) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .font(font(TypographyRole::Body))
                .color(color(token)),
        )
        .wrap(),
    );
}

/// A section title inside the setup page and popovers.
pub(super) fn title(ui: &mut Ui, text: &str) {
    self::text(ui, text, TypographyRole::Title, ColorToken::Strong);
}

/// A single-line text field on the Well fill. The egui visuals draw its border and focus ring.
pub(super) fn text_field(ui: &mut Ui, value: &mut String, hint: &str, width: f32) -> Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .font(font(TypographyRole::Body))
            .background_color(color(ColorToken::Well))
            .hint_text(
                egui::RichText::new(hint)
                    .font(font(TypographyRole::Body))
                    .color(color(ColorToken::Faint)),
            )
            .margin(egui::Margin::symmetric(
                i8::try_from(Spacing::S8.pixels()).unwrap_or(i8::MAX),
                i8::try_from(Spacing::S4.pixels()).unwrap_or(i8::MAX),
            ))
            .desired_width(width),
    )
}

/// The bordered box of a popover or modal: Surface fill, an Outline border, a Radius corner, and
/// the one permitted shadow. `pivot` places the box relative to `pos`.
pub(super) fn popover<R>(
    ctx: &egui::Context,
    id: egui::Id,
    pos: Pos2,
    pivot: Align2,
    width: Option<f32>,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> InnerResponse<R> {
    egui::Area::new(id)
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .pivot(pivot)
        .constrain(true)
        .fade_in(false)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(color(ColorToken::Surface))
                .stroke(hairline(ColorToken::Outline))
                .corner_radius(corner_radius())
                .inner_margin(egui::Margin::symmetric(
                    i8::try_from(Spacing::S12.pixels()).unwrap_or(i8::MAX),
                    i8::try_from(Spacing::S10.pixels()).unwrap_or(i8::MAX),
                ))
                .shadow(egui::Shadow {
                    offset: [0, i8::try_from(Spacing::S10.pixels()).unwrap_or(i8::MAX)],
                    blur: Spacing::S24.pixels() + Spacing::S16.pixels(),
                    spread: 0,
                    color: color(ColorToken::Well).gamma_multiply(0.6),
                })
                .show(ui, |ui| {
                    if let Some(width) = width {
                        ui.set_width(width);
                    }
                    add_contents(ui)
                })
                .inner
        })
}

/// A popover title: a Title-role heading at the left, a Body detail in Dim at the right.
pub(super) fn popover_title(ui: &mut Ui, title: &str, detail: &str) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, line()), Sense::hover());
    let detail_rect = paint_text(
        ui,
        rect.right_center(),
        Align2::RIGHT_CENTER,
        detail,
        TypographyRole::Body,
        ColorToken::Dim,
        width / 2.0,
    );
    paint_text(
        ui,
        rect.left_center(),
        Align2::LEFT_CENTER,
        title,
        TypographyRole::Title,
        ColorToken::Strong,
        (detail_rect.left() - rect.left() - sp(Spacing::S12)).max(0.0),
    );
    ui.add_space(sp(Spacing::S8));
}

/// A transparent full-screen layer under popovers. It swallows clicks so nothing beneath reacts;
/// returns true when it was clicked.
pub(super) fn scrim(ctx: &egui::Context, id: egui::Id) -> bool {
    egui::Area::new(id)
        .order(egui::Order::Middle)
        .fixed_pos(Pos2::ZERO)
        .fade_in(false)
        .show(ctx, |ui| {
            let rect = ctx.screen_rect();
            ui.allocate_rect(rect, Sense::click()).clicked()
                || ui
                    .interact(rect, id.with("secondary"), Sense::click())
                    .secondary_clicked()
        })
        .inner
}

/// Paints a dashed rectangle outline, used for empty preview slots.
pub(super) fn dashed_rect(ui: &Ui, rect: Rect, token: ColorToken) {
    let rect = rect.shrink(px(Layout::Hairline) / 2.0);
    let points = [
        rect.left_top(),
        rect.right_top(),
        rect.right_bottom(),
        rect.left_bottom(),
        rect.left_top(),
    ];
    ui.painter().extend(egui::Shape::dashed_line(
        &points,
        hairline(token),
        sp(Spacing::S4),
        sp(Spacing::S2) + px(Layout::Hairline),
    ));
}
