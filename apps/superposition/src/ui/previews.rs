//! Editor pictures: the newest capture per plug-in instance, uploaded once as egui textures,
//! and the preview tile that shows one in a rack column.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, Align2, ColorImage, Pos2, Rect, Response, Sense, TextureHandle, TextureOptions, Ui, Vec2,
    WidgetInfo, WidgetType,
};
use sp_ui::components::StateToken;
use sp_ui::design::{ColorToken, Layout, Spacing, TypographyRole};

use super::style::{color, dashed_rect, hairline, line, paint_text, px, sp};
use sp_session::EditorPreviewFile;

/// Picture opacity for bypassed slots, and for unavailable ones drawn in grayscale.
const BYPASSED_OPACITY: f32 = 0.4;
const UNAVAILABLE_OPACITY: f32 = 0.35;

/// One stored picture with its colour and grayscale textures.
pub(super) struct Preview {
    file: EditorPreviewFile,
    color: TextureHandle,
    gray: TextureHandle,
}

/// Newest picture per plug-in instance. Textures are uploaded once per capture.
#[derive(Default)]
pub(super) struct PreviewCache {
    entries: HashMap<String, Preview>,
    loaded: bool,
}

impl PreviewCache {
    /// Whether the session's stored pictures have been read.
    pub(super) fn loaded(&self) -> bool {
        self.loaded
    }

    pub(super) fn mark_loaded(&mut self) {
        self.loaded = true;
    }

    /// Decodes and uploads a picture unless the same capture is already shown. Undecodable
    /// pictures are dropped; the slot keeps its previous picture.
    pub(super) fn insert(&mut self, ctx: &egui::Context, file: EditorPreviewFile) {
        if self.entries.get(&file.instance_id).is_some_and(|existing| {
            existing.file.captured_at_unix_ms >= file.captured_at_unix_ms
                && existing.file.png == file.png
        }) {
            return;
        }
        let Ok(decoded) = image::load_from_memory_with_format(&file.png, image::ImageFormat::Png)
        else {
            return;
        };
        let rgba = decoded.to_rgba8();
        let size = [rgba.width() as usize, rgba.height() as usize];
        let mut gray = rgba.clone();
        for pixel in gray.pixels_mut() {
            let [red, green, blue, alpha] = pixel.0;
            let luma =
                (0.2126 * f32::from(red) + 0.7152 * f32::from(green) + 0.0722 * f32::from(blue))
                    .round()
                    .clamp(0.0, 255.0);
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "luma is clamped to 0..=255"
            )]
            let luma = luma as u8;
            pixel.0 = [luma, luma, luma, alpha];
        }
        let upload = |name: &str, pixels: &[u8]| {
            ctx.load_texture(
                format!("preview-{name}-{}", file.instance_id),
                ColorImage::from_rgba_unmultiplied(size, pixels),
                TextureOptions::LINEAR,
            )
        };
        let color = upload("color", rgba.as_raw());
        let gray = upload("gray", gray.as_raw());
        self.entries
            .insert(file.instance_id.clone(), Preview { file, color, gray });
    }

    pub(super) fn get(&self, instance_id: &str) -> Option<&Preview> {
        self.entries.get(instance_id)
    }

    /// Pictures for the instances in `model`, written by an explicit save.
    pub(super) fn files(&self, model: &sp_model::Session) -> Vec<EditorPreviewFile> {
        model
            .racks
            .iter()
            .flat_map(|rack| &rack.slots)
            .filter_map(|slot| self.entries.get(&slot.id.0))
            .map(|preview| preview.file.clone())
            .collect()
    }
}

/// What one filled preview slot shows.
pub(super) struct Tile<'a> {
    pub(super) name: &'a str,
    pub(super) token: StateToken,
    pub(super) picture: Option<&'a Preview>,
    /// The plug-in's editor window is open.
    pub(super) live: bool,
    /// `SC Drums` or `SC in 7-8`.
    pub(super) sidechain: Option<String>,
    pub(super) selected: bool,
}

/// Draws a preview tile. Click opens the editor, right-click opens the slot menu, and hover
/// shows the full capture beside the tile.
pub(super) fn tile(ui: &mut Ui, rect: Rect, id: egui::Id, tile: &Tile<'_>) -> Response {
    let response = ui.interact(rect, id, Sense::click());
    let border = if tile.selected {
        ColorToken::Text
    } else if response.hovered() {
        ColorToken::Dim
    } else {
        ColorToken::Hairline
    };
    if tile.picture.is_none() && !tile.live {
        never_opened(ui, rect, tile, border);
    } else {
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, color(ColorToken::Base));
        if let Some(picture) = tile.picture {
            let unavailable = matches!(
                tile.token,
                StateToken::Missing
                    | StateToken::Faulted
                    | StateToken::Loading
                    | StateToken::Recovering
                    | StateToken::Unloaded
            );
            let (texture, opacity) = if unavailable {
                (&picture.gray, UNAVAILABLE_OPACITY)
            } else if tile.token == StateToken::Bypassed {
                (&picture.color, BYPASSED_OPACITY)
            } else {
                (&picture.color, 1.0)
            };
            painter.image(
                texture.id(),
                rect,
                cover_uv(texture.size_vec2(), rect.size()),
                egui::Color32::WHITE.gamma_multiply(opacity),
            );
        }
        caption(ui, rect, tile);
        ui.painter()
            .rect_stroke(rect, 0.0, hairline(border), egui::StrokeKind::Inside);
    }
    super::style::focus_outline(ui, &response, rect);
    response.widget_info(|| {
        WidgetInfo::selected(
            WidgetType::Button,
            true,
            tile.selected,
            format!("{}, {}", tile.name, tile.token.meaning()),
        )
    });
    if response.hovered()
        && let Some(picture) = tile.picture
    {
        peek(ui, rect, id, tile.name, picture);
    }
    response
}

/// A slot whose editor has never been opened: dashed, with the name and a note.
fn never_opened(ui: &Ui, rect: Rect, tile: &Tile<'_>, border: ColorToken) {
    let border = if border == ColorToken::Hairline {
        ColorToken::Faint
    } else {
        border
    };
    dashed_rect(ui, rect, border);
    let center = rect.center();
    let half = px(Layout::Caption) / 2.0;
    paint_text(
        ui,
        Pos2::new(center.x, center.y - half),
        Align2::CENTER_CENTER,
        tile.name,
        TypographyRole::Label,
        ColorToken::Dim,
        rect.width() - sp(Spacing::S12),
    );
    let (note, token) = match (&tile.sidechain, tile.token) {
        (_, StateToken::Missing | StateToken::Faulted) => {
            (tile.token.code().to_owned(), tile.token.color())
        }
        (Some(sidechain), _) => (sidechain.clone(), ColorToken::Info),
        (None, _) => ("not opened yet".to_owned(), ColorToken::Faint),
    };
    paint_text(
        ui,
        Pos2::new(center.x, center.y + half),
        Align2::CENTER_CENTER,
        note,
        TypographyRole::Label,
        token,
        rect.width() - sp(Spacing::S12),
    );
}

/// The 14 px caption bar: name at the left; sidechain marker, token, and capture age or `live`
/// at the right. The bar is opaque so plug-in pictures never reduce its contrast.
fn caption(ui: &Ui, rect: Rect, tile: &Tile<'_>) {
    let bar = Rect::from_min_max(
        Pos2::new(rect.left(), rect.bottom() - px(Layout::Caption)),
        rect.max,
    );
    ui.painter().rect_filled(bar, 0.0, color(ColorToken::Base));
    let inset = sp(Spacing::S6);
    let name_token = match tile.token {
        StateToken::Missing | StateToken::Faulted => ColorToken::Fault,
        StateToken::Loading | StateToken::Recovering => ColorToken::Warn,
        _ => ColorToken::Text,
    };
    let mut right = bar.right() - inset;
    let age = if tile.live {
        "live".to_owned()
    } else {
        tile.picture.map_or_else(String::new, |picture| {
            age_label(picture.file.captured_at_unix_ms)
        })
    };
    let age_rect = caption_right(ui, bar, &mut right, &age, ColorToken::Dim);
    if tile.live {
        let dot = px(Layout::LiveDot);
        ui.painter().circle_filled(
            Pos2::new(
                age_rect.left() - sp(Spacing::S4) - dot / 2.0,
                bar.center().y,
            ),
            dot / 2.0,
            color(ColorToken::Fault),
        );
        right -= dot + sp(Spacing::S4);
    }
    caption_right(ui, bar, &mut right, tile.token.code(), tile.token.color());
    if let Some(sidechain) = &tile.sidechain {
        caption_right(ui, bar, &mut right, sidechain, ColorToken::Info);
    }
    paint_text(
        ui,
        Pos2::new(bar.left() + inset, bar.center().y),
        Align2::LEFT_CENTER,
        tile.name,
        TypographyRole::Label,
        name_token,
        (right - bar.left() - inset).max(0.0),
    );
}

/// Paints caption text right-aligned at `right`, then moves `right` left past it.
fn caption_right(ui: &Ui, bar: Rect, right: &mut f32, text: &str, token: ColorToken) -> Rect {
    let placed = paint_text(
        ui,
        Pos2::new(*right, bar.center().y),
        Align2::RIGHT_CENTER,
        text,
        TypographyRole::Label,
        token,
        bar.width() / 2.0,
    );
    *right = placed.left() - sp(Spacing::S8);
    placed
}

/// The full 320×200 capture in a bordered popover beside the tile.
fn peek(ui: &Ui, rect: Rect, id: egui::Id, name: &str, picture: &Preview) {
    let size = Vec2::new(px(Layout::CaptureWidth), px(Layout::CaptureHeight))
        + Vec2::splat(px(Layout::Hairline) * 2.0);
    let screen = ui.ctx().screen_rect();
    let gap = sp(Spacing::S8);
    let left = if rect.right() + gap + size.x <= screen.right() {
        rect.right() + gap
    } else {
        rect.left() - gap - size.x
    };
    let top = (rect.top() - line() * 4.0)
        .clamp(screen.top(), (screen.bottom() - size.y).max(screen.top()));
    let frame = Rect::from_min_size(Pos2::new(left, top), size);
    egui::Area::new(id.with("peek"))
        .order(egui::Order::Tooltip)
        .fixed_pos(frame.min)
        .interactable(false)
        .fade_in(false)
        .show(ui.ctx(), |ui| {
            let (frame, _) = ui.allocate_exact_size(size, Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(frame, 0.0, color(ColorToken::Base));
            let inner = frame.shrink(px(Layout::Hairline));
            painter.image(
                picture.color.id(),
                inner,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                egui::Color32::WHITE,
            );
            painter.rect_stroke(
                frame,
                0.0,
                hairline(ColorToken::Text),
                egui::StrokeKind::Inside,
            );
            let bar = Rect::from_min_max(
                Pos2::new(inner.left(), inner.bottom() - px(Layout::Caption)),
                inner.max,
            );
            painter.rect_filled(bar, 0.0, color(ColorToken::Base));
            paint_text(
                ui,
                Pos2::new(bar.left() + sp(Spacing::S8), bar.center().y),
                Align2::LEFT_CENTER,
                format!(
                    "{name}, captured {}",
                    age_label(picture.file.captured_at_unix_ms)
                ),
                TypographyRole::Label,
                ColorToken::Text,
                bar.width() - sp(Spacing::S16),
            );
        });
}

/// UV rectangle that scales a picture to cover `target`, keeping its middle band.
fn cover_uv(picture: Vec2, target: Vec2) -> Rect {
    if picture.x <= 0.0 || picture.y <= 0.0 || target.x <= 0.0 || target.y <= 0.0 {
        return Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
    }
    let scale = (target.x / picture.x).max(target.y / picture.y);
    let visible = Vec2::new(
        target.x / (picture.x * scale),
        target.y / (picture.y * scale),
    );
    let margin = (Vec2::splat(1.0) - visible) / 2.0;
    Rect::from_min_max(margin.to_pos2(), (margin + visible).to_pos2())
}

/// `now`, `2 min`, `1 h`, or `Sep 18`.
fn age_label(captured_at_unix_ms: u64) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let seconds = now_ms.saturating_sub(captured_at_unix_ms) / 1000;
    if seconds < 60 {
        "now".to_owned()
    } else if seconds < 3600 {
        format!("{} min", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} h", seconds / 3600)
    } else {
        month_day(captured_at_unix_ms / 1000)
    }
}

/// `Sep 18` for a Unix time, in UTC.
fn month_day(unix_seconds: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // Howard Hinnant's days-to-civil conversion.
    let days = i64::try_from(unix_seconds / 86_400).unwrap_or(0) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let name = usize::try_from(month - 1)
        .ok()
        .and_then(|index| MONTHS.get(index))
        .copied()
        .unwrap_or("Jan");
    format!("{name} {day}")
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Pos2, Vec2};

    use super::{cover_uv, month_day};

    #[test]
    fn cover_crop_keeps_the_middle_band_of_a_wide_tile() {
        let uv = cover_uv(Vec2::new(320.0, 200.0), Vec2::new(196.0, 84.0));
        assert!((uv.min.x - 0.0).abs() < 1e-6 && (uv.max.x - 1.0).abs() < 1e-6);
        let band = 84.0 / (200.0 * 196.0 / 320.0);
        assert!((uv.height() - band).abs() < 1e-4);
        assert!((uv.center().y - 0.5).abs() < 1e-6);
        let tall = cover_uv(Vec2::new(320.0, 200.0), Vec2::new(40.0, 84.0));
        assert!((tall.height() - 1.0).abs() < 1e-6 && tall.width() < 1.0);
        assert_eq!(tall.center(), Pos2::new(0.5, 0.5));
    }

    #[test]
    fn capture_dates_read_as_month_and_day() {
        assert_eq!(month_day(0), "Jan 1");
        // 2026-09-18 12:00 UTC.
        assert_eq!(month_day(1_789_732_800), "Sep 18");
    }
}
