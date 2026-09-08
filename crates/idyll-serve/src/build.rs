//! The **build driver**. The server treats the app crate a bit like an interpreted
//! language: it builds it to wasm on demand rather than depending on a pre-built
//! artifact. `cargo build --target wasm32-wasip2` produces the one `idyll:ssr`
//! `app`-world component — the *same* artifact runs SSR in the membrane and,
//! transpiled (in-process, via `js-component-bindgen`), in the browser via the
//! hand-written `runtime.js`.
//!
//! The browser bundle is published **content-addressed**: every served file is named
//! `<hash>.<ext>` and is immutable — same name, same bytes, forever. The
//! [`AssetManifest`] (role → filename) is the only place the names are recorded; the
//! server reads it at boot and writes the URLs into the document, which is the one
//! mutable pointer in the system. Any host retention policy is safe against these
//! names (a CDN can cache them unbounded); locally the `assets/` dir holds exactly
//! the current manifest and nothing else.
//!
//! Layout under `target/idyll-client/<app>/`:
//!
//! ```text
//! stage/          transpile workdir: raw bindgen output (never served)
//! assets/         the served dir: exactly the manifest's files (+ .br sidecars)
//! manifest.json   role → filename (server boot input; not served)
//! .idyll-stamp    transpile freshness gate (not served)
//! ```
//!
//! Rebuilds are cheap when nothing changed: cargo is the wasm oracle (it fingerprints
//! sources, deps, features, profile, toolchain), and the transpile is gated on a
//! content stamp of the wasm + transpiler identity + embedded runtime assets.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use oxc_ast::ast::{BindingPattern, Expression, Statement};

/// The framework's client sources ship inside the server binary, so the published
/// bundle always matches the framework version that built it.
const RUNTIME_JS: &str = include_str!("../runtime/runtime.js");

/// The WASI shim modules the transpiled glue imports (per the map below), keyed by
/// the staged filename the glue references.
const SHIMS: [(&str, &str); 3] = [
    ("wasi-shim.js", include_str!("../runtime/wasi-shim.js")),
    ("wasi-clock-monotonic.js", include_str!("../runtime/wasi-clock-monotonic.js")),
    ("wasi-clock-wall.js", include_str!("../runtime/wasi-clock-wall.js")),
];

/// The published client bundle: role → content-addressed filename. Serialized as
/// `manifest.json` beside `assets/`; the document is rendered from this, so asset
/// URLs never appear anywhere else.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AssetManifest {
    /// The framework runtime module. It carries no reference to the app (the document
    /// hands it the app URL), so its name is stable across app deploys — a returning
    /// visitor cache-hits the runtime even when the app changed.
    pub runtime: String,
    /// The app bundle — the component the browser mounts pages and live through.
    pub app: AppAssets,
}

/// The app half of the bundle: the transpiled glue module plus everything it references.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct AppAssets {
    /// The transpiled glue module. The runtime imports it lazily by the URL the document
    /// passes (`data-app` on the runtime's script tag).
    pub js: String,
    /// Core wasm module(s), fetched by the glue; each one is preload-hinted
    /// (`as="fetch"`) so the download starts at first head bytes.
    pub wasm: Vec<String>,
    /// The WASI shim modules the glue imports; modulepreload-hinted.
    pub shims: Vec<String>,
    /// The live chunks' map (live → chunk filenames). The primary is `wasm[0]`
    /// — the module the glue fetches; a live's chunks are ordinary assets its
    /// mount links first.
    pub chunks: crate::chunks::ChunkManifest,
}

impl AssetManifest {
    /// Every served filename this manifest names.
    pub fn files(&self) -> Vec<&str> {
        let mut files = vec![self.runtime.as_str(), self.app.js.as_str()];
        files.extend(self.app.wasm.iter().map(String::as_str));
        files.extend(self.app.chunks.live.values().flatten().map(String::as_str));
        files.extend(self.app.shims.iter().map(String::as_str));
        files
    }

    /// Read the manifest published under `client_root`.
    pub fn load(client_root: &Path) -> Result<AssetManifest> {
        let path = client_root.join("manifest.json");
        let json = std::fs::read_to_string(&path)
            .with_context(|| format!("reading asset manifest {}", path.display()))?;
        serde_json::from_str(&json).context("parsing asset manifest")
    }

    fn store(&self, client_root: &Path) -> Result<()> {
        let json = serde_json::to_string_pretty(self).expect("manifest serializes");
        std::fs::write(client_root.join("manifest.json"), json)
            .context("writing asset manifest")
    }
}

/// One file headed for `assets/`, named by its content: `<fnv1a-hex16>.<ext>`.
struct Asset {
    name: String,
    bytes: Vec<u8>,
}

fn asset(bytes: Vec<u8>, ext: &str) -> Asset {
    Asset { name: format!("{:016x}.{ext}", fnv1a(&bytes)), bytes }
}

/// Publish the bundle: write every asset (and, when `compress`, a `.br` sidecar for
/// content negotiation), record the manifest, and sweep `assets/` down to exactly
/// that set. Content addressing makes the writes idempotent — an existing name
/// already holds these bytes — and makes the sweep the whole retention story:
/// deployment environments that want old generations keep them in *their* cache.
fn publish(
    client_root: &Path,
    manifest: &AssetManifest,
    files: &[Asset],
    compress: bool,
) -> Result<()> {
    match std::fs::remove_file(client_root.join(".idyll-stamp")) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("invalidating client build stamp"),
    }
    let dir = client_root.join("assets");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating assets dir {}", dir.display()))?;

    let mut keep: HashSet<String> = HashSet::new();
    for file in files {
        let path = dir.join(&file.name);
        if !path.exists() {
            std::fs::write(&path, &file.bytes)
                .with_context(|| format!("writing asset {}", file.name))?;
        }
        if compress {
            let sidecar = format!("{}.br", file.name);
            if !dir.join(&sidecar).exists() {
                std::fs::write(dir.join(&sidecar), brotli_bytes(&file.bytes))
                    .with_context(|| format!("writing sidecar {sidecar}"))?;
            }
            keep.insert(sidecar);
        }
        keep.insert(file.name.clone());
    }

    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !keep.contains(name) {
            std::fs::remove_file(&path)
                .with_context(|| format!("sweeping stale asset {name}"))?;
        }
    }
    manifest.store(client_root)
}

/// The transpiler's per-crossing debug logger. Its switch is `process.env.JCO_DEBUG` and
/// we transpile with node compatibility off, so no `process` exists to hold it: the body
/// is unreachable and every call is dead. A minifier cannot know that — the guard is a
/// member chain it must assume can throw — so [`strip_dead_logger`] is where the fact
/// gets said, and the minifier does the rest of the work around it.
const DEAD_LOGGER: &str = "_debugLog";

/// Cut the dead logger out of a parsed module: its declaration, and every statement that
/// is just a call to it.
///
/// The arguments go with the calls, which is the point — three quarters of the ~200 sites
/// build an object literal to hand a function that returns immediately. Emptying the
/// function instead would leave the callers paying for all of them.
///
/// The post-condition is the check: after this, the name must not appear anywhere in the
/// module. A call in a position this misses — an argument, an initializer, a shape the
/// transpiler has since changed — leaves one behind and fails the build rather than
/// shipping glue with a logger that no longer exists.
struct StripDeadLogger {
    calls: usize,
    declarations: usize,
    survivors: usize,
}

impl<'a> oxc_ast_visit::VisitMut<'a> for StripDeadLogger {
    fn visit_statements(&mut self, statements: &mut oxc_allocator::Vec<'a, Statement<'a>>) {
        statements.retain(|statement| {
            let drop = match statement {
                Statement::ExpressionStatement(expression) => match &expression.expression {
                    Expression::CallExpression(call) => match &call.callee {
                        Expression::Identifier(name) => name.name == DEAD_LOGGER,
                        _ => false,
                    },
                    _ => false,
                },
                Statement::VariableDeclaration(declaration) => {
                    declaration.declarations.iter().any(|declarator| match &declarator.id {
                        BindingPattern::BindingIdentifier(name) => name.name == DEAD_LOGGER,
                        _ => false,
                    })
                }
                _ => false,
            };
            if drop {
                match statement {
                    Statement::ExpressionStatement(_) => self.calls += 1,
                    _ => self.declarations += 1,
                }
            }
            !drop
        });
        oxc_ast_visit::walk_mut::walk_statements(self, statements);
    }

    fn visit_identifier_reference(&mut self, name: &mut oxc_ast::ast::IdentifierReference<'a>) {
        if name.name == DEAD_LOGGER {
            self.survivors += 1;
        }
    }
}

/// Minify the transpiled glue, stripping the dead logger on the way through.
///
/// Renaming is why this runs last: every rewrite the caller made is anchored on a name
/// the transpiler chose. Two things must survive by contract — the module's exports,
/// which `runtime.js` binds by name, and `new URL('./….wasm', import.meta.url)`, which is
/// how the core modules are fetched. Both are checked, and so is the far blunter fact
/// that what comes out still parses: a compressor that emits something the language does
/// not accept costs every live on the page and says only `SyntaxError`, from a file with
/// no line breaks left to point at.
fn minify_glue(source: &str) -> Result<String> {
    use oxc_ast_visit::VisitMut as _;

    let urls_before = source.matches("import.meta.url").count();

    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::mjs();
    let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
    if let Some(error) = parsed.diagnostics.first() {
        bail!("parsing the transpiled glue: {error}");
    }
    let mut program = parsed.program;
    let exports_before = exported_names(&program);

    let mut strip = StripDeadLogger { calls: 0, declarations: 0, survivors: 0 };
    strip.visit_program(&mut program);
    if strip.declarations != 1 || strip.calls == 0 || strip.survivors != 0 {
        bail!(
            "the transpiler's debug logger moved: cut {} declaration(s) and {} call(s), \
             {} reference(s) left — output shape changed?",
            strip.declarations,
            strip.calls,
            strip.survivors,
        );
    }

    oxc_minifier::Minifier::new(oxc_minifier::MinifierOptions::default())
        .minify(&allocator, &mut program);
    let exports_after = exported_names(&program);
    let out = oxc_codegen::Codegen::new()
        .with_options(oxc_codegen::CodegenOptions {
            minify: true,
            ..oxc_codegen::CodegenOptions::default()
        })
        .build(&program)
        .code;

    let reread = oxc_allocator::Allocator::default();
    let parsed = oxc_parser::Parser::new(&reread, &out, source_type).parse();
    if let Some(error) = parsed.diagnostics.first() {
        bail!("minifying emitted glue that does not parse: {error}");
    }
    if exports_after != exports_before {
        bail!(
            "minifying changed the glue's exports ({exports_before:?} became \
             {exports_after:?}) — the runtime binds them by name"
        );
    }
    if out.matches("import.meta.url").count() != urls_before {
        bail!("minifying dropped an `import.meta.url` — the core modules are fetched by it");
    }
    Ok(out)
}

/// The module's exported names, sorted — what `runtime.js` binds the glue by, and so the
/// one thing minifying is not allowed to change.
fn exported_names(program: &oxc_ast::ast::Program<'_>) -> Vec<String> {
    let mut names: Vec<String> = program
        .body
        .iter()
        .filter_map(|statement| match statement {
            Statement::ExportNamedDeclaration(export) => Some(
                export
                    .specifiers
                    .iter()
                    .map(|specifier| specifier.exported.name().to_string())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    names.sort();
    names
}

/// Brotli one asset for the `.br` sidecar `ServeDir` negotiates. Quality 10: the
/// sidecars are written once per content change, so encode time is off every path
/// that matters.
pub(crate) fn brotli_bytes(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 10, 22);
    writer.write_all(input).expect("in-memory brotli write");
    drop(writer);
    out
}

/// `cargo metadata` for the workspace containing `from_dir`.
fn cargo_metadata(from_dir: &Path) -> Result<serde_json::Value> {
    let out = Command::new("cargo")
        .args(["metadata", "--locked", "--format-version", "1", "--no-deps"])
        .current_dir(from_dir)
        .output()
        .context("running `cargo metadata` (is cargo on PATH?)")?;
    if !out.status.success() {
        bail!("cargo metadata failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    serde_json::from_slice(&out.stdout).context("parsing cargo metadata")
}

fn target_dir(meta: &serde_json::Value) -> Result<PathBuf> {
    Ok(PathBuf::from(
        meta["target_directory"]
            .as_str()
            .context("cargo metadata: no target_directory")?,
    ))
}

/// Everything needed to build the app crate's wasm and publish its browser bundle,
/// discovered once from `cargo metadata`.
#[derive(Clone)]
pub struct AppBuild {
    /// The cargo package name (`-p <app_crate>`).
    app_crate: String,
    /// The app crate's directory (where the cargo build runs).
    app_manifest_dir: PathBuf,
    /// The workspace target directory (where cargo drops the wasip2 component).
    target_dir: PathBuf,
    /// This app's client root (`target/idyll-client/<app>` — per-app, so two servers
    /// in one workspace never sweep each other's live bundles).
    client_root: PathBuf,
    /// The published `schema.json` the app build validates against, handed to the cargo
    /// child as `IDYLL_SCHEMA` so the client macros read the schema the running server
    /// actually defines (their offline fallback is the checked-in artifact).
    schema_path: Option<PathBuf>,
}

impl AppBuild {
    /// Discover the app crate's location and the target dir via `cargo metadata`, run from
    /// `from_dir` (a directory inside the workspace — the server passes its own manifest dir
    /// so this works regardless of the runtime cwd).
    pub fn discover(app_crate: &str, from_dir: &Path) -> Result<Self> {
        let meta = cargo_metadata(from_dir)?;
        let target_dir = target_dir(&meta)?;
        let pkg = meta["packages"]
            .as_array()
            .context("cargo metadata: no packages")?
            .iter()
            .find(|p| p["name"] == app_crate)
            .with_context(|| format!("cargo metadata: package `{app_crate}` not found"))?;
        let app_manifest_dir = PathBuf::from(
            pkg["manifest_path"]
                .as_str()
                .context("cargo metadata: no manifest_path")?,
        )
        .parent()
        .context("app manifest has no parent dir")?
        .to_path_buf();

        let client_root = target_dir.join("idyll-client").join(app_crate);
        Ok(AppBuild {
            app_crate: app_crate.to_string(),
            app_manifest_dir,
            target_dir,
            client_root,
            schema_path: None,
        })
    }

    /// Point the app build at the published schema artifact (see [`Self::schema_path`]).
    pub fn set_schema_path(&mut self, path: PathBuf) {
        self.schema_path = Some(path);
    }

    /// The directory to watch for source changes in dev mode.
    pub fn watch_dir(&self) -> PathBuf {
        self.app_manifest_dir.join("src")
    }

    /// The served assets directory (immutable, content-addressed files).
    pub fn client_dir(&self) -> PathBuf {
        self.client_root.join("assets")
    }

    fn crate_underscore(&self) -> String {
        self.app_crate.replace('-', "_")
    }

    /// Build **the** app component (one artifact: the `idyll:ssr` `app` world — render +
    /// mount/dispatch) and return its bytes. The same wasm is rendered by the membrane and
    /// transpiled for the browser, so this is the single cargo build per change.
    ///
    /// Staleness is cargo's job, not ours: it fingerprints sources, deps, features,
    /// profile and toolchain, and a fresh build is a sub-second no-op — so `prod`
    /// "rebuild if stale" is simply "run the build".
    pub fn build_component(&self, release: bool) -> Result<Vec<u8>> {
        let mut command = Command::new("cargo");
        command
            .args(["build", "--locked", "-p", &self.app_crate, "--target", "wasm32-wasip2"])
            .current_dir(&self.app_manifest_dir);
        // The splitter's attribution input: `--emit-relocs` has wasm-ld keep the
        // linking and reloc.* sections in the core module. Appended so a caller's own
        // RUSTFLAGS survive; the encoded form wins over RUSTFLAGS when set, so whichever
        // one is live gets the flag.
        match std::env::var("CARGO_ENCODED_RUSTFLAGS") {
            Ok(encoded) => command
                .env("CARGO_ENCODED_RUSTFLAGS", format!("{encoded}\x1f-C\x1flink-arg=--emit-relocs")),
            Err(_) => {
                let flags = std::env::var("RUSTFLAGS").unwrap_or_default();
                command.env("RUSTFLAGS", format!("{flags} -C link-arg=--emit-relocs"))
            }
        };
        if release {
            command.arg("--release");
        }
        if let Some(schema) = &self.schema_path {
            command.env("IDYLL_SCHEMA", schema);
        }
        let status = command
            .status()
            .context("running `cargo build` for the app component")?;
        if !status.success() {
            bail!("cargo build (app component, wasm32-wasip2) failed");
        }
        let wasm = self
            .target_dir
            .join("wasm32-wasip2")
            .join(if release { "release" } else { "debug" })
            .join(format!("{}.wasm", self.crate_underscore()));
        std::fs::read(&wasm)
            .with_context(|| format!("reading app component wasm at {}", wasm.display()))
    }

    /// Ensure the published bundle matches `wasm`, transpiling only when stale, and
    /// return the manifest (plus whether a transpile ran). Freshness is a content
    /// stamp, not mtimes (mtimes lie across checkouts/copies): the wasm bytes, the
    /// transpiler identity, and the embedded runtime assets all participate — and the
    /// manifest's files must actually exist, so a swept or hand-pruned dir republishes.
    pub fn ensure_client(&self, wasm: &[u8], live: &[String]) -> Result<(AssetManifest, bool)> {
        let stamp = client_stamp(wasm);
        let stamp_path = self.client_root.join(".idyll-stamp");
        if let Some(manifest) = cached_release(&self.client_root, &stamp) {
            return Ok((manifest, false));
        }
        let manifest = self.build_client(wasm, true, live)?;
        std::fs::write(&stamp_path, stamp).context("writing client build stamp")?;
        Ok((manifest, true))
    }

    /// Package the **same** component bytes for the browser: transpile them in
    /// `stage/`, rewrite the glue's references onto content-addressed names
    /// (leaf-first: the wasm and shims get their names, then the rewritten glue gets
    /// its own), and publish the hashed set into `assets/` with the manifest. The
    /// served bundle is self-contained — no bundler, no npm at runtime — and every
    /// WASI import is mapped onto the embedded shim modules.
    pub fn build_client(&self, wasm: &[u8], compress: bool, live: &[String]) -> Result<AssetManifest> {
        let stage = self.client_root.join("stage");
        std::fs::create_dir_all(&stage)
            .with_context(|| format!("creating stage dir {}", stage.display()))?;

        // The transpile runs in-process — `js-component-bindgen` is the library the
        // jco CLI wraps, so the toolchain stays cargo (no Node, no machine install).
        // `wasi:<ns>/*` matches the versioned import keys (e.g.
        // `wasi:cli/stdout@0.2.6`); a `*`-free replacement collapses them all onto
        // the one shim module. The two clock interfaces both export
        // `now`/`resolution`, so they cannot share one shim module.
        let map: std::collections::HashMap<String, String> = [
            ("wasi:cli/*", "./wasi-shim.js"),
            ("wasi:io/*", "./wasi-shim.js"),
            ("wasi:random/*", "./wasi-shim.js"),
            ("wasi:clocks/monotonic-clock@*", "./wasi-clock-monotonic.js"),
            ("wasi:clocks/wall-clock@*", "./wasi-clock-wall.js"),
        ]
        .into_iter()
        .map(|(from, to)| (from.to_string(), to.to_string()))
        .collect();
        let transpiled = js_component_bindgen::transpile(
            wasm,
            js_component_bindgen::TranspileOpts {
                name: "app".to_string(),
                no_typescript: true,
                nodejs_compat_disabled: true,
                import_bindings: Some(js_component_bindgen::BindingsMode::Hybrid),
                map: Some(map),
                // Every core module lands as its own staged file (never inlined
                // base64): the splitter and content-addressing below expect files.
                base64_cutoff: 0,
                ..Default::default()
            },
        )
        .map_err(|err| anyhow::anyhow!("transpiling the app component: {err}"))?;
        for (name, bytes) in &transpiled.files {
            std::fs::write(stage.join(name), bytes)
                .with_context(|| format!("staging transpile output {name}"))?;
        }

        let mut glue = std::fs::read_to_string(stage.join("app.js"))
            .context("reading the transpiled glue (stage/app.js)")?;
        let mut files = Vec::new();

        // Core wasm modules, leaf-first: name each by content, then point the glue's
        // `new URL('./app.core*.wasm', import.meta.url)` fetches at the new names.
        let mut cores = Vec::new();
        for entry in std::fs::read_dir(&stage)? {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            if name.starts_with("app.core") && name.ends_with(".wasm") {
                cores.push((name, std::fs::read(&path)?));
            }
        }
        cores.sort_by(|a, b| a.0.cmp(&b.0)); // app.core.wasm first, then app.core2…
        if cores.is_empty() {
            bail!("the transpile produced no core wasm module — output shape changed?");
        }
        // The main core module splits at live boundaries; EVERY piece is an
        // ordinary content-addressed asset (immutable, brotli sidecar, HTTP-cached,
        // streaming-compiled): the primary rejoins the wasm list below, each live
        // chunk publishes as its own file, and the manifest maps live → filenames.
        // The glue's one patch hands the instantiated core's exports to the runtime,
        // so live chunks link into the same instance.
        let mut cores = cores.into_iter();
        let (main_name, main_bytes) = cores.next().expect("checked non-empty");
        let (primary_bytes, island_chunks) = crate::chunks::split_core(&main_bytes, live)
            .context("splitting the core module at live boundaries")?;
        let mut island_map: std::collections::BTreeMap<String, Vec<String>> =
            live.iter().map(|name| (name.clone(), Vec::new())).collect();
        for (name, bytes) in island_chunks {
            let chunk = asset(bytes, "wasm");
            island_map.insert(name, vec![chunk.name.clone()]);
            files.push(chunk);
        }
        let chunks = crate::chunks::ChunkManifest { live: island_map };
        let exports_handoff = "memory0 = exports1.memory;";
        if !glue.contains(exports_handoff) {
            bail!("the glue's core-exports binding moved — output shape changed?");
        }
        glue = glue.replace(
            exports_handoff,
            "memory0 = exports1.memory;
    globalThis.__IDYLL_CHUNKS.core(exports1);",
        );

        let mut wasm_names = Vec::new();
        for (staged_name, bytes) in std::iter::once((main_name, primary_bytes)).chain(cores) {
            let hashed = asset(bytes, "wasm");
            if !rewrite(&mut glue, &staged_name, &hashed.name) {
                bail!("the glue never references ./{staged_name} — output shape changed?");
            }
            wasm_names.push(hashed.name.clone());
            files.push(hashed);
        }

        // Shims: only the ones the glue actually imports are published.
        let mut shim_names = Vec::new();
        for (staged_name, source) in SHIMS {
            let hashed = asset(source.as_bytes().to_vec(), "js");
            if rewrite(&mut glue, staged_name, &hashed.name) {
                shim_names.push(hashed.name.clone());
                files.push(hashed);
            }
        }

        // Last, because every rewrite above is anchored on a name the transpiler chose and
        // minifying renames them.
        let glue = minify_glue(&glue).context("minifying the transpiled glue")?;

        let glue = asset(glue.into_bytes(), "js");
        let runtime = asset(RUNTIME_JS.as_bytes().to_vec(), "js");
        let manifest = AssetManifest {
            runtime: runtime.name.clone(),
            app: AppAssets {
                js: glue.name.clone(),
                wasm: wasm_names,
                shims: shim_names,
                chunks,
            },
        };
        files.push(glue);
        files.push(runtime);
        publish(&self.client_root, &manifest, &files, compress)?;
        Ok(manifest)
    }
}

fn cached_release(client_root: &Path, stamp: &str) -> Option<AssetManifest> {
    let previous = std::fs::read_to_string(client_root.join(".idyll-stamp")).ok()?;
    if previous != stamp {
        return None;
    }
    let manifest = AssetManifest::load(client_root).ok()?;
    let assets = client_root.join("assets");
    manifest.files().iter().all(|file| {
        assets.join(file).is_file() && assets.join(format!("{file}.br")).is_file()
    }).then_some(manifest)
}

/// Repoint every `./{from}` reference in the glue at `./{to}`; false if none existed.
fn rewrite(glue: &mut String, from: &str, to: &str) -> bool {
    let pattern = format!("./{from}");
    if !glue.contains(&pattern) {
        return false;
    }
    *glue = glue.replace(&pattern, &format!("./{to}"));
    true
}

/// The client bundle's freshness stamp: wasm content, the transpiler's identity, and
/// the embedded runtime assets (which ship inside this server binary). The transpiler
/// is compiled in (`js-component-bindgen`), so its identity rides idyll-serve's own
/// version — which is why upgrading that dependency warrants a version bump: the
/// stamp is what invalidates a staged bundle transpiled by the older one.
fn client_stamp(wasm: &[u8]) -> String {
    let assets = fnv1a(
        &[RUNTIME_JS, SHIMS[0].1, SHIMS[1].1, SHIMS[2].1]
            .concat()
            .into_bytes(),
    );
    format!(
        "wasm:{:016x}:{} transpiler:js-component-bindgen+idyll-serve@{} assets:{:016x}",
        fnv1a(wasm),
        wasm.len(),
        env!("CARGO_PKG_VERSION"),
        assets,
    )
}

/// FNV-1a 64 — a content-addressing fingerprint (not a security boundary): collisions
/// are astronomically unlikely and the failure mode is a stale-looking rebuild.
pub(crate) fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_dev_release_requires_republication_and_complete_sidecars() {
        let root = std::env::temp_dir().join(format!("idyll-cache-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let release = asset(b"release".to_vec(), "js");
        let dev = asset(b"development".to_vec(), "js");
        let manifest = |file: &Asset| AssetManifest {
            runtime: file.name.clone(),
            app: AppAssets {
                js: file.name.clone(),
                wasm: Vec::new(),
                shims: Vec::new(),
                chunks: crate::chunks::ChunkManifest { live: Default::default() },
            },
        };
        let stamp = client_stamp(b"release component");
        let stamp_path = root.join(".idyll-stamp");
        publish(&root, &manifest(&release), std::slice::from_ref(&release), true).unwrap();
        std::fs::write(&stamp_path, &stamp).unwrap();
        assert!(cached_release(&root, &stamp).is_some());

        publish(&root, &manifest(&dev), std::slice::from_ref(&dev), false).unwrap();
        assert!(!stamp_path.exists());
        assert!(cached_release(&root, &stamp).is_none());
        std::fs::write(&stamp_path, &stamp).unwrap();
        assert!(cached_release(&root, &stamp).is_none());

        publish(&root, &manifest(&release), std::slice::from_ref(&release), true).unwrap();
        std::fs::write(&stamp_path, &stamp).unwrap();
        let cached = cached_release(&root, &stamp).unwrap();
        assert_eq!(cached.runtime, release.name);
        assert!(!root.join("assets").join(&dev.name).exists());
        std::fs::remove_file(root.join("assets").join(format!("{}.br", release.name))).unwrap();
        assert!(cached_release(&root, &stamp).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
}
