//! Source-level style extraction — the style table is derived data.
//!
//! The `#[styles]` attribute compiles a module's styles; this module *reads* the same
//! modules from source (through the same `idyll-styles-parse` grammar, so they cannot
//! disagree) and produces the [`StyleTable`] the server joins guest rule names
//! against. No binary is in the loop: the table rebuilds in milliseconds from
//! whatever is on disk, which is what makes style values in *any* crate hot in dev —
//! including crates whose compiled binary is stale.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use idyll_styles_parse as parse;

use crate::StyleTable;

/// A workspace crate whose sources may declare styles: the package name and manifest
/// dir are the two halves of every declaration's path identity; `deps` (its declared
/// dependencies that are themselves workspace members) is what [`universe`] closes
/// over.
#[derive(Debug, Clone)]
pub struct SourceRoot {
    pub package: String,
    pub manifest_dir: PathBuf,
    pub deps: Vec<String>,
}

impl SourceRoot {
    /// The directory whose `.rs` files are scanned (and watched, in dev).
    pub fn src_dir(&self) -> PathBuf {
        self.manifest_dir.join("src")
    }
}

/// The workspace's member crates, from cargo metadata. `dir` is any directory inside
/// the workspace.
pub fn source_roots(dir: &Path) -> Result<Vec<SourceRoot>> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(dir)
        .output()
        .context("running cargo metadata for style extraction")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed under {}:\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;
    let packages = metadata["packages"]
        .as_array()
        .context("cargo metadata has no packages")?;
    let members: std::collections::BTreeSet<&str> =
        packages.iter().filter_map(|package| package["name"].as_str()).collect();

    packages
        .iter()
        .map(|package| {
            let name = package["name"]
                .as_str()
                .context("package without a name in cargo metadata")?;
            let manifest = package["manifest_path"]
                .as_str()
                .context("package without a manifest path in cargo metadata")?;
            let manifest_dir = Path::new(manifest)
                .parent()
                .context("manifest path without a parent")?;
            let deps = package["dependencies"]
                .as_array()
                .map(|deps| {
                    deps.iter()
                        .filter_map(|dep| dep["name"].as_str())
                        .filter(|dep| members.contains(dep))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            Ok(SourceRoot {
                package: name.to_string(),
                manifest_dir: manifest_dir.to_path_buf(),
                deps,
            })
        })
        .collect()
}

/// The live universe: the app crate and its transitive workspace dependencies —
/// every package whose code can link into the live wasm, and therefore every
/// package whose rules a live could materialize client-side.
pub fn universe(roots: &[SourceRoot], app_crate: &str) -> Result<std::collections::BTreeSet<String>> {
    let by_name: std::collections::BTreeMap<&str, &SourceRoot> =
        roots.iter().map(|root| (root.package.as_str(), root)).collect();
    if !by_name.contains_key(app_crate) {
        bail!("app crate `{app_crate}` is not a workspace member");
    }
    let mut closure = std::collections::BTreeSet::new();
    let mut frontier = vec![app_crate.to_string()];
    while let Some(package) = frontier.pop() {
        if !closure.insert(package.clone()) {
            continue;
        }
        if let Some(root) = by_name.get(package.as_str()) {
            frontier.extend(root.deps.iter().cloned());
        }
    }
    Ok(closure)
}

/// Every style rule declared under the given roots — the whole-app table.
pub fn extract(roots: &[SourceRoot]) -> Result<StyleTable> {
    let mut table = StyleTable::default();
    let mut prefixes = Prefixes::default();
    for root in roots {
        let src = root.src_dir();
        if src.is_dir() {
            extract_dir(root, &src, &mut table, &mut prefixes)?;
        }
    }
    Ok(table)
}

/// Class prefix → the path identity that claimed it. The prefix is 32 bits of the
/// identity's FNV-1a hash, so two declaring files *can* hash alike — in which case
/// their class names could shadow each other silently. The whole-table walk is the
/// one place every styled file is seen together, so the collision is refused here.
#[derive(Default)]
struct Prefixes(std::collections::HashMap<String, String>);

impl Prefixes {
    fn claim(&mut self, prefix: &str, identity: &str) -> Result<()> {
        match self.0.get(prefix) {
            Some(previous) if previous != identity => bail!(
                "style class prefix collision: `{previous}` and `{identity}` both hash \
                 to `{prefix}` — rename one of the files"
            ),
            _ => {
                self.0.insert(prefix.to_string(), identity.to_string());
                Ok(())
            }
        }
    }
}

/// [`source_roots`] + [`extract`] in one step, for callers without a watch loop.
pub fn workspace(dir: &Path) -> Result<StyleTable> {
    extract(&source_roots(dir)?)
}

fn extract_dir(
    root: &SourceRoot,
    dir: &Path,
    table: &mut StyleTable,
    prefixes: &mut Prefixes,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            extract_dir(root, &path, table, prefixes)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            file_claiming(root, &path, table, prefixes)?;
        }
    }
    Ok(())
}

/// Extract one source file's `#[styles] mod styles` (if it has one) into `table`.
pub fn file(root: &SourceRoot, path: &Path, table: &mut StyleTable) -> Result<()> {
    file_claiming(root, path, table, &mut Prefixes::default())
}

fn file_claiming(
    root: &SourceRoot,
    path: &Path,
    table: &mut StyleTable,
    prefixes: &mut Prefixes,
) -> Result<()> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let ast = syn::parse_file(&source)
        .map_err(|error| located(path, &error))
        .with_context(|| format!("parsing {}", path.display()))?;

    let mut module = None;
    for item in &ast.items {
        find_styles_modules(item, true, &mut |found, top_level| {
            if !top_level {
                bail!(
                    "{}: #[styles] modules live at the top of the file — a nested one \
                     would collide with the file's class identity",
                    path.display()
                );
            }
            if module.replace(found).is_some() {
                bail!(
                    "{}: two #[styles] modules in one file (the compiler rejects this \
                     too — the module is named `styles`)",
                    path.display()
                );
            }
            Ok(())
        })?;
    }
    let Some(module) = module else { return Ok(()) };

    let identity = parse::path_identity(&root.package, &root.manifest_dir, path)
        .with_context(|| {
            format!("{} is not under {}'s manifest dir", path.display(), root.package)
        })?;
    let prefix = parse::class_prefix(&identity);
    prefixes.claim(&prefix, &identity)?;
    // Var identity is the crate; the group's `:root` rules ride the table like any
    // rule, and `css!` references derive the same names syntactically.
    let vars_prefix = parse::vars_prefix(&root.package);
    let groups =
        parse::var_groups(module, &vars_prefix).map_err(|error| located(path, &error))?;
    for group in &groups {
        table.insert(&root.package, group.name.clone(), group.rule());
    }
    for theme in parse::theme_list(module, &prefix, &vars_prefix)
        .map_err(|error| located(path, &error))?
    {
        for atom in theme.atoms {
            table.insert(&root.package, atom.class.clone(), atom.rule());
        }
    }
    for rule in parse::document_list(module, &prefix, &vars_prefix)
        .map_err(|error| located(path, &error))?
    {
        table.insert(&root.package, rule.name, rule.css);
    }
    for kf in parse::keyframes_list(module, &vars_prefix).map_err(|error| located(path, &error))? {
        table.insert(&root.package, kf.name.clone(), kf.css.clone());
    }
    for style in parse::style_consts(module).map_err(|error| located(path, &error))? {
        let atoms = parse::parse_style(&prefix, &vars_prefix, &style.name, style.body)
            .map_err(|error| located(path, &error))?;
        for atom in atoms {
            table.insert(&root.package, atom.class.clone(), atom.rule());
        }
    }
    Ok(())
}

fn find_styles_modules<'a>(
    item: &'a syn::Item,
    top_level: bool,
    found: &mut impl FnMut(&'a syn::ItemMod, bool) -> Result<()>,
) -> Result<()> {
    let syn::Item::Mod(module) = item else { return Ok(()) };
    if module.attrs.iter().any(is_styles_attr) {
        found(module, top_level)?;
    }
    if let Some((_, items)) = &module.content {
        for item in items {
            find_styles_modules(item, false, found)?;
        }
    }
    Ok(())
}

fn is_styles_attr(attr: &syn::Attribute) -> bool {
    let path = attr.path();
    path.is_ident("styles")
        || (path.segments.len() == 2
            && path.segments[0].ident == "idyll_styles"
            && path.segments[1].ident == "styles")
}

fn located(path: &Path, error: &syn::Error) -> anyhow::Error {
    let start = error.span().start();
    anyhow::anyhow!("{}:{}:{}: {error}", path.display(), start.line, start.column + 1)
}

#[cfg(test)]
mod tests {
    use super::Prefixes;

    #[test]
    fn a_prefix_collision_between_distinct_files_is_refused() {
        let mut prefixes = Prefixes::default();
        prefixes.claim("i00000001", "app/src/a.rs").unwrap();
        // Re-seeing the same file (a watch loop re-extract) is not a collision.
        prefixes.claim("i00000001", "app/src/a.rs").unwrap();
        let error = prefixes.claim("i00000001", "app/src/b.rs").unwrap_err();
        assert!(error.to_string().contains("app/src/a.rs"), "{error}");
        assert!(error.to_string().contains("app/src/b.rs"), "{error}");
    }
}
