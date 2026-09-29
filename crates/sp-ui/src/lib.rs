//! Shared UI design foundations for Superposition.
//!
//! This crate contains the film-strip design tokens and renderer-independent component models.
//! It has no rendering-framework dependency.

#![forbid(unsafe_code)]

/// Pure data models shared by UI renderers.
pub mod components;
/// Design tokens, typography, and accessibility rules.
pub mod design;

#[cfg(test)]
mod design_source_tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    fn rust_sources(directory: &Path, sources: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(directory).expect("design source directory is readable") {
            let path = entry.expect("design source entry is readable").path();
            if path.is_dir() {
                rust_sources(&path, sources);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                sources.push(path);
            }
        }
    }

    fn production_source(contents: &str) -> &str {
        contents.split("#[cfg(test)]").next().unwrap_or(contents)
    }

    /// The egui renderer: `ui.rs` and everything under `ui/`.
    fn renderer_sources() -> Vec<PathBuf> {
        let app = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/superposition/src");
        let mut sources = vec![app.join("ui.rs")];
        rust_sources(&app.join("ui"), &mut sources);
        sources
    }

    fn assert_tokenized_first_argument(path: &Path, source: &str, call: &str) {
        let mut remaining = source;
        while let Some(position) = remaining.find(call) {
            remaining = &remaining[position + call.len()..];
            let first = remaining
                .trim_start()
                .chars()
                .next()
                .expect("visual call has an argument");
            assert!(
                !first.is_ascii_digit() && !matches!(first, '+' | '-' | '.'),
                "visual call {call:?} in {} must start with a design token, not {first:?}",
                path.display()
            );
        }
    }

    #[test]
    fn raw_palette_values_are_confined_to_the_token_module() {
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let approved = source_root.join("design/tokens.rs");
        let mut sources = Vec::new();
        rust_sources(&source_root, &mut sources);
        sources.extend(renderer_sources());

        for source in sources {
            if source == approved {
                continue;
            }
            let contents = fs::read_to_string(&source).expect("Rust source is readable");
            let production_source = production_source(&contents);
            for literal in [
                "Color::rgb(",
                "0x12, 0x13, 0x15",
                "0xD9, 0xDA, 0xD6",
                "0x7C, 0x80, 0x88",
                "0x3A, 0x3E, 0x45",
                "0x26, 0x29, 0x2E",
                "0x24, 0x27, 0x2C",
                "0x1B, 0x1D, 0x21",
                "0xD9, 0xA2, 0x1B",
                "0xE5, 0x48, 0x4D",
                "0x7F, 0xB0, 0xFF",
                "0x1A, 0x12, 0x14",
            ] {
                assert!(
                    !production_source.contains(literal),
                    "raw palette literal {literal:?} found outside {}",
                    approved.display()
                );
            }
        }
    }

    #[test]
    fn egui_renderer_uses_the_token_bridge_for_visual_values() {
        let mut constructors = 0;
        for path in renderer_sources() {
            let contents = fs::read_to_string(&path).expect("egui renderer source is readable");
            let source = production_source(&contents);
            constructors += source.matches("Color32::from").count();
            for call in [
                ".exact_height(",
                ".exact_width(",
                ".inner_margin(",
                ".corner_radius(",
                ".min_size(",
                "Vec2::new(",
                "vec2(",
                "ui.set_min_width(",
                "ui.add_space(",
                "FontId::new(",
            ] {
                assert_tokenized_first_argument(&path, source, call);
            }
        }
        assert_eq!(
            constructors, 1,
            "only the token bridge may construct an egui colour"
        );
    }
}
