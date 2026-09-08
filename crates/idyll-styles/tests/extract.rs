//! The pinned macro==extractor agreement: both sides go through idyll-styles-parse,
//! and this test holds their *path identity* normalizations together — the one place
//! they could drift (the macro reads `Span::file()` + cargo env; the extractor reads
//! the file it is walking).

use std::path::Path;

use idyll_styles::extract::{self, SourceRoot};
use idyll_styles::StyleTable;

#[path = "fixtures/agreement.rs"]
mod fixture;

fn fixture_root() -> SourceRoot {
    SourceRoot {
        package: env!("CARGO_PKG_NAME").to_string(),
        manifest_dir: Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf(),
        deps: Vec::new(),
    }
}

#[test]
fn extractor_and_macro_agree_on_classes_and_rule_text() {
    let root = fixture_root();
    let mut table = StyleTable::default();
    extract::file(&root, &root.manifest_dir.join("tests/fixtures/agreement.rs"), &mut table)
        .expect("fixture extracts");

    let atoms: Vec<_> = fixture::styles::BANNER
        .atoms()
        .iter()
        .chain(fixture::styles::QUIET.atoms())
        .chain(fixture::styles::INKED.atoms())
        .chain(fixture::styles::MOTION.atoms())
        .chain(fixture::styles::TYPESET.atoms())
        .chain(fixture::styles::RESPONSIVE.atoms())
        .chain(fixture::styles::Inverted.atoms())
        .collect();
    assert!(!atoms.is_empty());
    for atom in &atoms {
        assert_eq!(
            table.resolve(atom.class),
            Some(atom.rule),
            "extracted text for {}",
            atom.class
        );
    }

    // The var group: the compiled handle carries the name, the extractor carries the
    // `:root` rule text under the group's identity, and a referencing atom's rule
    // reads `var(--…)` — the value never left the stylesheet.
    let ink = fixture::styles::Palette::ink;
    let _typed: idyll_styles::Var<idyll_styles::kind::Color> = ink;
    let prefix = ink.name.strip_prefix("--").unwrap().split('-').next().unwrap();
    let group_rule = table
        .resolve(&format!("{prefix}-palette"))
        .expect("the group's :root rule is in the table");
    assert!(group_rule.starts_with(":root{"), "{group_rule}");
    assert!(group_rule.contains(&format!("{}:#1c1e21;", ink.name)), "{group_rule}");
    assert!(
        group_rule.contains(&format!("dark){{:root{{{}:#e6e8ea;", ink.name)),
        "{group_rule}"
    );
    let inked = fixture::styles::INKED.atoms();
    assert_eq!(inked[0].rule, format!(".{}{{color:var({})}}", inked[0].class, ink.name));

    // The non-color kinds: each handle compiles at its shape's kind, and the group
    // rule carries the raw values.
    let control = fixture::styles::Metrics::control;
    let _typed: idyll_styles::Var<idyll_styles::kind::Length> = control;
    let _typed: idyll_styles::Var<idyll_styles::kind::Number> = fixture::styles::Metrics::body_line;
    let _typed: idyll_styles::Var<idyll_styles::kind::FontStack> = fixture::styles::Metrics::sans;
    let metrics_rule = table
        .resolve(&format!("{prefix}-metrics"))
        .expect("the metrics group's :root rule is in the table");
    assert!(metrics_rule.contains(&format!("{}:32px;", control.name)), "{metrics_rule}");

    // The document rules: nothing in Rust names them (there is no element to put a
    // class on), so the extractor is their ONLY path to the sheet.
    let file_prefix = atoms[0].class.split('-').next().expect("class carries its file prefix");
    assert_eq!(
        table.resolve(&format!("{file_prefix}-document-root")),
        Some(":root{color-scheme:light dark}"),
    );
    assert_eq!(
        table.resolve(&format!("{file_prefix}-document-body")),
        Some(
            format!(
                "body{{margin-top:0;margin-right:0;margin-bottom:0;margin-left:0;\
                 background:var({})}}",
                ink.name
            )
            .as_str()
        ),
    );

    // A theme is atoms like any other style: one per overridden var, so two themes on
    // one element resolve by the same last-wins merge rather than by rule order. A
    // remap emits a `var()` reference — the colour keeps one definition.
    let inverted = fixture::styles::Inverted.atoms();
    let ink_atom = inverted.iter().find(|a| a.class.ends_with("-palette-ink")).expect("ink override");
    assert_eq!(ink_atom.rule, format!(".{}{{{}:#ffffff}}", ink_atom.class, ink.name));
    let accent_atom =
        inverted.iter().find(|a| a.class.ends_with("-palette-accent")).expect("accent remap");
    assert!(accent_atom.rule.ends_with(&format!(":var({})}}", ink.name)), "{}", accent_atom.rule);

    // …and the extractor found nothing the compiled consts don't have (the extra
    // rules are the two var groups' and the two document rules').
    assert_eq!(table.rules().count(), atoms.len() + 4);
}

#[test]
fn a_nested_styles_module_is_rejected() {
    let root = fixture_root();
    let mut table = StyleTable::default();
    let error =
        extract::file(&root, &root.manifest_dir.join("tests/fixtures/nested.rs"), &mut table)
            .expect_err("nested module rejected");
    assert!(error.to_string().contains("top of the file"), "{error}");
}

// The live-universe closure, against the real workspace: an app crate's universe
// is itself plus its transitive workspace deps — never a sibling app.
#[test]
fn the_universe_is_the_app_crates_workspace_closure() {
    let roots = extract::source_roots(Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("cargo metadata runs in the workspace");
    let universe = extract::universe(&roots, "todo-app").expect("todo-app is a member");

    assert!(universe.contains("todo-app"));
    assert!(universe.contains("idyll-styles"), "workspace deps close transitively");
    assert!(!universe.contains("todo-spa-app"), "a sibling app is out of the universe");
    assert!(!universe.contains("todo-server"), "dependents don't enter the closure");

    assert!(extract::universe(&roots, "no-such-crate").is_err());
}

#[test]
fn files_without_styles_extract_nothing() {
    let root = fixture_root();
    let mut table = StyleTable::default();
    extract::file(&root, &root.manifest_dir.join("src/lib.rs"), &mut table)
        .expect("plain files pass through");
    assert!(table.is_empty());
}
