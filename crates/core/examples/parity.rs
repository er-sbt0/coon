//! Load a project with both backends and say where libclang falls short of
//! clangd.
//!
//! ```text
//! cargo run --release -p core --example parity -- <project> [--flavour <file>]
//!     [--allow <file>] [--sample <n>] [--keep <dir>]
//! ```
//!
//! libclang is the default backend on the claim that it finds everything
//! clangd finds and then some; this is that claim, rerunnable. Both loads go
//! through the same bootstrap as `load` (one after the other, in this
//! process), and the graphs are compared as sets:
//!
//! - a **function** is `name @ file:line`, the file relative to the project —
//!   what a `SymbolId` is made of, so agreeing on it is agreeing on identity;
//! - a **pair** is `caller -> callee` over those, one per pair however many
//!   call sites it has, since the two backends do not report call sites alike.
//!
//! A clangd-only function whose name is the only clangd-only one by that name,
//! and matches exactly one libclang-only function, is the same function placed
//! differently (clangd puts a few at a declaration) — it is counted as
//! *relocated*, and its pairs are compared under libclang's key.
//!
//! The exit status is the regression direction only: non-zero when clangd has
//! a function or pair that libclang lacks and `--allow` does not list. What
//! libclang has on top is expected and only reported. An allowlist holds one
//! entry per line, exactly as the output prints it; `#` starts a comment.
//! `--keep` writes every list, in full, to a directory — which is where to
//! copy allowlist entries from.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use core::{Backend, LoaderOptions};
use model::CallGraph;

fn main() {
    // A libclang worker is this executable started again; it must not get as
    // far as loading anything itself.
    core::run_if_index_worker();
    std::process::exit(run());
}

#[tokio::main]
async fn run() -> i32 {
    if let Err(e) = core::logging::init_logging() {
        eprintln!("failed to initialize logging: {e}");
    }

    let usage = "usage: cargo run -p core --example parity -- <project> [--flavour <file>] \
                 [--allow <file>] [--sample <n>] [--keep <dir>]";
    let mut args = std::env::args().skip(1);
    let Some(project) = args.next() else {
        eprintln!("{usage}");
        return 2;
    };
    let mut options = LoaderOptions::default();
    let mut allow: Option<PathBuf> = None;
    let mut keep: Option<PathBuf> = None;
    let mut sample = 10usize;
    while let Some(arg) = args.next() {
        let Some(value) = args.next() else {
            eprintln!("`{arg}` needs a value\n{usage}");
            return 2;
        };
        match arg.as_str() {
            "--flavour" => options.flavour = Some(value.into()),
            "--allow" => allow = Some(value.into()),
            "--keep" => keep = Some(value.into()),
            "--sample" => match value.parse() {
                Ok(n) => sample = n,
                Err(_) => {
                    eprintln!("--sample needs a number");
                    return 2;
                }
            },
            other => {
                eprintln!("unexpected argument `{other}`\n{usage}");
                return 2;
            }
        }
    }

    let allowed = match &allow {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => parse_allowlist(&text),
            Err(e) => {
                eprintln!("could not read {}: {e}", path.display());
                return 2;
            }
        },
        None => BTreeSet::new(),
    };

    let root = std::fs::canonicalize(&project)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| project.clone());

    let mut sides = Vec::new();
    for backend in [Backend::Clangd, Backend::Libclang] {
        println!("loading with {backend:?}");
        match common::load_settled(&project, backend, options.clone()).await {
            // The app goes out of scope here, and clangd with it, before the
            // next load starts.
            Ok(app) => sides.push(Side::of(&app.call_graph, &root)),
            Err(message) => {
                eprintln!("{backend:?}: {message}");
                return 1;
            }
        }
    }
    let libclang = sides.pop().expect("two loads");
    let clangd = sides.pop().expect("two loads");

    let report = compare(&clangd, &libclang);
    let verdict = report.verdict(&allowed);

    if let Some(dir) = &keep {
        if let Err(e) = keep_lists(dir, &clangd, &libclang, &report) {
            eprintln!("could not write to {}: {e}", dir.display());
        }
    }

    print!("{}", report.render(&verdict, sample));
    if verdict.failing.is_empty() {
        0
    } else {
        1
    }
}

/// What one backend's graph says, as the keys the two are compared by.
#[derive(Debug, Default)]
struct Side {
    /// Every function, by key, with its bare name beside it for relocation.
    functions: BTreeMap<String, String>,
    /// `(caller key, callee key)`, one per pair.
    pairs: BTreeSet<(String, String)>,
}

impl Side {
    fn of(graph: &CallGraph, root: &str) -> Self {
        let key_of = |name: &str, file: &str, line: u32| {
            let file = Path::new(file)
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| file.to_string());
            format!("{name} @ {file}:{line}")
        };
        let keys: std::collections::HashMap<_, _> = graph
            .functions()
            .map(|f| {
                let at = &f.definition_location;
                (
                    f.id,
                    (
                        key_of(&f.qualified_name, &at.file_path, at.line),
                        f.qualified_name.clone(),
                    ),
                )
            })
            .collect();
        Self {
            functions: keys.values().cloned().collect(),
            pairs: graph
                .edges()
                .filter_map(|e| {
                    Some((
                        keys.get(&e.caller)?.0.clone(),
                        keys.get(&e.callee)?.0.clone(),
                    ))
                })
                .collect(),
        }
    }

    #[cfg(test)]
    fn new(functions: &[(&str, &str)], pairs: &[(&str, &str)]) -> Self {
        Self {
            functions: functions
                .iter()
                .map(|(k, n)| (k.to_string(), n.to_string()))
                .collect(),
            pairs: pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }
    }
}

/// One kind of thing compared: how many both have, and what only one has.
#[derive(Debug, Default, PartialEq)]
struct Diff {
    shared: usize,
    only_libclang: Vec<String>,
    only_clangd: Vec<String>,
}

impl Diff {
    fn of(clangd: &BTreeSet<String>, libclang: &BTreeSet<String>) -> Self {
        Self {
            shared: clangd.intersection(libclang).count(),
            only_libclang: libclang.difference(clangd).cloned().collect(),
            only_clangd: clangd.difference(libclang).cloned().collect(),
        }
    }
}

#[derive(Debug, Default, PartialEq)]
struct Report {
    functions: Diff,
    /// `(clangd's key, libclang's key)` for one function placed differently.
    relocated: Vec<(String, String)>,
    pairs: Diff,
}

/// The clangd-only entries, split by whether the allowlist explains them.
#[derive(Debug, Default, PartialEq)]
struct Verdict {
    failing: Vec<String>,
    allowed: Vec<String>,
    /// Allowlist entries that matched nothing: fixed, or misspelt.
    stale: Vec<String>,
}

fn pair_line(caller: &str, callee: &str) -> String {
    format!("{caller} -> {callee}")
}

/// `only`'s keys grouped by the bare name of the function each names.
fn by_name<'a>(side: &'a Side, only: &BTreeSet<&'a String>) -> BTreeMap<&'a str, Vec<&'a str>> {
    let mut names: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for key in only {
        names
            .entry(side.functions[*key].as_str())
            .or_default()
            .push(key);
    }
    names
}

/// Compare clangd's graph against libclang's.
fn compare(clangd: &Side, libclang: &Side) -> Report {
    let clangd_keys: BTreeSet<String> = clangd.functions.keys().cloned().collect();
    let libclang_keys: BTreeSet<String> = libclang.functions.keys().cloned().collect();

    // Same name, one candidate each side, both otherwise unmatched: the same
    // function, placed differently.
    let clangd_only: BTreeSet<&String> = clangd_keys.difference(&libclang_keys).collect();
    let libclang_only: BTreeSet<&String> = libclang_keys.difference(&clangd_keys).collect();
    let libclang_names = by_name(libclang, &libclang_only);
    let relocated: Vec<(String, String)> = by_name(clangd, &clangd_only)
        .into_iter()
        .filter_map(
            |(name, keys)| match (keys.as_slice(), libclang_names.get(name)?.as_slice()) {
                ([ours], [theirs]) => Some((ours.to_string(), theirs.to_string())),
                _ => None,
            },
        )
        .collect();
    let alias: BTreeMap<&str, &str> = relocated
        .iter()
        .map(|(c, l)| (c.as_str(), l.as_str()))
        .collect();
    let rename = |key: &str| alias.get(key).copied().unwrap_or(key).to_string();

    let functions = Diff::of(
        &clangd_keys.iter().map(|k| rename(k)).collect(),
        &libclang_keys,
    );
    let pairs = Diff::of(
        &clangd
            .pairs
            .iter()
            .map(|(a, b)| pair_line(&rename(a), &rename(b)))
            .collect(),
        &libclang
            .pairs
            .iter()
            .map(|(a, b)| pair_line(a, b))
            .collect(),
    );
    Report {
        functions,
        relocated,
        pairs,
    }
}

/// The entries of an allowlist: one per line, `#` to the end of a line is a
/// comment, blank lines ignored.
fn parse_allowlist(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

impl Report {
    fn verdict(&self, allowlist: &BTreeSet<String>) -> Verdict {
        let mut verdict = Verdict::default();
        let mut used = BTreeSet::new();
        for entry in self
            .functions
            .only_clangd
            .iter()
            .chain(&self.pairs.only_clangd)
        {
            if allowlist.contains(entry) {
                used.insert(entry.as_str());
                verdict.allowed.push(entry.clone());
            } else {
                verdict.failing.push(entry.clone());
            }
        }
        verdict.stale = allowlist
            .iter()
            .filter(|e| !used.contains(e.as_str()))
            .cloned()
            .collect();
        verdict
    }

    fn render(&self, verdict: &Verdict, sample: usize) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let row = |out: &mut String, what: &str, d: &Diff| {
            let _ = writeln!(
                out,
                "{what:<10} {:>7} shared  {:>7} only libclang  {:>7} only clangd",
                d.shared,
                d.only_libclang.len(),
                d.only_clangd.len()
            );
        };
        let _ = writeln!(out);
        row(&mut out, "functions", &self.functions);
        row(&mut out, "pairs", &self.pairs);
        let _ = writeln!(
            out,
            "relocated  {:>7} (same function, clangd places it elsewhere; counted as shared)",
            self.relocated.len()
        );

        let list = |out: &mut String, title: &str, items: Vec<String>| {
            let total = items.len();
            if total == 0 {
                return;
            }
            let _ = writeln!(out, "\n{title} ({total}):");
            for item in items.iter().take(sample) {
                let _ = writeln!(out, "  {item}");
            }
            if total > sample {
                let _ = writeln!(out, "  … {} more", total - sample);
            }
        };
        // Unexplained first, so a sample shows what failed the run.
        let flagged = |entries: &[String]| -> Vec<String> {
            let (allowed, failing): (Vec<_>, Vec<_>) =
                entries.iter().partition(|e| verdict.allowed.contains(e));
            failing
                .into_iter()
                .cloned()
                .chain(allowed.into_iter().map(|e| format!("{e}  (allowed)")))
                .collect()
        };
        list(
            &mut out,
            "only clangd, functions",
            flagged(&self.functions.only_clangd),
        );
        list(
            &mut out,
            "only clangd, pairs",
            flagged(&self.pairs.only_clangd),
        );
        list(
            &mut out,
            "only libclang, functions",
            self.functions.only_libclang.clone(),
        );
        list(
            &mut out,
            "only libclang, pairs",
            self.pairs.only_libclang.clone(),
        );
        list(
            &mut out,
            "relocated (clangd => libclang)",
            self.relocated
                .iter()
                .map(|(c, l)| format!("{c}  =>  {l}"))
                .collect(),
        );
        list(&mut out, "stale allowlist entries", verdict.stale.clone());

        let _ = writeln!(
            out,
            "\n{}: {} clangd-only entries unexplained, {} allowed",
            if verdict.failing.is_empty() {
                "PASS"
            } else {
                "FAIL"
            },
            verdict.failing.len(),
            verdict.allowed.len()
        );
        out
    }
}

/// Write every list in full under `dir`.
fn keep_lists(dir: &Path, clangd: &Side, libclang: &Side, report: &Report) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let write =
        |name: &str, lines: Vec<String>| std::fs::write(dir.join(name), lines.join("\n") + "\n");
    for (name, side) in [("clangd", clangd), ("libclang", libclang)] {
        write(
            &format!("{name}-functions.txt"),
            side.functions.keys().cloned().collect(),
        )?;
        write(
            &format!("{name}-pairs.txt"),
            side.pairs.iter().map(|(a, b)| pair_line(a, b)).collect(),
        )?;
    }
    write(
        "only-clangd-functions.txt",
        report.functions.only_clangd.clone(),
    )?;
    write("only-clangd-pairs.txt", report.pairs.only_clangd.clone())?;
    write(
        "only-libclang-functions.txt",
        report.functions.only_libclang.clone(),
    )?;
    write(
        "only-libclang-pairs.txt",
        report.pairs.only_libclang.clone(),
    )?;
    write(
        "relocated.txt",
        report
            .relocated
            .iter()
            .map(|(c, l)| format!("{c}  =>  {l}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: (&str, &str) = ("main @ main.c:3", "main");
    const PUTS: (&str, &str) = ("puts_twice @ print.c:5", "puts_twice");
    const HELPER: (&str, &str) = ("helper @ util.c:9", "helper");

    #[test]
    fn identical_graphs_have_nothing_only_on_one_side() {
        let side = || Side::new(&[MAIN, PUTS], &[(MAIN.0, PUTS.0)]);
        let report = compare(&side(), &side());
        assert_eq!(report.functions.shared, 2);
        assert_eq!(report.pairs.shared, 1);
        assert!(report.functions.only_clangd.is_empty());
        assert!(report.pairs.only_libclang.is_empty());
        assert!(report.verdict(&BTreeSet::new()).failing.is_empty());
    }

    #[test]
    fn libclang_extras_are_reported_but_pass() {
        let clangd = Side::new(&[MAIN, PUTS], &[(MAIN.0, PUTS.0)]);
        let libclang = Side::new(
            &[MAIN, PUTS, HELPER],
            &[(MAIN.0, PUTS.0), (MAIN.0, HELPER.0)],
        );
        let report = compare(&clangd, &libclang);
        assert_eq!(report.functions.only_libclang, vec![HELPER.0.to_string()]);
        assert_eq!(
            report.pairs.only_libclang,
            vec![pair_line(MAIN.0, HELPER.0)]
        );
        assert!(report.verdict(&BTreeSet::new()).failing.is_empty());
    }

    #[test]
    fn clangd_extras_fail_unless_allowed() {
        let clangd = Side::new(
            &[MAIN, PUTS, HELPER],
            &[(MAIN.0, PUTS.0), (MAIN.0, HELPER.0)],
        );
        let libclang = Side::new(&[MAIN, PUTS], &[(MAIN.0, PUTS.0)]);
        let report = compare(&clangd, &libclang);
        let pair = pair_line(MAIN.0, HELPER.0);
        assert_eq!(
            report.verdict(&BTreeSet::new()).failing,
            vec![HELPER.0.to_string(), pair.clone()]
        );

        let allow = parse_allowlist(&format!(
            "# known\n{}  # macro\n\n{pair}\nghost @ x.c:1\n",
            HELPER.0
        ));
        let verdict = report.verdict(&allow);
        assert!(verdict.failing.is_empty());
        assert_eq!(verdict.allowed.len(), 2);
        assert_eq!(verdict.stale, vec!["ghost @ x.c:1".to_string()]);
    }

    #[test]
    fn a_function_placed_elsewhere_is_relocated_and_its_pairs_still_match() {
        let decl = ("api_alloc @ include/api.h:12", "api_alloc");
        let defn = ("api_alloc @ src/api.c:40", "api_alloc");
        let clangd = Side::new(&[MAIN, decl], &[(MAIN.0, decl.0)]);
        let libclang = Side::new(&[MAIN, defn], &[(MAIN.0, defn.0)]);
        let report = compare(&clangd, &libclang);
        assert_eq!(
            report.relocated,
            vec![(decl.0.to_string(), defn.0.to_string())]
        );
        assert_eq!(report.functions.shared, 2);
        assert_eq!(report.pairs.shared, 1);
        assert!(report.verdict(&BTreeSet::new()).failing.is_empty());
    }

    #[test]
    fn an_ambiguous_name_is_not_relocated() {
        // Two statics called `init` in libclang's graph: which one clangd's
        // is cannot be told from the name, so it stays a difference.
        let clangd = Side::new(&[("init @ a.h:1", "init")], &[]);
        let libclang = Side::new(&[("init @ a.c:1", "init"), ("init @ b.c:1", "init")], &[]);
        let report = compare(&clangd, &libclang);
        assert!(report.relocated.is_empty());
        assert_eq!(
            report.functions.only_clangd,
            vec!["init @ a.h:1".to_string()]
        );
        assert_eq!(report.verdict(&BTreeSet::new()).failing.len(), 1);
    }

    #[test]
    fn call_sites_are_one_pair() {
        let mut graph = CallGraph::new();
        let node = |name: &str, line| {
            model::FunctionNode::new(
                name.into(),
                name.into(),
                model::Location::new("/p/a.c".into(), line, 0),
            )
        };
        let a = graph.add_function(node("a", 1));
        let b = graph.add_function(node("b", 5));
        for line in [2, 3] {
            graph.add_call(a, b, model::Location::new("/p/a.c".into(), line, 4));
        }
        let side = Side::of(&graph, "/p");
        assert_eq!(
            side.pairs.into_iter().collect::<Vec<_>>(),
            vec![("a @ a.c:1".to_string(), "b @ a.c:5".to_string())]
        );
    }
}
