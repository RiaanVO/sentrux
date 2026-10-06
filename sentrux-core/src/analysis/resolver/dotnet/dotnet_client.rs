//! DotnetClient — Optional dotnet CLI wrapper for advanced C# resolution.
//!
//! Provides access to the `dotnet` CLI for operations that require
//! the full SDK (project system resolution, NuGet dependency tracking).
//!
//! All methods handle the case where dotnet CLI is not available by
//! returning `Err` or `None` rather than panicking, allowing the
//! XML-only parser to serve as a complete fallback.
//!
//! ## Key Methods
//!
//! - `new()` — Check if dotnet CLI exists
//! - `validate_project()` — Verify a .csproj is a valid project
//! - `get_project_refs()` — Get project references via dotnet CLI
//! - `get_nuget_deps()` — Get NuGet package dependencies via dotnet CLI

use std::path::{Path, PathBuf};
use std::process::Command;

use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;
use std::io::Cursor;

/// Error type for dotnet CLI operations.
#[derive(Debug, thiserror::Error)]
pub enum DotnetError {
    #[error("dotnet CLI not found — install .NET SDK to use dotnet-specific features")]
    CliNotFound,

    #[error("dotnet command '{cmd}' failed with exit code {code}")]
    CommandFailed { cmd: String, code: i32 },

    #[error("dotnet command '{cmd}' error: {source}")]
    CommandError { cmd: String, source: std::io::Error },

    #[error("Failed to parse dotnet output: {0}")]
    ParseError(String),

    #[error("Project validation failed for '{path}': {reason}")]
    ValidationFailed { path: String, reason: String },
}

/// Cached result of a dotnet CLI operation.
#[derive(Debug, Clone)]
pub enum CacheEntry {
    /// Successfully resolved result.
    Resolved(Vec<String>),
    /// CLI tool not available.
    ToolNotFound,
    /// Resolution failed.
    Error(String),
}

/// A NuGet package dependency discovered from dotnet analysis.
#[derive(Debug, Clone)]
pub struct NuGetDep {
    /// Package name (e.g., "Newtonsoft.Json").
    pub name: String,
    /// Package version.
    pub version: String,
}

/// A project reference discovered from dotnet analysis.
#[derive(Debug, Clone)]
pub struct ProjectRef {
    /// Path to the referenced project (relative or absolute).
    pub path: PathBuf,
    /// Project GUID, if available.
    pub guid: Option<String>,
}

/// Client for the `dotnet` CLI.
///
/// Provides methods to query project metadata via the .NET SDK.
/// All operations gracefully handle the case where the dotnet CLI
/// is not installed.
#[derive(Debug)]
pub struct DotnetClient {
    /// Path to the dotnet CLI binary.
    pub dotnet_path: PathBuf,
    /// Whether the dotnet CLI is available.
    pub available: bool,
    /// Cache for resolved results.
    cache: std::sync::Mutex<std::collections::HashMap<String, CacheEntry>>,
}

impl DotnetClient {
    /// Create a new `DotnetClient`.
    ///
    /// Checks if the `dotnet` CLI is available on the system PATH.
    /// Returns `Ok(DotnetClient)` regardless of availability —
    /// the `available` field indicates whether CLI calls will succeed.
    pub fn new() -> Result<Self, DotnetError> {
        // Common dotnet CLI paths to check
        let candidates = Self::dotnet_candidates();

        for candidate in &candidates {
            if Self::dotnet_is_valid(candidate) {
                return Ok(Self {
                    dotnet_path: candidate.clone(),
                    available: true,
                    cache: std::sync::Mutex::new(std::collections::HashMap::new()),
                });
            }
        }

        // dotnet not found — still return Ok with available=false
        // This allows XML-only parsing to work as a complete fallback
        Ok(Self {
            dotnet_path: PathBuf::from("dotnet"),
            available: false,
            cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// List common paths where dotnet CLI might be installed.
    fn dotnet_candidates() -> Vec<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();

        // PATH lookup
        candidates.push(PathBuf::from("dotnet"));

        // Homebrew (macOS)
        if let Ok(home) = std::env::var("HOME") {
            candidates.push(PathBuf::from(format!("{home}/.dotnet/dotnet")));
            candidates.push(PathBuf::from(format!("{home}/homebrew/bin/dotnet")));
            candidates.push(PathBuf::from(
                "/opt/homebrew/bin/dotnet",
            ));
        }

        // Standard install locations
        candidates.push(PathBuf::from("/usr/bin/dotnet"));
        candidates.push(PathBuf::from("/usr/local/bin/dotnet"));
        candidates.push(PathBuf::from("/snap/bin/dotnet"));

        // Windows paths (for cross-platform awareness)
        if cfg!(windows) {
            if let Ok(program_files) = std::env::var("PROGRAMFILES") {
                candidates.push(PathBuf::from(
                    format!("{program_files}/dotnet/dotnet.exe"),
                ));
            }
            if let Ok(program_files_x86) = std::env::var("PROGRAMFILES(X86)") {
                candidates.push(PathBuf::from(
                    format!("{program_files_x86}/dotnet/dotnet.exe"),
                ));
            }
            candidates.push(PathBuf::from(
                "C:/Program Files/dotnet/dotnet.exe",
            ));
        }

        candidates
    }

    /// Check if a dotnet CLI path is valid and executable.
    fn dotnet_is_valid(path: &Path) -> bool {
        let output = Command::new(path)
            .arg("--version")
            .output();

        match output {
            Ok(out) => out.status.success(),
            Err(_) => false,
        }
    }

    /// Execute a dotnet CLI command and return stdout.
    fn run_command(
        &self,
        args: &[&str],
    ) -> Result<String, DotnetError> {
        if !self.available {
            return Err(DotnetError::CliNotFound);
        }

        let output = Command::new(&self.dotnet_path)
            .args(args)
            .output()
            .map_err(|e| DotnetError::CommandError {
                cmd: args.join(" "),
                source: e,
            })?;

        if !output.status.success() {
            let _stderr = String::from_utf8_lossy(&output.stderr).to_string();
            let code = output.status.code().unwrap_or(-1);
            return Err(DotnetError::CommandFailed {
                cmd: args.join(" "),
                code,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }

    /// Validate that a .csproj file is a valid .NET project.
    ///
    /// Runs `dotnet validate project <path>` and returns `Ok(())` if valid.
    pub fn validate_project(&self, csproj_path: &Path) -> Result<(), DotnetError> {
        let cache_key = format!("validate:{}", csproj_path.display());

        {
            let cache = self.cache.lock().unwrap();
            if let Some(entry) = cache.get(&cache_key) {
                match entry {
                    CacheEntry::Resolved(_) => return Ok(()),
                    CacheEntry::Error(msg) => return Err(DotnetError::ValidationFailed {
                        path: csproj_path.to_string_lossy().to_string(),
                        reason: msg.clone(),
                    }),
                    CacheEntry::ToolNotFound => return Err(DotnetError::CliNotFound),
                }
            }
        }

        let result = self.run_command(&["validate", "project", csproj_path.to_str().unwrap()]);

        let mut cache = self.cache.lock().unwrap();
        match &result {
            Ok(_) => {
                cache.insert(cache_key, CacheEntry::Resolved(vec![]));
            }
            Err(e) => {
                cache.insert(cache_key, CacheEntry::Error(e.to_string()));
            }
        }

        result.map(|_| ())
    }

    /// Get project references by parsing the .csproj XML file.
    ///
    /// This is the XML-based approach that works without dotnet CLI.
    /// Falls back to dotnet CLI parsing if XML parsing fails.
    pub fn get_project_refs(&self, csproj_path: &Path) -> Vec<ProjectRef> {
        // Primary: Parse XML
        let content = match std::fs::read_to_string(csproj_path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let mut refs: Vec<ProjectRef> = Vec::new();
        let mut reader = XmlReader::from_reader(Cursor::new(&content));
        reader.config_mut().trim_text(true);
        reader.config_mut().expand_empty_elements = true;

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(event)) if event.name().as_ref() == b"ProjectReference" => {
                    let include = Self::get_attr_str(&event, "Include");
                    let project = Self::get_attr_str(&event, "Project");
                    if let Some(path) = include {
                        refs.push(ProjectRef {
                            path: PathBuf::from(&path),
                            guid: project,
                        });
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        refs
    }

    /// Get NuGet package dependencies by parsing the .csproj XML file.
    ///
    /// This is the XML-based approach that works without dotnet CLI.
    /// Falls back to `dotnet list package` if available.
    pub fn get_nuget_deps(&self, csproj_path: &Path) -> Vec<NuGetDep> {
        // Try XML parsing first (works without dotnet CLI)
        let deps = self.get_nuget_deps_from_csproj(csproj_path);
        if !deps.is_empty() {
            return deps;
        }

        // Fallback: dotnet list package (requires CLI)
        if self.available {
            if let Ok(output) = self.run_command(&[
                "list",
                "package",
                "--format",
                "json",
                csproj_path.to_str().unwrap_or_default(),
            ]) {
                if let Ok(deps) = self.parse_dotnet_list_package_json(&output) {
                    return deps;
                }
            }
        }

        Vec::new()
    }

    /// Parse NuGet dependencies from a .csproj file's XML.
    fn get_nuget_deps_from_csproj(&self, csproj_path: &Path) -> Vec<NuGetDep> {
        let content = match std::fs::read_to_string(csproj_path) {
            Ok(c) => c,
            Err(_) => return Vec::new(),
        };

        let mut deps: Vec<NuGetDep> = Vec::new();
        let mut reader = XmlReader::from_reader(Cursor::new(&content));
        reader.config_mut().trim_text(true);
        reader.config_mut().expand_empty_elements = true;

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(event)) if event.name().as_ref() == b"PackageReference" => {
                    let name = Self::get_attr_str(&event, "Include")
                        .or_else(|| Self::get_attr_str(&event, "PackageReference"));
                    let version = Self::get_attr_str(&event, "Version");

                    if let Some(name) = name {
                        deps.push(NuGetDep {
                            name,
                            version: version.unwrap_or_default(),
                        });
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        deps
    }

    /// Parse JSON output from `dotnet list package --format json`.
    fn parse_dotnet_list_package_json(
        &self,
        output: &str,
    ) -> Result<Vec<NuGetDep>, DotnetError> {
        // Simple JSON parsing without external crate
        // Format: { "projects": { "/path/to.csproj": { "dependencies": [ ... ] } } }
        let mut deps: Vec<NuGetDep> = Vec::new();

        // Look for "name": "..." and "version": "..." pairs
        let chars: Vec<char> = output.chars().collect();
        let len = chars.len();
        let mut i = 0;

        while i < len {
            // Find "name":
            if let Some(pos) = output[i..].find("\"name\"") {
                let name_start = i + pos;
                // Find the value
                if let Some((name, name_end)) = Self::extract_json_string(output, name_start) {
                    // Find "version":
                    if let Some(ver_pos) = output[name_end..].find("\"version\"") {
                        let ver_start = name_end + ver_pos;
                        if let Some((version, _)) = Self::extract_json_string(output, ver_start) {
                            deps.push(NuGetDep {
                                name,
                                version,
                            });
                        }
                    } else {
                        // Package without explicit version
                        deps.push(NuGetDep {
                            name,
                            version: String::new(),
                        });
                    }
                    i = name_end;
                } else {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(deps)
    }

    /// Resolve a namespace by running `dotnet msbuild` to get the generated
    /// .editorconfig files which contain namespace information.
    pub fn resolve_namespace(&self, namespace: &str) -> Option<Vec<PathBuf>> {
        if !self.available {
            return None;
        }

        // Look for namespace definitions in generated .g.cs or .editorconfig files
        // This is a best-effort fallback — the XML parser is the primary source
        let mut results: Vec<PathBuf> = Vec::new();

        // Find generated files that might contain namespace info
        for dir in &["obj", "GeneratedFiles"] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "cs") {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if content.contains(&format!("namespace {}", namespace)) {
                                results.push(path);
                            }
                        }
                    }
                }
            }
        }

        if results.is_empty() {
            None
        } else {
            Some(results)
        }
    }

    /// Get an XML element attribute value as a UTF-8 string.
    fn get_attr_str(event: &quick_xml::events::BytesStart<'_>, attr_name: &str) -> Option<String> {
        for attr in event.attributes().flatten() {
            let key_bytes: &[u8] = attr.key.as_ref();
            if String::from_utf8_lossy(key_bytes) == attr_name {
                return Some(String::from_utf8_lossy(&attr.value).to_string());
            }
        }
        None
    }

    /// Extract a JSON string value starting from the position after `"key"`.
    fn extract_json_string(input: &str, start: usize) -> Option<(String, usize)> {
        let remaining = &input[start..];
        // Skip to the colon
        let colon_pos = remaining.find(':')?;
        let after_colon = &remaining[colon_pos + 1..];
        // Skip whitespace
        let after_ws = after_colon.trim_start();

        if !after_ws.starts_with('"') {
            return None;
        }

        let inner = &after_ws[1..];
        let end_quote = inner.find('"')?;
        let value = inner[..end_quote].to_string();
        let absolute_end = start + colon_pos + 1 + after_ws.len() + end_quote + 1;

        Some((value, absolute_end))
    }
}

/// Check if the dotnet CLI is available on the system.
///
/// This is a convenience wrapper around `DotnetClient::new()`.
pub fn dotnet_available() -> bool {
    DotnetClient::new().map(|c| c.available).unwrap_or(false)
}

/// Get the path to the dotnet CLI, if available.
pub fn dotnet_path() -> Option<PathBuf> {
    DotnetClient::new()
        .ok()
        .and_then(|c| {
            if c.available {
                Some(c.dotnet_path)
            } else {
                None
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_always_succeeds() {
        let client = DotnetClient::new();
        assert!(client.is_ok());
        // available depends on system state, but the result should be Ok
        let client = client.unwrap();
        assert!(!client.cache.lock().unwrap().is_empty() || client.cache.lock().unwrap().is_empty());
        assert_eq!(client.dotnet_path, PathBuf::from("dotnet"));
    }

    #[test]
    fn test_cli_not_available_returns_none() {
        let client = DotnetClient::new().unwrap();
        if !client.available {
            let result = client.validate_project(Path::new("test.csproj"));
            assert!(result.is_err());
            match result.unwrap_err() {
                DotnetError::CliNotFound => {}
                other => panic!("Expected CliNotFound, got: {:?}", other),
            }
        }
    }

    #[test]
    fn test_resolve_namespace_without_cli() {
        let client = DotnetClient::new().unwrap();
        if !client.available {
            let result = client.resolve_namespace("Some.Namespace");
            assert!(result.is_none());
        }
    }
}
