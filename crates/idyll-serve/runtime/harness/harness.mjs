// The client-fold harness: drives the REAL runtime.js against jsdom, no browser.
//
// For each fixture `{ name, commands, html }` (a real live's mount stream plus the
// server fold of that exact stream, written by `idyll-serve/tests/client_fold.rs`):
//
// 1. BUILD — apply the stream into an empty wrapper and DOM-compare the result with
//    the parsed server HTML: the two folds of one stream must agree node for node.
// 2. CLAIM — apply the same stream against the server HTML in claim mode: every
//    element and text node of the SSR DOM must survive with identity (adopted, not
//    rebuilt), and a post-claim `set-text` must land on an adopted node.
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

const { Live, newFold, planRegions, RULES, injectStyles } = await import('../runtime.js');

/** A comparable clone: anchor comments are claim-time bookkeeping, not content. */
function withoutComments(node) {
  const clone = node.cloneNode(true);
  const walker = document.createTreeWalker(clone, dom.window.NodeFilter.SHOW_COMMENT);
  const comments = [];
  while (walker.nextNode()) comments.push(walker.currentNode);
  for (const comment of comments) comment.remove();
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
  live.claimPlan = planRegions(commands, live.fold);
  live.hydrating = false;
  live.applyAll(commands);

  const built = withoutComments(wrapper);
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
  const before = [...wrapper.querySelectorAll('*')];

  const live = new Live(name, 0, wrapper, newFold());
  live.claimPlan = planRegions(commands, live.fold);
  live.hydrating = true;
  live.applyAll(commands);
  live.hydrating = false;
  live.fold.pendingAdopt.clear();

  for (const el of before) {
    if (!wrapper.contains(el)) {
      fail(name, 'claim rebuilt instead of adopting', `lost <${el.tagName.toLowerCase()}>: ${el.outerHTML}`);
      return;
    }
  }
  if (withoutComments(wrapper).isEqualNode(parseHtml(html)) === false) {
    fail(name, 'claim changed the document', `after: ${wrapper.innerHTML}`);
  }

  // A post-claim patch must land on an adopted node: retarget every text the
  // stream set and confirm the live DOM followed.
  const texts = commands.filter((c) => c.tag === 'set-text' && c.val.text.length > 0);
  for (const [i, c] of texts.entries()) {
    live.apply({ tag: 'set-text', val: { node: c.val.node, text: `patched-${i}` } });
    if (!wrapper.textContent.includes(`patched-${i}`)) {
      fail(name, 'post-claim patch missed', `set-text on node ${c.val.node} did not reach the document`);
      return;
    }
  }
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
  if (!buildOnly) checkClaim(name, commands, html);
  checkStyles(name, commands);
  if (!failed) console.log(`ok [${name}] ${buildOnly ? 'build converges' : 'build+claim converge'} (${commands.length} commands)`);
}
process.exit(process.exitCode ?? 0);
