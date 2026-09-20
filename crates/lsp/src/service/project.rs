//! What counts as project code.
//!
//! Two kinds of reply name a file: `workspace/symbol`, which is where the
//! project's functions are discovered, and the `outgoingCalls`/`incomingCalls`
//! that follow a prepare, whose items name the function on the other end of
//! each call. Both have to be held to the same rule, or the graph is a mix of
//! two: the discovery query would refuse a file that a call reply then walked
//! straight in through.
//!
//! The rule is the project **root**, not the compilation database. A database
//! lists translation units, and a C project's headers are not among them —
//! jemalloc defines ~736 functions in `include/jemalloc/internal/*.h`, all of
//! them project code the user means to see. What the rule is for is the other
//! end: libc and the system headers, which arrive only through call replies
//! (`ctl_lookup -> strchr`, in `/usr/include/string.h`) and are not the program
//! under analysis. They also *move*: which declaration clangd resolves such a
//! callee to depends on the live view of the headers, so leaving them in made
//! the edge count differ between two runs over an unchanged tree.

use lsp_types::Url;
use std::path::{Component, Path, PathBuf};

/// The project's root directory, and the one question asked of every path
/// clangd names: does it lie under that directory?
#[derive(Debug, Default)]
pub(super) struct ProjectScope {
    /// `None` until the loader says where the project is, and everything is
    /// kept until it does — an unbounded scope filters nothing, which is what
    /// the tests and the demo path want.
    root: Option<PathBuf>,
}

impl ProjectScope {
    /// Bound the scope to `root`.
    ///
    /// Canonicalised here as well as by the loader, because a root that is not
    /// the same spelling as the paths clangd reports rejects the whole project.
    /// The candidate paths are *not* canonicalised — that is a syscall per
    /// symbol over the ~19,600 a jemalloc query answers with, for URIs clangd
    /// has already made absolute — so they are only normalised lexically, which
    /// resolves the `..` an `-I../include` can leave behind.
    pub(super) fn new(root: PathBuf) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root: Some(normalise(&root)),
        }
    }

    /// Is `path` project code?
    pub(super) fn contains_path(&self, path: &str) -> bool {
        match &self.root {
            None => true,
            Some(root) => normalise(Path::new(path)).starts_with(root),
        }
    }

    /// Is the file `uri` names project code?
    pub(super) fn contains_uri(&self, uri: &Url) -> bool {
        match uri.to_file_path() {
            Ok(path) => match &self.root {
                None => true,
                Some(root) => normalise(&path).starts_with(root),
            },
            // Not a `file:` URI at all, so not a file in this project.
            // Anything clangd names a callee in lives on disk.
            Err(()) => self.root.is_none(),
        }
    }
}

/// Resolve `.` and `..` without touching the filesystem.
///
/// Purely textual, so it does not follow a symlink — a `..` out of a symlinked
/// directory lands somewhere else than this says. Paths that reach here come
/// from clangd's own URIs and are already absolute and mostly already clean;
/// this is for the `..` an include path can leave in one.
fn normalise(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                // A `..` above the root of an absolute path is nothing; on a
                // relative path there is nothing to pop it against, so keep it.
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> ProjectScope {
        // A root that does not exist, so `canonicalize` fails and the path is
        // taken as given: the test is about the rule, not about this disk.
        ProjectScope::new(PathBuf::from("/home/u/jemalloc"))
    }

    #[test]
    fn a_source_file_under_the_root_is_project_code() {
        assert!(scope().contains_path("/home/u/jemalloc/src/arena.c"));
    }

    #[test]
    fn a_header_under_the_root_is_project_code() {
        // The reason the rule is the root and not the compilation database:
        // this file is in no translation unit's place in the database, and
        // jemalloc defines ~736 functions in files like it.
        assert!(scope().contains_path("/home/u/jemalloc/include/jemalloc/internal/tsd.h"));
    }

    #[test]
    fn a_system_header_is_not() {
        assert!(!scope().contains_path("/usr/include/string.h"));
    }

    #[test]
    fn a_sibling_directory_sharing_the_name_is_not() {
        assert!(!scope().contains_path("/home/u/jemalloc-tests/src/arena.c"));
    }

    #[test]
    fn dot_dot_is_resolved_before_the_comparison() {
        let scope = scope();
        assert!(scope.contains_path("/home/u/jemalloc/src/../include/jemalloc/jemalloc.h"));
        assert!(!scope.contains_path("/home/u/jemalloc/../other/src/main.c"));
    }

    #[test]
    fn a_uri_is_judged_by_its_path() {
        let scope = scope();
        assert!(scope.contains_uri(&Url::parse("file:///home/u/jemalloc/src/jemalloc.c").unwrap()));
        assert!(!scope.contains_uri(&Url::parse("file:///usr/include/string.h").unwrap()));
    }

    #[test]
    fn an_unbounded_scope_keeps_everything() {
        let scope = ProjectScope::default();
        assert!(scope.contains_path("/usr/include/string.h"));
        assert!(scope.contains_uri(&Url::parse("file:///usr/include/string.h").unwrap()));
    }
}
