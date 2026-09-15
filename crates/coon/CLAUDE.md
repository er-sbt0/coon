# crates/coon — deprecated

**Do not do new work here.** This crate is the `coon` binary: CLI parsing plus
the runners for the two deprecated frontends, the TUI and the web server. The
desktop app is `crates/desktop`, and `cargo run` launches that one — `coon` is
outside `default-members` precisely so it does not compete for the bare
`cargo run`.

The user-facing notice is `main::warn_deprecated`, printed to stderr before the
TUI takes the terminal. Keep it.

## What is still worth doing here

- **Fix it when a `core` or `protocol` change breaks the build.**
- **Fix a test this crate's own tests catch.**

## What is not

- New CLI flags, new runners, new features of any kind. A new user-facing
  action belongs in `buildCommands()` in `crates/web/static/app.js`, reaching
  the desktop app.
- Refactors, cleanups, or doc passes for their own sake.

See `crates/tui/CLAUDE.md` and `crates/web/CLAUDE.md` for the frontends this
binary runs.
