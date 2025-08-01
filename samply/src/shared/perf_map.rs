use std::sync::Arc;

use debugid::DebugId;
use fxprof_processed_profile::{LibMappings, LibraryInfo, Profile, Symbol, SymbolTable};

use super::jit_category_manager::JitCategoryManager;
use super::jit_function_recycler::JitFunctionRecycler;
use super::lib_mappings::LibMappingInfo;

fn process_perf_map_line(line: &str) -> Option<(u64, u64, &str)> {
    let mut split = line.splitn(3, ' ');
    let addr = split.next()?;
    let len = split.next()?;
    let symbol_name = split.next()?;
    if symbol_name.is_empty() {
        return None;
    }
    let addr = u64::from_str_radix(addr.trim_start_matches("0x"), 16).ok()?;
    let len = u64::from_str_radix(len.trim_start_matches("0x"), 16).ok()?;
    Some((addr, len, symbol_name))
}

/// Tries to load a perf mapping file that could have been generated by the process during
/// execution.
pub fn try_load_perf_map(
    pid: u32,
    profile: &mut Profile,
    jit_category_manager: &mut JitCategoryManager,
    mut recycler: Option<&mut JitFunctionRecycler>,
) -> Option<LibMappings<LibMappingInfo>> {
    let name = format!("perf-{pid}.map");
    let path = format!("/tmp/{name}");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return None;
    };

    // Read the map file and set everything up so that absolute addresses
    // in JIT code get symbolicated to the right function name.

    // There are three ways to put function names into the profile:
    //
    //  1. Function name without address ("label frame"),
    //  2. Address with after-the-fact symbolicated function name, and
    //  3. Address with up-front symbolicated function name.
    //
    // Having the address on the frame allows the assembly view in the
    // Firefox profiler to compute the right hitcount per instruction.
    // However, with a perf.map file, we don't have the code bytes of the jitted
    // code, so we have no way of displaying the instructions. So the code
    // address is not overly useful information, and we could just discard
    // it and use label frames for perf.map JIT frames (approach 1).
    //
    // We'll be using approach 3 here anyway, so our JIT frames will have
    // both a function name and a code address.

    // Create a fake "library" for the JIT code.
    let lib_handle = profile.add_lib(LibraryInfo {
        debug_name: name.clone(),
        name,
        debug_path: path.clone(),
        path,
        debug_id: DebugId::nil(),
        code_id: None,
        arch: None,
    });

    let mut symbols = Vec::new();
    let mut mappings = LibMappings::new();
    let mut cumulative_address = 0;

    for (addr, len, symbol_name) in content.lines().filter_map(process_perf_map_line) {
        let start_address = addr;
        let end_address = addr + len;
        let code_size = len as u32;

        // Pretend that all JIT code is laid out consecutively in our fake library.
        // This relative address is used for symbolication whenever we add a frame
        // to the profile.
        let relative_address = cumulative_address;
        cumulative_address += code_size;

        // Add a symbol for this function to the fake library's symbol table.
        // This symbol will be looked up when the address is added to the profile,
        // based on the relative address.
        symbols.push(Symbol {
            address: relative_address,
            size: Some(code_size),
            name: symbol_name.to_owned(),
        });

        let (lib_handle, relative_address) = if let Some(recycler) = recycler.as_deref_mut() {
            recycler.recycle(symbol_name, code_size, lib_handle, relative_address)
        } else {
            (lib_handle, relative_address)
        };

        let (category, js_frame) = jit_category_manager.classify_jit_symbol(symbol_name, profile);

        // Add this function to the JIT lib mappings so that it can be consulted for
        // category information, JS function prepending, and to translate the absolute
        // address into a relative address.
        mappings.add_mapping(
            start_address,
            end_address,
            relative_address,
            LibMappingInfo::new_jit_function(lib_handle, category, js_frame),
        );
    }

    profile.set_lib_symbol_table(lib_handle, Arc::new(SymbolTable::new(symbols)));

    Some(mappings)
}
