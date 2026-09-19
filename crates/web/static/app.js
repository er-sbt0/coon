// ── Cytoscape setup ──────────────────────────────────────────────────────────
// Styles read from the CSS custom properties in theme.css so the graph and the
// chrome stay on one palette, and a theme switch moves both together.

// Restore a pinned theme before anything reads the palette. Without a pin we
// leave the attribute off entirely so the OS preference wins.
try {
  const saved = localStorage.getItem('coon.theme');
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
let graphDepth       = 3;
let collapsed        = new Set();
let currentView      = 'ClusterMap';  // or 'Rooted'; a workspace opens on the map
let clusterMap       = null;          // the laid-out map, while one is showing
let mapScope         = null;          // the function the map is drawn from, or null for all of it
let mapLevel         = 0;             // which rung of the coarsening ladder is showing
let mapLevels        = 0;             // how many rungs there are; 0 while nothing is clustered
let mapClusters      = 0;             // how many boxes this rung holds — what the zoom control reads out
let clusterMethod    = null;          // which algorithm built the map
let clusterMethods   = [];            // every algorithm on offer, for the picker
let selectedClusterId = null;         // the box pointed at on the map, if it is still on this rung
let lastFitKey       = null;                      // refit only on root/direction change

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
  clusterMethod = active?.method ?? null;
  if (Array.isArray(msg.cluster_methods)) clusterMethods = msg.cluster_methods;
  if (typeof active?.depth === 'number') graphDepth = active.depth;
  updateDepthControl();
  updateZoomControl();
  updateMethodControl();
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
    rebuildGraph(currentGraph, msg.layout);
  }
  renderSidebar(searchEl.value);
  renderBreadcrumb();

  const n = Object.keys(allFunctions).length;
  fnCountEl.textContent = n ? `(${n})` : '';
  statSymbolsEl.textContent = n.toLocaleString();
  // What is actually on the canvas. The rooted layout ships on every snapshot
  // whether or not it is the view being drawn, so counting it on the map would
  // report a number for a graph the user cannot see.
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

// ── Map zoom ──────────────────────────────────────────────────────────────────
// Rung 0 is the finest map and `mapLevels - 1` the coarsest. The control reads
// out how many boxes the rung holds rather than which rung it is, because the
// count is the thing the user is choosing between; the rung number is an
// implementation detail of the ladder underneath it.

function updateZoomControl() {
  const onMap = currentView === 'ClusterMap';
  const live = onMap && mapLevels > 0;
  $('zoom-value').textContent = live ? mapClusters : '—';
  // `+` is more boxes, which is a *finer* rung and so a lower number.
  $('zoom-in').disabled = !live || mapLevel <= 0;
  $('zoom-out').disabled = !live || mapLevel >= mapLevels - 1;
  const control = $('zoom-control');
  control.classList.toggle('inactive', !live);
  control.title = !onMap
    ? 'Grouping applies to the cluster map (Ctrl+M)'
    : live
      ? `${mapClusters} clusters, ${mapLevel + 1} of ${mapLevels} grouping levels`
      : 'Nothing is clustered yet';
}

function setMapLevel(next) {
  if (currentView !== 'ClusterMap' || mapLevels === 0) return;
  const clamped = Math.min(Math.max(next, 0), mapLevels - 1);
  if (clamped === mapLevel) return;
  sendCommand({ SetMapLevel: clamped });
}

$('zoom-in').addEventListener('click', () => setMapLevel(mapLevel - 1));
$('zoom-out').addEventListener('click', () => setMapLevel(mapLevel + 1));

// ── Clustering method ─────────────────────────────────────────────────────────

const methodSelectEl = $('method-select');

function updateMethodControl() {
  const onMap = currentView === 'ClusterMap';
  // Rebuilt only when the list itself changes: replacing the options on every
  // snapshot would close the dropdown under a user who had it open.
  const wanted = clusterMethods.map((m) => m.id).join(',');
  if (methodSelectEl.dataset.methods !== wanted) {
    methodSelectEl.dataset.methods = wanted;
    methodSelectEl.innerHTML = '';
    for (const method of clusterMethods) {
      const option = document.createElement('option');
      option.value = method.id;
      option.textContent = method.label;
      option.title = method.description;
      methodSelectEl.appendChild(option);
    }
  }
  if (clusterMethod != null) methodSelectEl.value = clusterMethod;
  methodSelectEl.disabled = !onMap || clusterMethods.length === 0;
  const chosen = clusterMethods.find((m) => m.id === clusterMethod);
  const control = $('method-control');
  control.classList.toggle('inactive', methodSelectEl.disabled);
  control.title = onMap
    ? chosen?.description ?? 'How the cluster map is built'
    : 'Grouping applies to the cluster map (Ctrl+M)';
}

methodSelectEl.addEventListener('change', () => {
  if (methodSelectEl.value !== clusterMethod) {
    sendCommand({ SetClusterMethod: methodSelectEl.value });
  }
});

// ── Graph relationships ───────────────────────────────────────────────────────
// The client holds the whole call graph, not just the laid-out subset, so it
// can answer "is there more below this node?" without another round trip.

/// Children of `id` in the current direction, as ids.
function childrenOf(id) {
  const outgoing = currentDirection === 'Outgoing';
  const out = [];
  for (const edge of currentGraph.edges || []) {
    if (outgoing && edge.caller === id) out.push(edge.callee);
    else if (!outgoing && edge.callee === id) out.push(edge.caller);
  }
  return [...new Set(out)];
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

function updateDepthControl() {
  // Depth is where the rooted view's BFS stops. The map is bounded by the rung
  // of the ladder it is drawn at instead, so the stepper governs nothing here —
  // and a live control that does nothing when pressed is worse than a quiet
  // one. The zoom control next to it is the map's equivalent.
  const onMap = currentView === 'ClusterMap';
  $('depth-value').textContent = graphDepth;
  $('depth-dec').disabled = onMap || graphDepth <= DEPTH_MIN;
  $('depth-inc').disabled = onMap || graphDepth >= DEPTH_MAX;
  const control = $('depth-control');
  control.classList.toggle('inactive', onMap);
  control.title = onMap
    ? 'Depth applies to the call tree — double-click a cluster to get there'
    : 'How many levels of calls to show';
}

function setDepth(next) {
  if (currentView === 'ClusterMap') return;   // nothing on the map to deepen
  const clamped = Math.min(Math.max(next, DEPTH_MIN), DEPTH_MAX);
  if (clamped === graphDepth) return;
  // Reflect it immediately; the snapshot that follows confirms it.
  graphDepth = clamped;
  updateDepthControl();
  sendCommand({ SetGraphDepth: clamped });
}

$('depth-dec').addEventListener('click', () => setDepth(graphDepth - 1));
$('depth-inc').addEventListener('click', () => setDepth(graphDepth + 1));

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

/// Push `load` to the status bar.
///
/// A stage with a total reads as a fraction; one without reads as a running
/// count, because clangd streams workspace symbols without ever saying how
/// many are coming.
function renderLoad() {
  if (!loading()) return;
  // The numbers belong to whichever stage last set them, and a finished stage's
  // numbers are a lie: painting them is what leaves "Resolving calls 15 / 19" on
  // screen after every call has in fact been resolved.
  if (load.stage === 'edges' && !load.edges) return;
  if (load.stage === 'symbols' && !load.symbols) return;

  const counts = load.total > 0
    ? `${load.done.toLocaleString()} / ${load.total.toLocaleString()}`
    : (load.count != null ? load.count.toLocaleString() : '');

  setStatus(counts ? `${load.label} ${counts}` : load.label, 'busy');
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
        if (phase === 'Completed') setStatus('Ready', 'ready');
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
    if (!loading()) setStatus('Ready', 'ready');
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
    if (!loading()) setStatus('Ready', 'ready');
  } else if (key === 'RefreshingWorkspaceSymbols') {
    setStatus('Refreshing', 'busy');
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

/// Names that more than one symbol answers to.
///
/// Nothing is duplicated when this is non-empty — C gives every `static`
/// function file-local linkage, so jemalloc's test suite really does define
/// nine different `thd_start`s, and a C++ overload set is several functions
/// with one name. They are distinct symbols with distinct call graphs, so the
/// list must keep all of them; what it must not do is draw them identically.
function ambiguousNames(fns) {
  const seen = new Set();
  const dup = new Set();
  for (const id in fns) {
    const name = fns[id].name;
    if (seen.has(name)) dup.add(name);
    else seen.add(name);
  }
  return dup;
}

function renderSidebar(filter) {
  const q = (filter || '').toLowerCase();
  const ambiguous = ambiguousNames(allFunctions);
  const ids = Object.keys(allFunctions)
    .filter(id => {
      const n = allFunctions[id];
      return !q || n.name.toLowerCase().includes(q) || (n.qualified_name || '').toLowerCase().includes(q);
    })
    .sort((a, b) => {
      const byName = allFunctions[a].name.localeCompare(allFunctions[b].name);
      // Same name: keep the group in a stable, readable order rather than
      // whatever the id hash happened to give.
      return byName || symbolWhere(allFunctions[a]).localeCompare(symbolWhere(allFunctions[b]));
    });

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
    const where = symbolWhere(node);
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
  sendCommand({ SetGraphOrientation: next });
}

dirToggleEl.addEventListener('click', () => sendCommand('ToggleCallDirection'));
orientLrEl.addEventListener('click', () => setOrientation('LeftRight'));
orientTdEl.addEventListener('click', () => setOrientation('TopDown'));
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


// ── Theme ─────────────────────────────────────────────────────────────────────

function currentTheme() {
  return document.documentElement.dataset.theme
    || (matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark');
}

function toggleTheme() {
  const next = currentTheme() === 'dark' ? 'light' : 'dark';
  document.documentElement.dataset.theme = next;
  try { localStorage.setItem('coon.theme', next); } catch { /* storage blocked */ }
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
    { id: 'view.direction', label: 'View: Toggle Call Direction', hint: 'Ctrl+D', run: () => sendCommand('ToggleCallDirection') },
    { id: 'view.orientation', label: 'View: Toggle Graph Orientation', hint: 'Ctrl+Shift+D', run: () => setOrientation(currentOrientation === 'LeftRight' ? 'TopDown' : 'LeftRight') },
    { id: 'view.fit', label: 'View: Fit Graph to Screen', disabled: cy.nodes().length === 0, run: fitGraph },
    { id: 'view.deeper', label: 'View: Show More Levels', hint: 'Ctrl+]', disabled: onMap || graphDepth >= DEPTH_MAX, run: () => setDepth(graphDepth + 1) },
    { id: 'view.shallower', label: 'View: Show Fewer Levels', hint: 'Ctrl+[', disabled: onMap || graphDepth <= DEPTH_MIN, run: () => setDepth(graphDepth - 1) },
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

    { id: 'lsp.refresh', label: 'LSP: Refresh Symbols', hint: 'Ctrl+R', run: () => sendCommand('Refresh') },
    { id: 'lsp.refs', label: 'LSP: Find References', disabled: !selectedNodeId, run: () => sendCommand('FindReferences') },
  ];

  // One entry per clustering method, so a method added in `logic` is reachable
  // without the client knowing anything about it but its name.
  clusterMethods.forEach((method) => {
    cmds.push({
      id: `view.method.${method.id}`,
      label: `Map: Group by ${method.label}`,
      hint: method.description,
      disabled: !onMap || method.id === clusterMethod,
      run: () => sendCommand({ SetClusterMethod: method.id }),
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
  const ambiguous = ambiguousNames(allFunctions);
  const items = Object.entries(allFunctions).map(([id, fn]) => ({
    id,
    label: fn.name,
    // Only worth showing when it adds something beyond the bare name — and
    // where the name is not the symbol's own, the location is what picks one
    // `thd_start` out of nine.
    detail: fn.qualified_name && fn.qualified_name !== fn.name
      ? fn.qualified_name
      : (ambiguous.has(fn.name) ? symbolWhere(fn) : ''),
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
  else if (key === 'r') { e.preventDefault(); sendCommand('Refresh'); }
  else if (key === 'e') { e.preventDefault(); exportGraph(); }
  else if (key === 'm' && e.shiftKey) { e.preventDefault(); clusterThisFunction(); }
  else if (key === 'm') { e.preventDefault(); toggleView(); }
  else if (key === 'd' && e.shiftKey) { e.preventDefault(); setOrientation(currentOrientation === 'LeftRight' ? 'TopDown' : 'LeftRight'); }
  else if (key === 'd') { e.preventDefault(); sendCommand('ToggleCallDirection'); }
  // One pair of keys, one meaning — more or less detail — pointed at whichever
  // view is showing: the tree's depth limit, or the map's grouping.
  else if (key === ']') {
    e.preventDefault();
    if (currentView === 'ClusterMap') setMapLevel(mapLevel - 1); else setDepth(graphDepth + 1);
  }
  else if (key === '[') {
    e.preventDefault();
    if (currentView === 'ClusterMap') setMapLevel(mapLevel + 1); else setDepth(graphDepth - 1);
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
