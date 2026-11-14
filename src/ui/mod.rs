//! Slint user interface and application state.

mod app;
pub mod format;

slint::include_modules!();

pub use app::run;
