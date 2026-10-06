//! C# Import Resolver — resolves C# imports via .csproj analysis.
//!
//! Combines XML parsing of .csproj files with namespace-to-file mapping
//! to resolve C# imports that tree-sitter cannot handle (namespace-level
//! imports, cross-project references, NuGet-qualified types).
//!
//! ## Modules
//!
//! - `csproj_parser` — Parse .csproj XML files into structured metadata
//! - `namespace_map` — Build and query namespace → file_path mappings
//! - `dotnet_client` — Optional dotnet CLI wrapper for advanced resolution

pub mod csproj_parser;
pub mod namespace_map;
pub mod dotnet_client;

use std::path::{Path, PathBuf};

use csproj_parser::Csproj;
use namespace_map::NamespaceMap;

/// A single resolved import target discovered by the resolver.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    /// The fully qualified namespace (e.g., "Cart.Services.CartValidation")
    pub namespace: String,
    /// Absolute path to the source file
    pub file_path: PathBuf,
    /// Optional list of type names found in this namespace within the file
    pub types: Vec<String>,
}

/// Error types for C# import resolution failures.
#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("Failed to read csproj at {path}: {source}")]
    ReadError { path: PathBuf, source: std::io::Error },

    #[error("Failed to parse csproj at {path}")]
    ParseError { path: PathBuf, source: quick_xml::errors::Error },

    #[error("dotnet CLI not found at {path}")]
    DotnetNotFound { path: PathBuf },

    #[error("dotnet CLI command '{cmd}' failed: {source}")]
    DotnetCommandError { cmd: String, source: std::io::Error },

    #[error("No .csproj files found in {root}")]
    NoCsprojFound { root: PathBuf },

    #[error("Resolution failed for '{import_path}' from '{source_file}': {reason}")]
    ResolutionFailed {
        import_path: String,
        source_file: PathBuf,
        reason: String,
    },
}

/// Main resolver that ties together .csproj parsing, namespace mapping,
/// and optional dotnet CLI resolution.
#[derive(Debug)]
pub struct DotnetResolver {
    /// Path to the dotnet CLI (if available).
    dotnet_client: Option<dotnet_client::DotnetClient>,
    /// Parsed .csproj files found during build.
    pub csprojects: Vec<Csproj>,
    /// Namespace → file_path mapping.
    namespace_map: NamespaceMap,
}

impl DotnetResolver {
    /// Create a new `DotnetResolver` by scanning `scan_root` for .csproj files.
    ///
    /// This is a convenience method that:
    /// 1. Discovers all .csproj files under `scan_root`
    /// 2. Parses each .csproj
    /// 3. Builds the namespace map
    /// 4. Attempts to locate the dotnet CLI (optional)
    pub fn new(scan_root: &Path) -> Result<Self, ResolverError> {
        let csproj_paths = Self::find_csproj_files(scan_root)?;
        let csprojects: Vec<Csproj> = csproj_paths
            .iter()
            .map(|p| {
                Csproj::parse(p).map_err(|e| ResolverError::ParseError {
                    path: p.clone(),
                    source: e,
                })
            })
            .collect::<Result<Vec<_>, ResolverError>>()?;

        let namespace_map = NamespaceMap::build_with_declarations(&csprojects);
        let dotnet_client = dotnet_client::DotnetClient::new().ok();

        Ok(Self {
            dotnet_client,
            csprojects,
            namespace_map,
        })
    }

    /// Create a `DotnetResolver` from pre-parsed `.csproj` files.
    ///
    /// Useful when .csproj files have already been parsed (e.g., from cache).
    pub fn from_csprojects(csprojects: Vec<Csproj>) -> Self {
        let namespace_map = NamespaceMap::build_with_declarations(&csprojects);
        let dotnet_client = dotnet_client::DotnetClient::new().ok();

        Self {
            dotnet_client,
            csprojects,
            namespace_map,
        }
    }

    /// Find all `.csproj` files recursively under `scan_root`.
    ///
    /// Skips `bin`, `obj`, `.git`, `node_modules`, `.nuget`, and `packages` directories.
    fn find_csproj_files(root: &Path) -> Result<Vec<PathBuf>, ResolverError> {
        let mut results: Vec<PathBuf> = Vec::new();
        Self::_find_csproj_recursive(root, &mut results);
        results.sort();

        if results.is_empty() {
            return Err(ResolverError::NoCsprojFound { root: root.to_path_buf() });
        }

        Ok(results)
    }

    /// Recursive helper for finding `.csproj` files.
    fn _find_csproj_recursive(dir: &Path, results: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    let skip = path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .map(|name| {
                            name == "bin"
                                || name == "obj"
                                || name == ".git"
                                || name == "node_modules"
                                || name == ".nuget"
                                || name == "packages"
                        })
                        .unwrap_or(false);

                    if !skip {
                        Self::_find_csproj_recursive(&path, results);
                    }
                } else if path.extension().is_some_and(|ext| ext == "csproj") {
                    results.push(path);
                }
            }
        }
    }

    /// Resolve all imports in the given `.csproj` files.
    ///
    /// Returns a `NamespaceMap` mapping each discovered namespace to its source file.
    pub fn resolve_imports(&self) -> NamespaceMap {
        self.namespace_map.clone()
    }

    /// Resolve a single import path against the namespace map.
    ///
    /// `import_path` is typically a fully qualified namespace like
    /// `"Cart.Services.CartValidation"` and `source_file` is the file
    /// containing the import statement.
    ///
    /// Returns the file path if a matching namespace was found,
    /// or an error describing why resolution failed.
    pub fn resolve(
        &self,
        import_path: &str,
        source_file: &Path,
    ) -> Result<PathBuf, ResolverError> {
        // 1. Direct namespace match
        if let Some(path) = self.namespace_map.resolve(import_path) {
            return Ok(path);
        }

        // 2a. Try progressive prefix stripping (strip leftmost segments)
        // e.g., "Cart.Services.CartValidation" -> "Services.CartValidation" -> "CartValidation"
        let parts: Vec<&str> = import_path.split('.').collect();
        for i in 1..parts.len() {
            let partial: String = parts[i..].join(".");
            if partial.is_empty() {
                continue;
            }
            if let Some(path) = self.namespace_map.resolve(&partial) {
                return Ok(path);
            }
        }

        // 2b. Try progressive suffix stripping (strip rightmost segments)
        // e.g., "Application.Pipeline.Steps.AddItem" -> "Application.Pipeline.Steps" -> "Application.Pipeline"
        // This handles cases where the import is a parent namespace that was registered
        // via register_namespace_without_stem or register_prefixes.
        for i in (1..parts.len()).rev() {
            let partial: String = parts[..i].join(".");
            if partial.is_empty() {
                continue;
            }
            if let Some(path) = self.namespace_map.resolve(&partial) {
                return Ok(path);
            }
        }

        // 3. Try filename stem match (without extension)
        let stem = import_path.split('.').next_back().unwrap_or(import_path);
        let stem_key = format!(".{}", stem);
        if let Some(path) = self.namespace_map.resolve_by_suffix(&stem_key) {
            return Ok(path);
        }

        // 4. Bidirectional containment check
        // Handles both:
        //   - ns.contains(import_path): e.g., "Common.Cart.Services.Base" contains "Cart.Services"
        //   - import_path.contains(ns): e.g., "Application.Pipeline.Steps.AddItem" contains "Application.Pipeline.Steps"
        for (ns, path) in self.namespace_map.all_entries() {
            if ns.contains(import_path) || import_path.contains(&ns) {
                return Ok(path);
            }
        }

        // 5. Substring directory match
        // For 'Cart.Services', find files whose namespace path components contain 'Cart.Services'
        let import_dir = import_path.replace('.', "/");
        for (ns, path) in self.namespace_map.all_entries() {
            let ns_dir = ns.replace('.', "/");
            if ns_dir.contains(&import_dir) {
                return Ok(path);
            }
        }

        // 6. If dotnet client is available, try it as a fallback
        if let Some(ref client) = self.dotnet_client {
            if let Some(paths) = client.resolve_namespace(import_path) {
                if !paths.is_empty() {
                    return Ok(paths[0].clone());
                }
            }
        }

        Err(ResolverError::ResolutionFailed {
            import_path: import_path.to_string(),
            source_file: source_file.to_path_buf(),
            reason: format!(
                "No matching file found for namespace '{}'. \
                 Total known namespaces: {}. \
                 Try increasing logging to debug.",
                import_path,
                self.namespace_map.len()
            ),
        })
    }

    /// Resolve an import and return full target info (namespace + types).
    pub fn resolve_with_types(
        &self,
        import_path: &str,
        source_file: &Path,
    ) -> Result<ResolvedTarget, ResolverError> {
        let file_path = self.resolve(import_path, source_file)?;

        // Extract type names from the file (simple heuristic: public class/struct names)
        let types = namespace_map::extract_types_from_file(&file_path);

        Ok(ResolvedTarget {
            namespace: import_path.to_string(),
            file_path,
            types,
        })
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_csproj_error() {
        let result = DotnetResolver::new(Path::new("/nonexistent/path"));
        assert!(result.is_err());
        match result.unwrap_err() {
            ResolverError::NoCsprojFound { .. } => {}
            other => panic!("Expected NoCsprojFound, got: {:?}", other),
        }
    }

    #[test]
    fn test_csproj_parser_error() {
        // Test that parsing a non-existent file returns an error
        let result = Csproj::parse(Path::new("/nonexistent/path/does/not/exist.csproj"));
        assert!(result.is_err());
    }
}
