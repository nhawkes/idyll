//! # idyll-serve — the framework's dev server
//!
//! One typed builder ([`Server`]) stands up the whole app around **one distinguished
//! operation**: the route query, `route(request) -> Option<Page>`. A request path is a
//! query execution — the interpreter runs the persisted route artifact natively, the
//! page node comes back as **pure data**, and the server mounts the app's **page
//! component** through the membrane with that executed seed, folds it, and discards
//! the instance. The page is content; views exist in exactly one place, the app crate
//! ("if it were two languages, the app would be the JS and the server the Rust").
//!
//! In the browser, **liveness enters exactly where a live marker painted**: the
//! document's load plan derives from the fold — a markerless page ships zero
//! framework bytes and its links are native navigation; a live page ships the
//! runtime, the seed, and the app module for its mounts. An app that wants SPA
//! behaviour puts a live at the top and drives navigation as data.
//!
//! The client bundle is **content-addressed**: every file serves under `assets_route`
//! (default `/__idyll__`, Next's `_next`) as `<hash>.<ext>`, immutable and
//! brotli-negotiated, and the document's head names every URL the page will need —
//! the load is one dynamic response plus one flat wave of cached fetches, no
//! discovery hops. Version skew recovers rather than retains: a stale document whose
//! assets have been swept reloads itself once (the fresh document *is* the re-sync).
//!
//! ```no_run
//! # use idyll_serve::Server;
//! # async fn f<Src: Clone + Send + Sync + 'static>(
//! #     route_query: idyll_data::QueryFile,
//! #     root: idyll_data::AppRoot<Src>,
//! #     data: Src,
//! # ) -> anyhow::Result<()> {
//! Server::builder()
//!     .app_crate("my-app")             // ALL the UI: the page component + live
//!     .route_query(route_query)        // THE operation: route(request) -> Page
//!     .root(root)                      // Root { query, mutation } — schema AND resolvers
//!     .data(data)                      // the host-side data source
//!     .manifest_dir(env!("CARGO_MANIFEST_DIR"))
//!     .build()
//!     .serve()
//!     .await
//! # }
//! ```
//!
//! At boot the server validates the **route contract**
//! ([`validate_route_contract`](idyll_data::validate_route_contract) — a drifted schema
//! is a startup failure), publishes the contract (`schema.json` + the `ops/` registry),
//! and serves. Client data needs (an SPA live's navigation, mutation refreshes) ride
//! the *same* persisted artifacts over `GET /__idyll/q/<hash>` — one wire shape.

mod build;
mod chunks;
mod split_emit;

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Router,
};
use idyll_host::{MembraneEngine, MountOutcome};
use tokio::sync::broadcast;
use tokio_stream::{
    wrappers::{BroadcastStream, UnboundedReceiverStream},
    StreamExt,
};
use tower_http::{services::ServeDir, set_header::SetResponseHeaderLayer};
use idyll_route::Route as _;

pub use build::{AppAssets, AppBuild, AssetManifest};

/// How the server runs — the three commands every idyll app server exposes
/// (`<server> dev|build|prod`). Staleness never needs detecting by hand: `prod`
/// simply runs the build — cargo fingerprints sources/deps/profile/toolchain and
/// no-ops when fresh, and the jco transpile is gated on a content stamp of the
/// wasm + jco version + embedded runtime assets ([`AppBuild::ensure_client`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Mode {
    /// Debug-profile wasm, source watcher, hot rebuild, live browser reload.
    #[default]
    Dev,
    /// Build the optimized app bundle, then exit.
    Build,
    /// Optimized wasm (rebuilt only if stale), no watcher, no reload channel.
    Prod,
}

/// Parse the standard idyll server CLI (`dev` | `build` | `prod`; none = dev).
pub fn mode_from_args() -> Mode {
    use clap::Parser as _;

    #[derive(clap::Parser)]
    #[command(about = "idyll app server", disable_version_flag = true)]
    struct Cli {
        #[command(subcommand)]
        command: Option<Cmd>,
    }

    #[derive(clap::Subcommand)]
    enum Cmd {
        /// Debug build + source watcher + live reload (the default).
        Dev,
        /// Build the optimized app bundle and exit.
        Build,
        /// Serve the optimized build (rebuilding only if stale).
        Prod,
    }

    match Cli::parse().command {
        None | Some(Cmd::Dev) => Mode::Dev,
        Some(Cmd::Build) => Mode::Build,
        Some(Cmd::Prod) => Mode::Prod,
    }
}

/// The idyll dev server. Build it with [`Server::builder`] and drive it with
/// [`serve`](Server::serve). Generic over the app's data source `Src` (whatever the
/// resolvers execute against).
#[derive(bon::Builder)]
pub struct Server<Src: Clone + Send + Sync + 'static> {
    /// The cargo package name of the app's UI crate — **all the UI code**, built to
    /// one wasm component: the page component (the root), the optional head, and the
    /// live. The server mounts the page through the membrane per request and
    /// folds it; the browser mounts only the live its paint declared.
    #[builder(into)]
    app_crate: String,
    /// **The route query** — the one distinguished persisted operation
    /// (`route(request) -> Page`). Document loads execute it natively; an SPA
    /// live's navigation refires the same artifact over `/__idyll/q/<hash>`.
    route_query: idyll_data::QueryFile,
    /// The host-side data source the resolvers execute against.
    data: Src,
    /// A directory inside the workspace, used to locate the app crate and target dir at
    /// runtime. Pass `env!("CARGO_MANIFEST_DIR")` from the server binary.
    #[builder(into)]
    manifest_dir: PathBuf,
    /// TCP port to bind. Defaults to `3001`.
    #[builder(default = 3001)]
    port: u16,
    /// App-owned static assets (stylesheets, images…), served at `/static`. Three
    /// namespaces: [`assets_route`](Self::assets_route) is the framework's immutable
    /// content-addressed bundle, `/__idyll/*` its dynamic endpoints (operations,
    /// mutations, dev reload); everything else, including `/static`, is the app's.
    #[builder(into)]
    assets: Option<PathBuf>,
    /// Where the content-addressed client bundle serves (Next's `_next`). Every file
    /// under it is `<hash>.<ext>` with an unbounded-cache `immutable` header — any
    /// CDN policy is safe against these names. Defaults to `/__idyll__`.
    #[builder(into, default = "/__idyll__".to_string())]
    assets_route: String,
    /// The per-live-mount preemption budget, in [`MembraneEngine::TICK`]s. The
    /// ticker sleeps between increments, so a tick is a floor on elapsed time rather
    /// than an exact millisecond — coarser wherever the platform's timer is. A mount
    /// that overruns traps and ships its region unpainted. Defaults to `100`.
    #[builder(default = 100)]
    render_budget_ticks: u64,
    /// How to run (see [`Mode`]); wire the CLI in with [`mode_from_args`].
    #[builder(default)]
    mode: Mode,
    /// The app's **root object** — `Root { query, mutation }` (plus chained `Ref`
    /// fetchers): the ONE entry surface both the published schema and the resolver
    /// table derive from. Must honour the route contract: a `route` root taking
    /// `request: Request` and yielding a page node with `id`/`title` — checked loud
    /// at boot. Everything else on the page node is data the app's components read.
    #[builder(into)]
    root: idyll_data::AppRoot<Src>,
    /// The app's persisted **mutation** artifacts. Boot-validated like the route query;
    /// the client only ever sends an artifact's hash. Defaults to none.
    #[builder(default)]
    mutations: Vec<idyll_data::MutationFile>,
    /// Where `schema.json` is written (and checked in); the `ops/` registry directory
    /// sits beside it. Defaults to the parent of `manifest_dir`.
    #[builder(into)]
    schema_path: Option<PathBuf>,
}

/// The app's engine: the membrane plus what its mount table declares. Rebuilt (and
/// re-validated) whole on every dev hot-swap, so `has_head` can never go stale
/// against the component it describes.
struct AppEngine {
    membrane: MembraneEngine,
    has_head: bool,
}

/// Validate a freshly built component's mount table and wrap it: an app without a
/// `page` cannot serve — the route view is the root component.
fn app_engine(membrane: MembraneEngine) -> anyhow::Result<AppEngine> {
    let names = membrane.live().context("querying the app's mount table")?;
    anyhow::ensure!(
        names.iter().any(|name| name == "page"),
        "the app component declares no `page` — the route view is the root component \
         (`guest! {{ page: …, … }}`); found: {names:?}"
    );
    Ok(AppEngine { has_head: names.iter().any(|name| name == "head"), membrane })
}

/// Shared server state behind the axum handlers.
struct AppState<Src: Clone + Send + Sync + 'static> {
    /// The app engine — the membrane every page render mounts through. Hot-swappable;
    /// dev rebuilds replace it under the lock.
    engine: Arc<RwLock<AppEngine>>,
    data: Src,
    executor: Executor<Src>,
    /// The route operation's persisted identity — what `render_path` executes and
    /// what the mutation endpoint re-executes for its refresh seed.
    route_hash: idyll_data::OpHash,
    /// The page node's validated contract shape (from `validate_route_contract`).
    page: idyll_data::RecordDef,
    /// The published client bundle's role → filename map. Behind a lock because dev
    /// rebuilds mint new names (content-addressed: new bytes, new name).
    manifest: Arc<RwLock<AssetManifest>>,
    /// The URL prefix the manifest's files serve under.
    assets_route: String,
    budget: u64,
    /// Broadcasts what a dev rebuild changed, so connected browsers swap styles in
    /// place or reload — see [`Rebuild`].
    reload: broadcast::Sender<Rebuild>,
    /// The style state — the workspace rule table plus the live universe. Behind a
    /// lock because the dev watcher re-extracts the table on every source change;
    /// whatever is here is authoritative for any name it contains (compiled-in rule
    /// text may be staler).
    styles: Arc<RwLock<Styles>>,
    /// Dev mode: the rendered page gets the live-reload client injected.
    dev: bool,
}

/// The interpreter's state: schema + resolvers + the boot-validated persisted-operation
/// sets. The maps — keyed by [`OpHash`](idyll_data::OpHash) — **are** the allowlist:
/// an unknown hash has no entry, so nothing of a client's choosing ever executes.
struct Executor<Src: Clone + Send + Sync + 'static> {
    schema: idyll_data::Schema,
    resolvers: idyll_data::Resolvers<Src>,
    ops: HashMap<idyll_data::OpHash, idyll_data::CanonOp>,
    mutations: HashMap<idyll_data::OpHash, idyll_data::CanonMutation>,
}

impl<Src: Clone + Send + Sync + 'static> Executor<Src> {
    /// Execute one boot-validated operation by hash with the given variables.
    async fn execute(
        &self,
        data: &Src,
        hash: idyll_data::OpHash,
        vars: &serde_json::Value,
    ) -> Option<Result<idyll_data::Executed, idyll_data::ExecError>> {
        let op = self.ops.get(&hash)?;
        Some(idyll_data::execute(&self.schema, op, &self.resolvers, data, vars).await)
    }
}

/// The style state behind the handlers: the whole workspace's rule table (resolution
/// always searches everything) and the **live universe** — the app crate's
/// transitive workspace closure, the only packages whose un-collected rules a live
/// page must pre-provision (a live is wasm linked from the app crate; it cannot
/// materialize a rule declared outside its link closure). The universe is fixed for a
/// server's lifetime (a dependency edit needs the restart it needs today); the table
/// hot-swaps under it.
struct Styles {
    table: idyll_styles::StyleTable,
    universe: std::collections::BTreeSet<String>,
}

/// The `styles.json` artifact `build` publishes beside the client bundle and `prod`
/// boots from — [`Styles`], as reviewable derived data (the schema.json pattern).
#[derive(serde::Serialize, serde::Deserialize)]
struct StylesArtifact {
    universe: std::collections::BTreeSet<String>,
    rules: idyll_styles::StyleTable,
}

/// What a dev rebuild changed — the two-stage hot reload's signal. `Styles` fires from
/// the fast track (the extracted style table changed: swap the browser's sheet in
/// place, no reload, live keep their state); `Reload` from the full track (the wasm
/// bytes actually changed — a style-only edit compiles values out of wasm, so
/// byte-identity proves no reload is needed and the full track ends silently).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Rebuild {
    Styles,
    Reload,
    /// Live whose chunk this rebuild changed — names, the cross-build
    /// identity (hashes are content identity, per build). The client reloads only
    /// if it LINKED one of them; an edit to a live the page never mounted moves
    /// nothing.
    Chunks(Vec<String>),
}

/// A server-side failure, answered honestly: **loud here, opaque there**.
///
/// `detail` is whatever the app's resolver returned — realistically a database error,
/// internal field or table names, a connection string. That is the operator's to read,
/// so it always reaches the log and reaches the caller only in dev.
fn fault(what: &str, detail: impl std::fmt::Display, dev: bool) -> Response {
    eprintln!("{what}: {detail}");
    let body = if dev {
        format!("{what}: {detail}")
    } else {
        format!("{what} failed")
    };
    (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
}

/// Why a render didn't produce a document — surfaced as an honest HTTP status, not HTML.
enum RenderError {
    /// The route query resolved to nothing for this path (`ExecError::Absent`) — the
    /// typed 404. Browsers probe `/favicon.ico` and the like; that's not a fault.
    NoRoute,
    /// A genuine fault (route execution failed, a contract field broke, a live
    /// mount trapped, …).
    Fault(String),
}

impl<Src: Clone + Send + Sync + 'static> Server<Src> {
    /// Validate the contract, publish the artifacts, build the live component (if
    /// any), stand up the router, and serve until shutdown. In dev mode this also
    /// watches the app's sources and hot-reloads on change.
    /// Stand up everything a request needs — schema + registry publish, executor,
    /// styles, the one cargo build, membrane engine, client bundle + manifest — and hand
    /// back the assembled [`AppState`]. `Ok(None)` is the `Mode::Build` outcome (bundle
    /// written, nothing to serve). Shared by [`serve`](Self::serve) and
    /// [`prerender`](Self::prerender): the two entry points differ only in what they do
    /// with the prepared state, never in how it is built.
    async fn prepare(self) -> anyhow::Result<Option<Prepared<Src>>> {
        // The one root object yields both halves of the execution surface.
        let idyll_data::AppRoot { schema, resolvers } = self.root;

        // The route contract is the boot gate: a drifted schema is a config error,
        // never a request-time surprise.
        let page = idyll_data::validate_route_contract(&schema)
            .map_err(|err| anyhow::anyhow!("route contract: {err}"))?;

        // Publish the contract first — the app build consumes it (the dev server is the
        // compiler driver: emit schema + registry → build app → serve). Only the two
        // authoring modes publish. `Mode::Prod` is handed a tree it did not write — a
        // deployment, or the prerender that builds the static site — so it reads the same
        // artifacts and refuses to boot on drift rather than editing the checkout under
        // itself. The boot gate is the same either way; what differs is whether a
        // mismatch is repaired or reported.
        let publishing = matches!(self.mode, Mode::Dev | Mode::Build);
        let stale = |what: &str| {
            format!("{what} does not match the app — run `dev` or `build` to republish it")
        };
        let schema_file = match &self.schema_path {
            Some(path) => path.clone(),
            None => self
                .manifest_dir
                .parent()
                .context("manifest_dir has no parent for the default schema path")?
                .join("schema.json"),
        };
        let schema_json = schema.to_json().map_err(|problem| anyhow::anyhow!(problem))?;
        if std::fs::read_to_string(&schema_file).ok().as_deref() != Some(schema_json.as_str()) {
            anyhow::ensure!(publishing, "{}", stale(&schema_file.display().to_string()));
            std::fs::write(&schema_file, &schema_json)
                .with_context(|| format!("writing schema artifact {}", schema_file.display()))?;
            println!(
                "schema published: {} ({})",
                schema_file.display(),
                schema.content_hash().map_err(|problem| anyhow::anyhow!(problem))?
            );
        }

        // The persisted-operation **registry**: the route query plus the mutation set,
        // written beside the schema (reviewable, self-verifying with `sha256sum`).
        // Stale artifacts are pruned — the checked-in directory IS the executable
        // surface, nothing more.
        let registry_dir = schema_file
            .parent()
            .context("schema path has no parent for the registry dir")?
            .join("ops");
        if publishing {
            std::fs::create_dir_all(&registry_dir)
                .with_context(|| format!("creating registry dir {}", registry_dir.display()))?;
        }
        let current: HashSet<String> = std::iter::once(self.route_query.filename.clone())
            .chain(self.mutations.iter().map(|f| f.filename.clone()))
            .collect();
        let listing = std::fs::read_dir(&registry_dir)
            .with_context(|| format!("reading registry dir {}", registry_dir.display()))?;
        for entry in listing {
            let entry = entry?.path();
            let name = entry.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if (name.ends_with(".query") || name.ends_with(".mutation")) && !current.contains(name)
            {
                anyhow::ensure!(publishing, "{}", stale(&format!("ops/{name}")));
                std::fs::remove_file(&entry)
                    .with_context(|| format!("pruning stale artifact {}", entry.display()))?;
                println!("registry pruned: {name}");
            }
        }
        let artifacts = std::iter::once((&self.route_query.filename, &self.route_query.contents))
            .chain(self.mutations.iter().map(|f| (&f.filename, &f.contents)));
        for (filename, contents) in artifacts {
            let path = registry_dir.join(filename);
            if std::fs::read_to_string(&path).ok().as_deref() != Some(contents.as_str()) {
                anyhow::ensure!(publishing, "{}", stale(&format!("ops/{filename}")));
                std::fs::write(&path, contents)
                    .with_context(|| format!("writing artifact {}", path.display()))?;
                println!("registry published: ops/{filename}");
            }
        }

        // Stand up the op executor: parse the persisted artifacts back from canonical
        // JSON and typecheck them against the schema NOW — drift is a boot failure.
        let route_hash;
        let executor = {
            let mut ops = HashMap::new();
            let op = idyll_data::CanonOp::from_canonical_json(&self.route_query.contents)
                .with_context(|| {
                    format!("route operation {} does not parse", self.route_query.filename)
                })?;
            idyll_data::validate(&schema, &op).map_err(|err| {
                anyhow::anyhow!(
                    "route operation {} does not typecheck against the schema: {err}",
                    self.route_query.filename
                )
            })?;
            // Typechecking sees the schema; this sees the resolver table. A root or a
            // `Ref` edge the app never registered would otherwise boot clean and 500 on
            // the first render that reaches it.
            idyll_data::validate_registered(&schema, &op, &resolvers).map_err(|err| {
                anyhow::anyhow!(
                    "route operation {} cannot execute: {err}",
                    self.route_query.filename
                )
            })?;
            route_hash = op.op_hash();
            ops.insert(route_hash, op);
            println!("route operation accepted: {route_hash}");

            let mut mutations = HashMap::new();
            for file in &self.mutations {
                let mutation = idyll_data::CanonMutation::from_canonical_json(&file.contents)
                    .with_context(|| format!("persisted mutation {} does not parse", file.filename))?;
                idyll_data::validate_mutation(&schema, &mutation).map_err(|err| {
                    anyhow::anyhow!(
                        "persisted mutation {} does not typecheck against the schema: {err}",
                        file.filename
                    )
                })?;
                if !resolvers.has_mutation(mutation.mutation_name()) {
                    anyhow::bail!(
                        "persisted mutation {} names handler `{}`, which is not registered on the resolvers",
                        file.filename,
                        mutation.mutation_name()
                    );
                }
                let hash = mutation.op_hash();
                if mutations.insert(hash, mutation).is_some() {
                    anyhow::bail!("mutation hash collision on {hash} — widen OpHash");
                }
                println!("persisted mutation accepted: {hash}");
            }
            Executor { schema, resolvers, ops, mutations }
        };

        let (reload_tx, _) = broadcast::channel(16);

        // The style table is **derived from source**: the extractor reads the same
        // `#[styles]` modules the attribute macro compiled, through the same parser.
        // Dev extracts at boot and re-extracts in the watcher's fast track; `build`
        // writes the `styles.json` artifact beside the client bundle; prod loads the
        // artifact (resolved below, once the bundle dir exists) so a deployment need
        // not carry source.
        let (style_roots, styles) = match self.mode {
            Mode::Dev | Mode::Build => {
                let roots = idyll_styles::extract::source_roots(&self.manifest_dir)
                    .context("locating the workspace's style sources")?;
                let table = idyll_styles::extract::extract(&roots)
                    .context("extracting the style table")?;
                let universe = idyll_styles::extract::universe(&roots, &self.app_crate)
                    .context("closing the live universe")?;
                (roots, Arc::new(RwLock::new(Styles { table, universe })))
            }
            Mode::Prod => (
                Vec::new(),
                Arc::new(RwLock::new(Styles { table: Default::default(), universe: Default::default() })),
            ),
        };

        // ONE cargo build stands up the whole app: the component the membrane mounts
        // pages and live through, and — jco-transpiled — the identical component
        // the browser claims live with.
        let dev = matches!(self.mode, Mode::Dev);
        let release = !dev;
        let mut build = AppBuild::discover(&self.app_crate, &self.manifest_dir)?;
        build.set_schema_path(schema_file.clone());
        println!(
            "building app component (wasm32-wasip2{})…",
            if release { ", release" } else { "" }
        );
        let wasm = build.build_component(release)?;
        if matches!(self.mode, Mode::Build) {
            let live = MembraneEngine::new(&wasm)?.live()?;
            let (published, transpiled) = build.ensure_client(&wasm, &live)?;
            if transpiled {
                println!("packaging client bundle (jco)…");
            } else {
                println!("client bundle already fresh");
            }
            write_styles_artifact(&build.client_dir(), &styles)?;
            report_optimized_sizes(&build.client_dir(), &published)?;
            println!("client bundle → {}", build.client_dir().display());
            return Ok(None);
        }
        let membrane = MembraneEngine::new(&wasm)?;
        let island_names = membrane.live().context("querying the app's mount table")?;
        let engine = Arc::new(RwLock::new(app_engine(membrane)?));
        let published = if dev {
            println!("packaging client bundle (jco)…");
            // Dev splits too (uncompressed): the in-process splitter needs no external tool,
            // so per-live chunks exist for the selective-reload gate to diff.
            build.build_client(&wasm, false, &island_names)?
        } else {
            let (published, transpiled) = build.ensure_client(&wasm, &island_names)?;
            if transpiled {
                println!("packaging client bundle (jco)…");
            } else {
                println!("client bundle fresh — skipping jco");
            }
            published
        };
        let client_dir = build.client_dir();
        let manifest = Arc::new(RwLock::new(published));
        if dev {
            // Dev only: watch every style-contributing crate. Two tracks per change —
            // fast re-extraction (browser sheet swap, no reload) and the wasm rebuild
            // (app-crate changes only; reload only when the bytes actually changed).
            spawn_watcher(
                build.clone(),
                wasm,
                engine.clone(),
                reload_tx.clone(),
                manifest.clone(),
                styles.clone(),
                style_roots.clone(),
            );
        }

        if matches!(self.mode, Mode::Prod) {
            *styles.write().expect("style table lock poisoned") =
                prod_styles(&client_dir, &self.manifest_dir, &self.app_crate)?;
        }

        anyhow::ensure!(
            self.assets_route.starts_with('/') && self.assets_route.len() > 1,
            "assets_route must be a non-root absolute path (got `{}`)",
            self.assets_route
        );
        let state = Arc::new(AppState {
            engine,
            data: self.data,
            executor,
            route_hash,
            page,
            manifest,
            assets_route: self.assets_route.clone(),
            budget: self.render_budget_ticks,
            reload: reload_tx,
            styles,
            dev,
        });
        Ok(Some(Prepared { state, client_dir, assets: self.assets, dev, port: self.port }))
    }

    /// Build the app and serve it over HTTP. `Mode::Build` builds the bundle and returns.
    pub async fn serve(self) -> anyhow::Result<()> {
        let Some(Prepared { state, client_dir, assets, dev, port }) = self.prepare().await? else {
            return Ok(());
        };
        let assets_route = state.assets_route.clone();

        // The content-addressed bundle: every name is a content hash, so the cache
        // lifetime is unbounded by construction; `.br` sidecars serve via negotiation.
        let assets_service = Router::new()
            .fallback_service(ServeDir::new(client_dir).precompressed_br())
            .layer(SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=31536000, immutable"),
            ));
        let mut app = Router::new()
            .fallback(get(page_endpoint::<Src>))
            .route("/__idyll/q/:hash", get(operation_endpoint::<Src>))
            .route("/__idyll/m/:hash", post(mutation_endpoint::<Src>))
            .nest_service(&assets_route, assets_service);
        if dev {
            app = app
                .route("/__idyll/reload", get(reload_sse::<Src>))
                .route("/__idyll/styles", get(styles_endpoint::<Src>));
        }
        if let Some(assets) = assets {
            app = app.nest_service("/static", ServeDir::new(assets));
        }
        let app = app.with_state(state);
        let app = if dev { app } else { app.layer(tower_http::compression::CompressionLayer::new()) };

        let addr = format!("0.0.0.0:{port}");
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        println!("idyll {} server on http://{addr}", if dev { "dev" } else { "prod" });
        axum::serve(listener, app).await?;
        Ok(())
    }
}

/// Everything a request needs, assembled — the shared output of [`Server::prepare`], consumed
/// by [`Server::serve`] and [`Server::prerender`] alike.
struct Prepared<Src: Clone + Send + Sync + 'static> {
    state: Arc<AppState<Src>>,
    client_dir: std::path::PathBuf,
    assets: Option<std::path::PathBuf>,
    dev: bool,
    port: u16,
}

/// The app's route **set**, for prerendering — which routes exist, computed from its data.
/// Async because enumerating may read the world (scan the content index, query a store).
/// Implemented on the [data source](Server::data); each route's URL is a projection
/// ([`idyll_route::Route::url`]), so this trait is only the *set*, never the URL shape.
pub trait Sitemap {
    /// The app's typed route identity.
    type Route: idyll_route::Route;
    /// Every route to prerender, in any order.
    #[allow(async_fn_in_trait)]
    async fn routes(&self) -> Vec<Self::Route>;
}

impl<Src: Clone + Send + Sync + 'static + Sitemap> Server<Src> {
    /// Prerender every route the [`Sitemap`] names into `out` as static files: run the same
    /// SSR the server does, only early and to disk. `<out>/<url>.html` per page, the client
    /// bundle under the assets route, the app's `/static` alongside, and a `manifest.json`
    /// index. The result is a self-contained folder any static host serves — no running idyll.
    pub async fn prerender(self, out: &std::path::Path, standalone: bool) -> anyhow::Result<()> {
        if out.exists() {
            anyhow::ensure!(std::fs::read_dir(out)?.next().is_none(), "prerender output must be empty: {}", out.display());
        }
        let routes = self.data.routes().await;
        let destinations = prerender_destinations(&routes)?;
        let Prepared { state, client_dir, assets, .. } = self
            .prepare()
            .await?
            .context("prerender needs a servable mode, not `Mode::Build`")?;
        let assets_route = state.assets_route.trim_start_matches('/').to_string();

        std::fs::create_dir_all(out)?;
        // `--standalone` inlines every asset as a `data:` URL (the JS graph rewritten to match), so
        // each page is a self-contained file — no fetches, works from `file://`. Otherwise assets
        // ride at the same paths the HTML references: the content-addressed bundle under the
        // assets route, the app's `/static` (fonts, favicon) alongside.
        let embed = if standalone {
            Some(build_embed(&client_dir, &state.assets_route, assets.as_deref())?)
        } else {
            copy_tree(&client_dir, &out.join(&assets_route))?;
            if let Some(static_dir) = &assets {
                copy_tree(static_dir, &out.join("static"))?;
            }
            None
        };

        // The route set is typed values; each projects to its URL, which drives both the
        // render and the output file. Parse-don't-validate: no URL strings enumerated here.
        println!(
            "prerendering {} routes → {}{}",
            routes.len(),
            out.display(),
            if standalone { " (standalone)" } else { "" }
        );
        let mut pages = Vec::new();
        for (route, file) in routes.iter().zip(destinations) {
            let url = route.url();
            let faults = Arc::new(std::sync::Mutex::new(Vec::new()));
            let body = match render_path(&state, url.as_str(), Some(faults.clone())).await {
                Ok(body) => body,
                Err(err) => {
                    let why = match err {
                        RenderError::NoRoute => "no such route".to_string(),
                        RenderError::Fault(message) => message,
                    };
                    anyhow::bail!("prerender {url}: {why}");
                }
            };
            let bytes = axum::body::to_bytes(body, usize::MAX)
                .await
                .with_context(|| format!("collecting {url}"))?;
            let mut html = String::from_utf8(bytes.to_vec()).with_context(|| format!("{url} is not UTF-8"))?;
            let faults = faults.lock().expect("render faults lock poisoned");
            anyhow::ensure!(faults.is_empty(), "prerender {url}: {}", faults.join("; "));
            anyhow::ensure!(html.ends_with("</body></html>"), "prerender {url}: incomplete document");
            if let Some(embed) = &embed {
                html = embed.inline(html);
            }
            let dest = out.join(&file);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, html.as_bytes())?;
            if !standalone {
                std::fs::write(dest.with_extension("html.br"), build::brotli_bytes(html.as_bytes()))?;
            }
            pages.push((url.into_string(), file.to_string_lossy().into_owned()));
        }

        let manifest = pages
            .iter()
            .map(|(url, file)| format!("  {{ \"url\": {}, \"file\": {} }}", json_str(url), json_str(file)))
            .collect::<Vec<_>>()
            .join(",\n");
        std::fs::write(out.join("manifest.json"), format!("[\n{manifest}\n]\n"))?;
        println!("prerendered {} pages", pages.len());
        Ok(())
    }
}

fn prerender_destinations<R: idyll_route::Route>(routes: &[R]) -> anyhow::Result<Vec<PathBuf>> {
    anyhow::ensure!(!routes.is_empty(), "prerender route set is empty");
    let mut destinations = HashSet::new();
    routes.iter().map(|route| {
        let url = route.url();
        for segment in idyll_route::split_path(url.as_str()) {
            let decoded = idyll_route::decode_segment(segment).context("route is not UTF-8")?;
            anyhow::ensure!(
                !decoded.is_empty() && decoded != "." && decoded != ".."
                    && !decoded.contains(['/', '\\', ':', '\0'])
                    && !decoded.ends_with(['.', ' ']),
                "route cannot be represented as a file: {url}"
            );
        }
        let file = url_to_file(url.as_str());
        anyhow::ensure!(
            destinations.insert(file.to_string_lossy().to_lowercase()),
            "duplicate prerender destination: {}", file.display()
        );
        Ok(file)
    }).collect()
}

/// Map a URL path to its output file: `/` → `index.html`, `/docs/a/b` → `docs/a/b.html`.
/// Segments are percent-decoded, so the file name matches what a static server resolves a
/// request path to.
fn url_to_file(url: &str) -> std::path::PathBuf {
    let segs: Vec<String> = idyll_route::split_path(url)
        .into_iter()
        .map(|s| idyll_route::decode_segment(s).unwrap_or_else(|| s.to_string()))
        .collect();
    if segs.is_empty() {
        "index.html".into()
    } else {
        format!("{}.html", segs.join("/")).into()
    }
}

/// Recursively copy a directory tree. A missing source is a no-op.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    if !from.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Minimal JSON string quoting for the manifest (urls and file paths).
fn json_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The inlined-asset table for `--standalone`: every asset URL → its `data:` URL. Non-JS assets
/// embed directly; the fonts stylesheet inlines its woff2 first; JS embeds last, in dependency
/// order, with its `./x.js` imports and `new URL('./x.wasm')` refs rewritten to the data URLs
/// already computed — so an embedded module resolves entirely from within the page.
struct Embed {
    data_urls: std::collections::HashMap<String, String>,
}

impl Embed {
    /// Replace every `"<asset-url>"` in the document with its `data:` URL — the module script's
    /// `src`/`data-app`, `<link>` hrefs, the fonts stylesheet, the favicon, and the chunk map.
    /// An asset URL always appears double-quoted (an attribute or a JSON string), so one quoted
    /// replace per asset covers every site.
    fn inline(&self, mut html: String) -> String {
        for (url, data) in &self.data_urls {
            html = html.replace(&format!("\"{url}\""), &format!("\"{data}\""));
        }
        html
    }
}

fn data_url(mime: &str, bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn asset_mime(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("wasm") => "application/wasm",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("woff2") => "font/woff2",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// Build the [`Embed`] table from the built assets: the content-addressed client bundle under
/// `assets_route`, plus the app's `/static` tree.
fn build_embed(
    client_dir: &std::path::Path,
    assets_route: &str,
    static_dir: Option<&std::path::Path>,
) -> anyhow::Result<Embed> {
    // Every asset as `url-path -> bytes`. The bundle's files sit at the top of `client_dir`;
    // its `stage/`/`split/` subdirs are build scratch, and `.br` are brotli sidecars.
    let mut raw: std::collections::HashMap<String, Vec<u8>> = std::collections::HashMap::new();
    for entry in std::fs::read_dir(client_dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !entry.file_type()?.is_file() || name.ends_with(".br") {
            continue;
        }
        raw.insert(format!("{assets_route}/{name}"), std::fs::read(entry.path())?);
    }
    if let Some(dir) = static_dir {
        collect_tree(dir, "/static", &mut raw)?;
    }

    let mut data_urls: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    // Non-JS, non-CSS first — a woff2 must have a data URL before the stylesheet inlines it.
    for (path, bytes) in &raw {
        if !path.ends_with(".js") && !path.ends_with(".css") {
            data_urls.insert(path.clone(), data_url(asset_mime(path), bytes));
        }
    }
    // CSS: rewrite its asset `url(...)` refs to the data URLs just built, then embed.
    for (path, bytes) in &raw {
        if path.ends_with(".css") {
            let mut css = String::from_utf8_lossy(bytes).into_owned();
            for (asset, data) in &data_urls {
                css = css.replace(asset, data);
            }
            data_urls.insert(path.clone(), data_url("text/css", css.as_bytes()));
        }
    }
    // JS: dependency order — a module's `./x.js` imports must already be data URLs.
    let mut pending: std::collections::HashSet<String> =
        raw.keys().filter(|p| p.ends_with(".js")).cloned().collect();
    while !pending.is_empty() {
        let ready: Vec<String> = pending
            .iter()
            .filter(|p| js_module_deps(&raw[*p], assets_route).iter().all(|d| !pending.contains(d)))
            .cloned()
            .collect();
        anyhow::ensure!(!ready.is_empty(), "unresolved or cyclic JS imports in the bundle");
        for path in ready {
            let src = String::from_utf8_lossy(&raw[&path]).into_owned();
            let rewritten = rewrite_js_refs(&src, assets_route, &data_urls);
            data_urls.insert(path.clone(), data_url("text/javascript", rewritten.as_bytes()));
            pending.remove(&path);
        }
    }
    Ok(Embed { data_urls })
}

/// The `assets_route`-relative JS modules a module statically imports (`from './x.js'`) — its
/// dependency edges for the embed ordering. Wasm refs are leaves, embedded up front.
fn js_module_deps(src: &[u8], assets_route: &str) -> Vec<String> {
    let src = String::from_utf8_lossy(src);
    src.split("from '")
        .skip(1)
        .filter_map(|token| token.split('\'').next())
        .filter_map(|rel| rel.strip_prefix("./"))
        .filter(|name| name.ends_with(".js"))
        .map(|name| format!("{assets_route}/{name}"))
        .collect()
}

/// Rewrite a JS module's asset references to data URLs: `from './x.js'` → that module's data
/// URL, `new URL('./x.wasm', import.meta.url)` → the wasm's data URL — the exact forms jco
/// emits. Only assets under `assets_route` (not, say, a `'./wasi-shim.js'` in a comment).
fn rewrite_js_refs(
    src: &str,
    assets_route: &str,
    data_urls: &std::collections::HashMap<String, String>,
) -> String {
    let mut out = src.to_string();
    for (path, data) in data_urls {
        if !path.starts_with(assets_route) {
            continue;
        }
        let name = path.rsplit('/').next().unwrap_or(path);
        if path.ends_with(".js") {
            out = out.replace(&format!("'./{name}'"), &format!("'{data}'"));
        } else if path.ends_with(".wasm") {
            out = out.replace(&format!("new URL('./{name}', import.meta.url)"), &format!("'{data}'"));
        }
    }
    out
}

/// Collect a directory tree as `<prefix>/<relpath> -> bytes`.
fn collect_tree(
    dir: &std::path::Path,
    prefix: &str,
    into: &mut std::collections::HashMap<String, Vec<u8>>,
) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let url = format!("{prefix}/{}", entry.file_name().to_string_lossy());
        if entry.file_type()?.is_dir() {
            collect_tree(&entry.path(), &url, into)?;
        } else {
            into.insert(url, std::fs::read(entry.path())?);
        }
    }
    Ok(())
}

/// Produce the document **stream** for `path`. The route query and the page contract
/// run before the first byte, so absence is still an honest 404 and contract drift an
/// honest 500. Then the document streams: the prelude (title, load plan, runtime tag,
/// the app's head mount) goes out first, the **page mount**'s paint follows as it
/// folds — live nested in the paint recurse through the same machinery — and the
/// seed rides last (nothing in paint reads it, and as a classic inline script it
/// still executes before the deferred runtime module).
///
/// Once bytes have flushed there is no error page left to send, so from that point
/// every mount fault — blown budget or trap — bounds its damage to its own region
/// (the page: an unpainted body; a live: one unpainted wrapper), loud on the
/// server, and the browser mount still runs.
async fn render_path<Src: Clone + Send + Sync + 'static>(
    state: &AppState<Src>,
    path: &str,
    faults: Option<Arc<std::sync::Mutex<Vec<String>>>>,
) -> Result<axum::body::Body, RenderError> {
    let vars = serde_json::json!({ "request": { "path": path } });
    let executed = match state.executor.execute(&state.data, state.route_hash, &vars).await {
        Some(Ok(executed)) => executed,
        // The route resolver said `None`: content that isn't there — the typed 404.
        Some(Err(idyll_data::ExecError::Absent { .. })) => return Err(RenderError::NoRoute),
        Some(Err(err)) => return Err(RenderError::Fault(format!("route query failed: {err}"))),
        None => return Err(RenderError::Fault("route operation missing from executor".into())),
    };
    let title =
        idyll_data::page_title(&executed, &state.page, path)
            .map_err(|err| RenderError::Fault(err.to_string()))?;

    let sheet = {
        let styles = state.styles.read().expect("style table lock poisoned");
        document_sheet(&styles)
    };
    // The seed the browser gets is the same bytes every mount consumes — the claim is
    // against identical input.
    let seed = executed.to_preloaded_json();
    // One unguessable seed for the whole request: every SSR mount draws from it, and
    // it rides the page so the browser hydrates against the identical randomness.
    let insecure_seed = request_seed();

    let engine = state.engine.clone();
    let manifest = state.manifest.clone();
    let assets_route = state.assets_route.clone();
    let dev = state.dev;
    let budget = state.budget;

    // Mounts are synchronous wasm work: produce on a blocking thread, let the channel
    // be the wire. Unbounded is right — production is bounded by the page itself. A
    // send failing means the client went away; stop producing.
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    // The document's status is the **page mount's** to decide, and it is decided before
    // the first byte: the page and the head are materialized whole before the prelude
    // goes out anyway (only live stream), so a page that cannot paint is still an
    // honest 500 rather than a 200 carrying an empty body. It has to be this task that
    // says so — one engine lock for the whole document, so a dev hot-swap cannot land
    // between deciding and painting and mix two builds on one page.
    let (status_tx, status_rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    tokio::task::spawn_blocking(move || {
        let send = |chunk: String| tx.send(Ok::<_, Infallible>(Bytes::from(chunk))).is_ok();
        let engine = engine.read().expect("engine lock poisoned");
        let mut stream = MountStream {
            send: &send,
            engine: &engine.membrane,
            seed: &seed,
            insecure_seed,
            budget,
            counters: HashMap::new(),
            faults,
        };

        // The app's head content — mounted before the prelude flushes, because it
        // rides inside `<head>`. Optional: an app without a `head` entry contributes
        // nothing beyond the framework envelope.
        let head = if engine.has_head {
            match stream.paint_html("head") {
                Ok(head) => head,
                Err(reason) => {
                    let _ = status_tx.send(Err(reason));
                    return;
                }
            }
        } else {
            String::new()
        };

        // The page's own paint, materialized before the prelude: the document's load
        // plan derives from what actually painted — a page whose data put no live
        // on it ships zero framework bytes. The page is content (static chrome), so
        // holding it is cheap; the live mounts below still stream.
        // The page is the document root, not a live wrapper, so its own static-ness is
        // moot — only the live it declares are wrapped and claimed.
        let page = match stream.segments_of("page", None, 0) {
            Ok((page, _)) => page,
            Err(reason) => {
                let _ = status_tx.send(Err(reason));
                return;
            }
        };
        let has_islands =
            page.iter().any(|s| matches!(s, idyll::BodySegment::Live { .. }));

        // Past here the response is committed, so every remaining fault bounds its
        // damage to its own live region (see `MountStream::mount`).
        if status_tx.send(Ok(())).is_err() {
            return; // the client went away before we answered
        }

        let (prelude, chunks_json) = {
            let manifest = manifest.read().expect("manifest lock poisoned");
            (
                document_prelude(&title, &head, &sheet, &manifest, &assets_route, dev, has_islands),
                chunks_script(&manifest.app.chunks, &assets_route),
            )
        };
        if !send(prelude) {
            return;
        }
        for segment in page {
            match segment {
                idyll::BodySegment::Html(html) => {
                    if !send(html.as_str().to_owned()) {
                        return;
                    }
                }
                idyll::BodySegment::Live { name, key, fallback, .. } => {
                    if !stream.mount(&name, key.as_deref(), &fallback, 1) {
                        return;
                    }
                }
            }
        }
        send(document_tail(has_islands.then_some((seed.as_slice(), insecure_seed, chunks_json.as_str()))));
    });

    match status_rx.await {
        Ok(Ok(())) => Ok(axum::body::Body::from_stream(UnboundedReceiverStream::new(rx))),
        Ok(Err(reason)) => Err(RenderError::Fault(reason)),
        // The task ended without answering: it panicked, which `spawn_blocking`
        // swallows into a JoinError we never see here.
        Err(_) => Err(RenderError::Fault("the render task ended without painting".into())),
    }
}

/// An unguessable per-request random seed. `std`'s `RandomState` hashes with keys the
/// OS seeds per thread; run through two independent hashers (salted by a per-request
/// counter so successive requests differ) it yields 128 unpredictable bits — enough to
/// defeat HashMap-flooding, and the one nondeterministic input the whole render is a
/// pure function of. Dependency-free: SipHash with a secret key is a PRF.
fn request_seed() -> u128 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let word = || {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
        hasher.finish()
    };
    ((word() as u128) << 64) | word() as u128
}

/// One document's mount pipeline: the engine, the request's seeds, and the page-wide
/// per-name instance counters. Mount identity is (name, per-name occurrence in
/// DOCUMENT order) — one counter map, bumped depth-first at each splice point,
/// because a mount's paint can itself declare live and the browser counts them
/// all in one document scan.
struct MountStream<'a> {
    send: &'a dyn Fn(String) -> bool,
    engine: &'a MembraneEngine,
    seed: &'a [u8],
    insecure_seed: u128,
    budget: u64,
    counters: HashMap<String, u32>,
    faults: Option<Arc<std::sync::Mutex<Vec<String>>>>,
}

impl MountStream<'_> {
    /// Paint one entry and fold it to body segments — HTML runs and live splice
    /// points.
    fn segments_of(
        &mut self,
        name: &str,
        key: Option<&str>,
        depth: u8,
    ) -> Result<(Vec<idyll::BodySegment>, bool), String> {
        let (commands, static_paint) = self.paint(name, key, depth)?;
        let mut fold = idyll::HtmlFold::new();
        for command in &commands {
            fold.apply(command);
        }
        Ok((fold.segments(), static_paint))
    }

    /// Mount one **live** and stream its paint, recursing into live the paint itself
    /// declares (the page's markers). Returns false when the client has gone away.
    ///
    /// A live that cannot paint ships its one wrapper carrying the marker's fallback,
    /// loud on the server: the status went out with the prelude, so there is no error
    /// page left to send and a fallback is the only honest thing that fits. That is the
    /// whole reason [`paint`](Self::paint) hands the reason back instead of deciding —
    /// the **page** takes the same failure and answers 500, because nothing of its
    /// response has gone out yet.
    fn mount(&mut self, name: &str, key: Option<&str>, fallback: &idyll::Html, depth: u8) -> bool {
        // This layer owns the wrapper (not the parent's fold): only here, where the mount
        // ran, is the paint's static-ness known. A paint that failed ships a
        // non-static wrapper — loud on the server, still mounted by the browser.
        let (segments, static_paint) = match self.segments_of(name, key, depth) {
            Ok(painted) => painted,
            Err(reason) => {
                if let Some(faults) = &self.faults {
                    faults.lock().expect("render faults lock poisoned").push(reason.clone());
                }
                eprintln!("{reason} — shipping the region's fallback");
                return (self.send)(idyll::live_wrapper_open(name, key, false))
                    && (self.send)(fallback.as_str().to_owned())
                    && (self.send)(idyll::LIVE_WRAPPER_CLOSE.to_owned());
            }
        };
        if !(self.send)(idyll::live_wrapper_open(name, key, static_paint)) {
            return false;
        }
        for segment in segments {
            match segment {
                idyll::BodySegment::Html(html) => {
                    if !(self.send)(html.as_str().to_owned()) {
                        return false;
                    }
                }
                idyll::BodySegment::Live { name: child, key: child_key, fallback, .. } => {
                    if !self.mount(&child, child_key.as_deref(), &fallback, depth + 1) {
                        return false;
                    }
                }
            }
        }
        (self.send)(idyll::LIVE_WRAPPER_CLOSE.to_owned())
    }

    /// Mount one entry and return its paint as one HTML string — the head's shape
    /// (it rides inside the prelude, and `<head>` content has no live to splice;
    /// a marker there is a loud app error, shipped as nothing).
    fn paint_html(&mut self, name: &str) -> Result<String, String> {
        let (commands, _) = self.paint(name, None, 0)?;
        let mut fold = idyll::HtmlFold::new();
        for command in &commands {
            fold.apply(command);
        }
        let mut html = String::new();
        for segment in fold.segments() {
            match segment {
                idyll::BodySegment::Html(chunk) => html.push_str(chunk.as_str()),
                idyll::BodySegment::Live { name: child, .. } => {
                    if let Some(faults) = &self.faults {
                        faults.lock().expect("render faults lock poisoned")
                            .push(format!("`{name}` declares live `{child}` in head content"));
                    }
                    eprintln!("`{name}` declares live `{child}` — head content cannot splice mounts");
                }
            }
        }
        Ok(html)
    }

    /// One membrane mount. `Err` is **why it could not paint**, returned rather than
    /// printed, because what that means depends on which mount it was: the page decides
    /// the response and can still be an honest 500, while a live is one region of a
    /// document whose status is already sent. Only the caller knows which it is.
    fn paint(
        &mut self,
        name: &str,
        key: Option<&str>,
        depth: u8,
    ) -> Result<(Vec<idyll::DomCommand>, bool), String> {
        let instance = {
            let counter = self.counters.entry(name.to_string()).or_insert(0);
            let instance = *counter;
            *counter += 1;
            instance
        };
        // A cycle (a mount whose paint declares itself) would recurse forever.
        if depth >= 8 {
            return Err(format!("mount `{name}`#{instance} nests deeper than 8"));
        }
        match self.engine.mount(name, instance, key, self.seed, self.insecure_seed, self.budget) {
            Ok(MountOutcome::Commands { commands, static_paint }) => Ok((commands, static_paint)),
            Ok(MountOutcome::BlewBudget) => {
                Err(format!("mount `{name}`#{instance} blew its SSR budget"))
            }
            Ok(MountOutcome::Failed(message)) => {
                Err(format!("mount `{name}`#{instance} declined: {message}"))
            }
            Err(err) => Err(format!("mount `{name}`#{instance} trapped: {err}")),
        }
    }
}

/// The document's stylesheet text: the **live universe**'s rules — every rule the
/// app's components could reference, provisioned ahead of any paint. With all views
/// in the app crate there is nothing else to collect: the page mount, its live,
/// and every lazy branch any of them could materialize client-side draw from the
/// same universe (the app crate's workspace link closure). StyleX's one-sheet model,
/// scoped to the app.
fn document_sheet(styles: &Styles) -> String {
    let mut sheet = String::new();
    for rule in route_rules(styles) {
        // `</` never occurs in table-generated text (values are typed; the one
        // free-form value, a font stack, is a CSS string where `<\/` reads back
        // identically) — the escape only guards the inline `<style>` element from an
        // early close.
        sheet.push_str(&rule.css.expect("route_rules resolves every rule").replace("</", "<\\/"));
        sheet.push('\n');
    }
    sheet
}

/// The universe's resolved rule set (every `css` present). The dev style endpoint
/// serves exactly this list, so the browser's swapped sheet and a fresh document's
/// inline sheet are the same rules from the same join. (A guest template can reference a rule the table doesn't know only
/// through build/source skew — a stale artifact naming a declaration the source
/// renamed; the next rebuild heals it, and the missing rule simply isn't in the
/// sheet.)
fn route_rules(styles: &Styles) -> Vec<idyll::StyleRule> {
    styles
        .table
        .scoped(&styles.universe)
        .map(|(name, css)| idyll::StyleRule {
            name: name.to_string().into(),
            css: Some(css.to_string().into()),
        })
        .collect()
}

/// The document's first flush: everything before the body's content. The **host**
/// owns the envelope (`<!doctype>`, `<html>`, `<head>`, `<body>`) and the `<title>`
/// (a contract field, not markup the app builds). No string surgery on folded output
/// — the parts compose by construction. (The one exception is the standalone
/// prerender's asset inlining, which rewrites quoted URLs over the collected
/// document — see `Embed::inline`.)
///
/// The head is the load plan, and it derives from the paint: a markerless page names
/// NOTHING — no runtime, no app, no capture, no guard; its links are native
/// navigation (dev adds only the runtime module, for the reload client). A live
/// page's document is the one dynamic response; everything else it names is immutable
/// and content-addressed, so the head declares the full set up front — runtime, glue,
/// shims, wasm — and the browser fetches it as one flat wave while the body streams.
/// The delegated event surface — the event types the browser runtime routes to
/// live (its `DELEGATED` list in `runtime.js`; a test pins the two equal). The
/// prelude's capture snippet listens for exactly this set so input landing between
/// first paint and the runtime module executing is queued, not dropped.
const DELEGATED_EVENTS: &[&str] =
    &["click", "input", "change", "keydown", "keyup", "submit", "blur", "focus"];

/// The inline early-capture snippet: queue `(target, type, value, key)` records into
/// `window.__IDYLL_Q` from first paint; the runtime adopts the queue at import and
/// retires these listeners via `window.__IDYLL_Q_OFF` — one capture pipeline from
/// then on. Deliberately dumb: record and stand down, no routing logic (that would
/// be a second runtime to keep honest).
fn capture_snippet() -> String {
    let types = DELEGATED_EVENTS
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "<script>(()=>{{const q=window.__IDYLL_Q=[];\
         const h=e=>{{q.push({{target:e.target,type:e.type,\
         targetValue:e.target&&'value'in e.target?String(e.target.value):void 0,\
         key:e.key}})}};const ts=[{types}];for(const t of ts)addEventListener(t,h,true);\
         window.__IDYLL_Q_OFF=()=>{{for(const t of ts)removeEventListener(t,h,true)}}}})()</script>"
    )
}

fn document_prelude(
    title: &str,
    head: &str,
    sheet: &str,
    manifest: &AssetManifest,
    assets_route: &str,
    dev: bool,
    has_islands: bool,
) -> String {
    use std::fmt::Write as _;

    let title = html_escape::encode_text(title);
    let style = if sheet.is_empty() {
        String::new()
    } else {
        format!("<style data-idyll>{sheet}</style>")
    };

    let runtime_url = format!("{assets_route}/{}", manifest.runtime);

    // The load plan derives from the paint: a page whose data put no live on it
    // ships **zero framework bytes** — no runtime, no capture, no skew guard, no
    // hints, no app module; its links are native navigation. Dev keeps the runtime
    // module everywhere (the reload client lives in it), and nothing else.
    if !has_islands {
        let runtime_tag = if dev {
            format!(
                "<script type=\"module\" src=\"{runtime_url}\"></script>\
                 <script>window.__IDYLL_DEV__=1</script>"
            )
        } else {
            String::new()
        };
        return format!(
            "<!doctype html><html lang=\"en\"><head><title>{title}</title>{runtime_tag}\
             {style}{head}</head><body><idyll-root style=\"display:contents\">"
        );
    }

    // Early input capture: the page's live are live mounts, so early events
    // always have a consumer to replay into.
    let capture = capture_snippet();

    // The skew guard, installed before any asset tag: a deploy may sweep files a
    // stale document still names, so any asset load error under the assets route
    // reloads the page — the fresh document IS the re-sync. The sessionStorage
    // timestamp rate-limits recovery to one reload per 10s, so a genuinely broken
    // deploy can't loop; the terminal state is a served SSR page with dead live.
    // (The runtime applies the same guard to the lazy app import, which rejects as a
    // promise instead of firing a resource error event.)
    let skew_guard = format!(
        "<script>addEventListener('error',e=>{{const u=e.target&&(e.target.src||e.target.href);\
         if(typeof u=='string'&&u.includes('{assets_route}/')){{const t=Date.now();\
         if(t-(sessionStorage.getItem('idyll-skew')||0)>1e4){{\
         sessionStorage.setItem('idyll-skew',t);location.reload()}}}}}},true)</script>"
    );

    // The live pages' load plan: everything the mounts need, hinted up front.
    let app = &manifest.app;
    let mut hints = format!("<link rel=\"modulepreload\" href=\"{runtime_url}\">");
    write!(hints, "<link rel=\"modulepreload\" href=\"{assets_route}/{}\">", app.js).unwrap();
    for shim in &app.shims {
        write!(hints, "<link rel=\"modulepreload\" href=\"{assets_route}/{shim}\">").unwrap();
    }
    for wasm in &app.wasm {
        // `crossorigin` matches the glue's plain same-origin `fetch()` credentials
        // mode — a mismatched preload downloads twice.
        write!(
            hints,
            "<link rel=\"preload\" as=\"fetch\" type=\"application/wasm\" \
             href=\"{assets_route}/{wasm}\" crossorigin>"
        )
        .unwrap();
    }

    let data_app = format!(" data-app=\"{assets_route}/{}\"", app.js);

    // The dev client lives in runtime.js (it needs the constructed sheet for the
    // style hot-swap); this marker is all the document contributes.
    let reload_client = if dev {
        "<script>window.__IDYLL_DEV__=1</script>"
    } else {
        ""
    };

    format!(
        "<!doctype html><html lang=\"en\"><head><title>{title}</title>{skew_guard}{capture}{hints}\
         <script type=\"module\" src=\"{runtime_url}\"{data_app}></script>{reload_client}{style}{head}</head>\
         <body><idyll-root style=\"display:contents\">"
    )
}

/// The document's last flush: close the root, ship the data (live pages only),
/// close the envelope.
///
/// The seed is what the live mounts consume — `<` → `<` so it can't break out
/// of its `<script>` (JSON `<` only appears inside string values, where the escape is
/// transparent). Data rides last by design: nothing in paint reads it, and as a
/// classic inline script it still executes before the deferred runtime module.
/// The random seed is
/// `[low, high]` hex, matching the host's `insecure-seed` split
/// `(seed as u64, (seed >> 64) as u64)`. A markerless page ships no data at all.
fn document_tail(data: Option<(&[u8], u128, &str)>) -> String {
    let Some((seed, insecure_seed, chunks)) = data else {
        return "</idyll-root></body></html>".to_string();
    };
    let seed_json = String::from_utf8_lossy(seed).replace('<', "\\u003c");
    let low = insecure_seed as u64;
    let high = (insecure_seed >> 64) as u64;
    format!(
        "</idyll-root>\
         <script>window.__IDYLL_SEED__={seed_json};\
         window.__IDYLL_RAND=[\"{low:016x}\",\"{high:016x}\"];\
         window.__IDYLL_CHUNKS__={chunks};</script>\
         </body></html>",
    )
}

/// The chunk map a live page hands its runtime: live → chunk URLs, each an
/// ordinary immutable asset the mount streams and links first. `null` when nothing
/// split — every live lives in the primary.
fn chunks_script(chunks: &chunks::ChunkManifest, assets_route: &str) -> String {
    if chunks.live.values().all(|files| files.is_empty()) {
        return "null".to_string();
    }
    let live: std::collections::BTreeMap<&str, Vec<String>> = chunks
        .live
        .iter()
        .map(|(name, files)| {
            (
                name.as_str(),
                files.iter().map(|f| format!("{assets_route}/{f}")).collect(),
            )
        })
        .collect();
    serde_json::json!({ "live": live }).to_string()
}

/// Serve any path: execute the route query, stream the document. Absence is an
/// honest 404 (the stream only starts after the route resolves).
async fn page_endpoint<Src: Clone + Send + Sync + 'static>(
    uri: axum::http::Uri,
    State(state): State<Arc<AppState<Src>>>,
) -> Response {
    match render_path(&state, uri.path(), None).await {
        Ok(body) => Response::builder()
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(body)
            .expect("static response parts are valid"),
        Err(RenderError::NoRoute) => (StatusCode::NOT_FOUND, "no such page").into_response(),
        Err(RenderError::Fault(message)) => fault("render", message, state.dev),
    }
}

/// The operation endpoint's query string: `?vars=<url-encoded JSON object>`; absent
/// means no variables.
#[derive(serde::Deserialize)]
struct OperationQuery {
    vars: Option<String>,
}

/// The **operation endpoint** — the one data transport: `GET /__idyll/q/<hash>?vars=…`
/// executes a single boot-validated artifact and returns its `Preloaded` payload
/// (an SPA live's navigation refires the route query here). The parse to
/// [`OpHash`](idyll_data::OpHash) is the boundary and the boot-validated set is the
/// allowlist — nothing of a client's choosing ever executes.
async fn operation_endpoint<Src: Clone + Send + Sync + 'static>(
    Path(hash): Path<String>,
    axum::extract::Query(query): axum::extract::Query<OperationQuery>,
    State(state): State<Arc<AppState<Src>>>,
) -> Response {
    let Ok(hash) = hash.parse::<idyll_data::OpHash>() else {
        return (StatusCode::BAD_REQUEST, "malformed operation hash").into_response();
    };
    let vars = match &query.vars {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(vars) => match serde_json::from_str(vars) {
            Ok(vars) => vars,
            Err(err) => {
                return (StatusCode::BAD_REQUEST, format!("malformed vars: {err}")).into_response()
            }
        },
    };
    match state.executor.execute(&state.data, hash, &vars).await {
        None => (StatusCode::NOT_FOUND, "no such operation").into_response(),
        Some(Ok(executed)) => {
            let payload = executed.to_preloaded_json();
            ([(header::CONTENT_TYPE, "application/json")], payload).into_response()
        }
        Some(Err(idyll_data::ExecError::Absent { root })) => {
            (StatusCode::NOT_FOUND, format!("no `{root}` for these variables")).into_response()
        }
        Some(Err(err)) => fault("operation", err, state.dev),
    }
}

/// The **mutation endpoint**: `POST /__idyll/m/<hash>` executes one boot-validated
/// mutation artifact with the POSTed variables. The response is an **envelope**:
/// the handler's node masked to the artifact's recorded selection (`outcome`), plus
/// — when the client says which page it is on (`x-idyll-path`) — the route query
/// re-executed against the post-mutation data (`seed`). One round trip makes every
/// store projection consistent: the client replays the seed into its context cache,
/// where value-equality absorption reduces it to a delta. RSC-style refresh, riding the
/// mutation's own response.
async fn mutation_endpoint<Src: Clone + Send + Sync + 'static>(
    Path(hash): Path<String>,
    State(state): State<Arc<AppState<Src>>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(hash) = hash.parse::<idyll_data::OpHash>() else {
        return (StatusCode::BAD_REQUEST, "malformed operation hash").into_response();
    };
    let Some(mutation) = state.executor.mutations.get(&hash) else {
        return (StatusCode::NOT_FOUND, "no such mutation").into_response();
    };
    let vars: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(vars) => vars,
        Err(err) => {
            return (StatusCode::BAD_REQUEST, format!("malformed vars: {err}")).into_response()
        }
    };
    let masked = match idyll_data::execute_mutation(
        &state.executor.schema,
        mutation,
        &state.executor.resolvers,
        &state.data,
        &vars,
    )
    .await
    {
        Ok(masked) => masked,
        Err(err) => return fault("mutation", err, state.dev),
    };

    // The refresh seed: the same persisted route query the page loaded from, against
    // the world the mutation just changed. A failure here degrades to outcome-only —
    // the mutation itself succeeded, and the client's floor is refetch-on-navigation.
    let seed = match headers.get("x-idyll-path").and_then(|p| p.to_str().ok()) {
        None => serde_json::Value::Null,
        Some(path) => {
            let vars = serde_json::json!({ "request": { "path": path } });
            match state.executor.execute(&state.data, state.route_hash, &vars).await {
                Some(Ok(executed)) => serde_json::from_slice(&executed.to_preloaded_json())
                    .expect("preloaded json parses"),
                _ => serde_json::Value::Null,
            }
        }
    };
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&serde_json::json!({ "outcome": masked, "seed": seed }))
            .expect("mutation envelope serializes"),
    )
        .into_response()
}

/// Dev live-reload channel: an SSE stream carrying what each rebuild changed —
/// `styles` (swap the sheet in place) or `reload` (the wasm changed).
async fn reload_sse<Src: Clone + Send + Sync + 'static>(
    State(state): State<Arc<AppState<Src>>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.reload.subscribe();
    let stream = BroadcastStream::new(rx).map(|rebuild| {
        let data = match rebuild {
            Ok(Rebuild::Styles) => "styles".to_string(),
            Ok(Rebuild::Chunks(live)) => format!("chunks {}", live.join(",")),
            // A lagged receiver missed events — reload is the safe catch-up.
            Ok(Rebuild::Reload) | Err(_) => "reload".to_string(),
        };
        Ok(Event::default().data(data))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// The current joined rule set, for the **dev** style hot-swap: the client fetches
/// this on a `styles` reload event and swaps the constructed sheet in place.
async fn styles_endpoint<Src: Clone + Send + Sync + 'static>(
    State(state): State<Arc<AppState<Src>>>,
) -> Response {
    let rules = {
        let styles = state.styles.read().expect("style table lock poisoned");
        route_rules(&styles)
    };
    ([(header::CONTENT_TYPE, "application/json")], serde_json::to_vec(&rules).expect("rules serialize"))
        .into_response()
}

/// Write the extracted style state beside the client bundle (`build` mode) — the
/// artifact `prod` boots from, schema.json-style: derived, published, reviewable.
fn write_styles_artifact(
    client_dir: &std::path::Path,
    styles: &RwLock<Styles>,
) -> anyhow::Result<()> {
    let path = client_dir
        .parent()
        .context("client dir has no parent for the style artifact")?
        .join("styles.json");
    let styles = styles.read().expect("style table lock poisoned");
    let artifact = StylesArtifact {
        universe: styles.universe.clone(),
        rules: styles.table.clone(),
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&artifact)?)
        .with_context(|| format!("writing style artifact {}", path.display()))?;
    println!("style table → {}", path.display());
    Ok(())
}

/// `build`'s closing size report: every core wasm and live chunk through binaryen's
/// default pipeline (`wasm-opt -O`, in-process via the `wasm-opt` crate — no external
/// tool). A comparator for the shipped bytes; the bundle itself is untouched.
fn report_optimized_sizes(
    client_dir: &std::path::Path,
    manifest: &AssetManifest,
) -> anyhow::Result<()> {
    let chunks = manifest
        .app
        .chunks
        .live
        .iter()
        .flat_map(|(live, files)| files.iter().map(move |f| (live.as_str(), f)));
    for (role, file) in manifest.app.wasm.iter().map(|f| ("app", f)).chain(chunks) {
        let path = client_dir.join(file);
        let optimized = std::env::temp_dir().join(file);
        wasm_opt::OptimizationOptions::new_optimize_for_size()
            .run(&path, &optimized)
            .with_context(|| format!("wasm-opt on {file}"))?;
        let before = std::fs::metadata(&path)?.len();
        let after = std::fs::metadata(&optimized)?.len();
        std::fs::remove_file(&optimized).ok();
        println!("wasm-opt -O {role} ({file}): {before} → {after} bytes");
    }
    Ok(())
}

/// Prod's style state: the `styles.json` artifact `build` published, falling back to
/// live extraction — loud, because a deployment should ship the artifact, and the
/// fallback needs the workspace source present.
fn prod_styles(
    client_dir: &std::path::Path,
    manifest_dir: &std::path::Path,
    app_crate: &str,
) -> anyhow::Result<Styles> {
    let path = client_dir
        .parent()
        .context("client dir has no parent for the style artifact")?
        .join("styles.json");
    match std::fs::read(&path) {
        Ok(bytes) => {
            let artifact: StylesArtifact = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing style artifact {}", path.display()))?;
            Ok(Styles { table: artifact.rules, universe: artifact.universe })
        }
        Err(_) => {
            eprintln!(
                "no style artifact at {} — extracting from source (run `build` to publish it)",
                path.display()
            );
            let roots = idyll_styles::extract::source_roots(manifest_dir)
                .context("locating style sources (no artifact to load)")?;
            let table = idyll_styles::extract::extract(&roots)
                .context("extracting the style table (no artifact to load)")?;
            let universe = idyll_styles::extract::universe(&roots, app_crate)
                .context("closing the live universe (no artifact to load)")?;
            Ok(Styles { table, universe })
        }
    }
}

/// Watch every style-contributing crate's sources. On change, the **fast track**
/// re-extracts the style table from source (no cargo in the loop — milliseconds) and
/// swaps it, restyling connected browsers in place; an **app-crate** change then also
/// runs the full track — rebuild the wasm, hot-swap the membrane and the manifest
/// (content-addressed: new bytes mean new names), and signal a reload. Runs on its own
/// thread (the cargo/jco builds are blocking); failures are logged, not fatal.
fn spawn_watcher(
    build: AppBuild,
    initial_wasm: Vec<u8>,
    engine: Arc<RwLock<AppEngine>>,
    reload: broadcast::Sender<Rebuild>,
    manifest: Arc<RwLock<AssetManifest>>,
    styles: Arc<RwLock<Styles>>,
    style_roots: Vec<idyll_styles::extract::SourceRoot>,
) {
    use notify::{RecursiveMode, Watcher};

    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        }) {
            Ok(w) => w,
            Err(err) => {
                eprintln!("dev watch disabled: {err}");
                return;
            }
        };
        let app_src = build.watch_dir();
        if let Err(err) = watcher.watch(&app_src, RecursiveMode::Recursive) {
            eprintln!("dev watch disabled: {err}");
            return;
        }
        for root in &style_roots {
            let src = root.src_dir();
            if src != app_src && src.is_dir() {
                if let Err(err) = watcher.watch(&src, RecursiveMode::Recursive) {
                    eprintln!("style watch disabled for {}: {err}", root.package);
                }
            }
        }

        let mut current_wasm = initial_wasm;
        loop {
            // Block for the first event, then drain the burst an editor save produces,
            // keeping the changed paths — they decide whether the wasm track runs.
            let mut batch = Vec::new();
            match rx.recv() {
                Ok(Ok(event)) if is_source_change(&event) => batch.extend(event.paths),
                Ok(_) => continue,
                Err(_) => break, // watcher dropped
            }
            while let Ok(more) = rx.recv_timeout(Duration::from_millis(150)) {
                if let Ok(event) = more {
                    if is_source_change(&event) {
                        batch.extend(event.paths);
                    }
                }
            }

            // Fast track: re-extract the table from source and swap — connected
            // browsers restyle in place, ahead of (or without) any cargo build.
            match idyll_styles::extract::extract(&style_roots) {
                Ok(fresh) => {
                    let changed = {
                        let mut styles = styles.write().expect("style table lock poisoned");
                        (styles.table != fresh).then(|| styles.table = fresh).is_some()
                    };
                    if changed {
                        let _ = reload.send(Rebuild::Styles);
                        println!("styles changed — sheet swapped, no reload.");
                    }
                }
                // Mid-edit sources often don't parse; keep the last good table.
                Err(err) => eprintln!("style extraction failed (keeping the previous table): {err:#}"),
            }

            // Full track: only the app crate compiles into the wasm.
            if !batch.iter().any(|path| path.starts_with(&app_src)) {
                continue;
            }
            println!("app changed — rebuilding…");
            let rebuilt = build.build_component(false);
            let wasm = match rebuilt {
                Ok(wasm) => wasm,
                Err(err) => {
                    eprintln!("rebuild failed (keeping previous build): {err}");
                    continue;
                }
            };
            // Byte-identity is the reload gate: a style-value edit compiles values out
            // of wasm targets, so the rebuilt component is identical and the browsers'
            // running live are provably current — the sheet swap was the whole change.
            if wasm == current_wasm {
                println!("wasm byte-identical — no reload.");
                continue;
            }
            let swapped = (|| -> anyhow::Result<_> {
                // Re-validated whole: a rebuild that dropped its `page` is a broken
                // build, kept out of the running server like any other failure.
                let membrane = MembraneEngine::new(&wasm)?;
                let live = membrane.live()?;
                let new_engine = app_engine(membrane)?;
                let published = build.build_client(&wasm, false, &live)?;
                Ok((new_engine, published))
            })();
            match swapped {
                Ok((new_engine, published)) => {
                    // Both halves swap under the engine's write lock: a document
                    // render holds the engine read guard across its manifest read,
                    // so no request can pair one build's engine with the other's
                    // asset names.
                    let mut engine_slot = engine.write().expect("engine lock poisoned");
                    // The reload gate is per live: the NAME is the cross-build
                    // identity, and a live's chunk changing means every page that
                    // linked it holds stale code. Two changes still reload everyone:
                    // framework assets (glue, runtime, the primary — shared code),
                    // and the document entries (`page`/`head` — their content is the
                    // served HTML itself, which no browser links as a chunk).
                    let (changed, reload_all) = {
                        let old = manifest.read().expect("manifest lock poisoned");
                        let new = &published.app.chunks;
                        let changed: Vec<String> = new
                            .live
                            .keys()
                            .filter(|name| {
                                new.live.get(*name) != old.app.chunks.live.get(*name)
                            })
                            .cloned()
                            .collect();
                        let document_changed =
                            changed.iter().any(|name| name == "page" || name == "head");
                        let framework_changed = old.runtime != published.runtime
                            || old.app.js != published.app.js
                            || old.app.shims != published.app.shims
                            || old.app.wasm != published.app.wasm;
                        (changed, framework_changed || document_changed)
                    };
                    *manifest.write().expect("manifest lock poisoned") = published;
                    *engine_slot = new_engine;
                    drop(engine_slot);
                    current_wasm = wasm;
                    if reload_all {
                        let _ = reload.send(Rebuild::Reload);
                        println!("reloaded (shared or document code changed).");
                    } else {
                        println!(
                            "live chunks changed: {} — pages that linked one will reload.",
                            changed.join(", ")
                        );
                        let _ = reload.send(Rebuild::Chunks(changed));
                    }
                }
                Err(err) => eprintln!("rebuild failed (keeping previous build): {err}"),
            }
        }
    });
}

fn is_source_change(event: &notify::Event) -> bool {
    use notify::EventKind;
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && event
        .paths
        .iter()
        .any(|p| p.extension().map(|e| e == "rs").unwrap_or(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prerender_rejects_colliding_and_unrepresentable_routes() {
        #[derive(idyll_route::Route)]
        enum Route {
            #[route("/")]
            Index,
            #[route("/{slug}")]
            Doc { slug: String },
        }
        let doc = |slug: &str| Route::Doc { slug: slug.into() };
        assert!(prerender_destinations::<Route>(&[]).is_err());
        assert!(prerender_destinations(&[Route::Index, doc("index")]).is_err());
        assert!(prerender_destinations(&[doc("intro"), doc("INTRO")]).is_err());
        for slug in ["..", ".", "../outside", "a/b", "a\\b", "c:drive", "trailing."] {
            assert!(prerender_destinations(&[doc(slug)]).is_err(), "{slug}");
        }
        assert_eq!(prerender_destinations(&[Route::Index, doc("intro")]).unwrap(),
            [PathBuf::from("index.html"), PathBuf::from("intro.html")]);
    }

    #[test]
    fn a_failed_nested_mount_is_reported_to_prerender() {
        let engine = MembraneEngine::new(include_bytes!("../../idyll-host/tests/fixtures/guest.wasm")).unwrap();
        let faults = Arc::new(std::sync::Mutex::new(Vec::new()));
        let output = std::cell::RefCell::new(String::new());
        let send = |html: String| { output.borrow_mut().push_str(&html); true };
        let mut stream = MountStream {
            send: &send,
            engine: &engine,
            seed: b"{}",
            insecure_seed: 0,
            budget: 1000,
            counters: HashMap::new(),
            faults: Some(faults.clone()),
        };
        assert!(stream.mount("decline", None, &idyll::Html::EMPTY, 1));
        assert!(output.borrow().contains("idyll-live"));
        assert_eq!(faults.lock().unwrap().len(), 1);
        assert!(faults.lock().unwrap()[0].contains("declined"));
    }

    fn manifest() -> AssetManifest {
        AssetManifest {
            runtime: "0000000000000001.js".into(),
            app: AppAssets {
                js: "0000000000000002.js".into(),
                wasm: vec!["0000000000000003.wasm".into()],
                shims: vec!["0000000000000004.js".into()],
                chunks: chunks::ChunkManifest {
                    live: [("board".to_string(), vec!["0000000000000005.wasm".into()])]
                        .into(),
                },
            },
        }
    }

    /// A whole (empty-bodied) live-page document, as the stream would concatenate
    /// it — the emitters compose, they never parse.
    fn doc(manifest: &AssetManifest) -> String {
        format!(
            "{}{}",
            document_prelude("t", "", "", manifest, "/__idyll__", false, true),
            document_tail(Some((
                br#"{"a":"<x>"}"#,
                0,
                chunks_script(&manifest.app.chunks, "/__idyll__").as_str(),
            ))),
        )
    }

    /// A markerless page's document: content only.
    fn static_doc(manifest: &AssetManifest) -> String {
        format!(
            "{}{}",
            document_prelude("t", "", "", manifest, "/__idyll__", false, false),
            document_tail(None),
        )
    }

    // A live page is a live mount in the browser: root wrapper, the seed its
    // claims consume, and the full load plan — runtime, glue and shim
    // modulepreloads, the wasm `as="fetch"` preload (crossorigin, to match the
    // glue's fetch).
    #[test]
    fn island_documents_ship_the_seed_and_the_full_load_plan() {
        let html = doc(&manifest());
        assert!(html.contains("window.__IDYLL_SEED__={\"a\":\"\\u003cx>\"};"));
        assert!(html.contains(
            "<script type=\"module\" src=\"/__idyll__/0000000000000001.js\" \
             data-app=\"/__idyll__/0000000000000002.js\">"
        ));
        assert!(html.contains("<link rel=\"modulepreload\" href=\"/__idyll__/0000000000000001.js\">"));
        assert!(html.contains("<link rel=\"modulepreload\" href=\"/__idyll__/0000000000000002.js\">"));
        assert!(html.contains("<link rel=\"modulepreload\" href=\"/__idyll__/0000000000000004.js\">"));
        assert!(html.contains(
            "<link rel=\"preload\" as=\"fetch\" type=\"application/wasm\" \
             href=\"/__idyll__/0000000000000003.wasm\" crossorigin>"
        ));
        assert!(html.contains("<idyll-root"));
        assert!(html.contains("<title>t</title>"));
    }

    // A page whose paint declared no live ships ZERO framework bytes: no runtime,
    // no capture, no skew guard, no hints, no app module, no seed — links are native
    // navigation. Dev adds only the runtime module (the reload client rides in it).
    #[test]
    fn markerless_documents_ship_zero_framework_bytes() {
        let html = static_doc(&manifest());
        assert!(!html.contains("/__idyll__/0"), "asset referenced: {html}");
        assert!(!html.contains("__IDYLL_SEED__"), "seed shipped: {html}");
        assert!(!html.contains("__IDYLL_Q"), "capture shipped: {html}");
        assert!(!html.contains("idyll-skew"), "skew guard shipped: {html}");
        assert!(!html.contains("data-app"), "app module named: {html}");
        assert!(html.contains("<title>t</title>"));

        let dev = document_prelude("t", "", "", &manifest(), "/__idyll__", true, false);
        assert!(dev.contains("0000000000000001.js"), "dev keeps the runtime: {dev}");
        assert!(dev.contains("__IDYLL_DEV__"));
        assert!(!dev.contains("data-app"), "a dev static page still names no app: {dev}");
    }

    // The app's head mount rides inside `<head>`, after the load plan.
    #[test]
    fn the_head_mount_rides_in_the_document_head() {
        let prelude = document_prelude(
            "t", "<meta charset=\"utf-8\">", "", &manifest(), "/__idyll__", false, true,
        );
        let head_end = prelude.find("</head>").expect("head closes");
        let meta = prelude.find("<meta charset").expect("head content present");
        assert!(meta < head_end);
    }

    // The skew guard installs before any asset tag and is scoped to the assets route:
    // a swept hash 404s, the guard reloads once, the fresh document re-syncs.
    #[test]
    fn the_skew_guard_precedes_every_asset_reference() {
        let html = doc(&manifest());
        let guard = html.find("sessionStorage.getItem('idyll-skew')").expect("guard present");
        let first_asset = html.find("/__idyll__/00000000").expect("assets referenced");
        assert!(guard < first_asset, "guard must install before any asset tag: {html}");
    }

    // The prelude's capture snippet and the runtime's delegated listeners are one
    // event surface: the snippet queues what the runtime will replay, so the two
    // lists drifting means dropped (or unroutable) early input.
    #[test]
    fn the_capture_snippet_and_the_runtime_delegate_the_same_events() {
        let runtime = include_str!("../runtime/runtime.js");
        let list = runtime
            .split("const DELEGATED = [")
            .nth(1)
            .and_then(|rest| rest.split(']').next())
            .expect("runtime.js declares DELEGATED");
        let runtime_events: Vec<&str> = list
            .split(',')
            .map(|s| s.trim().trim_matches('\''))
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            runtime_events, DELEGATED_EVENTS,
            "runtime.js DELEGATED drifted from DELEGATED_EVENTS"
        );
        let snippet = capture_snippet();
        for ty in DELEGATED_EVENTS {
            assert!(snippet.contains(&format!("'{ty}'")), "snippet misses {ty}");
        }
    }

    // Early capture rides on live pages — their mounts are the consumer the
    // replay feeds.
    #[test]
    fn island_documents_carry_the_capture_snippet() {
        assert!(doc(&manifest()).contains("window.__IDYLL_Q="));
    }

    // Dev mode's marker is part of the document prelude — the smart client (style
    // hot-swap + reload) lives in runtime.js and keys off it.
    #[test]
    fn dev_documents_carry_the_dev_marker_in_the_prelude() {
        let prelude = document_prelude("t", "", "", &manifest(), "/__idyll__", true, true);
        assert!(prelude.contains("window.__IDYLL_DEV__=1"));
        let prod = document_prelude("t", "", "", &manifest(), "/__idyll__", false, true);
        assert!(!prod.contains("__IDYLL_DEV__"));
    }

    fn styles(universe: &[&str], rules: serde_json::Value) -> Styles {
        Styles {
            table: serde_json::from_value(rules).unwrap(),
            universe: universe.iter().map(|s| s.to_string()).collect(),
        }
    }

    // The document sheet is the live universe's join: every rule the app's
    // components could reference — the page mount, its live, and any lazy branch
    // either materializes client-side — provisioned ahead of any paint. A package
    // outside the universe (another app in the workspace) never ships.
    #[test]
    fn the_sheet_is_the_universe_join() {
        let styles = styles(
            &["the-app"],
            serde_json::json!({
                "the-app": {
                    "x-page": ".x-page{color:#0af}",
                    "x-branch": ".x-branch{display:none}",
                },
                "other-app": { "x-foreign": ".x-foreign{margin:0}" },
            }),
        );

        let sheet = document_sheet(&styles);

        assert!(sheet.contains(".x-page{color:#0af}"), "universe rule rides: {sheet}");
        assert!(sheet.contains(".x-branch{display:none}"), "lazy-branch rule rides: {sheet}");
        assert!(!sheet.contains("x-foreign"), "another app's rules stay out: {sheet}");
        assert_eq!(sheet.matches(".x-page{").count(), 1, "no duplicates: {sheet}");
    }

    // The one free-form value (font stacks) can't close the inline <style> early.
    #[test]
    fn the_sheet_escapes_style_element_closers() {
        let styles = styles(
            &["the-app"],
            serde_json::json!({
                "the-app": { "x-evil": ".x-evil{font-family:\"</style>\"}" },
            }),
        );
        let sheet = document_sheet(&styles);
        assert!(!sheet.contains("</style>"), "escaped: {sheet}");
        assert!(sheet.contains("<\\/style>"));
    }

    // The styles.json artifact round-trips both halves of the style state.
    #[test]
    fn the_styles_artifact_round_trips() {
        let artifact = StylesArtifact {
            universe: ["the-app".to_string()].into(),
            rules: serde_json::from_value(serde_json::json!({
                "the-app": { "x-a": ".x-a{gap:1rem}" },
            }))
            .unwrap(),
        };
        let json = serde_json::to_vec_pretty(&artifact).unwrap();
        let back: StylesArtifact = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.universe, artifact.universe);
        assert_eq!(back.rules, artifact.rules);
    }

    // The sheet rides the prelude; no rules, no element.
    #[test]
    fn the_prelude_emits_the_style_element() {
        let styled =
            document_prelude("t", "", ".x{color:#0af}\n", &manifest(), "/__idyll__", false, true);
        assert!(styled.contains("<style data-idyll>.x{color:#0af}\n</style>"));
        let bare = document_prelude("t", "", "", &manifest(), "/__idyll__", false, true);
        assert!(!bare.contains("<style"));
    }
}
