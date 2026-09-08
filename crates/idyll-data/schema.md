# Schema-first idyll-data: the persisted-operation architecture

Decided 2026-07-06 (Nathan + design session). Supersedes the "shared contract crate"
sketch; subsumes the name-based mutation endpoint.

## The layering

```
server (native)                          app-server / app-client (wasm containers)
  schema definition                        UI ONLY: views, live, loops,
  (nodes, roots, mutations)                fragments + operations (data needs)
  Db, Fetch/Root resolvers,
  mutation handlers, op executor
        │                                        │
        ▼                                        ▼
   schema.json  ────── validates ──────►  fragment!/query!/mutation! codegen
   (the published contract,                      │
    checked in, diffable)                        ▼
                                    persisted registry (checked in)
                                    ops/<hash>.query, ops/<hash>.mutation
                                    + generated client code per op:
                                      HASH const, typed Vars, projections
```

- **Queries and mutations execute on the server, never in wasm.** The wasm containers
  render from seeds and ask for changes; they resolve nothing. (This is also what makes
  the SSR container edge-deployable: it physically cannot reach a Db.)
- **The schema is defined by the server** and published as `schema.json` — an explicit
  builder (`Schema::new().node::<Todo>().root::<todos>().mutation::<AddTodo>()`), no
  global registries. `#[node]` / `#[root]` / mutation types carry their own descriptors.
- **The client references operations only by hash.** Codegen canonicalizes every
  operation to IR (the existing `CanonOp`/`<hash>.query` machinery — mutations join it),
  hashes it, and writes the artifact into the persisted registry. The generated client
  handle carries the hash, typed vars, and the response/fragment **projections** — field
  types drawn from `schema.json`, so **no server type compiles into the UI crates**.
  Masking becomes physical: a field you didn't select doesn't exist client-side.
- **The registry is the server's entire executable surface.** `POST /__idyll/q/<hash>`
  and `POST /__idyll/m/<hash>` look the hash up in the checked-in registry and interpret
  the canonical IR against schema-typed resolvers/handlers. Unknown hash = 404 by
  construction; the reviewed artifact set is the allowlist. At boot the server
  typechecks every registry op against the current schema and **fails loud at startup**
  — schema drift is a deploy error, not a request-time 500. Deleting an artifact is
  explicitly revoking an operation; keeping old ones supports rollouts.

## No separate build tool — the dev server orchestrates

Explicitly decided: no relay-compiler-style CLI. `idyll-serve` is the compiler driver,
extending the existing build-on-change loop:

```
boot / source change
  → emit schema.json                       (server side, before the app build)
  → cargo build app crate                  (IDYLL_SCHEMA env → macros validate + emit
                                            registry artifacts + projections)
  → load + verify registry against schema  (fail loud on drift)
  → hot-swap membrane + reload browsers
```

Macros find the schema via the `IDYLL_SCHEMA` env var set by the dev server for the
cargo child process; when unset (bare `cargo build`, rust-analyzer, CI) they fall back
to the checked-in `schema.json` found by upward search from `CARGO_MANIFEST_DIR`.
Rebuild tracking via `include_str!` of the schema path in the expansion. Global checks
that a macro can't see (duplicate op names across crates, orphaned artifacts, registry
freshness) are the dev server's sweep at boot + a CI check.

## What survives from the name-based mutation round-trip (built 2026-07-06)

The transport machinery is unchanged: `ctx.mutate` (Live-only, commands out / messages
in), `DomCommand::ServerRequest`, the pending-request registry + `deliver_response`,
the `deliver` WIT export, runtime.js fetch-and-deliver. What changes: the request
carries the **operation hash** instead of a name; the server-side table is derived from
the registry + schema-typed handlers instead of `.mutation::<AddTodo>(handler)` builder
calls; responses are masked to the operation's recorded selection.

## The break list (Nathan 2026-07-06: "make sure you're breaking bits that need to
## break to realise the vision — not tip-tapping around")

No fallbacks, no dual paths. The following die outright:

- **`query!`'s generated `preload<Src>`** and the route table's preload closures — the
  server executes ONLY via the interpreter. `AppRoute` becomes `pattern → (op hash,
  vars-from-match)`; `AppRoutes` loses `preload`/`preload_operation` and the name-based
  mutation table.
- **`exec.rs` entirely** (`Fetch`/`Root`/`ResolveEdge`/`SeedFragment`/`SeedList`) — the
  `Resolvers` closure table is the one resolver surface. **`#[root]` dies with it**; the
  server declares roots as schema `RootDef`s and registers resolver closures.
- **The typed cache**: per-type cells, appliers, `Cache::register`, `RegisterTypes` —
  replaced by JSON records keyed `(schema type, id)`. `Seed::to_cache` loses its
  register argument; replay needs no type knowledge at all.
- **`Ref<T>` / typed `Frag` keys client-side** — `Frag<F>` carries a schema-tagged
  opaque id (or an inline `Masked<Value>` projection for value edges).
- **Node types in UI crates** — `on Todo` in `fragment!` resolves against `schema.json`
  (macro reads `IDYLL_SCHEMA` env, else upward search; clean macro errors). Projections
  are generated with schema-derived field types. `#[node]`/`#[value]`/`Db`/resolvers
  move to the server crate — finally possible, nothing in the app references them.
- **`inventory` operation collection** (`RegisteredOperation`/`operations()`) — the
  macros write `<hash>.query`/`<hash>.mutation` artifacts into the registry dir
  (`IDYLL_REGISTRY` env from the dev server; convention `ops/` next to `schema.json`);
  the server loads + boot-validates the DIRECTORY. The checked-in artifact set is the
  entire executable surface.
- **The name-based mutation endpoint** (`/__idyll/m/<name>`) — mutations are persisted
  registry artifacts like queries: `mutation!` generates the op handle (HASH, typed
  Vars, response projection), `ctx.mutate` ships the hash, the endpoint is
  `/__idyll/m/<hash>`, the server binds handlers by schema mutation name and masks the
  response to the artifact's recorded selection. The `ServerRequest` WIT command carries
  the hash.
- **Seed commits stop carrying whole nodes** — the executor masks each commit to the
  operation's selection (safe once the client cache is projection-based).

Shared schema parsing lives in a new tiny `idyll-schema` crate (types + serde), used by
`idyll-data` (re-export) and `idyll-macros` (which cannot depend on idyll-data — cycle).

## The seed is an ordered list, bound by parameter position (Nathan, 2026-07-06)

Hash-keying was wrong: **the same query can appear twice on one page** (a split-screen
route running `ArticleQuery` for two slugs), so the operation's identity cannot be the
binding. The binding is the **page component's parameter** — and Rust has no named
props, so position is the key:

```json
[ { "seed": { "commits": [...] }, "roots": { "article": "intro" } },
  { "seed": { "commits": [...] }, "roots": { "article": "outro" } } ]
```

- A route declares its operation **instances in parameter order**:
  `AppRoute::new(pattern).operation(ArticleQuery::query_file(), vars_left)
  .operation(ArticleQuery::query_file(), vars_right)` — the same persisted artifact
  twice with different vars is two entries. The registry stays hash-deduplicated (one
  `<hash>.query`); *instances* live on routes.
- The **page signature is the contract**: `async fn page(ctx, left:
  Preloaded<ArticleRoots>, right: Preloaded<ArticleRoots>, …)`. The guest's page table
  deserializes `seeds[0]`, `seeds[1]` into exactly those leading parameters — an
  arity/type mismatch fails loud at the one place a route meets its page.
- A **static page is the empty list** and takes no leading `Preloaded` params. No route
  kinds, no seed-closure escape hatch.
- **No bundled path-seed endpoint** (Nathan: `/__idyll/s/<path>` removed — "just make
  multiple GET requests; refactor for efficiency if need be"). The one data endpoint is
  `GET /__idyll/q/<hash>?vars=…`: one request per operation instance, individually
  browser/CDN-cacheable; a transition fires its route's instances concurrently and
  assembles the positional list client-side.
- Which instances a path wants is the **shared route table's** knowledge, so the table
  lives in the app crate and compiles into both sides (it is Src-free pure data now:
  pattern → [(artifact, vars-from-match)]). The server imports the same `routes()` for
  SSR; mutation handlers — the only Src-bound part — live on the server's `Resolvers`
  (M4).

## Blog content is schema (Nathan, 2026-07-06 — no static-props escape hatch)

The blog models its content as nodes and roots like any app: `Article { id: slug,
title, markdown, sims: [SimRef] }` (+ value types for TOC sections / sim refs), roots
`article(slug) -> Article`, `articles -> [Article]`, `book -> …`. Its pages read
projections via `fragment!`/`query!`; `PageSeed` dies. Static-by-type stays exactly as
is (Msg = Infallible, zero client bytes) — static means *no interactivity*, not *no
data model*.

## Milestones — ALL BUILT & BROWSER-VERIFIED (2026-07-06)

1. ✔ Schema model + descriptors + builder + emission + dev-server orchestration.
2. ✔ Server-side executor over canonical IR; boot-validated registry.
3. ✔ `fragment!`/`query!` validate against `schema.json` and emit projections; the
   cache is schema-typed JSON records with per-field merge; masking enforced at the
   source; value edges and value **lists** spread inline.
4. ✔ `mutation!` artifacts on the registry, executed only by `OpHash`
   (`{msb,lsb}` two-word identity); `#[root]`/`#[mutation_handler]` emit descriptor +
   typed execution glue from one fn signature (no JSON digging in app code); responses
   masked to the artifact's selection.
5. ✔ Both apps rewired and verified in Chrome: todo (SSR → hydrate → per-instance-GET
   transitions → hash-addressed add-todo → refresh persistence) and the blog (content
   modeled as schema — `Article`/`IndexData`; `PageSeed` deleted; zero client bytes).

As-built deltas from the plan above: the seed is a **positional list** (not a map) bound
to the page's leading `Preloaded` parameters; `/__idyll/s/<path>` was removed in favor
of per-instance `GET /__idyll/q/<hash>?vars=…`; the shared route table lives in the app
crate and compiles into both sides (the `route` WIT export resolves transitions).
