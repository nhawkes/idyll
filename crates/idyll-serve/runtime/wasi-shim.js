// wasi-shim.js — minimal, dependency-free browser shim for the WASI Preview 2
// imports of a jco-transpiled component (the app component).
//
// The transpiled JS was generated with:
//   jco transpile <app>.wasm -o . \
//     --map 'wasi:cli/*=./wasi-shim.js' \
//     --map 'wasi:io/*=./wasi-shim.js' \
//     --map 'wasi:random/*=./wasi-shim.js'
// so ALL wasi:cli, wasi:io and wasi:random interfaces resolve to this single
// relative module. The generated code imports exactly these names:
//
//   import { Error as Error$1, InputStream, OutputStream, Pollable,
//            TerminalInput, TerminalOutput, exit, getEnvironment,
//            getStderr, getStdin, getStdout, getTerminalStderr,
//            getTerminalStdin, getTerminalStdout, insecureSeed }
//     from './wasi-shim.js';
//
// The component does no real IO: stdout/stderr are routed to
// console.log/console.error, everything else is inert. Anything the
// component is not observed to call throws a descriptive error so misuse
// is loud instead of silently wrong.

const UNSUPPORTED = (what) => {
  throw new Error(`wasi-shim: ${what} is not implemented (this shim only backs console stdout/stderr)`);
};

// ---------------------------------------------------------------------------
// wasi:io/poll — Pollable + the free-standing `poll` function
// The component only waits on pollables obtained from stream subscribe()s.
// Our streams are always ready, so blocking is a no-op and `poll` reports
// every pollable ready at once.
// ---------------------------------------------------------------------------
export class Pollable {
  block() {}
  ready() { return true; }
}

export function poll(pollables) {
  return pollables.map((_, i) => i);
}

// ---------------------------------------------------------------------------
// wasi:io/error — Error (the io-error *resource*, not a JS Error)
// The transpiled code only needs the class for `instanceof` checks when a
// stream operation fails with `{ tag: 'last-operation-failed', val: <Error> }`.
// Our streams never fail, so instances are never created.
// Exported under the WIT name `Error` (aliased locally to avoid shadowing
// the global Error inside this module).
// ---------------------------------------------------------------------------
class IoError {
  toDebugString() { return 'wasi-shim io error (should never exist)'; }
}
export { IoError as Error };

// ---------------------------------------------------------------------------
// wasi:io/streams — InputStream / OutputStream
// Observed calls from the transpiled code:
//   OutputStream: checkWrite(), write(bytes), blockingFlush(), subscribe()
//   InputStream:  only obtained via getStdin(); never read.
// checkWrite must return a u64 (BigInt); write receives a Uint8Array.
// ---------------------------------------------------------------------------
export class InputStream {
  read(_len) { UNSUPPORTED('InputStream.read (stdin)'); }
  blockingRead(_len) { UNSUPPORTED('InputStream.blockingRead (stdin)'); }
  skip(_len) { UNSUPPORTED('InputStream.skip'); }
  blockingSkip(_len) { UNSUPPORTED('InputStream.blockingSkip'); }
  subscribe() { return new Pollable(); }
}

export class OutputStream {
  /** @param {(line: string) => void} sink e.g. console.log */
  constructor(sink) {
    this.sink = sink;
    this.buf = '';                       // text carried over until a newline / flush
    this.dec = new TextDecoder();        // stream-mode decoder (handles split UTF-8)
  }
  // How many bytes may be written right now. Always plenty.
  checkWrite() { return 1n << 32n; }
  // Accept bytes; emit completed lines to the sink, keep the tail buffered.
  write(bytes) {
    this.buf += this.dec.decode(bytes, { stream: true });
    const lines = this.buf.split('\n');
    this.buf = lines.pop();              // last piece has no newline yet
    for (const line of lines) this.sink(line);
  }
  flush() { this.blockingFlush(); }
  blockingFlush() {
    this.buf += this.dec.decode();       // drain any pending partial UTF-8
    if (this.buf.length > 0) { this.sink(this.buf); this.buf = ''; }
  }
  blockingWriteAndFlush(bytes) { this.write(bytes); this.blockingFlush(); }
  subscribe() { return new Pollable(); }
  splice(_src, _len) { UNSUPPORTED('OutputStream.splice'); }
  blockingSplice(_src, _len) { UNSUPPORTED('OutputStream.blockingSplice'); }
  writeZeroes(_len) { UNSUPPORTED('OutputStream.writeZeroes'); }
  blockingWriteZeroesAndFlush(_len) { UNSUPPORTED('OutputStream.blockingWriteZeroesAndFlush'); }
}

// Singletons: the component may fetch stdout/stderr repeatedly; returning the
// same instance keeps the partial-line buffers coherent across fetches.
const stdin = new InputStream();
const stdout = new OutputStream((line) => console.log(line));
const stderr = new OutputStream((line) => console.error(line));

// ---------------------------------------------------------------------------
// wasi:cli/{stdin,stdout,stderr}
// ---------------------------------------------------------------------------
export function getStdin() { return stdin; }
export function getStdout() { return stdout; }
export function getStderr() { return stderr; }

// ---------------------------------------------------------------------------
// wasi:cli/environment — get-environment: list<tuple<string, string>>
// No environment variables in the browser.
// ---------------------------------------------------------------------------
export function getEnvironment() { return []; }

// ---------------------------------------------------------------------------
// wasi:cli/exit — exit(status: result)
// status arrives as { tag: 'ok' | 'err', val: undefined }. There is no
// process to terminate, so throw; the error propagates out of the wasm call.
// ---------------------------------------------------------------------------
export function exit(status) {
  // Flush anything still buffered so the exit reason isn't swallowed.
  stdout.blockingFlush();
  stderr.blockingFlush();
  throw new Error(`wasi-shim: component called exit(${status.tag})`);
}

// ---------------------------------------------------------------------------
// wasi:cli/terminal-{input,output,stdin,stdout,stderr}
// The getters return option<terminal-*>; `undefined` means "none" (no TTY),
// which is exactly right for a browser. The classes exist only so the
// generated instanceof checks have something to refer to.
// ---------------------------------------------------------------------------
export class TerminalInput {}
export class TerminalOutput {}
export function getTerminalStdin() { return undefined; }
export function getTerminalStdout() { return undefined; }
export function getTerminalStderr() { return undefined; }

// ---------------------------------------------------------------------------
// wasi:random/insecure-seed — insecure-seed: tuple<u64, u64>
//
// The ONE door host randomness enters the guest by: it seeds std's HashMap
// randomization AND idyll's `random_u64` PRNG. The server generates an
// **unguessable seed per request** and injects it into the page bootstrap
// (`window.__IDYLL_RAND`, two u64 as hex `[low, high]`, matching the host's
// `(seed as u64, (seed >> 64) as u64)`) — so the seed is unguessable (defeating
// HashMap-flooding), identical on the server that rendered the page and the browser
// that hydrates it, and **recorded with the page** (so a replay reuses it and every
// draw downstream is deterministic). A page served without a seed (a bare static
// document) falls back to crypto, then to a fixed value.
// ---------------------------------------------------------------------------
export function insecureSeed() {
  const seed = globalThis.__IDYLL_RAND;
  if (Array.isArray(seed) && seed.length === 2) {
    return [BigInt('0x' + seed[0]), BigInt('0x' + seed[1])];
  }
  if (globalThis.crypto?.getRandomValues) {
    const words = crypto.getRandomValues(new BigUint64Array(2));
    return [words[0], words[1]];
  }
  return [0x6b6579310a736565n, 0x646b657932313233n]; // arbitrary fixed seed
}
