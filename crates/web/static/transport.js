// ── Transport abstraction ────────────────────────────────────────────────────
//
// The backend speaks one protocol (core::protocol::ServerMsg / ClientMsg) over
// two different pipes:
//
//   web frontend      → JSON over a WebSocket to the axum server
//   desktop frontend  → the same JSON over Tauri IPC
//
// app.js is written against this interface alone and never touches either pipe
// directly, so a single client serves both frontends.
//
// Contract:
//   CoonTransport.connect({ onStatus(text), onMessage(serverMsg) })
//   CoonTransport.send(command)          // a core::Command, serde-shaped

(function () {
  function websocketTransport() {
    let ws = null;

    return {
      name: 'websocket',
      connect(handlers) {
        ws = new WebSocket(`ws://${location.host}/ws`);
        ws.onopen = () => handlers.onStatus('Connected');
        ws.onerror = () => handlers.onStatus('Error');
        ws.onclose = () => {
          handlers.onStatus('Disconnected — reconnecting…');
          setTimeout(() => location.reload(), 2000);
        };
        ws.onmessage = (e) => handlers.onMessage(JSON.parse(e.data));
      },
      send(command) {
        if (ws && ws.readyState === WebSocket.OPEN) {
          ws.send(JSON.stringify({ type: 'command', command }));
        }
      },
    };
  }

  function tauriTransport() {
    // `withGlobalTauri` in tauri.conf.json exposes the API on window.__TAURI__,
    // which keeps the client a plain script with no JS build step.
    const { invoke } = window.__TAURI__.core;
    const { listen } = window.__TAURI__.event;

    return {
      name: 'tauri',
      connect(handlers) {
        listen('coon://message', (e) => handlers.onMessage(e.payload));
        // Unlike the WebSocket case there is no connection event to wait on;
        // ask for the opening snapshot explicitly once listening.
        invoke('request_snapshot')
          .then(() => handlers.onStatus('Ready'))
          .catch((err) => {
            console.error('request_snapshot failed', err);
            handlers.onStatus('Error');
          });
      },
      send(command) {
        invoke('dispatch', { command }).catch((err) =>
          console.error('dispatch failed', command, err),
        );
      },
    };
  }

  const isTauri = typeof window.__TAURI__ !== 'undefined';

  window.CoonTransport = isTauri ? tauriTransport() : websocketTransport();
  window.CoonTransport.isDesktop = isTauri;

  // Window management is the one genuinely desktop-only surface. It lives here
  // rather than in app.js so __TAURI__ is touched in exactly one file; in the
  // browser these are no-ops and the controls stay hidden.
  //
  // Saving a file is here for the same reason: `core` builds the bytes and
  // says nothing about where they land, and where they land is exactly what
  // the two pipes disagree about. A browser can only hand the file to the
  // download machinery; the desktop shell has a real filesystem and writes to
  // it. Both answer with a promise for something to show the user, because
  // "saved" without a location is not an answer.
  function browserSave(filename, contents) {
    const url = URL.createObjectURL(new Blob([contents], { type: 'text/plain' }));
    const a = document.createElement('a');
    a.href = url;
    a.download = filename;
    a.click();
    // Revoked on the next turn: revoking synchronously races the download in
    // some browsers, which then save an empty file.
    setTimeout(() => URL.revokeObjectURL(url), 0);
    return Promise.resolve(filename);
  }

  window.CoonHost = isTauri
    ? (() => {
        const win = window.__TAURI__.window.getCurrentWindow();
        const { invoke } = window.__TAURI__.core;
        return {
          minimize: () => win.minimize(),
          toggleMaximize: () => win.toggleMaximize(),
          close: () => win.close(),
          saveFile: (filename, contents) => invoke('save_export', { filename, contents }),
        };
      })()
    : { minimize() {}, toggleMaximize() {}, close() {}, saveFile: browserSave };

  console.log(`[coon] transport: ${window.CoonTransport.name}`);
})();
