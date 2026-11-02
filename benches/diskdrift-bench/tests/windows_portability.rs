use diskdrift_bench::report;
use diskdrift_bench::runner::{Machine, Results, Row};

fn results(rows: Vec<Row>) -> Results {
    Results {
        suite: "full".into(),
        started: "start".into(),
        finished: "finish".into(),
        machine: Machine {
            model: "test".into(),
            cpu: "test".into(),
            cpus: 1,
            memory_bytes: 0,
            os: "macOS test".into(),
            filesystem: "apfs".into(),
            device: "test".into(),
            rustc: "test".into(),
            cache_note: "test".into(),
        },
        rows,
    }
}

fn row(backend: &str, param: &str, ms: f64, rss: u64, footprint: u64) -> Row {
    Row {
        group: "production-store".into(),
        backend: backend.into(),
        n: 1_000_000,
        param: param.into(),
        trial: 0,
        ms,
        wall_ms: ms,
        rss,
        items: 1_000_000,
        footprint,
        ok: true,
        error: None,
        extra: serde_json::Value::Null,
    }
}

#[test]
fn report_records_a_passing_dds2_production_gate() {
    let report = report::render(&results(vec![
        row("dds1", "import", 100.0, 100_000_000, 30_000_000),
        row("dds1", "read", 80.0, 100_000_000, 0),
        row("dds2", "import", 115.0, 110_000_000, 33_000_000),
        row("dds2", "read", 92.0, 110_000_000, 0),
    ]));

    assert!(report.contains("DDS2 production gate: PASS"), "{report}");
}

#[test]
fn report_fails_the_production_gate_when_a_required_row_is_missing() {
    let report = report::render(&results(vec![
        row("dds1", "import", 100.0, 100_000_000, 30_000_000),
        row("dds1", "read", 80.0, 100_000_000, 0),
        row("dds2", "import", 110.0, 105_000_000, 31_000_000),
    ]));

    assert!(report.contains("DDS2 production gate: FAIL"), "{report}");
    assert!(
        report.contains("missing required production row: dds2 read"),
        "{report}"
    );
}

#[test]
fn report_fails_the_production_gate_when_a_limit_is_exceeded() {
    let report = report::render(&results(vec![
        row("dds1", "import", 100.0, 100_000_000, 30_000_000),
        row("dds1", "read", 80.0, 100_000_000, 0),
        row("dds2", "import", 116.0, 110_000_000, 33_000_000),
        row("dds2", "read", 90.0, 110_000_000, 0),
    ]));

    assert!(report.contains("DDS2 production gate: FAIL"), "{report}");
    assert!(report.contains("| time/import | 100.00 | 116.00 | 115.00 | FAIL |"));
}
