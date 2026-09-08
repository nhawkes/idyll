// wasi-clock-monotonic.js — browser shim for `wasi:clocks/monotonic-clock`.
//
// The app component pulls this in when it embeds tokio (the queue-viz live's
// current-thread runtime reads the monotonic clock during construction and
// bookkeeping). Instants are u64 nanoseconds.
//
// This is a **deterministic virtual clock**: each read advances a counter by a fixed
// quantum rather than sampling `performance.now()`. Real wall time must not leak into
// a component's replayable state — the timeline a component actually runs on is the
// stream of frame messages (each tick carries its own `dt`), never the host clock. A
// strictly-monotonic counter satisfies tokio's construction/bookkeeping reads while
// keeping the guest a pure function of (seed, messages).
//
// `subscribe*` (timer pollables) are deliberately unimplemented: live never block
// on host time, so a component asking the browser to sleep is a bug to surface.

let nanos = 0n;

export function now() {
  nanos += 1000n; // advance 1 µs per read — deterministic and strictly monotonic
  return nanos;
}

now[Symbol.for('cabiLower')] = () => now;

export function resolution() {
  return 1000n; // 1 µs
}

export function subscribeInstant(_when) {
  throw new Error('wasi-clock-monotonic: subscribe-instant is not implemented (no host timers in live)');
}

export function subscribeDuration(_when) {
  throw new Error('wasi-clock-monotonic: subscribe-duration is not implemented (no host timers in live)');
}
