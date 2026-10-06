use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use libloading::Library;

fn main() {
    // Read the actual snapshot JSON
    let json = std::fs::read_to_string("/tmp/sentrux_snapshot.json")
        .expect("Failed to read snapshot JSON");
    
    println!("Read JSON: {} bytes", json.len());
    
    // Load the comment plugin
    let lib = unsafe {
        Library::new("/Users/riaanvo/.sentrux/plugins/comment/comment.dylib")
            .expect("Failed to load plugin")
    };
    
    type PluginComputeFn = unsafe extern "C" fn(*const c_char) -> *const c_char;
    type PluginFreeFn = unsafe extern "C" fn(*const c_char);
    
    let compute_fn: libloading::Symbol<PluginComputeFn> = unsafe {
        lib.get(b"plugin_compute").expect("Missing plugin_compute")
    };
    let free_fn: libloading::Symbol<PluginFreeFn> = unsafe {
        lib.get(b"plugin_free").expect("Missing plugin_free")
    };
    
    // Create CString and call plugin
    let cstr = CString::new(json.clone()).unwrap();
    println!("CString created: {} bytes", cstr.as_bytes().len());
    
    let result_ptr = unsafe { compute_fn(cstr.as_ptr()) };
    
    if result_ptr.is_null() {
        println!("Result: NULL");
    } else {
        let result_str = unsafe { CStr::from_ptr(result_ptr).to_str().unwrap() };
        println!("Result: {}", result_str);
        unsafe { free_fn(result_ptr as *mut c_char) };
    }
}
