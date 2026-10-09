use serde::{Deserialize, Serialize};

/// Phases of LSP loading lifecycle for UI feedback
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LspLoadPhase {
    NotStarted,
    SpawningServer,
    Initializing,
    Initialized,
    DiscoveringFiles,
    PreloadingDocuments {
        done: usize,
        total: usize,
    },
    /// clangd is building its background index. Nothing can be asked of
    /// `workspace/symbol` until this finishes without getting back a sample of
    /// the project rather than the project.
    IndexingProject {
        done: usize,
        total: usize,
    },
    LoadingWorkspaceSymbols {
        loaded: usize,
    },
    Completed,
    Failed(String),
}

/// Messages sent from the background LSP loader into the TUI
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LspUiMessage {
    Progress(LspLoadPhase),
    AddFunction(crate::WorkspaceSymbolInfo),
    /// A finished call graph, functions and edges together: the whole of it,
    /// replacing whatever the app held — a reload's graph included, so what
    /// was deleted from the source is gone.
    ///
    /// What a backend sends that reads the whole build before it answers — the
    /// libclang indexer — rather than discovering symbols and asking for their
    /// edges one at a time. Boxed: a graph is large and every other message is
    /// a few words.
    AddGraph(Box<crate::CallGraph>),
    /// How much of the build a backend that parses it managed to read.
    ///
    /// Sent before `Completed` by the libclang indexer, and never by clangd,
    /// whose index has no such count to give. Without it a graph built from
    /// half the build looks exactly like one built from all of it.
    ParseReport(ParseSummary),
}

/// What a parse of the whole build came to, per translation unit.
///
/// Counted in TUs rather than diagnostics: "40 files with errors" says how much
/// of the project is in doubt, where "1,200 errors" may be one bad header
/// reported by every file that includes it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseSummary {
    /// TUs read, including those with errors.
    pub parsed: usize,
    /// TUs libclang could not parse at all, so contributed nothing.
    pub failed: usize,
    /// Parsed TUs with at least one error, whose bodies may have been read
    /// incompletely — usually a missing include or define.
    pub with_errors: usize,
    /// Calls left out of the graph because their callee has no body in the
    /// build and [`BodilessCallees`] did not keep it: libc and system headers,
    /// or with `Drop` also what a project header declares and nothing defines.
    pub external_calls: usize,
    /// Compiler arguments libclang refused and parsed without.
    pub rejected_arguments: Vec<String>,
    /// Of `parsed`, TUs whose result was reused from the on-disk cache because
    /// nothing they read had changed.
    #[serde(default)]
    pub cached: usize,
}

/// Which calls to a function with no body in the build become edges.
///
/// A backend that parses the build knows which functions it defines, and a call
/// whose callee it never saw a body for — libc, a system header, or a function
/// a project header declares and no TU of this database compiles — has nothing
/// behind it to draw but the declaration. Whether to draw that is a question
/// about what the reader wants to see, not about the code, so it is a setting:
/// jemalloc's unit tests call `jet_mallctl`, which only a `-DJEMALLOC_JET`
/// compile of `src/` defines, and a database without that compile either shows
/// the call ending at `jemalloc.h` or does not show it at all.
///
/// The callee becomes a leaf at its declaration, since nothing says what it
/// calls. clangd answers the middle one by construction (its call replies are
/// held to the project root), which is why that is the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BodilessCallees {
    /// Only calls between functions the build defines.
    Drop,
    /// Also calls to functions declared under the project root.
    #[default]
    Project,
    /// Also calls to everything else the build declares: libc, system headers.
    All,
}

impl BodilessCallees {
    pub const ALL: [Self; 3] = [Self::Drop, Self::Project, Self::All];

    /// Whether a callee with no body becomes a node, given whether its
    /// declaration is under the project root.
    pub fn keeps(self, in_project: bool) -> bool {
        match self {
            Self::Drop => false,
            Self::Project => in_project,
            Self::All => true,
        }
    }
}

impl std::str::FromStr for BodilessCallees {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "drop" => Ok(Self::Drop),
            "project" => Ok(Self::Project),
            "all" => Ok(Self::All),
            other => Err(format!(
                "`{other}` is not a choice; expected `drop`, `project` or `all`"
            )),
        }
    }
}

/// How far a [`ParseSummary`] should be trusted, for how loudly to show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParseQuality {
    /// Every TU parsed without an error.
    Clean,
    /// Some TUs failed or had errors, but most of the build was read cleanly.
    Partial,
    /// Fewer than half the TUs were read cleanly: the graph is a fragment, and
    /// the cause is usually the build's arguments rather than its code.
    Poor,
}

impl ParseSummary {
    /// Every TU the build asked for, parsed or not.
    pub fn total(&self) -> usize {
        self.parsed + self.failed
    }

    /// TUs parsed with no error at all.
    pub fn clean(&self) -> usize {
        self.parsed.saturating_sub(self.with_errors)
    }

    /// Errors count against the build as well as failures, because the one
    /// misconfiguration that is common — libclang not finding its own builtin
    /// headers, or an include path the command line does not carry — fails no
    /// TU outright and puts an error in every one of them.
    pub fn quality(&self) -> ParseQuality {
        if self.failed == 0 && self.with_errors == 0 && self.parsed > 0 {
            ParseQuality::Clean
        } else if self.clean() * 2 >= self.total() && self.parsed > 0 {
            ParseQuality::Partial
        } else {
            ParseQuality::Poor
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(parsed: usize, failed: usize, with_errors: usize) -> ParseSummary {
        ParseSummary {
            parsed,
            failed,
            with_errors,
            ..ParseSummary::default()
        }
    }

    #[test]
    fn a_build_read_without_an_error_is_clean() {
        assert_eq!(summary(217, 0, 0).quality(), ParseQuality::Clean);
    }

    #[test]
    fn a_few_failures_or_errors_are_partial() {
        assert_eq!(summary(212, 5, 0).quality(), ParseQuality::Partial);
        assert_eq!(summary(217, 0, 40).quality(), ParseQuality::Partial);
        // Exactly half read cleanly is still most of a graph.
        assert_eq!(summary(10, 0, 5).quality(), ParseQuality::Partial);
    }

    #[test]
    fn mostly_failed_or_mostly_errors_is_poor() {
        assert_eq!(summary(12, 205, 0).quality(), ParseQuality::Poor);
        // Nothing failed, but every file had errors: a missing builtin header.
        assert_eq!(summary(217, 0, 217).quality(), ParseQuality::Poor);
        assert_eq!(summary(0, 0, 0).quality(), ParseQuality::Poor);
    }
}
