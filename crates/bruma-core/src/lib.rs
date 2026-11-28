//! Windowless core of bruma.
//!
//! Everything here is pure: a [`session::Session`] turns input into effects,
//! and a [`render::Renderer`] turns a session into pixels. No window, no OS.

pub mod backdrop;
pub mod blur;
pub mod clip;
pub mod compare;
pub mod diff;
pub mod doc;
pub mod draw;
pub mod export;
pub mod geom;
pub mod glyph;
pub mod image;
pub mod intro;
pub mod links;
pub mod logo;
pub mod menu;
pub mod par;
pub mod redact;
pub mod render;
pub mod session;
pub mod settings;
pub mod snap;
pub mod sound;
pub mod stitch;
pub mod strip;
pub mod text;
pub mod theme;

pub use tiny_skia;

pub use geom::{Point, Rect};
pub use image::Image;
