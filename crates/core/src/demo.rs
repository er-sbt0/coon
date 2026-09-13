//! The demo graph models `mdb`, a small C key-value store daemon:
//!
//! ```text
//! src/
//!   main.c          argument parsing, start-up and shutdown
//!   config.c        key=value configuration file
//!   log.c           rotating log file
//!   signals.c       SIGINT/SIGTERM handling
//!   net/socket.c    thin BSD-socket wrappers
//!   net/server.c    listener and per-connection loop
//!   proto/parser.c  wire-protocol request parsing
//!   proto/response.c reply formatting
//!   store/store.c   the key-value store itself
//!   store/index.c   open-addressed hash index
//!   store/wal.c     write-ahead log
//!   util/buffer.c   growable byte buffer
//!   util/alloc.c    checked allocation
//!   util/crc32.c    checksums
//! ```
//!
//! It is deliberately larger than a toy: it has deep chains (`main` →
//! `run_server` → `server_accept_loop` → `handle_client` → `store_get` →
//! `index_lookup` → `hash_key`), widely shared leaves (`xmalloc`,
//! `log_write`, `buf_append`), a second root that is not `main`
//! (`on_signal`, installed as a handler), direct recursion (`parse_token`)
//! and a cycle (`index_insert` ⇄ `index_rehash`) — the shapes a call-graph
//! viewer has to survive.

use std::collections::HashMap;

use log::debug;
use model::{
    CallGraph, Diagnostic, DiagnosticSeverity, FunctionNode, Location, ReferenceSymbolKind,
    ReferencingSymbol, SymbolId,
};

/// A function definition: name, defining file, and the line it starts on.
struct FnSpec {
    name: &'static str,
    file: &'static str,
    line: u32,
}

/// A diagnostic, positioned relative to the start of its function.
struct DiagSpec {
    func: &'static str,
    line_offset: u32,
    column: u32,
    severity: DiagnosticSeverity,
    message: &'static str,
    code: &'static str,
}

const FUNCTIONS: &[FnSpec] = &[
    // src/main.c
    FnSpec {
        name: "main",
        file: "src/main.c",
        line: 42,
    },
    FnSpec {
        name: "parse_args",
        file: "src/main.c",
        line: 96,
    },
    FnSpec {
        name: "print_usage",
        file: "src/main.c",
        line: 158,
    },
    FnSpec {
        name: "run_server",
        file: "src/main.c",
        line: 184,
    },
    FnSpec {
        name: "shutdown_gracefully",
        file: "src/main.c",
        line: 246,
    },
    // src/config.c
    FnSpec {
        name: "config_load",
        file: "src/config.c",
        line: 34,
    },
    FnSpec {
        name: "config_parse_line",
        file: "src/config.c",
        line: 96,
    },
    FnSpec {
        name: "config_get_int",
        file: "src/config.c",
        line: 150,
    },
    FnSpec {
        name: "config_get_str",
        file: "src/config.c",
        line: 178,
    },
    FnSpec {
        name: "config_free",
        file: "src/config.c",
        line: 206,
    },
    // src/log.c
    FnSpec {
        name: "log_init",
        file: "src/log.c",
        line: 28,
    },
    FnSpec {
        name: "log_write",
        file: "src/log.c",
        line: 64,
    },
    FnSpec {
        name: "log_rotate",
        file: "src/log.c",
        line: 118,
    },
    FnSpec {
        name: "log_close",
        file: "src/log.c",
        line: 166,
    },
    // src/signals.c
    FnSpec {
        name: "install_signal_handlers",
        file: "src/signals.c",
        line: 22,
    },
    FnSpec {
        name: "on_signal",
        file: "src/signals.c",
        line: 58,
    },
    // src/net/server.c
    FnSpec {
        name: "server_start",
        file: "src/net/server.c",
        line: 40,
    },
    FnSpec {
        name: "server_accept_loop",
        file: "src/net/server.c",
        line: 98,
    },
    FnSpec {
        name: "handle_client",
        file: "src/net/server.c",
        line: 162,
    },
    FnSpec {
        name: "conn_close",
        file: "src/net/server.c",
        line: 254,
    },
    FnSpec {
        name: "server_stop",
        file: "src/net/server.c",
        line: 288,
    },
    // src/net/socket.c
    FnSpec {
        name: "socket_listen",
        file: "src/net/socket.c",
        line: 30,
    },
    FnSpec {
        name: "socket_accept",
        file: "src/net/socket.c",
        line: 82,
    },
    FnSpec {
        name: "socket_set_nonblocking",
        file: "src/net/socket.c",
        line: 118,
    },
    FnSpec {
        name: "socket_read",
        file: "src/net/socket.c",
        line: 146,
    },
    FnSpec {
        name: "socket_write",
        file: "src/net/socket.c",
        line: 190,
    },
    FnSpec {
        name: "socket_close",
        file: "src/net/socket.c",
        line: 232,
    },
    // src/proto/parser.c
    FnSpec {
        name: "parse_request",
        file: "src/proto/parser.c",
        line: 38,
    },
    FnSpec {
        name: "parse_token",
        file: "src/proto/parser.c",
        line: 104,
    },
    FnSpec {
        name: "parse_integer",
        file: "src/proto/parser.c",
        line: 158,
    },
    FnSpec {
        name: "request_free",
        file: "src/proto/parser.c",
        line: 196,
    },
    // src/proto/response.c
    FnSpec {
        name: "response_write_ok",
        file: "src/proto/response.c",
        line: 24,
    },
    FnSpec {
        name: "response_write_value",
        file: "src/proto/response.c",
        line: 58,
    },
    FnSpec {
        name: "response_write_error",
        file: "src/proto/response.c",
        line: 102,
    },
    // src/store/store.c
    FnSpec {
        name: "store_open",
        file: "src/store/store.c",
        line: 52,
    },
    FnSpec {
        name: "store_get",
        file: "src/store/store.c",
        line: 124,
    },
    FnSpec {
        name: "store_put",
        file: "src/store/store.c",
        line: 170,
    },
    FnSpec {
        name: "store_delete",
        file: "src/store/store.c",
        line: 232,
    },
    FnSpec {
        name: "store_compact",
        file: "src/store/store.c",
        line: 286,
    },
    FnSpec {
        name: "store_close",
        file: "src/store/store.c",
        line: 364,
    },
    // src/store/index.c
    FnSpec {
        name: "hash_key",
        file: "src/store/index.c",
        line: 24,
    },
    FnSpec {
        name: "index_lookup",
        file: "src/store/index.c",
        line: 50,
    },
    FnSpec {
        name: "index_insert",
        file: "src/store/index.c",
        line: 94,
    },
    FnSpec {
        name: "index_remove",
        file: "src/store/index.c",
        line: 148,
    },
    FnSpec {
        name: "index_rehash",
        file: "src/store/index.c",
        line: 190,
    },
    // src/store/wal.c
    FnSpec {
        name: "wal_append",
        file: "src/store/wal.c",
        line: 46,
    },
    FnSpec {
        name: "wal_replay",
        file: "src/store/wal.c",
        line: 110,
    },
    FnSpec {
        name: "wal_sync",
        file: "src/store/wal.c",
        line: 182,
    },
    FnSpec {
        name: "wal_truncate",
        file: "src/store/wal.c",
        line: 210,
    },
    // src/util/buffer.c
    FnSpec {
        name: "buf_init",
        file: "src/util/buffer.c",
        line: 18,
    },
    FnSpec {
        name: "buf_reserve",
        file: "src/util/buffer.c",
        line: 46,
    },
    FnSpec {
        name: "buf_append",
        file: "src/util/buffer.c",
        line: 84,
    },
    FnSpec {
        name: "buf_free",
        file: "src/util/buffer.c",
        line: 118,
    },
    // src/util/alloc.c
    FnSpec {
        name: "xmalloc",
        file: "src/util/alloc.c",
        line: 14,
    },
    FnSpec {
        name: "xrealloc",
        file: "src/util/alloc.c",
        line: 36,
    },
    FnSpec {
        name: "oom_die",
        file: "src/util/alloc.c",
        line: 62,
    },
    // src/util/crc32.c
    FnSpec {
        name: "crc32_update",
        file: "src/util/crc32.c",
        line: 30,
    },
];

/// Call edges, as (caller, callee) pairs grouped by caller. Call-site
/// positions are derived from the caller's definition line, so the table
/// stays readable and the line numbers stay inside the calling function.
const CALLS: &[(&str, &str)] = &[
    // main.c
    ("main", "parse_args"),
    ("main", "print_usage"),
    ("main", "config_load"),
    ("main", "log_init"),
    ("main", "install_signal_handlers"),
    ("main", "run_server"),
    ("main", "shutdown_gracefully"),
    ("parse_args", "parse_integer"),
    ("parse_args", "print_usage"),
    ("parse_args", "xmalloc"),
    ("run_server", "config_get_int"),
    ("run_server", "config_get_str"),
    ("run_server", "store_open"),
    ("run_server", "server_start"),
    ("run_server", "server_accept_loop"),
    ("run_server", "server_stop"),
    ("run_server", "log_write"),
    ("shutdown_gracefully", "store_close"),
    ("shutdown_gracefully", "config_free"),
    ("shutdown_gracefully", "log_close"),
    ("shutdown_gracefully", "log_write"),
    // config.c
    ("config_load", "buf_init"),
    ("config_load", "buf_append"),
    ("config_load", "config_parse_line"),
    ("config_load", "log_write"),
    ("config_parse_line", "parse_token"),
    ("config_parse_line", "xmalloc"),
    ("config_parse_line", "log_write"),
    ("config_get_int", "config_get_str"),
    ("config_get_int", "parse_integer"),
    ("config_free", "buf_free"),
    // log.c
    ("log_init", "buf_init"),
    ("log_init", "xmalloc"),
    ("log_write", "buf_append"),
    ("log_write", "log_rotate"),
    ("log_rotate", "buf_free"),
    ("log_rotate", "buf_init"),
    ("log_close", "log_write"),
    ("log_close", "buf_free"),
    // signals.c
    ("install_signal_handlers", "log_write"),
    ("on_signal", "server_stop"),
    ("on_signal", "log_write"),
    // net/server.c
    ("server_start", "socket_listen"),
    ("server_start", "socket_set_nonblocking"),
    ("server_start", "xmalloc"),
    ("server_start", "log_write"),
    ("server_accept_loop", "socket_accept"),
    ("server_accept_loop", "handle_client"),
    ("server_accept_loop", "conn_close"),
    ("server_accept_loop", "log_write"),
    ("handle_client", "socket_read"),
    ("handle_client", "parse_request"),
    ("handle_client", "store_get"),
    ("handle_client", "store_put"),
    ("handle_client", "store_delete"),
    ("handle_client", "response_write_ok"),
    ("handle_client", "response_write_value"),
    ("handle_client", "response_write_error"),
    ("handle_client", "request_free"),
    ("handle_client", "log_write"),
    ("conn_close", "socket_close"),
    ("conn_close", "buf_free"),
    ("conn_close", "log_write"),
    ("server_stop", "socket_close"),
    ("server_stop", "log_write"),
    // net/socket.c
    ("socket_listen", "socket_set_nonblocking"),
    ("socket_listen", "log_write"),
    ("socket_accept", "socket_set_nonblocking"),
    ("socket_read", "buf_reserve"),
    ("socket_write", "log_write"),
    // proto/parser.c
    ("parse_request", "parse_token"),
    ("parse_request", "parse_integer"),
    ("parse_request", "buf_init"),
    ("parse_request", "xmalloc"),
    ("parse_token", "parse_token"),
    ("parse_token", "buf_append"),
    ("request_free", "buf_free"),
    // proto/response.c
    ("response_write_ok", "socket_write"),
    ("response_write_ok", "buf_append"),
    ("response_write_value", "buf_reserve"),
    ("response_write_value", "buf_append"),
    ("response_write_value", "socket_write"),
    ("response_write_error", "buf_append"),
    ("response_write_error", "socket_write"),
    ("response_write_error", "log_write"),
    // store/store.c
    ("store_open", "wal_replay"),
    ("store_open", "index_rehash"),
    ("store_open", "xmalloc"),
    ("store_open", "log_write"),
    ("store_get", "index_lookup"),
    ("store_get", "log_write"),
    ("store_put", "wal_append"),
    ("store_put", "index_insert"),
    ("store_put", "store_compact"),
    ("store_put", "xmalloc"),
    ("store_delete", "wal_append"),
    ("store_delete", "index_remove"),
    ("store_compact", "index_rehash"),
    ("store_compact", "wal_truncate"),
    ("store_compact", "wal_sync"),
    ("store_compact", "crc32_update"),
    ("store_compact", "xmalloc"),
    ("store_compact", "log_write"),
    ("store_close", "wal_sync"),
    ("store_close", "wal_truncate"),
    ("store_close", "buf_free"),
    ("store_close", "log_write"),
    // store/index.c
    ("index_lookup", "hash_key"),
    ("index_insert", "hash_key"),
    ("index_insert", "index_rehash"),
    ("index_insert", "xmalloc"),
    ("index_remove", "hash_key"),
    ("index_rehash", "index_insert"),
    ("index_rehash", "hash_key"),
    ("index_rehash", "xrealloc"),
    // store/wal.c
    ("wal_append", "crc32_update"),
    ("wal_append", "buf_append"),
    ("wal_append", "wal_sync"),
    ("wal_replay", "crc32_update"),
    ("wal_replay", "buf_reserve"),
    ("wal_replay", "index_insert"),
    ("wal_replay", "log_write"),
    ("wal_truncate", "log_write"),
    // util/
    ("buf_init", "xmalloc"),
    ("buf_reserve", "xrealloc"),
    ("buf_append", "buf_reserve"),
    ("xmalloc", "oom_die"),
    ("xrealloc", "oom_die"),
];

/// Diagnostics in the style clangd reports them (clang-tidy / static analyser
/// check names), so the severity styling has something realistic to render.
const DIAGNOSTICS: &[DiagSpec] = &[
    DiagSpec {
        func: "store_compact",
        line_offset: 31,
        column: 12,
        severity: DiagnosticSeverity::ERROR,
        message: "Use of memory after it is freed: 'old->slots'",
        code: "clang-analyzer-unix.Malloc",
    },
    DiagSpec {
        func: "store_compact",
        line_offset: 44,
        column: 8,
        severity: DiagnosticSeverity::WARNING,
        message: "Value stored to 'written' is never read",
        code: "clang-analyzer-deadcode.DeadStores",
    },
    DiagSpec {
        func: "handle_client",
        line_offset: 26,
        column: 9,
        severity: DiagnosticSeverity::ERROR,
        message: "Dereference of null pointer: 'req' may be null after parse_request()",
        code: "clang-analyzer-core.NullDereference",
    },
    DiagSpec {
        func: "handle_client",
        line_offset: 61,
        column: 4,
        severity: DiagnosticSeverity::WARNING,
        message: "Potential leak of memory pointed to by 'req' on the error path",
        code: "clang-analyzer-unix.Malloc",
    },
    DiagSpec {
        func: "socket_read",
        line_offset: 12,
        column: 4,
        severity: DiagnosticSeverity::WARNING,
        message: "Return value of 'read' is not checked",
        code: "bugprone-unused-return-value",
    },
    DiagSpec {
        func: "xrealloc",
        line_offset: 9,
        column: 11,
        severity: DiagnosticSeverity::ERROR,
        message: "Potential leak of memory pointed to by 'p' when realloc() fails",
        code: "clang-analyzer-unix.Malloc",
    },
    DiagSpec {
        func: "parse_token",
        line_offset: 0,
        column: 12,
        severity: DiagnosticSeverity::WARNING,
        message: "Function 'parse_token' is recursive; deep input may exhaust the stack",
        code: "misc-no-recursion",
    },
    DiagSpec {
        func: "config_parse_line",
        line_offset: 18,
        column: 16,
        severity: DiagnosticSeverity::WARNING,
        message: "Function 'strtok' is not thread-safe; use 'strtok_r'",
        code: "concurrency-mt-unsafe",
    },
    DiagSpec {
        func: "wal_replay",
        line_offset: 33,
        column: 20,
        severity: DiagnosticSeverity::WARNING,
        message: "Comparison of integers of different signs: 'ssize_t' and 'size_t'",
        code: "clang-diagnostic-sign-compare",
    },
    DiagSpec {
        func: "on_signal",
        line_offset: 6,
        column: 4,
        severity: DiagnosticSeverity::WARNING,
        message: "Call to non-async-signal-safe function 'log_write' from a signal handler",
        code: "bugprone-signal-handler",
    },
    DiagSpec {
        func: "hash_key",
        line_offset: 0,
        column: 9,
        severity: DiagnosticSeverity::INFORMATION,
        message: "No previous prototype for function 'hash_key'; consider making it static",
        code: "clang-diagnostic-missing-prototypes",
    },
    DiagSpec {
        func: "log_write",
        line_offset: 14,
        column: 8,
        severity: DiagnosticSeverity::INFORMATION,
        message: "Format string is not a string literal",
        code: "clang-diagnostic-format-nonliteral",
    },
    DiagSpec {
        func: "index_rehash",
        line_offset: 11,
        column: 4,
        severity: DiagnosticSeverity::HINT,
        message: "Loop variable 'i' can be declared inside the for statement",
        code: "readability-isolate-declaration",
    },
    DiagSpec {
        func: "buf_reserve",
        line_offset: 7,
        column: 22,
        severity: DiagnosticSeverity::HINT,
        message: "Narrowing conversion from 'size_t' to 'unsigned int'",
        code: "bugprone-narrowing-conversions",
    },
];

/// Where a call to `callee` sits inside `caller`'s body. Spreading the call
/// sites out keeps every call-site location distinct and inside the function.
fn call_site(caller: &FnSpec, nth: u32) -> Location {
    Location::new(
        caller.file.to_string(),
        caller.line + 4 + nth * 3,
        4 + (nth % 3) * 4,
    )
}

pub fn create_demo_call_graph() -> CallGraph {
    let specs: HashMap<&str, &FnSpec> = FUNCTIONS.iter().map(|f| (f.name, f)).collect();

    let mut nodes: HashMap<&str, FunctionNode> = FUNCTIONS
        .iter()
        .map(|f| {
            let node = FunctionNode::new(
                f.name.to_string(),
                // Plain C has no namespaces, so clangd reports the bare name
                // as the qualified name too.
                f.name.to_string(),
                Location::new(f.file.to_string(), f.line, 0).with_length(f.name.len() as u32),
            );
            (f.name, node)
        })
        .collect();

    for d in DIAGNOSTICS {
        let spec = specs[d.func];
        nodes.get_mut(d.func).unwrap().add_diagnostic(Diagnostic {
            location: Location::new(spec.file.to_string(), spec.line + d.line_offset, d.column),
            severity: d.severity,
            message: d.message.to_string(),
            code: Some(d.code.to_string()),
        });
    }

    // Resolve each call to a concrete call site, and record it on the callee
    // as a reference carrying the calling function — that is what clangd's
    // textDocument/references gives us, and what the references pane shows.
    let mut seen_per_caller: HashMap<&str, u32> = HashMap::new();
    let mut calls: Vec<(&str, &str, Location)> = Vec::with_capacity(CALLS.len());
    for (caller, callee) in CALLS {
        let nth = seen_per_caller.entry(caller).or_insert(0);
        let location = call_site(specs[caller], *nth);
        *nth += 1;
        calls.push((caller, callee, location));
    }

    for (caller, callee, location) in &calls {
        nodes.get_mut(callee).unwrap().add_reference_with_symbol(
            location.clone(),
            Some(ReferencingSymbol {
                name: caller.to_string(),
                qualified_name: caller.to_string(),
                kind: ReferenceSymbolKind::Function,
            }),
        );
    }

    // A few references that are not calls: taking a function's address, and
    // naming it in a header. These exercise the non-Function reference kinds.
    for (callee, location, symbol) in NON_CALL_REFERENCES {
        nodes.get_mut(callee).unwrap().add_reference_with_symbol(
            Location::new(location.0.to_string(), location.1, location.2),
            Some(ReferencingSymbol {
                name: symbol.0.to_string(),
                qualified_name: symbol.0.to_string(),
                kind: symbol.1.clone(),
            }),
        );
    }

    let mut graph = CallGraph::new();
    let mut ids: HashMap<&str, SymbolId> = HashMap::new();
    for f in FUNCTIONS {
        let node = nodes.remove(f.name).unwrap();
        ids.insert(f.name, graph.add_function(node));
    }

    for (caller, callee, location) in calls {
        graph.add_call(ids[caller], ids[callee], location);
    }

    debug!(
        "Demo call graph: {} functions, {} calls, {} diagnostics",
        graph.node_count(),
        graph.edge_count(),
        DIAGNOSTICS.len()
    );
    debug!("  main");
    debug!("  ├─ parse_args → parse_integer");
    debug!("  ├─ config_load → config_parse_line → parse_token (recursive)");
    debug!("  ├─ log_init");
    debug!("  ├─ install_signal_handlers    (on_signal is a second root)");
    debug!("  ├─ run_server");
    debug!("  │  ├─ store_open → wal_replay → index_insert ⇄ index_rehash");
    debug!("  │  ├─ server_start → socket_listen");
    debug!("  │  └─ server_accept_loop → handle_client (has errors!)");
    debug!("  │     ├─ parse_request → parse_token");
    debug!("  │     ├─ store_get/put/delete → index_* → hash_key");
    debug!("  │     └─ response_write_* → socket_write");
    debug!("  └─ shutdown_gracefully → store_close, config_free, log_close");

    graph
}

/// (callee, (file, line, column), (referencing symbol, its kind))
type NonCallRef = (
    &'static str,
    (&'static str, u32, u32),
    (&'static str, ReferenceSymbolKind),
);

/// Non-call references: a handler installed by address, a compaction callback
/// stored in a struct field, and prototypes named in headers.
const NON_CALL_REFERENCES: &[NonCallRef] = &[
    (
        "on_signal",
        ("src/signals.c", 30, 22),
        ("install_signal_handlers", ReferenceSymbolKind::Function),
    ),
    (
        "store_compact",
        ("src/store/store.c", 38, 24),
        ("store_ops", ReferenceSymbolKind::Field),
    ),
    (
        "crc32_update",
        ("src/util/crc32.h", 12, 8),
        ("crc32", ReferenceSymbolKind::Module),
    ),
    (
        "hash_key",
        ("src/store/index.c", 18, 14),
        ("index_table", ReferenceSymbolKind::Struct),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_demo_graph_creation() {
        let graph = create_demo_call_graph();

        assert_eq!(graph.node_count(), FUNCTIONS.len());
        assert_eq!(graph.edge_count(), CALLS.len());
        assert!(graph.find_function_by_name("main").is_some());

        let error_func = graph.find_function_by_name("handle_client").unwrap();
        assert!(error_func
            .diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::ERROR));

        let shared = graph.find_function_by_name("log_write").unwrap();
        assert!(shared.references.len() > 5);
    }

    #[test]
    fn test_every_call_names_a_defined_function() {
        let defined: std::collections::HashSet<&str> = FUNCTIONS.iter().map(|f| f.name).collect();
        for (caller, callee) in CALLS {
            assert!(defined.contains(caller), "undefined caller {caller}");
            assert!(defined.contains(callee), "undefined callee {callee}");
        }
        for d in DIAGNOSTICS {
            assert!(
                defined.contains(d.func),
                "diagnostic on undefined {}",
                d.func
            );
        }
        for (callee, _, _) in NON_CALL_REFERENCES {
            assert!(defined.contains(callee), "reference to undefined {callee}");
        }
    }

    #[test]
    fn test_main_function_relationships() {
        let graph = create_demo_call_graph();
        let main_func = graph.find_function_by_name("main").unwrap();

        let callees = graph.get_callees(&main_func.id);
        assert!(callees.len() >= 6);

        let callers = graph.get_callers(&main_func.id);
        assert_eq!(callers.len(), 0);
    }

    #[test]
    fn test_shared_leaf_has_many_callers() {
        let graph = create_demo_call_graph();

        let xmalloc = graph.find_function_by_name("xmalloc").unwrap();
        assert!(graph.get_callers(&xmalloc.id).len() >= 5);
        assert_eq!(graph.get_callees(&xmalloc.id).len(), 1); // oom_die

        let oom_die = graph.find_function_by_name("oom_die").unwrap();
        assert!(graph.get_callees(&oom_die.id).is_empty());
    }

    #[test]
    fn test_recursion_and_cycles_are_present() {
        let graph = create_demo_call_graph();

        let parse_token = graph.find_function_by_name("parse_token").unwrap();
        assert!(graph
            .get_callees(&parse_token.id)
            .iter()
            .any(|f| f.id == parse_token.id));

        let insert = graph.find_function_by_name("index_insert").unwrap();
        let rehash = graph.find_function_by_name("index_rehash").unwrap();
        assert!(graph
            .get_callees(&insert.id)
            .iter()
            .any(|f| f.id == rehash.id));
        assert!(graph
            .get_callees(&rehash.id)
            .iter()
            .any(|f| f.id == insert.id));
    }

    #[test]
    fn test_references_carry_the_calling_symbol() {
        let graph = create_demo_call_graph();
        let hash_key = graph.find_function_by_name("hash_key").unwrap();

        let callers = hash_key.get_referencing_function_names();
        assert!(callers.contains(&"index_lookup"));
        assert!(callers.contains(&"index_insert"));

        // The struct-field reference is not reported as a calling function.
        assert!(!hash_key
            .get_referencing_symbols_by_kind(ReferenceSymbolKind::Struct)
            .is_empty());
    }

    #[test]
    fn test_call_sites_fall_inside_the_calling_function() {
        let specs: HashMap<&str, &FnSpec> = FUNCTIONS.iter().map(|f| (f.name, f)).collect();
        let mut nth: HashMap<&str, u32> = HashMap::new();
        let mut seen: std::collections::HashSet<(String, u32, u32)> =
            std::collections::HashSet::new();

        for (caller, _) in CALLS {
            let spec = specs[caller];
            let n = nth.entry(caller).or_insert(0);
            let loc = call_site(spec, *n);
            *n += 1;

            assert_eq!(loc.file_path, spec.file);
            assert!(loc.line > spec.line, "call site above {caller}");
            // Every call site in a given caller is distinct.
            assert!(
                seen.insert((loc.file_path, loc.line, loc.column)),
                "duplicate call site in {caller}"
            );
        }
    }
}
