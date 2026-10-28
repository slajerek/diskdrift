//! Deterministic synthetic metadata trees, streamed in canonical order.
//!
//! Shape: every directory gets a budget of entries for its subtree; fan-out is
//! log-normal, budgets are split with heavy-tailed weights, depth is capped. File
//! sizes are log-normal (median ~6 kB, heavy tail), names 3-30 bytes with
//! extensions. Version `v` of the tree applies changes that are pure functions of
//! (entry id, v), so any version streams in O(1) memory per entry:
//!
//! - a "hot" set changes size (growth and shrinkage) and mtime in every version,
//! - mortal files disappear at a scheduled version,
//! - birth slots add new files at scheduled versions,
//! - a few files are renamed in place (same inode: a move),
//! - ~0.5% of files have a hardlink twin.
//!
//! Rates are calibrated so that roughly `change` of all entries differ between
//! consecutive versions.

use diskdrift_core::record::{Kind, Record, flags};

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Params {
    pub entries: u64,
    pub seed: u64,
    /// Fraction of entries changed between consecutive versions.
    pub change: f64,
    /// Versions over which deaths/births/renames are spread.
    pub horizon: u32,
}

pub fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn h2(a: u64, b: u64) -> u64 {
    mix(a ^ mix(b.wrapping_add(0x5851_F42D_4C95_7F2D)))
}

fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        mix(self.0)
    }
    fn f(&mut self) -> f64 {
        unit(self.next())
    }
    fn normal(&mut self) -> f64 {
        let (u1, u2) = (self.f().max(1e-12), self.f());
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
}

const T0: i64 = 1_760_000_000;
const EXTS: [&[u8]; 16] = [
    b".txt", b".js", b".png", b".json", b".h", b".c", b".rs", b".o", b".dylib", b".plist", b".jpg", b".py", b".md",
    b".html", b".swift", b"",
];
const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_-abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

fn name_for(id: u64, dir: bool) -> Vec<u8> {
    let mut r = Rng(id);
    let mut len = 2 + (r.next() % 6) + (r.next() % 6);
    if r.next() % 10 < 3 {
        len += r.next() % 18;
    }
    let mut n: Vec<u8> = (0..len).map(|_| ALPHA[(r.next() % ALPHA.len() as u64) as usize]).collect();
    if !dir {
        n.extend_from_slice(EXTS[(r.next() % EXTS.len() as u64) as usize]);
    }
    n
}

struct Rates {
    hot: f64,
    mortal: f64,
    births_per_child: f64,
    rename: f64,
}

impl Rates {
    fn new(p: &Params) -> Rates {
        let h = p.horizon.max(1) as f64;
        Rates {
            hot: (0.70 * p.change).min(1.0),
            mortal: (0.15 * p.change * h).min(1.0),
            births_per_child: 0.15 * p.change * h,
            rename: (0.02 * p.change * h).min(1.0),
        }
    }
}

struct Child {
    rec: Record,
    budget: u64,
    id: u64,
}

struct Frame {
    children: std::vec::IntoIter<Child>,
    depth: u16,
}

/// Streams version `version` of the tree defined by `params`.
pub struct Gen {
    p: Params,
    version: u32,
    rates: Rates,
    stack: Vec<Frame>,
    started: bool,
}

impl Gen {
    pub fn new(p: Params, version: u32) -> Gen {
        Gen { rates: Rates::new(&p), p, version, stack: Vec::new(), started: false }
    }

    fn dir_record(&self, name: Vec<u8>, id: u64) -> Record {
        Record {
            name,
            kind: Kind::Dir,
            size: 64 + (mix(id) % 2048),
            alloc: 0,
            mtime: T0 - (mix(id ^ 3) % 90_000_000) as i64,
            btime: T0 - 90_000_000 - (mix(id ^ 5) % 30_000_000) as i64,
            ino: mix(id) >> 16,
            nlink: 1,
            ..Default::default()
        }
    }

    fn file_record(&self, name: Vec<u8>, id: u64, hot: bool) -> Record {
        let mut r = Rng(id ^ 0xF11E);
        let base = (8.7 + 2.6 * r.normal()).exp().min(64e9) as u64;
        let v = self.version as u64;
        let size = if hot && v > 0 { (base as f64 * (0.5 + unit(h2(id, v)))) as u64 } else { base };
        let mtime = if hot && v > 0 { T0 + v as i64 * 3600 } else { T0 - (mix(id ^ 7) % 90_000_000) as i64 };
        Record {
            name,
            kind: Kind::File,
            size,
            alloc: size.div_ceil(4096) * 4096,
            mtime,
            btime: mtime.min(T0) - (mix(id ^ 9) % 30_000_000) as i64,
            ino: mix(id) >> 16,
            nlink: 1,
            ..Default::default()
        }
    }

    fn children(&self, dir_id: u64, budget: u64, depth: u16) -> Vec<Child> {
        let avail = budget.saturating_sub(1);
        if avail == 0 {
            return Vec::new();
        }
        let mut rng = Rng(h2(self.p.seed, dir_id));
        let fanout = ((12f64.ln() + 1.2 * rng.normal()).exp().round() as u64).clamp(1, 4000);
        let mut k = fanout.min(avail);
        if depth >= 16 {
            k = avail.min(20_000);
        }
        let nd = if avail > k { ((k as f64 * 0.18).round() as u64).clamp(1, k) } else { 0 };
        let rest = avail - k;
        let weights: Vec<f64> = (0..nd).map(|_| rng.f().max(1e-6).powf(-1.2)).collect();
        let wsum: f64 = weights.iter().sum();
        let mut budgets: Vec<u64> = weights.iter().map(|w| 1 + (rest as f64 * w / wsum) as u64).collect();
        let given: u64 = budgets.iter().sum::<u64>() - nd;
        if let Some(b) = budgets.first_mut() {
            *b += rest - given.min(rest);
        }

        let v = self.version;
        let r = &self.rates;
        let mut out: Vec<Child> = Vec::with_capacity(k as usize + 4);
        for i in 0..k {
            let id = h2(dir_id, i);
            if i < nd {
                let rec = self.dir_record(name_for(id, true), id);
                out.push(Child { rec, budget: budgets[i as usize], id });
                continue;
            }
            let u = |salt: u64| unit(h2(id, salt));
            if u(1) < r.mortal {
                let death = 1 + (h2(id, 2) % self.p.horizon.max(1) as u64) as u32;
                if v >= death {
                    continue;
                }
            }
            let mut name = name_for(id, false);
            if u(3) < r.rename {
                let at = 1 + (h2(id, 4) % self.p.horizon.max(1) as u64) as u32;
                if v >= at {
                    name.extend_from_slice(b".moved");
                }
            }
            let hot = u(5) < r.hot;
            let rec = self.file_record(name.clone(), id, hot);
            if u(6) < 0.005 {
                let mut owner = rec.clone();
                owner.nlink = 2;
                let mut twin = owner.clone();
                twin.name.extend_from_slice(b".lnk");
                twin.flags = flags::HARDLINK_DUP;
                out.push(Child { rec: owner, budget: 0, id });
                out.push(Child { rec: twin, budget: 0, id });
            } else {
                out.push(Child { rec, budget: 0, id });
            }
        }
        // Birth slots.
        let expected = k as f64 * r.births_per_child;
        let slots = expected.floor() as u64 + u64::from(rng.f() < expected.fract());
        for s in 0..slots {
            let id = h2(dir_id, (1 << 40) + s);
            let born = 1 + (h2(id, 8) % self.p.horizon.max(1) as u64) as u32;
            if v >= born {
                let mut name = name_for(id, false);
                name.extend_from_slice(b".new");
                out.push(Child { rec: self.file_record(name, id, false), budget: 0, id });
            }
        }
        out.sort_by(|a, b| a.rec.name.cmp(&b.rec.name));
        // Unique names (rare collisions of random names).
        for i in 1..out.len() {
            if out[i].rec.name == out[i - 1].rec.name {
                let suffix = format!("~{i}");
                out[i].rec.name.extend_from_slice(suffix.as_bytes());
            }
        }
        out.sort_by(|a, b| a.rec.name.cmp(&b.rec.name));
        out
    }
}

impl Iterator for Gen {
    type Item = Record;

    fn next(&mut self) -> Option<Record> {
        if !self.started {
            self.started = true;
            let root = self.dir_record(b"/synthetic".to_vec(), 0);
            let children = self.children(0, self.p.entries, 1);
            self.stack.push(Frame { children: children.into_iter(), depth: 0 });
            return Some(root);
        }
        loop {
            let frame = self.stack.last_mut()?;
            let depth = frame.depth + 1;
            match frame.children.next() {
                None => {
                    self.stack.pop();
                }
                Some(c) => {
                    let mut rec = c.rec;
                    rec.depth = depth;
                    if rec.kind == Kind::Dir {
                        let children = self.children(c.id, c.budget, depth + 1);
                        self.stack.push(Frame { children: children.into_iter(), depth });
                    }
                    return Some(rec);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diskdrift_core::paths::{PathStack, compare_paths};

    #[test]
    fn deterministic_canonical_and_sized() {
        let p = Params { entries: 50_000, seed: 1, change: 0.01, horizon: 100 };
        let a: Vec<Record> = Gen::new(p, 0).collect();
        let b: Vec<Record> = Gen::new(p, 0).collect();
        assert_eq!(a, b);
        assert!((a.len() as f64 - 50_000.0).abs() < 2_500.0, "got {}", a.len());
        let mut stack = PathStack::new();
        let mut prev: Option<Vec<Vec<u8>>> = None;
        for r in &a {
            stack.set(r.depth, &r.name).unwrap();
            let cur = stack.components().to_vec();
            if let Some(p) = &prev {
                assert_eq!(compare_paths(p, &cur), std::cmp::Ordering::Less);
            }
            prev = Some(cur);
        }
    }

    #[test]
    fn change_rate_is_roughly_calibrated() {
        for change in [0.01, 0.1, 0.5] {
            let p = Params { entries: 100_000, seed: 2, change, horizon: 1 };
            let a = Box::new(Gen::new(p, 0).map(Ok));
            let b = Box::new(Gen::new(p, 1).map(Ok));
            let ops = diskdrift_core::delta::compute(a, b).unwrap();
            let ratio = ops.len() as f64 / 100_000.0;
            assert!(ratio > change * 0.5 && ratio < change * 1.5, "change {change}: ratio {ratio}");
        }
    }
}
