// The client-fold harness: drives the REAL runtime.js against jsdom, no browser.
//
// For each fixture `{ name, commands, html }` (a real live's mount stream plus the
// server fold of that exact stream, written by `idyll-serve/tests/client_fold.rs`):
//
// 1. BUILD — apply the stream into an empty wrapper and DOM-compare the result with
//    the parsed server HTML: the two folds of one stream must agree node for node.
// 2. CLAIM — hydrate the same stream against the server HTML: every element and text
//    node of the SSR DOM must survive with identity (adopted, not rebuilt), the document
//    must be unchanged, and a post-claim `set-text` must land on the node it bound. The
//    same HTML with one node the stream never made must be refused.
//
// Usage: node harness.mjs <fixtures-dir>

import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { JSDOM } from 'jsdom';

const dom = new JSDOM('<!doctype html><html><body></body></html>', { url: 'http://localhost/' });
for (const key of ['window', 'document', 'Node', 'sessionStorage', 'location', 'history']) {
  globalThis[key] = dom.window[key];
}
globalThis.addEventListener = dom.window.addEventListener.bind(dom.window);

const { Live, newFold, RULES, injectStyles, islandsByRoot, unmountedWrappers } =
  await import('../runtime.js');

/** A comparable clone: the fold's anchor comments are its bookkeeping, not content — but
 * a comment the content itself carries (in unescaped markup) stays. */
function withoutAnchors(node, fold) {
  const comments = (root) => {
    const walker = document.createTreeWalker(root, dom.window.NodeFilter.SHOW_COMMENT);
    const found = [];
    while (walker.nextNode()) found.push(walker.currentNode);
    return found;
  };
  const originals = comments(node);
  const clone = node.cloneNode(true);
  comments(clone).forEach((comment, i) => {
    if (fold.anchorIds.has(originals[i])) comment.remove();
  });
  clone.normalize();
  return clone;
}

function parseHtml(html) {
  const container = document.createElement('div');
  container.innerHTML = html;
  container.normalize();
  return container;
}

/**
 * The first structural difference between two trees, as a sentence.
 *
 * `isEqualNode` compares namespace as well as tag, so two trees can serialize to the
 * same string and still differ — an element built with `createElement` inside an
 * `<svg>` is an HTMLUnknownElement that prints as `<path>` and paints nothing. Diffing
 * the serializations would report "no difference" on exactly the bug worth catching,
 * so walk the nodes and name the mismatch.
 */
function firstDifference(a, b, path = 'root') {
  if (a.nodeType !== b.nodeType || a.nodeName !== b.nodeName) {
    // Same tag in two namespaces differs only by `nodeName` case — say so outright
    // rather than leaving a "G vs g" riddle.
    const sameTag = a.localName && a.localName === b.localName;
    const ns = sameTag ? ` — namespace ${a.namespaceURI} vs ${b.namespaceURI}` : '';
    return `${path}: ${a.nodeName} vs ${b.nodeName}${ns}`;
  }
  if (a.nodeType === 1 && a.namespaceURI !== b.namespaceURI) {
    return `${path} <${a.localName}>: namespace ${a.namespaceURI} vs ${b.namespaceURI}`;
  }
  if (a.nodeType === 3 && a.data !== b.data) {
    return `${path}: text ${JSON.stringify(a.data)} vs ${JSON.stringify(b.data)}`;
  }
  if (a.nodeType === 1) {
    const names = new Set([...a.getAttributeNames(), ...b.getAttributeNames()]);
    for (const n of names) {
      if (a.getAttribute(n) !== b.getAttribute(n)) {
        return `${path} <${a.localName}>: @${n} ${a.getAttribute(n)} vs ${b.getAttribute(n)}`;
      }
    }
  }
  if (a.childNodes.length !== b.childNodes.length) {
    return `${path} <${a.nodeName}>: ${a.childNodes.length} children vs ${b.childNodes.length}`;
  }
  for (let i = 0; i < a.childNodes.length; i++) {
    const where = `${path} > ${a.childNodes[i].nodeName.toLowerCase()}[${i}]`;
    const d = firstDifference(a.childNodes[i], b.childNodes[i], where);
    if (d) return d;
  }
  return null;
}

let failed = false;
function fail(fixture, what, detail) {
  console.error(`FAIL [${fixture}] ${what}\n${detail}`);
  failed = true;
  process.exitCode = 1;
}

function checkBuild(name, commands, html) {
  const wrapper = document.createElement('div');
  const live = new Live(name, 0, wrapper, newFold());
  live.applyAll(commands);

  const built = withoutAnchors(wrapper, live.fold);
  const expected = parseHtml(html);
  if (!built.isEqualNode(expected)) {
    fail(
      name,
      'build fold diverges from the server fold',
      `first difference: ${firstDifference(built, expected) ?? '(none found — isEqualNode disagrees)'}\n` +
        `built:    ${built.innerHTML}\nexpected: ${expected.innerHTML}`
    );
  }
}

function checkClaim(name, commands, html) {
  const wrapper = parseHtml(html);
  const walker = document.createTreeWalker(wrapper, dom.window.NodeFilter.SHOW_ELEMENT | dom.window.NodeFilter.SHOW_TEXT);
  const before = [];
  while (walker.nextNode()) before.push(walker.currentNode);

  const live = new Live(name, 0, wrapper, newFold());
  try {
    live.hydrate(commands);
  } catch (err) {
    fail(name, 'claim refused the served DOM', err.stack);
    return;
  }

  for (const el of before) {
    if (!wrapper.contains(el)) {
      fail(name, 'claim rebuilt instead of adopting', `lost ${el.nodeName.toLowerCase()}: ${el.outerHTML ?? JSON.stringify(el.data)}`);
      return;
    }
  }
  if (withoutAnchors(wrapper, live.fold).isEqualNode(parseHtml(html)) === false) {
    fail(name, 'claim changed the document', `after: ${wrapper.innerHTML}`);
  }

  // A post-claim patch must land on the node the claim bound: retarget every text the
  // stream set and confirm the page followed — or, for a text in a branch the stream
  // left detached, the parked node did.
  const texts = commands.filter((c) => c.tag === 'set-text' && c.val.text.length > 0);
  for (const [i, c] of texts.entries()) {
    const text = `patched-${i}`;
    live.apply({ tag: 'set-text', val: { node: c.val.node, text } });
    const node = live.fold.nodes.get(c.val.node);
    const landed = wrapper.contains(node) ? wrapper.textContent.includes(text) : node.data === text;
    if (!landed) {
      fail(name, 'post-claim patch missed', `set-text on node ${c.val.node} did not reach its node`);
      return;
    }
  }
}

/** A served document that is not its stream's must be refused, never adopted: the same
 * stream against the served HTML with one extra node the stream never made. */
function checkClaimRefusesTampering(name, commands, html) {
  const wrapper = parseHtml(html);
  wrapper.append(document.createElement('ins'));
  const live = new Live(name, 0, wrapper, newFold());
  try {
    live.hydrate(commands);
  } catch {
    return;
  }
  fail(name, 'claim adopted a document its stream never made', wrapper.innerHTML);
}

/** Style rules riding the stream's templates land in the runtime's RULES map (the
 * constructed sheet's contents), and the map is fill-if-absent: a later — possibly
 * staler — arrival never overwrites a delivered rule. */
function checkStyles(name, commands) {
  const declared = new Map();
  for (const c of commands) {
    if (c.tag !== 'replace-template') continue;
    for (const rule of c.val.styles ?? []) if (rule.css != null) declared.set(rule.name, rule.css);
  }
  if (declared.size === 0) return;
  for (const [ruleName, css] of declared) {
    if (RULES.get(ruleName) !== css) {
      fail(name, 'a template rule did not reach the sheet', `${ruleName}: ${RULES.get(ruleName)}`);
      return;
    }
  }
  const [first] = declared.keys();
  injectStyles([{ name: first, css: '.stale{}' }]);
  if (RULES.get(first) === '.stale{}') {
    fail(name, 'fill-if-absent violated', `a later arrival overwrote ${first}`);
  }
}

/** A wrapper's life through the fold: built into a fragment it is owed a mount; parked
 * with a detached branch it keeps its island; attached it is owed a mount again only if
 * it never had one; dropped with its fragment its island is disposed. */
function checkWrapperLifecycle() {
  const name = 'wrapper-lifecycle';
  const wrapper = document.createElement('div');
  document.body.append(wrapper);
  const live = new Live(name, 0, wrapper, newFold());
  live.applyAll([
    { tag: 'replace-template', val: { templateId: 0, nodes: [{ tag: 'anchor-slot', val: 0 }], styles: [] } },
    { tag: 'mount-root', val: 0 },
    { tag: 'bind-slot', val: { slot: 0, node: 1 } },
    {
      tag: 'replace-template',
      val: { templateId: 1, nodes: [{ tag: 'live', val: { name: 'inner', key: null, fallback: 0 } }], styles: [] },
    },
    { tag: 'mount-fragment', val: { anchor: 1, template: 1 } },
  ]);
  const built = wrapper.querySelector('idyll-live');
  if (!unmountedWrappers.has(built)) return fail(name, 'a built wrapper is not owed a mount', wrapper.innerHTML);

  let disposed = 0;
  islandsByRoot.set(built, { dispose: () => disposed++ });
  unmountedWrappers.delete(built);
  live.applyAll([{ tag: 'detach-fragment', val: 1 }]);
  if (disposed !== 0 || !islandsByRoot.has(built)) {
    return fail(name, 'detaching a branch disposed the island inside it', `disposed ${disposed}`);
  }
  live.applyAll([{ tag: 'attach-fragment', val: 1 }]);
  if (!built.isConnected || unmountedWrappers.has(built)) {
    return fail(name, 'attaching lost the wrapper or owed a second mount', wrapper.innerHTML);
  }
  live.applyAll([{ tag: 'remove-fragment', val: 1 }]);
  if (disposed !== 1 || islandsByRoot.has(built)) {
    return fail(name, 'removing a branch did not dispose the island inside it', `disposed ${disposed}`);
  }
  wrapper.remove();
  console.log(`ok [${name}] built, parked, attached, dropped`);
}

const dir = process.argv[2];
if (!dir) {
  console.error('usage: node harness.mjs <fixtures-dir>');
  process.exit(2);
}
for (const file of readdirSync(dir).filter((f) => f.endsWith('.json'))) {
  const { name, commands, html, buildOnly } = JSON.parse(readFileSync(join(dir, file), 'utf8'));
  failed = false;
  checkBuild(name, commands, html);
  // A driven stream (mounts plus teardown — detach/attach/move/remove/free) has no
  // SSR document to adopt: its fold is a post-interaction state, so only the build
  // path replays it. That teardown region is exactly where fold divergence has
  // historically lived, which is why it gets a fixture at all.
  if (!buildOnly) {
    checkClaim(name, commands, html);
    checkClaimRefusesTampering(name, commands, html);
  }
  checkStyles(name, commands);
  if (!failed) console.log(`ok [${name}] ${buildOnly ? 'build converges' : 'build+claim converge'} (${commands.length} commands)`);
}
failed = false;
checkWrapperLifecycle();
process.exit(process.exitCode ?? 0);
