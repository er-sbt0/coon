---

### P0 — Visually Broken

**1. Shared vertical spine (most damaging issue)**
All edges between the same pair of layers compute `mid_x = (from.x + to.x) / 2`. Since every edge from layer N has the same `from.x = layer_N * 28 + 23` and every edge to layer N+1 has the same `to.x = (layer_N+1) * 28`, all their vertical bars land on the *identical* column. Every edge between a layer pair collapses into one thick line — this is the prominent tangled bar visible in the screenshot center.

**2. Single exit port per parent, shared by all children**
All children of a node exit from identical coordinates `(pos.x + 23, pos.y + 1.5)`. With 3 children, three horizontal lines start at the same pixel and travel to mid_x — they look like one edge until the vertical bar. Combined with issue #1, multiple edges are completely invisible.

---

### P1 — High / Confusing

**3. Vertical bar cuts through sibling node bounding boxes**
The vertical segment at mid_x spans from the topmost to the bottommost child's y. If that range overlaps the y-range of other nodes in the same layers, the bar visually passes through those boxes.

**4. Arrowhead lands inside the child node border**
`render_arrowhead` places `▶` at `seg.to`, which is the child node's left-edge x (`pos.x`). That's the position of the node's left border character (`│`), so the arrowhead overwrites the box border — it should be at `pos.x - 1`.

**5. Aspect ratio of world coordinates**
Terminal cells are ~2:1 (height:width). `node_separation = 5` (row gap) vs `level_separation - node_width = 4` (column gap between layers). The vertical spacing appears much larger than horizontal, creating a "squashed" horizontal layout. `level_separation` needs to account for char-width vs row-height ratio — effectively `level_separation` should be ≈ 2–3× what `node_separation` is.

---

### P2 — Medium / Aesthetic

**6. Label left-aligned inside node**
`Paragraph::new(label)` defaults to left alignment. Center alignment looks standard for graph nodes.

**7. Root node not visually distinct**
Root is Yellow, same as every unselected node. The function being analyzed has no special color — Green, Magenta, or a different border style would make it immediately identifiable.

**8. Status bar overwrites the outer border**
`let status_y = area.y + area.height.saturating_sub(1)` writes into the outer block's bottom border row. Should be `area.y + area.height - 2` (inside the content area).

---

### P3 — Low

**9. `iter_visible` uses node top-left for visibility test**
A node at `screen_pos.x = inner_area.width - 2` passes the bounds check but 22 of its 24 chars are clipped. Should check `screen_pos.x + node_width > 0 && screen_pos.x < area.width`.

**10. Navigation to parent is arbitrary for shared nodes**
`navigate_to_parent` picks `predecessors.first()` — if a node has multiple parents (the whole point of a DAG), the second parent is unreachable by keyboard.

---

**Most impactful fix:** P0 issue #1 — the shared vertical spine. The fix is to **stagger each edge's `mid_x`** based on its vertical position within the bundle: instead of `(from.x + to.x) / 2`, assign each edge a unique x in the inter-layer gap proportional to its y-spread. This is the standard "bundled orthogonal routing" approach. Shall I start with that?
