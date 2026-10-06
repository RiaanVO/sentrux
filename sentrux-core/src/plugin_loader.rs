//! Runtime metrics plugin loader for sentrux.
//!
//! Discovers and loads `.dylib`/`.so`/`.dll` plugins from `~/.sentrux/plugins/`.
//! Each plugin exports three C-compatible FFI functions:
//! - `plugin_info()` → JSON metadata
//! - `plugin_compute(snapshot_json)` → JSON metrics result
//! - `plugin_free(ptr)` → free allocated string
//!
//! ## Architecture
//!
//! ```
//! ┌─────────────┐     ┌────────────────┐     ┌──────────────────┐
//! │ sentrux-bin │────▶│ plugin_loader  │────▶│ ~/.sentrux/      │
//! │              │     │ (this module)  │     │ plugins/*.dylib  │
//! │ main_impl    │     │                │     │                  │
//! │ ─ init()     │     │ load_all()     │     │ comment.dylib    │
//! │ ─ scan()     │     │ compute_all()  │     │ security.dylib   │
//! └─────────────┘     └────────────────┘     └──────────────────┘
//! ```
//!
//! ## Capability Negotiation
//!
//! Plugins declare what data they need via `capabilities` in `plugin_info()`.
//! The core checks capabilities at load time and enables features accordingly.
//!
//! Example:
//! ```json
//! {"name":"comment","capabilities":["comment_data"]}
//! ```
//! This tells the core to ensure `FuncInfo.comment_lines` is populated.

pub mod loader {
    use std::collections::HashMap;
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::path::{Path, PathBuf};
    use std::panic;
    use libloading::Library;

    // ─── FFI Function Pointer Types ───

    type PluginInfoFn = unsafe extern "C" fn() -> *const c_char;
    type PluginComputeFn = unsafe extern "C" fn(*const c_char) -> *const c_char;
    type PluginFreeFn = unsafe extern "C" fn(*const c_char);

    // ─── Plugin Metadata ───

    /// Metadata extracted from a plugin's `plugin_info()` call.
    #[derive(Debug, Clone, serde::Deserialize)]
    pub struct PluginMetadata {
        /// Unique identifier. Used as the key in `HealthReport.extension_data`.
        pub name: String,
        /// Human-readable name for display in UI.
        pub display_name: String,
        /// Semver version string.
        pub version: String,
        /// Capabilities the plugin declares it needs (e.g., "comment_data").
        pub capabilities: Vec<String>,
        /// Minimum sentrux version this plugin requires (optional).
        pub min_sentrux_version: Option<String>,
    }

    /// Error loading a single plugin (non-fatal, logged and skipped).
    #[derive(Debug)]
    pub struct LoadError {
        pub plugin_dir: PathBuf,
        pub error: String,
    }

    impl std::fmt::Display for LoadError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}: {}", self.plugin_dir.display(), self.error)
        }
    }

    // ─── Loaded Plugin ───

    /// A fully loaded metrics plugin with FFI handles.
    /// The Library stays loaded via Box leak for the process lifetime.
    #[derive(Debug)]
    pub struct LoadedPlugin {
        /// Raw function pointers — remain valid as long as the library is loaded.
        info_fn: PluginInfoFn,
        compute_fn: PluginComputeFn,
        free_fn: PluginFreeFn,
        /// Library stays loaded via Box leak. Stored as &'static Library.
        #[allow(dead_code)]
        _lib: &'static Library,
        /// Plugin metadata from plugin_info().
        pub metadata: PluginMetadata,
    }

    // ─── Plugin Registry ───

    /// Global registry of loaded metrics plugins.
    /// Thread-safe: accessed via Mutex during compute calls.
    pub struct PluginRegistry {
        plugins: Vec<LoadedPlugin>,
    }

    impl PluginRegistry {
        /// Create a new empty registry.
        pub fn new() -> Self {
            Self { plugins: Vec::new() }
        }

        /// Call all plugins' compute() and return combined results.
        pub fn compute_all(&self, snapshot_json: &str) -> HashMap<String, serde_json::Value> {
            let mut results = HashMap::new();
            for plugin in &self.plugins {
                match self.call_compute(plugin, snapshot_json) {
                    Ok(Some(value)) => {
                        results.insert(plugin.metadata.name.clone(), value);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        crate::debug_log!(
                            "[plugin] Compute error for '{}': {}",
                            plugin.metadata.name,
                            e
                        );
                    }
                }
            }
            results
        }

        /// Check if any loaded plugin needs a specific capability.
        pub fn needs_capability(&self, capability: &str) -> bool {
            self.plugins
                .iter()
                .any(|p| p.metadata.capabilities.contains(&capability.to_string()))
        }

        /// Return the number of loaded plugins.
        pub fn len(&self) -> usize {
            self.plugins.len()
        }

        /// Call a single plugin's compute function with panic protection.
        fn call_compute(
            &self,
            plugin: &LoadedPlugin,
            snapshot_json: &str,
        ) -> Result<Option<serde_json::Value>, String> {
            let snapshot_cstr = CString::new(snapshot_json)
                .map_err(|e| format!("CString creation: {}", e))?;
            let snapshot_ptr = snapshot_cstr.as_ptr();

            // Call compute with panic protection
            let result = panic::catch_unwind(panic::AssertUnwindSafe(|| unsafe {
                (plugin.compute_fn)(snapshot_ptr)
            }));

            match result {
                Ok(result_ptr) => {
                    if result_ptr.is_null() {
                        return Ok(None);
                    }
                    let json_str = unsafe {
                        CStr::from_ptr(result_ptr)
                            .to_str()
                            .map_err(|e| format!("UTF-8 decode: {}", e))?
                    };
                    unsafe { (plugin.free_fn)(result_ptr) };
                    let value: serde_json::Value = serde_json::from_str(json_str)
                        .map_err(|e| format!("JSON parse error: {}", e))?;
                    Ok(Some(value))
                }
                Err(panic_err) => {
                    let msg = if let Some(s) = panic_err.downcast_ref::<String>() {
                        s.as_str()
                    } else if let Some(s) = panic_err.downcast_ref::<&str>() {
                        s
                    } else {
                        "unknown panic"
                    };
                    Err(format!("Plugin panicked: {}", msg))
                }
            }
        }
    }

    // Global registry — lives for the entire process lifetime via LazyLock
    static REGISTRY: std::sync::LazyLock<std::sync::Mutex<PluginRegistry>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(PluginRegistry::new()));

    // ─── Public API ───

    /// Get the plugins directory path.
    ///
    /// Check order:
    /// 1. `SENTRUX_PLUGINS_DIR` environment variable
    /// 2. `~/.sentrux/plugins/`
    pub fn plugins_dir() -> Option<PathBuf> {
        std::env::var("SENTRUX_PLUGINS_DIR")
            .ok()
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".sentrux").join("plugins")))
    }

    /// Discover and load all metrics plugins from the plugins directory.
    ///
    /// Returns:
    /// - `Ok(Vec<PluginMetadata>)` with metadata for all successfully loaded plugins
    /// - `Err(Vec<LoadError>)` with errors for failed plugins
    ///
    /// Failed plugins are logged but don't prevent loading of other plugins.
    pub fn load_all_metrics_plugins() -> Result<Vec<PluginMetadata>, Vec<LoadError>> {
        let dir = match plugins_dir() {
            Some(d) if d.is_dir() => d,
            Some(d) => {
                let _ = std::fs::create_dir_all(&d);
                return Ok(Vec::new());
            }
            None => return Ok(Vec::new()),
        };

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                crate::debug_log!("[plugin] Cannot read plugins dir '{}': {}", dir.display(), e);
                return Ok(Vec::new());
            }
        };

        let mut loaded_metadata = Vec::new();
        let mut errors = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            match load_single_plugin(&path) {
                Ok(metadata) => loaded_metadata.push(metadata),
                Err(e) => errors.push(LoadError {
                    plugin_dir: path,
                    error: e,
                }),
            }
        }

        if errors.is_empty() {
            if !loaded_metadata.is_empty() {
                crate::debug_log!("[plugin] Loaded {} metrics plugins", loaded_metadata.len());
            }
        } else {
            crate::debug_log!("[plugin] {} plugin load error(s)", errors.len());
            for e in &errors {
                crate::debug_log!("[plugin] {}: {}", e.plugin_dir.display(), e.error);
            }
        }

        if errors.is_empty() {
            Ok(loaded_metadata)
        } else {
            Err(errors)
        }
    }

    /// Call all loaded plugins' compute() functions.
    pub fn compute_all_metrics(snapshot_json: &str) -> HashMap<String, serde_json::Value> {
        REGISTRY
            .lock()
            .map(|reg| reg.compute_all(snapshot_json))
            .unwrap_or_default()
    }

    /// Check if any loaded plugin needs a specific capability.
    pub fn needs_capability(capability: &str) -> bool {
        REGISTRY
            .lock()
            .map(|reg| reg.needs_capability(capability))
            .unwrap_or(false)
    }

    /// Get the number of loaded plugins.
    pub fn plugin_count() -> usize {
        REGISTRY.lock().map(|r| r.len()).unwrap_or(0)
    }

    // ─── Internal Plugin Loading ───

    /// Load a single plugin from a directory.
    fn load_single_plugin(plugin_dir: &Path) -> Result<PluginMetadata, String> {
        let dylib_base = plugin_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or("Cannot determine directory name")?;

        let lib_path = find_dylib(plugin_dir, &dylib_base)?;
        crate::debug_log!("[plugin] Loading plugin from {}", lib_path.display());

        // Load the shared library
        let lib = unsafe { Library::new(&lib_path) }
            .map_err(|e| format!("Failed to load {}: {}", lib_path.display(), e))?;

        // Extract FFI function pointers
        let info_fn: libloading::Symbol<PluginInfoFn> = unsafe { lib.get(b"plugin_info") }
            .map_err(|e| format!("Missing symbol 'plugin_info': {}", e))?;
        let compute_fn: libloading::Symbol<PluginComputeFn> = unsafe { lib.get(b"plugin_compute") }
            .map_err(|e| format!("Missing symbol 'plugin_compute': {}", e))?;
        let free_fn: libloading::Symbol<PluginFreeFn> = unsafe { lib.get(b"plugin_free") }
            .map_err(|e| format!("Missing symbol 'plugin_free': {}", e))?;

        // Call plugin_info() to get metadata
        let info_ptr = unsafe { info_fn() };
        if info_ptr.is_null() {
            return Err("plugin_info() returned null".into());
        }

        let info_json = unsafe {
            CStr::from_ptr(info_ptr)
                .to_str()
                .map_err(|e| format!("Invalid UTF-8 in plugin_info: {}", e))?
                .to_string()
        };

        // Parse metadata
        let metadata: PluginMetadata = serde_json::from_str(&info_json)
            .map_err(|e| format!("Invalid plugin_info JSON: {} (raw: {})", e, info_json))?;

        // Validate name matches directory name
        let dir_name = plugin_dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or("Cannot determine directory name")?;
        if metadata.name != dir_name {
            return Err(format!(
                "Plugin name '{}' does not match directory name '{}'",
                metadata.name, dir_name
            ));
        }

        // Extract raw function pointers via transmute_copy
        // Safety: Symbol<T> contains a pointer to the function. transmute_copy
        // extracts the raw function pointer which remains valid as long as the
        // library is loaded.
        let info_ptr_raw: PluginInfoFn = unsafe { std::ptr::read(&*info_fn) };
        let compute_ptr_raw: PluginComputeFn = unsafe { std::ptr::read(&*compute_fn) };
        let free_ptr_raw: PluginFreeFn = unsafe { std::ptr::read(&*free_fn) };

        // Leak the library so it stays loaded for the process lifetime.
        // Box::leak returns &'static T which is Send + Sync.
        let lib_static: &'static Library = Box::leak(Box::new(lib));

        let loaded_plugin = LoadedPlugin {
            info_fn: info_ptr_raw,
            compute_fn: compute_ptr_raw,
            free_fn: free_ptr_raw,
            _lib: lib_static,
            metadata: metadata.clone(),
        };

        // Store in global registry
        {
            let mut reg = REGISTRY.lock().unwrap();
            reg.plugins.push(loaded_plugin);
        }

        crate::debug_log!(
            "[plugin] Loaded: {} v{} [{}]",
            metadata.display_name,
            metadata.version,
            metadata.capabilities.join(", ")
        );

        Ok(metadata)
    }

    /// Find the platform-specific dylib in a plugin directory.
    fn find_dylib(plugin_dir: &Path, dylib_name: &str) -> Result<PathBuf, String> {
        let ext = if cfg!(target_os = "macos") {
            ".dylib"
        } else if cfg!(target_os = "windows") {
            ".dll"
        } else {
            ".so"
        };

        let lib_path = plugin_dir.join(format!("{}{}", dylib_name, ext));
        if lib_path.exists() {
            return Ok(lib_path);
        }

        // Also check if any .so/.dylib/.dll file exists in the directory
        if let Ok(entries) = std::fs::read_dir(plugin_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let path = entry.path();
                if path.is_file() {
                    if let Some(ext) = path.extension() {
                        if ext == "dylib" || ext == "so" || ext == "dll" {
                            return Ok(path);
                        }
                    }
                }
            }
        }

        Err(format!(
            "No dylib found in {}. Expected '{}{}'",
            plugin_dir.display(),
            dylib_name,
            ext
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_plugins_dir() {
            let dir = plugins_dir();
            assert!(dir.is_some());
            assert!(dir.unwrap().ends_with(".sentrux/plugins"));
        }

        #[test]
        fn test_load_empty_dir() {
            let result = load_all_metrics_plugins();
            assert!(result.is_ok());
            assert!(result.unwrap().is_empty());
        }

        #[test]
        fn test_capability_check_no_plugins() {
            assert!(!needs_capability("comment_data"));
            assert!(!needs_capability("anything"));
        }

        #[test]
        fn test_compute_all_no_plugins() {
            let results = compute_all_metrics("{}");
            assert!(results.is_empty());
        }
    }
}
