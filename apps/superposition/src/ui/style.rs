//! Token bridge and the painted primitives every screen shares.
//!
//! Every colour, size, radius, and font the renderer uses resolves through this module from
//! `sp_ui::design`. Primitives follow T3 Code's dark style: near-black cards with hairline
//! borders and rounded corners, one blue accent for selection and primary actions, Inter for text,
//! and IBM Plex Mono for numbers.

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId,
    InnerResponse, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2, WidgetInfo, WidgetType,
    text::LayoutJob,
};
use sp_ui::design::{
    ColorToken, FontFace, FontWeight, Layout, Radius, Spacing, TypographyRole, fonts,
};

const SANS: &str = "inter";
const SANS_SEMIBOLD: &str = "inter-semibold";
const MONO: &str = "plex-mono";

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

pub(super) fn radius(radius: Radius) -> CornerRadius {
    CornerRadius::same(radius.pixels())
}

pub(super) fn font(role: TypographyRole) -> FontId {
    let typography = role.typography();
    FontId::new(
        f32::from(typography.size),
        match (typography.face, typography.weight) {
            (FontFace::Mono, _) => FontFamily::Monospace,
            (FontFace::Sans, FontWeight::Regular) => FontFamily::Proportional,
            (FontFace::Sans, FontWeight::SemiBold) => FontFamily::Name(SANS_SEMIBOLD.into()),
        },
    )
}

pub(super) fn line() -> f32 {
    px(Layout::Line)
}

pub(super) fn hairline(token: ColorToken) -> Stroke {
    Stroke::new(px(Layout::Hairline), color(token))
}

/// The tinted fill behind a lit control of `lit` colour.
fn tint(lit: ColorToken) -> ColorToken {
    match lit {
        ColorToken::Warn => ColorToken::WarnTint,
        ColorToken::Info => ColorToken::InfoTint,
        ColorToken::Fault => ColorToken::FaultTint,
        _ => ColorToken::HoverFill,
    }
}

/// Installs Inter, IBM Plex Mono, and the token-driven egui style.
pub fn install_style(ctx: &egui::Context) {
    let mut definitions = FontDefinitions::default();
    for (name, bytes) in [
        (SANS, fonts::INTER_REGULAR),
        (SANS_SEMIBOLD, fonts::INTER_SEMIBOLD),
        (MONO, fonts::PLEX_MONO_REGULAR),
    ] {
        definitions.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(FontData::from_static(bytes)),
        );
    }
    // egui's bundled Hack and icon fonts supply symbols the embedded faces lack.
    let fallbacks = ["Hack", "emoji-icon-font", "NotoEmoji-Regular"].map(str::to_owned);
    let family = |first: &[&str]| {
        first
            .iter()
            .map(|name| (*name).to_owned())
            .chain(fallbacks.iter().cloned())
            .collect::<Vec<_>>()
    };
    definitions
        .families
        .insert(FontFamily::Proportional, family(&[SANS]));
    definitions
        .families
        .insert(FontFamily::Monospace, family(&[MONO, SANS]));
    definitions.families.insert(
        FontFamily::Name(SANS_SEMIBOLD.into()),
        family(&[SANS_SEMIBOLD, SANS]),
    );
    ctx.set_fonts(definitions);

    let mut style = (*ctx.style()).clone();
    let visuals = &mut style.visuals;
    *visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(color(ColorToken::Text));
    visuals.panel_fill = color(ColorToken::Base);
    visuals.window_fill = color(ColorToken::Surface);
    visuals.extreme_bg_color = color(ColorToken::Base);
    visuals.faint_bg_color = color(ColorToken::HoverFill);
    visuals.code_bg_color = color(ColorToken::Base);
    visuals.window_stroke = hairline(ColorToken::Hairline);
    visuals.window_corner_radius = radius(Radius::Card);
    visuals.menu_corner_radius = radius(Radius::Control);
    visuals.window_shadow = shadow();
    visuals.popup_shadow = shadow();
    visuals.selection.bg_fill = color(ColorToken::Accent).gamma_multiply(0.45);
    visuals.selection.stroke = hairline(ColorToken::Accent);
    visuals.text_cursor.stroke = hairline(ColorToken::Text);
    for (widget, border) in [
        (&mut visuals.widgets.noninteractive, ColorToken::Hairline),
        (&mut visuals.widgets.inactive, ColorToken::Hairline),
        (&mut visuals.widgets.hovered, ColorToken::Dim),
        (&mut visuals.widgets.active, ColorToken::Accent),
        (&mut visuals.widgets.open, ColorToken::Accent),
    ] {
        widget.corner_radius = radius(Radius::Control);
        widget.bg_fill = color(ColorToken::Surface);
        widget.weak_bg_fill = color(ColorToken::Surface);
        widget.bg_stroke = hairline(border);
        widget.fg_stroke = hairline(ColorToken::Text);
        widget.expansion = 0.0;
    }
    // Only meters animate: no hover, scroll, or popover transitions.
    style.animation_time = 0.0;
    style.spacing.item_spacing = Vec2::new(sp(Spacing::S8), 0.0);
    style.spacing.button_padding = Vec2::new(sp(Spacing::S6), 0.0);
    style.spacing.interact_size.y = line();
    style.spacing.scroll = egui::style::ScrollStyle::thin();
    for (text_style, role) in [
        (egui::TextStyle::Body, TypographyRole::Body),
        (egui::TextStyle::Button, TypographyRole::Body),
        (egui::TextStyle::Monospace, TypographyRole::Mono),
        (egui::TextStyle::Small, TypographyRole::Label),
        (egui::TextStyle::Heading, TypographyRole::Title),
    ] {
        style.text_styles.insert(text_style, font(role));
    }
    ctx.set_style(style);
}

/// The soft shadow under popovers, menus, and the scene bar.
fn shadow() -> egui::Shadow {
    egui::Shadow {
        offset: [0, i8::try_from(Spacing::S6.pixels()).unwrap_or(i8::MAX)],
        blur: Spacing::S24.pixels(),
        spread: 0,
        color: color(ColorToken::Base).gamma_multiply(0.7),
    }
}

/// A card: surface fill, hairline border, rounded corners. `ring` draws the accent selection
/// ring instead of the border.
pub(super) fn card(ui: &Ui, rect: Rect, ring: bool) {
    let painter = ui.painter();
    painter.rect_filled(rect, radius(Radius::Card), color(ColorToken::Surface));
    if ring {
        painter.rect_stroke(
            rect.expand(sp(Spacing::S2)),
            radius(Radius::Card),
            Stroke::new(
                sp(Spacing::S2),
                color(ColorToken::Accent).gamma_multiply(0.3),
            ),
            egui::StrokeKind::Outside,
        );
    }
    painter.rect_stroke(
        rect,
        radius(Radius::Card),
        hairline(if ring {
            ColorToken::Accent
        } else {
            ColorToken::Hairline
        }),
        egui::StrokeKind::Inside,
    );
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

/// A status dot of `token` colour centred at `center`.
pub(super) fn dot(ui: &Ui, center: Pos2, token: ColorToken) {
    ui.painter()
        .circle_filled(center, px(Layout::Dot) / 2.0, color(token));
}

/// Paints a key hint (`⌘S`) in a small rounded box at `left`, centred on `center_y`. Returns its
/// width.
fn paint_key(ui: &Ui, left: f32, center_y: f32, key: &str, token: ColorToken) -> f32 {
    let galley = galley(ui, key, TypographyRole::MonoSmall, token, f32::INFINITY);
    let pad = sp(Spacing::S4);
    let height = galley.size().y + sp(Spacing::S2);
    let rect = Rect::from_min_size(
        Pos2::new(left, center_y - height / 2.0),
        Vec2::new(galley.size().x + pad * 2.0, height),
    );
    ui.painter().rect_stroke(
        rect,
        radius(Radius::Small),
        Stroke::new(px(Layout::Hairline), color(token).gamma_multiply(0.5)),
        egui::StrokeKind::Inside,
    );
    ui.painter().galley(
        Pos2::new(rect.left() + pad, center_y - galley.size().y / 2.0),
        galley,
        color(token),
    );
    rect.width()
}

/// How a button is drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ButtonKind {
    /// Accent fill: the one main action of a place, such as Save or Capture.
    Primary,
    /// Hairline border.
    Outline,
    /// No border until hovered.
    Ghost,
}

/// A rounded button of control height: the label, then an optional key hint in a key box.
pub(super) fn button(
    ui: &mut Ui,
    label: &str,
    key: Option<&str>,
    kind: ButtonKind,
    enabled: bool,
) -> Response {
    let text_token = match (enabled, kind) {
        (false, _) => ColorToken::Faint,
        (true, ButtonKind::Primary) => ColorToken::AccentText,
        (true, _) => ColorToken::Text,
    };
    let text = galley(ui, label, TypographyRole::Body, text_token, f32::INFINITY);
    let key_width = key.map_or(0.0, |key| {
        galley(
            ui,
            key,
            TypographyRole::MonoSmall,
            text_token,
            f32::INFINITY,
        )
        .size()
        .x + sp(Spacing::S4) * 2.0
            + sp(Spacing::S6)
    });
    let pad = sp(Spacing::S10);
    let size = Vec2::new(pad * 2.0 + text.size().x + key_width, px(Layout::Control));
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        let painter = ui.painter();
        match kind {
            ButtonKind::Primary => {
                let fill = if enabled {
                    ColorToken::Accent
                } else {
                    ColorToken::SegmentOff
                };
                let fill = color(fill);
                painter.rect_filled(
                    rect,
                    radius(Radius::Control),
                    if hovered {
                        fill.gamma_multiply(0.85)
                    } else {
                        fill
                    },
                );
            }
            ButtonKind::Outline | ButtonKind::Ghost => {
                if hovered {
                    painter.rect_filled(
                        rect,
                        radius(Radius::Control),
                        color(ColorToken::HoverFill),
                    );
                }
                if kind == ButtonKind::Outline {
                    painter.rect_stroke(
                        rect,
                        radius(Radius::Control),
                        hairline(ColorToken::Hairline),
                        egui::StrokeKind::Inside,
                    );
                }
            }
        }
        let text_width = text.size().x;
        let text_pos = Pos2::new(rect.left() + pad, rect.center().y - text.size().y / 2.0);
        ui.painter().galley(text_pos, text, color(text_token));
        if let Some(key) = key {
            paint_key(
                ui,
                text_pos.x + text_width + sp(Spacing::S6),
                rect.center().y,
                key,
                text_token,
            );
        }
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response
}

/// A compact text action on the 16 px line: hover fills it, a key hint follows in dim, and
/// disabled actions are faint.
pub(super) fn action(ui: &mut Ui, label: &str, key: Option<&str>, enabled: bool) -> Response {
    action_with(ui, label, key, enabled, false)
}

/// [`action`], drawn selected (accent fill) when `selected`.
pub(super) fn action_with(
    ui: &mut Ui,
    label: &str,
    key: Option<&str>,
    enabled: bool,
    selected: bool,
) -> Response {
    let (text_token, key_token) = match (enabled, selected) {
        (false, _) => (ColorToken::Faint, ColorToken::Faint),
        (true, true) => (ColorToken::AccentText, ColorToken::AccentText),
        (true, false) => (ColorToken::Text, ColorToken::Dim),
    };
    let text = galley(ui, label, TypographyRole::Body, text_token, f32::INFINITY);
    let key = key.map(|key| galley(ui, key, TypographyRole::Mono, key_token, f32::INFINITY));
    let pad = sp(Spacing::S6);
    let key_width = key
        .as_ref()
        .map_or(0.0, |key| sp(Spacing::S6) + key.size().x);
    let size = Vec2::new(pad * 2.0 + text.size().x + key_width, line());
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if selected {
            painter.rect_filled(rect, radius(Radius::Small), color(ColorToken::Accent));
        } else if enabled && response.hovered() {
            painter.rect_filled(rect, radius(Radius::Small), color(ColorToken::HoverFill));
        }
        let text_pos = Pos2::new(rect.left() + pad, rect.center().y - text.size().y / 2.0);
        let text_width = text.size().x;
        painter.galley(text_pos, text, color(text_token));
        if let Some(key) = key {
            let pos = Pos2::new(
                text_pos.x + text_width + sp(Spacing::S6),
                rect.center().y - key.size().y / 2.0,
            );
            painter.galley(pos, key, color(key_token));
        }
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    response
}

/// A rounded toggle sized to its label. Lit fills with the tint of `lit` and draws in `lit`.
pub(super) fn toggle(ui: &mut Ui, label: &str, on: bool, lit: ColorToken) -> Response {
    let width = galley(
        ui,
        label,
        TypographyRole::Body,
        ColorToken::Dim,
        f32::INFINITY,
    )
    .size()
    .x + sp(Spacing::S10) * 2.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, px(Layout::Control)), Sense::hover());
    let id = ui.id().with(("toggle", label));
    toggle_at(ui, rect, id, label, on, lit)
}

/// A rounded toggle filling `rect`.
pub(super) fn toggle_at(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    label: &str,
    on: bool,
    lit: ColorToken,
) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = response.hovered();
        let (fill, border, text_token) = if on {
            (Some(tint(lit)), lit, lit)
        } else if hovered {
            (
                Some(ColorToken::HoverFill),
                ColorToken::Hairline,
                ColorToken::Text,
            )
        } else {
            (None, ColorToken::Hairline, ColorToken::Dim)
        };
        let painter = ui.painter();
        if let Some(fill) = fill {
            painter.rect_filled(rect, radius(Radius::Control), color(fill));
        }
        painter.rect_stroke(
            rect,
            radius(Radius::Control),
            if on {
                Stroke::new(px(Layout::Hairline), color(border).gamma_multiply(0.6))
            } else {
                hairline(border)
            },
            egui::StrokeKind::Inside,
        );
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

/// A chip's colouring.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChipTone {
    /// Route and count chips.
    Neutral,
    /// A route the device cannot play.
    Warn,
    /// A sidechain source.
    Info,
}

/// Width of a chip holding `text`.
pub(super) fn chip_width(ui: &Ui, text: &str) -> f32 {
    galley(
        ui,
        text,
        TypographyRole::Mono,
        ColorToken::Text,
        f32::INFINITY,
    )
    .size()
    .x + sp(Spacing::S6) * 2.0
}

/// A small rounded mono chip at `rect`. Hovering a clickable chip draws its border.
pub(super) fn chip_at(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    text: &str,
    tone: ChipTone,
    clickable: bool,
) -> Response {
    let sense = if clickable {
        Sense::click()
    } else {
        Sense::hover()
    };
    let response = ui.interact(rect, id, sense);
    if ui.is_rect_visible(rect) {
        let (fill, token) = match tone {
            ChipTone::Neutral => (ColorToken::SegmentOff, ColorToken::Text),
            ChipTone::Warn => (ColorToken::WarnTint, ColorToken::Warn),
            ChipTone::Info => (ColorToken::InfoTint, ColorToken::Info),
        };
        let painter = ui.painter();
        painter.rect_filled(rect, radius(Radius::Small), color(fill));
        if clickable && response.hovered() {
            painter.rect_stroke(
                rect,
                radius(Radius::Small),
                hairline(ColorToken::Dim),
                egui::StrokeKind::Inside,
            );
        }
        paint_text(
            ui,
            rect.center(),
            Align2::CENTER_CENTER,
            text,
            TypographyRole::Mono,
            token,
            rect.width() - sp(Spacing::S4),
        );
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, clickable, text));
    response
}

/// A chip sized to its text, in layout.
pub(super) fn chip(ui: &mut Ui, text: &str, tone: ChipTone, clickable: bool) -> Response {
    let size = Vec2::new(chip_width(ui, text), px(Layout::Chip));
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let id = ui.id().with(("chip", text));
    chip_at(ui, rect, id, text, tone, clickable)
}

/// A bordered pill: a status dot of `dot_token`, then `text`, truncated to `max_width`.
pub(super) fn pill(ui: &mut Ui, dot_token: ColorToken, text: &str, max_width: f32) -> Response {
    let galley = galley(ui, text, TypographyRole::Label, ColorToken::Text, max_width);
    let pad = sp(Spacing::S8);
    let dot_size = px(Layout::Dot);
    let size = Vec2::new(
        pad * 2.0 + dot_size + sp(Spacing::S6) + galley.size().x,
        px(Layout::Chip) + sp(Spacing::S4),
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_stroke(
            rect,
            radius(Radius::Pill),
            hairline(ColorToken::Hairline),
            egui::StrokeKind::Inside,
        );
        dot(
            ui,
            Pos2::new(rect.left() + pad + dot_size / 2.0, rect.center().y),
            dot_token,
        );
        ui.painter().galley(
            Pos2::new(
                rect.left() + pad + dot_size + sp(Spacing::S6),
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            color(ColorToken::Text),
        );
    }
    response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, text));
    response
}

/// Segmented tabs. Returns the index the user clicked.
pub(super) fn segmented(ui: &mut Ui, labels: &[&str], selected: usize) -> Option<usize> {
    let pad = sp(Spacing::S12);
    let inset = sp(Spacing::S2);
    let widths: Vec<f32> = labels
        .iter()
        .map(|label| {
            galley(
                ui,
                *label,
                TypographyRole::Body,
                ColorToken::Text,
                f32::INFINITY,
            )
            .size()
            .x + pad * 2.0
        })
        .collect();
    let size = Vec2::new(
        widths.iter().sum::<f32>() + inset * 2.0,
        px(Layout::Control) + inset * 2.0,
    );
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    ui.painter()
        .rect_filled(rect, radius(Radius::Control), color(ColorToken::Surface));
    ui.painter().rect_stroke(
        rect,
        radius(Radius::Control),
        hairline(ColorToken::Hairline),
        egui::StrokeKind::Inside,
    );
    let mut left = rect.left() + inset;
    let mut clicked = None;
    for (index, (label, width)) in labels.iter().zip(widths).enumerate() {
        let item = Rect::from_min_size(
            Pos2::new(left, rect.top() + inset),
            Vec2::new(width, px(Layout::Control)),
        );
        left += width;
        let response = ui.interact(item, ui.id().with(("segment", *label)), Sense::click());
        let active = index == selected;
        if active {
            ui.painter()
                .rect_filled(item, radius(Radius::Small), color(ColorToken::SegmentOff));
        }
        let token = if active || response.hovered() {
            ColorToken::Text
        } else {
            ColorToken::Dim
        };
        paint_text(
            ui,
            item.center(),
            Align2::CENTER_CENTER,
            *label,
            TypographyRole::Body,
            token,
            item.width(),
        );
        focus_outline(ui, &response, item);
        response.widget_info(|| WidgetInfo::selected(WidgetType::Button, true, active, *label));
        if response.clicked() {
            clicked = Some(index);
        }
    }
    clicked
}

/// The accent ring around a keyboard-focused control.
pub(super) fn focus_outline(ui: &Ui, response: &Response, rect: Rect) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(sp(Spacing::S2)),
            radius(Radius::Control),
            hairline(ColorToken::Accent),
            egui::StrokeKind::Outside,
        );
    }
}

/// A route or sidechain jack: rounded channel label, accent when selected, faint when the
/// device lacks the channel.
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
                Some(ColorToken::Accent),
                ColorToken::Accent,
                ColorToken::AccentText,
            ),
            (true, false, true) => (
                Some(ColorToken::HoverFill),
                ColorToken::Dim,
                ColorToken::Text,
            ),
            (true, false, false) => (None, ColorToken::Hairline, ColorToken::Dim),
        };
        let painter = ui.painter();
        if let Some(fill) = fill {
            painter.rect_filled(rect, radius(Radius::Small), color(fill));
        }
        painter.rect_stroke(
            rect,
            radius(Radius::Small),
            hairline(border),
            egui::StrokeKind::Inside,
        );
        paint_text(
            ui,
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            TypographyRole::Mono,
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
    /// A radio dot for one choice among several.
    Radio,
    /// A check box for independent choices.
    Check,
}

/// A full-width choice row: mark, label, and a right-aligned dim value. Hover fills the row
/// and promotes the value to text colour so it keeps AA contrast.
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
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, line()), sense);
    if ui.is_rect_visible(rect) {
        let hovered = enabled && response.hovered();
        if hovered {
            ui.painter().rect_filled(
                rect.expand2(Vec2::Y * sp(Spacing::S2)),
                radius(Radius::Small),
                color(ColorToken::HoverFill),
            );
        }
        let (text, secondary) = if !enabled {
            (ColorToken::Faint, ColorToken::Faint)
        } else if hovered {
            (ColorToken::Text, ColorToken::Text)
        } else {
            (ColorToken::Text, ColorToken::Dim)
        };
        let inset = sp(Spacing::S6);
        let mark_size = sp(Spacing::S10);
        let center = Pos2::new(rect.left() + inset + mark_size / 2.0, rect.center().y);
        paint_mark(ui, mark, on, enabled, center, mark_size);
        let value_rect = paint_text(
            ui,
            Pos2::new(rect.right() - inset, rect.center().y),
            Align2::RIGHT_CENTER,
            value,
            TypographyRole::Body,
            secondary,
            rect.width() / 2.0,
        );
        let label_left = rect.left() + inset + mark_size + sp(Spacing::S10);
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

/// A radio dot or check box: accent filled when on, a hairline outline when off.
fn paint_mark(ui: &Ui, mark: Mark, on: bool, enabled: bool, center: Pos2, size: f32) {
    let painter = ui.painter();
    let on_token = if enabled {
        ColorToken::Accent
    } else {
        ColorToken::Faint
    };
    let off = hairline(if enabled {
        ColorToken::Dim
    } else {
        ColorToken::Faint
    });
    let square = Rect::from_center_size(center, Vec2::splat(size));
    match (mark, on) {
        (Mark::Radio, true) => {
            painter.circle_filled(center, size / 2.0, color(on_token));
            painter.circle_filled(center, size / 5.0, color(ColorToken::AccentText));
        }
        (Mark::Radio, false) => {
            painter.circle_stroke(center, size / 2.0, off);
        }
        (Mark::Check, true) => {
            painter.rect_filled(square, radius(Radius::Small), color(on_token));
            let tick = [
                Pos2::new(square.left() + size * 0.25, center.y),
                Pos2::new(square.left() + size * 0.45, square.bottom() - size * 0.28),
                Pos2::new(square.right() - size * 0.22, square.top() + size * 0.28),
            ];
            painter.add(egui::Shape::line(
                tick.to_vec(),
                Stroke::new(sp(Spacing::S2) - 0.5, color(ColorToken::AccentText)),
            ));
        }
        (Mark::Check, false) => {
            painter.rect_stroke(square, radius(Radius::Small), off, egui::StrokeKind::Inside);
        }
    }
}

/// A full-width list row that fills on hover and turns accent when it is the keyboard cursor.
/// Disabled rows are faint and inert.
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
    let height = line() + sp(Spacing::S6);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);
    if ui.is_rect_visible(rect) {
        let (fill, text, secondary) = match (enabled, current, response.hovered()) {
            (false, _, _) => (None, ColorToken::Faint, ColorToken::Faint),
            (true, true, _) => (
                Some(ColorToken::Accent),
                ColorToken::AccentText,
                ColorToken::AccentText,
            ),
            (true, false, true) => (
                Some(ColorToken::HoverFill),
                ColorToken::Text,
                ColorToken::Text,
            ),
            (true, false, false) => (None, ColorToken::Text, ColorToken::Dim),
        };
        if let Some(fill) = fill {
            ui.painter()
                .rect_filled(rect, radius(Radius::Control), color(fill));
        }
        let inset = sp(Spacing::S10);
        let (right, right_role) = match key {
            Some(key) => (key, TypographyRole::Mono),
            None => (detail, TypographyRole::Body),
        };
        let right_rect = paint_text(
            ui,
            Pos2::new(rect.right() - inset, rect.center().y),
            Align2::RIGHT_CENTER,
            right,
            right_role,
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
    self::text(ui, text, TypographyRole::Title, ColorToken::Text);
}

/// A single-line text field.
pub(super) fn text_field(ui: &mut Ui, value: &mut String, hint: &str, width: f32) -> Response {
    ui.add(
        egui::TextEdit::singleline(value)
            .font(font(TypographyRole::Body))
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

/// The rounded card of a popover or modal, with a soft shadow. `pivot` places the box relative
/// to `pos`.
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
                .stroke(hairline(ColorToken::Hairline))
                .corner_radius(radius(Radius::Card))
                .inner_margin(egui::Margin::symmetric(
                    i8::try_from(Spacing::S12.pixels()).unwrap_or(i8::MAX),
                    i8::try_from(Spacing::S10.pixels()).unwrap_or(i8::MAX),
                ))
                .shadow(shadow())
                .show(ui, |ui| {
                    if let Some(width) = width {
                        ui.set_width(width);
                    }
                    add_contents(ui)
                })
                .inner
        })
}

/// A popover title: semibold text at the left, a dim detail at the right.
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
        ColorToken::Text,
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

/// Paints a dashed outline with rounded corners, used for empty slots and add targets.
pub(super) fn dashed_rect(ui: &Ui, rect: Rect, token: ColorToken, corner: Radius) {
    const ARC_STEPS: usize = 4;
    let rect = rect.shrink(px(Layout::Hairline) / 2.0);
    let r = f32::from(corner.pixels()).min(rect.width().min(rect.height()) / 2.0);
    let corners = [
        (Pos2::new(rect.right() - r, rect.top() + r), -90.0_f32),
        (Pos2::new(rect.right() - r, rect.bottom() - r), 0.0),
        (Pos2::new(rect.left() + r, rect.bottom() - r), 90.0),
        (Pos2::new(rect.left() + r, rect.top() + r), 180.0),
    ];
    let mut points = vec![Pos2::new(rect.left() + r, rect.top())];
    for (center, start) in corners {
        for step in 0..=ARC_STEPS {
            #[allow(clippy::cast_precision_loss, reason = "four arc steps")]
            let angle = (start + 90.0 * step as f32 / ARC_STEPS as f32).to_radians();
            points.push(center + Vec2::angled(angle) * r);
        }
    }
    points.push(Pos2::new(rect.left() + r, rect.top()));
    ui.painter().extend(egui::Shape::dashed_line(
        &points,
        hairline(token),
        sp(Spacing::S4),
        sp(Spacing::S4),
    ));
}
