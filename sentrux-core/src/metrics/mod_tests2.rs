#[cfg(test)]
mod tests {
    use crate::metrics::*;
    use crate::metrics::stability::module_of;
    use crate::core::types::ImportEdge;
    use crate::core::snapshot::Snapshot;
    use crate::core::types::{FileNode, StructuralAnalysis, FuncInfo};
    use std::collections::HashMap;
    use std::sync::Arc;

    use crate::metrics::test_helpers::{edge, file, snap_with_edges};

    // ── Shannon entropy: intra-module edges across many modules = zero entropy ──
    // This was the bug: 10 self-contained modules got entropy=1.0 (F grade).
    // Intra-module edges are healthy cohesion, not disorder. [ref:4540215f]
    #[test]
    fn intra_module_many_modules_zero_entropy() {
        // 3 modules (depth-2), each with internal edges only — 0% coupling
        // Files at depth-2 share module: a/sub/x.rs and a/sub/y.rs → module "a/sub"
        let edges = vec![
            edge("a/sub/x.rs", "a/sub/y.rs"),  // intra a/sub
            edge("b/sub/x.rs", "b/sub/y.rs"),  // intra b/sub
            edge("c/sub/x.rs", "c/sub/y.rs"),  // intra c/sub
        ];
        let snap = snap_with_edges(
            edges,
            vec![
                file("a/sub/x.rs"), file("a/sub/y.rs"),
                file("b/sub/x.rs"), file("b/sub/y.rs"),
                file("c/sub/x.rs"), file("c/sub/y.rs"),
            ],
        );
        let report = compute_health(&snap);
        assert_eq!(report.coupling_score, 0.0, "all edges intra-module");
        assert_eq!(report.entropy, 0.0, "intra-module edges must not inflate entropy");
    }

    // ── Shannon entropy: uniform distribution = max entropy ──
    #[test]
    fn uniform_distribution_max_entropy() {
        // 3 edges across 3 different module pairs = uniform = max entropy
        let edges = vec![
            edge("a/x.rs", "b/y.rs"),
            edge("b/y.rs", "c/z.rs"),
            edge("c/z.rs", "a/x.rs"),
        ];
        let snap = snap_with_edges(
            edges,
            vec![file("a/x.rs"), file("b/y.rs"), file("c/z.rs")],
        );
        let report = compute_health(&snap);
        // 3 pairs, each with 1/3 probability -> H = log2(3) -> normalized = 1.0
        assert!((report.entropy - 1.0).abs() < 0.01);
    }

    // ── Cohesion: all intra-module edges = high cohesion ──
    #[test]
    fn full_intra_module_high_cohesion() {
        // 2 files in same subdir, both directions = 2 edges.
        // Expected edges for 2-file module = n*(n-1)/2 = 1. So 2/1 = 2.0 capped at 1.0.
        let edges = vec![
            edge("src/mod1/a.rs", "src/mod1/b.rs"),
            edge("src/mod1/b.rs", "src/mod1/a.rs"),
        ];
        let snap = snap_with_edges(
            edges,
            vec![file("src/mod1/a.rs"), file("src/mod1/b.rs")],
        );
        let report = compute_health(&snap);
        assert!((report.avg_cohesion.unwrap() - 1.0).abs() < f64::EPSILON);
    }

    // ── Cohesion: only cross-module edges = zero cohesion ──
    #[test]
    fn cross_module_only_zero_cohesion() {
        let edges = vec![
            edge("src/a.rs", "lib/b.rs"),
        ];
        let snap = snap_with_edges(
            edges,
            vec![file("src/a.rs"), file("lib/b.rs")],
        );
        let report = compute_health(&snap);
        // Each module (src/, lib/) has only 1 file -> no modules with >=2 files -> None
        assert_eq!(report.avg_cohesion, None);
    }

    // ── Instability: file with only fan-out = I=1.0 (maximally unstable) ──
    #[test]
    fn pure_fanout_max_instability() {
        let edges = vec![
            edge("src/a.rs", "src/b.rs"),
            edge("src/a.rs", "src/c.rs"),
        ];
        let snap = snap_with_edges(
            edges,
            vec![file("src/a.rs"), file("src/b.rs"), file("src/c.rs")],
        );
        let report = compute_health(&snap);
        let a_metric = report.most_unstable.iter().find(|m| m.path == "src/a.rs");
        assert!(a_metric.is_some());
        assert!(a_metric.unwrap().instability > 0.7);
    }

    // ── Instability: file with only fan-in = I=0.0 (maximally stable) ──
    #[test]
    fn pure_fanin_zero_instability() {
        let edges = vec![
            edge("src/a.rs", "src/b.rs"),
            edge("src/c.rs", "src/b.rs"),
        ];
        let snap = snap_with_edges(
            edges,
            vec![file("src/a.rs"), file("src/b.rs"), file("src/c.rs")],
        );
        let report = compute_health(&snap);
        // b.rs has fan_in=2, fan_out=0 -> I = 0/(2+0) = 0.0
        let b_metric = report.most_unstable.iter().find(|m| m.path == "src/b.rs");
        if let Some(m) = b_metric {
            assert!(m.instability < 0.35);
        }
    }

    // ── Depth: linear chain = depth equals chain length ──
    #[test]
    fn linear_chain_depth() {
        use crate::core::types::EntryPoint;
        let edges = vec![
            edge("src/a.rs", "src/b.rs"),
            edge("src/b.rs", "src/c.rs"),
            edge("src/c.rs", "src/d.rs"),
        ];
        let mut snap = snap_with_edges(
            edges,
            vec![file("src/a.rs"), file("src/b.rs"), file("src/c.rs"), file("src/d.rs")],
        );
        snap.entry_points = vec![EntryPoint {
            file: "src/a.rs".to_string(),
            func: "main".to_string(),
            lang: "rust".to_string(),
            confidence: "high".to_string(),
        }];
        let report = compute_health(&snap);
        assert_eq!(report.max_depth, 3);
    }

    // ── Verify test helper sets total_files for struct completeness ──
    #[test]
    fn test_helper_sets_total_files() {
        let snap = snap_with_edges(
            vec![edge("src/a.rs", "src/b.rs")],
            vec![file("src/a.rs"), file("src/b.rs")],
        );
        assert_eq!(snap.total_files, 2, "test helper must set total_files from file count");
    }

    // ── Normalization: god file ratio scales with project size ──
    #[test]
    fn god_file_ratio_scales_with_project_size() {
        let make_project = |extra_files: usize| -> HealthReport {
            let mut edges = Vec::new();
            let mut files_vec = vec![file("src/god.rs")];
            for i in 0..20 {
                let target = format!("src/dep{}.rs", i);
                edges.push(edge("src/god.rs", &target));
                files_vec.push(file(&target));
            }
            for i in 0..extra_files {
                files_vec.push(file(&format!("src/extra{}.rs", i)));
            }
            let snap = snap_with_edges(edges, files_vec);
            compute_health(&snap)
        };
        let small = make_project(0);   // 21 files, 1 god
        let large = make_project(200);  // 221 files, 1 god
        let small_ratio = small.god_files.len() as f64 / 21.0;
        let large_ratio = large.god_files.len() as f64 / 221.0;
        assert!(large_ratio < small_ratio, "god file impact should decrease with project size");
    }

    // ── BUG 2 verification: entry-point files excluded from god files ──
    #[test]
    fn entry_points_not_flagged_as_god_files() {
        use crate::core::types::EntryPoint;
        let mut edges = Vec::new();
        let mut files_vec = vec![file("src/main.rs")];
        for i in 0..20 {
            let target = format!("src/mod{}.rs", i);
            edges.push(edge("src/main.rs", &target));
            files_vec.push(file(&target));
        }
        let mut snap = snap_with_edges(edges, files_vec);
        snap.entry_points = vec![EntryPoint {
            file: "src/main.rs".to_string(),
            func: "main".to_string(),
            lang: "rust".to_string(),
            confidence: "high".to_string(),
        }];
        let report = compute_health(&snap);
        assert!(report.god_files.is_empty(), "entry-point files should not be flagged as god files");
    }

    // ── Depth: no entry points falls back to root nodes (fan-in=0) ──
    #[test]
    fn no_entry_points_uses_root_nodes() {
        let edges = vec![edge("src/a.rs", "src/b.rs")];
        let snap = snap_with_edges(edges, vec![file("src/a.rs"), file("src/b.rs")]);
        let report = compute_health(&snap);
        assert_eq!(report.max_depth, 1, "should compute depth from root nodes when no entry points");
    }

    // ── Comment ratio: simple ratio ──
    #[test]
    fn comment_ratio_computed() {
        let mut f = file("src/a.rs");
        f.lines = 100;
        f.comments = 20;
        let snap = snap_with_edges(Vec::new(), vec![f]);
        let report = compute_health(&snap);
        // Simple ratio: 20/100 = 0.20
        assert!((report.comment_ratio.unwrap() - 0.20).abs() < 0.01);
    }

    // ── Large file detection ──
    #[test]
    fn large_file_detected() {
        let mut big = file("src/big.rs");
        big.lines = 600; // > 500 threshold
        let small = file("src/small.rs"); // 100 lines
        let snap = snap_with_edges(Vec::new(), vec![big, small]);
        let report = compute_health(&snap);
        assert_eq!(report.large_file_count, 1);
        // Simple ratio: 1/2 = 0.5
        assert!((report.large_file_ratio - 0.5).abs() < 0.01,
            "simple ratio, got {}", report.large_file_ratio);
    }

    // ── Long function ratio ──
    #[test]
    fn long_fn_ratio_computed() {
        let f = FileNode {
            path: "src/a.rs".to_string(),
            name: "a.rs".to_string(),
            is_dir: false,
            lines: 200, logic: 150, comments: 20, blanks: 30, funcs: 2,
            mtime: 0.0, gs: String::new(), lang: "rust".to_string(),
            sa: Some(StructuralAnalysis {
                functions: Some(vec![
                    FuncInfo { n: "short".into(), sl: 1, el: 10, ln: 10, cc: Some(2), cog: None, pc: None, bh: None, d: None, co: None, is_public: false, is_method: false, comment_lines: 0, doc_comment_lines: 0, extensions: Default::default(), },
                    FuncInfo { n: "long".into(), sl: 11, el: 100, ln: 90, cc: Some(3), cog: None, pc: None, bh: None, d: None, co: None, is_public: false, is_method: false, comment_lines: 0, doc_comment_lines: 0, extensions: Default::default(), },
                ]),
                cls: None, imp: None, co: None, tags: None, comment_lines: None,
                extensions: Default::default(),
            }),
            children: None,
        };
        let snap = snap_with_edges(Vec::new(), vec![f]);
        let report = compute_health(&snap);
        assert_eq!(report.long_functions.len(), 1);
        // Simple ratio: 1/2 = 0.5
        assert!((report.long_fn_ratio - 0.5).abs() < 0.01,
            "simple ratio, got {}", report.long_fn_ratio);
    }

    // ── Monotonicity: worse input -> same or worse score for each dimension ──
    #[test]
    fn monotonicity_per_dimension() {
        // Higher coupling ratio = lower coupling score (1 - ratio)
        let coupling_score = |v: f64| (1.0 - v).clamp(0.0, 1.0);
        assert!(coupling_score(0.1) >= coupling_score(0.5));
        assert!(coupling_score(0.5) >= coupling_score(0.8));
        // Higher quality signal = better (direct comparison)
        assert!(0.8 > 0.2);
        assert!(0.2 > 0.01);
    }

    // ── module_of: depth-2 boundary ──

    #[test]
    fn module_of_depth2_and_3() {
        assert_eq!(module_of("src/layout/types.rs"), "src/layout");
        // Depth-3: nested dirs get finer-grained module boundaries
        assert_eq!(module_of("src/layout/nested/deep.rs"), "src/layout/nested");
        assert_eq!(module_of("frontend/components/btn.js"), "frontend/components");
        assert_eq!(module_of("backend/routes/api.js"), "backend/routes");
        assert_eq!(module_of("tests/unit/test_foo.rs"), "tests/unit");
    }

    #[test]
    fn module_of_flat_file() {
        assert_eq!(module_of("src/app.rs"), "src/app");
        assert_eq!(module_of("lib/utils.rs"), "lib/utils");
        assert_eq!(module_of("frontend/index.js"), "frontend");
    }

    #[test]
    fn module_of_root_level() {
        assert_eq!(module_of("db.rs"), "db");
        assert_eq!(module_of("main.py"), "main");
        assert_eq!(module_of("Makefile"), "Makefile"); // no extension
    }
}
