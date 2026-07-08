//! Molt engine — shared between the CLI and the GUI.
pub mod formats;
pub mod punch;
pub mod util;

/// Molt's version: `major.minor.hotfix` (from Cargo.toml).
///
/// - **major**: big feature drops / redesigns
/// - **minor**: small features and behavior tweaks
/// - **hotfix**: bug fixes only (starts at 1, not 0)
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
