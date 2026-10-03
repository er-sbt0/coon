//! The indexer against a project written out on the spot. Needs libclang at
//! runtime, as the `lsp` tests need clangd.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;

use clang_index::{index, Job};
use lsp::compile_commands::CompileCommand;
use model::CallGraph;

/// libclang allows one `Clang` per process at a time, and the test harness runs
/// tests on parallel threads.
static LIBCLANG: Mutex<()> = Mutex::new(());

fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
}

fn job(dir: &Path, file: &str) -> Job {
    Job::from_entry(&CompileCommand {
        directory: dir.display().to_string(),
        command: None,
        file: file.into(),
        arguments: Some(
            ["cc", "-Iinclude", "-MMD", "-c", file, "-o", "out.o"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        ),
        output: None,
    })
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

#[test]
fn the_dependency_file_flag_writes_nothing() {
    let _guard = LIBCLANG.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "x.c", "void f(void) {}\n");
    index(dir.path(), &[job(dir.path(), "x.c")], |_, _| {}).unwrap();
    assert!(!dir.path().join("x.d").exists());
    assert!(!dir.path().join("out.d").exists());
}
