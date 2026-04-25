// ── Cytoscape setup ──────────────────────────────────────────────────────────

const cy = cytoscape({
  container: document.getElementById('cy'),
  style: [
    {
      selector: 'node',
      style: {
        'background-color': '#1f2937',
        'border-color': '#374151',
        'border-width': 1,
        'label': 'data(label)',
        'color': '#c9d1d9',
        'font-family': 'monospace',
        'font-size': 11,
        'text-valign': 'center',
        'text-halign': 'center',
        'width': 'label',
        'height': 28,
        'padding': '6px',
        'shape': 'roundrectangle',
      }
    },
    {
      selector: 'node.root',
      style: {
        'background-color': '#1f3a5f',
        'border-color': '#58a6ff',
        'border-width': 2,
        'color': '#58a6ff',
      }
    },
    {
      selector: 'node.focused',
      style: {
        'background-color': '#2d1f3a',
        'border-color': '#a371f7',
        'border-width': 2,
        'color': '#d2a8ff',
      }
    },
    {
      selector: 'edge',
      style: {
        'width': 1,
        'line-color': '#374151',
        'target-arrow-color': '#4b5563',
        'target-arrow-shape': 'triangle',
        'curve-style': 'bezier',
      }
    }
  ],
  layout: { name: 'preset' },
});

// ── State ────────────────────────────────────────────────────────────────────

let allFunctions   = {};                          // id → { name, qualified_name }
let currentGraph   = { nodes: {}, edges: [] };   // cached full graph
let rootNodeId     = null;                        // graph root (workspace.selected)
let selectedNodeId = null;                        // last focused node (snapshot.selected)
let currentDirection = 'Outgoing';
let lastFitKey     = null;                        // `${rootNodeId}:${direction}` — fit only on root/direction change

// ── DOM refs ─────────────────────────────────────────────────────────────────

const statusEl    = document.getElementById('status');
const infoEl      = document.getElementById('info');
const searchEl    = document.getElementById('search');
const fnListEl    = document.getElementById('fn-list');
const fnCountEl   = document.getElementById('fn-count');
const dirToggleEl = document.getElementById('dir-toggle');

// ── WebSocket ────────────────────────────────────────────────────────────────

const ws = new WebSocket(`ws://${location.host}/ws`);
ws.onopen  = () => { statusEl.textContent = 'Connected'; };
ws.onclose = () => {
  statusEl.textContent = 'Disconnected — reconnecting…';
  setTimeout(() => location.reload(), 2000);
};
ws.onerror = () => { statusEl.textContent = 'Error'; };

ws.onmessage = (e) => {
  const msg = JSON.parse(e.data);
  if (msg.type === 'snapshot') {
    handleSnapshot(msg);
  } else if (msg.type === 'layout') {
    // Layout-only: use the cached graph structure with new positions.
    rebuildGraph(currentGraph, msg.layout);
  } else if (msg.type === 'event') {
    handleEvent(msg.event);
  }
};

// ── Message handlers ─────────────────────────────────────────────────────────

function handleSnapshot(msg) {
  currentGraph   = msg.graph;
  allFunctions   = msg.graph.nodes || {};
  selectedNodeId = msg.selected ?? null;
  rootNodeId     = msg.workspaces?.[0]?.selected ?? null;

  const dir = msg.workspaces?.[0]?.direction;
  if (dir) {
    currentDirection = dir;
    updateDirToggle();
  }

  rebuildGraph(currentGraph, msg.layout);
  renderSidebar(searchEl.value);

  const n = Object.keys(allFunctions).length;
  fnCountEl.textContent = n ? `(${n})` : '';
  if (n > 0 && Object.keys(msg.layout).length === 0) {
    infoEl.textContent = `${n} symbols loaded. Click a function in the sidebar to explore its call graph.`;
  }
}

// ── Graph rebuild ─────────────────────────────────────────────────────────────

function rebuildGraph(graph, layout) {
  cy.elements().remove();

  for (const [id, node] of Object.entries(graph.nodes || {})) {
    const pos = layout[id];
    if (!pos) continue;   // not part of the visible subgraph
    cy.add({
      group: 'nodes',
      data: { id, label: node.name },
      position: { x: pos.x * 14, y: pos.y * 14 },
    });
  }

  const edgesSeen = new Set();
  for (const edge of (graph.edges || [])) {
    const eid = `${edge.caller}=>${edge.callee}`;
    if (!edgesSeen.has(eid) && cy.$id(edge.caller).length && cy.$id(edge.callee).length) {
      edgesSeen.add(eid);
      cy.add({ group: 'edges', data: { id: eid, source: edge.caller, target: edge.callee } });
    }
  }

  // Apply visual classes after all elements are in place
  if (rootNodeId) {
    cy.$id(rootNodeId).addClass('root');
  }
  if (selectedNodeId && selectedNodeId !== rootNodeId) {
    cy.$id(selectedNodeId).addClass('focused');
  }

  // Only auto-fit when the focused/selected node or direction changes.
  // Using selectedNodeId (updated after every ExpandFunction resolution) means
  // we also re-fit when the user expands a node and the BFS window shifts.
  const fitKey = `${selectedNodeId ?? rootNodeId}:${currentDirection}`;
  if (cy.nodes().length > 0 && fitKey !== lastFitKey) {
    lastFitKey = fitKey;
    requestAnimationFrame(() => cy.fit(cy.nodes(), 40));
  }
}

// ── Event handling ────────────────────────────────────────────────────────────

function handleEvent(event) {
  if (typeof event === 'string') {
    updateStatusFromEvent(event, null);
    return;
  }
  const key = Object.keys(event)[0];
  if (!key) return;
  updateStatusFromEvent(key, event[key]);
}

function updateStatusFromEvent(key, payload) {
  if (key === 'LspProgress') {
    const phase = typeof payload === 'string' ? payload : Object.keys(payload)[0];
    if (phase === 'Completed') {
      statusEl.textContent = 'Ready';
    } else if (phase === 'PreloadingDocuments' && payload[phase]) {
      const { done, total } = payload[phase];
      statusEl.textContent = `Indexing… ${done}/${total}`;
    } else if (phase === 'LoadingWorkspaceSymbols' && payload[phase]) {
      statusEl.textContent = `Loading symbols… ${payload[phase].loaded}`;
    } else {
      statusEl.textContent = phase.replace(/([A-Z])/g, ' $1').trim();
    }
  } else if (key === 'LoadingCalls') {
    statusEl.textContent = `Loading ${(payload?.direction || '').toLowerCase()} calls…`;
  } else if (key === 'FunctionHasNoCallees') {
    infoEl.textContent = 'No callees in this direction.';
  } else if (key === 'OutgoingCallsLoaded' || key === 'IncomingCallsLoaded') {
    statusEl.textContent = 'Ready';
  }
}

// ── Sidebar ───────────────────────────────────────────────────────────────────

function renderSidebar(filter) {
  const q = (filter || '').toLowerCase();
  const ids = Object.keys(allFunctions)
    .filter(id => {
      const n = allFunctions[id];
      return !q || n.name.toLowerCase().includes(q) || (n.qualified_name || '').toLowerCase().includes(q);
    })
    .sort((a, b) => allFunctions[a].name.localeCompare(allFunctions[b].name));

  fnListEl.innerHTML = '';

  if (ids.length === 0) {
    const li = document.createElement('li');
    li.className = 'empty';
    li.textContent = Object.keys(allFunctions).length === 0
      ? 'No symbols loaded yet'
      : 'No matches';
    fnListEl.appendChild(li);
    return;
  }

  for (const id of ids) {
    const node = allFunctions[id];
    const li = document.createElement('li');
    li.textContent = node.name;
    li.title = node.qualified_name || node.name;
    if (id === rootNodeId) li.classList.add('selected');
    li.addEventListener('click', () => selectRoot(id));
    fnListEl.appendChild(li);
  }
}

searchEl.addEventListener('input', () => renderSidebar(searchEl.value));

// ── Commands ──────────────────────────────────────────────────────────────────

function sendCommand(command) {
  ws.send(JSON.stringify({ type: 'command', command }));
}

// Sidebar click: change the graph root to this function.
function selectRoot(id) {
  const name = allFunctions[id]?.name ?? id;
  infoEl.textContent = `Exploring call graph for: ${name}`;
  sendCommand({ SelectFunction: id });
}

// Graph node click: expand this node in place (load its children without
// changing the root). Mirrors the TUI "navigate + press Enter" behavior.
function expandNode(id) {
  const name = allFunctions[id]?.name ?? id;
  infoEl.textContent = `Expanding: ${name}`;
  sendCommand({ ExpandFunction: id });
}

cy.on('tap', 'node', (e) => {
  expandNode(e.target.id());
});

// ── Direction toggle ──────────────────────────────────────────────────────────

function updateDirToggle() {
  if (dirToggleEl) {
    dirToggleEl.textContent = currentDirection === 'Outgoing' ? '→ Outgoing' : '← Incoming';
  }
}

if (dirToggleEl) {
  dirToggleEl.addEventListener('click', () => {
    sendCommand('ToggleCallDirection');
  });
}
