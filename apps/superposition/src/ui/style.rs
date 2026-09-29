//! Token bridge and the painted primitives every screen shares.
//!
//! Every colour, size, and font the renderer uses resolves through this module from
//! `sp_ui::design`. Primitives are plain text on the 16 px line: no cards, radii, or shadows
//! (popovers are the one exception), inverted video for selection, and hairline borders.

use eframe::egui::{
    self, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, InnerResponse, Pos2,
    Rect, Response, Sense, Stroke, Ui, Vec2, WidgetInfo, WidgetType, text::LayoutJob,
};
use sp_ui::design::{ColorToken, FontWeight, Layout, Spacing, TypographyRole, fonts};

const MEDIUM_FAMILY: &str = "plex-mono-medium";

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
            FontWeight::Medium => FontFamily::Name(MEDIUM_FAMILY.into()),
        },
    )
}

pub(super) fn line() -> f32 {
    px(Layout::Line)
}

pub(super) fn hairline(token: ColorToken) -> Stroke {
    Stroke::new(px(Layout::Hairline), color(token))
}

/// Installs IBM Plex Mono and the token-driven egui style.
pub fn install_style(ctx: &egui::Context) {
    let mut definitions = FontDefinitions::default();
    for (name, bytes) in [
        ("plex-mono", fonts::PLEX_MONO_REGULAR),
        (MEDIUM_FAMILY, fonts::PLEX_MONO_MEDIUM),
    ] {
        definitions.font_data.insert(
            name.to_owned(),
            std::sync::Arc::new(FontData::from_static(bytes)),
        );
    }
    // Plex Mono lacks ⌘ and ⇧; egui's bundled Hack and icon font supply them.
    let fallbacks = ["Hack", "emoji-icon-font", "NotoEmoji-Regular"].map(str::to_owned);
    let family = |first: &str| {
        std::iter::once(first.to_owned())
            .chain(fallbacks.iter().cloned())
            .collect::<Vec<_>>()
    };
    definitions
        .families
        .insert(FontFamily::Proportional, family("plex-mono"));
    definitions
        .families
        .insert(FontFamily::Monospace, family("plex-mono"));
    definitions.families.insert(
        FontFamily::Name(MEDIUM_FAMILY.into()),
        family(MEDIUM_FAMILY),
    );
    ctx.set_fonts(definitions);

    let mut style = (*ctx.style()).clone();
    let visuals = &mut style.visuals;
    *visuals = egui::Visuals::dark();
    visuals.override_text_color = Some(color(ColorToken::Text));
    visuals.panel_fill = color(ColorToken::Base);
    visuals.window_fill = color(ColorToken::Base);
    visuals.extreme_bg_color = color(ColorToken::Base);
    visuals.faint_bg_color = color(ColorToken::HoverFill);
    visuals.code_bg_color = color(ColorToken::Base);
    visuals.window_stroke = hairline(ColorToken::Dim);
    visuals.window_corner_radius = egui::CornerRadius::ZERO;
    visuals.menu_corner_radius = egui::CornerRadius::ZERO;
    visuals.window_shadow = egui::Shadow::NONE;
    visuals.popup_shadow = egui::Shadow::NONE;
    visuals.selection.bg_fill = color(ColorToken::Faint);
    visuals.selection.stroke = hairline(ColorToken::Text);
    visuals.text_cursor.stroke = hairline(ColorToken::Text);
    for (widget, border) in [
        (&mut visuals.widgets.noninteractive, ColorToken::Hairline),
        (&mut visuals.widgets.inactive, ColorToken::Faint),
        (&mut visuals.widgets.hovered, ColorToken::Dim),
        (&mut visuals.widgets.active, ColorToken::Text),
        (&mut visuals.widgets.open, ColorToken::Text),
    ] {
        widget.corner_radius = egui::CornerRadius::ZERO;
        widget.bg_fill = color(ColorToken::Base);
        widget.weak_bg_fill = color(ColorToken::Base);
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
        (egui::TextStyle::Monospace, TypographyRole::Body),
        (egui::TextStyle::Small, TypographyRole::Label),
        (egui::TextStyle::Heading, TypographyRole::Title),
    ] {
        style.text_styles.insert(text_style, font(role));
    }
    ctx.set_style(style);
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

/// Plain text that behaves as a button: hover underlines it, a key hint follows in dim, and
/// disabled actions are faint. `inverted` draws it selected.
pub(super) fn action(ui: &mut Ui, label: &str, key: Option<&str>, enabled: bool) -> Response {
    action_with(ui, label, key, enabled, false)
}

pub(super) fn action_with(
    ui: &mut Ui,
    label: &str,
    key: Option<&str>,
    enabled: bool,
    inverted: bool,
) -> Response {
    let (text_token, key_token) = match (enabled, inverted) {
        (false, _) => (ColorToken::Faint, ColorToken::Faint),
        (true, true) => (ColorToken::Base, ColorToken::Faint),
        (true, false) => (ColorToken::Text, ColorToken::Dim),
    };
    let text = galley(ui, label, TypographyRole::Body, text_token, f32::INFINITY);
    let key = key.map(|key| galley(ui, key, TypographyRole::Body, key_token, f32::INFINITY));
    let pad = if inverted { sp(Spacing::S6) } else { 0.0 };
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
        if inverted {
            painter.rect_filled(rect, 0.0, color(ColorToken::Text));
        }
        let text_pos = Pos2::new(rect.left() + pad, rect.center().y - text.size().y / 2.0);
        let text_width = text.size().x;
        painter.galley(text_pos, text, color(text_token));
        if enabled && response.hovered() && !inverted {
            let y = rect.bottom() - sp(Spacing::S2);
            painter.hline(
                text_pos.x..=text_pos.x + text_width,
                y,
                hairline(text_token),
            );
        }
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

/// A bordered text toggle. Lit fills with `lit`.
pub(super) fn toggle(ui: &mut Ui, label: &str, on: bool, lit: ColorToken) -> Response {
    let text = galley(
        ui,
        label,
        TypographyRole::Body,
        ColorToken::Dim,
        f32::INFINITY,
    );
    let size = Vec2::new(
        text.size().x + sp(Spacing::S6) * 2.0,
        line() + px(Layout::Hairline) * 2.0,
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = response.hovered();
        let (fill, border, text_token) = if on {
            (Some(lit), lit, ColorToken::Base)
        } else if hovered {
            (None, ColorToken::Dim, ColorToken::Text)
        } else {
            (None, ColorToken::Faint, ColorToken::Dim)
        };
        let painter = ui.painter();
        if let Some(fill) = fill {
            painter.rect_filled(rect, 0.0, color(fill));
        }
        painter.rect_stroke(rect, 0.0, hairline(border), egui::StrokeKind::Inside);
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font(TypographyRole::Body),
            color(text_token),
        );
        focus_outline(ui, &response, rect);
    }
    response.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, true, on, label));
    response
}

/// A readout cell: a 10 px dim label over a 12 px value, truncated to `max_width`.
pub(super) fn readout(ui: &mut Ui, label: &str, value: &str, token: ColorToken, max_width: f32) {
    let label_galley = galley(
        ui,
        label,
        TypographyRole::Label,
        ColorToken::Dim,
        f32::INFINITY,
    );
    let value_galley = galley(ui, value, TypographyRole::Body, token, max_width);
    let caption = px(Layout::Caption);
    let size = Vec2::new(
        label_galley.size().x.max(value_galley.size().x),
        caption * 2.0,
    );
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.galley(rect.min, label_galley, color(ColorToken::Dim));
    painter.galley(
        Pos2::new(rect.left(), rect.top() + caption),
        value_galley,
        color(token),
    );
    response.widget_info(|| {
        let mut info = WidgetInfo::labeled(WidgetType::Label, true, label);
        info.current_text_value = Some(value.to_owned());
        info
    });
}

/// A 1 px text-colour outline around a keyboard-focused control.
pub(super) fn focus_outline(ui: &Ui, response: &Response, rect: Rect) {
    if response.has_focus() {
        ui.painter().rect_stroke(
            rect.expand(sp(Spacing::S2)),
            0.0,
            hairline(ColorToken::Text),
            egui::StrokeKind::Outside,
        );
    }
}

/// A route or sidechain jack: bordered channel label, inverted when selected, faint when the
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
            (true, true, _) => (Some(ColorToken::Text), ColorToken::Text, ColorToken::Base),
            (true, false, true) => (None, ColorToken::Text, ColorToken::Text),
            (true, false, false) => (None, ColorToken::Faint, ColorToken::Dim),
        };
        let painter = ui.painter();
        if let Some(fill) = fill {
            painter.rect_filled(rect, 0.0, color(fill));
        }
        painter.rect_stroke(rect, 0.0, hairline(border), egui::StrokeKind::Inside);
        painter.text(
            rect.center(),
            Align2::CENTER_CENTER,
            label,
            font(TypographyRole::Body),
            color(text),
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
        let painter = ui.painter();
        if hovered {
            painter.rect_filled(rect, 0.0, color(ColorToken::HoverFill));
        }
        let (text, secondary, mark_token) = if !enabled {
            (ColorToken::Faint, ColorToken::Faint, ColorToken::Faint)
        } else if hovered {
            (ColorToken::Text, ColorToken::Text, ColorToken::Text)
        } else {
            (
                ColorToken::Text,
                ColorToken::Dim,
                if on {
                    ColorToken::Text
                } else {
                    ColorToken::Dim
                },
            )
        };
        let inset = sp(Spacing::S6);
        let mark_size = sp(Spacing::S6);
        let center = Pos2::new(rect.left() + inset + mark_size / 2.0, rect.center().y);
        let stroke = hairline(mark_token);
        let square = Rect::from_center_size(center, Vec2::splat(mark_size));
        match (mark, on) {
            (Mark::Radio, true) => {
                painter.circle_filled(center, mark_size / 2.0, color(mark_token));
            }
            (Mark::Radio, false) => {
                painter.circle_stroke(center, mark_size / 2.0, stroke);
            }
            (Mark::Check, true) => {
                painter.rect_filled(square, 0.0, color(mark_token));
            }
            (Mark::Check, false) => {
                painter.rect_stroke(square, 0.0, stroke, egui::StrokeKind::Inside);
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

/// A full-width list row that inverts on hover or when it is the keyboard cursor. Returns the
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
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, line()), sense);
    if ui.is_rect_visible(rect) {
        let inverted = enabled && (current || response.hovered());
        let (text, secondary) = match (enabled, inverted) {
            (false, _) => (ColorToken::Faint, ColorToken::Faint),
            (true, true) => (ColorToken::Base, ColorToken::Faint),
            (true, false) => (ColorToken::Text, ColorToken::Dim),
        };
        if inverted {
            ui.painter().rect_filled(rect, 0.0, color(ColorToken::Text));
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
    self::text(ui, text, TypographyRole::Title, ColorToken::Text);
}

/// A single-line text field on the base fill.
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
                i8::try_from(Spacing::S6.pixels()).unwrap_or(i8::MAX),
                i8::try_from(Spacing::S2.pixels()).unwrap_or(i8::MAX),
            ))
            .desired_width(width),
    )
}

/// The bordered box of a popover or modal: base fill, dim border, and the one permitted
/// shadow. `pivot` places the box relative to `pos`.
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
                .fill(color(ColorToken::Base))
                .stroke(hairline(ColorToken::Dim))
                .inner_margin(egui::Margin::symmetric(
                    i8::try_from(Spacing::S12.pixels()).unwrap_or(i8::MAX),
                    i8::try_from(Spacing::S10.pixels()).unwrap_or(i8::MAX),
                ))
                .shadow(egui::Shadow {
                    offset: [0, i8::try_from(Spacing::S10.pixels()).unwrap_or(i8::MAX)],
                    blur: Spacing::S24.pixels() + Spacing::S16.pixels(),
                    spread: 0,
                    color: color(ColorToken::Base).gamma_multiply(0.6),
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

/// A popover title: medium text at the left, a dim detail at the right.
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
