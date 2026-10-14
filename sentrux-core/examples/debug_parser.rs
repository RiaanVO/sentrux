// Debug parser: reads TypeScript files and prints extracted functions/classes
// Run with: cargo run --example debug_parser
// Uses the public parse_file API

fn main() {
    let files = vec![
        "/Users/Shared/Projects/web-scraper/packages/core/src/types.ts",
        "/Users/Shared/Projects/web-scraper/packages/core/src/crawler.ts",
    ];

    for file_path in &files {
        println!("\n================================================================================");
        println!("FILE: {}", file_path);
        println!("================================================================================");

        // Parse using sentrux-core public API
        let lang = "typescript";
        let max_parse_size = 1024; // 1MB
        let sa = match sentrux_core::analysis::parser::parse_file(file_path, lang, max_parse_size) {
            Some(sa) => sa,
            None => {
                println!("ERROR: parse_file returned None for {}", file_path);
                continue;
            }
        };

        println!("\n--- CLASSES/INTERFACES ---");
        match &sa.cls {
            Some(classes) => {
                println!("Count: {}", classes.len());
                for cls in classes {
                    let kind = cls.k.as_deref().unwrap_or("unknown");
                    let ln = cls.el.wrapping_sub(cls.sl).wrapping_add(1);
                    let range = format!("lines {}-{} ({} lines)", cls.sl, cls.el, ln);
                    let visibility = if cls.is_public { "public" } else { "internal" };
                    println!(
                        "  [{:12}] {} {} ({}): {} comment lines, {} doc lines",
                        kind, cls.n, range, visibility, cls.comment_lines, cls.doc_comment_lines
                    );
                }
            }
            None => println!("  (none)"),
        }

        println!("\n--- FUNCTIONS ---");
        match &sa.functions {
            Some(functions) => {
                println!("Count: {}", functions.len());
                for func in functions {
                    let range = format!("lines {}-{} ({} lines)", func.sl, func.el, func.ln);
                    let visibility = if func.is_public { "public" } else { "internal" };
                    println!(
                        "  {}: {} ({}): {} comment lines, {} doc lines",
                        func.n,
                        range,
                        visibility,
                        func.comment_lines,
                        func.doc_comment_lines
                    );
                }
            }
            None => println!("  (none)"),
        }

        println!("\n--- CALLS ---");
        match &sa.co {
            Some(calls) if !calls.is_empty() => {
                println!("Module-level calls: {:?}", calls);
            }
            _ => println!("  (none)"),
        }

        match &sa.functions {
            Some(functions) => {
                for func in functions {
                    if let Some(calls) = &func.co {
                        if !calls.is_empty() {
                            println!("  {} calls: {:?}", func.n, calls);
                        }
                    }
                }
            }
            None => {}
        }

        println!("\n--- IMPORTS ---");
        match &sa.imp {
            Some(imports) => {
                println!("Count: {}", imports.len());
                for imp in imports {
                    println!("  {}", imp);
                }
            }
            None => println!("  (none)"),
        }

        println!("\n--- COMMENT LINES (file total) ---");
        match sa.comment_lines {
            Some(n) => println!("  Total comment lines in file: {}", n),
            None => println!("  (not calculated)"),
        }
    }
}
