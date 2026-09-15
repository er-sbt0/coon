# crates/tui — deprecated

**Do not do new work here.** The Ratatui TUI is a deprecated frontend. The
desktop app (`crates/desktop` + the shared client in `crates/web/static`) is
where frontend work goes.

If you are here because a task mentioned the TUI, check whether it actually
wants the desktop app instead, and say so rather than implementing it twice.

## What is still worth doing here

Deprecated means kept working, not frozen out. This crate still builds, still
runs and is still covered by `cargo test`. So:

- **Fix it when a `core` or `protocol` change breaks the build.** That is the
  cost of keeping it compiling and it is a cost worth paying — a break here is
  a change to fix, not a reason to delete the crate.
- **Fix a test that this crate's own tests catch.**

## What is not

- Porting a desktop feature across. A feature that lands in the desktop app is
  not expected to land here, and nobody has asked for it.
- Refactors, cleanups, lint fixes, doc passes, "while I'm here" improvements.
- Anything that starts "the TUI could also…".

Nothing here is marked `#[deprecated]` as an attribute: `coon` is the only
caller and is deprecated itself, so the warnings would land nowhere that does
not already say so. This file is the notice instead.
