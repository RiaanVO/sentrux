//! Sentrux core library — structural quality analysis engine.
//!
//! This crate contains all analysis, metrics, visualization, and MCP server logic.
//! It is consumed by:
//! - `sentrux-bin` (the main binary — GUI, CLI, MCP entry points)
//! - `sentrux-pro` (private crate — pro tool handlers, license validation)
//!
//! All modules are `pub` so that external crates can access types like
//! `ToolDef`, `McpState`, `Tier`, `Snapshot`, etc.

/// Debug logging — only prints when SENTRUX_DEBUG=1.
/// Release builds show nothing unless the user opts in.
#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if std::env::var("SENTRUX_DEBUG").is_ok() {
            eprintln!($($arg)*);
        }
    };
}

pub mod analysis;
pub mod app;
pub mod core;
pub mod layout;
pub mod license;
pub mod metrics;
pub mod plugin_loader;
pub mod pro_registry;
pub mod renderer;
