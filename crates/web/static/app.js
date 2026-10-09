// ── Cytoscape setup ──────────────────────────────────────────────────────────
// Styles read from the CSS custom properties in theme.css so the graph and the
// chrome stay on one palette, and a theme switch moves both together.

// ── Remembered settings ──────────────────────────────────────────────────────
// Everything this client remembers between runs is one `localStorage` object.
// The backend persists nothing — not the depth, not the direction, not which
// algorithm built the map — so a setting that has to survive a restart
// survives it here, and is handed to each workspace the first time this client
// sees one. That makes one mechanism do two jobs: the restore after a restart,
// and the default a new tab opens with.
//
// One object rather than a key per setting, because they are read together —
// every one of them on every snapshot — and because a store you can print is a
// store you can reason about when a setting comes back wrong.

const PREFS_KEY = 'coon.prefs';

function prefs() {
  try {
    return JSON.parse(localStorage.getItem(PREFS_KEY) || '{}') || {};
  } catch { return {}; /* private mode, or something else wrote here */ }
}

function savePrefs(patch) {
  try {
    localStorage.setItem(PREFS_KEY, JSON.stringify({ ...prefs(), ...patch }));
  } catch { /* storage blocked: this run simply does not remember */ }
}

// Restore a pinned theme before anything reads the palette. Without a pin we
// leave the attribute off entirely so the OS preference wins. `coon.theme` is
// the key this moved out of, read once so nobody loses a pinned theme.
try {
  const saved = prefs().theme || localStorage.getItem('coon.theme');
  if (saved) document.documentElement.dataset.theme = saved;
} catch { /* private mode / blocked storage: fall back to the OS preference */ }

const css = (name) => getComputedStyle(document.documentElement).getPropertyValue(name).trim();

// Node geometry mirrors core::layout — the server lays out in these exact
// pixels and sends node centres, so positions are used verbatim here. Keep
// these in step with NODE_WIDTH / NODE_HEIGHT in crates/core/src/layout.rs.
const NODE_W = 150;
const NODE_H = 32;
const LABEL_MAX = 18;

const truncateTo = (name, max) => (name.length > max ? `${name.slice(0, max - 1)}…` : name);
const truncate = (name) => truncateTo(name, LABEL_MAX);

// Room a group box leaves around its contents, and how far its label clears
// the top border. Compound parents size themselves around their children, so
// this padding is the only thing standing between a box's border and the first
// node inside it. The label rides above the border rather than inside that
// band: the band is exactly where every edge reaching a node inside the box
// crosses, and edges are painted over a compound parent whatever z-index it is
// given, so a name written in there gets struck through.

/// Built from the CSS custom properties, so the canvas and the chrome stay on
/// one palette — and re-running it is all a theme switch needs.
function graphStyle() {
  return [
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
        'height': NODE_H,
        'shape': 'roundrectangle',
        'transition-property': 'border-color, border-width, background-color, opacity',
        'transition-duration': '90ms',
      }
    },
    { selector: 'node.hovered', style: { 'background-color': css('--surface-hover') } },
    // A node is either selected or it is not — those are the only two states a
    // border carries, and there is only ever one selected node. It is the
    // accent against every other node's neutral hairline, and it carries no
    // fill: the ground stays the surface they all sit on. The graph root used
    // to wear a ring of its own at a lighter weight, which made the border a
    // three-way signal the eye had to measure rather than read; the breadcrumb
    // already says where the tree is rooted. Declared after `hovered` so that
    // running the pointer over the selection does not wash it out.
    {
      selector: 'node.focused',
      style: {
        'border-color': css('--accent'),
        'border-width': 2,
      }
    },
    // A node with children it isn't showing gets a dashed edge — the same
    // signal whether the user collapsed it or the depth limit cut it off.
    { selector: 'node.has-hidden', style: { 'border-style': 'dashed' } },
    // The path from the root to the selected node, drawn on the edges only.
    { selector: 'edge.on-path', style: { 'line-color': css('--accent-border'), 'target-arrow-color': css('--accent-border'), 'width': 1.5 } },
    // Transient hover focus: everything not adjacent recedes.
    { selector: '.faded', style: { 'opacity': 0.18 } },
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

    // ── Cluster map ──
    // A cluster is named, not called: its label is prose about a group of
    // functions rather than a symbol, so it takes the UI stack while the
    // functions elsewhere keep the mono one.
    {
      selector: 'node.community',
      style: {
        'font-family': css('--font-ui'),
        'text-wrap': 'ellipsis',
        'text-max-width': NODE_W - 16,
      }
    },
  ];
}

const cy = cytoscape({
  container: document.getElementById('cy'),
  style: graphStyle(),
  layout: { name: 'preset' },
  wheelSensitivity: 0.2,
});

// ── State ────────────────────────────────────────────────────────────────────

let allFunctions     = {};                        // id → { name, qualified_name }
let currentGraph     = { nodes: {}, edges: [] };  // cached full graph
let rootNodeId       = null;                      // graph root (active workspace)
let selectedNodeId   = null;                      // last focused node
let currentDirection = 'Outgoing';
let currentOrientation = 'LeftRight';   // or 'TopDown'; the server lays both out
let workspaces       = [];
let activeWorkspace  = 0;
let graphDepth       = 3;     // levels from the root, or null for no bound at all
let collapsed        = new Set();
let currentView      = 'ClusterMap';  // or 'Rooted'; a workspace opens on the map
let clusterMap       = null;          // the laid-out map, while one is showing
let mapScope         = null;          // the function the map is drawn from, or null for all of it
let mapLevel         = 0;             // which rung of the coarsening ladder is showing
let mapLevels        = 0;             // how many rungs there are; 0 while nothing is clustered
let mapClusters      = 0;             // how many boxes this rung holds — what the pill reads out
let mapLevelSizes    = [];            // how many boxes every rung holds, finest first
let mapOpening       = 0;             // the rung the method itself stands behind
let clusterMethod    = null;          // which algorithm built the map
let clusterMethods   = [];            // every algorithm on offer, each with its own knob
let clusterSettings  = null;          // and what the map was built to: method plus knobs
let clusterTopSpec   = null;          // the knob that belongs to no one method
let selectedClusterId = null;         // the box pointed at on the map, if it is still on this rung
let lastFitKey       = null;                      // refit only on root/direction change
let parseReport      = null;          // how much of the build was parsed; null under clangd
let capabilities     = null;          // what the backend can be asked; null until the first snapshot
let bodilessCallees  = null;          // which calls to a function with no body the graph keeps
let bodilessRestored = false;         // whether the remembered choice has been handed over yet
let readyShown       = false;         // whether the status line is the ready line, to redraw it

const MAX_FIT_ZOOM = 1.15;
// Below roughly this scale a 12px label stops being readable, so auto-fit
// stops zooming out and the user pans instead. Manual zoom is unbounded.
const MIN_FIT_ZOOM = 0.7;

// ── DOM refs ─────────────────────────────────────────────────────────────────

const $ = (id) => document.getElementById(id);
const statusEl   = $('status');
const lspDotEl   = $('lsp-dot');
const infoEl     = $('info');
const searchEl   = $('search');
const fnListEl   = $('fn-list');
const fnCountEl  = $('fn-count');
const dirToggleEl = $('dir-toggle');
const orientLrEl = $('orient-lr');
const orientTdEl = $('orient-td');
const tabsEl     = $('tabs');
const crumbEl    = $('breadcrumb');
const statusBarEl   = $('status-bar');
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
  parseReport     = msg.parse ?? null;
  capabilities    = msg.capabilities ?? null;
  bodilessCallees = msg.bodiless_callees ?? null;
  restoreBodilessCallees();
  updateBodilessControl();
  if (readyShown) showReady();
  // The demo has nothing to reload from, so it gets no button that does
  // nothing. Before the first snapshot the button stays, as it always was.
  $('refresh-btn').hidden = canRefresh() === false;

  const active = workspaces[activeWorkspace];
  rootNodeId = active?.selected ?? null;
  collapsed = new Set(active?.collapsed || []);
  // The map rides along on the snapshot and is absent while the workspace is
  // showing the call tree, so the two always agree about which view this is.
  currentView = active?.view ?? 'Rooted';
  clusterMap = msg.cluster_layout ?? null;
  mapScope = active?.map_scope ?? null;
  mapLevel = active?.map_level ?? 0;
  mapLevels = active?.map_levels ?? 0;
  mapClusters = active?.map_clusters ?? 0;
  mapLevelSizes = active?.map_level_sizes ?? [];
  mapOpening = active?.map_opening_level ?? 0;
  clusterMethod = active?.method ?? null;
  if (active?.cluster) clusterSettings = active.cluster;
  if (Array.isArray(msg.cluster_methods)) clusterMethods = msg.cluster_methods;
  if (msg.cluster_top) clusterTopSpec = msg.cluster_top;
  // `null` is the depth with no bound, so a missing workspace is the only
  // reason to keep what is on screen — not a workspace that reports one.
  if (active && 'depth' in active) {
    graphDepth = active.depth ?? null;
    if (graphDepth !== DEPTH_ALL) lastBoundedDepth = graphDepth;
  }
  updateDepthControl();
  updateClusterControl();
  renderClusterPanel();
  // Last, because it answers with commands: whatever this workspace is missing
  // of the remembered settings arrives on the next snapshot, not this one.
  restoreWorkspace(active);
  if (active?.direction) {
    currentDirection = active.direction;
    updateDirToggle();
  }
  if (active?.orientation) {
    currentOrientation = active.orientation;
    updateOrientToggle();
  }

  renderTabs();
  if (currentView === 'ClusterMap') {
    // The map always rides along with this view, but an empty one is a real
    // answer — before any symbols load there is nothing to cluster.
    rebuildClusterMap(clusterMap ?? {});
  } else {
    // And the rooted layout rides along with this one, on the same terms: it
    // is absent under the map, and an empty one is a real answer — a tab with
    // no root yet has an empty canvas.
    rebuildGraph(currentGraph, msg.layout ?? {});
  }
  renderSidebar(searchEl.value);
  renderBreadcrumb();

  const n = Object.keys(allFunctions).length;
  fnCountEl.textContent = n ? `(${n})` : '';
  statSymbolsEl.textContent = n.toLocaleString();
  // What is actually on the canvas. Only the view being drawn is laid out, so
  // this reads the one that came with the view — the other is not there at all.
  const placed = currentView === 'ClusterMap'
    ? Object.keys(clusterMap?.nodes || {}).length
    : Object.keys(msg.layout || {}).length;
  statNodesEl.textContent = placed.toLocaleString();

  if (n > 0 && placed === 0) {
    setInfo('Ctrl+P to find a symbol · Ctrl+Shift+P for commands');
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

/// One crumb, appended with the separator every entry but the first needs.
function addCrumb(label, { title, current, run }) {
  if (crumbEl.children.length > 0) {
    const sep = document.createElement('span');
    sep.className = 'sep';
    sep.textContent = '›';
    crumbEl.appendChild(sep);
  }
  const crumb = document.createElement('button');
  crumb.className = 'crumb' + (current ? ' current' : '');
  crumb.textContent = label;
  if (title) crumb.title = title;
  crumb.addEventListener('click', run);
  crumbEl.appendChild(crumb);
  return crumb;
}

/// The maps this view sits under: the whole program's, and — when the map has
/// been scoped to a function — that function's.
///
/// Two crumbs rather than one because there are now two maps to go back to,
/// and which of them 'Cluster map' means is exactly what a single crumb could
/// not say. The scope crumb is the head of the path whether the user is
/// looking at that map or at a tree entered from it, so the lineage reads the
/// same from either.
function renderMapCrumbs() {
  const onMap = currentView === 'ClusterMap';
  addCrumb('Cluster map', {
    title: mapScope ? 'Out to the whole project' : 'Back to the cluster map (Ctrl+M)',
    current: onMap && !mapScope,
    // Widening is a different map, so it is a scope command and not a view one;
    // on the whole-program map already, this is just a refit.
    run: () => (mapScope ? sendCommand({ SetMapScope: null }) : showClusterMap()),
  }).classList.add('view');

  if (!mapScope) return;
  const name = allFunctions[mapScope]?.name ?? mapScope;
  addCrumb(name, {
    title: `Communities under ${allFunctions[mapScope]?.qualified_name ?? name}`,
    current: onMap,
    run: () => (onMap ? fitGraph() : sendCommand({ SetMapScope: mapScope })),
  }).classList.add('view');
}

function renderBreadcrumb() {
  crumbEl.innerHTML = '';

  // The map has no root and no path through it; what it has is a rung of the
  // coarsening ladder and however many boxes that rung holds.
  if (currentView === 'ClusterMap') {
    renderMapCrumbs();

    const shown = clusterMap?.metas?.length ?? 0;
    const note = document.createElement('span');
    note.className = 'placeholder';
    note.textContent = shown === 0
      ? 'Nothing clustered yet'
      : `${shown} ${shown === 1 ? 'cluster' : 'clusters'}` +
        (mapLevels > 1 ? ` · grouping ${mapLevel + 1} of ${mapLevels}` : '');
    crumbEl.appendChild(note);
    return;
  }

  // The map is where a rooted tree came from: picking a function is the
  // gesture that left it, so the way back belongs at the head of the path and
  // not only in the palette. Shown even with no root, where it is the only
  // thing there is to do from here.
  renderMapCrumbs();

  if (!rootNodeId) {
    const span = document.createElement('span');
    span.className = 'placeholder';
    span.textContent = 'No graph selected';
    crumbEl.appendChild(span);
    return;
  }

  // The map crumbs precede the whole path, so every entry has something to its
  // left to be separated from.
  const path = pathFromRoot(rootNodeId, selectedNodeId);
  path.forEach((id, i) => {
    addCrumb(allFunctions[id]?.name ?? id, {
      title: allFunctions[id]?.qualified_name ?? '',
      current: i === path.length - 1,
      // A crumb is a place on the path, so clicking one goes back to it — the
      // same gesture as clicking the node, with the same non-effect on the
      // graph.
      run: () => focusNode(id),
    });
  });
}

/// Shortest root→target path over the current edges, following call direction.
function pathFromRoot(root, target) {
  if (!target || target === root) return [root];

  const adj = adjacency();
  const prev = { [root]: null };
  const queue = [root];
  while (queue.length) {
    const node = queue.shift();
    if (node === target) {
      const path = [];
      for (let at = target; at != null; at = prev[at]) path.unshift(at);
      return path;
    }
    for (const next of adj.get(node) || NO_CHILDREN) {
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
  // core::compute_layout works in the same pixels the canvas draws in and
  // reports node centres, so positions go straight through.
  for (const [id, node] of Object.entries(graph.nodes || {})) {
    const pos = layout[id];
    if (!pos) continue;   // not part of the visible subgraph
    const hidden = hiddenChildCount(id, layout);
    cy.add({
      group: 'nodes',
      data: {
        id,
        label: truncate(node.name) + (hidden > 0 ? `  +${hidden}` : ''),
        full: node.qualified_name || node.name,
      },
      position: { x: pos.x, y: pos.y },
    });
    if (hidden > 0) cy.$id(id).addClass('has-hidden');
  }

  const outgoing = currentDirection === 'Outgoing';
  const edgesSeen = new Set();
  for (const edge of (graph.edges || [])) {
    // Drop edges leaving a collapsed node. Without this, collapsing a node in a
    // DAG is often invisible: its children stay on screen because other callers
    // reach them, and the edges still point at them.
    const parent = outgoing ? edge.caller : edge.callee;
    if (collapsed.has(parent)) continue;

    const eid = `${edge.caller}=>${edge.callee}`;
    if (!edgesSeen.has(eid) && cy.$id(edge.caller).length && cy.$id(edge.callee).length) {
      edgesSeen.add(eid);
      cy.add({ group: 'edges', data: { id: eid, source: edge.caller, target: edge.callee } });
    }
  }

  if (selectedNodeId) cy.$id(selectedNodeId).addClass('focused');
  markPathToSelection();

  // Only auto-fit when the tree itself changes — a new root, the other
  // direction, the other orientation. Expanding a node appends to this tree and
  // must not yank the viewport the user just positioned, so the node they
  // expanded is deliberately not part of the key.
  const fitKey = `${rootNodeId}:${currentDirection}:${currentOrientation}`;
  if (cy.nodes().length > 0 && fitKey !== lastFitKey) {
    lastFitKey = fitKey;
    requestAnimationFrame(fitGraph);
  }
}

/// Frame the whole graph. Shared by auto-fit and the explicit command.
function fitGraph() {
  if (cy.nodes().length === 0) return;
  // Cytoscape caches the container size from init; without this the fit is
  // computed against stale dimensions and the graph lands off-centre.
  cy.resize();
  cy.fit(cy.nodes(), 60);

  // Clamp the automatic zoom at both ends; manual zoom stays unbounded.
  const zoom = cy.zoom();
  if (zoom > MAX_FIT_ZOOM) {
    // A handful of nodes fitted to a large canvas renders at a cartoon size.
    cy.zoom(MAX_FIT_ZOOM);
    cy.center(cy.nodes());
  } else if (zoom < MIN_FIT_ZOOM) {
    // Below this, labels stop being readable and a large graph is just a
    // hairball. Keep text legible and start the user at the focused node so
    // they pan outwards, rather than fitting everything illegibly.
    cy.zoom(MIN_FIT_ZOOM);
    const anchor = cy.$id(selectedNodeId ?? rootNodeId);
    cy.center(anchor.length ? anchor : cy.nodes());
  }
}


// ── Cluster map ───────────────────────────────────────────────────────────────
// What a workspace opens on: one rung of the coarsening ladder the backend
// built out of call structure. Every cluster at that rung is a single node
// standing in for the functions inside it, with aggregated edges for the calls
// that cross between them.
//
// The map is flat, and that is the whole design. Every rung partitions the same
// functions, so a rung *is* a whole map and exactly one is on screen — zooming
// redraws all of it a step finer or coarser rather than opening one box. The
// ladder halves at each round, so a box opened in place would reveal two boxes
// and a user would click eleven times to travel what one control walks in a
// step.

/// The wire id for a cluster, matching `LayoutNodeId::Cluster`'s Display. Every
/// other id arrives as a string already and is used verbatim as an element id.
const clusterId = (key) => `cluster:${key.level}:${key.anchor}`;

const functionsLabel = (n) => `${n} function${n === 1 ? '' : 's'}`;

function rebuildClusterMap(map) {
  cy.elements().remove();

  const metas = new Map((map.metas ?? []).map((meta) => [clusterId(meta.key), meta]));
  for (const [id, pos] of Object.entries(map.nodes || {})) {
    const meta = metas.get(id);
    if (!meta) continue;   // the map draws clusters and nothing else

    // The count is the point of a box, so it gets its share of the label budget
    // first and the name is cut to what is left — truncating the name alone
    // leaves the count to be ellipsised away by the node's width.
    const count = `  +${meta.count}`;
    const data = {
      id,
      key: meta.key,
      name: meta.label,
      label: truncateTo(meta.label, LABEL_MAX - count.length) + count,
      full: `${meta.label} — ${functionsLabel(meta.count)}`,
    };

    // Positions are node centres in the pixels the canvas draws in, so they go
    // straight through — no cytoscape layout runs over the map.
    cy.add({ group: 'nodes', data, position: { x: pos.x, y: pos.y } });
    // A box is hiding everything it stands for, which is the same thing a
    // collapsed node in the rooted view is doing: dashed, plus the +N.
    cy.$id(id).addClass('community has-hidden');
  }

  for (const edge of map.edges || []) {
    const id = `${edge.source}=>${edge.target}`;
    // Drawn exactly like a call edge in the rooted view. The weight (how many
    // calls cross between the two boxes) used to scale the line width, which
    // made the map read as a different kind of picture from the graph it
    // summarises; one line, one arrow, the same as everywhere else.
    cy.add({ group: 'edges', data: { id, source: edge.source, target: edge.target } });
  }

  // Put the mark back on the box the user pointed at. A snapshot tears the
  // canvas down for reasons that have nothing to do with them — a batch of
  // warmup edges landing, a regrouping — and a mark that vanished then would
  // read as a click that stopped working. The rung is part of the element id,
  // so a zoom step clears it without a rule of its own: the box that was
  // pointed at is not on this map any more.
  if (selectedClusterId) {
    const marked = cy.$id(selectedClusterId);
    if (marked.length) marked.addClass('focused');
    else selectedClusterId = null;
  }

  // The rung is deliberately *not* part of the key. Every box is placed where
  // the functions it holds sit (`core::layout::MapFrame`), so a zoom step
  // merges boxes in place rather than redrawing a different picture of the same
  // program — and refitting would throw away the viewport the user had over a
  // map that has not moved under it. `View: Fit Graph to Screen` is there for
  // when they want it back. Everything else in the key changes what is being
  // drawn, not how much of it.
  const fitKey = `cluster:${mapScope ?? 'all'}:${clusterMethod}:${currentDirection}:${currentOrientation}`;
  if (cy.nodes().length > 0 && fitKey !== lastFitKey) {
    lastFitKey = fitKey;
    requestAnimationFrame(fitGraph);
  }
}

/// One click on the map: point at a cluster, and say what it holds.
///
/// Pointing, not acting — the same rule the rooted view follows. There is
/// nothing to open here: the rung below is a step of the zoom control, and a
/// click that redrew the whole map would be a very large thing to do by
/// accident while aiming at a label.
function clusterTap(node) {
  const key = node.data('key');
  if (!key) return;
  // The same accent border a pointed-at node wears in the rooted view, and for
  // the same reason: the info line alone leaves the box the user just hit
  // looking exactly like every other one. One mark, one meaning — a second
  // class for "selected here" would be a second signal saying the first one's
  // thing. Exactly one box wears it, so the old one gives it up first.
  selectedClusterId = node.id();
  cy.nodes().removeClass('focused');
  node.addClass('focused');
  setInfo(node.data('full'));
}

/// Two clicks: leave the map for the call tree, rooted at the entry point the
/// clustering picked as this cluster's front door.
function clusterDrill(node) {
  const key = node.data('key');
  if (!key) return;
  setInfo(`Drilling into ${node.data('name')}`);
  sendCommand({ DrillIntoCluster: key });
}

// ── Cluster map settings ─────────────────────────────────────────────────────
// One pill and one panel for everything that decides what the map shows: which
// algorithm builds it, the knob that algorithm reads, how far the ladder
// coarsens, and which rung of it is drawn. The pill stays a readout — method
// and box count — because a setting whose value appears only once you open
// something is a setting nobody checks; the knobs live behind it because the
// toolbar is not where five sliders belong.
//
// Rung 0 is the finest map and `mapLevels - 1` the coarsest. Every control
// here reads out how many boxes a rung holds rather than which rung it is: the
// count is the thing the user is choosing between, and the rung number is an
// implementation detail of the ladder underneath it.
//
// Two of these settings rebuild the map on a worker thread and two do not, and
// the panel does not say which is which. What it does instead is wait: a
// slider sends nothing until it is let go, so a drag across a range is one
// coarsening run and not thirty.
//
// Everything here is remembered, with the rest of the view settings, in the
// one store at the top of this file.

const clusterMenuEl   = $('cluster-menu');
const clusterToggleEl = $('cluster-control');
const clusterPanelEl  = $('cluster-panel');

/// Workspaces this session has already handed the remembered settings to.
///
/// First sight only, and never again. A preference that kept re-asserting
/// itself would undo every change the user made after it, which is the one way
/// a remembered setting is worse than no memory at all. The rung is tracked
/// separately because it cannot be restored at the same moment: it needs a
/// ladder to land on, and the settings that build that ladder have only just
/// been sent.
const settingsRestored = new Set();
const levelRestored    = new Set();

const near = (a, b) => Math.abs((a ?? 0) - (b ?? 0)) < 1e-6;

function sameSettings(a, b) {
  return !!a && !!b
    && a.method === b.method
    && near(a.resolution, b.resolution)
    && near(a.inflation, b.inflation)
    && a.target_top === b.target_top;
}

/// Hand every remembered view setting to a workspace the first time it is
/// seen: how deep the tree goes, which way it is read, which way it flows, and
/// what builds the map.
///
/// Absolute commands throughout, including for the direction — which is why
/// `SetCallDirection` exists beside the toggle. A restore knows which way it
/// wants to read; "the other one from whatever is there" is a coin flip
/// dressed up as a setting.
///
/// The map's rung is stored as a *box count*, not a rung number, for the same
/// reason every control reads that out: a ladder built to other settings is a
/// different height, so rung 3 of one is not rung 3 of the next, where "about
/// twenty boxes" means the same on both.
function restoreWorkspace(ws) {
  if (!ws) return;
  const saved = prefs();

  if (!settingsRestored.has(ws.id)) {
    settingsRestored.add(ws.id);
    // `null` is a remembered setting — no bound — where `undefined` is nothing
    // remembered at all. The two are not interchangeable here, so this asks
    // for the key by name rather than for a truthy value.
    const wantDepth = 'depth' in saved ? saved.depth ?? DEPTH_ALL : undefined;
    if (wantDepth !== undefined && wantDepth !== (ws.depth ?? DEPTH_ALL)) {
      sendCommand({ SetGraphDepth: wantDepth });
    }
    if (saved.direction && saved.direction !== ws.direction) {
      sendCommand({ SetCallDirection: saved.direction });
    }
    if (saved.orientation && saved.orientation !== ws.orientation) {
      sendCommand({ SetGraphOrientation: saved.orientation });
    }
    if (saved.cluster && !sameSettings(saved.cluster, ws.cluster)) {
      sendCommand({ SetClusterSettings: saved.cluster });
      // The rung waits for the ladder these settings build. Landing one from
      // the old ladder on the new one is how a restore puts a user somewhere
      // neither of them asked for.
      return;
    }
  }

  const sizes = ws.map_level_sizes || [];
  if (levelRestored.has(ws.id) || !saved.clusters || sizes.length === 0) return;
  if (saved.cluster && !sameSettings(saved.cluster, ws.cluster)) return;
  levelRestored.add(ws.id);
  const want = nearestRung(sizes, saved.clusters);
  if (want !== ws.map_level) sendCommand({ SetMapLevel: want });
}

/// The rung whose box count comes closest to the one remembered, ties to the
/// finer — more boxes is the one that can still be zoomed out of.
function nearestRung(sizes, clusters) {
  let best = 0;
  for (let i = 1; i < sizes.length; i++) {
    if (Math.abs(sizes[i] - clusters) < Math.abs(sizes[best] - clusters)) best = i;
  }
  return best;
}

// ── The pill ──────────────────────────────────────────────────────────────────

function updateClusterControl() {
  const onMap = currentView === 'ClusterMap';
  const live = onMap && mapLevels > 0;
  const label = clusterMethods.find((m) => m.id === clusterMethod)?.label ?? 'Clusters';
  $('cluster-summary').textContent = live ? `${label} · ${mapClusters} clusters` : label;
  clusterToggleEl.classList.toggle('inactive', !onMap);
  clusterToggleEl.title = !onMap
    ? 'These settings build the cluster map (Ctrl+M)'
    : live
      ? `${label} · ${mapClusters} clusters, grouping ${mapLevel + 1} of ${mapLevels}`
      : 'Nothing is clustered yet';
}

function clusterPanelOpen() {
  return !clusterPanelEl.hidden;
}

function openClusterPanel() {
  clusterPanelEl.hidden = false;
  clusterToggleEl.setAttribute('aria-expanded', 'true');
  renderClusterPanel();
}

function closeClusterPanel() {
  clusterPanelEl.hidden = true;
  clusterToggleEl.setAttribute('aria-expanded', 'false');
}

function toggleClusterPanel() {
  if (clusterPanelOpen()) closeClusterPanel(); else openClusterPanel();
}

clusterToggleEl.addEventListener('click', toggleClusterPanel);

// Closed by clicking away or by Escape. A menu that only closes by the button
// that opened it is one the user has to aim at twice.
document.addEventListener('pointerdown', (e) => {
  if (clusterPanelOpen() && !clusterMenuEl.contains(e.target)) closeClusterPanel();
});
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || !clusterPanelOpen() || CoonPalette.isOpen()) return;
  closeClusterPanel();
  clusterToggleEl.focus();
});

// ── The panel ─────────────────────────────────────────────────────────────────

function renderClusterPanel() {
  if (!clusterPanelOpen()) return;
  // A snapshot lands every second or so while a project is loading, and
  // rebuilding this under a thumb on a slider would drop the drag. The panel
  // is a generation stale for as long as a control is being used, which is the
  // one moment its readouts are coming from the user anyway.
  const busy = clusterPanelEl.contains(document.activeElement)
    && document.activeElement.tagName === 'INPUT';
  if (busy) return;

  renderMethodRows();
  renderKnobs();
  renderRungs();
  $('cluster-memory').textContent = currentView === 'ClusterMap'
    ? 'Remembered for next time'
    : 'Applies to the cluster map (Ctrl+M)';
}

/// One row per method: the name, and the claim it makes about the program.
///
/// Drawn from what the snapshot sent, so a method added in `logic` arrives
/// here with its description and its own knob and needs nothing written for it.
function renderMethodRows() {
  const list = $('method-list');
  list.innerHTML = '';
  for (const method of clusterMethods) {
    const row = document.createElement('button');
    row.className = 'method-row';
    row.type = 'button';
    row.setAttribute('role', 'radio');
    row.setAttribute('aria-checked', String(method.id === clusterMethod));

    const name = document.createElement('span');
    name.className = 'method-name';
    name.textContent = method.label;
    const why = document.createElement('span');
    why.className = 'method-why';
    why.textContent = method.description;

    row.append(name, why);
    row.addEventListener('click', () => setClusterMethod(method.id));
    list.appendChild(row);
  }
}

/// The knobs: the one the chosen method reads, then the shared one.
function renderKnobs() {
  const list = $('knob-list');
  list.innerHTML = '';
  if (!clusterSettings) return;
  const grain = clusterMethods.find((m) => m.id === clusterMethod)?.grain;
  for (const spec of [grain, clusterTopSpec]) {
    if (spec) list.appendChild(knobRow(spec));
  }
}

/// One slider, drawn entirely from the spec the backend sent — range, step,
/// wording and default. Nothing about a knob is written on this side, so a
/// knob retuned in `logic` is retuned here too, and a method that arrives with
/// a knob of its own arrives with its control already drawn.
function knobRow(spec) {
  const row = document.createElement('div');
  row.className = 'knob';

  const input = document.createElement('input');
  input.type = 'range';
  input.id = `knob-${spec.field}`;
  input.min = spec.min;
  input.max = spec.max;
  input.step = spec.step;
  input.value = clusterSettings[spec.field];
  input.disabled = !clusterSettings;

  const head = document.createElement('div');
  head.className = 'knob-head';
  const label = document.createElement('label');
  label.htmlFor = input.id;
  label.textContent = spec.label;
  const value = document.createElement('span');
  value.className = 'knob-value';
  value.textContent = formatSetting(spec, Number(input.value));
  head.append(label, value);

  const why = document.createElement('div');
  why.className = 'knob-why';
  why.textContent = spec.description;

  // The readout follows the thumb; the command waits for it to be let go. A
  // knob that moves asks for a coarsening no cache holds, and sending one per
  // pixel of a drag queues a dozen ladders to throw away.
  input.addEventListener('input', () => {
    value.textContent = formatSetting(spec, Number(input.value));
  });
  input.addEventListener('change', () => {
    sendClusterSettings({ ...clusterSettings, [spec.field]: Number(input.value) });
  });

  row.append(head, input, why);
  return row;
}

/// As many decimals as the step has, so a quarter-step knob reads 1.25 and a
/// whole-step one reads 12 rather than 12.00.
function formatSetting(spec, value) {
  const places = spec.step >= 1 ? 0 : spec.step >= 0.1 ? 1 : 2;
  return value.toFixed(places);
}

/// The zoom, as the ladder actually is: a row of grains, each one a box count.
function renderRungs() {
  const list = $('grouping-list');
  list.innerHTML = '';
  // The knobs above still mean something off the map — they decide what it
  // will be when the user goes back to it — but a rung does not: there is no
  // ladder on screen to land on, and `setMapLevel` refuses. A control that
  // refuses is worse than one that says it is not for here.
  const live = currentView === 'ClusterMap' && mapLevels > 0;
  const count = live ? mapLevelSizes[mapLevel] ?? mapClusters : null;
  const valueEl = $('grouping-value');
  valueEl.textContent = live ? `${count} ${count === 1 ? 'cluster' : 'clusters'}` : '—';

  const input = document.createElement('input');
  input.type = 'range';
  input.id = 'grouping-range';
  input.min = 0;
  input.max = Math.max(mapLevels - 1, 0);
  input.step = 1;
  input.value = mapLevel;
  input.disabled = !live;
  input.setAttribute('aria-label', 'Grouping level');
  input.addEventListener('input', () => {
    const at = mapLevelSizes[Number(input.value)];
    if (at != null) valueEl.textContent = `${at} ${at === 1 ? 'cluster' : 'clusters'}`;
  });
  input.addEventListener('change', () => setMapLevel(Number(input.value)));

  const scale = document.createElement('div');
  scale.className = 'rung-scale';
  const finest = document.createElement('span');
  finest.textContent = 'finest';
  const coarsest = document.createElement('span');
  coarsest.textContent = 'coarsest';

  // The rung the method itself stands behind, marked and reachable. The rungs
  // past it exist only to force a wide optimum down to a box count — they are
  // where a user zooms out *to*, not the method's answer — and nothing else on
  // screen says which one is which.
  const natural = document.createElement('button');
  natural.className = 'natural';
  natural.type = 'button';
  if (live && mapOpening < mapLevelSizes.length) {
    const at = mapLevelSizes[mapOpening];
    natural.textContent = mapLevel === mapOpening
      ? '● the method’s own grouping'
      : `○ natural: ${at}`;
    natural.title = 'The grouping the method itself stands behind; the rungs above it merge to fit a box count';
    natural.disabled = mapLevel === mapOpening;
    natural.addEventListener('click', () => setMapLevel(mapOpening));
  }

  scale.append(finest, natural, coarsest);
  list.append(input, scale);
}

// ── What the panel sends ──────────────────────────────────────────────────────

/// Build the map to these settings, and remember them.
///
/// Remembered here rather than off the snapshot, because a snapshot describes
/// whatever workspace happens to be active — including a fresh tab sitting on
/// the defaults, which would overwrite the preference with them.
function sendClusterSettings(next) {
  if (!clusterSettings || sameSettings(next, clusterSettings)) return;
  savePrefs({ cluster: next });
  sendCommand({ SetClusterSettings: next });
}

function setClusterMethod(id) {
  if (!clusterSettings || id === clusterMethod) return;
  sendClusterSettings({ ...clusterSettings, method: id });
}

/// Put every knob back where `logic` says it belongs, method untouched: this
/// resets how the map is built, not what builds it, and a user who picked
/// Leiden did not ask to leave it.
function resetClusterSettings() {
  if (!clusterSettings) return;
  const defaults = {};
  for (const spec of [...clusterMethods.map((m) => m.grain), clusterTopSpec]) {
    if (spec) defaults[spec.field] = spec.default;
  }
  // And forget the remembered grain with them, so the map goes back to opening
  // where the method says rather than where this user last dragged it.
  savePrefs({ clusters: null });
  sendClusterSettings({ ...clusterSettings, ...defaults });
  renderClusterPanel();
}

$('cluster-reset').addEventListener('click', resetClusterSettings);

/// Draw the map at one rung. The only funnel — the panel, the palette and
/// Ctrl+[ / Ctrl+] all come through here, so the grain is remembered however
/// it was asked for.
function setMapLevel(next) {
  if (currentView !== 'ClusterMap' || mapLevels === 0) return;
  const clamped = Math.min(Math.max(next, 0), mapLevels - 1);
  if (clamped === mapLevel) return;
  savePrefs({ clusters: mapLevelSizes[clamped] ?? mapClusters });
  sendCommand({ SetMapLevel: clamped });
}

// ── Graph relationships ───────────────────────────────────────────────────────
// The client holds the whole call graph, not just the laid-out subset, so it
// can answer "is there more below this node?" without another round trip.

/// Adjacency in the current direction, built once per graph and kept.
///
/// Both readers come in bursts over edges that are not moving: one click walks
/// `pathFromRoot` twice — the breadcrumb, then the path highlight — and one
/// rebuild asks `hiddenChildCount` about every node it places. Building the map
/// inside each of them made all of that O(edges) apiece, over a graph that
/// changes only when a snapshot lands.
///
/// The graph and the direction it is read in are the whole key, because both
/// arrive on a snapshot and `currentGraph` is replaced wholesale rather than
/// edited, so identity is enough. Collapse is deliberately not in here:
/// `hiddenChildCount` and `rebuildGraph` apply that themselves, so one index
/// serves a collapsed node and an open one alike.
const NO_CHILDREN = [];
let adjacencyCache = null;

function adjacency() {
  if (adjacencyCache && adjacencyCache.graph === currentGraph && adjacencyCache.direction === currentDirection) {
    return adjacencyCache.map;
  }
  const outgoing = currentDirection === 'Outgoing';
  const map = new Map();
  // `core::protocol::wire_graph` already sends one edge per caller/callee pair
  // rather than one per call site, so there is nothing here to dedupe.
  for (const edge of currentGraph.edges || []) {
    const [from, to] = outgoing ? [edge.caller, edge.callee] : [edge.callee, edge.caller];
    const kids = map.get(from);
    if (kids) kids.push(to);
    else map.set(from, [to]);
  }
  adjacencyCache = { graph: currentGraph, direction: currentDirection, map };
  return map;
}

/// Children of `id` in the current direction, as ids. The array is the index's
/// own, so callers read it and do not keep or change it.
function childrenOf(id) {
  return adjacency().get(id) || NO_CHILDREN;
}

/// How many of a node's children are not shown *as its children* — because the
/// user collapsed it, or because the depth limit stopped there.
///
/// A collapsed node counts all of them. This is a DAG, not a tree: a child may
/// still be on screen because something else calls it, but it is no longer
/// shown hanging off this node.
function hiddenChildCount(id, layout) {
  const kids = childrenOf(id);
  if (kids.length === 0) return 0;
  if (collapsed.has(id)) return kids.length;
  return kids.filter((k) => !(k in layout)).length;
}

/// Light the edges from the root down to the selected node, so a dense graph
/// still shows the route the breadcrumb is describing. The nodes along it are
/// deliberately left alone: only the selected one wears the selection.
function markPathToSelection() {
  cy.elements().removeClass('on-path');
  if (!rootNodeId || !selectedNodeId || selectedNodeId === rootNodeId) return;

  const path = pathFromRoot(rootNodeId, selectedNodeId);
  path.forEach((id, i) => {
    if (i > 0) {
      cy.$id(`${path[i - 1]}=>${id}`).addClass('on-path');
      cy.$id(`${id}=>${path[i - 1]}`).addClass('on-path');
    }
  });
}

// ── View ──────────────────────────────────────────────────────────────────────

function showClusterMap() {
  if (currentView !== 'ClusterMap') sendCommand('ZoomOutToClusters');
}

/// Which function a 'cluster this' gesture is about: the node in focus, or the
/// root of the tree when nothing has been touched since it opened.
function scopeTarget() {
  return selectedNodeId ?? rootNodeId;
}

/// Redraw the map from one function's neighbourhood rather than the whole
/// project — the same coarsening, run again over what this function reaches,
/// which is a different question and usually a different answer.
///
/// The backend declines a function with nothing to group and says so, so there
/// is nothing to check for here.
function clusterThisFunction() {
  // A map has no function in focus to be about, and the node the tree left
  // selected is not on it — so from here this is a request with no subject.
  const id = currentView === 'ClusterMap' ? null : scopeTarget();
  if (!id) {
    setInfo('Open a call tree first — this groups what a function reaches');
    return;
  }
  setInfo(`Grouping what ${allFunctions[id]?.name ?? id} reaches`);
  sendCommand({ SetMapScope: id });
}

/// Ctrl+M. A toggle rather than two bindings because a key has no label to
/// read as a lie — the objection that keeps the *visible* view control a pair
/// of named segments does not apply here.
///
/// Going back to the tree needs a tree to go back to: a workspace that has
/// never been rooted would land on the 'No graph selected' canvas, so on the
/// map with no root this does nothing rather than stranding the user there.
function toggleView() {
  if (currentView === 'ClusterMap') {
    if (rootNodeId) sendCommand({ SetViewMode: 'Rooted' });
  } else {
    sendCommand('ZoomOutToClusters');
  }
}

// ── Depth ─────────────────────────────────────────────────────────────────────

const DEPTH_MIN = 1;
const DEPTH_MAX = 8;
/// Where the toggle lands a workspace that has never held a number — the
/// backend's `DEFAULT_DEPTH`, so putting a limit back puts back the one a
/// fresh tab opens with.
const DEPTH_DEFAULT = 3;
/// No bound at all: walk until the calls run out. `null` rather than a large
/// number, because there is no number that means "as far as it goes" and a
/// sentinel is something every reader has to be told about. It is what the
/// backend's `GraphDepth::Unlimited` travels as.
const DEPTH_ALL = null;

/// The number the ∞ toggle comes back to.
///
/// Turning the limit off does not throw away which limit it was: a user who
/// wants a look at the whole tree wants their own depth back afterwards, and
/// landing them on the default instead would be a setting the control quietly
/// changed while they were looking at something else. Remembered across a
/// restart too, beside the depth itself, because the toggle survives one.
let lastBoundedDepth = DEPTH_DEFAULT;
{
  // Read once, here rather than in `restoreWorkspace`: this is not a setting a
  // workspace is handed, it is what the toggle beside it will offer next.
  const saved = prefs().depthLevels;
  if (typeof saved === 'number' && saved >= DEPTH_MIN && saved <= DEPTH_MAX) {
    lastBoundedDepth = saved;
  }
}

const depthLabel = (d) => (d === DEPTH_ALL ? 'all' : String(d));

/// Whether the depth stepper governs what is on screen.
///
/// It does on the call tree, where it is where the BFS stops — and on a map
/// scoped to one function, which groups that function's neighbourhood walked
/// to the same depth. It does not on the whole-program map: that one starts
/// nowhere in particular, so there is no walk for a number of levels to bound,
/// and the zoom control beside it is what moves there instead.
function depthApplies() {
  return currentView !== 'ClusterMap' || mapScope !== null;
}

function updateDepthControl() {
  // A live control that does nothing when pressed is worse than a quiet one,
  // so where the number governs nothing the whole pill goes inactive.
  const live = depthApplies();
  const unbounded = graphDepth === DEPTH_ALL;

  $('depth-value').textContent = depthLabel(graphDepth);
  // The stepper names a number, so while no number applies it is dimmed rather
  // than hidden: the range is still what the toggle will come back to, and a
  // control that vanishes makes the toolbar jump.
  $('depth-dec').disabled = !live || unbounded || graphDepth === DEPTH_MIN;
  $('depth-inc').disabled = !live || unbounded || graphDepth === DEPTH_MAX;

  const all = $('depth-all');
  all.disabled = !live;
  all.setAttribute('aria-pressed', String(unbounded));
  all.title = unbounded
    ? `Put the limit back at ${lastBoundedDepth}`
    : 'Show every level of calls, however deep they go';

  const control = $('depth-control');
  control.classList.toggle('inactive', !live);
  control.title = !live
    ? 'Depth applies to the call tree — double-click a cluster to get there'
    : currentView === 'ClusterMap'
      ? 'How many levels of calls this map groups'
      : 'How many levels of calls to show';
}

/// Ask for a depth: a number inside the range, or `DEPTH_ALL` for no bound.
function setDepth(next) {
  if (!depthApplies()) return;   // nothing on the whole-program map to deepen
  const want = next === DEPTH_ALL
    ? DEPTH_ALL
    : Math.min(Math.max(next, DEPTH_MIN), DEPTH_MAX);
  if (want === graphDepth) return;
  if (want !== DEPTH_ALL) lastBoundedDepth = want;
  // Reflect it immediately; the snapshot that follows confirms it.
  graphDepth = want;
  updateDepthControl();
  savePrefs({ depth: want, depthLevels: lastBoundedDepth });
  sendCommand({ SetGraphDepth: want });
}

/// One step along the range. Plain arithmetic, because the range is plain
/// numbers again: whether a number applies at all is the ∞ toggle's question,
/// not a ninth position that + walks onto.
function stepDepth(delta) {
  if (graphDepth === DEPTH_ALL) return;
  setDepth(graphDepth + delta);
}

/// Turn the limit off, or put back the one it was on.
function toggleDepthLimit() {
  setDepth(graphDepth === DEPTH_ALL ? lastBoundedDepth : DEPTH_ALL);
}

$('depth-dec').addEventListener('click', () => stepDepth(-1));
$('depth-inc').addEventListener('click', () => stepDepth(1));
$('depth-all').addEventListener('click', toggleDepthLimit);

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

/// Write the status line. `content` is a string, or a list of strings and
/// elements for the one writer — the load line — that needs a fixed-width field
/// in the middle of its text. `detail` is the tooltip, and is cleared by any
/// writer that has none: details of a load are wrong under "Refreshing".
function setStatus(content, state, detail) {
  if (Array.isArray(content)) statusEl.replaceChildren(...content);
  else statusEl.textContent = content;
  lspDotEl.className = state || '';
  statusEl.classList.toggle('poor', state === 'poor');
  statusEl.parentElement.title = detail || '';
  readyShown = false;
}

// ── The parse report ─────────────────────────────────────────────────────────
//
// A backend that parses the build (libclang) says how much of it it read, and
// "Ready" over half a build is a graph that looks complete and is not. So the
// ready line carries the count. It rides on the snapshot, not on `Completed`,
// because the snapshot is what a reconnecting client is sent; it lands just
// after `Completed` on the live path, which is why a ready line already on
// screen is redrawn when it does. Absent under clangd and the demo, where the
// line stays a plain "Ready".
//
// Loudness follows the backend's verdict (`quality`) rather than thresholds of
// the client's own: clean is calm, partial lights the dot, poor colours the
// line and says where to look.
const plural = (n, one, many) => `${n.toLocaleString()} ${n === 1 ? one : many}`;

function showReady() {
  const p = parseReport;
  if (!p) {
    setStatus('Ready', 'ready');
  } else {
    const parts = [`${plural(p.parsed, 'file', 'files')} parsed`];
    if (p.failed) parts.push(`${p.failed.toLocaleString()} failed`);
    if (p.with_errors) parts.push(`${p.with_errors.toLocaleString()} with errors`);
    const detail = [
      `${p.parsed.toLocaleString()} of ${(p.parsed + p.failed).toLocaleString()} translation units parsed`,
      p.cached ? `${p.cached.toLocaleString()} of them unchanged since a previous parse, and read from its cache` : null,
      p.failed ? `${p.failed.toLocaleString()} could not be parsed at all, and contributed nothing` : null,
      p.with_errors ? `${plural(p.with_errors, 'file', 'files')} parsed with errors — some function bodies may be incomplete` : null,
      `${plural(p.external_calls, 'call', 'calls')} to functions outside the project`,
      p.rejected_arguments?.length ? `Flags libclang ignored: ${p.rejected_arguments.join(' ')}` : null,
      p.quality === 'clean' ? null : 'Each file\'s diagnostics are in the log.',
    ].filter(Boolean).join('\n');
    if (p.quality === 'poor') {
      setStatus(`Incomplete · ${parts.join(' · ')} — check compile_commands.json and the log`, 'poor', detail);
    } else {
      setStatus(`Ready · ${parts.join(' · ')}`, p.quality === 'clean' ? 'ready' : 'warn', detail);
    }
  }
  readyShown = true;
}

// ── The hint line ────────────────────────────────────────────────────────────
//
// One line with many writers: hovering a node writes the symbol under the
// pointer, leaving it writes an empty string, and a regrouping writes that the
// map moved — which during the warmup is constant. That is right for a hint,
// where the next one is along in a moment and nothing is lost.
//
// It is wrong for a result. An export names the file it just wrote, and on a
// project the size of jemalloc that line was overwritten by the next
// `ClusteringUpdated` before it could be read — or by simply moving the mouse
// across the canvas. A message the user never sees is the same as no message,
// which is what makes a working export look like a key that does nothing.
//
// So a held message outranks the hints until it lapses. The same reason the
// `load` object exists for the status bar: one line, several writers, and only
// one of them saying something worth waiting for.
const INFO_HOLD_MS = 15000;
let infoHeldUntil = 0;

/// A passing hint. Yields to anything currently held.
function setInfo(text) {
  if (performance.now() < infoHeldUntil) return;
  infoEl.textContent = text;
}

/// A result worth reading. Holds the line against the hints for a while.
function holdInfo(text) {
  infoHeldUntil = performance.now() + INFO_HOLD_MS;
  infoEl.textContent = text;
}

// ── Load progress ────────────────────────────────────────────────────────────
//
// Loading is reported in the status bar and nowhere else, so this exists to
// keep that one line honest rather than to drive several surfaces. Without it
// the transient events fought over the line: a call-hierarchy reply would set
// it to "Ready" while the project was still halfway through indexing.
//
// It has two stages, because loading a project has two. Clangd hands over the
// symbols first (`LspProgress`), and only then are their call edges fetched
// (`ResolvingCalls`) — on a large project the second stage is the long one,
// and it runs entirely after the phase has gone to `Completed`.
const load = {
  label: '',        // what to call the current stage on screen
  done: 0,          // determinate progress…
  total: 0,         // …or 0 when this stage reports no total
  count: null,      // a bare running count, for stages that have no end in sight
  stage: null,      // which stage the numbers above describe
  symbols: false,   // stage 1: clangd still has symbols to hand over
  edges: false,     // stage 2: those symbols still need their calls
};

// Phases named for what the user is waiting on rather than for the protocol
// step. Absent from the map: NotStarted (idle, and demo mode's permanent
// phase), Completed and Failed, which are not waiting states.
const PHASE_LABEL = {
  SpawningServer:          'Starting clangd',
  Initializing:            'Starting clangd',
  Initialized:             'Starting clangd',
  DiscoveringFiles:        'Finding source files',
  PreloadingDocuments:     'Opening files',
  IndexingProject:         'Indexing project',
  LoadingWorkspaceSymbols: 'Loading symbols',
};

function loading() { return load.symbols || load.edges; }

/// `value`, right-aligned in a field held open to the width of `widest`.
///
/// Tabular figures make every digit the same width, which is not the same as
/// making every *number* the same width: 9 → 10 still grows the string and
/// shoves everything after it along the bar. Nor can the near side be padded
/// with spacing characters to the width of the far one — the far one is
/// `toLocaleString`d, so it carries thousands separators, and a comma is about
/// half a digit wide. Padding `999` to the *character* count of `2,916` gives
/// five digit-widths against four digits and a comma, and the line jumped 3px
/// narrower the moment the count reached `1,000` and grew a comma of its own.
///
/// So the width comes from the text itself: an invisible copy of the widest
/// value the field will hold, stacked under the live one in the same grid cell.
/// Exact in any font, with or without tabular figures, separators and all.
///
/// `widest` is optional. A field without one is free to grow — which is the
/// running count, where nothing has said how wide it will end up — and is still
/// a field, because the gap to the stage's name comes with it.
function countField(value, widest) {
  const field = document.createElement('span');
  field.className = 'count';

  if (widest != null) {
    const ghost = document.createElement('span');
    ghost.className = 'count-ghost';
    ghost.textContent = widest;
    field.append(ghost);
  }

  const shown = document.createElement('span');
  shown.textContent = value;

  field.append(shown);
  return field;
}

/// Push `load` to the status bar.
///
/// A stage with a total reads as a fraction; one without reads as a running
/// count, because clangd streams workspace symbols without ever saying how
/// many are coming. Only the fraction is held to a width — the bare count has
/// no total to be held to, and is the end of the line anyway.
function renderLoad() {
  // The tallies come and go with the load, so this rides along with the one
  // function both stages already call on every change.
  statusBarEl.classList.toggle('loading', loading());
  if (!loading()) return;
  // The numbers belong to whichever stage last set them, and a finished stage's
  // numbers are a lie: painting them is what leaves "Resolving calls 15 / 19" on
  // screen after every call has in fact been resolved.
  if (load.stage === 'edges' && !load.edges) return;
  if (load.stage === 'symbols' && !load.symbols) return;

  if (load.total > 0) {
    const total = load.total.toLocaleString();
    setStatus([
      load.label,
      countField(load.done.toLocaleString(), total),
      ` / ${total}`,
    ], 'busy');
  } else if (load.count != null) {
    setStatus([load.label, countField(load.count.toLocaleString())], 'busy');
  } else {
    setStatus(load.label, 'busy');
  }
}

function updateStatusFromEvent(key, payload) {
  if (key === 'LspProgress') {
    const phase = typeof payload === 'string' ? payload : Object.keys(payload)[0];
    const body = typeof payload === 'string' ? null : payload[phase];

    if ((phase === 'PreloadingDocuments' || phase === 'IndexingProject') && body) {
      Object.assign(load, {
        symbols: true, stage: 'symbols', label: PHASE_LABEL[phase],
        done: body.done, total: body.total, count: null,
      });
    } else if (phase === 'LoadingWorkspaceSymbols' && body) {
      // No total: clangd streams symbols without saying how many are coming,
      // so this stage shows a running count and no bar.
      Object.assign(load, {
        symbols: true, stage: 'symbols', label: PHASE_LABEL[phase],
        done: 0, total: 0, count: body.loaded,
      });
    } else if (PHASE_LABEL[phase]) {
      Object.assign(load, {
        symbols: true, stage: 'symbols', label: PHASE_LABEL[phase],
        done: 0, total: 0, count: null,
      });
    } else {
      // Completed, NotStarted or Failed — stage 1 is over either way. Whether
      // anything is still loading is now stage 2's to say.
      load.symbols = false;
      if (!load.edges) {
        if (phase === 'Completed') showReady();
        else if (phase === 'NotStarted') setStatus('Idle');
        else if (phase === 'Failed') setStatus(typeof body === 'string' ? body : 'LSP failed', 'error');
      }
    }
    renderLoad();
  } else if (key === 'ResolvingCalls') {
    const { done, total } = payload || {};
    load.edges = total > 0 && done < total;
    if (load.edges) {
      Object.assign(load, { stage: 'edges', label: 'Resolving calls', done, total, count: null });
    }
    renderLoad();
    if (!loading()) showReady();
  } else if (key === 'LoadingCalls') {
    // Drowned out by the warmup, which fires one of these per symbol. While it
    // runs, the overlay is already saying the same thing with a number on it.
    if (!loading()) setStatus(`Loading ${(payload?.direction || '').toLowerCase()} calls`, 'busy');
  } else if (key === 'FunctionHasNoCallees') {
    setInfo('No callees in this direction');
  } else if (key === 'FunctionHasNoCommunities') {
    // The view deliberately did not change, so without this the key looks dead.
    setInfo('Nothing to group here — this function reaches too little');
  } else if (key === 'OutgoingCallsLoaded' || key === 'IncomingCallsLoaded') {
    // Not "Ready" mid-load: the warmup answers hundreds of these while the
    // graph is still filling in, and each one used to declare the wait over.
    if (!loading()) showReady();
  } else if (key === 'RefreshingWorkspaceSymbols') {
    setStatus('Refreshing', 'busy');
  } else if (key === 'AlreadyLoading') {
    setInfo('Already loading — the project is being read as it stands');
  } else if (key === 'ReferencesFound' || key === 'NoReferencesFound') {
    // The count is the answer: the locations stay on the backend's node, and
    // nothing here draws them yet. Held, because it answers a command.
    const name = allFunctions[payload?.symbol_id]?.name || 'this function';
    holdInfo(`${plural(payload?.count ?? 0, 'reference', 'references')} to ${name}`);
  } else if (key === 'CannotCloseLastWorkspace') {
    setInfo('Cannot close the last graph');
  } else if (key === 'LspError') {
    setStatus(typeof payload === 'string' ? payload : 'LSP error', 'error');
  } else if (key === 'ClusteringUpdated') {
    // The map is about to redraw with different clusters on it; say so,
    // because a map shifting under the user unannounced reads as a bug.
    const n = payload?.clusters;
    setInfo(n ? `Regrouped into ${n} clusters` : 'Clusters regrouped');
  } else if (key === 'GraphExported') {
    // Where it goes is the transport's business: a browser download, or a real
    // file on the desktop. Either way say where, because a save the user
    // cannot find did not happen as far as they are concerned.
    CoonHost.saveFile(payload.filename, payload.contents)
      .then((where) => {
        // `null` is the desktop save dialog being dismissed. Nothing went
        // wrong and nothing was written, so the line says so plainly and does
        // not hold — there is no path to go and find.
        if (where) holdInfo(`Exported ${where}`);
        else setInfo('Export cancelled');
      })
      .catch((err) => {
        console.error('export failed', err);
        holdInfo(`Could not save ${payload.filename}`);
      });
  } else if (key === 'NothingToExport') {
    holdInfo('Nothing on screen to export');
  } else if (key === 'WorkspaceCreated' || key === 'WorkspaceSwitched') {
    setInfo(payload?.name ? `Graph: ${payload.name}` : '');
  }
}

// ── Sidebar ───────────────────────────────────────────────────────────────────

/// Where a symbol is defined, as `file.c:12`.
///
/// The basename alone is not enough: C++ overloads share a file, and a name
/// that repeats within one file is exactly the case the caller is trying to
/// tell apart.
function symbolWhere(fn) {
  const loc = fn && fn.definition_location;
  if (!loc || !loc.file_path) return '';
  const base = loc.file_path.split('/').pop();
  return loc.line ? `${base}:${loc.line}` : base;
}

/// Everything the sidebar and the symbol finder read off the symbol table,
/// worked out once: which names are ambiguous, where each symbol lives, what
/// order the rows go in, and the text the filter matches against.
///
/// Both callers run far more often than the table changes — the filter on
/// every keystroke, `handleSnapshot` on every snapshot, and during the edge
/// warmup the snapshots keep coming over symbols that stopped arriving long
/// ago. So a keystroke gets out on the table's identity, and a snapshot, which
/// always brings a new one, gets out on a fingerprint of what the rows are
/// drawn from instead: one that only grew edges hands back the same index, and
/// `renderSidebar` below then leaves the DOM alone as well.
///
/// `ambiguous` holds the names more than one symbol answers to. Nothing is
/// duplicated when it is non-empty — C gives every `static` function
/// file-local linkage, so jemalloc's test suite really does define nine
/// different `thd_start`s, and a C++ overload set is several functions with
/// one name. They are distinct symbols with distinct call graphs, so the list
/// must keep all of them; what it must not do is draw them identically.
let symbolIndexCache = null;

function symbolIndex() {
  if (symbolIndexCache && symbolIndexCache.fns === allFunctions) return symbolIndexCache;

  let fingerprint = '';
  for (const id in allFunctions) {
    const fn = allFunctions[id];
    const loc = fn.definition_location;
    fingerprint += `${id}\u0000${fn.name}\u0000${fn.qualified_name || ''}` +
      `\u0000${loc?.file_path || ''}\u0000${loc?.line ?? ''}\u0001`;
  }
  if (symbolIndexCache && symbolIndexCache.fingerprint === fingerprint) {
    // The same symbols in a new envelope. The index stands — keeping its
    // identity, which is what lets the DOM below stand too — and adopts the
    // table it now answers for.
    symbolIndexCache.fns = allFunctions;
    return symbolIndexCache;
  }

  const seen = new Set();
  const ambiguous = new Set();
  const rows = [];
  for (const id in allFunctions) {
    const fn = allFunctions[id];
    if (seen.has(fn.name)) ambiguous.add(fn.name);
    else seen.add(fn.name);
    rows.push({
      id,
      fn,
      name: fn.name,
      where: symbolWhere(fn),
      // What the filter matches. Lowercased once here rather than twice per
      // symbol per keystroke. The NUL is the join: no query anyone can type
      // spans it, so a match is still a match within one field or the other,
      // as it was when these were two `includes` calls.
      haystack: `${fn.name}\u0000${fn.qualified_name || ''}`.toLowerCase(),
    });
  }

  // Sorted once, and `where` is precomputed because it splits a path and
  // allocates — inside the comparator that ran n log n times.
  const sorted = rows.slice().sort((a, b) => {
    // Same name: keep the group in a stable, readable order rather than
    // whatever the id hash happened to give.
    return a.name.localeCompare(b.name) || a.where.localeCompare(b.where);
  });

  symbolIndexCache = { fns: allFunctions, fingerprint, rows, sorted, ambiguous };
  return symbolIndexCache;
}

// What the list currently shows. The rows are a function of these three and
// nothing else, so matching them means the DOM below is already right.
let sidebarShowing = null;

function renderSidebar(filter) {
  const q = (filter || '').toLowerCase();
  const index = symbolIndex();
  if (sidebarShowing
      && sidebarShowing.index === index
      && sidebarShowing.q === q
      && sidebarShowing.root === rootNodeId) return;
  sidebarShowing = { index, q, root: rootNodeId };

  const { ambiguous } = index;
  const shown = q ? index.sorted.filter((row) => row.haystack.includes(q)) : index.sorted;

  fnListEl.innerHTML = '';

  if (shown.length === 0) {
    const li = document.createElement('li');
    li.className = 'empty';
    li.textContent = index.sorted.length === 0 ? 'No symbols loaded yet' : 'No matches';
    fnListEl.appendChild(li);
    return;
  }

  for (const { id, fn: node, where } of shown) {
    const li = document.createElement('li');

    const nameEl = document.createElement('span');
    nameEl.className = 'fn-name';
    nameEl.textContent = node.name;
    li.appendChild(nameEl);

    // Only the rows that need telling apart carry their file. Stamping every
    // row with one would cost the 2,500 unique names half the sidebar's width
    // to say nothing.
    if (where && ambiguous.has(node.name)) {
      const whereEl = document.createElement('span');
      whereEl.className = 'fn-where';
      whereEl.textContent = where;
      li.appendChild(whereEl);
    }

    li.title = [node.qualified_name || node.name, where].filter(Boolean).join('  ');
    if (id === rootNodeId) li.classList.add('selected');
    li.addEventListener('click', () => selectRoot(id));
    // Middle-click opens the symbol in its own graph tab.
    li.addEventListener('mousedown', (e) => {
      if (e.button === 1) { e.preventDefault(); openInNewGraph(id); }
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
  setInfo(`Exploring ${allFunctions[id]?.name ?? id}`);
  // Naming a symbol is asking for its call tree, and `core` switches the
  // workspace to the rooted view on its own — no need to pair the commands here.
  sendCommand({ SelectFunction: id });
}

/// Point at a node: light its border and let the breadcrumb follow. Nothing is
/// fetched and nothing is laid out again, and the canvas is deliberately not
/// rebuilt — a click is how the user says "this one", and a redraw at that
/// moment loses their place. Loading a node's calls is the `+` button's job.
function focusNode(id) {
  if (selectedNodeId === id) return;
  selectedNodeId = id;
  // Exactly one node wears the selection, so the old one gives it up first.
  cy.nodes().removeClass('focused');
  cy.$id(id).addClass('focused');
  markPathToSelection();
  renderBreadcrumb();
  // `core` keeps the selection so the breadcrumb and the cluster scope agree
  // with the canvas the next time something else sends a snapshot.
  sendCommand({ FocusFunction: id });
}

// Expand a node in place: load its children without changing the root.
function expandNode(id) {
  setInfo(`Expanding ${allFunctions[id]?.name ?? id}`);
  sendCommand({ ExpandFunction: id });
}

/// Open a symbol in a tab of its own. A new workspace opens on the cluster map
/// like any other, but this one was asked for by name and already has a root,
/// so `core` starts it on the tree rather than making the user find it again.
function openInNewGraph(id) {
  sendCommand({ StartGraphFromFunction: id });
}

cy.on('tap', 'node', (e) => {
  if (currentView === 'ClusterMap') { clusterTap(e.target); return; }
  focusNode(e.target.id());
});
cy.on('dbltap', 'node', (e) => {
  e.preventDefault?.();
  if (currentView === 'ClusterMap') { clusterDrill(e.target); return; }
  toggleNode(e.target.id());
});

// Hover: highlight the node's own edges and dim the rest, so a dense graph
// still reads at a glance.
cy.on('mouseover', 'node', (e) => {
  const node = e.target;
  node.addClass('hovered');
  node.connectedEdges().addClass('incident');

  // Fade everything that isn't adjacent. At 50+ nodes this is the difference
  // between reading a node's calls and squinting at a hairball.
  cy.elements().difference(node.closedNeighborhood()).addClass('faded');

  // The canvas has no tooltips, and the label may be truncated, so the status
  // bar carries the full qualified name.
  setInfo(node.data('full'));
  // The overlay loads or hides a function's callees; a cluster has neither.
  if (currentView !== 'ClusterMap') showExpandBtn(node);
});
cy.on('mouseout', 'node', (e) => {
  e.target.removeClass('hovered');
  cy.edges().removeClass('incident');
  cy.elements().removeClass('faded');
  setInfo('');
  hideExpandBtn();
});

// ── Expand button overlay ─────────────────────────────────────────────────────

const expandBtn = $('expand-btn');
let expandTargetId = null;
let hideTimer = null;

function showExpandBtn(node) {
  clearTimeout(hideTimer);
  const id = node.id();
  expandTargetId = id;

  // One affordance, three cases: collapse what's shown, re-expand what the
  // user collapsed, or load children that were never fetched.
  const hasVisibleChildren = node.outgoers('node').length > 0;
  expandBtn.textContent = hasVisibleChildren ? '−' : '+';
  expandBtn.title = hasVisibleChildren
    ? 'Collapse children'
    : (collapsed.has(id) ? 'Expand children' : 'Load calls for this function');

  const bb = node.renderedBoundingBox();
  const container = $('cy').getBoundingClientRect();
  // Positioned inside the node's top-right corner: no gap for the cursor to
  // cross, so mouseenter fires before the hide timer expires.
  expandBtn.style.left = `${container.left + bb.x2 - 21}px`;
  expandBtn.style.top  = `${container.top + bb.y1 + 5}px`;
  expandBtn.style.display = 'flex';
}

/// Collapse, re-expand, or load — whichever the node's current state calls for.
function toggleNode(id) {
  const node = cy.$id(id);
  if (node.length && node.outgoers('node').length > 0) {
    sendCommand({ ToggleCollapsed: id });          // hide what's shown
  } else if (collapsed.has(id)) {
    sendCommand({ ToggleCollapsed: id });          // show it again
  } else {
    expandNode(id);                                 // fetch from clangd
  }
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
  if (expandTargetId) toggleNode(expandTargetId);
  expandBtn.style.display = 'none';
});

// ── Toolbar ───────────────────────────────────────────────────────────────────

function updateDirToggle() {
  dirToggleEl.textContent = currentDirection === 'Outgoing' ? '→ Outgoing' : '← Incoming';
  dirToggleEl.title = currentDirection === 'Outgoing'
    ? 'Showing calls made by each function — click for callers'
    : 'Showing callers of each function — click for callees';
}

function updateOrientToggle() {
  const leftRight = currentOrientation === 'LeftRight';
  orientLrEl.setAttribute('aria-pressed', String(leftRight));
  orientTdEl.setAttribute('aria-pressed', String(!leftRight));
}

/// Pick an orientation. Absolute, not a flip: the segment names the layout it
/// produces, so pressing the one already lit is a no-op rather than a surprise.
function setOrientation(next) {
  if (next === currentOrientation) return;
  // Reflect it immediately; the snapshot that follows confirms it.
  currentOrientation = next;
  updateOrientToggle();
  savePrefs({ orientation: next });
  sendCommand({ SetGraphOrientation: next });
}

/// Read the graph as callees or as callers.
///
/// The one funnel — button, palette and Ctrl+D all come through here — and it
/// names the direction rather than asking for the other one, so what is
/// remembered is a direction and not a number of flips. The button on the
/// toolbar is still a flip; it works out which way that lands and says so.
function setDirection(next) {
  if (next === currentDirection) return;
  savePrefs({ direction: next });
  sendCommand({ SetCallDirection: next });
}

function flipDirection() {
  setDirection(currentDirection === 'Outgoing' ? 'Incoming' : 'Outgoing');
}

dirToggleEl.addEventListener('click', flipDirection);
orientLrEl.addEventListener('click', () => setOrientation('LeftRight'));
orientTdEl.addEventListener('click', () => setOrientation('TopDown'));
$('refresh-btn').addEventListener('click', () => sendCommand('Refresh'));

// ── Sidebar dock: collapse and drag-resize ────────────────────────────────────

const sidebarEl = $('sidebar');

$('sidebar-toggle').addEventListener('click', toggleSidebar);
function toggleSidebar() {
  const collapsed = document.body.classList.toggle('sidebar-collapsed');
  savePrefs({ sidebar: { ...(prefs().sidebar || {}), collapsed } });
  requestAnimationFrame(() => cy.resize());
}

// The dock is chrome rather than a view setting, so it is restored here and
// now rather than handed to a workspace: there is no workspace in it, and
// waiting for a snapshot to size the sidebar would show it at one width and
// then another.
(function restoreDock() {
  const saved = prefs().sidebar || {};
  if (saved.collapsed) document.body.classList.add('sidebar-collapsed');
  if (typeof saved.width === 'number') sidebarEl.style.width = `${saved.width}px`;
})();

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
    // On release, not on every pixel of the drag: this is a write to disk in
    // the desktop app, and the width the user let go of is the only one they
    // asked for.
    savePrefs({ sidebar: { ...(prefs().sidebar || {}), width: sidebarEl.getBoundingClientRect().width } });
    cy.resize();
  };
  document.addEventListener('mousemove', onMove);
  document.addEventListener('mouseup', onUp);
});

// ── Window controls (desktop only) ────────────────────────────────────────────

$('wc-min').addEventListener('click', () => CoonHost.minimize());
$('wc-max').addEventListener('click', () => CoonHost.toggleMaximize());
$('wc-close').addEventListener('click', () => CoonHost.close());


// ── Theme ─────────────────────────────────────────────────────────────────────

function currentTheme() {
  return document.documentElement.dataset.theme
    || (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark');
}

function toggleTheme() {
  const next = currentTheme() === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  savePrefs({ theme: next });
  // The graph reads its colours from the same tokens, so re-apply them.
  cy.style(graphStyle());
}

// ── Command palette ───────────────────────────────────────────────────────────
// Rebuilt on each open so enabled states and the workspace list are current.
// This is where the commands core has always exposed become reachable, rather
// than growing a button per command.

function buildCommands() {
  // Half these commands drive the rooted view's BFS and mean nothing on a map
  // bounded by which boxes are open, so they go grey there rather than
  // silently changing a view the user is not looking at.
  const onMap = currentView === 'ClusterMap';
  const canZoom = onMap && mapLevels > 0;
  // The depth is the one of these the map does not simply ignore: a map scoped
  // to a function groups what that function reaches within it.
  const depth = depthApplies();

  const cmds = [
    { id: 'goto', label: 'Go to Symbol\u2026', hint: 'Ctrl+P', run: openSymbolFinder },

    { id: 'graph.new', label: 'Graph: New', hint: 'Ctrl+T', run: () => sendCommand('NewWorkspace') },
    {
      id: 'graph.close',
      label: 'Graph: Close',
      hint: 'Ctrl+W',
      disabled: workspaces.length <= 1,
      run: () => sendCommand({ CloseWorkspace: activeWorkspace }),
    },
    { id: 'graph.rename', label: 'Graph: Rename\u2026', run: renameActiveTab },
    { id: 'graph.next', label: 'Graph: Next', hint: 'Ctrl+Tab', disabled: workspaces.length <= 1, run: () => sendCommand('NextWorkspace') },
    { id: 'graph.prev', label: 'Graph: Previous', hint: 'Ctrl+Shift+Tab', disabled: workspaces.length <= 1, run: () => sendCommand('PreviousWorkspace') },
    { id: 'graph.expand', label: 'Graph: Expand or Collapse Selected', disabled: onMap || !selectedNodeId, run: () => sendCommand('ExpandOrCollapse') },

    // The two views, named rather than toggled — the same reason the
    // orientation control shows both segments instead of one that flips.
    { id: 'view.clusters', label: 'View: Cluster Map', hint: 'Ctrl+M', disabled: onMap, run: showClusterMap },
    {
      id: 'view.clusterfn',
      label: 'View: Cluster Map of This Function',
      hint: 'Ctrl+Shift+M',
      disabled: onMap || !scopeTarget(),
      run: clusterThisFunction,
    },
    {
      id: 'view.clusterall',
      label: 'View: Cluster Map of the Whole Project',
      disabled: !mapScope,
      run: () => sendCommand({ SetMapScope: null }),
    },
    { id: 'view.rooted', label: 'View: Rooted Graph', hint: 'Ctrl+M', disabled: !onMap || !rootNodeId, run: () => sendCommand({ SetViewMode: 'Rooted' }) },
    // The zoom, named at both ends as well as stepped: "as few boxes as there
    // are" is a place on the ladder a user asks for directly, and stepping to
    // it from the other end takes eleven presses on a real project.
    {
      id: 'view.settings',
      label: 'Map: Cluster Settings\u2026',
      hint: 'method, grain, grouping',
      run: openClusterPanel,
    },
    {
      id: 'view.finer',
      label: 'Map: Smaller Clusters',
      hint: 'Ctrl+]',
      disabled: !canZoom || mapLevel <= 0,
      run: () => setMapLevel(mapLevel - 1),
    },
    {
      id: 'view.coarser',
      label: 'Map: Larger Clusters',
      hint: 'Ctrl+[',
      disabled: !canZoom || mapLevel >= mapLevels - 1,
      run: () => setMapLevel(mapLevel + 1),
    },
    {
      id: 'view.finest',
      label: 'Map: Finest Grouping',
      disabled: !canZoom || mapLevel <= 0,
      run: () => setMapLevel(0),
    },
    {
      id: 'view.coarsest',
      label: 'Map: Coarsest Grouping',
      disabled: !canZoom || mapLevel >= mapLevels - 1,
      run: () => setMapLevel(mapLevels - 1),
    },

    { id: 'view.sidebar', label: 'View: Toggle Sidebar', hint: 'Ctrl+B', run: toggleSidebar },
    { id: 'view.direction', label: 'View: Toggle Call Direction', hint: 'Ctrl+D', run: flipDirection },
    { id: 'view.orientation', label: 'View: Toggle Graph Orientation', hint: 'Ctrl+Shift+D', run: () => setOrientation(currentOrientation === 'LeftRight' ? 'TopDown' : 'LeftRight') },
    { id: 'view.fit', label: 'View: Fit Graph to Screen', disabled: cy.nodes().length === 0, run: fitGraph },
    // Live on a scoped map too, where the depth is how far the neighbourhood
    // it groups was walked — but without the bracket hints there, because on
    // any map those keys are the zoom, and a hint that names a key doing
    // something else is worse than none.
    { id: 'view.deeper', label: 'View: Show More Levels', hint: onMap ? '' : 'Ctrl+]', disabled: !depth || graphDepth === DEPTH_ALL || graphDepth === DEPTH_MAX, run: () => stepDepth(1) },
    { id: 'view.shallower', label: 'View: Show Fewer Levels', hint: onMap ? '' : 'Ctrl+[', disabled: !depth || graphDepth === DEPTH_ALL || graphDepth === DEPTH_MIN, run: () => stepDepth(-1) },
    // Both directions of the ∞ toggle, named absolutely rather than as one
    // entry that does the opposite of whatever is there: a palette is searched
    // for what you want, not read for what you have.
    { id: 'view.alldepth', label: 'View: Show All Levels', disabled: !depth || graphDepth === DEPTH_ALL, run: () => setDepth(DEPTH_ALL) },
    { id: 'view.limitdepth', label: 'View: Limit Depth Again', disabled: !depth || graphDepth !== DEPTH_ALL, run: () => setDepth(lastBoundedDepth) },
    {
      id: 'view.collapse',
      label: 'View: Collapse or Expand Selected Node',
      disabled: onMap || !selectedNodeId,
      run: () => selectedNodeId && toggleNode(selectedNodeId),
    },
    {
      id: 'view.expandall',
      label: 'View: Expand All Collapsed Nodes',
      disabled: onMap || collapsed.size === 0,
      run: () => collapsed.forEach((id) => sendCommand({ ToggleCollapsed: id })),
    },
    { id: 'view.theme', label: 'View: Toggle Theme', run: toggleTheme },

    // Both views export — the rooted tree as a DAG, the map as one DOT
    // `subgraph cluster_` box per cluster, opened to show the functions inside
    // — so this is the one graph command that is never greyed out on the map.
    { id: 'graph.export', label: 'Graph: Export as Graphviz (DOT)', hint: 'Ctrl+E', run: exportGraph },

    // Offered only where the backend can answer: a command that is sent and
    // silently declined looks like a broken key. Under libclang a refresh is
    // a whole re-parse, so it says that rather than "symbols".
    {
      id: 'lsp.refresh',
      label: capabilities?.call_hierarchy === false ? 'Project: Re-index' : 'LSP: Refresh Symbols',
      hint: 'Ctrl+R',
      disabled: canRefresh() === false,
      run: () => sendCommand('Refresh'),
    },
    { id: 'lsp.refs', label: 'LSP: Find References', disabled: !selectedNodeId || capabilities?.references === false, run: () => sendCommand('FindReferences') },
  ];

  // Absent rather than greyed under clangd, which cannot choose: its replies
  // are held to the project root and say nothing about bodies.
  if (capabilities?.bodiless_callees) {
    BODILESS_CHOICES.forEach((choice) => {
      cmds.push({
        id: `project.bodiless.${choice.id}`,
        label: `Project: Calls Without a Body \u2014 ${choice.label}`,
        hint: choice.hint,
        disabled: choice.id === bodilessCallees,
        run: () => setBodilessCallees(choice.id),
      });
    });
  }

  // One entry per clustering method, so a method added in `logic` is reachable
  // without the client knowing anything about it but its name.
  clusterMethods.forEach((method) => {
    cmds.push({
      id: `view.method.${method.id}`,
      label: `Map: Group by ${method.label}`,
      hint: method.description,
      disabled: !onMap || method.id === clusterMethod,
      run: () => setClusterMethod(method.id),
    });
  });

  workspaces.forEach((ws, index) => {
    if (index === activeWorkspace) return;
    cmds.push({
      id: `graph.switch.${index}`,
      label: `Graph: Switch to ${ws.name}`,
      hint: index < 9 ? `Ctrl+${index + 1}` : '',
      run: () => sendCommand({ SwitchWorkspace: index }),
    });
  });

  return cmds;
}

/// Ask for the current view as a file.
///
/// The backend does the rendering and hands back `{ filename, contents }`;
/// `GraphExported` below is where it lands. Nothing is drawn or re-fetched, so
/// this costs no snapshot.
// Which calls to a function with no body in the build become edges. Project-
// wide, not per workspace — it decides what the graph is — so it is restored
// once, on the first snapshot that says the backend can choose, rather than
// handed to each workspace. That snapshot usually arrives mid-load; core
// applies a choice made then when the load ends.
const BODILESS_CHOICES = [
  { id: 'Drop', short: 'hide', label: 'Leave Out', hint: 'only functions the build defines' },
  { id: 'Project', short: 'project', label: 'Keep Project Declarations', hint: 'a leaf at the project header that declares it' },
  { id: 'All', short: 'all', label: 'Keep All', hint: 'libc and system headers too' },
];

function restoreBodilessCallees() {
  if (bodilessRestored || !capabilities?.bodiless_callees) return;
  bodilessRestored = true;
  const wanted = prefs().bodilessCallees;
  if (wanted && wanted !== bodilessCallees && BODILESS_CHOICES.some((c) => c.id === wanted)) {
    sendCommand({ SetBodilessCallees: wanted });
  }
}

function setBodilessCallees(id) {
  savePrefs({ bodilessCallees: id });
  sendCommand({ SetBodilessCallees: id });
}

// The toolbar's drop-down: the pill reads out the live choice, the list behind
// it says what each one means. Shown only where the backend can choose.
const bodilessMenuEl   = $('bodiless-menu');
const bodilessToggleEl = $('bodiless-control');
const bodilessPanelEl  = $('bodiless-panel');

function updateBodilessControl() {
  bodilessMenuEl.hidden = !capabilities?.bodiless_callees;
  if (bodilessMenuEl.hidden) closeBodilessPanel();
  const live = BODILESS_CHOICES.find((c) => c.id === bodilessCallees);
  $('bodiless-value').textContent = live?.short ?? '\u2014';
  bodilessToggleEl.title = live
    ? `Calls to functions with no body in the build: ${live.hint}`
    : 'Calls to functions with no body in the build';
  renderBodilessPanel();
}

function renderBodilessPanel() {
  if (bodilessPanelEl.hidden) return;
  bodilessPanelEl.innerHTML = '';
  for (const choice of BODILESS_CHOICES) {
    const row = document.createElement('button');
    row.className = 'method-row';
    row.type = 'button';
    row.setAttribute('role', 'radio');
    row.setAttribute('aria-checked', String(choice.id === bodilessCallees));
    const name = document.createElement('span');
    name.className = 'method-name';
    name.textContent = choice.label;
    const why = document.createElement('span');
    why.className = 'method-why';
    why.textContent = choice.hint;
    row.append(name, why);
    row.addEventListener('click', () => {
      if (choice.id !== bodilessCallees) setBodilessCallees(choice.id);
      closeBodilessPanel();
      bodilessToggleEl.focus();
    });
    bodilessPanelEl.appendChild(row);
  }
}

function openBodilessPanel() {
  bodilessPanelEl.hidden = false;
  bodilessToggleEl.setAttribute('aria-expanded', 'true');
  renderBodilessPanel();
}

function closeBodilessPanel() {
  bodilessPanelEl.hidden = true;
  bodilessToggleEl.setAttribute('aria-expanded', 'false');
}

bodilessToggleEl.addEventListener('click', () => {
  if (bodilessPanelEl.hidden) openBodilessPanel(); else closeBodilessPanel();
});
// Closed by clicking away or by Escape, as the cluster panel is.
document.addEventListener('pointerdown', (e) => {
  if (!bodilessPanelEl.hidden && !bodilessMenuEl.contains(e.target)) closeBodilessPanel();
});
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || bodilessPanelEl.hidden || CoonPalette.isOpen()) return;
  closeBodilessPanel();
  bodilessToggleEl.focus();
});

/// Whether the backend can refresh: true, false, or undefined before the first
/// snapshot has said — which is treated as yes, the way it always was.
function canRefresh() {
  return capabilities?.refresh;
}

function exportGraph() {
  sendCommand({ ExportGraph: 'Dot' });
}

function openCommandPalette() {
  CoonPalette.show({
    placeholder: 'Run a command\u2026',
    items: buildCommands(),
    onPick: (item) => item.run(),
  });
}

function openSymbolFinder() {
  // `rows`, not `sorted`: the picker scores what it is given and ties fall back
  // on the order they arrived in, so this stays the table's own order.
  const { rows, ambiguous } = symbolIndex();
  const items = rows.map(({ id, fn, where }) => ({
    id,
    label: fn.name,
    // Only worth showing when it adds something beyond the bare name — and
    // where the name is not the symbol's own, the location is what picks one
    // `thd_start` out of nine.
    detail: fn.qualified_name && fn.qualified_name !== fn.name
      ? fn.qualified_name
      : (ambiguous.has(fn.name) ? where : ''),
    mono: true,
  }));

  CoonPalette.show({
    placeholder: 'Go to symbol\u2026',
    items,
    altHint: 'Enter to focus \u00b7 Alt+Enter to open in a new graph',
    onPick: (item) => selectRoot(item.id),
    onAltPick: (item) => openInNewGraph(item.id),
  });
}

function renameActiveTab() {
  const nameEl = tabsEl.children[activeWorkspace]?.querySelector('.tab-name');
  if (nameEl) beginRename(nameEl, activeWorkspace);
}

// ── Keybindings ───────────────────────────────────────────────────────────────

document.addEventListener('keydown', (e) => {
  if (CoonPalette.isOpen()) return;   // the palette handles its own keys

  const mod = e.ctrlKey || e.metaKey;
  if (!mod) return;

  // Shift reports letter keys uppercase; normalise so combos match either way.
  const key = e.key.length === 1 ? e.key.toLowerCase() : e.key;

  if (key === 'p' && e.shiftKey) { e.preventDefault(); openCommandPalette(); }
  else if (key === 'p') { e.preventDefault(); openSymbolFinder(); }
  else if (key === 'b') { e.preventDefault(); toggleSidebar(); }
  else if (key === 't') { e.preventDefault(); sendCommand('NewWorkspace'); }
  else if (key === 'w') {
    e.preventDefault();
    if (workspaces.length > 1) sendCommand({ CloseWorkspace: activeWorkspace });
  }
  else if (key === 'f') { e.preventDefault(); searchEl.focus(); searchEl.select(); }
  else if (key === 'r') { e.preventDefault(); if (canRefresh() !== false) sendCommand('Refresh'); }
  else if (key === 'e') { e.preventDefault(); exportGraph(); }
  else if (key === 'm' && e.shiftKey) { e.preventDefault(); clusterThisFunction(); }
  else if (key === 'm') { e.preventDefault(); toggleView(); }
  else if (key === 'd' && e.shiftKey) { e.preventDefault(); setOrientation(currentOrientation === 'LeftRight' ? 'TopDown' : 'LeftRight'); }
  else if (key === 'd') { e.preventDefault(); flipDirection(); }
  // One pair of keys, one meaning — more or less detail — pointed at whichever
  // view is showing: the tree's depth limit, or the map's grouping.
  else if (key === ']') {
    e.preventDefault();
    if (currentView === 'ClusterMap') setMapLevel(mapLevel - 1); else stepDepth(1);
  }
  else if (key === '[') {
    e.preventDefault();
    if (currentView === 'ClusterMap') setMapLevel(mapLevel + 1); else stepDepth(-1);
  }
  else if (key === 'Tab') {
    e.preventDefault();
    sendCommand(e.shiftKey ? 'PreviousWorkspace' : 'NextWorkspace');
  }
  else if (key >= '1' && key <= '9') {
    const index = Number(key) - 1;
    if (index < workspaces.length) { e.preventDefault(); sendCommand({ SwitchWorkspace: index }); }
  }
});
