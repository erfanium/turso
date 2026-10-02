//! Rough per-phase timing over the captured corpus. Run with
//! `cargo test --release -p turso_mysql --test bench -- --nocapture --ignored`.
use std::time::{Duration, Instant};
use turso_mysql::catalog::Catalog;
use turso_mysql::dialect::open_memory_database;
use turso_mysql::rewrite::rewrite;

#[test]
#[ignore]
fn phases() {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../corpus");
    let Ok(shapes) = std::fs::read_to_string(dir.join("shapes.json")) else {
        return;
    };
    let shapes: serde_json::Value = serde_json::from_str(&shapes).unwrap();
    let shapes = shapes.as_array().unwrap();
    let db = open_memory_database("bench").unwrap();
    let conn = db.connect().unwrap();
    let catalog = Catalog::new();
    let t = Instant::now();
    for s in shapes.iter().filter(|s| {
        s["sql"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("create table")
    }) {
        for r in rewrite(s["sql"].as_str().unwrap(), &catalog).unwrap() {
            for n in &r.sql {
                conn.execute(n).unwrap();
            }
            if let Some(t) = r.table {
                catalog.register(t);
            }
        }
    }
    eprintln!("schema: {:?}", t.elapsed());
    let (mut rw, mut prep, mut run) = (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    let (mut weighted, mut total_n) = (Duration::ZERO, 0u64);
    let mut slow: Vec<(Duration, String)> = Vec::new();
    for s in shapes {
        let sql = s["sql"].as_str().unwrap();
        let n = s["n"].as_u64().unwrap();
        let l = sql.to_lowercase();
        if l.contains("create table")
            || l.contains("no_such")
            || !(l.starts_with("select") || l.starts_with("with"))
        {
            continue;
        }
        let t0 = Instant::now();
        let Ok(r) = rewrite(sql, &catalog) else {
            continue;
        };
        let t1 = Instant::now();
        let native = r[0].sql.last().unwrap().clone();
        let Ok(mut st) = conn.prepare(&native) else {
            continue;
        };
        let t2 = Instant::now();
        let _ = st.run_collect_rows();
        let t3 = Instant::now();
        rw += t1 - t0;
        prep += t2 - t1;
        run += t3 - t2;
        weighted += (t3 - t0) * n as u32;
        total_n += n;
        slow.push((t3 - t0, sql.chars().take(110).collect()));
    }
    eprintln!("selects: rewrite {rw:?} prepare {prep:?} run {run:?}");
    eprintln!(
        "weighted by frequency: {weighted:?} over {total_n} queries = {:?}/query",
        weighted / total_n as u32
    );
    slow.sort();
    slow.reverse();
    for (d, q) in slow.iter().take(6) {
        eprintln!("  {d:?}  {q}");
    }
}
