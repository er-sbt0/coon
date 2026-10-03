//! Discovering the whole project's functions without one unbounded query.
//!
//! An empty `workspace/symbol` query asks clangd for every symbol in its
//! index, and clangd builds the entire reply in memory before it sends a byte
//! of it — about 25 KB of its heap per result, a hundred times what the result
//! costs on the wire, and released only once the reply is gone. On jemalloc
//! that is ~20k results and half a gigabyte, which nobody noticed. On the
//! Linux kernel it is 344,769 results: clangd went from 1.2 GB to past 8 GB in
//! five seconds, inside its `WorkspaceSymbols` thread, and took a 14 GB
//! machine down with it twice. The cost follows the number of results and not
//! the size of the index — a 1,000-result query over the same kernel index
//! costs 18 MB — so the fix is to never ask for more than a bounded number at
//! once.
//!
//! So an empty query is answered by a *sweep*: one query per leading
//! character, each capped at [`DISCOVERY_CHUNK`] results by clangd's
//! `--limit-results`, and any query that comes back full is split into one
//! query per next character. clangd's matching is fuzzy — a one-letter query
//! matches every name with a word segment starting with that letter, so `b`
//! returns `foo_bar` as well as `bar` — which means the chunks overlap and
//! the union is deduplicated, but also means every symbol is reachable: a name
//! always matches the query made of its own first characters. Measured on
//! linux-7.2 at 50,000 per chunk: 259 queries, six of them split, 49 s, and
//! exactly the 141,259 project functions the unbounded query found, with
//! clangd peaking at 2.5 GB instead of dying past 8.
//!
//! The queries go out strictly one at a time, because the point is a bound on
//! clangd's peak and two in flight is two chunks' worth of it.

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use model::{FunctionNode, SymbolId};

/// How many results one discovery query may return: clangd's
/// `--limit-results`, and the chunk a sweep splits on.
///
/// This is the whole memory bound — clangd's transient cost is ~25 KB per
/// result, so 50,000 is ~1.25 GB above its baseline. Smaller is safer and
/// slower: every full chunk splits into [`ALPHABET`]`.len()` queries, and
/// fuzzy matching makes those overlap, so halving the chunk roughly doubles
/// the splits. It also caps every other `workspace/symbol` query, which is
/// harmless — nothing else asks for more than a screenful.
pub const DISCOVERY_CHUNK: usize = 50_000;

/// The characters a C or C++ identifier is made of, lowercase only: clangd's
/// fuzzy matcher ignores case, so `A` would repeat `a`.
const ALPHABET: &str = "abcdefghijklmnopqrstuvwxyz0123456789_";

/// Past this many characters a full query is no longer split.
///
/// A guard, not a tuning: on the kernel nothing needed more than three. A
/// prefix still full here means more than [`DISCOVERY_CHUNK`] symbols fuzzily
/// match four characters, and splitting further would only repeat them.
const MAX_PREFIX_LEN: usize = 4;

/// One empty-query discovery in progress.
#[derive(Debug)]
pub(crate) struct SymbolSweep {
    queue: VecDeque<String>,
    seen: HashSet<SymbolId>,
    functions: Vec<FunctionNode>,
    queries: usize,
    splits: usize,
    raw_results: usize,
    started: Instant,
}

impl SymbolSweep {
    pub(crate) fn new() -> Self {
        Self {
            queue: ALPHABET.chars().map(String::from).collect(),
            seen: HashSet::new(),
            functions: Vec::new(),
            queries: 0,
            splits: 0,
            raw_results: 0,
            started: Instant::now(),
        }
    }

    /// The next query to send, or `None` once the sweep is done.
    pub(crate) fn next_query(&mut self) -> Option<String> {
        self.queue.pop_front()
    }

    /// Take in the reply to `query`: `raw_count` is how many symbols clangd
    /// returned before any filtering — the number the chunk limit applies to —
    /// and `functions` is what of them is kept.
    ///
    /// A reply that filled its chunk was cut off by clangd, so the query is
    /// split: one query per next character, which between them match
    /// everything the cut-off one did and more narrowly.
    pub(crate) fn absorb(&mut self, query: &str, raw_count: usize, functions: Vec<FunctionNode>) {
        self.queries += 1;
        self.raw_results += raw_count;
        for function in functions {
            if self.seen.insert(function.id) {
                self.functions.push(function);
            }
        }
        if raw_count >= DISCOVERY_CHUNK {
            if query.len() < MAX_PREFIX_LEN {
                self.splits += 1;
                self.queue
                    .extend(ALPHABET.chars().map(|c| format!("{query}{c}")));
            } else {
                log::warn!(
                    "discovery: {query:?} still returns a full chunk of {DISCOVERY_CHUNK} at \
                     {MAX_PREFIX_LEN} characters; some symbols matching it may be missing"
                );
            }
        }
    }

    /// How many distinct functions have been found so far.
    pub(crate) fn found(&self) -> usize {
        self.functions.len()
    }

    /// The functions found, and a line for the log saying what it took.
    pub(crate) fn finish(self) -> (Vec<FunctionNode>, String) {
        let summary = format!(
            "discovery sweep: {} function(s) from {} quer{} ({} split), {} raw result(s), {:.1}s",
            self.functions.len(),
            self.queries,
            if self.queries == 1 { "y" } else { "ies" },
            self.splits,
            self.raw_results,
            self.started.elapsed().as_secs_f32()
        );
        (self.functions, summary)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn function(name: &str) -> FunctionNode {
        FunctionNode::new(
            name.to_string(),
            name.to_string(),
            model::Location {
                file_path: "/p/a.c".to_string(),
                line: 1,
                column: 0,
                length: None,
            },
        )
    }

    fn drain(sweep: &mut SymbolSweep) -> Vec<String> {
        std::iter::from_fn(|| sweep.next_query()).collect()
    }

    #[test]
    fn starts_with_one_query_per_identifier_character() {
        let mut sweep = SymbolSweep::new();
        let queries = drain(&mut sweep);
        assert_eq!(queries.len(), ALPHABET.len());
        assert_eq!(queries.first().map(String::as_str), Some("a"));
        assert!(queries.contains(&"_".to_string()));
    }

    #[test]
    fn a_full_reply_is_split_by_the_next_character() {
        let mut sweep = SymbolSweep::new();
        drain(&mut sweep);

        sweep.absorb("s", DISCOVERY_CHUNK, vec![]);

        let children = drain(&mut sweep);
        assert_eq!(children.len(), ALPHABET.len());
        assert!(children.iter().all(|q| q.len() == 2 && q.starts_with('s')));
    }

    #[test]
    fn a_reply_under_the_chunk_is_complete_and_not_split() {
        let mut sweep = SymbolSweep::new();
        drain(&mut sweep);

        sweep.absorb("j", DISCOVERY_CHUNK - 1, vec![]);

        assert!(sweep.next_query().is_none());
    }

    /// The chunks overlap by design: a fuzzy one-letter query matches any
    /// word segment, so the same function comes back from several of them.
    #[test]
    fn a_function_found_by_several_queries_is_kept_once() {
        let mut sweep = SymbolSweep::new();
        sweep.absorb("f", 10, vec![function("foo_bar"), function("foo")]);
        sweep.absorb("b", 10, vec![function("foo_bar")]);

        let (functions, _) = sweep.finish();
        let names: Vec<&str> = functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["foo_bar", "foo"]);
    }

    #[test]
    fn splitting_stops_at_the_prefix_cap() {
        let mut sweep = SymbolSweep::new();
        drain(&mut sweep);

        sweep.absorb("abcd", DISCOVERY_CHUNK, vec![]);

        assert!(sweep.next_query().is_none(), "four characters is the last split");
    }
}
