//! The indexer across worker processes. Needs libclang at runtime.
//!
//! A worker is the running executable started again, so this test binary is
//! its own worker: `main` serves when started as one and runs the tests
//! otherwise, which is why it has no harness. Started with `--test-worker
//! <crash-on|hang-on> <file>`, it is a worker that crashes or hangs on that
//! file, for the tests of what the parent does about it.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::Path;
use std::time::{Duration, Instant};

use clang_index::{index, index_with, Cancel, Job, Options, Report, WorkerCommand};
use compdb::CompileCommand;

fn main() {
    clang_index::worker::run_if_worker();
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.get(1).is_some_and(|a| a == "--test-worker") {
        let mode = args[2].to_string_lossy().into_owned();
        let target = args[3].to_string_lossy().into_owned();
        let root = Path::new(&args[4]);
        let result = clang_index::worker::serve(root, |file| {
            let hit = target == "*" || file.file_name().is_some_and(|n| *n == *target);
            match mode.as_str() {
                "crash-on" if hit => std::process::abort(),
                "hang-on" if hit => std::thread::sleep(Duration::from_secs(3600)),
                _ => {}
            }
        });
        std::process::exit(i32::from(result.is_err()));
    }

    let tests: &[(&str, fn())] = &[
        (
            "any_worker_count_gives_the_in_process_graph",
            any_worker_count_gives_the_in_process_graph,
        ),
        (
            "a_crashed_worker_costs_one_tu",
            a_crashed_worker_costs_one_tu,
        ),
        ("a_hung_parse_times_out", a_hung_parse_times_out),
        ("cancelling_kills_the_workers", cancelling_kills_the_workers),
        (
            "a_binary_that_is_not_a_worker_parses_in_process",
            a_binary_that_is_not_a_worker_parses_in_process,
        ),
    ];
    let filter = std::env::args().skip(1).find(|a| !a.starts_with('-'));
    let mut ran = 0;
    for (name, test) in tests {
        if filter.as_deref().is_some_and(|f| !name.contains(f)) {
            continue;
        }
        let started = Instant::now();
        test();
        ran += 1;
        println!("test {name} ... ok ({:.1?})", started.elapsed());
    }
    println!("test result: ok. {ran} passed");
}

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

fn job_with(dir: &Path, file: &str, flags: &[&str]) -> Job {
    let mut arguments: Vec<String> = flags.iter().map(|s| s.to_string()).collect();
    arguments.extend(["-c", file, "-o", "out.o"].map(String::from));
    Job::from_entry(&CompileCommand {
        directory: dir.display().to_string(),
        command: None,
        file: file.into(),
        arguments: Some(arguments),
        output: None,
    })
}

/// A project of `n` TUs over two flavours, every one including a header whose
/// inline body depends on the flavour, and every one passing a gcc flag clang
/// refuses — enough for the order a body is walked in, the order a node's
/// definition is taken in, and the arguments learned on the way to show up as
/// differences between two runs if any of them depended on timing.
fn project(root: &Path, n: usize) -> Vec<Job> {
    std::fs::create_dir_all(root.join("include")).unwrap();
    write(
        root,
        "include/pick.h",
        "void call_a(void);\n\
         void call_b(void);\n\
         static inline void pick(void) {\n\
         #ifdef FLAV_A\n\
             call_a();\n\
         #else\n\
             call_b();\n\
         #endif\n\
         }\n\
         void shared(int);\n",
    );
    write(
        root,
        "defs.c",
        "void call_a(void) {}\nvoid call_b(void) {}\n",
    );
    let mut jobs = vec![job_with(root, "defs.c", &["cc", "-Iinclude"])];
    for i in 0..n {
        let name = format!("c{i}.c");
        // Every TU defines `shared`'s callers; the odd ones also define a
        // static of the same name as the even ones, so which TU a node comes
        // from is decided by order.
        write(
            root,
            &name,
            &format!(
                "#include \"pick.h\"\n\
                 static void local(void) {{ pick(); }}\n\
                 void f{i}(void) {{ local(); shared({i}); }}\n\
                 {}\n",
                if i == 0 {
                    "void shared(int x) { (void)x; pick(); }"
                } else {
                    ""
                }
            ),
        );
        let flags: &[&str] = if i % 2 == 0 {
            &["cc", "-Iinclude", "-DFLAV_A", "-fconserve-stack"]
        } else {
            &["cc", "-Iinclude", "-fconserve-stack"]
        };
        jobs.push(job_with(root, &name, flags));
    }
    jobs
}

/// Everything a report says, in an order that does not depend on how its maps
/// iterate: the summary, every function with where it is defined, and every
/// call site.
fn fingerprint(report: &Report) -> (Vec<usize>, Vec<String>, BTreeSet<String>, BTreeSet<String>) {
    let graph = &report.graph;
    let functions = graph
        .functions()
        .map(|f| {
            format!(
                "{} @ {}:{}:{}",
                f.qualified_name,
                f.definition_location.file_path,
                f.definition_location.line,
                f.definition_location.column
            )
        })
        .collect();
    let name = |id| graph.get_function(id).unwrap().qualified_name.clone();
    let sites = graph
        .edges()
        .map(|e| {
            format!(
                "{} -> {} @ {}:{}:{}",
                name(&e.caller),
                name(&e.callee),
                e.call_location.file_path,
                e.call_location.line,
                e.call_location.column
            )
        })
        .collect();
    (
        vec![
            report.parsed,
            report.failed,
            report.errors,
            report.tus_with_errors,
            report.external_calls,
        ],
        report.rejected_arguments.clone(),
        functions,
        sites,
    )
}

fn options(workers: usize) -> Options {
    Options {
        workers,
        ..Options::default()
    }
}

/// This binary as a worker that misbehaves on `file`.
fn misbehaving(workers: usize, mode: &str, file: &str) -> Options {
    Options {
        workers,
        worker: WorkerCommand {
            program: std::env::current_exe().unwrap(),
            args: vec!["--test-worker".into(), mode.into(), file.into()],
        },
        ..Options::default()
    }
}

/// Live children of this process, read off `/proc`.
#[cfg(target_os = "linux")]
fn children() -> usize {
    let mut count = 0;
    for task in std::fs::read_dir("/proc/self/task").unwrap() {
        let path = task.unwrap().path().join("children");
        count += std::fs::read_to_string(path)
            .unwrap_or_default()
            .split_whitespace()
            .count();
    }
    count
}

fn any_worker_count_gives_the_in_process_graph() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = project(root, 12);
    let serial = index(root, &jobs, |_, _| {}).unwrap();
    assert_eq!(serial.parsed, 13);
    assert_eq!(
        serial.rejected_arguments,
        vec!["-fconserve-stack".to_string()]
    );
    let expected = fingerprint(&serial);
    for workers in [1, 3, 8] {
        let mut progress = Vec::new();
        let report = index_with(
            root,
            &jobs,
            &options(workers),
            &Cancel::default(),
            |done, total| progress.push((done, total)),
        )
        .unwrap();
        assert_eq!(fingerprint(&report), expected, "{workers} worker(s)");
        // The graph is built the same way, so it is the same graph down to the
        // order its edges went in.
        let order = |r: &Report| {
            r.graph
                .edges()
                .map(|e| (e.caller, e.callee, e.call_location.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&report), order(&serial), "{workers} worker(s)");
        assert_eq!(progress, (1..=13).map(|d| (d, 13)).collect::<Vec<_>>());
    }
}

fn a_crashed_worker_costs_one_tu() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = project(root, 6);
    let report = index_with(
        root,
        &jobs,
        &misbehaving(2, "crash-on", "c2.c"),
        &Cancel::default(),
        |_, _| {},
    )
    .unwrap();
    assert_eq!((report.parsed, report.failed), (6, 1));
    let names: BTreeSet<String> = report
        .graph
        .functions()
        .map(|f| f.qualified_name.clone())
        .collect();
    // Every TU but the one it died on is in, those after it included.
    for i in [0, 1, 3, 4, 5] {
        assert!(names.contains(&format!("f{i}")), "f{i} in {names:?}");
    }
    assert!(!names.contains("f2"));
}

fn a_hung_parse_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = project(root, 4);
    let mut options = misbehaving(2, "hang-on", "c1.c");
    options.timeout = Duration::from_millis(500);
    let started = Instant::now();
    let report = index_with(root, &jobs, &options, &Cancel::default(), |_, _| {}).unwrap();
    assert_eq!((report.parsed, report.failed), (4, 1));
    assert!(started.elapsed() < Duration::from_secs(10));
    #[cfg(target_os = "linux")]
    assert_eq!(children(), 0, "a worker outlived the run");
}

fn cancelling_kills_the_workers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = project(root, 4);
    let cancel = Cancel::default();
    let canceller = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            cancel.cancel();
        })
    };
    let started = Instant::now();
    let result = index_with(
        root,
        &jobs,
        &misbehaving(3, "hang-on", "*"),
        &cancel,
        |_, _| {},
    );
    canceller.join().unwrap();
    assert!(result.is_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    #[cfg(target_os = "linux")]
    assert_eq!(children(), 0, "a worker outlived the run");
}

fn a_binary_that_is_not_a_worker_parses_in_process() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = project(root, 3);
    let options = Options {
        workers: 2,
        worker: WorkerCommand {
            program: "true".into(),
            args: Vec::new(),
        },
        ..Options::default()
    };
    let report = index_with(root, &jobs, &options, &Cancel::default(), |_, _| {}).unwrap();
    assert_eq!((report.parsed, report.failed), (4, 0));
}
