//! Codec-specific parsers for extracting frame metadata

pub mod av1_parser;
pub mod vp9_parser;
pub mod vp8_parser;

pub use av1_parser::{Av1FrameHeader, FrameType};
pub use vp9_parser::{Vp9FrameHeader, Vp9FrameType, Vp9RefFrame};
pub use vp8_parser::{Vp8FrameHeader, Vp8FrameType, Vp8RefFrame};
