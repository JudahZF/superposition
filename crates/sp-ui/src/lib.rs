//! Shared UI design foundations for Superposition.
//!
//! This crate deliberately contains design tokens and renderer-independent component models.
//! It has no rendering-framework dependency.

#![forbid(unsafe_code)]

/// Stable examples rendered by the component gallery and snapshot tests.
pub mod component_gallery;
/// Pure data models shared by future UI renderers.
pub mod components;
/// Design tokens and the dark application theme.
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

    fn assert_tokenized_first_argument(source: &str, call: &str) {
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
                "visual call {call:?} must start with a design token, not {first:?}"
            );
        }
    }

    #[test]
    fn raw_brand_palette_values_are_confined_to_the_design_bridge() {
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let approved = source_root.join("design/tokens.rs");
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut sources = Vec::new();
        rust_sources(&source_root, &mut sources);
        sources.push(workspace.join("apps/superposition/src/ui.rs"));

        for source in sources {
            if source == approved {
                continue;
            }
            let contents = fs::read_to_string(&source).expect("Rust source is readable");
            let production_source = production_source(&contents);
            for literal in [
                "Color::rgb(",
                "0x0A, 0x0F, 0x16",
                "0x12, 0x18, 0x20",
                "0x1B, 0x23, 0x30",
                "0x2A, 0x32, 0x42",
                "0xE6, 0xE8, 0xEC",
                "0x9A, 0xA3, 0xAE",
                "0x00, 0xE5, 0xFF",
                "0x00, 0x7A, 0xFF",
                "0xA6, 0xFF, 0x00",
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
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let source = fs::read_to_string(workspace.join("apps/superposition/src/ui.rs"))
            .expect("egui renderer source is readable");
        let bridge = source
            .split("fn token_color")
            .nth(1)
            .and_then(|source| source.split("\n}\n").next())
            .expect("token-color bridge exists");

        assert_eq!(
            source.matches("Color32::").count(),
            1,
            "only the token bridge may construct an egui color"
        );
        assert!(
            bridge.contains("Color32::from_rgb"),
            "the token-color bridge must resolve design colors"
        );

        for call in [
            ".exact_height(",
            ".exact_width(",
            ".inner_margin(",
            ".corner_radius(",
            ".stroke(",
            ".size(",
            ".min_size(",
            "Vec2::new(",
            "ui.set_min_width(",
            "ui.add_space(",
        ] {
            assert_tokenized_first_argument(&source, call);
        }
    }
}
