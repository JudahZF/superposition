//! Editor pictures: the newest capture per plug-in instance, uploaded once as egui textures,
//! and the thumbnails and fitted pictures that show them.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use eframe::egui::{
    self, Align2, ColorImage, Pos2, Rect, Response, Sense, TextureHandle, TextureOptions, Ui, Vec2,
};
use sp_ui::components::StateToken;
use sp_ui::design::{ColorToken, Layout, Radius, Spacing, TypographyRole};

use super::style::{color, hairline, line, paint_text, px, radius, sp};
use sp_session::EditorPreviewFile;

/// Picture opacity for bypassed slots, and for unavailable ones drawn in grayscale.
const BYPASSED_OPACITY: f32 = 0.4;
const UNAVAILABLE_OPACITY: f32 = 0.35;

/// One stored picture with its colour and grayscale textures.
pub(super) struct Preview {
    pub(super) file: EditorPreviewFile,
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

/// Whether a slot's state draws its picture dimmed and in grayscale.
fn unavailable(token: StateToken) -> bool {
    matches!(
        token,
        StateToken::Missing
            | StateToken::Faulted
            | StateToken::Loading
            | StateToken::Recovering
            | StateToken::Unloaded
    )
}

/// The texture and opacity a slot in `token` state draws its picture with.
fn treatment(picture: &Preview, token: StateToken) -> (&TextureHandle, f32) {
    if unavailable(token) {
        (&picture.gray, UNAVAILABLE_OPACITY)
    } else if token == StateToken::Bypassed {
        (&picture.color, BYPASSED_OPACITY)
    } else {
        (&picture.color, 1.0)
    }
}

/// A rounded thumbnail of a slot's editor picture, cropped to cover `rect`. A slot without a
/// picture shows an empty tile.
pub(super) fn thumbnail(
    ui: &Ui,
    rect: Rect,
    picture: Option<&Preview>,
    token: StateToken,
    corner: Radius,
) {
    ui.painter()
        .rect_filled(rect, radius(corner), color(ColorToken::SegmentOff));
    if let Some(picture) = picture {
        let (texture, opacity) = treatment(picture, token);
        egui::Image::new((texture.id(), rect.size()))
            .uv(cover_uv(texture.size_vec2(), rect.size()))
            .corner_radius(radius(corner))
            .tint(egui::Color32::WHITE.gamma_multiply(opacity))
            .paint_at(ui, rect);
    }
}

/// A slot's editor picture scaled to fit inside `rect`, centred, and at most twice its captured
/// size so a small capture never smears across a large pane. Returns where it drew.
pub(super) fn fitted(ui: &Ui, rect: Rect, picture: &Preview, token: StateToken) -> Rect {
    const MAX_SCALE: f32 = 2.0;
    let (texture, opacity) = treatment(picture, token);
    let size = texture.size_vec2();
    let scale = (rect.width() / size.x)
        .min(rect.height() / size.y)
        .min(MAX_SCALE);
    let shown = Rect::from_center_size(rect.center(), size * scale);
    egui::Image::new((texture.id(), shown.size()))
        .corner_radius(radius(Radius::Control))
        .tint(egui::Color32::WHITE.gamma_multiply(opacity))
        .paint_at(ui, shown);
    shown
}

/// Shows the full capture beside `rect` while `response` is hovered.
pub(super) fn peek_on_hover(
    ui: &Ui,
    response: &Response,
    rect: Rect,
    name: &str,
    picture: Option<&Preview>,
) {
    if response.hovered()
        && let Some(picture) = picture
    {
        peek(ui, rect, response.id, name, picture);
    }
}

/// The full 320×200 capture in a rounded card beside `rect`.
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
            painter.rect_filled(frame, radius(Radius::Control), color(ColorToken::Surface));
            let inner = frame.shrink(px(Layout::Hairline));
            egui::Image::new((picture.color.id(), inner.size()))
                .corner_radius(radius(Radius::Control))
                .paint_at(ui, inner);
            let bar = Rect::from_min_max(
                Pos2::new(
                    inner.left(),
                    inner.bottom() - px(Layout::Caption) - sp(Spacing::S4),
                ),
                inner.max,
            );
            ui.painter().rect_filled(
                bar,
                egui::CornerRadius {
                    sw: Radius::Control.pixels(),
                    se: Radius::Control.pixels(),
                    ..egui::CornerRadius::ZERO
                },
                color(ColorToken::Surface),
            );
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
            ui.painter().rect_stroke(
                frame,
                radius(Radius::Control),
                hairline(ColorToken::Hairline),
                egui::StrokeKind::Inside,
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
pub(super) fn age_label(captured_at_unix_ms: u64) -> String {
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
