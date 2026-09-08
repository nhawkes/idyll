// runtime.js — the hand-written browser runtime for an idyll app. No framework JS is
// generated or bundled: the app is a wasm component (jco-transpiled to a glue module),
// and this file is the **client fold** of its render output. Every served asset —
// this file, the glue, the wasm — is content-addressed and immutable; the document is
// the one mutable pointer, and it names the app module on this script's own tag.
//
// The app has one render output — **templates as typed IR** (flat pre-order node lists;
// slots are first-class node kinds) plus a stream of DOM commands — and every way of
// showing it is a fold of that output: the server folds it to an HTML string (`fold_html`
// in idyll); this folds the *identical* data into the live document. Nothing here parses
// markup: build mode materializes DOM straight from the IR (`createElement` /
// `createTextNode` — no `innerHTML`, so no parser quirks), and claim mode tandem-walks
// the IR against the server-rendered DOM.
//
// This file ships only on pages whose paint declared live (a markerless page in
// prod carries no framework at all; dev adds it everywhere for the reload client).
// The page itself is content — the server rendered it, and this runtime never
// touches it. Each `<idyll-live data-i="name">` element gets its own
// `mount((name, instance), parent, seed)` call and its own claim — but every mount
// lands in the ONE guest runtime, so ids are global, the fold state is shared, and a
// stream returned by any export may touch any live's nodes (that is how live
// sharing a store update together). After the claim, `dispatch` patches fold in
// build mode. `<a>` navigation is native, always — an app that wants SPA behaviour
// puts a live at the top and drives navigation as data.
// The stream contract both folds rely on is pinned twice: shape by
// `examples/todo/app/tests/command_stream.rs`, fold semantics (build/claim
// convergence, THIS file against jsdom) by `idyll-serve/tests/client_fold.rs`.
//
// Events are **delegated**: one document-level capture listener per event type, installed
// at import time — before the wasm has even downloaded. Until the live are live,
// events are queued (value/key snapshotted at event time) and replayed through `dispatch`
// in order afterwards, so early clicks are deferred, never dropped.

// The app component (the live table) loads LAZILY, on the first live mount. The
// document names the module (`data-app` on this script's tag); one import promise,
// every mount awaits the same instance.
const APP_URL = document.querySelector('script[data-app]')?.dataset.app ?? null;
let appPromise = null;
function loadApp() {
  appPromise ??= APP_URL
    ? import(APP_URL)
    : Promise.reject(new Error('the page declares live but the document names no app module'));
  return appPromise;
}

// ── Live chunks ──────────────────────────────────────────────────────────────────
//
// A live's code rides ordinary content-addressed assets (immutable, brotli,
// HTTP-cached, `window.__IDYLL_CHUNKS__` maps live → chunk URLs); the primary —
// memory owner, shared code, the bulk — is the module the glue fetches. A chunk
// streams (`instantiateStreaming`) and links into the primary's live instance via
// the shared function table (`wasm-split`): one memory, one store, one program.
// Loading order is the whole contract — a chunk links before its live mounts (an
// unlinked chunk's table slots trap). The only state here is per-page memoization;
// the platform's HTTP cache owns everything else.

const CHUNKS = window.__IDYLL_CHUNKS__ ?? null;
let coreExports = null;
const chunkLinks = new Map(); // chunk URL → Promise<linked>
const chunkResponses = new Map();
const linkedIslands = new Set(); // live names linked here — the dev reload gate

// The patched glue's one hook: the instantiated core's exports, which live
// chunks import (namespace '').
globalThis.__IDYLL_CHUNKS = {
  core: (exports) => {
    coreExports = exports;
  },
};

function fetchChunk(url) {
  if (!chunkResponses.has(url)) {
    chunkResponses.set(url, fetch(url));
  }
  return chunkResponses.get(url);
}

function linkChunk(url) {
  if (!chunkLinks.has(url)) {
    chunkLinks.set(
      url,
      WebAssembly.instantiateStreaming(fetchChunk(url), { '': coreExports })
    );
  }
  return chunkLinks.get(url);
}

/** Link a live's chunks into the live instance — awaited before its mount. */
async function ensureChunks(name) {
  if (!CHUNKS) return;
  const urls = CHUNKS.live[name] ?? [];
  if (urls.length > 0) linkedIslands.add(name);
  for (const url of urls) await linkChunk(url);
}

/** Version-skew recovery: a stale document can name assets a deploy has swept — the
 * lazy app import is the one such load that fails as a promise rejection rather than
 * a resource error (the document's inline guard owns those). Reload at most once per
 * 10s (the fresh document is the re-sync); past the limit, stay on the SSR page and
 * say why. */
function recoverSkew(what, err) {
  const now = Date.now();
  if (now - (sessionStorage.getItem('idyll-skew') || 0) > 1e4) {
    sessionStorage.setItem('idyll-skew', now);
    location.reload();
  } else {
    console.error(`idyll: ${what} — repeated failure; staying on the server-rendered page`, err);
  }
}

// ── Event delegation (installed immediately; queues until live) ──────────────────

/** DOM node → (event type → { live, handler }). The delegated listeners consult this. */
const listeners = new Map();
let live = false;
const queue = [];
const DELEGATED = ['click', 'input', 'change', 'keydown', 'keyup', 'submit', 'blur', 'focus'];
const delegated = new Set();

// Adopt the document's early-capture queue: the inline prelude snippet records input
// from first paint until this module executes (its record shape matches ours by
// contract — the type lists are pinned equal by an idyll-serve test). Retire the
// snippet's listeners; one capture pipeline from here on.
queue.push(...(window.__IDYLL_Q?.splice(0) ?? []));
window.__IDYLL_Q_OFF?.();

function delegate(type) {
  if (delegated.has(type)) return;
  delegated.add(type);
  // Capture phase so even stopPropagation'd bubbles are seen; snapshot the interesting
  // fields now — by replay time an input's value may have moved on.
  document.addEventListener(type, (e) => {
    const rec = {
      target: e.target,
      type,
      targetValue: 'value' in e.target ? String(e.target.value) : undefined,
      key: e.key,
    };
    if (live) deliver(rec);
    else queue.push(rec);
  }, true);
}
for (const type of DELEGATED) delegate(type);

/** A wasm trap (guest panic) throws through jco and the instance is unreliable after —
 * there is deliberately NO recovery machinery (guests are panic=abort; prevention is
 * the policy). This makes the failure loud and attributable instead of a wedged page. */
function loud(live, what, run) {
  try {
    return run();
  } catch (err) {
    console.error(`idyll live [${live}] trapped during ${what} — the live is dead until reload`, err);
    return undefined;
  }
}

// ── The style sheet ─────────────────────────────────────────────────────────────────
//
// One constructed stylesheet holds every rule delivered after first paint (a template
// announced by the guest carries names only — its text was provisioned in the
// document's SSR `<style>`, which includes the app's whole style table). Adopted sheets cascade after document sheets
// at equal specificity, so re-adding an SSR-duplicate rule is harmless — identical text
// — and a dev hot-swap (`SHEET.replaceSync`) wins over the stale SSR tag without
// touching it. RULES is fill-if-absent: a rule already delivered (or swapped in as the
// authoritative dev set) is never overwritten by a later, possibly staler, arrival.

/** class name → rule text — the sheet's contents, consulted before every insert. */
const RULES = new Map();
const SHEET = (() => {
  try {
    const sheet = new CSSStyleSheet();
    document.adoptedStyleSheets = [...document.adoptedStyleSheets, sheet];
    return sheet;
  } catch {
    return null; // the jsdom fold harness: RULES still tracks, nothing renders
  }
})();

function injectStyles(rules) {
  for (const rule of rules ?? []) {
    if (rule.css == null || RULES.has(rule.name)) continue;
    RULES.set(rule.name, rule.css);
    try {
      SHEET?.insertRule(rule.css, SHEET.cssRules.length);
    } catch (err) {
      console.error(`idyll: style rule ${rule.name} failed to insert`, err);
    }
  }
}

/** Replace the sheet wholesale (the dev style hot-swap): the new set is authoritative. */
function replaceStyles(rules) {
  RULES.clear();
  for (const rule of rules ?? []) if (rule.css != null) RULES.set(rule.name, rule.css);
  SHEET?.replaceSync([...RULES.values()].join('\n'));
  inkEpoch++; // a swapped sheet may retint the tokens the canvases resolved
}

// ── Canvas painting ─────────────────────────────────────────────────────────────────
//
// A `painting=(…)` binding delivers one frame as a single `paint` command: how many
// layers the picture has, the frame's distinct inks, and — per layer whose shapes moved
// — that layer's changed slots in one flat run whose layout is stated on `paint-cmd` in
// `idyll-host/wit/ssr.wit`. This half owns the element: its backing store, the per-layer
// bitmaps, the clear, and the strokes. Nothing about a canvas reaches the app.
//
// Layers composite in order, layer 0 underneath. A layer that sent no delta this frame
// is blitted from the bitmap it was last stroked to; one that did is stroked straight to
// the element, and only takes a bitmap once it has held still for a frame — a layer that
// changes every frame would pay for a snapshot it never reuses.

/** Floats per shape, and where each of its fields sits in the run. */
const SHAPE = { span: 8, width: 10, alpha: 11, ink: 13, stride: 14 };

/** Canvas element → its 2D context, the size it is backed at, its resolved inks, and one
 * retained picture per layer. */
const canvases = new WeakMap();
/** Bumped whenever the stylesheet changes, so resolved inks are re-read once after. */
let inkEpoch = 0;

/** One layer's retained picture: the whole run slot by slot, each slot's resolved ink,
 * how many slots are live, whether it moved since it was last drawn, and the bitmap it
 * was stroked to while it was standing still — `cached` says whether that bitmap is
 * still what the run says. Only changed slots are written, but every slot is stroked —
 * a canvas has no memory of its own. */
function layer() {
  return { run: null, inkOf: [], len: 0, moved: true, cached: false, bitmap: null, bctx: null };
}

/** Release a layer's backing store. Sizing a bitmap to nothing before dropping it is
 * what hands the memory back now rather than at the next GC — the picture behind a
 * full-stage canvas is megabytes. */
function release(shown) {
  shown.run = null;
  shown.inkOf = [];
  shown.len = 0;
  if (shown.bitmap) [shown.bitmap.width, shown.bitmap.height] = [0, 0];
  shown.bitmap = null;
  shown.bctx = null;
}

/** The canvas's paint state, made on first sight. The element's box is *observed*
 * rather than read per frame: reading a laid-out size during a flush forces a
 * synchronous layout, which is the cost this surface exists to avoid. The observer
 * runs in the rendering steps, so it has not fired by the first paint — which reads
 * the box itself, once, and never again. */
function surface(el) {
  let state = canvases.get(el);
  if (state) return state;
  state = {
    ctx: el.getContext('2d'), css: null, w: 0, h: 0, dpr: 0, inks: new Map(), epoch: 0,
    layers: [],
  };
  state.observer = new ResizeObserver((entries) => {
    const box = entries[entries.length - 1].contentRect;
    if (state.css && state.css[0] === box.width && state.css[1] === box.height) return;
    state.css = [box.width, box.height];
    // Re-sizing the backing store empties it — the element's and every bitmap's — and a
    // layer that has not changed emits no command, so what was up is stroked again at
    // the new size from the runs this side is holding.
    if (state.layers.length) redrawCanvas(el);
  });
  state.observer.observe(el);
  canvases.set(el, state);
  return state;
}

/** Resolve one ink against the canvas element's own cascade — a `var(--token)`
 * included — into the two forms the strokes want: the opaque colour a flat stroke uses
 * as-is, and its channels for the `rgba(…)` stops a fading one builds. `color` is the
 * property that both accepts any CSS colour and computes to a canonical `rgb(…)`, so
 * the element wears the value for one computed read and hands it straight back. */
function resolveInk(el, css) {
  const worn = el.style.color;
  el.style.color = '';
  el.style.color = css;
  const computed = getComputedStyle(el).color;
  el.style.color = worn;
  const parts = computed.match(/[\d.]+/g) ?? ['0', '0', '0'];
  const channels = parts.slice(0, 3).join(',');
  return { solid: `rgb(${channels})`, channels };
}

/** The cubic `(a, b, c, d)` restricted to `[t0, t1]`, on one axis — de Casteljau twice:
 * the piece up to `t1`, then the piece of that from `t0`. Written back into `out` so a
 * frame of thousands of shapes allocates nothing. */
function restrict(a, b, c, d, t0, t1, out, at) {
  const ab = a + (b - a) * t1;
  const bc = b + (c - b) * t1;
  const cd = c + (d - c) * t1;
  const abc = ab + (bc - ab) * t1;
  const bcd = bc + (cd - bc) * t1;
  const end = abc + (bcd - abc) * t1;
  const u = t1 > 0 ? t0 / t1 : 0;
  const p = a + (ab - a) * u;
  const q = ab + (abc - ab) * u;
  const r = abc + (end - abc) * u;
  const pq = p + (q - p) * u;
  const qr = q + (r - q) * u;
  out[at] = pq + (qr - pq) * u;
  out[at + 2] = qr;
  out[at + 4] = r;
  out[at + 6] = end;
}

/** The one stretch being stroked, as `x0 y0 x1 y1 x2 y2 x3 y3` — scratch, refilled per
 * shape, so a frame of thousands allocates nothing. */
const stroked = new Float64Array(8);

/** Apply one frame — each changed layer's moved slots — into the retained pictures, then
 * redraw. `layers` is what the picture now has, so any this side still holds beyond it
 * are released. */
function paintCanvas(el, layers, inks, deltas) {
  const state = surface(el);
  if (!state.ctx) return;

  if (state.epoch !== inkEpoch) {
    state.inks.clear();
    state.epoch = inkEpoch;
  }
  const palette = inks.map((css) => {
    let ink = state.inks.get(css);
    if (!ink) {
      ink = resolveInk(el, css);
      state.inks.set(css, ink);
    }
    return ink;
  });

  for (const dropped of state.layers.splice(layers)) release(dropped);
  while (state.layers.length < layers) state.layers.push(layer());

  const stride = SHAPE.stride + 1;
  for (const delta of deltas) {
    const shown = state.layers[delta.layer];
    if (!shown) continue;
    const need = delta.len * SHAPE.stride;
    if (!shown.run || shown.run.length < need) {
      const grown = new Float32Array(Math.max(need, 64));
      if (shown.run) grown.set(shown.run.subarray(0, Math.min(shown.run.length, need)));
      shown.run = grown;
    }
    const changes = delta.changes;
    for (let c = 0; c < changes.length; c += stride) {
      const slot = changes[c];
      const at = slot * SHAPE.stride;
      for (let k = 0; k < SHAPE.stride; k++) shown.run[at + k] = changes[c + 1 + k];
      shown.inkOf[slot] = palette[changes[c + 1 + SHAPE.ink]];
    }
    shown.len = delta.len;
    shown.moved = true;
    shown.cached = false;
  }
  redrawCanvas(el);
}

/** Stroke one retained picture into `ctx`. A stretch whose two opacities differ fades
 * along itself (the afterglow behind a pulse's hard-cut head); one whose opacities match
 * is flat. */
function strokeLayer(ctx, shown) {
  const run = shown.run;
  if (!run) return;
  ctx.lineCap = 'round';
  for (let i = 0; i < shown.len * SHAPE.stride; i += SHAPE.stride) {
    const head = run[i + SHAPE.alpha + 1];
    const tail = run[i + SHAPE.alpha];
    if (head <= 0 && tail <= 0) continue;
    const t0 = Math.max(0, run[i + SHAPE.span]);
    const t1 = Math.min(1, run[i + SHAPE.span + 1]);
    if (t1 <= t0) continue;
    if (t0 === 0 && t1 === 1) {
      for (let k = 0; k < 8; k++) stroked[k] = run[i + k];
    } else {
      restrict(run[i], run[i + 2], run[i + 4], run[i + 6], t0, t1, stroked, 0);
      restrict(run[i + 1], run[i + 3], run[i + 5], run[i + 7], t0, t1, stroked, 1);
    }
    const ink = shown.inkOf[i / SHAPE.stride];
    if (ink === undefined) continue;
    if (head === tail) {
      ctx.globalAlpha = head;
      ctx.strokeStyle = ink.solid;
    } else {
      ctx.globalAlpha = 1;
      const fade = ctx.createLinearGradient(stroked[0], stroked[1], stroked[6], stroked[7]);
      fade.addColorStop(0, `rgba(${ink.channels},${tail})`);
      fade.addColorStop(1, `rgba(${ink.channels},${head})`);
      ctx.strokeStyle = fade;
    }
    ctx.lineWidth = run[i + SHAPE.width];
    ctx.beginPath();
    ctx.moveTo(stroked[0], stroked[1]);
    ctx.bezierCurveTo(stroked[2], stroked[3], stroked[4], stroked[5], stroked[6], stroked[7]);
    ctx.stroke();
  }
  ctx.globalAlpha = 1;
}

/** Stroke a layer that is standing still onto a bitmap of its own, so the frames after
 * this one are a blit. */
function snapshot(shown, width, height, dpr) {
  if (!shown.bitmap) {
    // A detached `<canvas>` rather than an `OffscreenCanvas`: both are off-document, but
    // only the element gets the same GPU backing the visible canvas has, which is what
    // makes the blit a texture copy instead of a per-frame upload.
    shown.bitmap = document.createElement('canvas');
    [shown.bitmap.width, shown.bitmap.height] = [width, height];
    shown.bctx = shown.bitmap.getContext('2d');
  } else if (shown.bitmap.width !== width || shown.bitmap.height !== height) {
    [shown.bitmap.width, shown.bitmap.height] = [width, height];
  }
  if (!shown.bctx) return;
  shown.bctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  shown.bctx.clearRect(0, 0, width / dpr, height / dpr);
  strokeLayer(shown.bctx, shown);
}

/** Composite the picture: every layer in order, blitted where it has been standing still
 * and stroked where it moved. Sizing the backing store to the element and the device
 * empties it, so everything below is unconditional. */
function redrawCanvas(el) {
  const state = surface(el);
  const ctx = state.ctx;
  if (!ctx) return;
  state.css ??= [el.clientWidth, el.clientHeight];
  const [w, h] = state.css;
  const dpr = window.devicePixelRatio || 1;
  if (state.w !== w || state.h !== h || state.dpr !== dpr) {
    [state.w, state.h, state.dpr] = [w, h, dpr];
    el.width = Math.round(w * dpr);
    el.height = Math.round(h * dpr);
    // Every bitmap is at the old size; the runs are not, so each layer restrokes.
    for (const shown of state.layers) [shown.moved, shown.cached] = [true, false];
  }
  if (w <= 0 || h <= 0) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);

  for (const shown of state.layers) {
    if (shown.len === 0) continue;
    if (shown.moved) {
      // It moved this frame, so its bitmap says nothing: stroke it where it belongs and
      // let the frame after this one decide whether it is worth a snapshot.
      strokeLayer(ctx, shown);
      shown.moved = false;
      continue;
    }
    if (!shown.cached) {
      snapshot(shown, el.width, el.height, dpr);
      shown.cached = shown.bctx != null;
    }
    // Backed in device pixels, drawn at the element's CSS size under the transform the
    // strokes use: it lands pixel for pixel.
    if (shown.cached) ctx.drawImage(shown.bitmap, 0, 0, w, h);
  }
}

// ── The interruptible flush scheduler ───────────────────────────────────────────────
//
// The guest is ONE shared runtime, so a flush drains it globally. Every entry point
// (`mount`/`dispatch`/`deliver`/`unmount`/tick) returns the FIRST slice — the commands
// produced so far plus `done`/`pendingLane`. A live update applies that slice and, if
// the graph has not settled, **pumps** `flush(budget)` to completion, yielding to the
// event loop between slices so the browser paints and queued input is delivered. The
// guest drains its Input lane before Idle, so an urgent click landing mid-pump is
// serviced before a deferred recompute; we reinforce that by yielding eagerly the
// moment only idle work remains. Hydration is the exception: the claim planner needs
// the whole self-contained stream up front, so `boot` drains it fully before applying.

/** Effect-step units granted per `flush` slice — small enough that a large `@for`
 * splice cannot monopolize a frame, large enough that ordinary updates settle in one. */
const FLUSH_BUDGET = 512;
/** Apply slices until this many milliseconds have elapsed, then yield for a frame. */
const FRAME_BUDGET_MS = 5;
/** One global pump at a time: slices are FIFO in the shared guest, so a second pump
 * would only race the first for the same queue. */
let pumping = false;

// A MessageChannel post is the tightest "yield to the event loop then resume" macrotask
// — unlike `setTimeout(0)` it is not clamped to 4ms, and it lets the browser paint and
// deliver queued DOM events before the next slice.
const yieldChannel = new MessageChannel();
let resumePump = null;
yieldChannel.port1.onmessage = () => {
  const resume = resumePump;
  resumePump = null;
  resume?.();
};
function yieldToEventLoop() {
  return new Promise((resume) => {
    resumePump = resume;
    yieldChannel.port2.postMessage(0);
  });
}

/** Apply an entry point's first slice; if the graph has not settled, pump the rest. */
function schedule(live, result) {
  if (!result) return; // a trap already logged by `loud`
  live.applyAll(result.commands);
  if (!result.done) pump(live);
}

/** Drain the shared guest's remaining reactive work, a budgeted slice at a time,
 * yielding to the event loop between slices. Applied through `live` — the guest
 * ignores the ref and handler ids are global, but attribution is NOT fully cosmetic:
 * `mount-root`, `watch-size`, and `measure` resolve against `live`'s own root, so a
 * stream must be applied through the live whose root it describes. */
async function pump(live) {
  if (pumping) return;
  pumping = true;
  try {
    let deadline = performance.now() + FRAME_BUDGET_MS;
    for (;;) {
      const slice = loud(live.name, 'flush', () => live.app.flush(FLUSH_BUDGET));
      if (!slice) return; // trap — the instance is dead
      live.applyAll(slice.commands);
      if (slice.done) return;
      // Yield when the frame budget is spent, or whenever only idle (deferred) work
      // remains — so urgent input queued during this frame is delivered first.
      if (slice.pendingLane === 'idle' || performance.now() >= deadline) {
        await yieldToEventLoop();
        deadline = performance.now() + FRAME_BUDGET_MS;
      }
    }
  } finally {
    pumping = false;
  }
}

/** Collect an entire flush into one command list — for hydration, whose claim planner
 * needs the whole self-contained stream before it can align regions to the SSR DOM. */
function drainFully(live, first) {
  const commands = [...first.commands];
  let done = first.done;
  while (!done) {
    const slice = loud(live.name, 'flush', () => live.app.flush(FLUSH_BUDGET));
    if (!slice) break; // trap
    commands.push(...slice.commands);
    done = slice.done;
  }
  return commands;
}

/** Route one (possibly replayed) event to the nearest ancestor with a handler. */
function deliver(rec) {
  for (let n = rec.target; n; n = n.parentNode) {
    const entry = listeners.get(n)?.get(rec.type);
    if (entry !== undefined) {
      const { live, handler } = entry;
      const slice = loud(live.name, 'dispatch', () =>
        live.app.dispatch(live.ref(), handler, { targetValue: rec.targetValue, key: rec.key })
      );
      schedule(live, slice);
      return;
    }
  }
  // Nothing interactive under the event — fine, most of the page is static.
}

// ── The fold ──────────────────────────────────────────────────────────────────────
//
// Every live mounts into the ONE guest runtime, so node ids, template ids, handler
// ids and fragment anchors live in one global space — and a stream returned by ANY
// export may touch ANY live's nodes (cross-live reactivity through a shared
// store is ordinary same-thread reactivity in the guest). The fold state is shared
// to match; an `Live` holds identity and claim-time state, not maps.

/** Attributes that must also be reflected onto the live DOM property: a dirty input
 * ignores its `value` *attribute*, and checked/disabled/selected live on the property. */
const PROPERTY_MIRROR = new Set(['value', 'checked', 'disabled', 'selected']);

/** The page's shared fold state (see the section comment). */
const FOLD = newFold();

function newFold() {
  return {
    /** Template id → `{ nodes, svg }`: the IR, and the namespace its own nodes are
     * created in. The two travel together — a template's namespace is not something a
     * reader of its nodes could work out. */
    templates: [],
    /** node id (u32) → live DOM node — one id space for the whole guest runtime. */
    nodes: new Map(),
    /** Fragment anchor id → { nodes: Node[], detached: bool }. */
    fragments: new Map(),
    /** template ids stashed for fragments mounted while their anchor was unpositioned. */
    pendingAdopt: new Map(),
    /**
     * Anchor node → the namespace its rows must be created in — the host context of
     * that position. Recorded where the anchor is made (build threads it down the IR
     * walk; claim reads the SSR parent, which the HTML parser namespaced correctly),
     * because by mount time the anchor may still be parked off-tree and its eventual
     * parent is unknowable. Keyed by node, so it dies with the anchor.
     */
    nsOf: new WeakMap(),
  };
}

/** A static-paint island the server rendered once and serialized (`data-static`): the
 * client adopts its SSR DOM untouched and never mounts it in the guest, so it holds no
 * guest state. It occupies a slot in `liveIslands` only so instance counting and the
 * disconnection sweep still see it — disposal is a no-op, and it is never a parent (a
 * static island declares no nested live). */
class StaticIsland {
  constructor(root) {
    this.name = root.getAttribute('data-i');
    this.key = root.getAttribute('data-k') ?? null;
    this.root = root;
  }
  ref() {
    return { name: this.name, instance: 0, key: this.key ?? undefined };
  }
  dispose() {}
}

class Live {
  constructor(name, instance, root, fold = FOLD) {
    this.name = name;
    /** The row identity for repeated live (`data-k`) — mount identity is
     * (name, key) when present, (name, instance) when not. */
    this.key = root?.getAttribute?.('data-k') ?? null;
    /** Per-name occurrence index in document order -- the mount identity. Every fold
     * derives it by walking the same tree in the same order. */
    this.instance = instance;
    /** The `<idyll-live>` wrapper element — the claim root. */
    this.root = root;
    /** The fold state this live's streams apply against (shared, normally). */
    this.fold = fold;
    /** Slots of the most recently instantiated template: slot (u32) → live DOM node.
     * Stream-scoped scratch — one stream applies through one Live object. */
    this.scratch = new Map();
    /** Claim mode: fragment mounts adopt the SSR DOM instead of building fresh nodes. */
    this.hydrating = false;
  }

  /** Hydrate: mount in the wasm and fold the (self-contained) initial stream — the root
   * template arrives as the stream's first `replace-template` and triggers the claim.
   *
   * Because the whole stream is available up front, a **planning pass** first computes
   * every region's live-DOM extent (how many SSR siblings its rows span) — root-level
   * and nested alike — so the claim can skip past a region and keep claiming: a region
   * does NOT have to be the last child of its parent. No other framework can do this
   * marker-free; it falls out of the stream being self-contained. */
  /** This live's WIT identity: the `live-ref` every export takes. */
  ref() {
    return { name: this.name, instance: this.instance, key: this.key ?? undefined };
  }

  /** Active tick subscriptions: handler id -> { raf, id, last }. */
  startTicks(handler, intervalMs) {
    this.stopTicks(handler);
    this.ticks ??= new Map();
    const entry = { raf: intervalMs === undefined || intervalMs === null, id: 0, last: performance.now() };
    const fire = () => {
      const now = performance.now();
      const dt = now - entry.last;
      entry.last = now;
      const slice = loud(this.name, 'tick', () =>
        this.app.dispatch(this.ref(), handler, { timestamp: dt })
      );
      schedule(this, slice);
    };
    if (entry.raf) {
      const loop = () => {
        if (!this.ticks?.has(handler)) return;
        fire();
        entry.id = requestAnimationFrame(loop);
      };
      entry.id = requestAnimationFrame(loop);
    } else {
      entry.id = setInterval(fire, intervalMs);
    }
    this.ticks.set(handler, entry);
  }

  /** Subscribe `handler` to navigation intents (`watch-navigation` command): the SPA
   * live owns the page, so interception is document-level. A same-origin plain
   * left-click pushes the history entry — the URL reflects intent immediately, like
   * native navigation — then dispatches the path; `popstate` (history already moved)
   * dispatches the same shape. One message arm in the live covers both. */
  watchNavigation(handler) {
    this.navWatchers ??= [];
    const intend = (path) => {
      const slice = loud(this.name, 'navigation intent', () =>
        this.app.dispatch(this.ref(), handler, { targetValue: path })
      );
      schedule(this, slice);
    };
    const onClick = (e) => {
      if (e.defaultPrevented || e.button !== 0) return;
      if (e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      const link = e.target.closest?.('a[href]');
      if (!link || link.origin !== window.location.origin) return;
      if (link.hasAttribute('download') || (link.target && link.target !== '_self')) return;
      e.preventDefault();
      const path = link.pathname + link.search;
      if (path === window.location.pathname + window.location.search) return;
      history.pushState(null, '', path);
      window.scrollTo(0, 0);
      intend(path);
    };
    const onPop = () => intend(window.location.pathname + window.location.search);
    document.addEventListener('click', onClick);
    window.addEventListener('popstate', onPop);
    this.navWatchers.push(() => {
      document.removeEventListener('click', onClick);
      window.removeEventListener('popstate', onPop);
    });
  }

  /** Subscribe `handler` to the mount root's width (`watch-size` command). The
   * `<idyll-live>` wrapper is `display:contents` — it has no box to measure — so the
   * observed element is the view's own root, which is the box the component was given
   * and the one whose width its layout may legitimately depend on. The root exists by
   * the time this command lands (`mount-root` precedes block mounting), but a view
   * that renders no element is possible, so a missing root simply never reports.
   *
   * Widths are rounded and deduped here: a drag fires a continuous stream of
   * sub-pixel changes, and the guest should be woken for real width changes only. */
  watchSize(handler) {
    const target = this.root?.firstElementChild;
    if (!target) return;
    let last = null;
    const observer = new ResizeObserver((entries) => {
      const width = Math.round(entries[entries.length - 1].contentRect.width);
      if (width === last) return;
      last = width;
      const slice = loud(this.name, 'size', () =>
        this.app.dispatch(this.ref(), handler, { targetValue: String(width) })
      );
      schedule(this, slice);
    });
    observer.observe(target);
    this.sizeWatchers ??= [];
    this.sizeWatchers.push(() => observer.disconnect());
  }

  /** Observe `el`'s post-layout rectangle (`measure=>` binding) and deliver it
   * **relative to the mount root** — the frame the component composes in, so an SVG
   * overlay over the root aligns to measured HTML without restating layout. Fires on
   * observe (the initial measurement) and on any size change; a window resize is watched
   * too, since a reflow can move the box without changing its size. Rects are integer-
   * rounded and deduped, so a drag wakes the guest only on a real change. */
  watchMeasure(el, handler) {
    let last = null;
    const measure = () => {
      const root = this.root?.firstElementChild;
      const r = el.getBoundingClientRect();
      const base = root ? root.getBoundingClientRect() : { left: 0, top: 0 };
      const rect = {
        x: Math.round(r.left - base.left),
        y: Math.round(r.top - base.top),
        width: Math.round(r.width),
        height: Math.round(r.height),
      };
      const key = `${rect.x},${rect.y},${rect.width},${rect.height}`;
      if (key === last) return;
      last = key;
      const slice = loud(this.name, 'measure', () =>
        this.app.dispatch(this.ref(), handler, { rect })
      );
      schedule(this, slice);
    };
    const observer = new ResizeObserver(() => measure());
    observer.observe(el);
    const onResize = () => measure();
    window.addEventListener('resize', onResize);
    // Also re-run when an island batch reflows the page (position-only moves a
    // ResizeObserver never sees) — see `scheduleRemeasure`.
    measures.add(measure);
    let done = false;
    const cleanup = () => {
      if (done) return;
      done = true;
      observer.disconnect();
      window.removeEventListener('resize', onResize);
      measures.delete(measure);
      this.measureByEl?.delete(el);
    };
    this.measureByEl ??= new Map();
    this.measureByEl.set(el, cleanup);
    this.measureWatchers ??= [];
    this.measureWatchers.push(cleanup);
  }

  stopTicks(handler) {
    const entry = this.ticks?.get(handler);
    if (!entry) return;
    this.ticks.delete(handler);
    if (entry.raf) cancelAnimationFrame(entry.id);
    else clearInterval(entry.id);
  }

  /** Tear the live down (a splice removed its wrapper): stop every timer, then
   * have the guest unmount the root — its tasks cancel, its cells dispose — and fold
   * the stream that produces (free-nodes, plus whatever store-sharing neighbours
   * emit in the same turn). */
  dispose() {
    for (const handler of [...(this.ticks?.keys() ?? [])]) this.stopTicks(handler);
    for (const off of this.navWatchers?.splice(0) ?? []) off();
    for (const off of this.sizeWatchers?.splice(0) ?? []) off();
    for (const off of this.measureWatchers?.splice(0) ?? []) off();
    if (!this.app) return;
    const slice = loud(this.name, 'unmount', () => this.app.unmount(this.ref()));
    schedule(this, slice);
  }

  /** Mount this live in the guest. `parent` is the enclosing live's ref (by
   * wrapper nesting) — the guest parents this mount's context scope to it, which is
   * how a store-root's provides reach the live inside it. */
  async boot(seed, claim = true, parent = null) {
    try {
      this.app = await loadApp();
      // Load-then-mount: the live's chunks must be linked into the live instance
      // before anything can call into them (an unlinked chunk's table slots trap).
      await ensureChunks(this.name);
    } catch (err) {
      recoverSkew(`live [${this.name}] could not load the app module`, err);
      return;
    }
    let result;
    try {
      result = this.app.mount(this.ref(), parent, seed, true);
    } catch (err) {
      // mount is result-typed: a typed decline (unknown name, undecodable seed)
      // arrives as a throw carrying the error payload, and leaves the instance
      // healthy for its neighbours; a genuine trap also lands here. Either way this
      // live ships inert and the reason is loud.
      console.error(
        `idyll live [${this.name}] mount failed — the live stays inert`,
        err?.payload ?? err
      );
      return;
    }
    // Mount returns a mount-result: the first flush slice plus a static-paint verdict the
    // server already acted on (a static island never reaches here — see `mountIslands`).
    // The claim planner needs the whole self-contained stream to align regions to the SSR
    // DOM, so drain the flush before applying.
    const commands = drainFully(this, result.flush);
    this.claimPlan = planRegions(commands, this.fold);
    // First load claims the server's DOM; a live spliced in later (its marker
    // arrived as content, no paint) builds fresh into the empty wrapper.
    this.hydrating = claim;
    this.applyAll(commands);
    this.hydrating = false;
    this.fold.pendingAdopt.clear();
  }

  applyAll(commands) {
    let wrappers = false;
    let moved = false;
    for (const c of commands) {
      this.apply(c);
      wrappers ||= c.tag === 'mount-fragment' || c.tag === 'replace-fragment';
      moved ||= MOVES_LAYOUT.has(c.tag);
    }
    if (wrappers) scheduleIslandReconcile();
    if (moved) scheduleRemeasure();
  }

  /** Fold one typed command (a jco variant: `{ tag, val }`) into the document. */
  /** The stream introduces every consumed id before use (the pinned contract in
   * `examples/todo/app/tests/command_stream.rs`). On the build path an unknown id is
   * an emitter bug, and folding past it would paint a wrong document — so the fold
   * stops loud instead (never silently misclaim). During a hydration claim the map
   * is still being matched against served DOM, so a miss there belongs to the claim
   * machinery (probe retirement, skew reload), not to this assertion — the caller
   * skips the op. */
  mustNode(id, tag) {
    const node = this.fold.nodes.get(id);
    if (node === undefined && !this.hydrating) {
      throw new Error(`${tag}: node ${id} was never introduced by the stream`);
    }
    return node;
  }

  apply(c) {
    const v = c.val;
    switch (c.tag) {
      case 'replace-template':
        this.fold.templates[v.templateId] = { nodes: v.nodes, svg: !!v.svg };
        injectStyles(v.styles);
        break;
      case 'mount-root': {
        // The stream's explicit root announcement: claim the SSR DOM against the
        // named template (or build it fresh when there is nothing to claim). Never
        // inferred — templates are content-addressed, so a remount's root is
        // already registered and re-emits no replace-template.
        if (this.rootMounted) break; // parity with the server fold's root_materialized guard
        this.rootMounted = true;
        const nodes = this.mustTemplate(v, 'mount-root').nodes;
        if (this.hydrating) this.claimRoot(nodes);
        else for (const n of this.buildInto(nodes, childNsOf(this.root))) this.root.appendChild(n);
        break;
      }
      case 'bind-slot': {
        const node = this.scratch.get(v.slot);
        if (node === undefined) {
          // A claim miss belongs to the claim machinery (skipped subtrees leave
          // slots unresolved); a build-path miss is an emitter bug — a bind against
          // a stale slot scope — and dropping it would only misblame the first
          // consuming op later.
          if (!this.hydrating) {
            throw new Error(`bind-slot: slot ${v.slot} is not in the current template's scope`);
          }
          break;
        }
        this.fold.nodes.set(v.node, node);
        break;
      }
      case 'set-text': {
        const bound = this.mustNode(v.node, 'set-text');
        if (!bound) break;
        if (bound.__pendingText) {
          // A slot claimed inside a merged SSR text node: now that the value (and so
          // its length) is known, split the node and isolate the slot's own text node.
          // SSR text equals this value by construction (same fold), so the split is
          // deterministic; runs resolve left-to-right because SetTexts arrive in
          // document order.
          const textNode = resolvePendingText(bound, v.text);
          this.fold.nodes.set(v.node, textNode);
          textNode.textContent = v.text;
          break;
        }
        bound.textContent = v.text;
        break;
      }
      case 'set-attr': {
        const el = this.mustNode(v.node, 'set-attr');
        if (!el) break;
        el.setAttribute(v.name, v.value);
        if (PROPERTY_MIRROR.has(v.name)) el[v.name] = v.value;
        break;
      }
      case 'set-style-prop': {
        // One CSS declaration via the CSSOM — the untouched declarations are not
        // reparsed (the whole point vs. rewriting the style attribute). Empty value
        // clears the property.
        const el = this.mustNode(v.node, 'set-style-prop');
        if (!el) break;
        if (v.value === '') el.style.removeProperty(v.name);
        else el.style.setProperty(v.name, v.value);
        break;
      }
      case 'remove-attr': {
        const el = this.mustNode(v.node, 'remove-attr');
        if (!el) break;
        el.removeAttribute(v.name);
        if (PROPERTY_MIRROR.has(v.name)) el[v.name] = v.name === 'value' ? '' : false;
        break;
      }
      case 'set-bool-attr': {
        const el = this.mustNode(v.node, 'set-bool-attr');
        if (!el) break;
        if (v.value) el.setAttribute(v.name, '');
        else el.removeAttribute(v.name);
        if (v.name in el) el[v.name] = v.value; // checked/disabled/… live on the property
        break;
      }
      case 'mount-fragment':
        this.mountFragment(v.anchor, v.template, false);
        break;
      case 'replace-fragment':
        this.mountFragment(v.anchor, v.template, true);
        break;
      case 'remove-fragment': {
        const anchor = this.mustNode(c.val, 'remove-fragment');
        this.removeFragmentNodes(c.val);
        anchor?.parentNode?.removeChild(anchor);
        this.fold.nodes.delete(c.val);
        this.fold.fragments.delete(c.val);
        break;
      }
      case 'detach-fragment': {
        this.mustNode(c.val, 'detach-fragment');
        const f = this.fold.fragments.get(c.val);
        if (!f || f.detached) break;
        for (const n of f.nodes) n.parentNode?.removeChild(n);
        f.detached = true;
        break;
      }
      case 'attach-fragment': {
        const anchor = this.mustNode(c.val, 'attach-fragment');
        const f = this.fold.fragments.get(c.val);
        if (!anchor || !f || !f.detached) break;
        insertAfter(anchor, f.nodes);
        f.detached = false;
        break;
      }
      case 'move-fragment':
        this.moveFragment(v.anchor, v.after);
        break;
      case 'add-event-listener': {
        const el = this.mustNode(v.node, 'add-event-listener');
        if (!el) break;
        if (!listeners.has(el)) listeners.set(el, new Map());
        listeners.get(el).set(v.eventType, { live: this, handler: v.handler });
        delegate(v.eventType); // the app may use a type outside the preinstalled set
        break;
      }
      case 'watch-measure': {
        // Not a DOM event: a ResizeObserver delivering the element's root-relative
        // rect — its own command on the wire, so nothing here matches event names.
        const el = this.mustNode(v.node, 'watch-measure');
        if (!el) break;
        this.watchMeasure(el, v.handler);
        break;
      }
      case 'unwatch-measure': {
        const el = this.fold.nodes.get(v.node);
        this.measureByEl?.get(el)?.();
        break;
      }
      case 'paint': {
        // The slots that moved, per layer. This side keeps the pictures and owns the
        // element, its device-pixel scaling, the per-layer bitmaps, the clear and the
        // strokes.
        const el = this.mustNode(v.node, 'paint');
        if (!el) break;
        paintCanvas(el, v.layers, v.inks, v.deltas);
        break;
      }
      case 'remove-event-listener': {
        const el = this.fold.nodes.get(v.node);
        listeners.get(el)?.delete(v.eventType);
        break;
      }
      case 'server-request':
        // A mutation the live's loop fired: run the fetch and hand the response
        // back through `deliver` — it resolves to a message in the loop's inbox.
        this.serverRequest(v);
        break;
      case 'navigate':
        // A navigation's route re-execution: fetch the data, deliver the payload —
        // it replays into the store (history moved where the intent was raised).
        this.navigateRequest(v);
        break;
      case 'watch-navigation':
        this.watchNavigation(c.val);
        break;
      case 'watch-size':
        // The measurement subscription's wire half: the mount root's width rides the
        // ordinary dispatch channel (an event whose target-value is the number), so a
        // reflow replays from the message log like every other input.
        this.watchSize(c.val);
        break;
      case 'start-ticks':
        // The Elm-style time subscription's wire half: deliver ticks to the handler
        // until stop-ticks. Ticks ride the ordinary dispatch channel (an event whose
        // timestamp is the delta), so backpressure is natural and recording is free.
        this.startTicks(v.handler, v.intervalMs);
        break;
      case 'stop-ticks':
        this.stopTicks(c.val);
        break;
      case 'free-nodes':
        // A view instance was torn down: drop its id → node entries, listener
        // registrations, fragment records, and measure/canvas observers, so removed
        // rows pin nothing — not detached DOM, not the fragment map the removal path
        // scans, not a ResizeObserver, not a canvas's retained picture.
        for (const id of c.val) {
          const node = this.fold.nodes.get(id);
          if (node !== undefined) {
            listeners.delete(node);
            this.measureByEl?.get(node)?.();
            const canvas = canvases.get(node);
            if (canvas) {
              // A canvas holds the two biggest things a torn-down node can pin: per
              // layer, the retained run (a float per number per shape, for every shape
              // the card ever showed) and the bitmap it was last stroked to (the whole
              // stage in device pixels). Both go here rather than being left for the
              // weak map to notice.
              canvas.observer.disconnect();
              for (const shown of canvas.layers) release(shown);
              canvas.layers = [];
              canvas.inks.clear();
              canvases.delete(node);
            }
          }
          this.fold.nodes.delete(id);
          this.fold.fragments.delete(id);
        }
        break;
      default:
        console.error(`idyll runtime [${this.name}]: unknown command`, c);
    }
  }

  /** POST a `server-request` command's args to its persisted mutation's endpoint (the
   * OpHash's two u64 words as 32-hex) and fold the commands the wasm emits on delivery.
   * Failures are delivered too (as `err`) — the loop hears about them as data; nothing
   * is silently dropped. */
  async serverRequest({ request, msb, lsb, args }) {
    const hash =
      msb.toString(16).padStart(16, '0') + lsb.toString(16).padStart(16, '0');
    let response;
    try {
      const res = await fetch(`/__idyll/m/${hash}`, {
        method: 'POST',
        // The page identity rides along so the server can bundle a route-refresh
        // seed into the response envelope (the store-consistency round trip).
        headers: {
          'content-type': 'application/json',
          'x-idyll-path': window.location.pathname + window.location.search,
        },
        body: args,
      });
      response = res.ok
        ? { tag: 'ok', val: new Uint8Array(await res.arrayBuffer()) }
        : { tag: 'err', val: { tag: 'http', val: { status: res.status, body: await res.text() } } };
    } catch (err) {
      response = { tag: 'err', val: { tag: 'transport', val: String(err) } };
    }
    const slice = loud(this.name, 'deliver', () =>
      this.app.deliver(this.ref(), request, response)
    );
    schedule(this, slice);
  }

  /** GET the route query for a `navigate` command and deliver the `Preloaded`
   * payload — it replays into the context store, and the live's route projection
   * swaps. A path the route refuses (HTTP 404 — the typed absence) falls back to a
   * document navigation: the server's answer for that path IS the next document.
   * Transport failures deliver as `err` and fault the live. */
  async navigateRequest({ request, msb, lsb, path }) {
    const hash =
      msb.toString(16).padStart(16, '0') + lsb.toString(16).padStart(16, '0');
    let response;
    try {
      const vars = encodeURIComponent(JSON.stringify({ request: { path } }));
      const res = await fetch(`/__idyll/q/${hash}?vars=${vars}`);
      if (!res.ok) {
        window.location.href = path;
        return;
      }
      response = { tag: 'ok', val: new Uint8Array(await res.arrayBuffer()) };
    } catch (err) {
      response = { tag: 'err', val: { tag: 'transport', val: String(err) } };
    }
    const slice = loud(this.name, 'deliver', () =>
      this.app.deliver(this.ref(), request, response)
    );
    schedule(this, slice);
  }

  // ── Structure: fragments and anchors ────────────────────────────────────────

  /** The comment node standing for a fragment anchor, created floating on first use. */
  ensureAnchor(id) {
    let anchor = this.fold.nodes.get(id);
    if (!anchor) {
      anchor = document.createComment('idyll');
      this.fold.nodes.set(id, anchor);
    }
    return anchor;
  }

  /**
   * The namespace rows mounted at `anchor` must be built in.
   *
   * Normally the anchor recorded it when it was made. An anchor minted by
   * [ensureAnchor] never went through a walk, so it has none — fall back to where it
   * currently sits, which is right whenever it sits anywhere at all.
   */
  /**
   * Build template `templateId`'s nodes for a mount at `anchor`.
   *
   * Two sources, and both are needed. The template says the namespace it was *written*
   * in — the only way to know for a fragment, whose row is built before its anchor is
   * positioned. The anchor says the namespace it is *mounted* into — the only way to
   * know for a template written in HTML and mounted inside an `<svg>`, which the
   * compiler cannot see from the template alone. Neither subsumes the other, so a
   * declared SVG template is SVG anywhere, and anything else inherits its position.
   */
  /** The registered template a structural command names — templates travel in-band
   * before anything instantiates them (stream contract 1), so a miss is an emitter
   * bug and an empty paint would be the silent-misclaim outcome this fold refuses. */
  mustTemplate(templateId, tag) {
    const tpl = this.fold.templates[templateId];
    if (tpl === undefined) {
      throw new Error(`${tag}: template ${templateId} was never registered`);
    }
    return tpl;
  }

  buildTemplate(templateId, anchor) {
    const tpl = this.mustTemplate(templateId, 'build-template');
    const ns = tpl.svg ? SVG_NS : this.nsAt(anchor);
    return this.buildInto(tpl.nodes, ns);
  }

  nsAt(anchor) {
    const recorded = this.fold.nsOf.get(anchor);
    return recorded !== undefined ? recorded : childNsOf(anchor.parentNode);
  }

  mountFragment(anchorId, templateId, replace) {
    // Claim mode, first fill: the server already rendered these nodes — adopt, don't
    // build. An anchor still floating (no position yet) gets its rows at `move-fragment`.
    if (this.hydrating && !replace && !this.fold.fragments.has(anchorId)) {
      const anchor = this.fold.nodes.get(anchorId);
      if (anchor?.parentNode) this.adoptFragment(anchorId, templateId);
      else this.fold.pendingAdopt.set(anchorId, templateId);
      this.ensureAnchor(anchorId);
      return;
    }
    if (replace) this.removeFragmentNodes(anchorId);
    const anchor = this.ensureAnchor(anchorId);
    if (!anchor.parentNode) this.root.appendChild(anchor); // floating: parked until moved
    const kids = this.buildTemplate(templateId, anchor);
    insertAfter(anchor, kids);
    this.fold.fragments.set(anchorId, { nodes: kids, detached: false });
  }

  removeFragmentNodes(anchorId) {
    const f = this.fold.fragments.get(anchorId);
    if (!f) return;
    // A nested fragment (a child component, `@for`, or `@if`) splices its own nodes *beside*
    // its anchor (`insertAfter`), so when that anchor is one of our top-level nodes the nested
    // DOM is a sibling, not a descendant — removing our nodes leaves it orphaned. Tear those
    // down first. (An anchor nested under one of our elements goes with that element, so it is
    // not handled here.)
    for (const [childId, cf] of this.fold.fragments) {
      if (childId === anchorId || cf.detached) continue;
      const childAnchor = this.fold.nodes.get(childId);
      if (childAnchor && f.nodes.includes(childAnchor)) this.removeFragmentNodes(childId);
    }
    for (const n of f.nodes) n.parentNode?.removeChild(n);
    f.detached = true;
  }

  moveFragment(anchorId, afterId) {
    const anchor = this.ensureAnchor(anchorId);
    // The after-anchor is a reference, never an introduction (contract 3).
    this.mustNode(afterId, 'move-fragment.after');
    const tail = this.fragmentTail(afterId);
    if (tail?.parentNode) {
      tail.parentNode.insertBefore(anchor, tail.nextSibling);
      const f = this.fold.fragments.get(anchorId);
      if (f && !f.detached) insertAfter(anchor, f.nodes);
    }
    // Now positioned: adopt any rows that were waiting on a floating anchor.
    const pending = this.fold.pendingAdopt.get(anchorId);
    if (pending !== undefined && this.hydrating) {
      this.fold.pendingAdopt.delete(anchorId);
      this.adoptFragment(anchorId, pending);
    }
  }

  /** The last live node of a fragment (or the anchor itself when it has no rows). */
  fragmentTail(anchorId) {
    const f = this.fold.fragments.get(anchorId);
    if (f && !f.detached && f.nodes.length > 0) return f.nodes[f.nodes.length - 1];
    return this.fold.nodes.get(anchorId);
  }

  // ── Build mode: materialize DOM straight from the IR ────────────────────────

  /**
   * Build a template's top-level nodes and refresh `scratch` from its slots. `ns` is
   * the namespace the caller's insertion point imposes — see [childNsOf].
   */
  buildInto(ir, ns) {
    this.scratch = new Map();
    const cursor = { i: 0 };
    const nodes = [];
    while (cursor.i < ir.length) nodes.push(this.buildNode(ir, cursor, ns));
    return nodes;
  }

  buildNode(ir, cursor, ns) {
    const node = ir[cursor.i++];
    switch (node.tag) {
      case 'text':
        return document.createTextNode(node.val);
      case 'text-slot': {
        const t = document.createTextNode('');
        this.scratch.set(node.val, t);
        return t;
      }
      case 'anchor-slot': {
        const anchor = document.createComment('idyll');
        this.fold.nsOf.set(anchor, ns);
        this.scratch.set(node.val, anchor);
        return anchor;
      }
      case 'element': {
        const v = node.val;
        // Namespace *within* this template: an `svg` subtree builds in the SVG
        // namespace, `foreignObject` re-enters HTML. Where the template itself sits
        // is not knowable from here and arrives with it (see [buildTemplate]). Plain
        // createElement would mint dead HTMLUnknownElements — right tag, right
        // attributes, no rendering — and the SSR parser namespaces these correctly,
        // so build mode must match or claim and build disagree.
        const elNs = v.tag === 'svg' ? SVG_NS : ns;
        const childNs = v.tag === 'foreignObject' ? undefined : elNs;
        const el = elNs
          ? document.createElementNS(elNs, v.tag)
          : document.createElement(v.tag);
        for (const attr of v.attrs) el.setAttribute(attr.name, attr.value);
        if (v.slot !== undefined && v.slot !== null) this.scratch.set(v.slot, el);
        for (let k = 0; k < v.children; k++) el.appendChild(this.buildNode(ir, cursor, childNs));
        return el;
      }
      case 'live': {
        // The live boundary's DOM edge format (identity lives in the IR). The
        // `data-i` attribute is what live discovery scans for.
        const v = node.val;
        const el = document.createElement('idyll-live');
        el.setAttribute('data-i', v.name);
        if (v.key != null) el.setAttribute('data-k', v.key);
        el.setAttribute('style', 'display:contents');
        // The fallback stands in the wrapper until this live's own mount paints over
        // it — the same rule the server fold applies to the same marker.
        for (let k = 0; k < v.fallback; k++) el.appendChild(this.buildNode(ir, cursor, ns));
        return el;
      }
    }
  }

  // ── Claim mode: adopt the SSR DOM by tandem-walking the IR against it ───────
  //
  // The served HTML is clean (no scaffolding of any kind); the IR *is* the expected
  // structure, so the walk lines them up 1:1. Control-flow regions sit anywhere in a
  // parent (the claim plan knows every region's extent, nested ones included); the
  // last-dynamic-child contract survives only as the fallback for an unplanned anchor.

  /** Claim the live root: walk `ir` against the live wrapper's children, with the
   * planned extents of the root template's regions. */
  claimRoot(ir) {
    this.scratch = new Map();
    const cursor = { i: 0 };
    this.claimChildren(ir, cursor, ir.length, this.root, this.root.firstChild, this.claimPlan.root);
  }

  /** Claim up to `count` IR nodes against siblings starting at `real`; returns the next
   * unclaimed sibling.
   *
   * Text handling: a maximal run of text-ish IR nodes claims ONE merged SSR text node
   * with a moving offset. Statics advance the offset eagerly; a text slot binds a
   * pending marker resolved at its `SetText` (when the value's length is known and the
   * node can be split precisely). */
  claimChildren(ir, cursor, count, realParent, real, extents) {
    let run = null; // active merged-text claim on the current live text node
    const endRun = () => {
      if (!run) return;
      real = run.node.nextSibling;
      run = null;
    };
    for (let k = 0; k < count && cursor.i < ir.length; k++) {
      const node = ir[cursor.i++];
      if (node.tag === 'text' || node.tag === 'text-slot') {
        if (!run) {
          if (real?.nodeType === Node.TEXT_NODE) {
            run = { node: real, offset: 0, blocked: false, queue: [] };
          } else if (node.tag === 'text-slot') {
            // Empty SSR text produced no node — materialize one to claim.
            const t = document.createTextNode('');
            realParent.insertBefore(t, real ?? null);
            this.scratch.set(node.val, t);
            continue;
          } else {
            continue; // static text with no live counterpart — nothing to claim
          }
        }
        if (node.tag === 'text') {
          if (run.blocked) run.queue.push({ type: 'static', text: node.val });
          else run.offset += node.val.length; // SSR is our own serialization: trusted
        } else {
          run.blocked = true;
          run.queue.push({ type: 'slot', slot: node.val });
          this.scratch.set(node.val, { __pendingText: true, run, slot: node.val });
        }
        continue;
      }
      endRun();
      switch (node.tag) {
        case 'anchor-slot': {
          // The IR has an anchor where the live DOM has the expanded rows. Splice a
          // real anchor in; the rows themselves are adopted at fragment mount.
          const anchor = document.createComment('idyll');
          realParent.insertBefore(anchor, real ?? null);
          this.fold.nsOf.set(anchor, childNsOf(realParent));
          this.scratch.set(node.val, anchor);
          const extent = extents?.get(node.val);
          if (extent === undefined) {
            // No plan info (row-scope region): the legacy contract applies — the
            // region must be the last dynamic child of its parent.
            return real;
          }
          // Skip exactly the region's SSR nodes and keep claiming after it.
          for (let s = 0; s < extent && real; s++) real = real.nextSibling;
          break;
        }
        case 'element': {
          const v = node.val;
          if (!real) {
            skipSubtree(ir, cursor, v.children);
            break;
          }
          if (v.slot !== undefined && v.slot !== null) this.scratch.set(v.slot, real);
          this.claimChildren(ir, cursor, v.children, real, real.firstChild, extents);
          real = real.nextSibling;
          break;
        }
        case 'live': {
          // Claims its serialized wrapper element; the live's own hydration is a
          // separate mount — this walk only lines the structure up. The fallback is
          // stepped over rather than claimed: what the server put inside the wrapper
          // is the paint where the mount succeeded, and the fallback only where it
          // did not — either way the wrapper is one node to this walk.
          skipSubtree(ir, cursor, node.val.fallback);
          if (real) real = real.nextSibling;
          break;
        }
      }
    }
    endRun();
    return real;
  }

  /** Adopt the server's already-rendered rows for a control-flow fragment: claim the
   * row template's IR against the live siblings following the anchor, with the planned
   * extents of any regions nested in these rows (keyed by this instance's anchor). */
  adoptFragment(anchorId, templateId) {
    const anchor = this.fold.nodes.get(anchorId);
    if (!anchor?.parentNode) return;
    const ir = this.fold.templates[templateId]?.nodes ?? [];
    this.scratch = new Map();
    const cursor = { i: 0 };
    const first = anchor.nextSibling;
    const extents = this.claimPlan?.rows.get(anchorId) ?? null;
    const end = this.claimChildren(ir, cursor, ir.length, anchor.parentNode, first, extents);
    const adopted = [];
    for (let n = first; n && n !== end; n = n.nextSibling) adopted.push(n);
    this.fold.fragments.set(anchorId, { nodes: adopted, detached: false });
  }
}

const SVG_NS = 'http://www.w3.org/2000/svg';

// ── Shared helpers ────────────────────────────────────────────────────────────────

/**
 * The namespace a child of `parent` must be created in.
 *
 * A template built from its own root infers this from its `svg` tag, but a fragment
 * built at runtime starts *inside* an existing tree — its IR begins at whatever tag
 * the `@for` row or `@if` arm opens with, so the tag says nothing. The parent is the
 * only thing that knows. Get it wrong and `createElement('path')` mints an
 * HTMLUnknownElement: it carries the right `d` and the right style, reports itself
 * visible, and paints nothing.
 */
function childNsOf(parent) {
  if (!parent || parent.namespaceURI !== SVG_NS) return undefined;
  return parent.tagName === 'foreignObject' ? undefined : SVG_NS;
}

function insertAfter(anchor, list) {
  const parent = anchor.parentNode;
  let ref = anchor.nextSibling;
  for (const n of list) {
    parent.insertBefore(n, ref);
    ref = n.nextSibling;
  }
}

/** Advance `cursor` past a subtree (`count` direct children, pre-order). Two node
 * kinds carry one: an element's children, and a live's fallback. Mirrors
 * `template::subtree_len` — the two must step identically or the folds desync. */
function skipSubtree(ir, cursor, count) {
  for (let k = 0; k < count && cursor.i < ir.length; k++) {
    const node = ir[cursor.i++];
    if (node.tag === 'element') {
      skipSubtree(ir, cursor, node.val.children);
    } else if (node.tag === 'live') {
      skipSubtree(ir, cursor, node.val.fallback);
    }
  }
}

/** Resolve a pending merged-text claim now that the slot's value is known: consume the
 * run's queued statics, then split the live text node to isolate the slot's own node. */
function resolvePendingText(pending, value) {
  const run = pending.run;
  while (run.queue.length) {
    const item = run.queue.shift();
    if (item.type === 'static') {
      run.offset += item.text.length;
      continue;
    }
    if (item.slot !== pending.slot) {
      console.error('idyll runtime: text run resolved out of order');
    }
    let node = run.node;
    if (run.offset > 0) {
      node = node.splitText(run.offset);
      run.node = node;
      run.offset = 0;
    }
    if (value.length < node.data.length) {
      run.node = node.splitText(value.length);
      run.offset = 0;
    } else {
      run.node = node;
      run.offset = node.data.length;
    }
    return node;
  }
  return run.node; // shouldn't happen; degrade to the whole node
}

/** The **claim plan**: since `mount` returns the whole self-contained stream up front,
 * scan it before applying to compute every region's live-DOM extent — how many pristine
 * SSR siblings its rows span — so the claim can skip past a region and keep claiming.
 * Regions surface two ways: a fragment mounted directly at its template anchor (`@if`
 * and rendered-embeds — `mount-fragment` at the anchor itself), and `move-fragment
 * { after }` chains identifying which row anchors belong to which region head (`@for`).
 * A row's span is its template's top-level node count, where a **nested** top-level
 * region contributes its own recursive extent. Bind-slot commands are attributed to
 * the fragment instance whose mount preceded them (exactly how `apply` resolves them
 * against `scratch`), which maps each instance's anchor slots to anchor node ids.
 *
 * Returns `{ root, rows }`: `root` keys slot id → extent for the root template's
 * anchors; `rows` keys fragment anchor node id → (slot id → extent) for the anchors
 * inside that fragment instance's rows, consumed when the fragment is adopted. */
function planRegions(commands, fold) {
  // Templates named by this stream, falling back to ones already registered in the
  // fold — content-addressing means a template another live's stream carried is
  // not re-announced here.
  const templates = [];
  const templateOf = (tid) => templates[tid] ?? fold?.templates[tid]?.nodes ?? [];
  const rootBinds = new Map(); // root-scope slot id → node id
  let segmentBinds = rootBinds; // binds resolve against the most recent scratch refresh
  const instanceBinds = new Map(); // fragment anchor node id → Map(slot id → node id)
  const anchorTemplate = new Map(); // fragment anchor node id → template id
  const rowsOf = new Map(); // region head node id → [row anchor node id, …]
  const headOfTail = new Map(); // row anchor node id → its region head node id
  for (const c of commands) {
    switch (c.tag) {
      case 'replace-template':
        templates[c.val.templateId] = c.val.nodes;  // planning walks nodes only
        break;
      case 'bind-slot':
        segmentBinds.set(c.val.slot, c.val.node);
        break;
      case 'mount-fragment':
      case 'replace-fragment': {
        anchorTemplate.set(c.val.anchor, c.val.template);
        segmentBinds = new Map();
        instanceBinds.set(c.val.anchor, segmentBinds);
        break;
      }
      case 'move-fragment': {
        const after = c.val.after;
        const head = headOfTail.get(after) ?? after;
        if (!rowsOf.has(head)) rowsOf.set(head, []);
        rowsOf.get(head).push(c.val.anchor);
        headOfTail.set(c.val.anchor, head);
        break;
      }
    }
  }

  /** Pristine-SSR span of one mounted fragment instance's rows. */
  const instanceSpan = (anchorNode, seen) => {
    const tid = anchorTemplate.get(anchorNode);
    if (tid === undefined) return 0;
    return templateSpan(templateOf(tid), instanceBinds.get(anchorNode), seen);
  };

  /** Total extent of the region at `head`: a directly-mounted fragment's rows (`@if`)
   * plus every row anchor chained after it (`@for`). Anchor comments themselves are
   * spliced at claim time, so they contribute nothing to the pristine count. */
  const extentOfRegion = (head, seen) => {
    if (head === undefined || head === null || seen.has(head)) return 0;
    seen.add(head);
    let extent = instanceSpan(head, seen);
    for (const rowAnchor of rowsOf.get(head) ?? []) extent += instanceSpan(rowAnchor, seen);
    return extent;
  };

  /** How many live DOM nodes a template's top level produces when server-rendered.
   * A top-level nested region counts as its own extent (via this instance's anchor
   * binds). Known limit (fail loud, never silently misclaim): text at a row **edge**
   * merges with a neighbouring row's text in SSR. */
  const templateSpan = (ir, binds, seen) => {
    const nodes = [];
    const cursor = { i: 0 };
    while (cursor.i < ir.length) {
      const node = ir[cursor.i++];
      if (node.tag === 'element') {
        skipSubtree(ir, cursor, node.val.children);
      } else if (node.tag === 'live') {
        skipSubtree(ir, cursor, node.val.fallback);
      }
      nodes.push(node);
    }
    let span = 0;
    for (const [index, node] of nodes.entries()) {
      if (node.tag === 'anchor-slot') {
        const anchorNode = binds?.get(node.val);
        if (anchorNode === undefined) {
          console.error(
            'idyll runtime: nested region has no bound anchor — its extent is unknown'
          );
          continue;
        }
        span += extentOfRegion(anchorNode, seen);
        continue; // the anchor comment itself does not exist in pristine SSR
      }
      if (
        (node.tag === 'text' || node.tag === 'text-slot') &&
        (index === 0 || index === nodes.length - 1)
      ) {
        console.error(
          'idyll runtime: text at a row edge merges with neighbouring rows in SSR — wrap it in an element'
        );
      }
      span += 1;
    }
    return span;
  };

  const root = new Map(); // root slot id → live-DOM node count of its region
  for (const [slot, node] of rootBinds) {
    root.set(slot, extentOfRegion(node, new Set()));
  }
  const rows = new Map(); // fragment anchor node id → (slot id → extent)
  for (const [anchorNode, binds] of instanceBinds) {
    const extents = new Map();
    for (const [slot, node] of binds) {
      extents.set(slot, extentOfRegion(node, new Set()));
    }
    rows.set(anchorNode, extents);
  }
  return { root, rows };
}

// ── Live: discovery and mounting ───────────────────────────────────────────────
//
// The page is content: the server rendered it, this runtime never re-renders it, and
// same-origin links are native navigation — the SSR'd MPA behaviour is not a
// fallback, it is the shape. Liveness enters exactly where a marker painted; an app
// that wants SPA behaviour puts a live at the top and drives navigation as data.

/** The current page's seed (the executed route query), consumed by every live
 * mount. */
let currentSeed = null;

/** The current page's live live — disposed (timers stopped) when a splice
 * removes their wrapper. */
let liveIslands = [];

/** Every active `measure=>` re-measure callback, across all live. A `ResizeObserver`
 * catches an element's *size* changes; this catches the *position* shifts it cannot see —
 * an element the layout moved without resizing (a dot that slid down as the column beside
 * it grew). Re-run on a frame once an island batch settles; each callback dedups on the
 * rect it last reported, so a run where nothing moved is a `getBoundingClientRect` and no
 * dispatch. */
const measures = new Set();

/** The commands that can move something without resizing it. Rows arriving, leaving, moving,
 * or being parked off-tree all shift what is under them, and none of it is a size change any
 * `ResizeObserver` reports.
 *
 * Deliberately not every command: these folds write attributes and inline styles every frame,
 * and remeasuring the document on each of those would cost a layout per frame to learn nothing.
 * A fragment is the unit that occupies space, so a fragment is what can displace a neighbour.
 *
 * Distinct from the reconcile question, which only `mount-fragment` and `replace-fragment`
 * answer, because only those can bring a new live wrapper into the tree. Removing the last
 * nine hundred rows of a list introduces no wrapper and reflows everything after it. */
const MOVES_LAYOUT = new Set([
  'mount-fragment',
  'replace-fragment',
  'remove-fragment',
  'detach-fragment',
  'attach-fragment',
  'move-fragment',
]);

let remeasureQueued = false;
function scheduleRemeasure() {
  if (remeasureQueued) return;
  remeasureQueued = true;
  const run = () => {
    remeasureQueued = false;
    for (const measure of measures) measure();
  };
  if (typeof requestAnimationFrame === 'function') requestAnimationFrame(run);
  else queueMicrotask(run);
}

/** Mount every live wrapper under `scope` by its (name, document-order instance)
 * identity — the same counting every fold derives by walking the same tree. Nested
 * wrappers mount after their enclosing live (document order) and pass it as their
 * mount's parent, so its context reaches them.
 *
 * Discovery is a worklist, not one scan: a spliced body carries only its OUTERMOST
 * markers — an enclosing live's wrappers appear when its mount builds its view
 * (SSR paint carries them all up front, but claim and build must converge). A mount
 * inserts wrappers only inside its own wrapper — after itself in document order —
 * so mounting the first unmounted wrapper and rescanning assigns each live the
 * same instance index the fully-expanded tree walk would. */
async function mountIslands(scope, seed, claim) {
  // Live whose wrapper a splice removed are dead: dispose them (guest unmount,
  // timers) before counting, so their identities free up for the new content.
  for (let i = liveIslands.length - 1; i >= 0; i--) {
    if (!liveIslands[i].root.isConnected) {
      liveIslands[i].dispose();
      liveIslands.splice(i, 1);
    }
  }
  for (const el of scope.querySelectorAll('idyll-live[data-i]')) {
    const name = el.getAttribute('data-i');
    if (linkedIslands.has(name)) continue;
    if (el.hasAttribute('data-static') && !el.querySelector('idyll-live[data-i]')) continue;
    for (const url of CHUNKS?.live[name] ?? []) fetchChunk(url).catch(() => {});
  }
  let deadline = performance.now() + FRAME_BUDGET_MS;
  for (;;) {
    const counts = new Map();
    let next = null;
    let instance = 0;
    for (const el of scope.querySelectorAll('idyll-live[data-i]')) {
      const name = el.getAttribute('data-i');
      const n = counts.get(name) ?? 0;
      counts.set(name, n + 1);
      if (next === null && !liveIslands.some((i) => i.root === el)) {
        next = el;
        instance = n;
      }
    }
    if (next === null) {
      // The batch is fully mounted, so the page's layout has settled — re-measure every
      // `measure=>` element against its final position (the wire overlays depend on it).
      scheduleRemeasure();
      return;
    }
    // A static-paint island (`data-static`): the server computed it once and serialized
    // the result, so there is nothing for the client to run — adopt the served DOM as-is,
    // no guest mount, no re-run. The nested-live guard is defence in depth: a static
    // island declares no nested live (the guest disqualifies one that does), so a wrapper
    // with a live descendant is a bug — fall back to a real mount rather than strand it.
    if (next.hasAttribute('data-static') && !next.querySelector('idyll-live[data-i]')) {
      liveIslands.push(new StaticIsland(next));
      continue;
    }
    // A live nested in another live's paint mounts under it — the parent's
    // context (its provided store) reaches it through the membrane's parent chain.
    // A top-level live has no parent: it is a root mount.
    const parentEl = next.parentElement?.closest('idyll-live[data-i]');
    const parent = parentEl
      ? (liveIslands.find((i) => i.root === parentEl)?.ref() ?? null)
      : null;
    const live = new Live(next.getAttribute('data-i'), instance, next);
    liveIslands.push(live);
    await live.boot(seed, claim, parent);
    if (claim && performance.now() >= deadline) {
      await new Promise((resolve) => setTimeout(resolve, 0));
      deadline = performance.now() + FRAME_BUDGET_MS;
    }
  }
}

/** A splice inside a live live (a tracked `@rendered` replacing content) can carry
 * live markers — content the guest built, which the guest cannot mount browser-side.
 * Whoever splices content containing a live owes it a mount: after any structural
 * apply, reconcile the document's wrappers against the live set (dispose the
 * disconnected, mount the new — fresh builds against the current seed, live store
 * inherited through their parent's context). Microtask-debounced; one pass covers a
 * whole flush. */
let reconcileQueued = false;
function scheduleIslandReconcile() {
  if (!live || reconcileQueued || currentSeed === null) return;
  reconcileQueued = true;
  queueMicrotask(() => {
    reconcileQueued = false;
    mountIslands(document, currentSeed, /* claim */ false).catch((err) =>
      console.error('idyll: live reconcile after splice failed', err)
    );
  });
}

// ── Boot ──────────────────────────────────────────────────────────────────────────

async function boot() {
  // The server's paint IS the page; this runtime claims only the live it
  // declared, each adopting its own wrapper's DOM node for node. A document with no
  // seed painted no live — nothing to do.
  const seedValue = window.__IDYLL_SEED__;
  if (seedValue !== undefined) {
    currentSeed = new TextEncoder().encode(JSON.stringify(seedValue));
    await mountIslands(document, currentSeed, /* claim */ true);
  }

  live = true;
  // Replay everything the user did while the wasm was loading, in order.
  for (const rec of queue.splice(0)) deliver(rec);
}

boot();

// ── The dev client (two-stage hot reload) ───────────────────────────────────────────
//
// The watcher broadcasts what a rebuild changed. `styles`: a style value moved —
// fetch the freshly-joined rule set and swap the constructed sheet in place: class
// names are value-free (declaration-site identity), so live DOM — running sims
// included — restyles by cascade alone, and a markup edit in flight can't be
// mis-styled by the swap (identity never carries values). `reload`: the wasm bytes
// actually changed — byte-identity is the gate, never a false alarm — and with every
// view in the component, that covers chrome markup too.
if (window.__IDYLL_DEV__) {
  new EventSource('/__idyll/reload').onmessage = async (e) => {
    if (e.data.startsWith('chunks ')) {
      // The chunk gate, by live NAME (the cross-build identity): a page reloads
      // only when a live it LINKED changed — editing one it never mounted
      // moves nothing.
      const changed = e.data.slice('chunks '.length).split(',').filter(Boolean);
      if (changed.some((name) => linkedIslands.has(name))) {
        location.reload();
      } else {
        console.info(
          `idyll dev: ${changed.length} live chunk(s) changed — none linked here, no reload`
        );
      }
      return;
    }
    if (e.data !== 'styles') {
      location.reload();
      return;
    }
    try {
      const res = await fetch('/__idyll/styles');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      replaceStyles(await res.json());
      console.info('idyll dev: styles swapped in place');
    } catch (err) {
      console.error('idyll dev: style swap failed — reloading', err);
      location.reload();
    }
  };
}

// Inert in the browser (nothing imports this module); the fold harness
// (`crates/idyll-serve/runtime/harness`) drives the same class the page runs.
export { Live, newFold, planRegions, RULES, injectStyles, replaceStyles };
