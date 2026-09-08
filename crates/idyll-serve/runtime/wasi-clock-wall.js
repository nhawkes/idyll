// wasi-clock-wall.js — browser shim for `wasi:clocks/wall-clock`.
//
// `now` returns a WIT `datetime` record: seconds since the Unix epoch (u64) plus a
// nanoseconds remainder (u32).
//
// Deliberately a **fixed epoch**, not `Date.now()`: the wall clock is pure host
// nondeterminism, and a component's replayable state must never depend on it. Real
// time reaches components only as tick `dt` on messages. The constant is a stable,
// obviously-synthetic instant so a value that escapes into the UI is recognizable.

export function now() {
  return { seconds: 1_700_000_000n, nanoseconds: 0 }; // fixed: 2023-11-14T22:13:20Z
}

export function resolution() {
  return { seconds: 0n, nanoseconds: 1_000_000 }; // 1 ms
}
