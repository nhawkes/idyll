# todo — an end-to-end idyll app

A minimal but complete idyll application demonstrating the whole architecture: a query
preloaded host-side, rendered to HTML **inside a wasm sandbox** through the `idyll-host`
membrane, made interactive in the browser, and mutated **on the server** — the browser
never owns state. **Two crates** — the server drives everything:

| Crate | Role |
|-------|------|
| `app` | **UI only**: fragments validated against the published `schema.json`, the `TodoList` query + `AddTodoOp` mutation (persisted operations), the shared route table, the views, the live, and the `idyll:ssr` component exports. No node types, no data source. Built to **one** `wasm32-wasip2` component *on demand* by the server. |
| `server` | The **data plane**: `#[node] Todo`, the `Db`, and the schema + resolvers — `#[root]`/`#[mutation_handler]` emit descriptor **and** typed execution glue from one fn signature each. The framework publishes the contract (`schema.json` + the `ops/` registry), builds `app` to wasm, renders SSR through the membrane, serves the operation/mutation endpoints, and hot-reloads. |

There is **no** `wasm-server`/`wasm-client` crate and **no** separate build step. The server
treats `app` a bit like an interpreted language: it builds it to wasm at startup (and again
on every source change in dev), caching via cargo itself. Config is a typed builder:

```rust
Server::builder()
    .app_crate("todo-app")
    .routes(routes())            // routing + persisted queries + mutations, one table
    .data(Db::seeded())
    .manifest_dir(env!("CARGO_MANIFEST_DIR"))
    .build()
    .serve()
    .await
```

## The membrane contract is WIT

The host↔guest boundary is a **component-model interface** ([`idyll-host/wit/ssr.wit`]),
not a hand-rolled ABI. One world, one artifact, both runtimes:

```wit
world app {
    export render:   func(path: string, seed: list<u8>) -> result<document, string>;
    export mount:    func(live: string, seed: list<u8>) -> list<command>;
    export dispatch: func(live: string, handler: u32, event: dom-event) -> list<command>;
    export deliver:  func(live: string, request: u32, response: result<list<u8>, string>) -> list<command>;
    export navigate: func(path: string, seed: list<u8>) -> result<page-update, string>;
}
```

`wasmtime::component::bindgen!` (host) + `wit_bindgen` (guest) marshal typed data both
ways — no `unsafe`, no pointer packing, no manual memory reads. A render fault is
`result::err`, surfaced as an honest HTTP 500, **never** in-band error HTML. The same
bytes are jco-transpiled for the browser, where the hand-written `runtime.js` drives
`mount`/`dispatch`/`deliver`/`navigate`.

## The flow (and why it's `!Send`-safe)

```
HTTP GET /  ──► server (native, Send)
                 └─ routes.preload(&db, "/").await        // native async fetch
                      └─ Preloaded { seed, roots }         // plain, serializable data
                 └─ serde_json → seed bytes
                 └─ membrane.render(path, seed)            // the Send↔!Send boundary
                      └─ [wasm component] renders → document parts
                 └─ HTML back out ──► response
```

The reactive core is `!Send` (`Rc`/`RefCell`) but never crosses a thread: it lives entirely
inside the wasm instance. Only bytes cross the membrane. The render is synchronous and
epoch-clocked; a runaway render traps and is caught, isolated to its store.

## Mutations: state lives in the server, addressed only by hash

The live keeps no books. `mutation! { AddTodoOp($text: String) = "add-todo"(text:
$text) { text, done } }` is a **persisted operation**: validated against the schema,
canonicalized, content-addressed. Clicking **Add** runs the loop's
`ctx.mutate::<AddTodoOp>(vars, Msg::Added)` — commands out, messages in:

```
click ─► dispatch ─► loop: ctx.mutate(...)   ─► ServerRequest{OpHash, vars} in the stream
runtime.js ─► POST /__idyll/m/<32-hex hash>  ─► boot-validated artifact ─► typed handler
             (#[mutation_handler("add-todo")] async fn add_todo(db: &Db, text: String))
executor masks the returned Todo to the artifact's selection ({text, done})
runtime.js ─► deliver(live, id, response)  ─► Msg::Added(Ok(response)) in the inbox
loop: items.push(row from the SERVER's answer) ─► patch commands ─► DOM
```

The boot-validated registry **is** the allowlist — the client never names anything, and
what an artifact didn't select never crosses back. Refresh the page after adding: the
todo is still there, because the only copy of the list is the server's `Db`.

## Run

```sh
cargo run -p todo-server     # → http://localhost:3001  (or `just todo`)
```

That's it — the server builds the wasip2 component, packages the browser bundle with jco,
loads the membrane, and serves. Edit `app/src/lib.rs` and the page hot-reloads. (The
client packaging needs `jco` on `PATH`.)

## Proof

`cargo test -p todo-app` proves the preload → render → document path natively **and** the
mutation round-trip as a command-stream contract (request out, no premature row, typed
response in, confirmed row mounts). `cargo test -p idyll-host` proves the WIT component
membrane in isolation (typed fault → `Err`, runaway → `BlewBudget`). Running the server
exercises the real wasip2 component end to end.
