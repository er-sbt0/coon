# crates/web — half deprecated, and the half matters

This crate is two things with opposite status. Read which one you are in before
you touch anything.

| Path | Status |
|---|---|
| `src/` — the Axum server, WebSocket at `/ws`, `rust-embed` assets | **Deprecated.** No new work. |
| `static/` — the JavaScript client | **Live.** The desktop app ships it as its `frontendDist`. Client work goes here. |

## `src/` — deprecated

**Do not do new work here.** `coon --web <PORT>` still serves the client over a
WebSocket and is still tested, but the desktop app is the frontend now.

Still worth doing: fixing it when a `core` or `protocol` change breaks the
build, and fixing a test this crate catches. Not worth doing: new endpoints,
new features, refactors, or porting a desktop capability across.

## `static/` — not deprecated, do not treat it as such

The desktop and web frontends **share this one client**. It is the desktop
app's frontend, so a change to `app.js`, `app.css`, `theme.css` or `palette.js`
is a change to the shipped product — treat it exactly as you would code under
`crates/desktop`.

The two differ only in `transport.js`, which feature-detects `window.__TAURI__`
and picks a WebSocket or Tauri `invoke`/`listen`. Keep it that way: shared
behaviour in `app.js`, anything pipe-specific (including window management, via
`CoonHost`) in `transport.js`. Because both backends emit identical
`core::protocol::ServerMsg` shapes, `app.js` needs no knowledge of which one it
is talking to.

Assets are vendored, not CDN-loaded (`static/vendor/cytoscape.min.js`), because
the desktop app must work offline.
