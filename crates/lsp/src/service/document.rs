use super::worker::LspWorkerState;
use anyhow::Result;
use lsp_types::Url;
use std::collections::VecDeque;

/// How many documents clangd is allowed to hold open at once.
///
/// Every open document costs clangd a live AST worker and a resident preamble
/// index, so the set of open documents — not the project's size — is what
/// dominates the server's memory. Opening a whole compilation database and
/// leaving it open runs to gigabytes on a 217-file tree like jemalloc.
///
/// It is a ceiling on memory, not a correctness bound. A document with a
/// request still outstanding against it — a prepare or a find-references
/// clangd has not answered — is *pinned* and never the one evicted: closing it
/// would answer that request against a file clangd no longer has ("trying to
/// get AST for non-added document"), and silently, since the symbol merely
/// comes out with no edges. The pin is what makes that impossible; this number
/// is only how many documents sit idle before the oldest goes. Because pinned
/// documents are skipped, the set can run over the limit by the number of
/// requests in flight until they are answered, which is why `core` keeps its
/// warmup concurrency well under it: see the `const` assertion next to
/// `WARMUP_IN_FLIGHT` in `core::app`, which is why this is public.
pub const OPEN_DOCUMENT_LIMIT: usize = 32;

/// The documents clangd currently has open, least-recently-used first.
///
/// A `VecDeque` rather than a set: at this size the linear scan is cheaper than
/// keeping a second structure in step, and the order *is* the eviction policy.
#[derive(Debug, Default)]
pub(super) struct OpenDocuments {
    lru: VecDeque<Url>,
}

impl OpenDocuments {
    pub(super) fn contains(&self, uri: &Url) -> bool {
        self.lru.contains(uri)
    }

    pub(super) fn len(&self) -> usize {
        self.lru.len()
    }

    /// Mark `uri` as the most recently used document.
    fn touch(&mut self, uri: &Url) {
        if let Some(idx) = self.lru.iter().position(|open| open == uri) {
            let uri = self
                .lru
                .remove(idx)
                .expect("index came from the same deque");
            self.lru.push_back(uri);
        }
    }

    fn insert(&mut self, uri: Url) {
        self.lru.push_back(uri);
    }

    /// The next document to close, or `None` once the set is within its limit
    /// — or when everything over it is pinned.
    ///
    /// Least recently used first, skipping any document `pinned` says has a
    /// request outstanding: those are the ones clangd is about to read, and
    /// a `didClose` overtaking that read answers it with an error. An overflow
    /// made entirely of pinned documents is left alone rather than forced, and
    /// the next open tries again; the pins are as many as the requests in
    /// flight, so the overshoot is bounded by whoever is sending them.
    fn evict_overflow(&mut self, pinned: impl Fn(&Url) -> bool) -> Option<Url> {
        if self.lru.len() <= OPEN_DOCUMENT_LIMIT {
            return None;
        }
        let idx = self.lru.iter().position(|uri| !pinned(uri))?;
        self.lru.remove(idx)
    }

    /// Forget every open document and hand them back to be closed.
    fn take_all(&mut self) -> Vec<Url> {
        self.lru.drain(..).collect()
    }
}

/// Close every document clangd has open.
///
/// Frees the AST workers and resident preambles; it does not take anything out
/// of clangd's merged index. A preamble publishes its document's view of every
/// header it includes, and `didClose` does not retract it — the view was
/// measured still standing eight seconds after the close — so closing is a
/// memory measure and never a way to change what `workspace/symbol` answers.
/// The loader no longer calls it: it opens one file and leaves it open.
pub(super) async fn close_all_documents(state: &mut LspWorkerState) {
    let uris = state.opened_documents.take_all();
    log::info!("Closing {} open document(s)", uris.len());

    for uri in uris {
        if let Err(e) = state.client.did_close(uri.clone()).await {
            log::warn!("Failed to close document {}: {}", uri, e);
        }
    }
}

pub(super) async fn ensure_document_opened(
    state: &mut LspWorkerState,
    document_uri: &Url,
) -> Result<()> {
    if state.opened_documents.contains(document_uri) {
        log::debug!("Document already opened: {}", document_uri);
        state.opened_documents.touch(document_uri);
        return Ok(());
    }

    log::info!("Opening document in LSP server: {}", document_uri);

    let file_path = document_uri
        .to_file_path()
        .map_err(|_| anyhow::anyhow!("Invalid file URI: {}", document_uri))?;

    let content = tokio::fs::read_to_string(&file_path)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read file {}: {}", file_path.display(), e))?;

    log::debug!(
        "Read file content: {} bytes from {}",
        content.len(),
        file_path.display()
    );

    // Advisory only: clangd takes the real language from the file's
    // `compile_commands.json` entry — or, for a header, from the entry it
    // infers from a similar translation unit — so this is what we claim, not
    // what gets parsed. It is still worth getting right, because a server that
    // *does* honour `languageId` parses a C++ header as C and reports every
    // template as a syntax error.
    //
    // `.hpp` and `.hxx` are C++ by construction — nobody writes a C header
    // with either. `.h` is the genuinely ambiguous one and stays `c`: in a C
    // project it is a C header, in a C++ project it usually is not, and the
    // extension cannot tell the two apart. Since the compile database decides
    // anyway, the honest answer is to keep the answer this tool has always
    // given rather than to guess the other way.
    let language_id = match file_path.extension().and_then(|ext| ext.to_str()) {
        Some("rs") => "rust",
        Some("c") | Some("h") => "c",
        Some("cpp") | Some("cxx") | Some("cc") => "cpp",
        Some("hpp") | Some("hxx") => "cpp",
        Some("py") => "python",
        Some("js") => "javascript",
        Some("ts") => "typescript",
        Some("java") => "java",
        Some("go") => "go",
        _ => "plaintext",
    };

    log::info!(
        "Sending didOpen notification: uri={}, language_id={}, version=1",
        document_uri,
        language_id
    );

    state
        .client
        .did_open(document_uri.clone(), language_id, 1, content)
        .await?;
    state.opened_documents.insert(document_uri.clone());

    log::info!(
        "Successfully opened document: {} (total opened: {})",
        document_uri,
        state.opened_documents.len()
    );

    // Only the pinned documents survive their turn: see `evict_overflow`, and
    // `LspWorkerState::request_documents` for what pins one. Copied out
    // first because the LRU is borrowed mutably for the eviction, and the set
    // is a handful of entries.
    let pinned: Vec<Url> = state.request_documents.values().cloned().collect();
    while let Some(evicted) = state
        .opened_documents
        .evict_overflow(|uri| pinned.contains(uri))
    {
        log::debug!("Closing least-recently-used document: {}", evicted);
        if let Err(e) = state.client.did_close(evicted.clone()).await {
            log::warn!("Failed to close document {}: {}", evicted, e);
        }
    }
    if state.opened_documents.len() > OPEN_DOCUMENT_LIMIT {
        log::debug!(
            "{} document(s) open, over the limit of {}: the rest have requests outstanding",
            state.opened_documents.len(),
            OPEN_DOCUMENT_LIMIT
        );
    }

    Ok(())
}

/// The chain of symbols containing `position`, outermost first.
///
/// The last entry is the symbol the position is inside; everything before it is
/// that symbol's scope. Both are needed, because a `DocumentSymbol` carries no
/// qualified name of its own: its scope *is* the tree it sits in.
pub(super) fn find_containing_symbol_path<'a>(
    symbols: &'a [lsp_types::DocumentSymbol],
    position: &lsp_types::Position,
) -> Vec<&'a lsp_types::DocumentSymbol> {
    // The tightest enclosing sibling, not the first one that matches. A server
    // that answers with the flat `SymbolInformation` form sends the namespace,
    // the class and the method as siblings, each spanning the next — so taking
    // the first match would report a reference as living in the outermost
    // namespace. In a properly nested reply there is only ever one match, and
    // this picks it.
    let Some(symbol) = symbols
        .iter()
        .filter(|symbol| position_in_range(position, &symbol.range))
        .min_by_key(|symbol| range_span(&symbol.range))
    else {
        return Vec::new();
    };

    let mut path = vec![symbol];
    if let Some(children) = &symbol.children {
        path.extend(find_containing_symbol_path(children, position));
    }
    path
}

/// How much of the document a range covers, for comparing two that both
/// enclose the same position. Lines dominate; the column only breaks ties
/// within one line.
fn range_span(range: &lsp_types::Range) -> (u32, u32) {
    (
        range.end.line.saturating_sub(range.start.line),
        range.end.character.saturating_sub(range.start.character),
    )
}

pub(super) fn position_in_range(position: &lsp_types::Position, range: &lsp_types::Range) -> bool {
    (position.line > range.start.line
        || (position.line == range.start.line && position.character >= range.start.character))
        && (position.line < range.end.line
            || (position.line == range.end.line && position.character <= range.end.character))
}

/// The qualified name of the symbol a `find_containing_symbol_path` ended on.
///
/// Built from the enclosing symbols, which is the only place the scope is
/// recorded. It used to be built from `detail`, but a `DocumentSymbol`'s detail
/// is its *signature* — clangd sends "void ()" for a function and "struct" for
/// a class — so that spelled a global function "void ()::globalFn". An absent
/// detail fared no better: "::globalFn", a leading separator standing in for a
/// scope that simply isn't there.
///
/// Names already carrying their own scope are left as they are: clangd reports
/// an out-of-line definition as `Widget::paint` under the namespace it is
/// written in, so joining the chain yields `outer::inner::Widget::paint` — the
/// same name the in-class declaration gets.
pub(super) fn qualified_name_of(path: &[&lsp_types::DocumentSymbol]) -> String {
    path.iter()
        .map(|symbol| symbol.name.as_str())
        .collect::<Vec<_>>()
        .join("::")
}

pub(super) fn convert_lsp_symbol_kind(kind: lsp_types::SymbolKind) -> model::ReferenceSymbolKind {
    match kind {
        lsp_types::SymbolKind::FUNCTION => model::ReferenceSymbolKind::Function,
        lsp_types::SymbolKind::METHOD => model::ReferenceSymbolKind::Method,
        lsp_types::SymbolKind::CONSTRUCTOR => model::ReferenceSymbolKind::Constructor,
        lsp_types::SymbolKind::VARIABLE => model::ReferenceSymbolKind::Variable,
        _ => model::ReferenceSymbolKind::Function,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `DocumentSymbol` spanning whole lines, with clangd's `detail`.
    #[allow(deprecated)] // `DocumentSymbol::deprecated` is deprecated but required.
    fn sym(
        name: &str,
        kind: lsp_types::SymbolKind,
        detail: &str,
        lines: std::ops::Range<u32>,
        children: Vec<lsp_types::DocumentSymbol>,
    ) -> lsp_types::DocumentSymbol {
        let range = lsp_types::Range {
            start: lsp_types::Position {
                line: lines.start,
                character: 0,
            },
            end: lsp_types::Position {
                line: lines.end,
                character: 0,
            },
        };
        lsp_types::DocumentSymbol {
            name: name.to_string(),
            detail: Some(detail.to_string()),
            kind,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: if children.is_empty() {
                None
            } else {
                Some(children)
            },
        }
    }

    /// The shape clangd actually returns, taken from a real response: nested
    /// namespaces, a class, and an out-of-line definition that carries part of
    /// its own scope. Note every `detail` is a signature, never a name.
    fn document() -> Vec<lsp_types::DocumentSymbol> {
        use lsp_types::SymbolKind;
        vec![
            sym(
                "outer",
                SymbolKind::NAMESPACE,
                "",
                0..10,
                vec![sym(
                    "inner",
                    SymbolKind::NAMESPACE,
                    "",
                    1..9,
                    vec![
                        sym(
                            "Widget",
                            SymbolKind::CLASS,
                            "struct",
                            2..5,
                            vec![sym("paint", SymbolKind::METHOD, "void ()", 3..4, vec![])],
                        ),
                        sym("Widget::paint", SymbolKind::METHOD, "void ()", 6..7, vec![]),
                    ],
                )],
            ),
            sym("globalFn", SymbolKind::FUNCTION, "void ()", 11..12, vec![]),
        ]
    }

    fn name_at(line: u32) -> String {
        let position = lsp_types::Position { line, character: 0 };
        qualified_name_of(&find_containing_symbol_path(&document(), &position))
    }

    #[test]
    fn a_method_is_named_by_the_scope_it_sits_in() {
        // Not "void ()::paint": a document symbol's detail is its signature,
        // and the scope is the tree it hangs in.
        assert_eq!(name_at(3), "outer::inner::Widget::paint");
    }

    #[test]
    fn an_out_of_line_definition_agrees_with_its_declaration() {
        // clangd names this one `Widget::paint` and files it under the
        // namespace it is written in, so the scope must not be added twice.
        assert_eq!(name_at(6), "outer::inner::Widget::paint");
    }

    #[test]
    fn a_function_at_global_scope_has_no_scope_to_show() {
        // Not "::globalFn": there is no enclosing scope to separate it from.
        assert_eq!(name_at(11), "globalFn");
    }

    /// The flat form: the same symbols as siblings, each range spanning the
    /// next, which is what a server without hierarchical support sends.
    fn flat_document() -> Vec<lsp_types::DocumentSymbol> {
        use lsp_types::SymbolKind;
        vec![
            sym("outer", SymbolKind::NAMESPACE, "", 0..10, vec![]),
            sym("inner", SymbolKind::NAMESPACE, "outer", 1..9, vec![]),
            sym("Widget", SymbolKind::CLASS, "outer::inner", 2..5, vec![]),
            sym(
                "paint",
                SymbolKind::METHOD,
                "outer::inner::Widget",
                3..4,
                vec![],
            ),
        ]
    }

    #[test]
    fn a_flat_reply_still_finds_the_innermost_symbol() {
        // Taking the first match here reported every reference as living in
        // the outermost namespace, whatever function it was actually in.
        let position = lsp_types::Position {
            line: 3,
            character: 0,
        };
        let flat = flat_document();
        let path = find_containing_symbol_path(&flat, &position);
        assert_eq!(path.len(), 1, "a flat reply has no nesting to walk");
        assert_eq!(path[0].name, "paint");
    }

    #[test]
    fn a_position_inside_nothing_yields_no_path() {
        let position = lsp_types::Position {
            line: 500,
            character: 0,
        };
        assert!(find_containing_symbol_path(&document(), &position).is_empty());
    }

    fn uri(n: usize) -> Url {
        Url::parse(&format!("file:///tmp/file_{n}.c")).unwrap()
    }

    fn fill(count: usize) -> OpenDocuments {
        let mut open = OpenDocuments::default();
        for n in 0..count {
            open.insert(uri(n));
        }
        open
    }

    #[test]
    fn stays_within_its_limit_by_dropping_the_oldest() {
        let mut open = fill(OPEN_DOCUMENT_LIMIT + 3);

        let evicted: Vec<Url> = std::iter::from_fn(|| open.evict_overflow(|_| false)).collect();

        assert_eq!(evicted, vec![uri(0), uri(1), uri(2)]);
        assert_eq!(open.len(), OPEN_DOCUMENT_LIMIT);
        assert!(open.contains(&uri(OPEN_DOCUMENT_LIMIT + 2)));
    }

    #[test]
    fn a_touched_document_outlives_the_ones_opened_after_it() {
        let mut open = fill(OPEN_DOCUMENT_LIMIT);
        open.touch(&uri(0));
        open.insert(uri(OPEN_DOCUMENT_LIMIT));

        assert_eq!(open.evict_overflow(|_| false), Some(uri(1)));
        assert!(
            open.contains(&uri(0)),
            "the touched document should survive"
        );
    }

    #[test]
    fn nothing_is_evicted_below_the_limit() {
        let mut open = fill(OPEN_DOCUMENT_LIMIT);
        assert_eq!(open.evict_overflow(|_| false), None);
    }

    /// A document clangd is about to read for a request still in flight is
    /// not the one to close, however old it is: the `didClose` would overtake
    /// the read and answer it with "non-added document".
    #[test]
    fn a_document_with_a_request_outstanding_is_skipped_over() {
        let mut open = fill(OPEN_DOCUMENT_LIMIT + 1);
        let oldest = uri(0);

        assert_eq!(
            open.evict_overflow(|uri| *uri == oldest),
            Some(uri(1)),
            "the oldest *idle* document goes instead"
        );
        assert!(open.contains(&oldest));
        assert_eq!(open.len(), OPEN_DOCUMENT_LIMIT);
    }

    /// An overflow made entirely of pinned documents is left standing rather
    /// than forced: the overshoot lasts exactly as long as the requests do.
    #[test]
    fn an_overflow_of_pinned_documents_is_left_alone() {
        let mut open = fill(OPEN_DOCUMENT_LIMIT + 2);

        assert_eq!(open.evict_overflow(|_| true), None);
        assert_eq!(open.len(), OPEN_DOCUMENT_LIMIT + 2, "nothing was closed");
        // And goes the moment the pins do.
        assert_eq!(open.evict_overflow(|_| false), Some(uri(0)));
    }

    #[test]
    fn taking_all_leaves_the_set_empty() {
        let mut open = fill(3);

        assert_eq!(open.take_all(), vec![uri(0), uri(1), uri(2)]);
        assert_eq!(open.len(), 0);
        assert!(!open.contains(&uri(0)));
    }
}
