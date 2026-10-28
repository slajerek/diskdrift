//! Encoded size and speed of the row and columnar block codecs across zstd levels,
//! on synthetic records and on a real scanned tree.
use diskdrift_bench::stores::blocks::{self, RecordBlockEncoder, columnar};
use diskdrift_bench::synth::{Gen, Params};
use diskdrift_core::record::Record;
use diskdrift_core::walk::{self, WalkOptions};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn measure(label: &str, recs: &[Record]) {
    for level in [1, 3, 6, 9, 12, 15, 19] {
        for col in [false, true] {
            if !col && level != 3 && level != 9 {
                continue;
            }
            let t = Instant::now();
            let mut blocks_out = Vec::new();
            if col {
                let mut e = columnar::Encoder::new(level);
                for r in recs {
                    if let Some(b) = e.push(r).unwrap() { blocks_out.push(b); }
                }
                if let Some(b) = e.finish().unwrap() { blocks_out.push(b); }
            } else {
                let mut e = RecordBlockEncoder::new(level);
                for r in recs {
                    if let Some(b) = e.push(r).unwrap() { blocks_out.push(b); }
                }
                if let Some(b) = e.finish().unwrap() { blocks_out.push(b); }
            }
            let enc = t.elapsed().as_secs_f64();
            let bytes: usize = blocks_out.iter().map(|b| b.data.len()).sum();
            let t = Instant::now();
            let mut n = 0;
            for b in &blocks_out {
                let raw = blocks::decompress(&b.data, b.raw_len, b.crc).unwrap();
                let mut v = Vec::new();
                if col { columnar::decode(&raw, b.count, &mut v).unwrap(); } else { blocks::decode_records(&raw, b.count, &mut v).unwrap(); }
                n += v.len();
            }
            let dec = t.elapsed().as_secs_f64();
            assert_eq!(n, recs.len());
            println!(
                "{label:<10} {:<8} zstd {level:>2}: {:>6.2} B/entry  encode {:>5.2} M/s  decode {:>5.2} M/s",
                if col { "columnar" } else { "row" },
                bytes as f64 / recs.len() as f64,
                recs.len() as f64 / enc / 1e6,
                recs.len() as f64 / dec / 1e6
            );
        }
    }
}

fn main() {
    let synth: Vec<Record> = Gen::new(Params { entries: 1_000_000, seed: 7, change: 0.01, horizon: 100 }, 0).collect();
    measure("synthetic", &synth);
    let root = std::env::args().nth(1).unwrap_or("/Applications".into());
    let mut real = Vec::new();
    let l = diskdrift_bench::jobs::lister_by_name("bulk");
    let cancel = AtomicBool::new(false);
    let mut sink = |r: &Record, _: &Path| { real.push(r.clone()); Ok(()) };
    walk::walk(Path::new(&root), l.as_ref(), &WalkOptions { threads: 8, ..Default::default() }, &mut sink, &cancel, None).unwrap();
    measure("real", &real);
}
