//! `#[styles]` / `props!` / `live_view! css=[…]` — the typed style surface end to end:
//! parse, class identity, merge composition, and rules riding the rendered template.

use idyll::{view, view_html};
use idyll_styles::{merge, props, styles};

props! { pub Chip { color, background } }

#[styles]
mod styles {
    use idyll_styles::Style;

    use super::ChipStyle;

    pub const CARD: Style = css! {{
        padding: "1.5rem",
        background: "#ffffff",
        max_width: "860px",
        margin: "0 auto",
        dark: { background: "#101214" },
        ":hover": { background: "#f4f6f8" },
        mobile: { padding: "1rem" },
    }};

    pub const ACTIVE: Style = css! {{
        background: "#e5f4f2",
    }};

    /// The relational shapes: a style that reaches into its own subtree, which is how
    /// a widget's children stay class-free (the code-tabs are the production case).
    pub const TABS: Style = css! {{
        display: "flex",
        child(input): { opacity: 0 },
        child(label, first): { border_radius: "6px 0 0 6px" },
        checked_next(input, label): { background: "#11151d" },
        focus_next(input, label): { outline: "2px solid #2a9d90" },
        checked_pairs(input, pre, 3): { display: "block" },
        hover_child(small): { visibility: "visible" },
        focus_within_child(small): { visibility: "visible" },
    }};

    pub const CHIP: ChipStyle = css! {{
        color: "#0af",
        background: "transparent",
    }};

    /// The grid family and the multi-value shorthands added to the typed table: a real
    /// dashboard grid (`repeat`/`minmax`/`fr`), a two-value `gap`, `column_gap`, a
    /// side-specific `border`, and an inset `box_shadow`. Compiling *is* the assertion —
    /// a value outside the typed surface is a compile error at its span.
    pub const GRID: Style = css! {{
        display: "grid",
        grid_template_columns: "repeat(auto-fill, minmax(200px, 1fr))",
        grid_template_rows: "auto 1fr",
        grid_auto_flow: "row dense",
        grid_auto_rows: "minmax(44px, auto)",
        gap: "1px 12px",
        column_gap: "8px",
        border_top: "1px solid #e2e5d5",
        box_shadow: "inset 0 -1.5px 0 #edefe1",
    }};

    pub const GRID_CELL: Style = css! {{
        grid_column: "1 / span 2",
        grid_row: "auto",
    }};

    /// `filter` added to the typed table (a sim's at-rest sheen is a `brightness` bump). A
    /// space-separated list of filter primitives, validated function-by-function — never a
    /// string pass-through; compiling *is* the assertion.
    pub const FILTER: Style = css! {{
        filter: "brightness(1.85) saturate(1.05)",
    }};
}

use styles::{ACTIVE, CARD, CHIP, FILTER, GRID, GRID_CELL, TABS};

#[test]
fn grid_and_multivalue_shorthands_lower_to_css() {
    let rules = merge(&[GRID.atoms(), GRID_CELL.atoms()]).rules;
    let css_of = |suffix: &str| {
        rules
            .iter()
            .find(|r| r.name.ends_with(suffix))
            .and_then(|r| r.css.as_deref())
            .unwrap_or_else(|| panic!("rule *{suffix} present"))
            .to_string()
    };
    assert!(css_of("-grid-template-columns")
        .ends_with("{grid-template-columns:repeat(auto-fill, minmax(200px, 1fr))}"));
    assert!(css_of("-grid-template-rows").ends_with("{grid-template-rows:auto 1fr}"));
    assert!(css_of("-grid-auto-flow").ends_with("{grid-auto-flow:row dense}"));
    assert!(css_of("-gap").ends_with("{gap:1px 12px}"));
    assert!(css_of("-column-gap").ends_with("{column-gap:8px}"));
    // `border_top` is the exception among the multi-value shorthands here: the others name
    // one property and survive whole, while a border's three facets each collide with a
    // longhand a sibling atom could set, so it lowers to the three.
    assert!(css_of("-border-top-width").ends_with("{border-top-width:1px}"));
    assert!(css_of("-border-top-style").ends_with("{border-top-style:solid}"));
    assert!(css_of("-border-top-color").ends_with("{border-top-color:#e2e5d5}"));
    assert!(css_of("-box-shadow").ends_with("{box-shadow:inset 0 -1.5px 0 #edefe1}"));
    assert!(css_of("-grid-column").ends_with("{grid-column:1 / span 2}"));
}

#[test]
fn filter_lowers_to_css() {
    let rules = merge(&[FILTER.atoms()]).rules;
    let css = rules
        .iter()
        .find(|r| r.name.ends_with("-filter"))
        .and_then(|r| r.css.as_deref())
        .expect("filter rule present");
    assert!(css.ends_with("{filter:brightness(1.85) saturate(1.05)}"), "got {css}");
}

#[test]
fn class_identity_is_site_const_property_condition() {
    let classes: Vec<&str> = CARD.atoms().iter().map(|a| a.class).collect();
    // One shared file prefix; the const name and readable property + condition
    // suffixes carry the rest of the identity.
    let prefix = classes[0].split('-').next().unwrap().to_string();
    assert!(prefix.starts_with('i') && prefix.len() == 9, "prefix shape: {prefix}");
    assert_eq!(
        classes,
        [
            format!("{prefix}-card-padding-top"),
            format!("{prefix}-card-padding-right"),
            format!("{prefix}-card-padding-bottom"),
            format!("{prefix}-card-padding-left"),
            format!("{prefix}-card-background"),
            format!("{prefix}-card-max-width"),
            format!("{prefix}-card-margin-top"),
            format!("{prefix}-card-margin-right"),
            format!("{prefix}-card-margin-bottom"),
            format!("{prefix}-card-margin-left"),
            format!("{prefix}-card-background-k"),
            format!("{prefix}-card-background-h"),
            format!("{prefix}-card-padding-top-m"),
            format!("{prefix}-card-padding-right-m"),
            format!("{prefix}-card-padding-bottom-m"),
            format!("{prefix}-card-padding-left-m"),
        ]
    );
    // Same file, different const: same prefix, distinct classes.
    assert_eq!(
        ACTIVE.atoms()[0].class,
        format!("{prefix}-active-background")
    );
}

#[test]
fn values_render_into_rule_text_only() {
    let rules = merge(&[CARD.atoms()]).rules;
    let css_of = |suffix: &str| {
        rules
            .iter()
            .find(|r| r.name.ends_with(suffix))
            .and_then(|r| r.css.as_deref())
            .unwrap_or_else(|| panic!("rule *{suffix} present"))
            .to_string()
    };
    // `margin: "0 auto"` is authoring spelling; the atoms are the sides it means.
    assert!(css_of("-margin-top").ends_with("{margin-top:0}"));
    assert!(css_of("-margin-right").ends_with("{margin-right:auto}"));
    assert!(css_of("-margin-left").ends_with("{margin-left:auto}"));
    assert!(css_of("-background-k").starts_with("@media (prefers-color-scheme: dark){"));
    assert!(css_of("-background-h").contains(":hover{background:#f4f6f8}"));
    assert!(css_of("-padding-top-m").starts_with("@media (max-width: 640px){"));
    // No class name contains a value fragment.
    assert!(rules.iter().all(|r| !r.name.contains("860") && !r.name.contains("ffffff")));
}

#[test]
fn relational_conditions_select_the_subtree_not_the_element() {
    let rules = merge(&[TABS.atoms()]).rules;
    let css_of = |suffix: &str| {
        rules
            .iter()
            .find(|r| r.name.ends_with(suffix))
            .and_then(|r| r.css.as_deref())
            .unwrap_or_else(|| panic!("rule *{suffix} present"))
            .to_string()
    };
    let class = |suffix: &str| {
        rules.iter().find(|r| r.name.ends_with(suffix)).expect("rule present").name.to_string()
    };

    let input = class("-opacity-c-input");
    assert_eq!(css_of("-opacity-c-input"), format!(".{input} > input{{opacity:0}}"));

    let first = class("-border-radius-c-label-first");
    assert_eq!(
        css_of("-border-radius-c-label-first"),
        format!(".{first} > label:first-of-type{{border-radius:6px 0 0 6px}}")
    );

    let checked = class("-background-n-input-label-ck");
    assert_eq!(
        css_of("-background-n-input-label-ck"),
        format!(".{checked} > input:checked + label{{background:#11151d}}")
    );

    // `outline: "2px solid #2a9d90"` is three atoms, and the relational condition rides
    // each of them — the selector is the property's, not the shorthand's.
    let focus = class("-outline-width-n-input-label-fv");
    assert_eq!(
        css_of("-outline-width-n-input-label-fv"),
        format!(".{focus} > input:focus-visible + label{{outline-width:2px}}")
    );
    let focus_color = class("-outline-color-n-input-label-fv");
    assert_eq!(
        css_of("-outline-color-n-input-label-fv"),
        format!(".{focus_color} > input:focus-visible + label{{outline-color:#2a9d90}}")
    );

    // One condition, a selector list of n positional pairs — the bound is how many
    // panels a set can hold.
    let pairs = class("-display-p-input-pre-3");
    let expected: Vec<String> = (1..=3)
        .map(|i| format!(".{pairs} > input:nth-of-type({i}):checked ~ pre:nth-of-type({i})"))
        .collect();
    assert_eq!(css_of("-display-p-input-pre-3"), format!("{}{{display:block}}", expected.join(",")));

    // The two halves of a reveal: the styled element is the trigger in both, and
    // what they select is its child — the pointer's path and the keyboard's.
    let hov = class("-visibility-hc-small");
    assert_eq!(
        css_of("-visibility-hc-small"),
        format!(".{hov}:hover > small{{visibility:visible}}")
    );
    let kbd = class("-visibility-fwc-small");
    assert_eq!(
        css_of("-visibility-fwc-small"),
        format!(".{kbd}:focus-within > small{{visibility:visible}}")
    );

    // The element condition stays a descendant; only these reach one level.
    assert!(!css_of("-display").contains('>'));
}

#[test]
fn composition_is_last_wins_per_property_and_condition() {
    let merged = merge(&[CARD.atoms(), ACTIVE.atoms()]);
    let classes: Vec<&str> = merged.class_attr.split(' ').collect();

    let card = |suffix: &str| {
        CARD.atoms()
            .iter()
            .find(|a| a.class.ends_with(suffix))
            .unwrap_or_else(|| panic!("CARD has a {suffix} atom"))
            .class
    };

    // ACTIVE's bare background replaced CARD's; the dark/hover backgrounds are other
    // conditions and survive.
    assert!(!classes.contains(&card("-card-background")));
    assert!(classes.contains(&ACTIVE.atoms()[0].class));
    assert!(classes.contains(&card("-card-background-k")));
}

#[test]
fn constrained_styles_carry_their_marker_type() {
    // `CHIP: ChipStyle` compiled — color and background are in Chip's set. The check
    // is the annotation itself; a `padding:` line in CHIP's body would fail to compile
    // (no `Allows<Padding>` impl for Chip).
    fn takes_chip(style: ChipStyle) -> usize {
        style.atoms().len()
    }
    assert_eq!(takes_chip(CHIP), 2);
}

mod reactive {
    use idyll::driver::{DomCommand, DomDriver as _, HandlerId};
    use idyll::{live_view, Ctx, Setup};
    use idyll_styles::merge;

    use super::styles::{ACTIVE, CARD};

    #[derive(Debug)]
    enum Msg {
        Toggle,
    }

    /// `css=[CARD, $sel => ACTIVE]`: the class attribute is a binding that re-merges
    /// the present entries per run — same last-wins law as the static path.
    async fn chip(ctx: Ctx<Setup, Msg>) -> idyll::Result {
        let on = ctx.mutable_signal(false);
        let sel = on.read();
        let mut ctx = ctx.render(live_view! {
            button.tag css=[CARD, $sel => ACTIVE] onclick=>(|_| Msg::Toggle) { "chip" }
        }).await?;
        loop {
            let (Msg::Toggle, turn) = ctx.recv().await?;
            on.update(&turn, |v| *v = !*v);
        }
    }

    #[test]
    fn toggling_remerges_the_class_attribute() {
        let mut rt = idyll::Runtime::new();
        let mut driver = idyll::CommandBufferDriver::new();
        let ctx = rt.ctx::<Msg>();
        rt.spawn(idyll::component::spawn_live(chip, ctx, idyll::component::report_to_log));
        rt.run_once();
        rt.process_pending_view(&mut driver);
        rt.flush(&mut driver);
        let initial = driver.take_commands();

        let class_writes = |commands: &[DomCommand]| -> Vec<String> {
            commands
                .iter()
                .filter_map(|c| match c {
                    DomCommand::SetAttr { name, value, .. } if name == "class" => {
                        Some(value.clone())
                    }
                    _ => None,
                })
                .collect()
        };

        let base = merge(&[CARD.atoms()]).class_attr;
        let selected = merge(&[CARD.atoms(), ACTIVE.atoms()]).class_attr;
        assert_eq!(
            class_writes(&initial),
            [format!("tag {base}")],
            "the first paint carries the true (base) state, shorthand prepended"
        );

        // Delivery is the union: the inactive variant's rule already rode the
        // template so a client-side flip has its rule present in-band.
        let announced: Vec<String> = initial
            .iter()
            .filter_map(|c| match c {
                DomCommand::ReplaceTemplate { template, .. } => Some(template.clone()),
                _ => None,
            })
            .flat_map(|t| t.styles.iter().map(|r| r.name.to_string()).collect::<Vec<_>>())
            .collect();
        for atom in CARD.atoms().iter().chain(ACTIVE.atoms()) {
            assert!(
                announced.contains(&atom.class.to_string()),
                "rule {} must ride in-band while inactive",
                atom.class
            );
        }

        driver.dispatch_event(HandlerId(0), idyll::Event::default());
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        assert_eq!(class_writes(&driver.take_commands()), [format!("tag {selected}")]);

        driver.dispatch_event(HandlerId(0), idyll::Event::default());
        rt.run_to_quiescence();
        rt.flush(&mut driver);
        assert_eq!(
            class_writes(&driver.take_commands()),
            [format!("tag {base}")],
            "toggling back restores the base merge"
        );
    }
}

#[test]
fn view_css_attaches_classes_and_rules() {
    let content = view! {
        div css=[CARD, ACTIVE] {
            a.nav css=[CHIP] href=("/") { "home" }
        }
    };

    let html = view_html(&content, []);
    let merged = merge(&[CARD.atoms(), ACTIVE.atoms()]);
    assert!(
        html.contains(&format!("class=\"{}\"", merged.class_attr)),
        "merged class attr in HTML: {html}"
    );
    // Static `.class` shorthands share the one class attribute with the merge.
    let chip_classes = merge(&[CHIP.atoms()]).class_attr;
    assert!(
        html.contains(&format!("class=\"nav {chip_classes}\"")),
        "shorthand + css classes combined: {html}"
    );

    // Every referenced rule rides the template, resolved (this is native code).
    let styles = &content.template().styles;
    for atom in merged.rules.iter().chain(merge(&[CHIP.atoms()]).rules.iter()) {
        assert!(
            styles.iter().any(|r| r.name == atom.name && r.css.is_some()),
            "rule {} rides the content",
            atom.name
        );
    }
}
