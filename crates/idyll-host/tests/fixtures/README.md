# idyll-host test fixtures

`guest.wasm` is a tiny **component** built against `../../wit/ssr.wit` (the `idyll:ssr`
**`app`** world — the app's live table: `live` + `mount`/`dispatch`/`deliver`). It
exists so the membrane's unit tests can exercise every host path without depending on
the full idyll app build. `mount` branches on the **live name** (the typed dispatch
channel — never magic seed bytes; the seed is data, and the paint echoes its size):

| live    | behaviour                                | host path exercised            |
| --------- | ---------------------------------------- | ------------------------------ |
| `paint`   | a canned one-`<p>` stream                | normal mount → `Commands`      |
| `boom`    | panics                                   | trap → `Err`                   |
| `spin`    | infinite loop                            | epoch trap → `BlewBudget`      |
| `decline` | returns the typed `err` arm              | app error → `Failed(message)`  |

`dispatch`/`deliver` are inert stubs — the membrane never calls them, but the world
requires them (one world, one artifact; the browser half is exercised by the real app +
`runtime.js`).

## Regenerating

The source lives in [`guest-src/`](guest-src/) (a standalone crate, not a workspace member):

```sh
cd guest-src
cargo build --release --target wasm32-wasip2
cp target/wasm32-wasip2/release/membrane_fixture_guest.wasm ../guest.wasm
```
