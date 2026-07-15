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
