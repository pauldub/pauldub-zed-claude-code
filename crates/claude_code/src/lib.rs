mod model;
mod tools;

pub use model::*;
pub use tools::*;

use gpui::App;

/// Register the Claude Code provider
/// We used to register settings here, but they're now part of language_models
pub fn init(_cx: &mut App) {
    // Nothing to do here
}
