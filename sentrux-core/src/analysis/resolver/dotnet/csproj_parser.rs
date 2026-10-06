//! CsprojParser — Parse `.csproj` XML files into structured metadata.
//!
//! Extracts assembly names, namespaces, compile entries, project references,
//! package references, implicit usings, and target frameworks from MSBuild
//! project files using `quick-xml`.
//!
//! ## Key Structures
//!
//! - `Csproj` — Full parsed project with all metadata
//! - `CompileEntry` — Individual source file compilation directive
//! - `ProjectReference` — Cross-project dependency
//! - `PackageReference` — NuGet package dependency

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader as XmlReader;
use std::io::Cursor;

/// A single source file compilation directive from a `.csproj` file.
///
/// `<Compile Include="src/Service.cs" />`
#[derive(Debug, Clone)]
pub struct CompileEntry {
    /// Path to the source file (relative to the project directory).
    pub include: PathBuf,
    /// Whether this file is excluded from compilation.
    pub remove: bool,
    /// Explicit namespace override, if set via `<Namespace>X.Y.Z</Namespace>`.
    pub explicit_namespace: Option<String>,
}

/// A global using directive extracted from GlobalUsings.cs
#[derive(Debug, Clone)]
pub struct GlobalUsing {
    /// The full namespace being imported (e.g., 'Cart.Services')
    pub name: String,
    /// Optional alias: 'global using CartAcl = Cart.Grpc.Api.Acl;' -> Some('CartAcl')
    pub alias: Option<String>,
}

/// A project-to-project reference from a `.csproj` file.
///
/// `<ProjectReference Include="../Other/Other.csproj" />`
#[derive(Debug, Clone)]
pub struct ProjectReference {
    /// Path to the referenced `.csproj` file (relative to the referencing project).
    pub include: PathBuf,
    /// Optional project GUID from the `<Project>` attribute.
    pub project_guid: Option<String>,
}

/// A NuGet package reference from a `.csproj` file.
///
/// `<PackageReference Include="Newtonsoft.Json" Version="13.0.3" />`
#[derive(Debug, Clone)]
pub struct PackageReference {
    /// Package name (e.g., "Newtonsoft.Json").
    pub name: String,
    /// Package version string (may be a range like "13.0.3" or "2.*").
    pub version: Option<String>,
    /// Whether this package's assets are private (not passed to consumers).
    pub private_assets: Option<String>,
}

/// Complete parsed representation of a `.csproj` file.
///
/// Extracts all relevant metadata from an MSBuild project file,
/// including property groups, compile entries, project references,
/// and package references.
#[derive(Debug, Clone)]
pub struct Csproj {
    /// Absolute or resolved path to the `.csproj` file.
    pub path: PathBuf,
    /// Assembly name from `<PropertyGroup><AssemblyName>`.
    pub assembly_name: Option<String>,
    /// Root namespace from `<PropertyGroup><RootNamespace>`.
    pub root_namespace: Option<String>,
    /// Default namespace from `<PropertyGroup><DefaultNamespace>`.
    pub default_namespace: Option<String>,
    /// Target framework (e.g., "net8.0", "net10.0").
    pub target_framework: Option<String>,
    /// Whether implicit usings are enabled.
    pub implicit_usings: Option<bool>,
    /// Nullable context setting ("enable", "disable", "warnings", etc.).
    pub nullable: Option<String>,
    /// Source files included in compilation.
    pub compile_files: Vec<CompileEntry>,
    /// Cross-project references.
    pub project_references: Vec<ProjectReference>,
    /// NuGet package references.
    pub package_references: Vec<PackageReference>,
    /// Global using directives found in the project (e.g., from GlobalUsings.cs).
    pub global_usings: Vec<GlobalUsing>,
}

impl Csproj {
    /// Parse a `.csproj` file from a given path.
    ///
    /// Reads and parses the XML content, extracting all project metadata.
    /// Returns `Ok(Csproj)` on success or an XML parsing error.
    pub fn parse(path: &Path) -> Result<Csproj, quick_xml::errors::Error> {
        let content = fs::read_to_string(path)
            .map_err(|e| quick_xml::errors::Error::Io(std::sync::Arc::new(e)))?;

        Self::parse_from_str(&content, path)
    }

    /// Parse a `.csproj` file from a string (for testing).
    pub fn parse_from_str(content: &str, path: &Path) -> Result<Csproj, quick_xml::errors::Error> {
        let mut reader = XmlReader::from_reader(Cursor::new(content));
        reader.config_mut().trim_text(true);
        reader.config_mut().check_end_names = false;
        reader.config_mut().expand_empty_elements = true;

        let mut csproj = Csproj {
            path: path.to_path_buf(),
            assembly_name: None,
            root_namespace: None,
            default_namespace: None,
            target_framework: None,
            implicit_usings: None,
            nullable: None,
            compile_files: Vec::new(),
            project_references: Vec::new(),
            package_references: Vec::new(),
            global_usings: Vec::new(),
        };

        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(event)) | Ok(Event::Empty(event)) => {
                    let tag = String::from_utf8_lossy(event.name().as_ref()).to_string();

                    match tag.as_str() {
                        "PropertyGroup" => {
                            csproj._parse_property_group(&mut reader, &event)?;
                        }
                        "Compile" if event.name().as_ref() == b"Compile" => {
                            csproj._parse_compile_entry(&event)?;
                        }
                        "ProjectReference" => {
                            csproj._parse_project_reference(&event);
                        }
                        "PackageReference" => {
                            csproj._parse_package_reference(&event)?;
                        }
                        _ => {}
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }

        // Default default_namespace from assembly_name if not set
        if csproj.default_namespace.is_none() {
            csproj.default_namespace = csproj.assembly_name.clone();
        }

        // For SDK-style projects (no explicit <Compile> entries), auto-discover
        // .cs source files in the project directory. This handles modern .NET SDKs
        // that auto-discover sources without explicit <Compile Include="..." /> entries.
        if csproj.compile_files.is_empty() {
            csproj.auto_discover_cs_files();
        }

        Ok(csproj)
    }

    /// Parse the `<PropertyGroup>` element and its children.
    fn _parse_property_group(
        &mut self,
        reader: &mut XmlReader<Cursor<&str>>,
        _event: &BytesStart<'_>,
    ) -> Result<(), quick_xml::errors::Error> {
        // Read child elements of PropertyGroup
        let mut depth = 1;
        loop {
            let mut buf = Vec::new();
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(child)) => {
                    let child_tag = String::from_utf8_lossy(child.name().as_ref()).to_string();
                    match child_tag.as_str() {
                        "AssemblyName" => {
                            if let Some(val) = Self::read_element_text(reader, "AssemblyName")? {
                                self.assembly_name = Some(val);
                            }
                        }
                        "RootNamespace" => {
                            if let Some(val) = Self::read_element_text(reader, "RootNamespace")? {
                                self.root_namespace = Some(val);
                            }
                        }
                        "DefaultNamespace" => {
                            if let Some(val) = Self::read_element_text(reader, "DefaultNamespace")? {
                                self.default_namespace = Some(val);
                            }
                        }
                        "TargetFramework" => {
                            if let Some(val) = Self::read_element_text(reader, "TargetFramework")? {
                                self.target_framework = Some(val);
                            }
                        }
                        "ImplicitUsings" => {
                            if let Some(val) = Self::read_element_text(reader, "ImplicitUsings")? {
                                self.implicit_usings = Some(val == "enable" || val == "true" || val == "1");
                            }
                        }
                        "Nullable" => {
                            if let Some(val) = Self::read_element_text(reader, "Nullable")? {
                                self.nullable = Some(val);
                            }
                        }
                        "Compile" => {
                            self._parse_compile_entry(&child)?;
                        }
                        "ProjectReference" => {
                            self._parse_project_reference(&child);
                        }
                        "PackageReference" => {
                            self._parse_package_reference(&child)?;
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(end)) if end.name().as_ref() == b"PropertyGroup" => {
                    depth -= 1;
                    if depth <= 0 {
                        break;
                    }
                }
                Ok(Event::End(_)) => {}
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }

        Ok(())
    }

    /// Parse a `<Compile>` entry and extract its Include attribute.
    fn _parse_compile_entry(
        &mut self,
        event: &BytesStart<'_>,
    ) -> Result<(), quick_xml::errors::Error> {
        let include = Self::get_attribute_str(event, "Include");
        let remove = Self::get_attribute_str(event, "Remove");

        let entry = if let Some(path) = include {
            CompileEntry {
                include: PathBuf::from(&path),
                remove: false,
                explicit_namespace: None,
            }
        } else if let Some(path) = remove {
            CompileEntry {
                include: PathBuf::from(&path),
                remove: true,
                explicit_namespace: None,
            }
        } else {
            return Ok(());
        };

        self.compile_files.push(entry);
        Ok(())
    }

    /// Parse a `<ProjectReference>` entry.
    fn _parse_project_reference(&mut self, event: &BytesStart<'_>) {
        let include = Self::get_attribute_str(event, "Include");
        let project_attr = Self::get_attribute_str(event, "Project");

        if let Some(path) = include {
            self.project_references.push(ProjectReference {
                include: PathBuf::from(&path),
                project_guid: project_attr,
            });
        }
    }

    /// Parse a `<PackageReference>` entry.
    fn _parse_package_reference(
        &mut self,
        event: &BytesStart<'_>,
    ) -> Result<(), quick_xml::errors::Error> {
        let name = Self::get_attribute_str(event, "Include")
            .or_else(|| Self::get_attribute_str(event, "PackageReference"))
            .unwrap_or_default();

        let version = Self::get_attribute_str(event, "Version");
        let private_assets = Self::get_attribute_str(event, "PrivateAssets");

        self.package_references.push(PackageReference {
            name,
            version,
            private_assets,
        });

        Ok(())
    }

    /// Read the text content of a named element.
    fn read_element_text(
        reader: &mut XmlReader<Cursor<&str>>,
        tag: &str,
    ) -> Result<Option<String>, quick_xml::errors::Error> {
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Text(text)) => {
                    let text = text.unescape()?.to_string();
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        // Consume the closing End event for this element
                        let mut end_buf = Vec::new();
                        if let Ok(Event::End(end)) = reader.read_event_into(&mut end_buf) {
                            let _ = String::from_utf8_lossy(end.name().as_ref()) == tag;
                        }
                        return Ok(Some(text));
                    }
                }
                Ok(Event::End(end)) => {
                    if String::from_utf8_lossy(end.name().as_ref()) == tag {
                        return Ok(None);
                    }
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
            buf.clear();
        }
        Ok(None)
    }

    /// Get an XML element attribute value as a UTF-8 string.
    fn get_attribute_str(event: &BytesStart<'_>, attr_name: &str) -> Option<String> {
        for attr in event.attributes().flatten() {
            let key_bytes: &[u8] = attr.key.as_ref();
            if String::from_utf8_lossy(key_bytes) == attr_name {
                return Some(String::from_utf8_lossy(&attr.value).to_string());
            }
        }
        None
    }

    /// Get the effective default namespace for file-to-namespace mapping.
    pub fn get_effective_default_namespace(&self) -> String {
        self.default_namespace
            .clone()
            .or_else(|| self.root_namespace.clone())
            .or_else(|| self.assembly_name.clone())
            .unwrap_or_default()
    }

    /// Auto-discover .cs source files in the project directory for SDK-style projects.
    /// Populates compile_files with discovered .cs files when no explicit <Compile> entries exist.
    fn auto_discover_cs_files(&mut self) {
        let project_dir = self.path.parent().unwrap_or(Path::new(""));
        let default_ns = self.get_effective_default_namespace();

        // Recursively find all .cs files, skipping build directories
        let mut cs_files = Vec::new();
        Self::find_cs_files_recursive(project_dir, &mut cs_files);

        for cs_path in cs_files {
            // Convert absolute path to relative path from project directory
            let relative = cs_path
                .strip_prefix(project_dir)
                .unwrap_or(&cs_path)
                .to_path_buf();

            self.compile_files.push(CompileEntry {
                include: relative,
                remove: false,
                explicit_namespace: None,
            });
        }
    }

    /// Recursively find .cs files, skipping common build/dependency directories.
    fn find_cs_files_recursive(dir: &Path, results: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_dir() {
                    // Skip build and dependency directories
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let skip = matches!(
                        name,
                        "bin" | "obj" | ".git" | "node_modules" | ".nuget" | "packages"
                    );
                    if !skip {
                        Self::find_cs_files_recursive(&path, results);
                    }
                } else if path.extension().is_some_and(|ext| ext == "cs") {
                    results.push(path);
                }
            }
        }
    }

    /// Extract namespace declarations from all .cs source files in the project.
    /// Reads the first meaningful line of each file looking for:
    ///   namespace X.Y.Z;    or   namespace X.Y.Z
    /// Returns a map of file_path -> namespace_string.
    pub fn extract_namespace_declarations(&self) -> HashMap<PathBuf, String> {
        let mut decls = HashMap::new();
        let project_dir = self.path.parent().unwrap_or(Path::new(""));

        for entry in &self.compile_files {
            if entry.remove {
                continue;
            }
            let full_path = project_dir.join(&entry.include);
            if let Some(ns) = Self::extract_namespace_from_file(&full_path) {
                decls.insert(full_path, ns);
            }
        }
        decls
    }

    /// Extract the namespace declaration from a single .cs file.
    fn extract_namespace_from_file(path: &Path) -> Option<String> {
        let content = std::fs::read_to_string(path).ok()?;
        for line in content.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("namespace ") {
                let ns = rest
                    .trim_end_matches(';')
                    .trim_end_matches('{')
                    .trim()
                    .to_string();
                if !ns.is_empty() && ns.contains('.') {
                    return Some(ns);
                }
            }
        }
        None
    }
}

/// Discover all `.csproj` files under a root directory.
///
/// Skips common build and dependency directories (`bin`, `obj`, `.git`, etc.).
/// Returns a vector of absolute paths to discovered `.csproj` files.
pub fn find_all(root: &Path) -> Vec<PathBuf> {
    let mut results: Vec<PathBuf> = Vec::new();
    _find_csproj_recursive(root, &mut results);
    results.sort();
    results
}

/// Recursive helper for discovering `.csproj` files.
fn _find_csproj_recursive(dir: &Path, results: &mut Vec<PathBuf>) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_dir() {
                let should_skip = path
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

                if !should_skip {
                    _find_csproj_recursive(&path, results);
                }
            } else if path.extension().is_some_and(|ext| ext == "csproj") {
                results.push(path);
            }
        }
    }
}

/// Compute the expected C# namespace for a source file based on the project's
/// default namespace and the file's relative path within the project.
///
/// Algorithm:
/// 1. Strip leading common directories (e.g., "src/", "Src/")
/// 2. Join remaining path components with `.`
/// 3. Prepend the default namespace
/// 4. Append the file stem (filename without extension)
///
/// # Examples
///
/// ```
/// // default_namespace = "MyApp"
/// // file_path = "src/Services/Validators/AuthValidator.cs"
/// // Result: "MyApp.Services.Validators.AuthValidator"
/// ```
/// Re-export from namespace_map for convenience.
pub use crate::analysis::resolver::dotnet::namespace_map::compute_namespace_for_file_relative;

pub fn compute_namespace_for_file(file_path: &Path, default_namespace: &str) -> String {
    let file_path = file_path.to_string_lossy();

    // Remove leading "src/" or "Src/" or "source/" or "Source/" if present
    let stripped = file_path
        .strip_prefix("src/")
        .or_else(|| file_path.strip_prefix("Src/"))
        .or_else(|| file_path.strip_prefix("source/"))
        .or_else(|| file_path.strip_prefix("Source/"))
        .unwrap_or(&file_path);

    // Get the parent directory parts and file stem
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

/// Parse multiple `.csproj` files from a list of paths.
///
/// Returns a vector of successfully parsed `Csproj` objects, silently
/// skipping files that fail to parse (with a warning to stderr).
pub fn parse_multiple(paths: &[PathBuf]) -> Vec<Csproj> {
    let mut results: Vec<Csproj> = Vec::new();

    for path in paths {
        match Csproj::parse(path) {
            Ok(csproj) => results.push(csproj),
            Err(e) => {
                eprintln!("[sentrux] Warning: Failed to parse {}: {}", path.display(), e);
            }
        }
    }

    results
}

/// Get unique compile file paths across all `.csproj` files,
/// deduplicating by resolved absolute path.
pub fn unique_compile_files(csprojects: &[Csproj]) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut files: Vec<PathBuf> = Vec::new();

    for csproj in csprojects {
        let project_dir = csproj.path.parent().unwrap_or(Path::new(""));
        for entry in &csproj.compile_files {
            if entry.remove {
                continue;
            }
            let full_path = project_dir.join(&entry.include);
            let canonical = full_path.canonicalize();
            let key = canonical.unwrap_or_else(|_| full_path.clone());
            if seen.insert(key) {
                files.push(full_path);
            }
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_csproj(content: &str) -> Csproj {
        Csproj::parse_from_str(content, Path::new("test.csproj")).expect("parse failed")
    }

    #[test]
    fn test_parse_basic_csproj() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
            <PropertyGroup>
                <TargetFramework>net8.0</TargetFramework>
                <ImplicitUsings>enable</ImplicitUsings>
                <Nullable>enable</Nullable>
                <AssemblyName>MyLibrary</AssemblyName>
                <RootNamespace>MyLibrary</RootNamespace>
            </PropertyGroup>
        </Project>"#;

        let csproj = make_csproj(content);
        assert_eq!(csproj.assembly_name, Some("MyLibrary".to_string()));
        assert_eq!(csproj.root_namespace, Some("MyLibrary".to_string()));
        assert_eq!(csproj.target_framework, Some("net8.0".to_string()));
        assert_eq!(csproj.implicit_usings, Some(true));
        assert_eq!(csproj.nullable, Some("enable".to_string()));
        assert!(csproj.compile_files.is_empty());
        assert!(csproj.project_references.is_empty());
        assert!(csproj.package_references.is_empty());
    }

    #[test]
    fn test_parse_with_project_reference() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
            <PropertyGroup>
                <TargetFramework>net8.0</TargetFramework>
            </PropertyGroup>
            <ItemGroup>
                <ProjectReference Include="../Other/Other.csproj" />
            </ItemGroup>
        </Project>"#;

        let csproj = make_csproj(content);
        assert_eq!(csproj.project_references.len(), 1);
        assert_eq!(
            csproj.project_references[0].include,
            PathBuf::from("../Other/Other.csproj")
        );
    }

    #[test]
    fn test_parse_with_package_reference() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
            <PropertyGroup>
                <TargetFramework>net8.0</TargetFramework>
            </PropertyGroup>
            <ItemGroup>
                <PackageReference Include="Newtonsoft.Json" Version="13.0.3" />
                <PackageReference Include="Grpc.Tools" Version="2.83.0" PrivateAssets="all" />
            </ItemGroup>
        </Project>"#;

        let csproj = make_csproj(content);
        assert_eq!(csproj.package_references.len(), 2);
        assert_eq!(csproj.package_references[0].name, "Newtonsoft.Json");
        assert_eq!(csproj.package_references[0].version, Some("13.0.3".to_string()));
        assert_eq!(csproj.package_references[1].name, "Grpc.Tools");
        assert_eq!(csproj.package_references[1].private_assets, Some("all".to_string()));
    }

    #[test]
    fn test_parse_with_compile_entries() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
            <PropertyGroup>
                <TargetFramework>net8.0</TargetFramework>
            </PropertyGroup>
            <ItemGroup>
                <Compile Include="src/Service.cs" />
                <Compile Remove="Models/**" />
            </ItemGroup>
        </Project>"#;

        let csproj = make_csproj(content);
        assert_eq!(csproj.compile_files.len(), 2);
        assert!(!csproj.compile_files[0].remove);
        assert!(csproj.compile_files[1].remove);
        assert_eq!(
            csproj.compile_files[0].include,
            PathBuf::from("src/Service.cs")
        );
    }

    #[test]
    fn test_compute_namespace_basic() {
        let ns = compute_namespace_for_file(
            Path::new("src/Services/Validators/AuthValidator.cs"),
            "MyApp",
        );
        assert_eq!(ns, "MyApp.Services.Validators.AuthValidator");
    }

    #[test]
    fn test_compute_namespace_no_src_prefix() {
        let ns = compute_namespace_for_file(
            Path::new("Services/Validators/AuthValidator.cs"),
            "MyApp",
        );
        assert_eq!(ns, "MyApp.Services.Validators.AuthValidator");
    }

    #[test]
    fn test_compute_namespace_file_in_root() {
        let ns = compute_namespace_for_file(
            Path::new("Program.cs"),
            "MyApp",
        );
        assert_eq!(ns, "MyApp.Program");
    }

    #[test]
    fn test_compute_namespace_deeply_nested() {
        let ns = compute_namespace_for_file(
            Path::new("src/Domain/Aggregates/Order/Commands/CreateOrder.cs"),
            "MyApp",
        );
        assert_eq!(ns, "MyApp.Domain.Aggregates.Order.Commands.CreateOrder");
    }

    #[test]
    fn test_default_namespace_fallback() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
            <PropertyGroup>
                <TargetFramework>net8.0</TargetFramework>
                <AssemblyName>MyAssembly</AssemblyName>
            </PropertyGroup>
        </Project>"#;

        let csproj = make_csproj(content);
        // default_namespace should fall back to assembly_name
        assert_eq!(csproj.default_namespace, Some("MyAssembly".to_string()));
    }

    #[test]
    fn test_efffective_default_namespace() {
        let content = r#"<Project Sdk="Microsoft.NET.Sdk">
            <PropertyGroup>
                <RootNamespace>MyRoot</RootNamespace>
                <DefaultNamespace>MyDefault</DefaultNamespace>
            </PropertyGroup>
        </Project>"#;

        let csproj = make_csproj(content);
        assert_eq!(csproj.get_effective_default_namespace(), "MyDefault");
    }

    #[test]
    fn test_compute_namespace_empty_default() {
        // When default namespace is empty, only the file stem is used
        // (src/ prefix is stripped as it's a common convention)
        let ns = compute_namespace_for_file(
            Path::new("src/Service.cs"),
            "",
        );
        assert_eq!(ns, "Service");
    }

    #[test]
    fn test_extract_namespace_from_file() {
        let temp_dir = std::env::temp_dir().join("sentrux_dotnet_ns_test");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cs_file = temp_dir.join("Service.cs");
        std::fs::write(
            &cs_file,
            b"namespace Common.Cart.Graph.Middleware;\n\npublic class MyService {}\n",
        )
        .unwrap();
        let ns = Csproj::extract_namespace_from_file(&cs_file);
        assert_eq!(ns, Some("Common.Cart.Graph.Middleware".to_string()));
        std::fs::remove_dir_all(&temp_dir).unwrap();
    }

    #[test]
    fn test_extract_namespace_from_file_no_decl() {
        let temp_dir = std::env::temp_dir().join("sentrux_dotnet_no_ns");
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cs_file = temp_dir.join("NoNamespace.cs");
        std::fs::write(
            &cs_file,
            b"// No namespace declaration\npublic class Foo {}\n",
        )
        .unwrap();
        let ns = Csproj::extract_namespace_from_file(&cs_file);
        assert_eq!(ns, None);
        std::fs::remove_dir_all(&temp_dir).unwrap();
    }
}
