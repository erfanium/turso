//! Replays a captured application corpus (not checked in) through the
//! rewriter and prepares every statement against an in-memory database.
//! Skipped when `mysql/corpus/` is absent.

use std::collections::BTreeMap;
use std::path::PathBuf;

use turso_mysql::catalog::Catalog;
use turso_mysql::dialect::open_memory_database;
use turso_mysql::rewrite::rewrite;

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../corpus")
}

#[test]
fn corpus_prepares() {
    let dir = corpus_dir();
    let Ok(shapes) = std::fs::read_to_string(dir.join("shapes.json")) else {
        eprintln!("corpus not present, skipping");
        return;
    };
    let shapes: serde_json::Value = serde_json::from_str(&shapes).unwrap();
    let shapes = shapes.as_array().unwrap();

    let db = open_memory_database("corpus").unwrap();
    let conn = db.connect().unwrap();
    let catalog = Catalog::new();

    let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ok = 0usize;
    // DDL first so every later statement resolves its tables.
    let (ddl, rest): (Vec<_>, Vec<_>) = shapes.iter().partition(|s| {
        s["sql"]
            .as_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("create table")
    });
    for shape in ddl.iter().chain(rest.iter()) {
        let sql = shape["sql"].as_str().unwrap();
        let rewritten = match rewrite(sql, &catalog) {
            Ok(r) => r,
            Err(e) => {
                failures
                    .entry(format!("rewrite: {}", truncate(&e.to_string(), 90)))
                    .or_default()
                    .push(sql.to_string());
                continue;
            }
        };
        let mut failed = false;
        for stmt in rewritten {
            let is_ddl = stmt.table.is_some();
            for native in &stmt.sql {
                let result = if is_ddl {
                    conn.execute(native)
                } else {
                    conn.prepare(native).map(|_| ())
                };
                // The application has tests that run invalid SQL on purpose.
                let expected =
                    |e: &turso_core::LimboError| e.to_string().contains("no_such_column");
                if let Err(e) = result.or_else(|e| if expected(&e) { Ok(()) } else { Err(e) }) {
                    failures
                        .entry(format!("engine: {}", truncate(&e.to_string(), 90)))
                        .or_default()
                        .push(format!("{sql}\n      => {native}"));
                    failed = true;
                    break;
                }
            }
            if let Some(table) = stmt.table {
                catalog.register(table);
            }
        }
        if !failed {
            ok += 1;
        }
    }

    eprintln!("\n=== corpus: {ok}/{} statements ok ===", shapes.len());
    for (reason, sqls) in &failures {
        eprintln!("\n[{}] {reason}", sqls.len());
        eprintln!("    {}", truncate(&sqls[0].replace('\n', " "), 900));
    }
    assert!(failures.is_empty(), "{} failure classes", failures.len());
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
