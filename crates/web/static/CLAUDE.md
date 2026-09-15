# crates/web/static — the live client

**This directory is not deprecated**, despite sitting inside a crate whose Rust
is. It is the shared JavaScript client, and `crates/desktop` ships it verbatim
as its `frontendDist` — so this is the desktop app's frontend and the place
client work belongs.

The parent `crates/web/CLAUDE.md` says the server in `src/` is deprecated. That
notice stops at this directory boundary.

| File | Holds |
|---|---|
| `theme.css` | Design tokens only — colours, type scale, 4px spacing grid, shell metrics. Light and dark. |
| `app.css` | Component styles. Reads tokens; defines no raw colours. |
| `transport.js` | The pipe (WebSocket vs Tauri IPC) and `CoonHost` window controls. The only file that touches `__TAURI__`. |
| `palette.js` | The command palette / fuzzy picker. Generic: callers hand it items and get a pick callback. |
| `app.js` | Everything else: tabs, graph, sidebar, breadcrumb, status bar, commands, keybindings. |

**New user-facing actions belong in `buildCommands()` in `app.js`, not as
another toolbar button** — that is how the commands `core` exposes stay
reachable without the chrome growing a control each time.

The design rules that govern this code — the readability mechanisms, node
sizing kept in step with `crates/core/src/layout.rs`, one accent, sans for
chrome and mono for code — are in the root `CLAUDE.md` under **Readability at
scale**. Read them before changing how the canvas looks.
