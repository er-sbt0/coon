// Binary crate — all logic lives in library crates.
// runner, logging, and demo are private modules used only by main.rs.
//
// DEPRECATED. This binary starts the TUI and the web server, both of which are
// kept working but no longer developed; `coon-desktop` (crate `desktop`) is the
// frontend. It is out of the workspace's `default-members` for that reason, so
// a bare `cargo run` launches the desktop app instead — build this one with
// `cargo build -p coon`.
