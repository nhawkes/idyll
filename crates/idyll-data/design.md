# idyll-data: a Relay-shaped data model in Rust

## What exists vs. what's missing

`idyll-data` already has the **runtime** half of Relay: a normalized store (`Cache`),
typed references (`Ref<T>`), resolvers, generation-tagged commits, a fixpoint executor
(`resolve_graph` over `ComponentNode`), and the waterfall-free read (`Cache::read`).

It is missing the **declarative** half — the part that makes Relay Relay:

- a **fragment**: a component's typed declaration of the fields it reads, checked against
  the schema, composing other components' declarations;
- **persisted operations**: content-addressed, committed, revocable query identities so
  the client ships a *hash*, not a query, and the server runs an *allowlist*;
- **masking**: a component may read only what it declared, and may hold only its
  children's *keys*, never their data.

Everything below adds that declarative layer. It all **compiles down to the existing
`Cache` / `resolve_graph` / `ComponentNode` / `server.rs` machinery** — that machinery
stops being the public API and becomes the compile target.

---

## Load-bearing invariants (hold these or the design breaks)

1. **The schema is a leaf.** `#[record]` types and resolvers never depend on fragments or
   queries. Queries hang off the schema; nothing hangs off queries. This is what keeps
   the whole build a DAG.
2. **`schema.json` is an OUTPUT, never a build INPUT.** The source of truth is the Rust
   `#[record]` structs, which compile on their own. `schema.json` is a *photograph* taken
   after a green build. Nothing reads it to build. (A future DSL front-end reads it as
   *staged codegen* — schema compiles → dump → DSL generates → rebuild — which is staging,
   not a cycle, precisely because of invariant #1.)
3. **The fragment checker is the live types, not `schema.json`.** Fragments are checked
   against the current in-tree structs in the same compilation, via associated-type
   projection (below). This is why editing a record + a fragment *in sync* always
   compiles: there is only one `Post` in the compilation, and it's the one you just
   edited. Checking against a dumped schema would reintroduce the build deadlock (check
   needs the dump → dump needs the build → build needs the check).
4. **The registry is content-addressed and self-verifying.** `filename == sha256(file
   bytes)`, files are canonical sorted-key JSON, the directory accumulates, and *deleting*
   a file is the only revoke. Build never auto-deletes.

---

## The four artifacts

### 1. Schema — `#[record]` structs + resolver functions

A schema type is a `#[record]` struct; edges are `Ref<T>` fields; roots/loads are
resolver functions (today's `Resolver<T>`).

```rust
#[idyll_data::record]
struct User { id: Id<User>, name: String, avatar_url: String }

#[idyll_data::record]
struct Post {
    id: Id<Post>,
    title: String,
    body: String,
    author: Ref<User>,             // edge
    comments: Vec<Ref<Comment>>,   // edge list
}
```

`#[record]` additionally emits a **`HasField` marker per field** — the machinery that lets
fragments name a field's type *without writing it*:

```rust
pub mod post { pub struct title; pub struct author; pub struct comments; }
impl HasField<post::title>    for Post { type Value = String; }
impl HasField<post::author>   for Post { type Value = Ref<User>; }
impl HasField<post::comments> for Post { type Value = Vec<Ref<Comment>>; }
```

with `pub trait HasField<F> { type Value; }` and `pub trait Fragment { type On; /* +
SELECTION */ }`. Marker-module scoping follows a convention (markers sit next to the
record) with an optional `schema = path` hint — the same shape `cynic` uses for its
generated schema module.

### 2. Fragment — a **type-free** selection via `fragment!`

Fields carry **no types**: the type flows from the schema. Because a typeless field is not
legal Rust, a fragment is a **function-like macro** body (an attribute-on-struct can't
parse type-free fields):

```rust
fragment! {
    Avatar on User { name, avatar_url }
}

fragment! {
    PostCard on Post {
        title,                  // scalar leaf → ReadSignal<schema type>
        author: Avatar,         // edge, spread the Avatar fragment → Frag<Avatar>
        comments: [CommentRow], // edge list → Vec<Frag<CommentRow>>
    }
}
```

The **only** structural annotation the author writes is leaf-vs-edge, which *must* be in
the syntax because it isn't in the types: bare `title` = leaf; `field: Child` = spread
fragment `Child` on that edge; `[Child]` = edge list.

**What `fragment!` generates** (for `PostCard`):

```rust
pub struct PostCard;                                    // the selection handle
impl Fragment for PostCard { type On = Post; const SELECTION: Selection = /* … */; }

pub struct PostCardData {                               // the reactive read view ($data)
    pub title:    ReadSignal<<Post as HasField<post::title>>::Value>,   // compiler → String
    pub author:   Frag<Avatar>,
    pub comments: Vec<Frag<CommentRow>>,
}

const _: () = {                                         // edge correctness probes
    fn _author(v: <Post as HasField<post::author>>::Value)
        -> Ref<<Avatar as Fragment>::On> { v }          // asserts Post.author == Ref<Avatar::On>
    fn _comments(v: <Post as HasField<post::comments>>::Value)
        -> Vec<Ref<<CommentRow as Fragment>::On>> { v }
};
```

`<Post as HasField<post::title>>::Value` **is** "the current type of `Post.title`,"
resolved in this compilation against the live struct — it both *names* the type (for
generation) and *checks* it exists (rename the field → the projection fails → error at
the fragment). Field-access probes could only check, never name; naming is why we need
the `HasField` markers.

### 3. Query — a root fragment + variables; the persisted operation

```rust
query! {
    PostRoute($id: Id<Post>) {
        post(id: $id): PostCard,   // root resolver + arg wiring + fragment spread
    }
}
```

A query composes fragments into **one** operation. Its `SELECTION`, walked transitively to
fixpoint, is the normalized operation — the thing that gets hashed and persisted.

### 4. `Frag<F>` — the masked fragment reference (Relay's `$key`)

```rust
pub struct Frag<F: Fragment> {
    reference: Ref<F::On>,          // a Frag<Avatar> wraps a Ref<User>
    _fragment: PhantomData<fn() -> F>,
}
```

A parent that spreads `Avatar` receives a `Frag<Avatar>` — a key, not the user's fields.
It has no descriptor to read into it and *must* hand it to the `avatar` component. Masking
is enforced by the type system, not a lint.

---

## Using it (the useFragment read)

```rust
async fn post_card(ctx: Ctx<Setup, Never>, post: Frag<PostCard>) -> Result {
    let data = ctx.use_fragment(post);   // PostCardData
    ctx.render(live_view! {
        h1 { (data.title.get(cx)) }
        (avatar(ctx.child(), data.author))   // pass the KEY; can't read into it
    })
}
```

`use_fragment` **is** `Cache::read` (no `begin_load`): it awaits *already-requested* data
and never starts a load, so a pre-render read can only wait for in-flight data and can
never open a waterfall. Under a fully-seeded operation it resolves synchronously (SSR
render stays sync); the only way it suspends is inside a `@defer`d boundary, which is
exactly a deferred stream.

---

## The persisted-query registry

- Each operation has a **content hash** as its identity. The client ships the *hash* (+
  variables), never the operation body.
- The registry is a directory of **`<sha256>.query`** files — canonical sorted-key JSON of
  the normalized IR, one per operation. `filename == sha256(contents)`, so the whole
  directory self-verifies with `sha256sum`, and a PR diff shows exactly which operations
  were added/removed (the reviewable control surface + allowlist).
- The directory **accumulates** and is committed. A server built from today's commit still
  holds last-month's `<hash>.query`, so an un-updated client keeps working — client and
  server deploys decouple.
- **Deleting a file is the revoke.** Explicit, auditable; an old client holding that hash
  now fails deliberately.
- **`.fragment` files dropped for now** — fragments inline into the operation
  (Relay-classic). Independently-addressable fragment files earn their keep only when a
  unit is *re-requested* on its own (`@defer`, refetch, pagination); add them then.

### The IR: custom, GraphQL-*shaped*, JSON

Not GraphQL-on-the-wire — both ends are Rust and share the actual types (`Id<Post>` is a
real type, `author` a real `Ref<User>`), and speaking GraphQL would drag in a second
schema representation + an execution engine dispatching by string field name. The `.query`
file holds a **canonical custom IR** (the normalized selection tree) that the server
deserializes and dispatches **straight to typed resolvers**. We steal GraphQL's *structure*
(selection sets, spreads, variables, `@defer`) so a real GraphQL backend could be reached
later by lowering the IR to GraphQL text at an adapter — the door stays unlocked without
building the engine now.

---

## The build: server-driven, git-reset back-compat, no drift

The native `server` orchestrates the build (a **dev** convenience; prod ships the committed
registry + prebuilt wasm):

```
server build (dev):
  1. compile wasm-server (wasip2, feature = "manifest")   ← THE CHECK: a bad fragment
                                                             fails to compile here
  2. instantiate it in the idyll-host membrane, call schema() + operations()
       → schema.json           (reviewable projection of the records)
       → normalized IR per op   (already validated at step 1)
  3. git checkout HEAD -- <query-dir>   (no git → empty it)   ← restore back-compat baseline,
                                                                 discard uncommitted churn
  4. write <sha256>.query for current ops  (ADDITIVE; existing = byte-identical no-ops;
                                            never auto-delete → removed-but-committed ops
                                            survive for old clients until a human git rm's)
  5. compile wasm-client (web, NO manifest feature)  ← ships operation hashes only
  6. load <query-dir> as the runtime allowlist, serve
```

**`schema()` / `operations()` are wasm exports** (same packed `(ptr,len)` shape as
`render()`, so `idyll-host` already knows how to call them), gated behind a `manifest`
feature so their baked JSON never reaches the browser bundle. Two payoffs:

- **No drift** — schema/ops come out of the *same compiled component* that renders, so the
  registry can never describe a schema the renderer doesn't implement.
- **They *dump*, they don't *check*.** Validation already happened at compile time (step 1,
  via the `HasField` projections). `schema()` just serializes the validated result — never
  a gate, so it can't create a cycle.

### The hourglass

```
fragment! / query! (Rust structs+macros)  ──┐
        [DSL later, reads schema.json]     ──┤  lower to
                                             ▼
        canonical GraphQL-shaped IR   ← the waist / stable contract
                                             │  sorted-key JSON
                                             ▼
              <sha256>.query  (committed, self-verifying, revocable)
                                             │  server loads as allowlist
                                             ▼
                  dispatch to typed resolvers → seed Cache → sync render
```

A DSL query and a struct query expressing the same requirement **hash to the same file** —
the proof that the front-end is cosmetic and the IR is the real contract.

---

## Bridge to what exists (nothing thrown away)

- `Frag<F>` = `Ref<F::On>` + phantom — reuses `Ref` + its id serialization.
- `use_fragment` **is** `Cache::read` (already built).
- `query!`'s `operations()` lowers the composed `SELECTION` to `Vec<ComponentNode>`;
  `resolve_graph` seeds them to fixpoint (already built); `@defer`d spreads become
  `.deferred()` Pending nodes (already built) → streamed boundaries in `server.rs`
  (already built). The hand-written `expand` closures were this macro's output, written by
  hand.
- Hydration: seeded records + operation keys serialize into the existing
  `HydrationSnapshot`; the client seeds the same cache and the same fragments read the same
  masked data → isomorphic.

---

## Runtime back-compat (a real, separate concern — not a build cycle)

An old committed `<hash>.query` encodes an operation against the *old* schema. After a
field is removed, the server may no longer be able to service that old hash. That is the
genuine persisted-query tension and it is a **runtime deprecation** problem (deprecate a
field before deleting it; a removed field breaks operations that still select it), *not* a
build cycle. Design for it with field deprecation later; it does not affect the build DAG.

---

## Phased build plan

- **D1 — Foundations (native).** `Fragment`, `HasField<F>`, `Frag<F>` (wrapping `Ref`),
  the `Selection` IR types + canonical sorted-key JSON serialization + `sha256` filename.
  `use_fragment` already exists as `Cache::read`. Tests: `Frag` round-trips; canonical
  JSON is byte-stable; filename == hash.
- **D2 — `#[record]` markers + `record` derive.** Emit `HasField` impls + the marker
  module. Tests: projection `<Post as HasField<post::title>>::Value == String`; a
  nonexistent field's projection fails to compile (trybuild).
- **D3 — `fragment!` macro.** Parse the type-free body, generate the `Fragment` impl +
  `…Data` reactive struct + edge probes + `SELECTION` const. Tests: a good fragment
  compiles and reads masked data; a field-schema mismatch is a compile error (trybuild);
  masking (parent can't read child fields) holds by type.
- **D4 — `query!` + operation lowering.** Compose `SELECTION` to fixpoint → normalized IR →
  `Vec<ComponentNode>`; hash → `<sha256>.query`. Tests: composed op is deterministic +
  hash-stable; lowering seeds via `resolve_graph`; `@defer` → Pending.
- **D5 — `schema()` / `operations()` exports + `manifest` feature.** Dump validated
  descriptors as canonical JSON through the membrane. Tests: `schema()` round-trips the
  records; client build (no feature) omits them.
- **D6 — Server-driven build + registry loader.** git-reset baseline, additive write,
  self-verify, load as allowlist, reject unknown hash. Tests: additive over a committed
  baseline; no-git = clean slate; unknown hash rejected; committed-but-removed op survives.

(Then the previously-planned Phase 4 streaming/fallback and Phase 5 real wasip2 guest fold
in on top: boundaries are `@defer`/blown-budget paths over this registry; the real guest is
the thing that exports `render`/`schema`/`operations`.)

---

## Update — Node vs value, FragTarget, async read, routing (built after D3)

A correction that reshaped the model: **not every schema type is a Node.** This section
supersedes the "everything is a record" framing above.

### Node vs value (built)

- `trait Record: Clone + Serialize + DeserializeOwned` — the id-less **value** base; a
  fragment target (has `HasField` markers) but not normalized and not `Ref`-able.
- `trait Node: Record { type Id; fn id(); }` — a globally-identified **entity**:
  fetchable by id, normalized in the cache, the only thing a `Ref<T>` points at.
- `#[node]` (requires an `id` field) vs `#[value]` (no id, reached inline through a
  parent edge). A `Post` with no id, reachable only via `Category`, is a `#[value]`.
- Only Nodes are in the resolver registry; **edges to Nodes resolve by id, value edges
  are inline** — which is the operation executor's whole dispatch rule.

### FragTarget + reader-backed Frag (built)

A value has no id, so a fragment key can't always be a `Ref`. `Frag<F>` wraps
`<F::On as FragTarget>::Key`, where `FragTarget` is emitted **per type** (blanket-over-Node
would collide with per-value impls under coherence):

- **Node target:** `Key = Ref<Self>`; `read_frag` **awaits the cache** — Ready when the
  route preloaded it, **Pending → a deferred boundary**.
- **value target:** `Key = Masked<Self>` (a captured reactive projection out of the parent
  Node); `read_frag` is immediately Ready.

`Masked<V>` is the masked reader — `get(cx)` tracks (views), `peek()` is untracked (derive
child keys without a `Cx`). Node vs value at an edge is dispatched by `EdgeField`
(blanket for `Ref<N>`, per-type for values via `#[value]`) through the free `edge_key`
helper, so `fragment!` authoring stays type-free: `author: Avatar` looks identical whether
`author` is a Node ref or an embedded value.

### The read is async (correction)

An earlier draft said the read goes synchronous — **wrong.** The guest is async
internally; `read().await` is a suspension point, and the render is polled **once**:
Ready → HTML, Pending → deferred boundary. That poll-once/suspend mechanism *is* how
`@defer` works, so the read must stay async. "Synchronous" only describes the **host
boundary** (no fiber resume across an await), never the guest read.

### Routing = component + preloaded query (the loadQuery/useFragment split)

Routing is ours: a **route = a component + a preloaded query** (+ variables from the URL).
This makes the two navigation modes fall out of the same async read:

- **Initial route (SSR):** the query is **preloaded** (fixpoint-resolved → cache seeded)
  *before* the poll-once render, so the shell's `read().await`s are Ready. (Unseeded →
  deferred boundary, streamed.)
- **Client navigation:** no SSR; preload **starts** the loads, the browser runs a normal
  async runtime, and `read().await` genuinely **suspends and resumes** behind a fallback.

Same component, same await — the only difference is whether the awaited data is already
seeded or still in flight. This is exactly Relay's split: **preload = `loadQuery`** (starts
requests — server to fixpoint, client kicks off fetches), **read = `useFragment`** (awaits
already-requested data, never starts a load). That is *why* read never starts a load: the
route already did. The `Router` (extending idyll-kit's) drives preload-then-render; it
lands with D4's query/entrypoint.

---

## Update — the Seed: one wire format across every boundary (built after D5)

The executor no longer writes a live [`Cache`]. `preload` returns a **`Seed`** — an
ordered, serializable `Vec<CacheMsg>` — and the far side [`replay`]s it into *its own*
cache. This is the load-bearing change that lets the whole render travel through the wasm
membrane: the reactive cache is `!Send` (`Rc`/`RefCell`), but a `Seed` is plain `Send`
data, so **only the seed crosses**, never the cache. (Confirmed by a `Send` assertion on
`preload`'s future and on `Seed` itself.)

**One format, four crossings** — deliberately the *same* bytes for all:

- **SSR seed** — host preloads → seed → guest replays → poll-once render reads present data.
- **Hydration** — the same seed ships to the browser; the client replays it so the same
  fragments read the same masked data → isomorphic.
- **Client navigation** — the query endpoint (`POST /__idyll/q/<hash>` + vars) runs the
  fetches server-side and returns a seed; the client replays it. `Fetch<Src>` never exists
  client-side.
- **Deferred boundary** — a `@defer`'d region streams `(html, seed-delta)`; the blown-budget
  isomorphic fallback streams just the seed-delta (HTML optional). `Seed::absorb` folds a
  delta into the base.

**Mechanics.** `Seed::push::<T: Node>` records a fetched Node (values ride inline in their
parent's JSON — never pushed). Generations are strictly increasing over the batch, so a
diamond fetch's later commit supersedes the earlier on replay (fetch-order last-write-wins),
with no `begin_load` bookkeeping — the generation rides inside the message, exactly as the
commit-log round-trip relies on. `apply_commit` still runs the generation check + upsert in
the reducer, so replay, hydration, and dev-replay are one mechanism.

**Stable type tags (was `std::any::type_name`).** A commit's `type_tag` and the applier
registry key are now `Record::TYPE_NAME` — a macro-emitted, version-stable schema name
(`"User"`), the *same* vocabulary as `FragmentDef::on` and the future `schema.json`. This
matters the instant a seed crosses compilations (native host → wasm guest) or reaches a
browser: `type_name` is explicitly unstable, and a mismatched tag makes `apply_commit`
**silently drop** the commit.

**No global type registry — registration is operation-scoped (explicit over magic).**
Replaying a seed needs the cache to know how to decode each `type_tag`. Rather than a
link-time `inventory` of every `#[node]` in the binary (a global registry: invisible in
the source, un-isolatable in tests, silently cross-crate), the *operation* registers
exactly the types **its own query reads**: `MyRoute::register(&cache)` walks the static
fragment tree via `RegisterTypes` (the replay-side mirror of `SeedFragment`), calling
`FragTarget::register` at each node (a Node registers its applier; a value is a no-op) and
recursing into edge children. You can read the generated `register` and see precisely which
types a route touches. The fragment tree is a DAG (a self-referential fragment does not
compile), so the walk terminates. This is a deliberate house rule: **no singletons, no
global registries** — a route declares its footprint.

> The one remaining `inventory` use — `operations()` collecting every `query!` for the D6
> registry writer — is on the same chopping block: the **route table** (a route = pattern +
> query + vars-mapper + component, written once and compiled into both server and client)
> is the explicit list, and `operations()` becomes a map over it. It is kept only until the
> route table lands, so the build step isn't left with nothing to enumerate.

**`BoxError` is `Send + Sync`** so the whole preload future stays `Send` on the native host.

This supersedes the design's earlier "`preload(cache)` seeds the cache in place" framing:
preload is now pure (source → seed), and seeding a live cache is `MyRoute::register` +
`replay` on the consuming side.

---

## Update — root resolvers: `#[root]` free functions, axum-style (built after the Seed)

A query's **root fields** name resolvers. The "first arg is the id, `Default` otherwise"
hack is gone. Roots are now plain functions, wired axum-style (dependencies declared in the
signature, provided at the boundary), with **no registry** — the honest expression of the
no-magic rule above.

```rust
#[root]
async fn post(db: &Db, id: u64) -> Result<Post, DbError> { db.post(id).await }

#[root]                                   // a LIST root: the blog's `posts()`
async fn posts(db: &Db) -> Result<Vec<Post>, DbError> { db.all_posts().await }

query! {
    HomeRoute()            { posts: [PostCard] }       // list root  → Vec<Node>
}
query! {
    PostRoute($id: u64)    { post(id: $id): PostCard } // single root → Node
}
```

- **`#[root]` is sugar over a `Root<Src>` trait**, keyed by `Src` exactly like `Fetch<Src>`.
  The macro reads `Src` from the first parameter, `Args`/`Output` from the rest of the
  signature, and generates `impl Root<Src>` on a mangled handle ZST — preserving the
  original function verbatim (renamed) and calling it. Because it is keyed by `Src`, roots
  are **test-swappable** the same way `Fetch` is (a mock `Src`); the source can be concrete
  (`&Db`, axum-`AppState`-style) or generic (`async fn post<S: Source>(src: &S, …)`), the
  author's choice per resolver.
- **No registry, no singleton.** `query!` dispatches `<handle as Root<Src>>::resolve(src,
  args)` by name; the handle is a private mangled ident (`__idyll_root_post`) both macros
  agree on, so it never collides with a `#[node]`'s snake-case marker module (`Post` → `mod
  post`) or a local binding. You can read the generated `preload` to see exactly which
  resolvers a route calls.
- **List vs single is in the query syntax** (`: [Child]` vs `: Child`) and in the operation
  hash (`Sel::Root { list }`) — a list root is a different execution contract, so it is part
  of the persisted identity. A list root seeds every element (push + recurse its fragment);
  a single root seeds one.
- **Type-checked against the fragment.** `preload`'s where-clause binds
  `handle: Root<Src, Args = (var types…), Output = <Child as Fragment>::On>` (or `Vec<…>`),
  so a resolver whose return type doesn't match the fragment's target — or whose args don't
  match the declared query variables — is a compile error at the `query!`.

### `Preloaded<Q>` — the route's proof-of-preload token

`preload` returns **`Preloaded<Q> { seed, roots }`**, not a bare `Seed`. `roots` is a
`query!`-generated `<Name>Roots` handle with one field per root field — a seeded `Ref`
(single) or `Vec<Ref>` (list). Its accessors hand out **`Frag` keys**, never record data:
`preloaded.roots.post()` → `Frag<PostCard>`, `preloaded.roots.posts()` → `Vec<Frag<PostCard>>`.

This is the keystone that makes routing waterfall-free and masked:

- **Only `preload` constructs a `Preloaded`**, so a route component can't name a query
  without evidence it was preloaded — there is no render-time-fetch path to write by accident.
- The component reads its **top-level frags off the token** (`roots.post()`), rather than
  hand-building a `Ref` or fetching — the read is `useFragment` over already-seeded data.
- `Preloaded` is `Serialize`/`Deserialize` (seed + refs), so the *whole* thing crosses the
  membrane: the server preloads once, ships `Preloaded`, and the guest/client replays the
  seed (`preloaded.seed.to_cache(Q::register)`) and reads the same roots — isomorphic.

Demonstrated end-to-end (native tests): a single root (`post(id)`) and a list root
(`posts()`) each round-trip through `serde_json`, replay into a fresh cache, and read every
element + its edges off `preloaded.roots` with no fetch at read time.
