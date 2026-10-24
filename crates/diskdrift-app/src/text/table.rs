//! Minimal aligned text tables.

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

pub struct Table {
    headers: Vec<String>,
    align: Vec<Align>,
    rows: Vec<Vec<String>>,
    indent: usize,
}

impl Table {
    pub fn new(cols: &[(&str, Align)]) -> Table {
        Table {
            headers: cols.iter().map(|(h, _)| h.to_string()).collect(),
            align: cols.iter().map(|(_, a)| *a).collect(),
            rows: Vec::new(),
            indent: 2,
        }
    }

    pub fn row<I: IntoIterator<Item = String>>(&mut self, cells: I) {
        self.rows.push(cells.into_iter().collect());
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn render(&self) -> String {
        let n = self.headers.len();
        let mut width: Vec<usize> = self.headers.iter().map(|h| h.chars().count()).collect();
        for r in &self.rows {
            for (i, c) in r.iter().enumerate().take(n) {
                width[i] = width[i].max(c.chars().count());
            }
        }
        let mut out = String::new();
        let line = |cells: &[String], out: &mut String| {
            out.push_str(&" ".repeat(self.indent));
            for (i, w) in width.iter().enumerate() {
                let c = cells.get(i).map(String::as_str).unwrap_or("");
                let pad = w.saturating_sub(c.chars().count());
                let last = i + 1 == n;
                match self.align[i] {
                    Align::Left if last => out.push_str(c),
                    Align::Left => {
                        out.push_str(c);
                        out.push_str(&" ".repeat(pad));
                    }
                    Align::Right => {
                        out.push_str(&" ".repeat(pad));
                        out.push_str(c);
                    }
                }
                if !last {
                    out.push_str("  ");
                }
            }
            out.push('\n');
        };
        line(&self.headers, &mut out);
        for r in &self.rows {
            line(r, &mut out);
        }
        out
    }
}

/// Shortens `s` to `max` characters, keeping the end (the most specific part of a path).
pub fn ellipsize_start(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max || max < 2 {
        return s.to_string();
    }
    let tail: String = s.chars().skip(count - (max - 1)).collect();
    format!("…{tail}")
}
