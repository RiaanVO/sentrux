//! NamespaceMap — Map C# namespaces to source file paths.
//!
//! Builds a bidirectional mapping between fully qualified C# namespaces
//! and their corresponding source files by analyzing `.csproj` compile
//! entries combined with namespace conventions.
//!
//! ## Algorithm
//!
//! For each compile file in a `.csproj`:
//! 1. Start with the project's `default_namespace` (or `root_namespace` or `assembly_name`)
//! 2. Split the file's relative path into directory components
//! 3. Join default_namespace + directory_parts + filename_stem
//! 4. Map the resulting namespace to the file path

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::analysis::resolver::dotnet::csproj_parser::{Csproj, GlobalUsing};

// Re-export for test convenience
use crate::analysis::resolver::dotnet::csproj_parser::CompileEntry;

/// Maps C# namespaces to source file paths.
///
/// The map supports both exact namespace lookups and suffix-based lookups
/// (for resolving partial namespace paths like "CartValidation" when the
/// full namespace is "Cart.Services.CartValidation").
///
/// ## Build
///
/// `NamespaceMap::build()` takes a slice of parsed `Csproj` files and
/// computes the namespace for each compile file using the project's
/// default namespace and the file's relative path.
#[derive(Debug, Clone)]
pub struct NamespaceMap {
    /// Exact namespace → file path mapping.
    /// e.g., "MyApp.Services.UserService" → "/project/src/Services/UserService.cs"
    by_namespace: HashMap<String, PathBuf>,
    /// File stem suffix → file path mapping (for partial lookups).
    /// e.g., ".UserService" → "/project/src/Services/UserService.cs"
    by_suffix: HashMap<String, PathBuf>,
}

impl NamespaceMap {
    /// Build a `NamespaceMap` from parsed `.csproj` files.
    ///
    /// For each `.csproj`, iterates over all compile entries and computes
    /// the expected namespace for each file:
    ///
    /// ```text
    /// default_namespace = "Cart"
    /// file = "src/Cart.Services/Validations/CartValidation.cs"
    /// namespace = "Cart.Cart.Services.Validations.CartValidation"
    /// ```
    ///
    /// If a namespace has already been mapped (from an earlier `.csproj`),
    /// the new mapping is added alongside it (multiple files can share a namespace).
    pub fn build(csprojects: &[Csproj]) -> Self {
        let mut by_namespace: HashMap<String, PathBuf> = HashMap::new();
        let mut by_suffix: HashMap<String, PathBuf> = HashMap::new();

        for csproj in csprojects {
            let default_ns = csproj.get_effective_default_namespace();
            let project_dir = csproj.path.parent().unwrap_or(Path::new(""));

            for entry in &csproj.compile_files {
                if entry.remove {
                    continue;
                }

                let full_path = project_dir.join(&entry.include);

                // Use explicit namespace if provided in .csproj
                let ns = if let Some(ref explicit_ns) = entry.explicit_namespace {
                    explicit_ns.clone()
                } else {
                    // Compute from file path
                    compute_namespace_for_file_relative(&entry.include, &default_ns)
                };

                // Register the full namespace
                by_namespace
                    .entry(ns.clone())
                    .or_insert_with(|| full_path.clone());

                // Register suffixes for partial matching
                register_suffixes(&ns, &full_path, &mut by_suffix);
            }
        }

        Self {
            by_namespace,
            by_suffix,
        }
    }

    /// Build a `NamespaceMap` with explicit namespace declarations extracted from .cs files.
    ///
    /// Unlike `build()`, this method reads actual `namespace X.Y.Z;` declarations
    /// from source files first, falling back to path-based computation only for
    /// files that don't have explicit declarations.
    pub fn build_with_declarations(csprojects: &[Csproj]) -> Self {
        let mut by_namespace: HashMap<String, PathBuf> = HashMap::new();
        let mut by_suffix: HashMap<String, PathBuf> = HashMap::new();

        // 1. Collect explicit namespace declarations from all .cs files
        for csproj in csprojects {
            let declarations = csproj.extract_namespace_declarations();

            for (file_path, namespace) in &declarations {
                // Register exact namespace
                by_namespace
                    .entry(namespace.clone())
                    .or_insert_with(|| file_path.clone());

                // Register suffixes for partial matching
                register_suffixes(namespace, file_path, &mut by_suffix);

                // Register all parent namespaces (prefixes)
                register_prefixes(namespace, file_path, &mut by_namespace);

                // Register namespace without the final stem (parent namespace).
                // This handles cases where the file path causes double-stemming,
                // e.g., file at "Steps/Steps/AddItem.cs" with declaration
                // "namespace Application.Pipeline.Steps.AddItem" might also need
                // "Application.Pipeline.Steps" registered so imports of the parent
                // namespace resolve correctly.
                register_namespace_without_stem(namespace, file_path, &mut by_namespace);
            }
        }

        // 2. Process compile entries for files without declarations
        for csproj in csprojects {
            let default_ns = csproj.get_effective_default_namespace();
            let project_dir = csproj.path.parent().unwrap_or(Path::new(""));

            for entry in &csproj.compile_files {
                if entry.remove {
                    continue;
                }
                let full_path = project_dir.join(&entry.include);
                if by_namespace.values().any(|p| p == &full_path) {
                    continue;
                }
                let ns = if let Some(ref explicit_ns) = entry.explicit_namespace {
                    explicit_ns.clone()
                } else {
                    compute_namespace_for_file_relative(&entry.include, &default_ns)
                };
                by_namespace.entry(ns.clone()).or_insert_with(|| full_path.clone());
                register_suffixes(&ns, &full_path, &mut by_suffix);
            }
        }

        // 3. Register GlobalUsings from each project
        for csproj in csprojects {
            let project_dir = csproj.path.parent().unwrap_or(Path::new(""));
            let default_ns = csproj.get_effective_default_namespace();
            let global_using_files = find_global_using_files(project_dir);
            for gu_file in &global_using_files {
                if let Some(usings) = parse_global_usings_file(gu_file) {
                    for using in &usings {
                        if let Some(alias) = &using.alias {
                            by_namespace.entry(alias.clone()).or_insert_with(|| gu_file.clone());
                        } else if !using.name.is_empty() {
                            let first_cs =
                                find_first_cs_file_in_project(project_dir, &default_ns);
                            let first_cs_path = first_cs.clone().unwrap_or_else(|| gu_file.clone());
                            by_namespace
                                .entry(using.name.clone())
                                .or_insert_with(|| first_cs_path.clone());
                            register_suffixes(
                                &using.name,
                                &first_cs_path,
                                &mut by_suffix,
                            );
                        }
                    }
                }
            }

            // 4. Register implicit SDK usings when ImplicitUsings is enabled
            //    (directly in .csproj or via Directory.Build.props/targets)
            let implicit_usings_enabled = csproj.implicit_usings.unwrap_or(false)
                || has_directory_build_file_with_implicit_usings(project_dir);
            if implicit_usings_enabled {
                register_implicit_sdk_usings(&mut by_namespace, project_dir);
            }
        }

        Self { by_namespace, by_suffix }
    }

    /// Resolve a namespace to its file path.
    ///
    /// Returns `Some(PathBuf)` if an exact match was found, `None` otherwise.
    pub fn resolve(&self, namespace: &str) -> Option<PathBuf> {
        self.by_namespace.get(namespace).cloned()
    }

    /// Resolve a namespace suffix to its file path.
    ///
    /// Looks for entries where the namespace ends with the given suffix.
    /// Useful for resolving partial namespace paths.
    pub fn resolve_by_suffix(&self, suffix: &str) -> Option<PathBuf> {
        self.by_suffix.get(suffix).cloned()
    }

    /// Get all namespaces in this map.
    pub fn namespaces(&self) -> impl Iterator<Item = &str> {
        self.by_namespace.keys().map(|k| k.as_str())
    }

    /// Get the total number of namespace entries.
    pub fn len(&self) -> usize {
        self.by_namespace.len()
    }

    /// Check if the map is empty.
    pub fn is_empty(&self) -> bool {
        self.by_namespace.is_empty()
    }

    /// Iterate over all namespace → file_path entries.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &PathBuf)> {
        self.by_namespace.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate over all namespace → file_path entries (owned namespace keys).
    ///
    /// Returns an iterator of `(namespace, file_path)` pairs where the namespace
    /// key is owned. Useful for substring/prefix matching against the full set.
    pub fn all_entries(&self) -> impl Iterator<Item = (String, PathBuf)> {
        self.by_namespace.clone().into_iter()
    }

    /// Get all file paths in this map.
    pub fn files(&self) -> impl Iterator<Item = &PathBuf> {
        self.by_namespace.values()
    }

    /// Find all namespaces that could match a given import path.
    ///
    /// Tries:
    /// 1. Exact namespace match
    /// 2. Progressive suffix matching (strip leftmost segments)
    /// 3. Stem matching (last component)
    pub fn find_candidates(&self, import_path: &str) -> Vec<(String, PathBuf)> {
        let mut candidates: Vec<(String, PathBuf)> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();

        // Exact match
        if let Some(path) = self.resolve(import_path) {
            if seen.insert(path.clone()) {
                candidates.push((import_path.to_string(), path));
            }
        }

        // Progressive suffix stripping
        let parts: Vec<&str> = import_path.split('.').collect();
        for i in 1..parts.len() {
            let partial: String = parts[i..].join(".");
            if let Some(path) = self.resolve(&partial) {
                if seen.insert(path.clone()) {
                    candidates.push((partial.clone(), path));
                }
            }
        }

        // Stem match
        if let Some(last) = parts.last() {
            let stem_key = format!(".{}", last);
            if let Some(path) = self.resolve_by_suffix(&stem_key) {
                if seen.insert(path.clone()) {
                    candidates.push((last.to_string(), path));
                }
            }
        }

        candidates
    }
}

/// Compute the expected namespace for a file relative to its project directory.
///
/// This is the core algorithm:
/// 1. Strip leading "src/" prefix if present
/// 2. Take remaining directory components as namespace parts
/// 3. Append the filename stem
/// 4. Prepend the default namespace
pub fn compute_namespace_for_file_relative(
    file_path: &Path,
    default_namespace: &str,
) -> String {
    let file_str = file_path.to_string_lossy();

    // Strip common leading directories
    let stripped = file_str
        .strip_prefix("src/")
        .or_else(|| file_str.strip_prefix("Src/"))
        .or_else(|| file_str.strip_prefix("source/"))
        .or_else(|| file_str.strip_prefix("Source/"))
        .unwrap_or(&file_str);

    // Get directory parts and file stem
    let parent_dir = Path::new(stripped).parent();
    let file_stem = Path::new(stripped).file_stem().and_then(|s| s.to_str());

    let mut parts: Vec<String> = Vec::new();

    if let Some(dir) = parent_dir {
        for component in dir.components() {
            if let std::path::Component::Normal(s) = component {
                if let Some(name) = s.to_str() {
                    parts.push(name.to_string());
                }
            }
        }
    }

    let dir_namespace = parts.join(".");

    let mut full_parts: Vec<String> = Vec::new();
    if !default_namespace.is_empty() {
        full_parts.push(default_namespace.to_string());
    }
    if !dir_namespace.is_empty() {
        full_parts.push(dir_namespace);
    }
    if let Some(stem) = file_stem {
        full_parts.push(stem.to_string());
    }

    full_parts.join(".")
}

/// Register all parent namespaces (prefixes) of a full namespace.
/// For 'Common.Cart.Graph.Middleware.UserService', registers:
///   Common.Cart, Common.Cart.Graph, Common.Cart.Graph.Middleware
fn register_prefixes(namespace: &str, file_path: &PathBuf, ns_map: &mut HashMap<String, PathBuf>) {
    let parts: Vec<&str> = namespace.split('.').collect();
    let mut dot_count = 0usize;
    let mut prefix = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            prefix.push('.');
            dot_count += 1;
        }
        prefix.push_str(part);
        if dot_count >= 1 {
            ns_map.entry(prefix.clone()).or_insert_with(|| file_path.clone());
        }
    }
}

/// Register the namespace without its final stem (i.e., the parent namespace).
///
/// For 'Application.Pipeline.Steps.AddItem', registers 'Application.Pipeline.Steps'.
/// This handles cases where:
/// - The file path causes double-stemming (e.g., Steps/Steps/AddItem.cs → ...
///   computed namespace 'Application.Pipeline.Steps.AddItem.AddItem')
/// - Imports reference the parent namespace rather than the full class namespace
fn register_namespace_without_stem(namespace: &str, file_path: &PathBuf, ns_map: &mut HashMap<String, PathBuf>) {
    let parts: Vec<&str> = namespace.split('.').collect();
    if parts.len() >= 2 {
        let ns_without_stem = parts[..parts.len() - 1].join(".");
        ns_map.entry(ns_without_stem).or_insert_with(|| file_path.clone());
    }
}

/// Register all suffix variants of a namespace for partial matching.
///
/// For "MyApp.Services.UserService", registers:
/// - ".UserService" → file
/// - ".Services.UserService" → file
/// - ".MyApp.Services.UserService" → file
fn register_suffixes(namespace: &str, file_path: &Path, suffix_map: &mut HashMap<String, PathBuf>) {
    let parts: Vec<&str> = namespace.split('.').collect();

    // Register suffixes from right to left
    for i in 0..parts.len() {
        let suffix: String = format!(".{}", parts[i..].join("."));
        suffix_map
            .entry(suffix)
            .or_insert_with(|| file_path.to_path_buf());
    }

    // Also register the stem alone (most specific match)
    if let Some(last) = parts.last() {
        suffix_map
            .entry(format!(".{}", last))
            .or_insert_with(|| file_path.to_path_buf());
    }
}

/// Extract type names from a C# source file (simple heuristic).
///
/// Looks for lines containing `public class`, `public struct`, `public interface`,
/// or `public record` followed by an identifier.
pub fn extract_types_from_file(file_path: &Path) -> Vec<String> {
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    let mut types: Vec<String> = Vec::new();
    let type_kinds = ["public class", "public struct", "public interface", "public record"];

    for line in content.lines() {
        let trimmed = line.trim();
        for kind in &type_kinds {
            if trimmed.contains(kind) {
                let remainder = trimmed.trim_start_matches(kind).trim();
                // Extract identifier before '{', 'where', or ';'
                if let Some(pos) = remainder.find(|c| ['{', ';', '('].contains(&c)) {
                    let ident = remainder[..pos].trim();
                    if !ident.is_empty() && !ident.contains('<') {
                        types.push(ident.to_string());
                    }
                } else {
                    let ident = remainder.split_whitespace().next();
                    if let Some(id) = ident {
                        types.push(id.to_string());
                    }
                }
                break;
            }
        }
    }

    types
}

/// Find all GlobalUsings.cs files recursively under the given directory.
fn find_global_using_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    _find_gu_recursive(dir, &mut files);
    files
}

/// Recursive helper for finding GlobalUsings.cs files.
fn _find_gu_recursive(dir: &Path, files: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let skip = matches!(
                    name,
                    "bin" | "obj" | ".git" | "node_modules" | ".nuget" | "packages"
                );
                if !skip {
                    _find_gu_recursive(&path, files);
                }
            } else if path.file_name().map_or(false, |n| n == "GlobalUsings.cs") {
                files.push(path);
            }
        }
    }
}

/// Check for Directory.Build.props or Directory.Build.targets files that might enable
/// ImplicitUsings globally for the entire project.
fn has_directory_build_file_with_implicit_usings(dir: &Path) -> bool {
    // Search upwards through parent directories for Directory.Build.props/targets
    let mut current = dir.to_path_buf();
    loop {
        for filename in ["Directory.Build.props", "Directory.Build.targets"] {
            let path = current.join(filename);
            if path.exists() {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    // Check if ImplicitUsings is set to enable/true
                    if content.contains("<ImplicitUsings>")
                        && (content.contains("<ImplicitUsings>enable</ImplicitUsings>")
                            || content.contains("<ImplicitUsings>true</ImplicitUsings>")
                            || content.contains("<ImplicitUsings>1</ImplicitUsings>"))
                    {
                        return true;
                    }
                }
            }
        }
        // Walk up one directory
        match current.parent() {
            Some(parent) if parent.to_string_lossy() != current.to_string_lossy() => {
                current = parent.to_path_buf();
            }
            _ => break,
        }
    }
    false
}

/// Parse global using directives from a GlobalUsings.cs file.
pub fn parse_global_usings_file(path: &Path) -> Option<Vec<GlobalUsing>> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut usings = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("global using ") {
            let rest = rest.trim_end_matches(';').trim();
            if let Some(eq_pos) = rest.find(" = ") {
                let alias = rest[..eq_pos].trim().to_string();
                let name = rest[eq_pos + 3..].trim().to_string();
                usings.push(GlobalUsing { name, alias: Some(alias) });
            } else {
                let name = rest.to_string();
                if !name.is_empty() {
                    usings.push(GlobalUsing { name, alias: None });
                }
            }
        }
    }
    if !usings.is_empty() {
        Some(usings)
    } else {
        None
    }
}

/// Register common .NET SDK implicit usings when a project has ImplicitUsings enabled.
///
/// When ImplicitUsings is set to "enable" or "true" in a .csproj (or via Directory.Build.props),
/// the C# compiler automatically imports these namespaces. They don't appear in any source file
/// but are part of compilation. We register them here so that imports like `using System.Linq;`
/// can resolve successfully.
fn register_implicit_sdk_usings(ns_map: &mut HashMap<String, PathBuf>, project_dir: &Path) {
    let implicit_ns = vec![
        "System",
        "System.Collections",
        "System.Collections.Generic",
        "System.Linq",
        "System.Threading",
        "System.Threading.Tasks",
        "System.IO",
        "System.Net.Http",
        "System.Text",
        "System.Text.Json",
        "System.Runtime.CompilerServices",
        "System.Diagnostics",
        "System.Globalization",
        "System.Security.Claims",
        "System.Reflection",
        "System.Xml",
    ];
    for ns in implicit_ns {
        // Register to a dummy path since there's no source file.
        // The key insight: we register so that 'using System.Linq' resolves,
        // even though there's no actual file for it. We just need the entry to exist.
        ns_map
            .entry(ns.to_string())
            .or_insert_with(|| project_dir.to_path_buf());
    }
}

/// Find the first .cs file in a project directory recursively.
fn find_first_cs_file_in_project(project_dir: &Path, _default_ns: &str) -> Option<PathBuf> {
    let mut queue = vec![project_dir.to_path_buf()];
    while let Some(dir) = queue.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_file()
                    && path
                        .extension()
                        .is_some_and(|ext| ext == "cs")
                {
                    return Some(path);
                }
                if path.is_dir() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let skip = matches!(
                        name,
                        "bin" | "obj" | ".git" | "node_modules" | ".nuget" | "packages"
                    );
                    if !skip {
                        queue.push(path);
                    }
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_csproj(path: &str, default_ns: &str, files: Vec<(&str, bool)>) -> Csproj {
        Csproj {
            path: PathBuf::from(path),
            assembly_name: Some("MyApp".to_string()),
            root_namespace: None,
            default_namespace: Some(default_ns.to_string()),
            target_framework: None,
            implicit_usings: None,
            nullable: None,
            compile_files: files
                .into_iter()
                .map(|(include, remove)| CompileEntry {
                    include: PathBuf::from(include),
                    remove,
                    explicit_namespace: None,
                })
                .collect(),
            project_references: Vec::new(),
            package_references: Vec::new(),
            global_usings: Vec::new(),
        }
    }

    #[test]
    fn test_build_basic_map() {
        let csproj = make_csproj(
            "/project/MyApp.csproj",
            "MyApp",
            vec![
                ("src/Services/UserService.cs", false),
                ("src/Models/User.cs", false),
                ("src/Program.cs", false),
            ],
        );

        let map = NamespaceMap::build(&[csproj]);

        assert_eq!(map.len(), 3);

        // Verify full namespaces
        let svc = map.resolve("MyApp.Services.UserService");
        assert!(svc.is_some());
        assert!(svc.unwrap().to_string_lossy().ends_with("UserService.cs"));

        let model = map.resolve("MyApp.Models.User");
        assert!(model.is_some());
        assert!(model.unwrap().to_string_lossy().ends_with("User.cs"));
    }

    #[test]
    fn test_build_map_stem_match() {
        let csproj = make_csproj(
            "/project/MyApp.csproj",
            "MyApp",
            vec![("src/Services/UserService.cs", false)],
        );

        let map = NamespaceMap::build(&[csproj]);

        // Stem-only match should work
        let result = map.resolve_by_suffix(".UserService");
        assert!(result.is_some());
        assert!(result.unwrap().to_string_lossy().ends_with("UserService.cs"));
    }

    #[test]
    fn test_build_excludes_removed_files() {
        let csproj = make_csproj(
            "/project/MyApp.csproj",
            "MyApp",
            vec![
                ("src/Service.cs", false),
                ("Models/**", true),
            ],
        );

        let map = NamespaceMap::build(&[csproj]);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn test_find_candidates_exactly() {
        let csproj = make_csproj(
            "/project/MyApp.csproj",
            "MyApp",
            vec![("src/Services/Validators/AuthValidator.cs", false)],
        );

        let map = NamespaceMap::build(&[csproj]);

        let candidates = map.find_candidates("MyApp.Services.Validators.AuthValidator");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "MyApp.Services.Validators.AuthValidator");
    }

    #[test]
    fn test_find_candidates_partial_suffix() {
        let csproj = make_csproj(
            "/project/MyApp.csproj",
            "MyApp",
            vec![("src/Services/Validators/AuthValidator.cs", false)],
        );

        let map = NamespaceMap::build(&[csproj]);

        // Stripping "MyApp.Services." should find "Validators.AuthValidator"
        let candidates = map.find_candidates("Validators.AuthValidator");
        assert!(!candidates.is_empty());
    }

    #[test]
    fn test_compute_namespace_from_file_relative() {
        let ns = compute_namespace_for_file_relative(
            Path::new("src/Domain/Aggregates/Order.cs"),
            "MyApp",
        );
        assert_eq!(ns, "MyApp.Domain.Aggregates.Order");

        let ns = compute_namespace_for_file_relative(Path::new("Order.cs"), "MyApp");
        assert_eq!(ns, "MyApp.Order");

        let ns = compute_namespace_for_file_relative(Path::new("src/Order.cs"), "MyApp");
        assert_eq!(ns, "MyApp.Order");
    }

    #[test]
    fn test_empty_map() {
        let map = NamespaceMap::build(&[]);
        assert!(map.is_empty());
        assert_eq!(map.len(), 0);
    }
}
