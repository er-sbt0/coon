//! The edge warmup's queue: which symbol is asked about next, and in what
//! form the question goes out.
//!
//! A symbol's edges are one `callHierarchy/outgoingCalls` request, which
//! clangd answers from its index in a few milliseconds and for which it needs
//! nothing but the `CallHierarchyItem` naming the symbol. It is *getting the
//! item* that is expensive: `textDocument/prepareCallHierarchy` runs against
//! the file's AST, so it opens the file (a preamble build, a quarter of a
//! second) and parses it. On jemalloc the prepares were 574 of the 623
//! seconds clangd spent on the warmup, and the follow-ups 49. But a prepare
//! is not the only source of items — every follow-up reply carries one for
//! each callee it names, and the item clangd hands back for a callee is the
//! same item it would produce by preparing that callee's definition. So the
//! queue keeps two pools. A symbol with an item in hand is *ready*, and goes
//! out as its follow-up directly; a symbol without one waits for a prepare,
//! and the order those go out in is chosen to make each as cheap as it can
//! be: grouped by file, a few files at a time, so a document is opened once
//! for all of its symbols rather than once for each — the old queue, in hash
//! order, changed file on 95% of consecutive prepares and reopened each of
//! the project's files nine times over.
//!
//! The set of symbols resolved, and the edges each gains, are exactly what
//! they were: the queue only reorders the questions and picks the cheaper of
//! two forms clangd offers for the same one.

use std::collections::{HashMap, HashSet, VecDeque};

use model::SymbolId;

/// The symbols the warmup still has to ask about.
#[derive(Debug, Default)]
pub(crate) struct WarmupQueue {
    /// Every symbol waiting to be sent. The one answer to "is this still
    /// queued": the pools below are lazily cleaned, so a symbol may sit in
    /// one after it has gone out through the other, and membership here is
    /// what is checked at the moment of sending.
    queued: HashSet<SymbolId>,
    /// Symbols whose call-hierarchy item is already in hand, oldest first.
    /// Index-only, so always sent ahead of a prepare.
    ready: VecDeque<SymbolId>,
    /// The same symbols as a set, so that "does this file still need a
    /// prepare" is answered without walking the deque.
    promoted: HashSet<SymbolId>,
    /// The symbols still needing a prepare, by the file they are defined in,
    /// each with the line it is defined on. Sorted bottom-up when a lane
    /// picks the file up: see [`WarmupQueue::next_prepare`].
    by_file: HashMap<String, VecDeque<(u32, SymbolId)>>,
    /// Files no lane has picked up yet. Unordered: a lane takes the one with
    /// the most symbols still needing a prepare, see
    /// [`WarmupQueue::next_prepare`].
    files: Vec<String>,
    /// The files being prepared from right now; the front is the next to
    /// draw on. Rotated after each draw, so the outstanding prepares spread
    /// across the lanes rather than stacking on one file's AST worker.
    lanes: VecDeque<String>,
    /// Files handed to clangd ahead of their turn, so their preambles are
    /// built by the time a lane reaches them, and not yet reached. Kept so
    /// that a file is opened ahead once, not on every pump, and so that the
    /// set of files open ahead is bounded: see [`WarmupQueue::files_to_prefetch`].
    prefetched: HashSet<String>,
}

impl WarmupQueue {
    /// Queue `id`, defined in `file` at `line`, for a prepare. Returns
    /// whether it was new: a symbol already waiting keeps its one place.
    pub(crate) fn push(&mut self, id: SymbolId, file: String, line: u32) -> bool {
        if !self.queued.insert(id) {
            return false;
        }
        let known_file = self.by_file.contains_key(&file) || self.lanes.contains(&file);
        if !known_file {
            self.files.push(file.clone());
        }
        self.by_file.entry(file).or_default().push_back((line, id));
        true
    }

    /// Note that an item for `id` is in hand: if it is still waiting for a
    /// prepare, it no longer needs one. A no-op for a symbol not queued,
    /// which is most of what a reply names.
    pub(crate) fn promote(&mut self, id: SymbolId) {
        if self.queued.contains(&id) && self.promoted.insert(id) {
            self.ready.push_back(id);
        }
    }

    /// The next symbol to send as a follow-up directly, taken off the queue.
    pub(crate) fn next_ready(&mut self) -> Option<SymbolId> {
        let id = self.ready.pop_front()?;
        self.promoted.remove(&id);
        self.queued.remove(&id);
        Some(id)
    }

    /// The next symbol to prepare, taken off the queue, drawn from at most
    /// `lanes` files at once.
    ///
    /// A lane holds a file until every symbol in it has gone — by prepare
    /// from here, or by follow-up from [`WarmupQueue::next_ready`] in the
    /// meantime — and then takes the next file nobody has started on.
    ///
    /// A free lane takes the file with the **most symbols still needing a
    /// prepare**. Every prepare seeds a cascade — its reply names callees,
    /// theirs name more — and the cascade covers most of what it reaches
    /// inside the same file, so the file where a prepare has the most left
    /// to cover is the one to spend it on. The small files, and the ones the
    /// cascade has been through, come last, by which time most of what they
    /// hold has an item already. It is also a *fixed* order, where taking
    /// files as they arrived was the graph's hash order and a different
    /// order every run: the prepare count swung by a fifth between runs of
    /// the same build, and the wall time with it.
    ///
    /// Within a file the prepares run **bottom-up**. C defines before it
    /// uses, so a file's helpers sit above the functions that call them and
    /// its entry points — the functions nothing in the file calls — at the
    /// bottom. A prepare at the bottom therefore seeds a cascade that names
    /// most of what is above it, and those go out as follow-ups before the
    /// lane reaches them; a prepare at the top names nothing the lane has
    /// not already passed. Replayed against the logged replies of two
    /// jemalloc runs this was a third fewer prepares than top-down at every
    /// file order tried.
    pub(crate) fn next_prepare(&mut self, lanes: usize) -> Option<SymbolId> {
        loop {
            while self.lanes.len() < lanes {
                let Some(file) = self.take_widest_file() else {
                    break;
                };
                if let Some(symbols) = self.by_file.get_mut(&file) {
                    symbols
                        .make_contiguous()
                        .sort_by_key(|(line, _)| std::cmp::Reverse(*line));
                }
                self.prefetched.remove(&file);
                self.lanes.push_back(file);
            }
            let file = self.lanes.pop_front()?;
            if let Some(symbols) = self.by_file.get_mut(&file) {
                while let Some((_, id)) = symbols.pop_front() {
                    // Promoted since it was queued here: it is on `ready`
                    // and goes out from there. Gone out already: not again.
                    // Only what still needs a prepare is drawn here.
                    if self.promoted.contains(&id) {
                        continue;
                    }
                    if self.queued.remove(&id) {
                        self.lanes.push_back(file);
                        return Some(id);
                    }
                }
            }
            // Exhausted: the lane is free for the next file, and a symbol of
            // this file that arrives later brings the file back through the
            // queue like any other.
            self.by_file.remove(&file);
        }
    }

    /// How many of `file`'s symbols still need a prepare.
    fn prepares_left_in(&self, file: &str) -> usize {
        self.by_file.get(file).map_or(0, |symbols| {
            symbols
                .iter()
                .filter(|(_, id)| self.queued.contains(id) && !self.promoted.contains(id))
                .count()
        })
    }

    /// The file with the most prepares left, removed from those waiting for
    /// a lane; a file with none left is dropped on the way.
    fn take_widest_file(&mut self) -> Option<String> {
        let widths: Vec<usize> = self
            .files
            .iter()
            .map(|file| self.prepares_left_in(file))
            .collect();
        let mut kept = 0;
        let mut widest: Option<(usize, usize)> = None;
        for (file, width) in std::mem::take(&mut self.files).into_iter().zip(widths) {
            if width == 0 {
                continue;
            }
            if widest.is_none_or(|(_, best)| width > best) {
                widest = Some((kept, width));
            }
            self.files.push(file);
            kept += 1;
        }
        let (idx, _) = widest?;
        Some(self.files.swap_remove(idx))
    }

    /// The files the lanes will reach next that have not been opened ahead
    /// yet, now recorded as opened ahead, keeping the set of files open
    /// ahead of the lanes at `depth`.
    ///
    /// The same order the lanes draw in, so what is opened is what is read
    /// next; a file whose symbols have all been promoted since it was queued
    /// is skipped, since opening it would cost clangd a preamble no prepare
    /// is coming to read. A file opened ahead leaves this set when a lane
    /// takes it, which is what bounds the set: the ranking moves as replies
    /// promote symbols, and without the bound each move would open one more
    /// file than the last.
    pub(crate) fn files_to_prefetch(&mut self, depth: usize) -> Vec<String> {
        let mut ranked: Vec<(usize, &String)> = self
            .files
            .iter()
            .map(|file| (self.prepares_left_in(file), file))
            .filter(|(left, _)| *left > 0)
            .collect();
        ranked.sort_by_key(|(left, file)| (std::cmp::Reverse(*left), (*file).clone()));
        let mut ahead = Vec::new();
        for (_, file) in ranked {
            if self.prefetched.len() + ahead.len() >= depth {
                break;
            }
            if !self.prefetched.contains(file) {
                ahead.push(file.clone());
            }
        }
        self.prefetched.extend(ahead.iter().cloned());
        ahead
    }

    /// Whether a symbol with an item in hand is waiting to go out.
    pub(crate) fn has_ready(&self) -> bool {
        !self.ready.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.queued.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.queued.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, id: &SymbolId) -> bool {
        self.queued.contains(id)
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Every symbol still waiting: the ones needing a prepare in the order
    /// the lanes would reach them, then the ones ready to follow up.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &SymbolId> {
        let mut waiting: Vec<&String> = self.files.iter().collect();
        waiting.sort();
        self.lanes
            .iter()
            .chain(waiting)
            .filter_map(|file| self.by_file.get(file))
            .flatten()
            .map(|(_, id)| id)
            .filter(|id| self.queued.contains(id) && !self.promoted.contains(id))
            .chain(self.ready.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u32) -> SymbolId {
        SymbolId::from_content("f", "test.c", n)
    }

    fn queue(files: &[(&str, &[u32])]) -> WarmupQueue {
        let mut queue = WarmupQueue::default();
        for (file, lines) in files {
            for line in lines.iter() {
                queue.push(id(*line), file.to_string(), *line);
            }
        }
        queue
    }

    fn drain_prepares(queue: &mut WarmupQueue, lanes: usize) -> Vec<SymbolId> {
        std::iter::from_fn(|| queue.next_prepare(lanes)).collect()
    }

    #[test]
    fn prepares_are_grouped_by_file_and_rotate_across_the_lanes() {
        let mut queue = queue(&[("c.c", &[6]), ("a.c", &[1, 2, 3]), ("b.c", &[4, 5])]);

        // Two lanes take the two widest files, a.c and b.c, and alternate,
        // each from the bottom; c.c starts when b.c runs out.
        let order = drain_prepares(&mut queue, 2);
        assert_eq!(order, vec![id(3), id(5), id(2), id(4), id(1), id(6)]);
        assert!(queue.is_empty());
    }

    #[test]
    fn one_lane_is_one_file_at_a_time_from_the_bottom_up() {
        // Queued top-down, as a symbol stream arrives; prepared bottom-up,
        // because the callers are at the bottom.
        let mut queue = queue(&[("a.c", &[1, 2]), ("b.c", &[3])]);
        assert_eq!(drain_prepares(&mut queue, 1), vec![id(2), id(1), id(3)]);
    }

    #[test]
    fn a_lane_takes_the_file_with_the_most_prepares_left() {
        let mut queue = queue(&[("a.c", &[1, 2, 3]), ("b.c", &[4, 5, 6, 7])]);
        // Three of b.c's four have items in hand: a.c has more left to cover.
        queue.promote(id(4));
        queue.promote(id(5));
        queue.promote(id(6));

        assert_eq!(queue.next_prepare(1), Some(id(3)), "a.c first");
        assert_eq!(drain_prepares(&mut queue, 1), vec![id(2), id(1), id(7)]);
    }

    #[test]
    fn a_promoted_symbol_goes_out_as_a_follow_up_and_not_as_a_prepare() {
        let mut queue = queue(&[("a.c", &[1, 2, 3])]);

        queue.promote(id(2));

        assert_eq!(queue.next_ready(), Some(id(2)));
        assert_eq!(queue.next_ready(), None, "and only once");
        assert_eq!(drain_prepares(&mut queue, 1), vec![id(3), id(1)]);
    }

    #[test]
    fn a_symbol_already_sent_through_a_lane_is_not_sent_again_when_its_item_arrives() {
        let mut queue = queue(&[("a.c", &[1])]);
        assert_eq!(queue.next_prepare(1), Some(id(1)));

        queue.promote(id(1));

        assert_eq!(queue.next_ready(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn promoting_twice_sends_once() {
        let mut queue = queue(&[("a.c", &[1])]);
        queue.promote(id(1));
        queue.promote(id(1));

        assert_eq!(queue.next_ready(), Some(id(1)));
        assert_eq!(queue.next_ready(), None);
        assert_eq!(queue.next_prepare(1), None);
    }

    #[test]
    fn a_symbol_queued_twice_holds_one_place() {
        let mut queue = queue(&[("a.c", &[1])]);
        assert!(!queue.push(id(1), "a.c".to_string(), 1));
        assert_eq!(queue.len(), 1);
        assert_eq!(drain_prepares(&mut queue, 1), vec![id(1)]);
    }

    #[test]
    fn the_files_ahead_of_the_lanes_are_named_once_for_opening() {
        // Widths 4, 3, 2, 1: the lane order is a, b, c, d.
        let mut queue = queue(&[
            ("d.c", &[1]),
            ("c.c", &[2, 3]),
            ("b.c", &[4, 5, 6]),
            ("a.c", &[7, 8, 9, 10]),
        ]);
        // One lane takes a.c; the next two are opened ahead.
        assert_eq!(queue.next_prepare(1), Some(id(10)));

        assert_eq!(queue.files_to_prefetch(2), vec!["b.c", "c.c"]);
        assert!(queue.files_to_prefetch(2).is_empty(), "not again");

        // The lane moves on to b.c, and the window slides to d.c.
        assert_eq!(
            drain_prepares_from(&mut queue, 3),
            vec![id(9), id(8), id(7)]
        );
        assert_eq!(queue.next_prepare(1), Some(id(6)));
        assert_eq!(queue.files_to_prefetch(2), vec!["d.c"]);
    }

    /// The first `n` prepares, for walking a lane part of the way.
    fn drain_prepares_from(queue: &mut WarmupQueue, n: usize) -> Vec<SymbolId> {
        (0..n).filter_map(|_| queue.next_prepare(1)).collect()
    }

    #[test]
    fn a_file_whose_symbols_were_all_promoted_is_not_opened_ahead() {
        let mut queue = queue(&[("a.c", &[1]), ("b.c", &[2]), ("c.c", &[3])]);
        queue.promote(id(2));
        assert_eq!(queue.next_prepare(1), Some(id(1)));

        assert_eq!(queue.files_to_prefetch(2), vec!["c.c"]);
    }

    /// The ranking moves as replies promote symbols; the set of files open
    /// ahead of the lanes must not grow with every move.
    #[test]
    fn the_set_of_files_open_ahead_is_bounded_at_its_depth() {
        let mut queue = queue(&[
            ("a.c", &[1, 2, 3, 4]),
            ("b.c", &[5, 6, 7]),
            ("d.c", &[8, 9, 10, 11, 12]),
            ("e.c", &[13]),
        ]);
        assert_eq!(queue.next_prepare(1), Some(id(12)), "d.c is widest");
        assert_eq!(queue.files_to_prefetch(1), vec!["a.c"], "then a.c");

        // a.c loses two symbols to the cascade and b.c is now the widest —
        // but a.c is still open ahead, and the window is one file.
        queue.promote(id(1));
        queue.promote(id(2));
        assert!(queue.files_to_prefetch(1).is_empty());
        assert_eq!(
            drain_prepares_from(&mut queue, 4),
            vec![id(11), id(10), id(9), id(8)]
        );
        assert_eq!(queue.next_prepare(1), Some(id(7)), "the lane takes b.c");
        assert!(
            queue.files_to_prefetch(1).is_empty(),
            "a.c is still open ahead: nothing more is opened"
        );

        // Only when a lane takes a.c does the window open again.
        assert_eq!(drain_prepares_from(&mut queue, 2), vec![id(6), id(5)]);
        assert_eq!(queue.next_prepare(1), Some(id(4)), "the lane takes a.c");
        assert_eq!(queue.files_to_prefetch(1), vec!["e.c"]);
    }

    #[test]
    fn a_file_that_comes_back_is_queued_and_opened_ahead_like_a_new_one() {
        let mut queue = queue(&[("a.c", &[1]), ("b.c", &[2])]);
        assert_eq!(drain_prepares(&mut queue, 1), vec![id(1), id(2)]);

        // A late symbol in a file the lanes already finished with.
        assert!(queue.push(id(3), "a.c".to_string(), 3));

        assert_eq!(queue.files_to_prefetch(2), vec!["a.c"]);
        assert_eq!(queue.next_prepare(1), Some(id(3)));
    }

    #[test]
    fn iteration_lists_every_waiting_symbol_once_in_lane_order() {
        let mut queue = queue(&[("a.c", &[1, 2]), ("b.c", &[3])]);
        assert_eq!(queue.next_prepare(1), Some(id(2)));
        queue.promote(id(3));

        let waiting: Vec<SymbolId> = queue.iter().copied().collect();
        assert_eq!(waiting, vec![id(1), id(3)]);
    }
}
