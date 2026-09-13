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
  console.log(`[coon] transport: ${window.CoonTransport.name}`);
})();
