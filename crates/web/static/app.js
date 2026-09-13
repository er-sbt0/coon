// ── Cytoscape setup ──────────────────────────────────────────────────────────
// Styles read from the CSS custom properties in theme.css so the graph and the
// chrome stay on one palette, and a theme switch moves both together.

const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

// Nodes sit on a fixed grid. core::compute_layout emits column/row indices on a
// fixed pitch, so variable-width nodes overlap their neighbours as soon as a
// symbol name gets long — routine for C++ with namespaces and templates.
// A fixed width plus a truncated label keeps the grid honest; the full
// qualified name is shown in the status bar on hover.
const NODE_W = 150;
const LABEL_MAX = 18;

const truncate = (name) => (name.length > LABEL_MAX ? `${name.slice(0, LABEL_MAX - 1)}…` : name);

const cy = cytoscape({
  container: document.getElementById('cy'),
  style: [
    {
      selector: 'node',
      style: {
        'background-color': css('--node-bg'),
        'border-color': css('--node-border'),
        'border-width': 1,
        'label': 'data(label)',
        'color': css('--text'),
        'font-family': css('--font-mono'),
        'font-size': 12,
        'text-valign': 'center',
        'text-halign': 'center',
        'width': NODE_W,
        'height': 32,
        'shape': 'roundrectangle',
        'transition-property': 'border-color, background-color, opacity',
        'transition-duration': '90ms',
      }
    },
    // One accent, applied at different strengths, rather than a different hue
    // per state — the eye tracks a single signal.
    {
      selector: 'node.root',
      style: {
        'border-color': css('--accent'),
        'border-width': 2,
        'color': css('--accent'),
      }
    },
    {
      selector: 'node.focused',
      style: {
        'border-color': css('--accent-border'),
        'border-width': 2,
      }
    },
    { selector: 'node.hovered', style: { 'background-color': css('--surface-hover') } },
    { selector: '.dimmed', style: { 'opacity': 0.25 } },
    {
      selector: 'edge',
      style: {
        'width': 1,
        'line-color': css('--edge'),
        'target-arrow-color': css('--edge-arrow'),
        'target-arrow-shape': 'triangle',
        'arrow-scale': 0.85,
        'curve-style': 'bezier',
        'transition-property': 'line-color, opacity',
        'transition-duration': '90ms',
      }
    },
    { selector: 'edge.incident', style: { 'line-color': css('--accent'), 'target-arrow-color': css('--accent') } },
  ],
  layout: { name: 'preset' },
  wheelSensitivity: 0.2,
});

// ── State ────────────────────────────────────────────────────────────────────

let allFunctions     = {};                        // id → { name, qualified_name }
let currentGraph     = { nodes: {}, edges: [] };  // cached full graph
let rootNodeId       = null;                      // graph root (active workspace)
let selectedNodeId   = null;                      // last focused node
let currentDirection = 'Outgoing';
let workspaces       = [];
let activeWorkspace  = 0;
let lastFitKey       = null;                      // refit only on root/direction change

const MAX_FIT_ZOOM = 1.15;

// ── DOM refs ─────────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);
const statusEl   = $('status');
const lspDotEl   = $('lsp-dot');
const infoEl     = $('info');
const searchEl   = $('search');
const fnListEl   = $('fn-list');
const fnCountEl  = $('fn-count');
const dirToggleEl = $('dir-toggle');
const tabsEl     = $('tabs');
const crumbEl    = $('breadcrumb');
const statSymbolsEl = $('stat-symbols');
const statNodesEl   = $('stat-nodes');

if (CoonTransport.isDesktop) document.body.classList.add('is-desktop');

// ── Transport ────────────────────────────────────────────────────────────────
// Supplied by transport.js — WebSocket in the browser, Tauri IPC on the desktop.

CoonTransport.connect({
  onStatus: (text) => setStatus(text),
  onMessage: (msg) => {
    if (msg.type === 'snapshot') {
      handleSnapshot(msg);
    } else if (msg.type === 'layout') {
      // Layout-only: use the cached graph structure with new positions.
      rebuildGraph(currentGraph, msg.layout);
    } else if (msg.type === 'event') {
      handleEvent(msg.event);
    }
  },
});

function sendCommand(command) {
  CoonTransport.send(command);
}

// ── Message handlers ─────────────────────────────────────────────────────────

function handleSnapshot(msg) {
  currentGraph    = msg.graph;
  allFunctions    = msg.graph.nodes || {};
  selectedNodeId  = msg.selected ?? null;
  workspaces      = msg.workspaces || [];
  activeWorkspace = msg.active_workspace ?? 0;

  const active = workspaces[activeWorkspace];
  rootNodeId = active?.selected ?? null;
  if (active?.direction) {
    currentDirection = active.direction;
    updateDirToggle();
  }

  renderTabs();
  rebuildGraph(currentGraph, msg.layout);
  renderSidebar(searchEl.value);
  renderBreadcrumb();

  const n = Object.keys(allFunctions).length;
  fnCountEl.textContent = n ? `(${n})` : '';
  statSymbolsEl.textContent = n.toLocaleString();
  statNodesEl.textContent = Object.keys(msg.layout || {}).length.toLocaleString();

  if (n > 0 && Object.keys(msg.layout).length === 0) {
    infoEl.textContent = 'Pick a symbol to explore its call graph';
  }
}

// ── Workspace tabs ───────────────────────────────────────────────────────────
// The backend has supported multiple workspaces all along; these are the
// commands that were already there but had no UI.

function renderTabs() {
  tabsEl.innerHTML = '';

  workspaces.forEach((ws, index) => {
    const tab = document.createElement('div');
    tab.className = 'tab' + (index === activeWorkspace ? ' active' : '');
    tab.setAttribute('role', 'tab');
    tab.setAttribute('aria-selected', String(index === activeWorkspace));

    const name = document.createElement('span');
    name.className = 'tab-name';
    name.textContent = ws.name;
    name.title = `${ws.name} — double-click to rename`;
    name.addEventListener('dblclick', (e) => { e.stopPropagation(); beginRename(name, index); });
    tab.appendChild(name);

    // Only offer a close control when closing is actually allowed; core
    // rejects closing the last workspace.
    if (workspaces.length > 1) {
      const close = document.createElement('button');
      close.className = 'tab-close';
      close.textContent = '×';
      close.title = 'Close graph';
      close.setAttribute('aria-label', `Close ${ws.name}`);
      close.addEventListener('click', (e) => {
        e.stopPropagation();
        sendCommand({ CloseWorkspace: index });
      });
      tab.appendChild(close);
    }

    tab.addEventListener('mousedown', (e) => {
      // Middle-click closes, matching editor convention.
      if (e.button === 1 && workspaces.length > 1) {
        e.preventDefault();
        sendCommand({ CloseWorkspace: index });
      } else if (e.button === 0 && index !== activeWorkspace) {
        sendCommand({ SwitchWorkspace: index });
      }
    });

    tabsEl.appendChild(tab);
  });
}

function beginRename(nameEl, index) {
  const original = nameEl.textContent;
  nameEl.contentEditable = 'true';
  nameEl.focus();
  document.execCommand?.('selectAll', false, null);

  const commit = (save) => {
    nameEl.contentEditable = 'false';
    const next = nameEl.textContent.trim();
    if (save && next && next !== original) {
      sendCommand({ RenameWorkspace: [index, next] });
    } else {
      nameEl.textContent = original;
    }
  };

  nameEl.addEventListener('blur', () => commit(true), { once: true });
  nameEl.addEventListener('keydown', (e) => {
    e.stopPropagation();
    if (e.key === 'Enter') { e.preventDefault(); nameEl.blur(); }
    if (e.key === 'Escape') { nameEl.textContent = original; nameEl.blur(); }
  });
}

$('tab-new').addEventListener('click', () => sendCommand('NewWorkspace'));

// ── Breadcrumb ───────────────────────────────────────────────────────────────
// Shows the path from the workspace root to the node currently in focus, which
// is the question the graph answers and the toolbar never did.

function renderBreadcrumb() {
  crumbEl.innerHTML = '';

  if (!rootNodeId) {
    const span = document.createElement('span');
    span.className = 'placeholder';
    span.textContent = 'No graph selected';
    crumbEl.appendChild(span);
    return;
  }

  const path = pathFromRoot(rootNodeId, selectedNodeId);
  path.forEach((id, i) => {
    if (i > 0) {
      const sep = document.createElement('span');
      sep.className = 'sep';
      sep.textContent = '›';
      crumbEl.appendChild(sep);
    }
    const crumb = document.createElement('button');
    crumb.className = 'crumb' + (i === path.length - 1 ? ' current' : '');
    crumb.textContent = allFunctions[id]?.name ?? id;
    crumb.title = allFunctions[id]?.qualified_name ?? '';
    crumb.addEventListener('click', () => expandNode(id));
    crumbEl.appendChild(crumb);
  });
}

/// Shortest root→target path over the current edges, following call direction.
function pathFromRoot(root, target) {
  if (!target || target === root) return [root];

  const outgoing = currentDirection === 'Outgoing';
  const adjacency = {};
  for (const edge of currentGraph.edges || []) {
    const [from, to] = outgoing ? [edge.caller, edge.callee] : [edge.callee, edge.caller];
    (adjacency[from] ||= []).push(to);
  }

  const prev = { [root]: null };
  const queue = [root];
  while (queue.length) {
    const node = queue.shift();
    if (node === target) {
      const path = [];
      for (let at = target; at != null; at = prev[at]) path.unshift(at);
      return path;
    }
    for (const next of adjacency[node] || []) {
      if (!(next in prev)) { prev[next] = node; queue.push(next); }
    }
  }
  // Disconnected (the target was expanded outside the root's subtree).
  return [root, target];
}

// ── Graph rebuild ─────────────────────────────────────────────────────────────

function rebuildGraph(graph, layout) {
  cy.elements().remove();

  // Layout positions come from core::compute_layout in grid units; SPACING
  // converts them to pixels. Sized for the pointer-driven canvas rather than
  // inherited from the TUI's character cell.
  const SPACING_X = 52;   // 4 grid units per level → 208px pitch, ~58px gutter
  const SPACING_Y = 26;   // ~27px between sibling rows at 32px node height

  for (const [id, node] of Object.entries(graph.nodes || {})) {
    const pos = layout[id];
    if (!pos) continue;   // not part of the visible subgraph
    cy.add({
      group: 'nodes',
      data: { id, label: truncate(node.name), full: node.qualified_name || node.name },
      position: { x: pos.x * SPACING_X, y: pos.y * SPACING_Y },
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

  if (rootNodeId) cy.$id(rootNodeId).addClass('root');
  if (selectedNodeId && selectedNodeId !== rootNodeId) cy.$id(selectedNodeId).addClass('focused');

  // Only auto-fit when the focused node or direction changes, so expanding a
  // node doesn't yank the viewport the user just positioned.
  const fitKey = `${selectedNodeId ?? rootNodeId}:${currentDirection}`;
  if (cy.nodes().length > 0 && fitKey !== lastFitKey) {
    lastFitKey = fitKey;
    requestAnimationFrame(() => {
      // Cytoscape caches the container size from init; without this the fit is
      // computed against stale dimensions and the graph lands off-centre.
      cy.resize();
      cy.fit(cy.nodes(), 60);
      // Fitting a handful of nodes to a large canvas magnifies them to a
      // cartoon size. Cap the automatic zoom; manual zoom stays unbounded.
      if (cy.zoom() > MAX_FIT_ZOOM) {
        cy.zoom(MAX_FIT_ZOOM);
        cy.center(cy.nodes());
      }
    });
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

function setStatus(text, state) {
  statusEl.textContent = text;
  lspDotEl.className = state || '';
}

function updateStatusFromEvent(key, payload) {
  if (key === 'LspProgress') {
    const phase = typeof payload === 'string' ? payload : Object.keys(payload)[0];
    if (phase === 'Completed') {
      setStatus('Ready', 'ready');
    } else if (phase === 'PreloadingDocuments' && payload[phase]) {
      const { done, total } = payload[phase];
      setStatus(`Indexing ${done}/${total}`, 'busy');
    } else if (phase === 'LoadingWorkspaceSymbols' && payload[phase]) {
      setStatus(`Loading symbols ${payload[phase].loaded}`, 'busy');
    } else if (phase === 'NotStarted') {
      // The bridge's initial phase, and the permanent one in demo mode.
      setStatus('Idle');
    } else {
      setStatus(phase.replace(/([A-Z])/g, ' $1').trim(), 'busy');
    }
  } else if (key === 'LoadingCalls') {
    setStatus(`Loading ${(payload?.direction || '').toLowerCase()} calls`, 'busy');
  } else if (key === 'FunctionHasNoCallees') {
    infoEl.textContent = 'No callees in this direction';
  } else if (key === 'OutgoingCallsLoaded' || key === 'IncomingCallsLoaded') {
    setStatus('Ready', 'ready');
  } else if (key === 'RefreshingWorkspaceSymbols') {
    setStatus('Refreshing', 'busy');
  } else if (key === 'CannotCloseLastWorkspace') {
    infoEl.textContent = 'Cannot close the last graph';
  } else if (key === 'LspError') {
    setStatus(typeof payload === 'string' ? payload : 'LSP error', 'error');
  } else if (key === 'WorkspaceCreated' || key === 'WorkspaceSwitched') {
    infoEl.textContent = payload?.name ? `Graph: ${payload.name}` : '';
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
    li.textContent = Object.keys(allFunctions).length === 0 ? 'No symbols loaded yet' : 'No matches';
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
    // Middle-click opens the symbol in its own graph tab.
    li.addEventListener('mousedown', (e) => {
      if (e.button === 1) { e.preventDefault(); sendCommand({ StartGraphFromFunction: id }); }
    });
    fnListEl.appendChild(li);
  }
}

searchEl.addEventListener('input', () => renderSidebar(searchEl.value));
searchEl.addEventListener('keydown', (e) => {
  e.stopPropagation();
  if (e.key === 'Escape') { searchEl.value = ''; renderSidebar(''); searchEl.blur(); }
});

// ── Commands ──────────────────────────────────────────────────────────────────

function selectRoot(id) {
  infoEl.textContent = `Exploring ${allFunctions[id]?.name ?? id}`;
  sendCommand({ SelectFunction: id });
}

// Expand a node in place: load its children without changing the root.
function expandNode(id) {
  infoEl.textContent = `Expanding ${allFunctions[id]?.name ?? id}`;
  sendCommand({ ExpandFunction: id });
}

cy.on('tap', 'node', (e) => expandNode(e.target.id()));

// Hover: highlight the node's own edges and dim the rest, so a dense graph
// still reads at a glance.
cy.on('mouseover', 'node', (e) => {
  const node = e.target;
  node.addClass('hovered');
  node.connectedEdges().addClass('incident');
  // The canvas has no tooltips, and the label may be truncated, so the status
  // bar carries the full qualified name.
  infoEl.textContent = node.data('full');
  showExpandBtn(node);
});
cy.on('mouseout', 'node', (e) => {
  e.target.removeClass('hovered');
  cy.edges().removeClass('incident');
  infoEl.textContent = '';
  hideExpandBtn();
});

// ── Expand button overlay ─────────────────────────────────────────────────────

const expandBtn = $('expand-btn');
let expandTargetId = null;
let hideTimer = null;

function showExpandBtn(node) {
  clearTimeout(hideTimer);
  expandTargetId = node.id();
  const bb = node.renderedBoundingBox();
  const container = $('cy').getBoundingClientRect();
  // Positioned inside the node's top-right corner: no gap for the cursor to
  // cross, so mouseenter fires before the hide timer expires.
  expandBtn.style.left = `${container.left + bb.x2 - 21}px`;
  expandBtn.style.top  = `${container.top + bb.y1 + 5}px`;
  expandBtn.style.display = 'flex';
}

function hideExpandBtn() {
  hideTimer = setTimeout(() => {
    expandBtn.style.display = 'none';
    expandTargetId = null;
  }, 300);
}

expandBtn.addEventListener('mouseenter', () => clearTimeout(hideTimer));
expandBtn.addEventListener('mouseleave', () => hideExpandBtn());
expandBtn.addEventListener('click', (e) => {
  e.stopPropagation();
  if (expandTargetId) expandNode(expandTargetId);
  expandBtn.style.display = 'none';
});

// ── Toolbar ───────────────────────────────────────────────────────────────────

function updateDirToggle() {
  dirToggleEl.textContent = currentDirection === 'Outgoing' ? '→ Outgoing' : '← Incoming';
  dirToggleEl.title = currentDirection === 'Outgoing'
    ? 'Showing calls made by each function — click for callers'
    : 'Showing callers of each function — click for callees';
}

dirToggleEl.addEventListener('click', () => sendCommand('ToggleCallDirection'));
$('refresh-btn').addEventListener('click', () => sendCommand('Refresh'));

// ── Sidebar dock: collapse and drag-resize ────────────────────────────────────

const sidebarEl = $('sidebar');

$('sidebar-toggle').addEventListener('click', toggleSidebar);
function toggleSidebar() {
  document.body.classList.toggle('sidebar-collapsed');
  requestAnimationFrame(() => cy.resize());
}

const dockHandle = $('dock-handle');
dockHandle.addEventListener('mousedown', (e) => {
  e.preventDefault();
  dockHandle.classList.add('dragging');
  document.body.classList.add('resizing');

  const onMove = (ev) => {
    const width = Math.min(Math.max(ev.clientX, 160), 480);
    sidebarEl.style.width = `${width}px`;
  };
  const onUp = () => {
    dockHandle.classList.remove('dragging');
    document.body.classList.remove('resizing');
    document.removeEventListener('mousemove', onMove);
    document.removeEventListener('mouseup', onUp);
    cy.resize();
  };
  document.addEventListener('mousemove', onMove);
  document.addEventListener('mouseup', onUp);
});

// ── Window controls (desktop only) ────────────────────────────────────────────

$('wc-min').addEventListener('click', () => CoonHost.minimize());
$('wc-max').addEventListener('click', () => CoonHost.toggleMaximize());
$('wc-close').addEventListener('click', () => CoonHost.close());

// ── Keybindings ───────────────────────────────────────────────────────────────

document.addEventListener('keydown', (e) => {
  const mod = e.ctrlKey || e.metaKey;
  if (!mod) return;

  if (e.key === 'b') { e.preventDefault(); toggleSidebar(); }
  else if (e.key === 't') { e.preventDefault(); sendCommand('NewWorkspace'); }
  else if (e.key === 'w') {
    e.preventDefault();
    if (workspaces.length > 1) sendCommand({ CloseWorkspace: activeWorkspace });
  }
  else if (e.key === 'f') { e.preventDefault(); searchEl.focus(); searchEl.select(); }
  else if (e.key === 'r') { e.preventDefault(); sendCommand('Refresh'); }
  else if (e.key === 'd') { e.preventDefault(); sendCommand('ToggleCallDirection'); }
  else if (e.key === 'Tab') {
    e.preventDefault();
    sendCommand(e.shiftKey ? 'PreviousWorkspace' : 'NextWorkspace');
  }
  else if (e.key >= '1' && e.key <= '9') {
    const index = Number(e.key) - 1;
    if (index < workspaces.length) { e.preventDefault(); sendCommand({ SwitchWorkspace: index }); }
  }
});
