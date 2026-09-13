// ── Command palette / fuzzy picker ───────────────────────────────────────────
//
// One overlay serves both the command palette and the symbol finder. It knows
// nothing about commands or symbols: callers hand it a list of items and get a
// callback when one is chosen. Matching, highlighting and keyboard navigation
// live here; what the items mean lives in app.js.
//
//   CoonPalette.show({ placeholder, items, onPick, onAltPick, altHint })
//
// item: { id, label, detail?, hint?, mono?, disabled? }

window.CoonPalette = (() => {
  const root    = document.getElementById('palette');
  const input   = document.getElementById('palette-input');
  const list    = document.getElementById('palette-list');
  const empty   = document.getElementById('palette-empty');
  const footer  = document.getElementById('palette-footer');

  const MAX_RENDERED = 200;

  let items = [];
  let filtered = [];
  let active = 0;
  let handlers = {};

  // ── Fuzzy matching ─────────────────────────────────────────────────────────

  /// True when position `i` starts a word: string start, or after a separator,
  /// or a camelCase hump. C++ symbols are mostly snake_case and `::`-qualified,
  /// so boundary awareness is what makes short queries land usefully.
  function isBoundary(text, i) {
    if (i === 0) return true;
    const prev = text[i - 1];
    const cur = text[i];
    if (prev === '_' || prev === ':' || prev === '.' || prev === ' ' || prev === '-') return true;
    return prev === prev.toLowerCase() && prev !== prev.toUpperCase()
        && cur === cur.toUpperCase() && cur !== cur.toLowerCase();
  }

  /// Subsequence match with boundary and adjacency scoring.
  /// Returns { score, indices } or null when `query` isn't a subsequence.
  ///
  /// Two passes, because preferring word starts is a ranking rule that must
  /// never decide whether there is a match at all. Skipping ahead to a boundary
  /// can step clean over the letters the rest of the query still needs:
  /// "cluster" against "View: Cluster Map of This Function" matches `clus`
  /// inside Cluster, then takes the `T` of **T**his over the `t` of Clus**t**er
  /// — and there is no `e` after it, so a label the user is looking straight at
  /// comes back as no match. Greedy has no way back, so when it fails, fall
  /// back to the plain leftmost subsequence, which cannot skip anything.
  function fuzzyMatch(query, text) {
    if (!query) return { score: 0, indices: [] };
    return scan(query, text, true) ?? scan(query, text, false);
  }

  function scan(query, text, preferBoundary) {
    const q = query.toLowerCase();
    const t = text.toLowerCase();
    const indices = [];
    let cursor = 0;
    let score = 0;
    let last = -2;

    for (const ch of q) {
      let found = -1;
      let from = cursor;

      // Take the next occurrence, but prefer one that starts a word.
      for (;;) {
        const at = t.indexOf(ch, from);
        if (at === -1) break;
        if (found === -1) found = at;
        if (!preferBoundary || isBoundary(text, at)) { found = at; break; }
        from = at + 1;
      }
      if (found === -1) return null;

      if (found === last + 1) score += 12;
      else score -= Math.min(found - last - 1, 8);
      if (isBoundary(text, found)) score += 10;
      if (found === 0) score += 14;

      indices.push(found);
      last = found;
      cursor = found + 1;
    }

    // Nudge shorter names ahead of longer ones that match equally well.
    return { score: score - text.length * 0.05, indices };
  }

  function filter(query) {
    const scored = [];
    for (const item of items) {
      let m = fuzzyMatch(query, item.label);
      let indices = m ? m.indices : [];
      if (!m && item.detail) {
        // Fall back to the qualified name, but don't highlight what isn't shown.
        const d = fuzzyMatch(query, item.detail);
        if (d) { m = { score: d.score - 20 }; indices = []; }
      }
      if (m) scored.push({ item, score: m.score, indices });
    }
    // Stable order for an empty query; best-first once the user types.
    if (query) scored.sort((a, b) => b.score - a.score);
    return scored.slice(0, MAX_RENDERED);
  }

  // ── Rendering ──────────────────────────────────────────────────────────────

  function highlight(text, indices) {
    if (!indices.length) return document.createTextNode(text);
    const frag = document.createDocumentFragment();
    let at = 0;
    for (const i of indices) {
      if (i > at) frag.appendChild(document.createTextNode(text.slice(at, i)));
      const mark = document.createElement('mark');
      mark.textContent = text[i];
      frag.appendChild(mark);
      at = i + 1;
    }
    if (at < text.length) frag.appendChild(document.createTextNode(text.slice(at)));
    return frag;
  }

  function render() {
    list.innerHTML = '';
    empty.hidden = filtered.length > 0;

    filtered.forEach((entry, index) => {
      const { item, indices } = entry;
      const li = document.createElement('li');
      li.className = 'palette-item'
        + (index === active ? ' active' : '')
        + (item.disabled ? ' disabled' : '');
      li.setAttribute('role', 'option');
      li.setAttribute('aria-selected', String(index === active));

      const main = document.createElement('span');
      main.className = 'palette-label' + (item.mono ? ' mono' : '');
      main.appendChild(highlight(item.label, indices));
      li.appendChild(main);

      if (item.detail) {
        const detail = document.createElement('span');
        detail.className = 'palette-detail';
        detail.textContent = item.detail;
        li.appendChild(detail);
      }
      if (item.hint) {
        const hint = document.createElement('kbd');
        hint.className = 'palette-hint';
        hint.textContent = item.hint;
        li.appendChild(hint);
      }

      li.addEventListener('mousemove', () => setActive(index));
      li.addEventListener('mousedown', (e) => { e.preventDefault(); pick(e.altKey || e.ctrlKey); });
      list.appendChild(li);
    });
  }

  function setActive(index) {
    if (!filtered.length) return;
    active = Math.max(0, Math.min(index, filtered.length - 1));
    [...list.children].forEach((el, i) => {
      el.classList.toggle('active', i === active);
      el.setAttribute('aria-selected', String(i === active));
    });
    list.children[active]?.scrollIntoView({ block: 'nearest' });
  }

  function pick(alt) {
    const entry = filtered[active];
    if (!entry || entry.item.disabled) return;
    const { item } = entry;
    // Capture the callbacks first: close() resets `handlers`, so reading them
    // afterwards silently does nothing.
    const { onPick, onAltPick } = handlers;
    close();
    if (alt && onAltPick) onAltPick(item);
    else onPick?.(item);
  }

  // ── Public API ─────────────────────────────────────────────────────────────

  function show(config) {
    items = config.items || [];
    handlers = config;
    active = 0;

    input.value = '';
    input.placeholder = config.placeholder || 'Type to search…';
    footer.textContent = config.altHint || '';
    footer.hidden = !config.altHint;

    filtered = filter('');
    render();

    root.hidden = false;
    input.focus();
  }

  function close() {
    root.hidden = true;
    items = [];
    filtered = [];
    handlers = {};
  }

  const isOpen = () => !root.hidden;

  input.addEventListener('input', () => {
    filtered = filter(input.value.trim());
    active = 0;
    render();
  });

  input.addEventListener('keydown', (e) => {
    // The palette owns the keyboard while open; don't let app shortcuts fire.
    e.stopPropagation();
    if (e.key === 'ArrowDown') { e.preventDefault(); setActive(active + 1); }
    else if (e.key === 'ArrowUp') { e.preventDefault(); setActive(active - 1); }
    else if (e.key === 'PageDown') { e.preventDefault(); setActive(active + 8); }
    else if (e.key === 'PageUp') { e.preventDefault(); setActive(active - 8); }
    else if (e.key === 'Home') { e.preventDefault(); setActive(0); }
    else if (e.key === 'End') { e.preventDefault(); setActive(filtered.length - 1); }
    else if (e.key === 'Enter') { e.preventDefault(); pick(e.altKey || e.ctrlKey); }
    else if (e.key === 'Escape') { e.preventDefault(); close(); }
  });

  document.getElementById('palette-backdrop').addEventListener('mousedown', close);

  return { show, close, isOpen, fuzzyMatch };
})();
