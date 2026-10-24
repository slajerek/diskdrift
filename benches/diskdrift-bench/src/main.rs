//! `diskdrift-bench`: M1 benchmark driver.
//!
//!   diskdrift-bench run --suite quick|full [--only scan,ops,change,history,robustness] --out results.json
//!   diskdrift-bench report results.json --out report.md
//!
//! All scratch data lives under `<workspace>/target/bench-work` and is removed at the end.

use clap::{Parser, Subcommand};
use diskdrift_bench::jobs;
use diskdrift_bench::report;
use diskdrift_bench::runner::{self, Results, Runner, Suite};
use std::path::PathBuf;

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Run {
        #[arg(long, default_value = "quick")]
        suite: String,
        /// Comma-separated subset: scan, ops, change, history, robustness.
        #[arg(long, value_delimiter = ',')]
        only: Vec<String>,
        #[arg(long)]
        out: PathBuf,
        /// Merge into an existing results file instead of overwriting it.
        #[arg(long)]
        append: bool,
        /// Only these history strategies (labels as in the report, substring match).
        #[arg(long)]
        strategy: Option<String>,
        /// Only these backends for history runs.
        #[arg(long, value_delimiter = ',')]
        backend: Vec<diskdrift_bench::stores::Backend>,
    },
    Report {
        results: PathBuf,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Internal: run one job and print its JSON result.
    Child { job: String },
}

fn work_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bench-work")
}

fn main() {
    match Cli::parse().cmd {
        Cmd::Child { job } => {
            let job: jobs::Job = serde_json::from_str(&job).expect("valid job");
            let r = jobs::execute(job);
            println!("{}", serde_json::to_string(&r).unwrap());
        }
        Cmd::Run { suite, only, out, append, strategy, backend } => {
            let mut s = match suite.as_str() {
                "full" => Suite::full(),
                _ => Suite::quick(),
            };
            if let Some(f) = &strategy {
                s.history_strategies.retain(|x| x.label().contains(f.as_str()));
            }
            s.history_backends = backend;
            let work = work_dir();
            std::fs::create_dir_all(&work).unwrap();
            let work = work.canonicalize().unwrap();
            let mut prior = if append {
                std::fs::read_to_string(&out).ok().and_then(|t| serde_json::from_str::<Results>(&t).ok())
            } else {
                None
            };
            let started = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let machine = runner::machine(&work);
            let mut r = Runner { work, rows: Vec::new(), log: true };
            r.run_suite(&s, &only);
            let finished = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let results = match prior.take() {
                Some(mut p) => {
                    let keys: std::collections::HashSet<(String, String, String)> =
                        r.rows.iter().map(|x| (x.group.clone(), x.backend.clone(), x.param.clone())).collect();
                    p.rows.retain(|x| !keys.contains(&(x.group.clone(), x.backend.clone(), x.param.clone())));
                    p.rows.extend(r.rows);
                    p.finished = finished;
                    p
                }
                None => Results { suite: s.name.clone(), started, finished, machine, rows: r.rows },
            };
            std::fs::write(&out, serde_json::to_string_pretty(&results).unwrap()).unwrap();
            eprintln!("wrote {}", out.display());
        }
        Cmd::Report { results, out } => {
            let res: Results = serde_json::from_str(&std::fs::read_to_string(&results).unwrap()).unwrap();
            let md = report::render(&res);
            match out {
                Some(p) => std::fs::write(p, md).unwrap(),
                None => print!("{md}"),
            }
        }
    }
}
