//! The indexer against a project written out on the spot. Needs libclang at
//! runtime, as the `lsp` tests need clangd.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use clang_index::{index, index_with, Cancel, Job, Options, Report};
use compdb::CompileCommand;
use model::{BodilessCallees, CallGraph};

/// libclang allows one `Clang` per process at a time, and the test harness runs
/// tests on parallel threads.
static LIBCLANG: Mutex<()> = Mutex::new(());

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

fn job(dir: &Path, file: &str) -> Job {
    job_with(dir, file, &["cc", "-Iinclude", "-MMD"])
}

/// A job compiling `file` from `dir` with `flags` (the compiler first), as a
/// database entry spells it.
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

/// Every call site as `caller -> callee @ file:line`, the file being where the
/// call is written.
fn sites(graph: &CallGraph) -> BTreeSet<String> {
    let name = |id| graph.get_function(id).unwrap().qualified_name.clone();
    graph
        .edges()
        .map(|e| {
            let file = Path::new(&e.call_location.file_path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            format!(
                "{} -> {} @ {file}:{}",
                name(&e.caller),
                name(&e.callee),
                e.call_location.line
            )
        })
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

/// Every edge as `caller -> callee`, with the file the caller is defined in so
/// two `static` functions of the same name can be told apart.
fn pairs(graph: &CallGraph) -> BTreeSet<String> {
    let label = |id| {
        let f = graph.get_function(id).unwrap();
        let file = Path::new(&f.definition_location.file_path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        format!("{}({file})", f.qualified_name)
    };
    graph
        .edges()
        .map(|e| format!("{} -> {}", label(&e.caller), label(&e.callee)))
        .collect()
}

#[test]
fn callers_come_from_every_translation_unit() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Reached through a relative `-I`, which libclang reports relative to the
    // compile command's directory.
    std::fs::create_dir(root.join("include")).unwrap();
    write(
        root,
        "include/util.h",
        "#include <string.h>\n\
         void shared(void);\n\
         static inline int twice(int x) { return x + x; }\n\
         typedef void (*cb)(void);\n\
         void subscribe(cb f);\n",
    );
    write(
        root,
        "a.c",
        "#include \"util.h\"\n\
         static void helper(void) { shared(); }\n\
         void on_event(void) {}\n\
         int main(void) { helper(); subscribe(on_event); return twice((int)strlen(\"x\")); }\n",
    );
    write(
        root,
        "b.c",
        "#include \"util.h\"\n\
         static void helper(void) {}\n\
         void shared(void) { helper(); (void)twice(2); }\n\
         void subscribe(cb f) { f(); }\n",
    );

    let jobs = vec![job(root, "a.c"), job(root, "b.c")];
    let report = index(root, &jobs, |_, _| {}).unwrap();

    assert_eq!(report.parsed, 2);
    assert_eq!(report.failed, 0);
    assert_eq!(report.errors, 0);
    let expected: BTreeSet<String> = [
        // A call into another TU resolves to the definition there.
        "helper(a.c) -> shared(b.c)",
        // A function handed over as a callback is an edge, as clangd reports it.
        "main(a.c) -> on_event(a.c)",
        "main(a.c) -> subscribe(b.c)",
        // Each `static helper` is its own function.
        "main(a.c) -> helper(a.c)",
        "shared(b.c) -> helper(b.c)",
        // A header's inline function is one node, however many TUs define it.
        "main(a.c) -> twice(util.h)",
        "shared(b.c) -> twice(util.h)",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    assert_eq!(pairs(&report.graph), expected);
    // `strlen` is libc: the call is counted, not drawn.
    assert_eq!(report.external_calls, 1);
    // main, on_event, helper ×2, twice, shared, subscribe.
    assert_eq!(report.graph.node_count(), 7);
}

/// Two `static` functions share a name, and the name index holds one of them.
/// Which one must not depend on the order the TUs were parsed in, nor on the
/// order the graph's nodes happen to be walked in when it is merged into the
/// app's — the same function every time, the one defined first by path.
#[test]
fn a_shared_name_resolves_to_the_same_function_in_any_parse_order() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "a.c",
        "static void helper(void) {}\nvoid a(void) { helper(); }\n",
    );
    write(
        root,
        "b.c",
        "static void helper(void) {}\nvoid b(void) { helper(); }\n",
    );

    let resolved = |jobs: &[Job]| {
        let graph = index(root, jobs, |_, _| {}).unwrap().graph;
        let helper = graph.find_function_by_name("helper").unwrap();
        let file = Path::new(&helper.definition_location.file_path)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        // Merged node by node, as `CoreApp` does with a graph from this
        // backend, which walks the nodes in hash order.
        let mut merged = CallGraph::new();
        for function in graph.functions() {
            merged.add_function(function.clone());
        }
        assert_eq!(
            merged.find_function_by_name("helper").unwrap().id,
            helper.id
        );
        file
    };
    for _ in 0..3 {
        assert_eq!(resolved(&[job(root, "a.c"), job(root, "b.c")]), "a.c");
        assert_eq!(resolved(&[job(root, "b.c"), job(root, "a.c")]), "a.c");
    }
}

#[test]
fn the_dependency_file_flag_writes_nothing() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "x.c", "void f(void) {}\n");
    index(dir.path(), &[job(dir.path(), "x.c")], |_, _| {}).unwrap();
    assert!(!dir.path().join("x.d").exists());
    assert!(!dir.path().join("out.d").exists());
}

/// A build laid out the way a real one is: sources under `src/`, headers under
/// `include/`, a vendored library beside the project rather than in it, and the
/// compiler run from `build/` so every path in the database goes through `..`.
#[test]
fn a_build_directory_project_resolves_across_files_in_any_order() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let outer = tempfile::tempdir().unwrap();
    let root = outer.path().join("proj");
    for d in ["proj/src", "proj/include", "proj/build", "vendor"] {
        std::fs::create_dir_all(outer.path().join(d)).unwrap();
    }
    // Outside the root: neither its inline function nor anything it declares
    // is project code, even though every TU includes it.
    write(
        outer.path(),
        "vendor/vendor.h",
        "static inline int vendor_hash(int x) { return x * 31; }\n\
         void vendor_log(const char *m);\n",
    );
    write(
        &root,
        "include/store.h",
        "void store_put(int k);\n\
         int store_get(int k);\n",
    );
    write(
        &root,
        "src/store.c",
        "#include \"store.h\"\n\
         #include \"vendor.h\"\n\
         static int slots[64];\n\
         static int slot(int k) { return vendor_hash(k) & 63; }\n\
         void store_put(int k) { slots[slot(k)] = k; vendor_log(\"put\"); }\n\
         int store_get(int k) { return slots[slot(k)]; }\n",
    );
    write(
        &root,
        "src/cache.c",
        "#include \"store.h\"\n\
         int cache_lookup(int k) {\n\
             int v = store_get(k);\n\
             if (!v) store_put(k);\n\
             return v;\n\
         }\n",
    );
    write(
        &root,
        "src/main.c",
        "#include \"store.h\"\n\
         int cache_lookup(int k);\n\
         int main(void) {\n\
             store_put(1);\n\
             return cache_lookup(1);\n\
         }\n",
    );

    let build = root.join("build");
    let flags = ["cc", "-I../include", "-I../../vendor", "-MMD"];
    let jobs: Vec<Job> = ["../src/main.c", "../src/cache.c", "../src/store.c"]
        .iter()
        .map(|f| job_with(&build, f, &flags))
        .collect();
    let forward = index(&root, &jobs, |_, _| {}).unwrap();
    let reversed: Vec<Job> = jobs.iter().rev().cloned().collect();
    let backward = index(&root, &reversed, |_, _| {}).unwrap();

    assert_eq!(forward.parsed, 3);
    assert_eq!(forward.errors, 0);
    // A call site is filed under the file it is written in — the caller's —
    // not the callee's, at the line it is on.
    let expected = set(&[
        "main -> store_put @ main.c:4",
        "main -> cache_lookup @ main.c:5",
        "cache_lookup -> store_get @ cache.c:3",
        "cache_lookup -> store_put @ cache.c:4",
        "store_put -> slot @ store.c:5",
        "store_get -> slot @ store.c:6",
    ]);
    assert_eq!(sites(&forward.graph), expected);
    // `vendor_hash` and `vendor_log`: counted, not drawn.
    assert_eq!(forward.external_calls, 2);
    assert_eq!(forward.graph.node_count(), 5);
    // A callee's callers are complete whichever TU was read first, and every
    // definition is filed where it is written.
    assert_eq!(sites(&backward.graph), expected);
    assert_eq!(pairs(&backward.graph), pairs(&forward.graph));
    let store_put = forward.graph.find_function_by_name("store_put").unwrap();
    assert!(store_put
        .definition_location
        .file_path
        .ends_with("proj/src/store.c"));
    let mut callers: Vec<_> = forward
        .graph
        .get_callers(&store_put.id)
        .iter()
        .map(|f| f.name.clone())
        .collect();
    callers.sort();
    assert_eq!(callers, ["cache_lookup", "main"]);
}

/// C++ across translation units: a class declared in one header and defined
/// in two files, overloads defined apart, a namespace, a constructor, a
/// virtual call, and a template that lives in a header.
#[test]
fn cxx_members_overloads_and_templates_resolve_across_files() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("include")).unwrap();
    write(
        root,
        "include/shape.h",
        "namespace geo {\n\
         void report(int n);\n\
         void report(const char *s);\n\
         struct Shape {\n\
             explicit Shape(int sides);\n\
             virtual ~Shape();\n\
             virtual int area() const;\n\
             int sides() const;\n\
             int n;\n\
         };\n\
         template <typename T> T clamp(T v) { report(\"clamp\"); return v < 0 ? 0 : v; }\n\
         }\n",
    );
    // The class's members are split over two files, as the overloads are.
    write(
        root,
        "shape.cpp",
        "#include \"shape.h\"\n\
         namespace geo {\n\
         Shape::Shape(int s) : n(s) { report(s); }\n\
         Shape::~Shape() {}\n\
         int Shape::area() const { return clamp(sides()); }\n\
         void report(int) {}\n\
         }\n",
    );
    write(
        root,
        "sides.cpp",
        "#include \"shape.h\"\n\
         namespace geo {\n\
         int Shape::sides() const { return n; }\n\
         void report(const char *) {}\n\
         }\n",
    );
    write(
        root,
        "main.cpp",
        "#include \"shape.h\"\n\
         int main() {\n\
             geo::Shape s(4);\n\
             const geo::Shape &r = s;\n\
             return r.area() + geo::clamp(2.5) + s.sides();\n\
         }\n",
    );

    let flags = ["c++", "-std=c++17", "-Iinclude", "-MMD"];
    let jobs: Vec<Job> = ["main.cpp", "shape.cpp", "sides.cpp"]
        .iter()
        .map(|f| job_with(root, f, &flags))
        .collect();
    let report = index(root, &jobs, |_, _| {}).unwrap();

    assert_eq!(report.parsed, 3);
    assert_eq!(report.errors, 0);
    // Each overload of `report` is its own node, in the file that defines it,
    // and each call goes to the one its argument picks.
    assert_eq!(
        pairs(&report.graph),
        set(&[
            "main(main.cpp) -> geo::Shape::Shape(shape.cpp)",
            "main(main.cpp) -> geo::Shape::area(shape.cpp)",
            "main(main.cpp) -> geo::Shape::sides(sides.cpp)",
            // `clamp<double>` and `clamp<int>` are both the header's template.
            "main(main.cpp) -> geo::clamp(shape.h)",
            "geo::Shape::area(shape.cpp) -> geo::clamp(shape.h)",
            "geo::Shape::area(shape.cpp) -> geo::Shape::sides(sides.cpp)",
            "geo::Shape::Shape(shape.cpp) -> geo::report(shape.cpp)",
            "geo::clamp(shape.h) -> geo::report(sides.cpp)",
        ])
    );
    // `s` going out of scope calls the destructor, but nothing in the source
    // spells that call, so there is no reference to record — and clangd's
    // index records none either. It is a node with no callers.
    let dtor = report.graph.find_function_by_name("~Shape").unwrap();
    assert!(report.graph.get_callers(&dtor.id).is_empty());
    // main, the constructor, destructor, two members, two overloads, clamp.
    assert_eq!(report.graph.node_count(), 8);
}

/// `g++ a.c` compiles C++, where libclang on its own would read the file as C
/// and lose the function a `class` is in to a syntax error.
#[test]
fn a_c_file_built_by_a_cxx_driver_is_parsed_as_cxx() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "a.c",
        "class Counter { public: int next() { return ++n; } int n = 0; };\n\
         int main(void) { Counter c; return c.next(); }\n",
    );
    let report = index(
        root,
        &[job_with(root, "a.c", &["ccache", "g++"])],
        |_, _| {},
    )
    .unwrap();
    assert_eq!((report.parsed, report.failed, report.errors), (1, 0, 0));
    assert_eq!(
        pairs(&report.graph),
        set(&["main(a.c) -> Counter::next(a.c)"])
    );
}

/// A cross compiler's name is its target: the code under `#ifdef __aarch64__`
/// is the code the build compiled.
#[test]
fn a_cross_compilers_name_sets_the_target() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root,
        "a.c",
        "#ifdef __aarch64__\n\
         static void arm(void) {}\n\
         void entry(void) { arm(); }\n\
         #else\n\
         #error built for the host\n\
         #endif\n",
    );
    let report = index(
        root,
        &[job_with(
            root,
            "a.c",
            &["/opt/cross/bin/aarch64-linux-gnu-gcc-12"],
        )],
        |_, _| {},
    )
    .unwrap();
    assert_eq!((report.parsed, report.failed, report.errors), (1, 0, 0));
    assert_eq!(pairs(&report.graph), set(&["entry(a.c) -> arm(a.c)"]));
}

/// A kernel-style gcc command line: flags clang does not have, and a
/// dependency file asked for through `-Wp,`. The TU is read in full — the `-D`
/// after the unknown flags included — nothing is written into the project, and
/// the refused flags are reported rather than counted as errors in the code.
#[test]
fn gcc_only_flags_neither_fail_the_tu_nor_count_as_errors() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let body = "#ifndef CONFIG_X\n#error the define was lost\n#endif\n\
                static void helper(void) {}\n\
                void entry(void) { helper(); }\n";
    write(root, "a.c", body);
    write(
        root,
        "b.c",
        &body.replace("helper", "other").replace("entry", "start"),
    );
    let flags = [
        "gcc",
        "-Wp,-MD,.a.o.d",
        "-MJ",
        "a.json",
        "-Werror",
        "-fconserve-stack",
        "-mindirect-branch=thunk-extern",
        "-fno-allow-store-data-races",
        "-mrecord-mcount",
        "-falign-jumps=1",
        "-Wno-maybe-uninitialized",
        "-DCONFIG_X",
    ];
    let jobs = [job_with(root, "a.c", &flags), job_with(root, "b.c", &flags)];
    let report = index(root, &jobs, |_, _| {}).unwrap();

    assert_eq!((report.parsed, report.failed, report.errors), (2, 0, 0));
    assert_eq!(
        pairs(&report.graph),
        set(&["entry(a.c) -> helper(a.c)", "start(b.c) -> other(b.c)"])
    );
    assert_eq!(
        report.rejected_arguments,
        [
            "-fconserve-stack",
            "-mindirect-branch=thunk-extern",
            "-fno-allow-store-data-races",
            "-mrecord-mcount",
        ]
    );
    assert!(!root.join(".a.o.d").exists());
    assert!(!root.join("a.json").exists());
}

/// The summary counts files, not diagnostics: one TU with three errors is one
/// file in doubt, and a TU libclang cannot open at all is a failure, not an
/// error.
#[test]
fn the_summary_counts_translation_units() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "good.c", "void ok(void) {}\n");
    write(
        root,
        "bad.c",
        "#include \"nowhere.h\"\n\
         void f(void) { undeclared_a(); undeclared_b(); }\n",
    );
    let jobs = vec![
        job(root, "good.c"),
        job(root, "bad.c"),
        job(root, "missing.c"),
    ];
    let report = index(root, &jobs, |_, _| {}).unwrap();
    let summary = report.summary();
    assert_eq!(
        (summary.parsed, summary.failed, summary.with_errors),
        (2, 1, 1),
        "{} error diagnostic(s)",
        report.errors
    );
    assert!(report.errors >= 1);
    assert_eq!(summary.quality(), model::ParseQuality::Poor);
}

/// A header whose inline function calls a different thing per build, and two
/// TUs that include it.
fn write_flavoured_header(root: &Path) {
    std::fs::create_dir_all(root.join("include")).unwrap();
    write(
        root,
        "include/pick.h",
        "void call_a(void);\n\
         void call_b(void);\n\
         void always(void);\n\
         static inline void pick(void) {\n\
         #ifdef FLAV_A\n\
             call_a();\n\
         #else\n\
             call_b();\n\
         #endif\n\
             always();\n\
         }\n",
    );
    write(
        root,
        "defs.c",
        "void call_a(void) {}\nvoid call_b(void) {}\nvoid always(void) {}\n",
    );
}

#[test]
fn a_header_body_is_walked_once_per_flavour() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_flavoured_header(root);
    write(
        root,
        "a.c",
        "#include \"pick.h\"\nvoid a_main(void) { pick(); }\n",
    );
    write(
        root,
        "b.c",
        "#include \"pick.h\"\nvoid b_main(void) { pick(); }\n",
    );

    // Two builds of the one database, as a run without `--flavour` sees it.
    let jobs = vec![
        job_with(root, "a.c", &["cc", "-Iinclude", "-DFLAV_A"]),
        job_with(root, "b.c", &["cc", "-Iinclude"]),
        job_with(root, "defs.c", &["cc", "-Iinclude"]),
    ];
    let report = index(root, &jobs, |_, _| {}).unwrap();

    // One node, carrying the calls of both builds; the call both builds make
    // is the same site and so one edge.
    assert_eq!(
        sites(&report.graph),
        set(&[
            "a_main -> pick @ a.c:2",
            "b_main -> pick @ b.c:2",
            "pick -> call_a @ pick.h:6",
            "pick -> call_b @ pick.h:8",
            "pick -> always @ pick.h:10",
        ])
    );
    let picks = report
        .graph
        .functions()
        .filter(|f| f.qualified_name == "pick")
        .count();
    assert_eq!(picks, 1);
    assert_eq!(report.external_calls, 0);
}

#[test]
fn a_header_body_is_walked_once_within_a_flavour() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_flavoured_header(root);
    // The same flavour — no `-D` tells them apart — though `a.c` defines the
    // macro itself. A flavour is taken to be one piece of code, so the second
    // includer's body is not walked: this is the skip that keeps a header from
    // being read once per includer, and what it costs when a TU breaks the
    // assumption.
    write(
        root,
        "a.c",
        "#define FLAV_A\n#include \"pick.h\"\nvoid a_main(void) { pick(); }\n",
    );
    write(
        root,
        "b.c",
        "#include \"pick.h\"\nvoid b_main(void) { pick(); }\n",
    );

    let jobs = vec![job(root, "a.c"), job(root, "b.c"), job(root, "defs.c")];
    let report = index(root, &jobs, |_, _| {}).unwrap();

    assert_eq!(
        sites(&report.graph),
        set(&[
            "a_main -> pick @ a.c:3",
            "b_main -> pick @ b.c:2",
            "pick -> call_a @ pick.h:6",
            "pick -> always @ pick.h:10",
        ])
    );
}

#[test]
fn an_external_call_two_flavours_make_is_counted_once() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("include")).unwrap();
    write(
        root,
        "include/ext.h",
        "void elsewhere(void);\nstatic inline void wrap(void) { elsewhere(); }\n",
    );
    write(
        root,
        "a.c",
        "#include \"ext.h\"\nvoid a_main(void) { wrap(); }\n",
    );
    write(
        root,
        "b.c",
        "#include \"ext.h\"\nvoid b_main(void) { wrap(); }\n",
    );

    let jobs = vec![
        job_with(root, "a.c", &["cc", "-Iinclude", "-DONE"]),
        job_with(root, "b.c", &["cc", "-Iinclude", "-DTWO"]),
    ];
    let report = index_keeping(root, &jobs, BodilessCallees::Drop, false);
    assert_eq!(report.external_calls, 1);
}

/// A serial run in this process that keeps `bodiless` callees, cached or not.
fn index_keeping(root: &Path, jobs: &[Job], bodiless: BodilessCallees, cache: bool) -> Report {
    let options = Options {
        workers: 0,
        cache,
        bodiless_callees: bodiless,
        ..Options::default()
    };
    index_with(root, jobs, &options, &Cancel::default(), |_, _| {}).unwrap()
}

/// A project whose `main` calls one function it defines, one a project header
/// declares and nothing defines, and one a header outside the project
/// declares — the shape of a unit test calling `jet_mallctl` and `strchr`.
fn write_bodiless_project(root: &Path, outside: &Path) -> Vec<Job> {
    std::fs::create_dir(root.join("include")).unwrap();
    write(root, "include/api.h", "int api_call(int);\n");
    write(outside, "libc.h", "int lib_call(int);\n");
    write(
        root,
        "main.c",
        "#include \"api.h\"\n#include \"libc.h\"\n\
         static int mine(int x) { return __builtin_clz((unsigned)x); }\n\
         int main(void) { return mine(api_call(lib_call(1))); }\n",
    );
    let outside = format!("-I{}", outside.display());
    vec![job_with(root, "main.c", &["cc", "-Iinclude", &outside])]
}

#[test]
fn bodiless_callees_are_kept_as_far_as_the_setting_says() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = write_bodiless_project(root, outside.path());

    let drop = index_keeping(root, &jobs, BodilessCallees::Drop, false);
    assert_eq!(pairs(&drop.graph), set(&["main(main.c) -> mine(main.c)"]));
    // The builtin is left out under every setting: the compiler declared it.
    assert_eq!(drop.external_calls, 3);

    let project = index_keeping(root, &jobs, BodilessCallees::Project, false);
    assert_eq!(
        pairs(&project.graph),
        set(&[
            "main(main.c) -> mine(main.c)",
            "main(main.c) -> api_call(api.h)",
        ])
    );
    assert_eq!(project.external_calls, 2);
    // A leaf at its declaration: that is all there is of it.
    let api = project.graph.find_function_by_name("api_call").unwrap();
    assert_eq!(api.definition_location.line, 1);
    assert!(api.definition_location.file_path.ends_with("include/api.h"));

    let all = index_keeping(root, &jobs, BodilessCallees::All, false);
    assert_eq!(
        pairs(&all.graph),
        set(&[
            "main(main.c) -> mine(main.c)",
            "main(main.c) -> api_call(api.h)",
            "main(main.c) -> lib_call(libc.h)",
        ])
    );
    assert_eq!(all.external_calls, 1);
}

/// The setting is read after the merge, so one cache entry answers all three
/// — and a hit answers them exactly as a parse does.
#[test]
fn a_cached_parse_answers_every_bodiless_setting() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = write_bodiless_project(root, outside.path());
    age(root);
    age(outside.path());

    for bodiless in BodilessCallees::ALL {
        let parsed = index_keeping(root, &jobs, bodiless, false);
        let cold = index_keeping(root, &jobs, bodiless, true);
        let warm = index_keeping(root, &jobs, bodiless, true);
        assert_eq!(warm.cached, 1, "{bodiless:?}: answered from the cache");
        assert_eq!(fingerprint(&cold), fingerprint(&parsed), "{bodiless:?}");
        assert_eq!(fingerprint(&warm), fingerprint(&parsed), "{bodiless:?}");
    }
}

// ── The on-disk cache ────────────────────────────────────────────────────────

/// A run that caches, in this process.
fn cached_run(root: &Path, jobs: &[Job]) -> Report {
    let options = Options {
        workers: 0,
        cache: true,
        ..Options::default()
    };
    index_with(root, jobs, &options, &Cancel::default(), |_, _| {}).unwrap()
}

/// Everything a report says but `cached`, with every function where it is
/// defined and every call site in full.
fn fingerprint(report: &Report) -> (Vec<usize>, Vec<String>, BTreeSet<String>, BTreeSet<String>) {
    let graph = &report.graph;
    let functions = graph
        .functions()
        .map(|f| {
            let at = &f.definition_location;
            format!(
                "{} @ {}:{}:{}",
                f.qualified_name, at.file_path, at.line, at.column
            )
        })
        .collect();
    let calls = graph
        .edges()
        .map(|e| {
            let name = |id| graph.get_function(id).unwrap().qualified_name.clone();
            let at = &e.call_location;
            format!(
                "{} -> {} @ {}:{}:{}",
                name(&e.caller),
                name(&e.callee),
                at.file_path,
                at.line,
                at.column
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
        calls,
    )
}

/// Set every file under `dir` an hour into the past. A file modified within
/// two seconds of a parse is not trusted to be what the parse read, and is not
/// cached; every file a test writes is that new.
fn age(dir: &Path) {
    let then = SystemTime::now() - Duration::from_secs(3600);
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            age(&path);
        } else {
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(then).unwrap();
        }
    }
}

/// A header included under two flavours and twice within one, so a cached
/// result standing in for a parse has header bodies to get wrong, and a gcc
/// flag clang refuses, so the arguments a run learns have something to differ
/// over; and a second header only one TU includes.
fn cached_project(root: &Path) -> Vec<Job> {
    write_flavoured_header(root);
    write(
        root,
        "include/only_c.h",
        "void always(void);\nstatic inline void only_c(void) { always(); }\n",
    );
    write(
        root,
        "a.c",
        "#include \"pick.h\"\nvoid a_main(void) { pick(); }\n",
    );
    write(
        root,
        "b.c",
        "#include \"pick.h\"\nvoid b_main(void) { pick(); }\n",
    );
    write(
        root,
        "c.c",
        "#include \"pick.h\"\n#include \"only_c.h\"\nvoid c_main(void) { pick(); only_c(); }\n",
    );
    age(root);
    vec![
        job_with(
            root,
            "a.c",
            &["cc", "-Iinclude", "-DFLAV_A", "-fconserve-stack"],
        ),
        job_with(root, "b.c", &["cc", "-Iinclude", "-fconserve-stack"]),
        job_with(root, "c.c", &["cc", "-Iinclude", "-fconserve-stack"]),
        job_with(root, "defs.c", &["cc", "-Iinclude", "-fconserve-stack"]),
    ]
}

#[test]
fn a_second_run_is_answered_from_the_cache_with_the_same_graph() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = cached_project(root);
    let uncached = index(root, &jobs, |_, _| {}).unwrap();
    assert_eq!(
        uncached.rejected_arguments,
        vec!["-fconserve-stack".to_string()]
    );

    let cold = cached_run(root, &jobs);
    assert_eq!(cold.cached, 0);
    assert_eq!(fingerprint(&cold), fingerprint(&uncached));
    assert!(root.join(".cache/coon/libclang").is_dir());

    let warm = cached_run(root, &jobs);
    assert_eq!(warm.cached, jobs.len());
    assert_eq!(fingerprint(&warm), fingerprint(&uncached));
    assert_eq!(warm.summary().cached, jobs.len());
}

#[test]
fn changing_an_included_header_invalidates_its_includers_only() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = cached_project(root);
    cached_run(root, &jobs);

    write(
        root,
        "include/only_c.h",
        "void call_b(void);\nstatic inline void only_c(void) { call_b(); }\n",
    );
    age(root);
    let warm = cached_run(root, &jobs);
    // a.c, b.c and defs.c never read it.
    assert_eq!(warm.cached, 3);
    assert!(sites(&warm.graph).contains("only_c -> call_b @ only_c.h:2"));
    assert_eq!(
        fingerprint(&warm),
        fingerprint(&index(root, &jobs, |_, _| {}).unwrap())
    );
    // And the new result is what the next run is answered with.
    assert_eq!(cached_run(root, &jobs).cached, 4);

    std::fs::remove_file(root.join("include/only_c.h")).unwrap();
    assert_eq!(
        cached_run(root, &jobs).cached,
        3,
        "a missing input is a miss"
    );
}

#[test]
fn a_touched_file_whose_contents_did_not_change_is_still_a_hit() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = cached_project(root);
    cached_run(root, &jobs);

    let header = std::fs::File::options()
        .write(true)
        .open(root.join("include/pick.h"))
        .unwrap();
    header
        .set_modified(SystemTime::now() - Duration::from_secs(60))
        .unwrap();
    assert_eq!(cached_run(root, &jobs).cached, 4);
}

#[test]
fn a_corrupt_entry_is_parsed_again() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = cached_project(root);
    let cold = cached_run(root, &jobs);

    let entries: Vec<_> = std::fs::read_dir(root.join(".cache/coon/libclang/v1"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries.len(), jobs.len());
    for (n, entry) in entries.iter().enumerate() {
        let bytes = std::fs::read(entry).unwrap();
        // Cut short, as a crash mid-write without the rename would leave it,
        // or simply not JSON.
        let broken = if n % 2 == 0 {
            &bytes[..bytes.len() / 2]
        } else {
            &b"\0\xff"[..]
        };
        std::fs::write(entry, broken).unwrap();
    }
    let reparsed = cached_run(root, &jobs);
    assert_eq!(reparsed.cached, 0);
    assert_eq!(fingerprint(&reparsed), fingerprint(&cold));
    assert_eq!(
        cached_run(root, &jobs).cached,
        jobs.len(),
        "rewritten whole"
    );
}

#[test]
fn a_file_written_just_before_the_parse_is_not_cached() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let jobs = cached_project(root);
    write(
        root,
        "a.c",
        "#include \"pick.h\"\nvoid a_main(void) { pick(); pick(); }\n",
    );
    cached_run(root, &jobs);
    // It may have been read before the write or after it.
    assert_eq!(cached_run(root, &jobs).cached, 3);
}
