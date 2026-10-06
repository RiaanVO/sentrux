# Sentrux Plugin Architecture — Dylib Extension System

**Status:** Draft design spec  
**Date:** 2025-01  
**Audience:** Rust engineers implementing the plugin system  

---

## 1. Problem Statement

Adding a new metrics extension (e.g. `sentrux-comment`) currently requires modifying 8+ files in `sentrux-core`:

| File | Change |
|------|--------|
| `metrics/mod.rs` | Add `pub mod comment;` and import in `compute_health` |
| `metrics/types.rs` | No change (extension_data is already `HashMap<String, Value>`) |
| `core/types.rs` | Add `comment_lines`, `doc_comment_lines` to `FuncInfo` |
| `main_impl.rs` | Add `sentrux_comment::init()` call |
| `app/panels/health_display.rs` | Already handles generic `extension_data` — no change |
| `app/mcp_server/handlers.rs` | Already returns `extension_data` — no change |
| `Cargo.toml` (bin) | Add `sentrux-comment` dependency |
| `Cargo.toml` (workspace) | Add `sentrux-comment` workspace member |

This defeats the whole purpose of plugins — adding one requires touching the core codebase.

## 2. Goals

1. **Zero core changes** to add a new extension — drop a `.dylib` into `~/.sentrux/plugins/` and it works.
2. **Self-contained extensions** — each extension is a separate Rust crate compiled to a shared library.
3. **Auto-discovery** — scan `~/.sentrux/plugins/` at startup, load any valid metrics plugin.
4. **Capability negotiation** — extensions declare what data they need; core enables features accordingly.
5. **Backward compatibility** — existing C# extension works as-is (it's hardcoded in core); new extensions work as dylibs.
6. **Error resilience** — a failing plugin doesn't crash the application.

## 3. Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│ sentrux-bin                                                      │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │ lib::run()                                               │   │
│  │   1. license::init()  (load Pro dylib)                   │   │
│  │   2. plugin::sync_embedded_plugins()                     │   │
│  │   3. metrics::csharp::init()  (builtin — no change)      │   │
│  │   4. plugin_loader::load_metrics_plugins()  ★ NEW        │   │
│  └──────────────────────────────────────────────────────────┘   │
└────────────────────────┬────────────────────────────────────────┘
                         │
                    ┌────▼─────┐
                    │ .sentrux │
                    │ /plugins │
                    └────┬─────┘
              ┌──────────┼──────────┐
              │          │          │
         ┌────▼───┐ ┌───▼────┐ ┌──▼──────┐
         │csharp· │ │comment·│ │security │
         │dylib   │ │dylib   │ │dylib    │
         │(built) │ │(dist)  │ │(dist)   │
         └────────┘ └────────┘ └─────────┘
              │          │          │
              └──────────┼──────────┘
                         │  FFI: plugin_compute()
                    ┌────▼─────┐
                    │sentrux- │
                    │ core    │
                    │          │
                    │ Plugin   │
                    │ Registry │
                    └──────────┘
```

### 3.1 Key Design Decision: Two Plugin Types

We support **two categories** of plugins with different loading mechanisms:

| Category | Example | Loading | FFI Required? |
|----------|---------|---------|---------------|
| **Built-in metrics** | C# (CSharpMetrics) | Static Rust link, `init()` call | No |
| **External metrics** | Comment, Security, Test Gap | Dynamic load from `~/.sentrux/plugins/` | Yes |

The C# extension stays hardcoded in core for performance (it's shipped with every binary). All other extensions are external dylibs.

## 4. FFI Contract

### 4.1 Function Signatures

Every metrics plugin dylib must export exactly these three functions:

```c
// Returns plugin metadata as JSON. Called once at load time.
const char* plugin_info(void);

// Compute metrics for a snapshot. Called during health computation.
// snapshot_json: JSON string of the Snapshot (may be large, ~50-500KB)
// Returns: JSON string of metrics result (keyed by extension name)
const char* plugin_compute(const char* snapshot_json);

// Free memory allocated by plugin_compute or plugin_info.
// C functions should use boxed pointers so we can free on both sides.
void plugin_free(const char* ptr);
```

### 4.2 Plugin Info Format

`plugin_info()` returns JSON with this shape:

```json
{
  "name": "comment",
  "display_name": "Comment Health",
  "version": "0.1.0",
  "capabilities": ["comment_data"],
  "min_sentrux_version": "0.5.7"
}
```

| Field | Required | Description |
|-------|----------|-------------|
| `name` | Yes | Unique identifier. Used as the key in `HealthReport.extension_data`. |
| `display_name` | Yes | Human-readable name for UI. |
| `version` | Yes | Semver string for compatibility checks. |
| `capabilities` | Yes | Array of strings declaring what data the extension needs (see §5). |
| `min_sentrux_version` | No | Minimum sentrux version this plugin requires. |

### 4.3 Plugin Compute Input/Output

**Input (`snapshot_json`):** Full `Snapshot` serialized to JSON (same format as `core/snapshot.rs` sends). This includes:
- `root`: File tree (FileNode tree)
- `call_graph`, `import_graph`, `inherit_graph`: All dependency graphs
- `entry_points`: Detected entry points
- `exec_depth`: BFS distances

**Output:** `serde_json::Value` serialized to JSON string, representing the extension's metrics.

### 4.4 Memory Management

The FFI returns `*const c_char` — in Rust this is a `*const i8` pointing to a null-terminated C string.

**Allocation strategy:** Use `Box::into_raw(Box::new(c_string))` to allocate the string on the heap, then return the raw pointer. The plugin caller must call `plugin_free()` to release it.

On the Rust side (sentrux-core), after converting from `*const c_char` to `&str` and parsing, call `plugin_free()` to leak-free the string.

```rust
use std::ffi::CStr;
use std::os::raw::c_char;

// Type alias for FFI function pointers
type PluginInfoFn = extern "C" fn() -> *const c_char;
type PluginComputeFn = extern "C" fn(snapshot_json: *const c_char) -> *const c_char;
type PluginFreeFn = extern "C" fn(ptr: *const c_char);

fn safe_json_from_raw(raw: *const c_char) -> Option<String> {
    if raw.is_null() { return None; }
    let cstr = unsafe { CStr::from_ptr(raw) };
    let s = cstr.to_str().ok()?.to_string();
    unsafe { PLUGIN_FREE.unwrap()(raw) }; // free after use
    Some(s)
}
```

## 5. Capability Negotiation

### 5.1 Declared Capabilities

Extensions declare what they need via the `capabilities` array in `plugin_info()`:

```json
{
  "capabilities": ["comment_data"]
}
```

| Capability | Meaning | Core Behavior |
|------------|---------|---------------|
| *(empty array)* | No special data needed | Standard function/class/edge analysis only |
| `comment_data` | Extension needs `FuncInfo.comment_lines` and `doc_comment_lines` | Core enables comment line tracking in FuncInfo during parsing |

### 5.2 Runtime Capability Check

During startup, after loading a plugin and calling `plugin_info()`, the plugin loader checks capabilities:

```rust
// After loading plugin and getting its info:
let needs_comment = plugin_info.capabilities.contains(&"comment_data".to_string());
if needs_comment {
    comment_tracking_enabled = true;
}
```

Then in `compute_health()`, before iterating extensions:

```rust
// Only enable comment tracking if at least one plugin needs it
if comment_tracking_enabled {
    // comment_lines are already populated by the scanner
}
```

This means the core only enables comment tracking if an extension declares it needs it — not always.

### 5.3 Future Capabilities

The capability system is extensible. New capabilities can be added without breaking changes:

| Future Capability | Meaning |
|-------------------|---------|
| `source_text` | Extension needs raw file content (currently not available in Snapshot) |
| `git_history` | Extension needs git blame/history data |
| `dependency_manifest` | Extension needs package.json, Cargo.toml, etc. contents |

Extensions that declare unknown capabilities will be loaded but with a warning log — they'll still work with the standard Snapshot data.

## 6. Plugin Directory Structure

### 6.1 Directory Layout

Plugins live in `~/.sentrux/plugins/`. Each plugin is a subdirectory:

```
~/.sentrux/plugins/
├── comment/              # Directory name = plugin name
│   ├── comment.dylib     # macOS
│   ├── comment.so        # Linux
│   └── comment.dll       # Windows
├── security/
│   └── security.dylib
└── csharp/               # (optional) dylib version of C# extension
    └── csharp.dylib
```

### 6.2 File Naming Convention

The dylib filename must match: `{plugin_name}.{platform_extension}`

Platform extensions:
| Platform | Extension |
|----------|-----------|
| macOS (aarch64) | `.dylib` |
| macOS (x86_64) | `.dylib` |
| Linux (x86_64) | `.so` |
| Linux (aarch64) | `.so` |
| Windows (x86_64) | `.dll` |

The filename (without extension) must match the `name` field in `plugin_info()`.

## 7. Plugin Loader Architecture

### 7.1 Module Location

```
sentrux-core/src/plugin_loader.rs
```

This is a **new file** in sentrux-core. It contains:
- `PluginRegistry` — holds loaded plugin handles and their FFI function pointers
- `load_all_metrics_plugins()` — discovery and loading
- `compute_all_metrics(snapshot_json: &str) -> HashMap<String, Value>` — calls each plugin's `compute()`
- Capability negotiation logic

### 7.2 PluginRegistry Structure

```rust
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use libloading::{Library, Symbol};

// FFI function pointer types
pub type PluginInfoFn = extern "C" fn() -> *const c_char;
pub type PluginComputeFn = extern "C" fn(snapshot_json: *const c_char) -> *const c_char;
pub type PluginFreeFn = extern "C" fn(ptr: *const c_char);

/// Metadata returned by plugin_info()
#[derive(Debug, Clone)]
pub struct PluginMetadata {
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub capabilities: Vec<String>,
    pub min_sentrux_version: Option<String>,
}

/// Loaded plugin with FFI handles
#[derive(Debug)]
pub struct LoadedPlugin {
    /// Library stays loaded for the process lifetime (leaked).
    #[allow(dead_code)]
    lib: Library,
    /// Plugin metadata
    pub metadata: PluginMetadata,
    /// FFI function pointers
    info_fn: Symbol<PluginInfoFn>,
    compute_fn: Symbol<PluginComputeFn>,
    free_fn: Symbol<PluginFreeFn>,
}

/// Global registry of loaded metrics plugins
struct PluginRegistry {
    plugins: Vec<LoadedPlugin>,
}

static REGISTRY: std::sync::LazyLock<std::sync::Mutex<PluginRegistry>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new(PluginRegistry { plugins: Vec::new() })
    });

impl PluginRegistry {
    /// Call all plugins' compute() and return combined results.
    pub fn compute_all(&self, snapshot_json: &str) -> HashMap<String, serde_json::Value> {
        let mut results = HashMap::new();
        for plugin in &self.plugins {
            let snapshot_ptr = CString::new(snapshot_json).unwrap().into_raw();
            let result_ptr = unsafe { (plugin.compute_fn)(snapshot_ptr) };
            if let Some(result_json) = self.cstr_to_string(result_ptr) {
                // Free the snapshot pointer (owned by caller, but FFI doesn't use it)
                unsafe { drop(CString::from_raw(snapshot_ptr) };
                
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&result_json) {
                    results.insert(plugin.metadata.name.clone(), value);
                }
            }
        }
        results
    }
}
```

### 7.3 Discovery Algorithm

```rust
/// Discover and load all metrics plugins from ~/.sentrux/plugins/
pub fn load_all_metrics_plugins() -> Result<(), Vec<LoadError>> {
    let plugin_dir = match plugins_dir() {
        Some(d) if d.is_dir() => d,
        _ => return Ok(()), // No plugins dir yet — no plugins to load
    };

    let mut loaded = Vec::new();
    let mut errors = Vec::new();

    let entries = std::fs::read_dir(&plugin_dir)
        .map_err(|e| vec![LoadError {
            path: plugin_dir.clone(),
            error: format!("Cannot read directory: {}", e),
        }])?;

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; } // Skip non-directories
        
        match load_single_plugin(&path) {
            Ok(plugin) => loaded.push(plugin),
            Err(e) => {
                eprintln!("[plugin] Failed to load plugin at {}: {}", path.display(), e);
                errors.push(LoadError {
                    path: path.clone(),
                    error: e,
                });
            }
        }
    }

    if errors.is_empty() {
        crate::debug_log!("[plugin] Loaded {} metrics plugins", loaded.len());
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(())
}
```

### 7.4 Loading a Single Plugin

```rust
fn load_single_plugin(plugin_dir: &Path) -> Result<LoadedPlugin, String> {
    // 1. Find the dylib
    let lib_path = find_dylib(plugin_dir)?;
    
    // 2. Load the library
    let lib = unsafe { Library::new(&lib_path) }
        .map_err(|e| format!("Failed to load {}: {}", lib_path.display(), e))?;

    // 3. Get FFI function pointers
    let info_fn: Symbol<PluginInfoFn> = unsafe { lib.get(b"plugin_info") }
        .map_err(|e| format!("Missing plugin_info: {}", e))?;
    let compute_fn: Symbol<PluginComputeFn> = unsafe { lib.get(b"plugin_compute") }
        .map_err(|e| format!("Missing plugin_compute: {}", e))?;
    let free_fn: Symbol<PluginFreeFn> = unsafe { lib.get(b"plugin_free") }
        .map_err(|e| format!("Missing plugin_free: {}", e))?;

    // 4. Get plugin metadata
    let info_json = safe_cstr_to_string(unsafe { info_fn() })
        .ok_or("plugin_info returned null or invalid UTF-8")?;
    let metadata: PluginMetadata = serde_json::from_str(&info_json)
        .map_err(|e| format!("Invalid plugin_info JSON: {}", e))?;

    // 5. Validate name matches directory name
    let dir_name = plugin_dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or("Cannot determine directory name")?;
    if metadata.name != dir_name {
        return Err(format!(
            "Plugin name '{}' does not match directory name '{}'",
            metadata.name, dir_name
        ));
    }

    // 6. Leak the library (keep alive for process lifetime)
    std::mem::forget(lib); // Safety: we're storing raw function pointers

    Ok(LoadedPlugin {
        lib: unsafe { // Re-borrow for the struct
            // We need to re-create this — but since we leaked it,
            // we just store the handle. Actually, we skip storing the Library.
            // The library stays loaded because we forgot it.
            // We'll restructure below to keep it.
            // Actually, let's fix: store it but leak ownership...
        },
        metadata,
        info_fn,
        compute_fn,
        free_fn,
    })
}
```

### 7.5 Better Library Lifetime Management

Instead of `mem::forget`, use `Box::leak` or a `LazyLock`:

```rust
/// Holds a loaded plugin with its library kept alive.
pub struct LoadedPlugin {
    /// The library handle — stored to keep the library loaded.
    /// We use a wrapper that drops at process exit.
    _lib: Box<Library>, // Drops at Drop, but we never Drop this — stored in registry
    pub metadata: PluginMetadata,
    pub info_fn: unsafe fn() -> *const c_char,
    pub compute_fn: unsafe fn(*const c_char) -> *const c_char,
    pub free_fn: unsafe fn(*const c_char),
}
```

Actually, the simplest approach — use `libloading::Library` directly stored in the struct, but take `Box<Library>` and `forget` it so it never drops:

```rust
// In the registry, store plugins. The registry lives for the process lifetime
// via LazyLock. The Library instances are stored inside LoadedPlugin.
// When registry is dropped at process exit, Libraries are dropped.
// Actually, Library::drop() unloads the library — that's fine for us.
```

Let me simplify: just store `Library` in the struct. The registry is `LazyLock<Mutex<Registry>>` which lives until process exit.

### 7.6 Integration with Existing Metrics System

The plugin loader's `compute_all()` returns `HashMap<String, Value>` which directly populates `HealthReport.extension_data`:

```rust
// In sentrux-core/src/metrics/mod.rs — modify compute_health():

pub fn compute_health(snapshot: &Snapshot) -> HealthReport {
    // ... existing computation ...

    // OLD: (statically linked extensions only)
    // let extension_data: HashMap<String, Value> = {
    //     let exts = extensions();
    //     exts.iter().map(|ext| (ext.name().to_string(), ext.compute(snapshot))).collect()
    // };

    // NEW: Merge static + dynamic extensions
    let extension_data: HashMap<String, Value> = {
        // 1. Get static (in-core) extensions — C# is still here
        let static_exts = extensions();
        let mut result: HashMap<String, Value> = static_exts
            .iter()
            .map(|ext| (ext.name().to_string(), ext.compute(snapshot)))
            .collect();

        // 2. Merge dynamic (plugin) extensions
        let plugin_results = plugin_loader::compute_all_metrics(snapshot);
        for (name, value) in plugin_results {
            result.insert(name, value);
        }

        result
    };

    HealthReport {
        // ... all existing fields ...
        extension_data,
    }
}
```

### 7.7 Capability Check Integration

```rust
// In sentrux-core/src/metrics/mod.rs

pub fn compute_health(snapshot: &Snapshot) -> HealthReport {
    let files = flatten_files_ref(&snapshot.root);
    
    // Check if any plugin needs comment data (done once at startup, cached)
    let needs_comment = plugin_loader::needs_capability("comment_data");

    // Compute metrics (comment_lines are already in FuncInfo from scanner)
    // ...
}
```

The capability check is done **once at startup**, not during every health computation. The result is cached in the registry:

```rust
impl PluginRegistry {
    pub fn needs_capability(&self, capability: &str) -> bool {
        self.plugins.iter().any(|p| p.metadata.capabilities.contains(&capability.to_string()))
    }
}
```

## 8. Changes by Crate

### 8.1 sentrux-core (NEW FILE: `plugin_loader.rs`)

**New file:** `sentrux-core/src/plugin_loader.rs` (~300 lines)

**Contains:**
- `PluginMetadata` struct
- `LoadedPlugin` struct  
- `PluginRegistry` with lazy static
- `load_all_metrics_plugins()` — entry point
- `load_single_plugin()` — FFI loading
- `compute_all_metrics(snapshot: &Snapshot) -> HashMap<String, Value>`
- `needs_capability(capability: &str) -> bool`
- `plugins_dir()` — shared with `analysis::plugin::loader`

**Dependencies to add to `Cargo.toml`:**
- `libloading` — already present ✓
- No new dependencies needed

**Existing file changes in sentrux-core:**

1. **`metrics/mod.rs`** (modify `compute_health`):
   - Add `use crate::plugin_loader;`
   - Merge static + dynamic extension results in `extension_data`
   - Add capability check before computing

2. **`metrics/mod.rs`** (add re-export):
   ```rust
   pub mod plugin_loader;
   ```

3. **No changes needed to:**
   - `core/types.rs` — `FuncInfo` already has `comment_lines` and `doc_comment_lines`
   - `metrics/types.rs` — `extension_data` is already `HashMap<String, Value>`
   - `app/panels/health_display.rs` — already handles generic `extension_data`
   - `app/mcp_server/handlers.rs` — already returns `extension_data`

### 8.2 sentrux-bin (modify `main_impl.rs`)

**Remove this line:**
```rust
// REMOVE: sentrux_comment::init();
```

**Remove this line:**
```rust
// REMOVE from Cargo.toml: sentrux-comment = { path = "../../sentrux-comment" }
```

**Add this call (after csharp::init):**
```rust
// NEW: Load external metrics plugins from ~/.sentrux/plugins/
// Each plugin is a self-contained dylib with plugin_info/plugin_compute/plugin_free.
let _ = sentrux_core::plugin_loader::load_all_metrics_plugins();
```

The C# extension stays hardcoded — it's in-core, not a plugin.

### 8.3 sentrux-core (remove `metrics/csharp` as extension — optional)

**Future work (not in this phase):** The C# extension can be migrated to a dylib too. For now, it stays hardcoded as a static extension in core. This is the simplest migration path.

### 8.4 sentrux-comment (new crate structure)

The existing `sentrux-comment` crate needs to be restructured:

**New `Cargo.toml`:**
```toml
[package]
name = "sentrux-comment-plugin"
version = "0.1.0"
edition = "2021"
description = "Comment health metrics plugin for sentrux"
license = "MIT"

# NOT a binary — a shared library
[lib]
name = "sentrux_comment_plugin"
crate-type = ["cdylib", "rlib"]  # cdylib for dylib, rlib for dev-time tests

[dependencies]
serde_json = "1"  # Only serde_json — NO sentrux-core dependency!
```

Key change: **NO dependency on sentrux-core**. The plugin is entirely self-contained. It receives JSON input and returns JSON output — it doesn't need any sentrux types.

### 8.5 sentrux-comment plugin FFI exports

**`src/lib.rs`** (new file in sentrux-comment):
```rust
//! FFI entry point for sentrux-comment plugin.
//!
//! This crate is compiled as a dylib and exported with three C-compatible
//! functions: plugin_info, plugin_compute, plugin_free.

use serde_json::{json, Value};
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic;

// ── plugin_info — called once at load time ──

#[no_mangle]
pub extern "C" fn plugin_info() -> *const c_char {
    let info = json!({
        "name": "comment",
        "display_name": "Comment Health",
        "version": "0.1.0",
        "capabilities": ["comment_data"],
        "min_sentrux_version": "0.5.7"
    });
    let s = info.to_string();
    CString::into_raw(CString::new(s).unwrap()).cast()
}

// ── plugin_compute — called during health computation ──

#[no_mangle]
pub extern "C" fn plugin_compute(snapshot_json: *const c_char) -> *const c_char {
    // Catch panics and return error JSON instead of crashing
    let result = panic::catch_unwind(|| -> Result<Value, String> {
        let json_str = unsafe {
            let cstr = CStr::from_ptr(snapshot_json);
            cstr.to_str().ok().ok_or("Invalid UTF-8")?.to_string()
        };

        // Parse the Snapshot from JSON
        let snapshot: Snapshot = serde_json::from_str(&json_str)
            .map_err(|e| format!("Invalid snapshot: {}", e))?;

        // Compute metrics (import from internal modules)
        compute_comment_metrics(&snapshot)
    });

    match result {
        Ok(Ok(value)) => {
            let s = value.to_string();
            CString::into_raw(CString::new(s).unwrap()).cast()
        }
        Ok(Err(e)) => {
            let s = format!(r#"{{"error":"{}"}}"#, e);
            CString::into_raw(CString::new(s).unwrap()).cast()
        }
        Err(panic_err) => {
            let msg = if let Some(s) = panic_err.downcast_ref::<String>() {
                s.as_str()
            } else if let Some(s) = panic_err.downcast_ref::<&str>() {
                s
            } else {
                "plugin panicked"
            };
            let s = format!(r#"{{"error":"plugin panic: {}"}}"#, msg);
            CString::into_raw(CString::new(s).unwrap()).cast()
        }
    }
}

// ── plugin_free — free memory allocated by plugin_compute/plugin_info ──

#[no_mangle]
pub extern "C" fn plugin_free(ptr: *const c_char) {
    if ptr.is_null() { return; }
    unsafe { drop(CString::from_raw(ptr as *mut c_char)) };
}

// ── Internal metrics computation (moved from sentrux-comment/src/metrics/mod.rs) ──

fn compute_comment_metrics(snapshot: &Snapshot) -> Value {
    let files = flatten_files_ref(&snapshot.root);

    // Per-function comment analysis
    let per_func = get_per_function_comments(&files);
    let doc_coverage = compute_doc_coverage(&files);
    let distribution = detect_verbosity_patterns(&files);
    let length_stats = compute_comment_length_stats(&per_func);

    let (avg_ratio, max_ratio, verbose_func_count) = compute_func_stats(&per_func);
    let verbosity_score = compute_verbosity_score(avg_ratio);
    let doc_coverage_score = doc_coverage.coverage_ratio;

    let total_comment_lines: u32 = per_func.iter()
        .map(|fc| fc.total_comment_lines).sum();
    let total_code_lines: u32 = per_func.iter()
        .map(|fc| fc.end_line.saturating_sub(fc.start_line).saturating_sub(1).max(1))
        .sum();
    let overall_ratio = if total_code_lines > 0 {
        total_comment_lines as f64 / total_code_lines as f64
    } else { 0.0 };

    json!({
        "overall_ratio": overall_ratio,
        "avg_func_ratio": avg_ratio,
        "max_func_ratio": max_ratio,
        "verbose_func_count": verbose_func_count,
        "doc_coverage": doc_coverage.coverage_ratio,
        "undocumented_files": doc_coverage.undocumented_files.len(),
        "verbose_zone_count": distribution.verbose_zone_count,
        "verbosity_score": verbosity_score,
        "doc_coverage_score": doc_coverage_score,
        "avg_comment_length": length_stats.avg_comment_length,
        "long_comment_ratio": length_stats.long_comment_ratio,
    })
}
```

**The plugin defines its own `Snapshot` type** that mirrors the core type but contains only the JSON-serializable fields the plugin needs:

```rust
// The plugin has its own minimal Snapshot type — no sentrux-core dependency.
#[derive(serde::Deserialize)]
struct Snapshot {
    root: FileNode,
    #[allow(dead_code)]
    total_files: u32,
    #[allow(dead_code)]
    total_lines: u32,
    #[allow(dead_code)]
    total_dirs: u32,
    #[allow(dead_code)]
    call_graph: Vec<serde_json::Value>,
    #[allow(dead_code)]
    import_graph: Vec<serde_json::Value>,
    #[allow(dead_code)]
    inherit_graph: Vec<serde_json::Value>,
    #[allow(dead_code)]
    entry_points: Vec<serde_json::Value>,
    #[allow(dead_code)]
    exec_depth: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct FileNode {
    path: String,
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    is_dir: bool,
    #[serde(skip_deserializing)]
    children: Vec<FileNode>,
    #[serde(rename = "sa")]
    sa: Option<StructuralAnalysis>,
}

#[derive(serde::Deserialize)]
struct StructuralAnalysis {
    #[serde(rename = "fn")]
    functions: Option<Vec<FuncInfo>>,
}

#[derive(serde::Deserialize)]
struct FuncInfo {
    n: String,
    sl: u32,
    el: u32,
    ln: u32,
    #[allow(dead_code)]
    cc: Option<u32>,
    #[allow(dead_code)]
    cog: Option<u32>,
    #[allow(dead_code)]
    pc: Option<u32>,
    #[allow(dead_code)]
    bh: Option<u64>,
    #[allow(dead_code)]
    d: Option<Vec<String>>,
    #[allow(dead_code)]
    co: Option<Vec<String>>,
    #[serde(default)]
    is_public: bool,
    #[serde(default)]
    is_method: bool,
    comment_lines: u32,
    doc_comment_lines: u32,
}
```

## 9. Migration Plan

### 9.1 Phase 1: Core Plugin Loader (Current Scope)

**Duration:** ~1 week

**Tasks:**

1. **Create `sentrux-core/src/plugin_loader.rs`**
   - PluginRegistry, LoadedPlugin structs
   - `load_all_metrics_plugins()` with discovery
   - FFI binding types and helpers
   - `compute_all_metrics()` that calls each plugin
   - `needs_capability()` check

2. **Modify `sentrux-core/src/metrics/mod.rs`**
   - Add `use crate::plugin_loader`
   - Modify `compute_health()` to merge static + dynamic results
   - Add capability check

3. **Modify `sentrux-bin/src/main_impl.rs`**
   - Replace `sentrux_comment::init()` with `plugin_loader::load_all_metrics_plugins()`

4. **Modify `sentrux-bin/Cargo.toml`**
   - Remove `sentrux-comment` dependency

5. **Modify workspace `Cargo.toml`**
   - Remove `sentrux-comment` from workspace members (if present)

### 9.2 Phase 2: sentrux-comment as Plugin

**Duration:** ~3-5 days

**Tasks:**

1. **Restructure sentrux-comment crate**
   - New `Cargo.toml` with `crate-type = ["cdylib", "rlib"]`
   - Remove `sentrux-core` dependency
   - Define local `Snapshot`/`FileNode`/`FuncInfo` types matching core's serialization format
   - Move all metric computation logic to standalone functions
   - Add FFI `#[no_mangle]` exports

2. **Build & test locally**
   - `cargo build --release` — produces `target/release/libsentrux_comment_plugin.dylib`
   - Copy to `~/.sentrux/plugins/comment/comment.dylib`
   - Run `sentrux` and verify comment metrics appear in health panel

3. **Create distribution script**
   - Build script that produces platform-specific dylibs
   - Publish on GitHub releases: `comment-v0.1.0/darwin-arm64.dylib`, `linux-x86_64.so`, etc.

### 9.3 Phase 3: sentrux-dotnet/C# as Plugin (Future)

**Duration:** ~2 weeks (deferred — C# extension works fine in-core)

**Tasks:**

1. Move `sentrux-core/src/metrics/csharp/` code to a new `sentrux-csharp` crate
2. Restructure as dylib (same pattern as comment plugin)
3. Remove `csharp::init()` from `main_impl.rs`
4. Build and distribute as `csharp.dylib`

### 9.4 Phase 4: Plugin Management CLI (Future)

**Tasks:**

1. Extend `sentrux plugin` subcommand:
   - `sentrux plugin list` — list all metrics plugins
   - `sentrux plugin add <name>` — download from registry
   - `sentrux plugin remove <name>` — delete from plugins dir
   - `sentrux plugin validate <dir>` — validate plugin dylib
2. Create plugin registry (JSON index of available plugins with download URLs)

## 10. Error Handling

### 10.1 Plugin Load Failures

```
[plugin] Failed to load plugin at ~/.sentrux/plugins/broken/:
  Missing plugin_info symbol
```

Common failure modes and messages:

| Failure | Message | Recovery |
|---------|---------|----------|
| Missing directory | Skipped (no-op) | Create directory |
| No dylib found | "No compatible dylib found" | Install plugin |
| Library load fails | "Failed to load: <dlerror>" | Reinstall plugin |
| Missing `plugin_info` | "Missing plugin_info symbol" | Rebuild plugin |
| Missing `plugin_compute` | "Missing plugin_compute symbol" | Rebuild plugin |
| Missing `plugin_free` | "Missing plugin_free symbol" | Rebuild plugin |
| Invalid JSON from `plugin_info` | "Invalid plugin_info JSON: ..." | Fix plugin |
| Name mismatch | "Plugin name 'X' != directory 'Y'" | Rename directory |
| Panic during `plugin_info` | "plugin_info panicked" | Fix plugin |
| Panic during `plugin_compute` | JSON error field: `{"error":"plugin panic: ..."}` | Logged, extension skipped |

### 10.2 Plugin Runtime Errors

If a plugin panics during `compute()`, the error is caught and returned as a JSON error:

```json
{ "error": "plugin panic: index out of bounds" }
```

The plugin loader wraps each `compute()` call in `catch_unwind`. A crashed plugin's result is omitted from `extension_data` — other plugins continue to work.

### 10.3 Log Output

Plugin errors are logged at `debug` level (visible with `RUST_LOG=sentrux_core=debug`):

```
[plugin] Loading plugin at ~/.sentrux/plugins/comment
[plugin] Loaded plugin: comment v0.1.0 (capabilities: [comment_data])
[plugin] Loaded 1 metrics plugins
[plugin] Failed to load ~/.sentrux/plugins/broken: Missing plugin_compute symbol
```

## 11. Configuration

### 11.1 Plugin Directory Path

The plugin directory is always `~/.sentrux/plugins/` (same as language plugins).

**Access via shared function:** `sentrux_core::analysis::plugin::loader::plugins_dir()`

No separate configuration is needed — one directory for all plugins.

### 11.2 Environment Variables

For development and testing:

| Variable | Default | Purpose |
|----------|---------|---------|
| `SENTRUX_PLUGINS_DIR` | `~/.sentrux/plugins/` | Override plugin directory path |
| `SENTRUX_PLUGINS_SKIP` | (not set) | Comma-separated plugin names to skip loading |
| `SENTRUX_DEBUG_PLUGINS` | (not set) | If set, print verbose plugin loading log |

## 12. Building and Distributing a Plugin

### 12.1 Development Build

```bash
cd ~/Projects/sentrux-comment
cargo build --release
```

Produces: `target/release/libsentrux_comment_plugin.dylib` (macOS)

### 12.2 Install to Plugins Directory

```bash
mkdir -p ~/.sentrux/plugins/comment
cp target/release/libsentrux_comment_plugin.dylib ~/.sentrux/plugins/comment/comment.dylib
```

### 12.3 Verify

```bash
sentrux scan /path/to/project
# Open health panel — comment metrics should appear under "Extended Metrics"
```

### 12.4 Distribution

```bash
# Build for all platforms
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-unknown-linux-gnu
cargo build --release --target x86_64-pc-windows-gnu

# Rename and package
cp target/aarch64-apple-darwin/release/libsentrux_comment_plugin.dylib comment.dylib
cp target/x86_64-unknown-linux-gnu/release/libsentrux_comment_plugin.so comment.so
cp target/x86_64-pc-windows-gnu/release/libsentrux_comment_plugin.dll comment.dll

# Upload to GitHub releases
# URL pattern: https://github.com/sentrux/plugins/releases/download/comment-v0.1.0/comment-darwin-arm64.tar.gz
```

## 13. Security Considerations

### 13.1 No Sandbox

Plugins are loaded as dynamic libraries into the sentrux process — they have full access to the process memory space. This is an intentional design choice for simplicity and performance. Users trust plugins they install.

### 13.2 Mitigations

1. **Verify download integrity** — distribution mechanism should use signed releases (GitHub tags + SHA256 checksums)
2. **Plugin whitelist** (future) — optional config to only load plugins from trusted authors
3. **Isolated compute** (future) — run compute in a separate process via IPC for untrusted plugins

### 13.3 Panic Safety

All `compute()` calls are wrapped in `catch_unwind`. A plugin panic cannot crash sentrux — it returns an error JSON instead.

## 14. API Stability

### 14.1 FFI Contract

The three FFI functions (`plugin_info`, `plugin_compute`, `plugin_free`) are the **stable contract**. Breaking changes to this contract require:

1. Bump `min_sentrux_version` in plugin metadata
2. Version the FFI: `plugin_info_v1`, `plugin_compute_v1`, etc.
3. Document breaking changes in sentrux release notes

### 14.2 JSON Schema Stability

- `plugin_info` JSON structure: stable — new fields are additive
- `plugin_compute` return: stable — extensions should ignore unknown keys
- `Snapshot` JSON format: stable — matches `core/types.rs` serialization

## 15. Testing

### 15.1 Unit Tests (in plugin_loader.rs)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plugin_dir_exists() {
        let dir = plugins_dir();
        assert!(dir.is_some());
    }

    #[test]
    fn test_load_nonexistent_plugins() {
        // With no plugins dir, should return Ok with empty plugins
        let result = load_all_metrics_plugins();
        assert!(result.is_ok());
    }

    #[test]
    fn test_capability_check() {
        // After loading a mock plugin, check capability
        // (requires mocking — use test fixtures)
    }
}
```

### 15.2 Integration Tests

Create a test fixture plugin:

```
tests/fixtures/test_plugin/
└── test_plugin.dylib   # FFI plugin that returns: {"test_key": "test_value"}
```

Test that:
1. Plugin is discovered and loaded
2. `plugin_info()` returns correct metadata
3. `plugin_compute()` returns expected data
4. Capability check works
5. Plugin errors don't crash

### 15.3 Test Plugin Implementation

```rust
// tests/fixtures/test_plugin/src/lib.rs
use std::ffi::{CStr, CString};
use std::os::raw::c_char;

#[no_mangle]
pub extern "C" fn plugin_info() -> *const c_char {
    let s = r#"{"name":"test","display_name":"Test","version":"0.1.0","capabilities":[],"min_sentrux_version":"0.5.7"}"#;
    CString::into_raw(CString::new(s).unwrap()).cast()
}

#[no_mangle]
pub extern "C" fn plugin_compute(snapshot_json: *const c_char) -> *const c_char {
    // Ignore snapshot, return test data
    let s = r#"{"test_key":"test_value","test_metric":42}"#;
    CString::into_raw(CString::new(s).unwrap()).cast()
}

#[no_mangle]
pub extern "C" fn plugin_free(ptr: *const c_char) {
    if ptr.is_null() { return; }
    unsafe { drop(CString::from_raw(ptr as *mut c_char)) };
}
```

## 16. Appendix: Full File Changes Summary

### sentrux-core/src/plugin_loader.rs — NEW FILE

```rust
//! Runtime metrics plugin loader.
//!
//! Discovers .dylib/.so plugins in ~/.sentrux/plugins/<name>/ and loads them
//! via libloading. Each plugin exports plugin_info/plugin_compute/plugin_free.

pub mod loader {
    use std::collections::HashMap;
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::path::{Path, PathBuf};
    use std::panic;
    use libloading::{Library, Symbol};

    // ── Types ──

    #[derive(Debug, Clone)]
    pub struct PluginMetadata {
        pub name: String,
        pub display_name: String,
        pub version: String,
        pub capabilities: Vec<String>,
        pub min_sentrux_version: Option<String>,
    }

    #[derive(Debug)]
    pub struct LoadedPlugin {
        lib: Box<Library>,
        pub metadata: PluginMetadata,
        info_fn: unsafe fn() -> *const c_char,
        compute_fn: unsafe fn(*const c_char) -> *const c_char,
        free_fn: unsafe fn(*const c_char),
    }

    pub struct PluginRegistry {
        plugins: Vec<LoadedPlugin>,
    }

    impl PluginRegistry {
        pub fn new() -> Self { Self { plugins: Vec::new() } }
        pub fn compute_all(&self, snapshot_json: &str) -> HashMap<String, serde_json::Value> {
            let mut results = HashMap::new();
            for plugin in &self.plugins {
                match self.call_compute(plugin, snapshot_json) {
                    Ok(Some(value)) => {
                        results.insert(plugin.metadata.name.clone(), value);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        crate::debug_log!("[plugin] Compute error for '{}': {}", plugin.metadata.name, e);
                    }
                }
            }
            results
        }
        fn call_compute(&self, plugin: &LoadedPlugin, snapshot_json: &str) -> Result<Option<serde_json::Value>, String> {
            let snapshot_cstr = CString::new(snapshot_json).map_err(|e| format!("CString: {}", e))?;
            let snapshot_ptr = snapshot_cstr.as_ptr();
            let result = panic::catch_unwind(|| unsafe { (plugin.compute_fn)(snapshot_ptr) });
            match result {
                Ok(result_ptr) => {
                    if result_ptr.is_null() {
                        return Ok(None);
                    }
                    let json_str = unsafe { CStr::from_ptr(result_ptr).to_str().map_err(|e| format!("UTF-8: {}", e))? };
                    unsafe { (plugin.free_fn)(result_ptr) };
                    let value: serde_json::Value = serde_json::from_str(json_str)
                        .map_err(|e| format!("JSON parse: {}", e))?;
                    Ok(Some(value))
                }
                Err(panic_err) => {
                    let msg = if let Some(s) = panic_err.downcast_ref::<String>() {
                        s.as_str()
                    } else if let Some(s) = panic_err.downcast_ref::<&str>() {
                        s
                    } else { "unknown panic" };
                    Err(format!("Plugin panicked: {}", msg))
                }
            }
        }
        pub fn needs_capability(&self, capability: &str) -> bool {
            self.plugins.iter().any(|p| p.metadata.capabilities.contains(&capability.to_string()))
        }
        pub fn len(&self) -> usize { self.plugins.len() }
    }

    static REGISTRY: std::sync::LazyLock<std::sync::Mutex<PluginRegistry>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(PluginRegistry::new()));

    // ── Public API ──

    pub fn plugins_dir() -> Option<PathBuf> {
        std::env::var("SENTRUX_PLUGINS_DIR")
            .ok()
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join(".sentrux").join("plugins")))
    }

    pub fn load_all_metrics_plugins() -> Result<Vec<PluginMetadata>, Vec<LoadError>> {
        let dir = match plugins_dir() {
            Some(d) if d.is_dir() => d,
            Some(d) => {
                std::fs::create_dir_all(&d).ok();
                d
            }
            None => return Ok(Vec::new()),
        };

        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => return Ok(Vec::new()),
        };

        let mut metadata = Vec::new();
        let mut errors = Vec::new();

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() { continue; }
            match load_single_plugin(&path) {
                Ok(loaded) => metadata.push(loaded.metadata.clone()),
                Err(e) => errors.push(LoadError { path: path.clone(), error: e }),
            }
        }

        if !errors.is_empty() {
            crate::debug_log!("[plugin] {} plugin load errors", errors.len());
            for e in &errors {
                crate::debug_log!("[plugin] {}: {}", e.path.display(), e.error);
            }
        } else if !metadata.is_empty() {
            crate::debug_log!("[plugin] Loaded {} metrics plugins", metadata.len());
        }

        // Store in global registry
        {
            let mut reg = REGISTRY.lock().unwrap();
            for entry in &metadata {
                // Find the loaded plugin and store it
            }
        }

        if errors.is_empty() {
            Ok(metadata)
        } else {
            Err(errors)
        }
    }

    pub fn compute_all_metrics(snapshot_json: &str) -> HashMap<String, serde_json::Value> {
        REGISTRY.lock()
            .map(|reg| reg.compute_all(snapshot_json))
            .unwrap_or_default()
    }

    pub fn needs_capability(capability: &str) -> bool {
        REGISTRY.lock()
            .map(|reg| reg.needs_capability(capability))
            .unwrap_or(false)
    }

    // ── Internal ──

    fn load_single_plugin(plugin_dir: &Path) -> Result<LoadedPlugin, String> {
        // Find dylib
        let dylib_name = match (cfg!(target_os = "macos"), cfg!(target_os = "windows")) {
            (true, _) => format!("{}.dylib", plugin_dir.file_name().unwrap_or_default().to_string_lossy()),
            (false, true) => format!("{}.dll", plugin_dir.file_name().unwrap_or_default().to_string_lossy()),
            (false, false) => format!("{}.so", plugin_dir.file_name().unwrap_or_default().to_string_lossy()),
        };
        let lib_path = plugin_dir.join(&dylib_name);
        if !lib_path.exists() {
            return Err(format!("No dylib found: {}", lib_path.display()));
        }

        // Load library
        let lib = unsafe { Library::new(&lib_path) }
            .map_err(|e| format!("Failed to load {}: {}", lib_path.display(), e))?;

        // Get symbols
        let info_fn: Symbol<unsafe extern "C" fn() -> *const c_char> = 
            unsafe { lib.get(b"plugin_info") }.map_err(|e| format!("Missing plugin_info: {}", e))?;
        let compute_fn: Symbol<unsafe extern "C" fn(*const c_char) -> *const c_char> =
            unsafe { lib.get(b"plugin_compute") }.map_err(|e| format!("Missing plugin_compute: {}", e))?;
        let free_fn: Symbol<unsafe extern "C" fn(*const c_char)> =
            unsafe { lib.get(b"plugin_free") }.map_err(|e| format!("Missing plugin_free: {}", e))?;

        // Get metadata
        let info_json = unsafe {
            let cstr = CStr::from_ptr(info_fn());
            cstr.to_str().ok().ok_or("Invalid UTF-8 in plugin_info")?
        }.to_string();
        let metadata: PluginMetadata = serde_json::from_str(&info_json)
            .map_err(|e| format!("Invalid plugin_info JSON: {}", e))?;

        // Validate name
        let dir_name = plugin_dir.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .ok_or("Cannot determine dir name")?;
        if metadata.name != dir_name {
            return Err(format!("Name mismatch: plugin='{}' dir='{}'", metadata.name, dir_name));
        }

        // Store in registry
        let loaded = LoadedPlugin {
            lib: Box::new(unsafe { lib }), // leaks library for process lifetime
            metadata: metadata.clone(),
            info_fn: *info_fn,
            compute_fn: *compute_fn,
            free_fn: *free_fn,
        };
        REGISTRY.lock().unwrap().plugins.push(loaded);

        crate::debug_log!("[plugin] Loaded: {} v{} (caps: {:?})", 
            metadata.display_name, metadata.version, metadata.capabilities);

        Ok(loaded)
    }

    #[derive(Debug)]
    pub struct LoadError {
        pub path: PathBuf,
        pub error: String,
    }
}
```

### sentrux-core/src/metrics/mod.rs — MODIFICATION

```rust
// ADD at top:
use crate::plugin_loader::loader;

// MODIFY compute_health():
pub fn compute_health(snapshot: &Snapshot) -> HealthReport {
    let files = flatten_files_ref(&snapshot.root);
    
    // Capability check (cached at load time)
    if loader::needs_capability("comment_data") {
        // Comment tracking already enabled by scanner
    }

    // ... existing computation ...

    // Merge static + dynamic extension results:
    let extension_data: HashMap<String, serde_json::Value> = {
        let mut result: HashMap<String, serde_json::Value> = {
            let exts = extensions();
            exts.iter()
                .map(|ext| (ext.name().to_string(), ext.compute(snapshot)))
                .collect()
        };
        // Merge plugin results
        let snapshot_json = serde_json::to_string(snapshot).expect("Snapshot serialization failed");
        let plugin_results = loader::compute_all_metrics(&snapshot_json);
        result.extend(plugin_results);
        result
    };

    HealthReport {
        // ... all existing fields unchanged ...
        extension_data,
    }
}
```

### sentrux-bin/src/main_impl.rs — MODIFICATION

```rust
// REPLACE these two lines:
// sentrux_core::metrics::csharp::init();
// sentrux_comment::init();

// WITH:
sentrux_core::metrics::csharp::init();  // Keep C# as built-in

// NEW: Load external metrics plugins
let _ = sentrux_core::plugin_loader::loader::load_all_metrics_plugins();
```

### sentrux-bin/Cargo.toml — MODIFICATION

```toml
[dependencies]
sentrux-core = { path = "../sentrux-core" }
clap = { version = "4", features = ["derive"] }
eframe = { workspace = true }
egui = { workspace = true }
dirs = "6"

# REMOVE: sentrux-comment = { path = "../../sentrux-comment" }
```
