pub mod agent_ids;
pub mod bridges;
pub mod config;
pub mod dialog;
pub mod layout;
pub mod live;
pub mod reducer;
pub mod release;
pub mod view;

#[cfg(unix)]
pub mod state_stream;

pub(crate) mod bars;
/// Public so embedders reuse the sidebar's own formatters instead of
/// reimplementing them: `duration_ms` and `age` back the trace detail panel,
/// and a second implementation would drift from what the cards render.
pub mod format;
pub(crate) mod metrics;
pub(crate) mod select;
/// Public for the same reason as `format`: `CardCtx` requires
/// `AgentAppearances`, so a host cannot build a card without naming this
/// module's types.
pub mod style;

#[cfg(unix)]
pub mod settings_file;

#[cfg(all(feature = "runtime", unix))]
pub mod tui;
