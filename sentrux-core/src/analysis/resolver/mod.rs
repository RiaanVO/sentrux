//! Import resolution — resolves import specifiers to file paths.
//!
//! Unified suffix-index resolver for ALL languages. No tier split.
//! JS/TS path aliases (tsconfig.json) handled via plugin-declared config.
//! C# imports resolved via .csproj XML parsing and namespace mapping.

pub mod helpers;
pub mod suffix;
pub mod dotnet;
