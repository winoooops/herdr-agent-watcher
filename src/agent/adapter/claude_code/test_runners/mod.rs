pub mod build;
pub mod cargo;
pub mod emitter;
pub mod matcher;
pub mod path_resolution;
pub mod preview;
pub mod sanitiser;
pub mod script_resolution;
pub mod test_file_patterns;
pub mod timestamps;
pub mod types;
pub mod vitest;

use once_cell::sync::Lazy;
use regex::Regex;

use types::TestRunner;

pub static RUNNERS: &[&TestRunner] = &[&vitest::VITEST, &cargo::CARGO_TEST];

/// Shared ANSI escape-sequence stripper used by every runner parser
/// (`vitest.rs`, `cargo.rs`) and the snapshot builder (`build.rs`).
/// Centralised here so a future pattern change (e.g. handling OSC sequences)
/// is a one-line update instead of three.
pub static ANSI_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap());
