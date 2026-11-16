"use strict";
// Arrow routing for the Architecture summary (#637), as arithmetic on frames: no DOM. Pele lays
// out the bands; routing then opens a gutter under each band row and routes every arrow
// orthogonally through gutters, a left side channel (long upward arrows), a right channel (long
// downward arrows, and runs to a farther shared box) and, when needed, a strip under the
// drawing. No arrow segment enters a band or a chip: arrows leave and reach bands only at their
// borders. Each count tag sits in a gutter's tag rows, on its own arrow's vertical run.
//
// The Pele adapter (architecture-pele.js) reads frames from the drawing, calls `plan`, moves the
// bands by `placement`, then calls `paths` with the moved frames and draws each polyline. Two
// tables carry the cases: PLAN, keyed by how an arrow's ends relate, and PATH, keyed by the
// plan's kind. A frame is `{x, y, w, h}`; `band(id)` returns the frame of a band or box.
// Loaded before architecture-pele.js, which reads it as window.TrellisArchitectureRouting.
(() => {
  // A gutter under each band row holds count tags (rows of the tag height plus TAG_GAP), then
  // one TRACK per horizontal run; DROP is the least vertical run that ends at a band, so an
  // arrowhead always fits. Long arrows run in side-channel lanes LANE apart.
  const GUTTER_PAD = 4, TAG_GAP = 4, TRACK = 10, LANE = 10, DROP = 18, MIN_GUTTER = 24, CHANNEL_PAD = 8, INSET = 10, SLOT_GAP = 6;
  const round = value => Math.round(value * 100) / 100;
  // Place vertical runs (and the tags on them) across one gutter edge, left to right.
  // Each run is a LANE-wide line through every tag row; a tagged run also holds its tag in one
  // row. Runs scan outward from their preferred x within their band's span.
  function slots(items) {
    const boxes = [], lines = [];
    const clear = (list, l, r) => !list.some(([a, b]) => l < b && a < r);
    const candidates = item => {
      const half = item.tag ? item.tag.w / 2 : LANE / 2, lo = item.lo + half, hi = item.hi - half;
      if (hi < lo) return [(item.lo + item.hi) / 2];
      const pref = Math.min(hi, Math.max(lo, item.pref)), list = [pref];
      for (let d = 2; pref - d >= lo || pref + d <= hi; d += 2) { if (pref - d >= lo) list.push(pref - d); if (pref + d <= hi) list.push(pref + d); }
      return list;
    };
    let rows = 0;
    for (const item of items) {
      const line = x => [x - LANE / 2 - SLOT_GAP / 2, x + LANE / 2 + SLOT_GAP / 2];
      const lineFree = (x, skip) => clear(lines, ...line(x)) && boxes.every(([row, l, r]) => row === skip || !(line(x)[0] < r && l < line(x)[1]));
      let placed = false;
      if (item.tag) {
        for (let row = 0; row < 8 && !placed; row++) for (const x of candidates(item)) {
          const l = x - item.tag.w / 2 - SLOT_GAP / 2, r = x + item.tag.w / 2 + SLOT_GAP / 2;
          if (lineFree(x, row) && clear(boxes.filter(box => box[0] === row).map(box => [box[1], box[2]]), l, r) && clear(lines, l, r)) {
            Object.assign(item, {x, row}); boxes.push([row, l, r]); rows = Math.max(rows, row + 1); placed = true; break;
          }
        }
        if (!placed) { item.x = candidates(item)[0]; item.row = rows; boxes.push([rows, item.x - item.tag.w / 2, item.x + item.tag.w / 2]); rows++; }
      } else {
        for (const x of candidates(item)) if (lineFree(x, -1)) { item.x = x; placed = true; break; }
        if (!placed) item.x = candidates(item)[0];
      }
      lines.push(line(item.x));
    }
    return rows;
  }
  // A side channel's width: its lanes and their padding. Tags stay in the gutters.
  const channelWidth = lanes => lanes ? lanes * LANE + 2 * CHANNEL_PAD : 0;
  // Each band's row in the ordered stack, and each shared box's index.
  function indexes(layout) {
    const rowOf = new Map(), sharedOf = new Map();
    layout.rows.forEach((ids, row) => ids.forEach(id => rowOf.set(id, row)));
    layout.shared.forEach((id, index) => sharedOf.set(id, index));
    return {rowOf, sharedOf};
  }
  // How an arrow's ends relate: by their rows in the stack (`rs`, `rd`) or, with no row, by
  // their shared boxes (`ks`, `kd`). The key of its PLAN row, or null when it is not routed.
  function relation(rs, rd, ks, kd) {
    if (rs !== undefined && rd !== undefined) return Math.abs(rs - rd) === 1 ? "adjacent" : rs === rd ? "peer" : rs < rd ? "down" : "up";
    if (rs !== undefined || rd !== undefined) return "shared";
    return ks !== undefined && kd !== undefined ? "boxes" : null;
  }
  // The side-channel lanes the arrows of `edges` (each `{from, to}`) take. The adapter reserves
  // their width before the fit; it counts with the same relations as PLAN.
  function countLanes(layout, edges) {
    const {rowOf, sharedOf} = indexes(layout);
    let left = 0, right = 0;
    for (const edge of edges) {
      const rs = rowOf.get(edge.from), rd = rowOf.get(edge.to), kind = relation(rs, rd);
      if (kind === "up") left++;
      else if (kind === "down") right++;
      else if (kind === "shared" && sharedOf.get(rs === undefined ? edge.from : edge.to) > 0) right++;
    }
    return {left, right};
  }
  // Planning state for one drawing. Gutter r lies under band row r; "strip" lies under the
  // stack and shared boxes. `top` and `bottom` reserve a vertical run at a gutter's upper or
  // lower edge within a band's span; `track` reserves one horizontal run in a gutter.
  function planner(layout, band) {
    const gutters = new Map(), gutter = key => { if (!gutters.has(key)) gutters.set(key, {tops:[], bottoms:[], tracks:[]}); return gutters.get(key); };
    const span = id => { const f = band(id); return {lo:f.x + INSET, hi:f.x + f.w - INSET, mid:f.x + f.w / 2}; };
    const top = (plan, key, id, extra) => { const s = span(id), entry = {plan, id, lo:s.lo, hi:s.hi, pref:s.mid, ...extra}; gutter(key).tops.push(entry); return entry; };
    const bottom = (plan, key, id, extra) => { const s = span(id), entry = {plan, id, lo:s.lo, hi:s.hi, pref:s.mid, ...extra}; gutter(key).bottoms.push(entry); return entry; };
    const track = (plan, key) => { const list = gutter(key).tracks; list.push(plan); return {key, index:list.length - 1}; };
    return {layout, gutters, gutter, span, top, bottom, track, left:[], right:[], ...indexes(layout)};
  }
  // The plan table: one row per relation. Each fills `plan` (its kind, the runs it reserved and
  // the gutter `key` its tag sits in) for the arrow `from` -> `to` with rows `rs`, `rd`.
  const PLAN = {
    // Adjacent rows share one gutter: straight down or up when the bands overlap, else one jog.
    adjacent(p, plan, {from, to, rs, rd}) {
      const {tag} = plan, upper = rs < rd ? from : to, lower = rs < rd ? to : from, key = Math.min(rs, rd), u = p.span(upper), l = p.span(lower);
      const lo = Math.max(u.lo, l.lo), hi = Math.min(u.hi, l.hi);
      if (hi - lo >= tag.w) { plan.kind = "straight"; plan.lower = lower; plan.top = p.top(plan, key, upper, {lo, hi, pref:(lo + hi) / 2, tag, straight:true}); p.gutter(key).bottoms.push(plan.top); }
      else { plan.kind = "jog"; plan.top = p.top(plan, key, upper, {tag, pref:l.mid}); plan.bottom = p.bottom(plan, key, lower, {pref:u.mid}); plan.track = p.track(plan, key); }
      plan.down = rs < rd; plan.key = key;
    },
    // Peers: down into the gutter under their row, across, and back up.
    peer(p, plan, {from, to, rs}) {
      const {tag} = plan, s = p.span(from), d = p.span(to);
      plan.kind = "peer"; plan.key = rs;
      plan.top = p.top(plan, rs, from, {tag, pref:s.mid < d.mid ? s.hi : s.lo}); plan.end = p.top(plan, rs, to, {pref:s.mid < d.mid ? d.lo : d.hi}); plan.track = p.track(plan, rs);
    },
    // Long downward: under the source row (its tag there), through the right channel, into the target from above.
    down(p, plan, {from, to, rs, rd}) {
      const {tag} = plan;
      plan.kind = "right"; plan.channel = p.right; p.right.push(plan); plan.key = rs;
      plan.top = p.top(plan, rs, from, {tag, pref:p.span(from).hi}); plan.first = p.track(plan, rs);
      plan.bottom = p.bottom(plan, rd - 1, to, {pref:p.span(to).hi}); plan.last = p.track(plan, rd - 1);
    },
    // Long upward: above the source row, through the left side channel, into the target from
    // below; its tag sits in the gutter under the target, on the arrow's last run.
    up(p, plan, {from, to, rs, rd}) {
      const {tag} = plan;
      plan.kind = "left"; plan.channel = p.left; p.left.push(plan); plan.key = rd;
      plan.bottom = p.bottom(plan, rs - 1, from, {pref:p.span(from).lo}); plan.first = p.track(plan, rs - 1);
      plan.top = p.top(plan, rd, to, {tag, pref:p.span(to).lo}); plan.last = p.track(plan, rd);
    },
    // An ordered band and a shared box: through the gutter under the band's row, across to the
    // nearest shared box's side (right or, #850, left), or by the right channel and the strip to a farther one.
    shared(p, plan, {from, to, rs}) {
      const {tag} = plan, ordered = rs !== undefined ? from : to, box = rs !== undefined ? to : from, row = p.rowOf.get(ordered), k = p.sharedOf.get(box);
      plan.kind = k === 0 ? "shared" : "far"; plan.toShared = rs !== undefined; plan.box = box; plan.key = row;
      plan.top = p.top(plan, row, ordered, {tag, pref:p.layout.sharedLeft ? p.span(ordered).lo : p.span(ordered).hi}); plan.first = p.track(plan, row);
      if (k > 0) { p.right.push(plan); plan.channel = p.right; plan.end = p.top(plan, "strip", box, {}); plan.last = p.track(plan, "strip"); }
    },
    // Two shared boxes: down into the strip under them, across, and back up.
    boxes(p, plan, {from, to}) {
      const {tag} = plan;
      plan.kind = "boxes"; plan.key = "strip";
      plan.top = p.top(plan, "strip", from, {tag}); plan.end = p.top(plan, "strip", to, {}); plan.track = p.track(plan, "strip");
    },
  };
  // Horizontal placement, then each gutter's height: tag rows, tracks and arrowhead room.
  function sizeGutters(gutters, rowCount) {
    for (const [key, g] of gutters) {
      const order = list => [...list].sort((a, b) => (b.tag ? 1 : 0) - (a.tag ? 1 : 0) || a.plan.edge.id.localeCompare(b.plan.edge.id));
      g.tagRows = slots(order(g.tops));
      const bottoms = g.bottoms.filter(entry => !entry.straight);
      // Bottom runs only meet the lower edge, under the tag rows: they avoid other runs, not tags.
      const lines = g.bottoms.filter(entry => entry.straight).map(entry => entry.x);
      for (const entry of order(bottoms)) {
        const options = [entry.pref]; for (let d = 2; entry.pref - d >= entry.lo || entry.pref + d <= entry.hi; d += 2) options.push(entry.pref - d, entry.pref + d);
        entry.x = options.find(x => x >= entry.lo && x <= entry.hi && lines.every(other => Math.abs(other - x) >= LANE + SLOT_GAP)) ?? Math.min(entry.hi, Math.max(entry.lo, entry.pref));
        lines.push(entry.x);
      }
      const tagH = Math.max(0, ...g.tops.filter(entry => entry.tag).map(entry => entry.tag.h));
      g.zone = g.tops.length ? Math.max(DROP, g.tagRows * (tagH + TAG_GAP)) : 0;
      g.tagH = tagH;
      g.height = GUTTER_PAD + g.zone + g.tracks.length * TRACK + (g.bottoms.length || g.tracks.length ? DROP : GUTTER_PAD);
      if (typeof key === "number" && key < rowCount - 1) g.height = Math.max(MIN_GUTTER, g.height);
    }
  }
  // How far each part moves sideways for the side channels. The channel between the stack and the
  // shared boxes widens only past the gap already there: the right one when the boxes are right
  // of the stack, the left one when they are left of it (#850). `stackDx` moves the stack and
  // its rows, `sharedDx` the shared boxes, `baseDx` the unassigned row; `grow` widens the drawing.
  function channelOffsets(layout, stack, sharedFrames, lanesLeft, lanesRight) {
    const LW = channelWidth(lanesLeft), CW = channelWidth(lanesRight);
    let inner, stackDx, sharedDx, baseDx, grow;
    if (layout.sharedLeft) {
      inner = Math.max(0, LW - (stack.x - Math.max(...sharedFrames.map(f => f.x + f.w))));
      stackDx = inner; sharedDx = 0; baseDx = 0; grow = inner + CW;
    } else {
      const firstShared = sharedFrames.length ? Math.min(...sharedFrames.map(f => f.x)) : null;
      inner = Math.max(0, CW - (firstShared === null ? 0 : firstShared - (stack.x + stack.w)));
      stackDx = LW; sharedDx = LW + inner; baseDx = LW; grow = LW + inner;
    }
    return {inner, stackDx, sharedDx, baseDx, grow};
  }
  // Plan every arrow of one drawing from the frames Pele drew. `stack` is the stack's frame,
  // `edges` the arrows (`{id, from, to}`), `tag(edge)` an arrow's count tag size `{w, h}`.
  // The result holds the plans, the gutters and how far every part moves (see `placement`).
  function plan({layout, stack, edges, band, tag}) {
    const p = planner(layout, band), plans = [];
    const rows = layout.rows.map(ids => { const frames = ids.map(band); return {top:Math.min(...frames.map(f => f.y)), bottom:Math.max(...frames.map(f => f.y + f.h))}; });
    for (const edge of edges) {
      const entry = {edge, tag:tag(edge)}, from = edge.from, to = edge.to, rs = p.rowOf.get(from), rd = p.rowOf.get(to);
      plans.push(entry);
      PLAN[relation(rs, rd, p.sharedOf.get(from), p.sharedOf.get(to))]?.(p, entry, {from, to, rs, rd});
    }
    const {gutters, left, right, rowOf, sharedOf} = p;
    sizeGutters(gutters, rows.length);
    // Space between rows that Pele already leaves, and what each gutter adds to it.
    const gap = r => (r + 1 < rows.length ? rows[r + 1].top : stack.y + stack.h) - rows[r].bottom;
    const extra = rows.map((_, r) => Math.max(0, (gutters.get(r)?.height ?? (r < rows.length - 1 ? MIN_GUTTER : 0)) - gap(r)));
    const below = r => extra.slice(0, r).reduce((sum, value) => sum + value, 0), total = below(rows.length);
    const sharedFrames = layout.shared.map(band), offsets = channelOffsets(layout, stack, sharedFrames, left.length, right.length);
    const strip = gutters.get("strip");
    const rootBottom = Math.max(stack.y + stack.h, ...sharedFrames.map(f => f.y + f.h));
    const stripH = strip ? strip.height : 0;
    const routing = {layout, stack, plans, gutters, rows, left, right, rowOf, sharedOf, below, total, rootBottom, stripH, ...offsets};
    const {trackY} = tracks(routing);
    for (const entry of [...left, ...right]) entry.reach = Math.abs(trackY(entry.first) - trackY(entry.last ?? entry.first));
    return routing;
  }
  // Where the part owned by `owner` moves: with its band row, with the shared boxes, with the
  // unassigned row under the strip, or with the stack. `part` names which, so the caller can
  // also grow that part's own box.
  function placement(routing, owner) {
    const {rowOf, sharedOf, layout} = routing;
    if (rowOf.has(owner)) return {part:"row", dx:routing.stackDx, dy:routing.below(rowOf.get(owner))};
    if (sharedOf.has(owner)) return {part:"shared", dx:routing.sharedDx, dy:0};
    if (owner && owner === layout.unassigned) return {part:"unassigned", dx:routing.baseDx, dy:routing.total + routing.stripH};
    return {part:"stack", dx:routing.stackDx, dy:0};
  }
  // The top of a gutter and the y of one reserved track, in final coordinates.
  function tracks({rows, gutters, below, total, rootBottom}) {
    const gutterTop = key => key === "strip" ? rootBottom + total : rows[key].bottom + below(key);
    const trackY = ({key, index}) => round(gutterTop(key) + GUTTER_PAD + gutters.get(key).zone + (index + 0.5) * TRACK);
    return {gutterTop, trackY};
  }
  // The path table: one row per plan kind. Each returns the arrow's polyline, source first, from
  // `g`: `X` (a run's final x), `f` (a moved frame and its bottom), `trackY` and `laneX`.
  const across = (g, plan) => {
    const y = g.trackY(plan.track), xs = g.X(plan.top), xd = g.X(plan.end);
    return [[xs, g.f(plan.top.id).bottom], [xs, y], [xd, y], [xd, g.f(plan.end.id).bottom]];
  };
  const channel = (g, plan) => {
    const {edge} = plan, lane = g.laneX(plan.channel, plan), y1 = g.trackY(plan.first), y2 = g.trackY(plan.last), xs = g.X(plan.kind === "left" ? plan.bottom : plan.top), xd = g.X(plan.kind === "left" ? plan.top : plan.bottom);
    const start = plan.kind === "left" ? g.f(edge.from).y : g.f(edge.from).bottom, end = plan.kind === "left" ? g.f(edge.to).bottom : g.f(edge.to).y;
    return [[xs, start], [xs, y1], [lane, y1], [lane, y2], [xd, y2], [xd, end]];
  };
  const toBox = (g, plan) => {
    const xs = g.X(plan.top), y1 = g.trackY(plan.first), band = g.f(plan.top.id).bottom;
    let run;
    if (plan.kind === "shared") run = [[xs, band], [xs, y1], [g.layout.sharedLeft ? g.f(plan.box).x + g.f(plan.box).w : g.f(plan.box).x, y1]];
    else { const lane = g.laneX(g.right, plan), y2 = g.trackY(plan.last), xk = g.X(plan.end); run = [[xs, band], [xs, y1], [lane, y1], [lane, y2], [xk, y2], [xk, g.f(plan.box).bottom]]; }
    return plan.toShared ? run : run.reverse();
  };
  const PATH = {
    straight(g, plan) {
      const x = g.X(plan.top), upper = g.f(plan.top.id).bottom, lower = g.f(plan.lower).y;
      return plan.down ? [[x, upper], [x, lower]] : [[x, lower], [x, upper]];
    },
    jog(g, plan) {
      const xt = g.X(plan.top), xb = g.X(plan.bottom), y = g.trackY(plan.track), upper = g.f(plan.top.id).bottom, lower = g.f(plan.bottom.id).y;
      return plan.down ? [[xt, upper], [xt, y], [xb, y], [xb, lower]] : [[xb, lower], [xb, y], [xt, y], [xt, upper]];
    },
    peer:across, boxes:across, left:channel, right:channel, shared:toBox, far:toBox,
  };
  // Final coordinates, once the caller has moved the bands by `placement`: `band(id)` now
  // returns the moved frames. Calls `each(edge, points, tag)` per routed arrow, in plan order:
  // `points` is its polyline and `tag` the centre `{cx, cy}` of its count tag.
  function paths(routing, band, each) {
    const {layout, stack, plans, gutters, left, right, sharedOf, stackDx, sharedDx} = routing;
    // x moves with the channels left of each box.
    const dx = id => sharedOf.has(id) ? sharedDx : stackDx, f = id => { const b = band(id); return {x:b.x, y:b.y, w:b.w, h:b.h, bottom:b.y + b.h}; };
    const X = entry => round(entry.x + dx(entry.id));
    const {gutterTop, trackY} = tracks(routing);
    const tagAt = (entry, key) => { const g = gutters.get(key); return {cx:X(entry), cy:round(gutterTop(key) + GUTTER_PAD + entry.row * (g.tagH + TAG_GAP) + entry.tag.h / 2)}; };
    const stackLeft = stack.x + stackDx, stackRight = stack.x + stack.w + stackDx;
    const laneX = (list, plan) => {
      const sorted = [...list].sort((a, b) => a.reach - b.reach || a.edge.id.localeCompare(b.edge.id)), i = sorted.indexOf(plan);
      return list === left ? round(stackLeft - CHANNEL_PAD - (i + 0.5) * LANE) : round(stackRight + CHANNEL_PAD + (i + 0.5) * LANE);
    };
    const g = {layout, right, X, f, trackY, laneX};
    for (const entry of plans) {
      const path = PATH[entry.kind];
      if (!path) continue;
      const points = path(g, entry);
      each(entry.edge, points, tagAt(entry.top, entry.key));
    }
  }
  window.TrellisArchitectureRouting = Object.freeze({plan, placement, paths, countLanes, channelWidth, relation, slots, round, PLAN, PATH});
})();
