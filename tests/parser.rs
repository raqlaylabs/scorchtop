//! Integration tests over the fixture tree in `tests/fixtures/`.

use std::path::PathBuf;

use agentop::aggregate::aggregate;
use agentop::source::claude_code::ClaudeCodeSource;
use agentop::source::Source;

fn fixture_root(sub: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(sub)
}

#[test]
fn scans_edge_case_fixtures() {
    let source = ClaudeCodeSource::with_root(fixture_root("cases"));
    let scan = source.scan();

    // Pre-dedup records:
    //   torn_last_line.jsonl:  1 x msg_A (torn tail silently ignored, user line skipped)
    //   duplicate_ids.jsonl:   2 x msg_A + 1 x msg_B
    //   unknown_model.jsonl:   1 x msg_U
    //   malformed_mid.jsonl:   msg_C + msg_D (garbage line counted as malformed)
    //   empty.jsonl:           0
    assert_eq!(scan.stats.files_scanned, 5);
    assert_eq!(scan.stats.malformed_lines, 1);
    assert_eq!(scan.records.len(), 7);

    // msg_A appears 3x across two files -> survives once.
    let agg = aggregate(&scan.records);
    assert_eq!(agg.duplicates_skipped, 2);
    assert_eq!(agg.totals.records, 5);

    // msg_A (1000/500/200/100) + msg_B (300/150) + msg_U (700/70)
    // + msg_C (10/5) + msg_D (20/10).
    assert_eq!(agg.totals.tokens.input, 1000 + 300 + 700 + 10 + 20);
    assert_eq!(agg.totals.tokens.output, 500 + 150 + 70 + 5 + 10);
    assert_eq!(agg.totals.tokens.cache_create, 200);
    assert_eq!(agg.totals.tokens.cache_read, 100);

    // Unknown model flags cost as incomplete but still counts tokens.
    assert!(agg.totals.has_unknown_model);
    assert!(agg.totals.known_cost > 0.0);
    let unknown = agg.by_model.get("quantum-mega-1").expect("unknown model bucket");
    assert_eq!(unknown.tokens.input, 700);
    assert_eq!(unknown.known_cost, 0.0);
    assert!(unknown.has_unknown_model);

    // Two projects, display names from cwd; two distinct local days.
    assert_eq!(agg.by_project.len(), 2);
    let names: Vec<_> = agg
        .by_project
        .values()
        .map(|p| p.display_name.as_str())
        .collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"beta"));
    assert_eq!(agg.by_day.len(), 2);
}

#[test]
fn scans_real_fixtures_when_present() {
    let root = fixture_root("real");
    let has_files = std::fs::read_dir(&root)
        .map(|d| d.flatten().any(|e| e.path().extension().is_some_and(|x| x == "jsonl")))
        .unwrap_or(false);
    if !has_files {
        eprintln!("skipping: no real fixtures present (tests/fixtures/real/ is git-ignored)");
        return;
    }

    // `fixtures/` acts as the scan root so `real/` is picked up as a project
    // directory containing the copied JSONL files.
    let source = ClaudeCodeSource::with_root(fixture_root(""));
    let scan = source.scan();
    let agg = aggregate(&scan.records);

    // Sanity: real Claude Code data parses without panic and yields usage.
    assert!(agg.totals.records > 0, "expected assistant records in real fixtures");
    assert!(agg.totals.tokens.total() > 0);
    assert!(!agg.by_model.is_empty());
}
