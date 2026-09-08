use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, ToTokens};

mod style;
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    braced, bracketed,
    ext::IdentExt,
    parenthesized,
    parse::{Parse, ParseStream},
    parse_macro_input,
    token,
    visit::{self, Visit},
    Expr, Ident, ItemFn, LitStr, Result, ReturnType, Token,
};

// ── AST ───────────────────────────────────────────────────────────────────────

enum Node {
    Element(Element),
    Text(LitStr),
    /// `(expr)` or `$sig` in child position. `reactive` (a `$` was present) picks the
    /// channel: reactive → a `$sig`-style `run`-match binding (always text); one-shot
    /// → a construction-time `RenderKind` dispatch (text or a `(view)` mount).
    Splice { expr: Expr, reactive: bool },
    Directive(Directive),
}

struct Element {
    tag: Ident,
    classes: Vec<Ident>,
    id: Option<Ident>,
    /// `css=[entry, …]` — style-or-nothing entries, merged (last-wins per property
    /// and condition) into the element's one class attribute. All-static lists merge
    /// once at view construction; any reactive entry turns the class attribute into
    /// an ordinary binding that re-merges the present entries per run.
    css: Vec<CssEntryAst>,
    attrs: Vec<Attr>,
    events: Vec<EventAttr>,
    children: Vec<Node>,
}

/// One `css=[…]` entry. `$sig => STYLE` is sugar for `($sig).then_some(STYLE)`;
/// everything routes through `idyll_styles::CssEntry` (style-or-nothing), so the
/// three forms share one semantics.
enum CssEntryAst {
    /// A plain style expression with no reactive reads — merged at construction.
    Static(Expr),
    /// `cond => style` — the condition may read signals; the style side may not.
    Conditional { cond: Expr, style: Expr },
    /// An open reactive entry evaluating to style-or-nothing
    /// (`$variant: Signal<Option<Style>>` read, a computed pick, …).
    Dynamic(Expr),
}

impl CssEntryAst {
    /// The entry as a style-or-nothing value expression, for the re-merge arm.
    fn value_tokens(&self) -> TokenStream2 {
        match self {
            CssEntryAst::Static(style) => quote! { (#style) },
            CssEntryAst::Conditional { cond, style } => quote! { ((#cond)).then_some(#style) },
            CssEntryAst::Dynamic(expr) => quote! { (#expr) },
        }
    }

    /// The entry's statically nameable style, for `rule_union` delivery — every rule
    /// any state can activate ships in-band. Open entries return `None`: their
    /// delivery is the document's universe sheet, which exists for exactly that.
    fn nameable_style(&self) -> Option<&Expr> {
        match self {
            CssEntryAst::Static(style) | CssEntryAst::Conditional { style, .. } => Some(style),
            CssEntryAst::Dynamic(_) => None,
        }
    }
}

/// Split a `css=[…]` bracket body into entries: top-level commas separate entries,
/// a top-level `=>` splits a conditional. `$` anywhere in an entry makes it reactive.
fn parse_css_entries(raw: TokenStream2) -> Result<Vec<CssEntryAst>> {
    fn split_top_level(tokens: Vec<proc_macro2::TokenTree>, is_sep: impl Fn(&[proc_macro2::TokenTree], usize) -> usize) -> Vec<Vec<proc_macro2::TokenTree>> {
        let mut out = vec![Vec::new()];
        let mut i = 0;
        while i < tokens.len() {
            let sep = is_sep(&tokens, i);
            if sep > 0 {
                out.push(Vec::new());
                i += sep;
            } else {
                out.last_mut().expect("one part is always open").push(tokens[i].clone());
                i += 1;
            }
        }
        out
    }
    let comma = |tokens: &[proc_macro2::TokenTree], i: usize| match &tokens[i] {
        proc_macro2::TokenTree::Punct(p) if p.as_char() == ',' => 1,
        _ => 0,
    };
    let fat_arrow = |tokens: &[proc_macro2::TokenTree], i: usize| match (&tokens[i], tokens.get(i + 1)) {
        (proc_macro2::TokenTree::Punct(eq), Some(proc_macro2::TokenTree::Punct(gt)))
            if eq.as_char() == '=' && eq.spacing() == proc_macro2::Spacing::Joint && gt.as_char() == '>' =>
        {
            2
        }
        _ => 0,
    };

    let cx = cx_ident();
    let mut entries = Vec::new();
    for entry in split_top_level(raw.into_iter().collect(), comma) {
        if entry.is_empty() {
            continue; // the trailing comma
        }
        let parts = split_top_level(entry.clone(), fat_arrow);
        let stream = |part: &[proc_macro2::TokenTree]| part.iter().cloned().collect::<TokenStream2>();
        match parts.as_slice() {
            [_] => {
                let tokens = stream(&entry);
                if find_dollar(&tokens).is_some() {
                    entries.push(CssEntryAst::Dynamic(syn::parse2(desugar_dollars(tokens, &cx))?));
                } else {
                    entries.push(CssEntryAst::Static(syn::parse2(tokens)?));
                }
            }
            [cond, style] => {
                let style_tokens = stream(style);
                if let Some(span) = find_dollar(&style_tokens) {
                    return Err(syn::Error::new(
                        span,
                        "the style side of `cond => style` is a plain `Style` — put the reactive \
                         read in the condition, or use an open entry evaluating to Option<Style>",
                    ));
                }
                entries.push(CssEntryAst::Conditional {
                    cond: syn::parse2(desugar_dollars(stream(cond), &cx))?,
                    style: syn::parse2(style_tokens)?,
                });
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    stream(&entry),
                    "a css entry has at most one `=>` (`cond => style`)",
                ));
            }
        }
    }
    Ok(entries)
}

enum Attr {
    /// `name=(expr)` — evaluates expr as a string attribute value
    Value { name: Ident, expr: Expr },
    /// `name[expr]` — boolean/optional attribute
    Bool { name: Ident, expr: Expr },
    /// `style:prop=(expr)` / `style:(Handle)=(expr)` — one CSS declaration, written
    /// via `setProperty` (no
    /// whole-attribute reparse). `prop` allows hyphens and `--custom` names.
    StyleProp { name: TokenStream2, span: proc_macro2::Span, expr: Expr },
    /// `painting=(expr)` on a `canvas` — the picture as a live list of layers, each a
    /// live list of shapes. The `<canvas>` restriction is checked at the parse: there
    /// is no other element the runtime can draw on.
    Painting { span: proc_macro2::Span, expr: Expr },
}

/// One canvas of a `live_view!` block: the slot its element is on, and the expression
/// that names its layers. Not a leaf — a picture is a live list, followed the way a
/// `@for`'s source is, so it is taken once at construction rather than re-read per run.
struct PaintingLeaf {
    slot: u32,
    expr: Expr,
}

/// Which wire an event binding subscribes on — decided once, at the parse, from the
/// attribute's spelling. The bare `measure` attribute is the layout observer; every
/// `on*` name is `addEventListener` with the suffix, `onmeasure` included (a DOM
/// `"measure"` event, not the observer).
///
/// The channel is independent of which slot the binding was written in: `measure` observes
/// the same way whether its rect goes to this component's inbox or to a caller's callback.
#[derive(Clone)]
enum EventChannel {
    /// `measure=…` — the runtime's `ResizeObserver` wiring.
    Measure,
    /// `on<name>=…` — `addEventListener(name, …)`.
    Dom(String),
}

/// The wire an attribute name subscribes on, or `None` for an ordinary attribute. Both slots
/// (`=>` to this component's inbox, `=` to a caller's callback) read the channel from here, so
/// which names are events cannot differ between them.
fn event_channel(name: &str) -> Option<EventChannel> {
    match name {
        "measure" => Some(EventChannel::Measure),
        _ => name.strip_prefix("on").map(|suffix| EventChannel::Dom(suffix.to_string())),
    }
}

struct EventAttr {
    channel: EventChannel,
    /// Written `onclick=>(…)`: a mapper naming this component's message. Written
    /// `onclick=(…)`: a [`Callback`](idyll::Callback) the event is handed to, which
    /// says nothing here. The two are told apart by which slot they were written in,
    /// so either may be a lambda, a name, or any other expression.
    to_mailbox: bool,
    /// mapper expression `|e| Some(Msg::X)` or `|e| Msg::X` — which form it is gets
    /// decided by type at the emitted call (`idyll::live_view::MapperCall`).
    mapper: Expr,
}

/// A component call's props: `name=(expr)`, `name=>(expr)` (bound to this inbox), and
/// `?name=(expr)` for the optional half. The same two spellings an element attribute has,
/// plus the `?` that says which struct a prop belongs to — so the call site never has to be
/// matched against a field list to be understood.
fn parse_props(
    input: ParseStream,
) -> Result<(Vec<(Ident, Expr, bool)>, Vec<(Ident, Expr, bool)>)> {
    let (mut required, mut optional) = (Vec::new(), Vec::new());
    loop {
        let optional_prop = input.peek(Token![?]);
        if optional_prop {
            input.parse::<Token![?]>()?;
        } else if !(input.peek(Ident::peek_any) && (input.peek2(Token![=]) || input.peek2(Token![=>]))) {
            break;
        }
        let name: Ident = input.call(Ident::parse_any)?;
        let to_mailbox = if input.peek(Token![=>]) {
            input.parse::<Token![=>]>()?;
            true
        } else {
            input.parse::<Token![=]>()?;
            false
        };
        let value;
        parenthesized!(value in input);
        let expr: Expr = value.parse()?;
        match optional_prop {
            true => optional.push((name, expr, to_mailbox)),
            false => required.push((name, expr, to_mailbox)),
        }
    }
    Ok((required, optional))
}

enum Directive {
    If {
        condition: Expr,
        then_nodes: Vec<Node>,
        else_nodes: Option<Vec<Node>>,
        keep: bool,
    },
    For {
        pat: syn::Pat,
        iter: Expr,
        key: Option<Expr>,
        body: Vec<Node>,
    },
    Match {
        expr: Expr,
        arms: Vec<MatchArm>,
        keep: bool,
    },
    Component {
        component: Ident,
        /// `name=(expr)` and `name=>(expr)`; the second binds to this component's inbox.
        required: Vec<(Ident, Expr, bool)>,
        /// `?name=(expr)` — the optional half, which defaults.
        optional: Vec<(Ident, Expr, bool)>,
        /// A trailing `{ … }` block — the `children` slot sugar. The nodes become a slot
        /// recipe built from the parent's frame and passed as the `children` prop, so the
        /// component wraps them (`Invite when=… { Slider … }`). `None` = no block.
        children: Option<Vec<Node>>,
    },
    /// `@content(expr)` — `view!`'s **eager** child splice: a `View` value
    /// inlined in place at construction — content composing inside structure. In
    /// `live_view!` content is not composed, it is *placed*: `(expr)` over a `View`
    /// (mounted once) or a `Signal<View>`/`Computed<View>` (a tracked splice).
    Content { expr: Expr },
    /// `@live(app::live::Def)` / `@live(Def, key = expr)` — a live
    /// **marker**: a named hole where live code mounts. Emits a first-class
    /// [`TplNode::Live`]; no component runs here. The `LiveDef` path resolves
    /// the name and type-checks `key` against the component's declared key type —
    /// string names exist only in content data (markdown fences), never in Rust.
    /// grammar — content the OUTER mount paints once; the live places copies).
    Live { name: syn::Path, key: Option<Expr> },
    /// `path(args)` in child position — the call form. In **content** (`view!`) it mounts a
    /// live: the callee is the component's `LiveDef` path (`live::Rows()`, `live::Check(frag)`),
    /// its one argument the typed key, placed as a first-class [`TplNode::Live`] marker. In
    /// `live_view!` it is a compile error — the eager view splice is gone; interpolate a built
    /// view with `(expr)` or wrap a component with a `{ … }` children block.
    LiveCall { expr: Expr },
}

struct MatchArm {
    pat: syn::Pat,
    body: Vec<Node>,
}

// ── Parsing ───────────────────────────────────────────────────────────────────

struct ViewInput {
    nodes: Vec<Node>,
}

impl Parse for ViewInput {
    fn parse(input: ParseStream) -> Result<Self> {
        let nodes = parse_nodes(input)?;
        Ok(ViewInput { nodes })
    }
}

fn parse_nodes(input: ParseStream) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    while !input.is_empty() {
        // Check for closing brace of a parent group — don't consume it.
        if input.peek(token::Brace) && nodes.is_empty() {
            break; // empty group
        }
        nodes.push(parse_node(input)?);
    }
    Ok(nodes)
}

/// Consume a leading `$` punct if present (syn has no `Token![$]`).
fn eat_dollar(input: ParseStream) -> bool {
    input
        .step(|cursor| match cursor.punct() {
            Some((punct, rest)) if punct.as_char() == '$' => Ok(((), rest)),
            _ => Err(cursor.error("expected `$`")),
        })
        .is_ok()
}

/// Rewrite `$ident` → `(ident.get(cx))` everywhere in an expression's tokens
/// (recursively, so it works inside `format!(…)` and other nested groups). This is how
/// a signal reads reactively wherever it appears — the one reactive-read syntax.
fn desugar_dollars(input: TokenStream2, cx: &Ident) -> TokenStream2 {
    use proc_macro2::Group;
    let mut out = TokenStream2::new();
    let mut iter = input.into_iter().peekable();
    while let Some(tt) = iter.next() {
        match tt {
            proc_macro2::TokenTree::Punct(p) if p.as_char() == '$' => match iter.next() {
                Some(proc_macro2::TokenTree::Ident(id)) => out.extend(quote! { (#id.get(#cx)) }),
                other => {
                    out.extend(std::iter::once(proc_macro2::TokenTree::Punct(p)));
                    out.extend(other);
                }
            },
            proc_macro2::TokenTree::Group(g) => {
                let inner = desugar_dollars(g.stream(), cx);
                out.extend(std::iter::once(proc_macro2::TokenTree::Group(Group::new(g.delimiter(), inner))));
            }
            other => out.extend(std::iter::once(other)),
        }
    }
    out
}

/// Whether an expression's tokens contain a `$` (a signal read), recursively — the
/// discriminator between a reactive interpolation and a one-shot one.
fn tokens_have_dollar(ts: &TokenStream2) -> bool {
    ts.clone().into_iter().any(|tt| match tt {
        proc_macro2::TokenTree::Punct(p) => p.as_char() == '$',
        proc_macro2::TokenTree::Group(g) => tokens_have_dollar(&g.stream()),
        _ => false,
    })
}

/// Parse the contents of a reactive `(…)` position, desugaring `$signal` reads first.
fn parse_reactive_expr(content: ParseStream) -> Result<Expr> {
    let cx = cx_ident();
    let toks = desugar_dollars(content.parse::<TokenStream2>()?, &cx);
    syn::parse2(toks)
}

fn parse_node(input: ParseStream) -> Result<Node> {
    // String literal → text node
    if input.peek(LitStr) {
        return Ok(Node::Text(input.parse()?));
    }

    // (expr) → a splice; reactive iff it reads a signal (`$`), else one-shot.
    if input.peek(token::Paren) {
        let content;
        parenthesized!(content in input);
        let raw: TokenStream2 = content.parse()?;
        let reactive = tokens_have_dollar(&raw);
        let cx = cx_ident();
        let expr = syn::parse2(desugar_dollars(raw, &cx))?;
        return Ok(Node::Splice { expr, reactive });
    }

    // $signal → reactive read
    if eat_dollar(input) {
        let ident: Ident = input.parse()?;
        let cx = cx_ident();
        return Ok(Node::Splice { expr: syn::parse_quote!(#ident.get(#cx)), reactive: true });
    }

    // @keyword → directive
    if input.peek(Token![@]) {
        input.parse::<Token![@]>()?;
        return parse_directive(input);
    }

    // `path(args)` in child position — a call form. In **content** (`view!`) this is the
    // live-mount spelling (`live::Rows()`, `live::Check(frag)`); in `live_view!` it is
    // rejected (the eager view splice is gone — interpolate a built view with `(expr)`, or
    // wrap a component with a `{ … }` children block). The token shape is the discriminator:
    // an element is followed by attrs or `{`, never `(`, and a `::` path never starts one.
    {
        let fork = input.fork();
        if fork.call(syn::Path::parse_mod_style).is_ok() && fork.peek(token::Paren) {
            let path: syn::Path = input.call(syn::Path::parse_mod_style)?;
            let args_content;
            parenthesized!(args_content in input);
            let raw = args_content.parse::<TokenStream2>()?;
            let expr: Expr = syn::parse2(quote!(#path(#raw)))?;
            if input.peek(token::Brace) {
                return Err(syn::Error::new_spanned(
                    &expr,
                    "a live mount takes no children block — content reaches a live component as \
                     data, through a content field read",
                ));
            }
            return Ok(Node::Directive(Directive::LiveCall { expr }));
        }
    }

    // A capitalised tag is a component, a lowercase one an element — the same rule React
    // uses, and the only thing separating `Slider name=(…)` from `div class=(…)`. The
    // component's name is checked at its declaration too, so a lowercase component is a
    // named error there rather than a silently unknown element here.
    if input.peek(Ident::peek_any) {
        let ahead = input.fork();
        let tag: Ident = ahead.call(Ident::parse_any)?;
        if tag.to_string().starts_with(|c: char| c.is_ascii_uppercase()) {
            let component: Ident = input.call(Ident::parse_any)?;
            let (required, optional) = parse_props(input)?;
            // A trailing `{ … }` block is the `children` slot: the component wraps what the
            // block builds (`Invite when=… { Slider … }`).
            let children = if input.peek(token::Brace) {
                let content;
                braced!(content in input);
                Some(parse_nodes(&content)?)
            } else {
                None
            };
            return Ok(Node::Directive(Directive::Component { component, required, optional, children }));
        }
    }

    // Otherwise: element
    parse_element(input).map(Node::Element)
}

fn parse_directive(input: ParseStream) -> Result<Node> {
    if input.peek(Token![if]) {
        input.parse::<Token![if]>()?;
        let keep = if input.peek(token::Bracket) {
            let inner;
            bracketed!(inner in input);
            let k: Ident = inner.parse()?;
            if k != "keep" {
                // A typo'd `[keep]` silently changing retention semantics is the
                // worst kind of accept; the bracket admits exactly one word.
                return Err(syn::Error::new(k.span(), "expected `keep`"));
            }
            true
        } else {
            false
        };
        let cond_content;
        parenthesized!(cond_content in input);
        let condition: Expr = parse_reactive_expr(&cond_content)?;
        let then_content;
        braced!(then_content in input);
        let then_nodes = parse_nodes(&then_content)?;
        let else_nodes = if input.peek(Token![else]) {
            input.parse::<Token![else]>()?;
            let else_content;
            braced!(else_content in input);
            Some(parse_nodes(&else_content)?)
        } else {
            None
        };
        Ok(Node::Directive(Directive::If {
            condition,
            then_nodes,
            else_nodes,
            keep,
        }))
    } else if input.peek(Token![match]) {
        input.parse::<Token![match]>()?;
        let keep = if input.peek(token::Bracket) {
            let inner;
            bracketed!(inner in input);
            let k: Ident = inner.parse()?;
            if k != "keep" {
                // A typo'd `[keep]` silently changing retention semantics is the
                // worst kind of accept; the bracket admits exactly one word.
                return Err(syn::Error::new(k.span(), "expected `keep`"));
            }
            true
        } else {
            false
        };
        // `@match $sig` / `@match (sig)` — a signal whose value selects the arm.
        let expr: Expr = if eat_dollar(input) {
            let ident: Ident = input.parse()?;
            syn::parse_quote!(#ident)
        } else {
            let expr_content;
            parenthesized!(expr_content in input);
            expr_content.parse()?
        };
        let arms_content;
        braced!(arms_content in input);
        let mut arms = Vec::new();
        while !arms_content.is_empty() {
            let pat = syn::Pat::parse_single(&arms_content)?;
            arms_content.parse::<Token![=>]>()?;
            let body_content;
            braced!(body_content in arms_content);
            let body = parse_nodes(&body_content)?;
            if arms_content.peek(Token![,]) {
                arms_content.parse::<Token![,]>()?;
            }
            arms.push(MatchArm { pat, body });
        }
        Ok(Node::Directive(Directive::Match { expr, arms, keep }))
    } else if input.peek(Token![for]) {
        input.parse::<Token![for]>()?;
        let pat: syn::Pat = syn::Pat::parse_single(input)?;
        input.parse::<Token![in]>()?;
        // `@for x in $source` / `@for x in (source)` — a list, not a `get(cx)` read.
        let iter: Expr = if eat_dollar(input) {
            let ident: Ident = input.parse()?;
            syn::parse_quote!(#ident)
        } else {
            let iter_content;
            parenthesized!(iter_content in input);
            iter_content.parse()?
        };
        let key = if input.peek(token::Bracket) {
            let inner;
            bracketed!(inner in input);
            let k: Ident = inner.parse()?;
            if k != "key" {
                return Err(syn::Error::new(k.span(), "expected `key = expr`"));
            }
            inner.parse::<Token![=]>()?;
            Some(inner.parse::<Expr>()?)
        } else {
            None
        };
        let body_content;
        braced!(body_content in input);
        let body = parse_nodes(&body_content)?;
        Ok(Node::Directive(Directive::For {
            pat,
            iter,
            key,
            body,
        }))
    } else {
        let component: Ident = input.parse()?;
        if component == "rendered" {
            return Err(syn::Error::new(
                component.span(),
                "`@rendered(…)` is gone — place content with `(expr)`: a `View` mounts \
                 once, a `Signal<View>`/`Computed<View>` re-splices when it changes",
            ));
        }
        if component == "content" {
            let expr_content;
            parenthesized!(expr_content in input);
            let expr: Expr = expr_content.parse()?;
            return Ok(Node::Directive(Directive::Content { expr }));
        }
        if component == "live" {
            // A marker, not a component: the typed `LiveDef` path, so the name and
            // the key type are compile-checked. String names exist only in content
            // *data* (markdown fences), validated at boot — never in Rust code.
            let content;
            parenthesized!(content in input);
            if content.peek(LitStr) {
                let lit: LitStr = content.parse()?;
                return Err(syn::Error::new_spanned(
                    lit,
                    "live markers in Rust code are typed — `@live(app::live::Name)`; \
                     string names are the content-data path (markdown fences), validated at boot",
                ));
            }
            let name: syn::Path = content.parse()?;
            let key = if content.peek(Token![,]) {
                content.parse::<Token![,]>()?;
                let key_kw: Ident = content.parse()?;
                if key_kw != "key" {
                    return Err(syn::Error::new_spanned(&key_kw, "expected `key = <expr>`"));
                }
                content.parse::<Token![=]>()?;
                Some(content.parse::<Expr>()?)
            } else {
                None
            };
            if input.peek(token::Brace) {
                return Err(syn::Error::new(
                    name.segments.last().expect("a path has a segment").ident.span(),
                    "a live marker takes no children block — content reaches a live component as \n                     data, through a content field read",
                ));
            }
            return Ok(Node::Directive(Directive::Live { name, key }));
        }
        let (required, optional) = parse_props(input)?;
        let children = if input.peek(token::Brace) {
            let content;
            braced!(content in input);
            Some(parse_nodes(&content)?)
        } else {
            None
        };
        Ok(Node::Directive(Directive::Component { component, required, optional, children }))
    }
}

fn parse_element(input: ParseStream) -> Result<Element> {
    // Tag name
    let tag: Ident = input.parse()?;

    let mut classes = Vec::new();
    let mut id = None;
    let mut css = Vec::new();
    let mut attrs = Vec::new();
    let mut events = Vec::new();

    // .class, #id, and attribute parsing loop
    loop {
        if input.peek(Token![.]) {
            input.parse::<Token![.]>()?;
            classes.push(input.parse::<Ident>()?);
        } else if input.peek(Token![#]) {
            input.parse::<Token![#]>()?;
            id = Some(input.parse::<Ident>()?);
        } else if input.peek(Ident::peek_any) && !input.peek2(token::Brace) {
            // Could be attr, event, or just about to hit a child element.
            // We need to lookahead to distinguish "attr=(expr)" from "child_tag { }".
            // `parse_any` so keyword attribute names (`type`, `for`, `as`) are legal —
            // HTML has them and idyll drives `<head>`/`<script type=…>` from views.
            let fork = input.fork();
            let name_ident: Ident = fork.call(Ident::parse_any)?;
            let name_str = name_ident.to_string();

            if name_str == "css" && fork.peek(Token![=]) && fork.peek2(token::Bracket) {
                // css=[entry, …] — typed style-or-nothing entries; the bracket is the
                // composition list (merge order), not a bool attr (those never follow `=`).
                input.call(Ident::parse_any)?;
                input.parse::<Token![=]>()?;
                let list;
                bracketed!(list in input);
                css.extend(parse_css_entries(list.parse::<TokenStream2>()?)?);
            } else if name_str == "style" && fork.peek(Token![:]) {
                // style:prop=(expr) — a single CSS declaration binding. The property
                // name grammar is `-* ident (-ident)*`: plain (`transform`),
                // hyphenated (`offset_distance` also accepted, underscores map to
                // hyphens), and custom properties (`--qv-p`).
                input.call(Ident::parse_any)?; // consume `style`
                input.parse::<Token![:]>()?;
                // `style:(Motion::a)=(…)` — a `vars!` handle names the custom
                // property, so a renamed var is a compile error instead of a
                // per-frame write that silently lands nowhere.
                if input.peek(token::Paren) {
                    let handle_content;
                    parenthesized!(handle_content in input);
                    let handle: Expr = handle_content.parse()?;
                    let span = syn::spanned::Spanned::span(&handle);
                    input.parse::<Token![=]>()?;
                    let val_content;
                    parenthesized!(val_content in input);
                    let expr: Expr = parse_reactive_expr(&val_content)?;
                    attrs.push(Attr::StyleProp {
                        name: quote! { (#handle).name },
                        span,
                        expr,
                    });
                    continue;
                }
                let mut prop = String::new();
                while input.peek(Token![-]) {
                    input.parse::<Token![-]>()?;
                    prop.push('-');
                }
                let first: Ident = input.call(Ident::parse_any)?;
                let span = first.span();
                prop.push_str(&first.to_string().replace('_', "-"));
                while input.peek(Token![-]) && input.peek2(Ident::peek_any) {
                    input.parse::<Token![-]>()?;
                    let part: Ident = input.call(Ident::parse_any)?;
                    prop.push('-');
                    prop.push_str(&part.to_string().replace('_', "-"));
                }
                input.parse::<Token![=]>()?;
                let val_content;
                parenthesized!(val_content in input);
                let expr: Expr = parse_reactive_expr(&val_content)?;
                attrs.push(Attr::StyleProp { name: quote! { #prop }, span, expr });
            } else if name_str == "painting" && fork.peek(Token![=]) && fork.peek2(token::Paren) {
                // painting=(expr) — a canvas's picture: a live list of layers, each a
                // live list of shapes. The list is a value, taken once like a `@for`'s
                // source (so no `$`); the runtime owns the element, its device-pixel
                // scaling, the per-layer bitmaps, the clear and the strokes.
                let span = input.call(Ident::parse_any)?.span();
                if tag != "canvas" {
                    return Err(syn::Error::new(
                        span,
                        "`painting` draws a picture, and a canvas is the only surface \
                         there is to draw it on — write it on a `canvas` element",
                    ));
                }
                input.parse::<Token![=]>()?;
                let val_content;
                parenthesized!(val_content in input);
                let expr: Expr = val_content.parse()?;
                attrs.push(Attr::Painting { span, expr });
            } else if fork.peek(Token![=>]) {
                // onclick=>(expr) — the expression names *this* component's message.
                let span = input.call(Ident::parse_any)?.span();
                input.parse::<Token![=>]>()?;
                let val_content;
                parenthesized!(val_content in input);
                let raw = val_content.parse::<TokenStream2>()?;
                // `measure=>(|e| …)` is a layout-measurement binding: the runtime observes
                // this element and delivers its root-relative rect on `e.rect` (on mount and
                // on resize) — the same inbox path as an `on*` event, wired by a
                // `ResizeObserver` instead of `addEventListener`.
                let Some(channel) = event_channel(&name_str) else {
                    return Err(syn::Error::new(
                        span,
                        format!("`{name_str}=>(…)` sends to this component's inbox, which only an event (or `measure`) does — write `{name_str}=(…)`"),
                    ));
                };
                let expr: Expr = syn::parse2(raw)?;
                events.push(EventAttr {
                    channel,
                    mapper: expr,
                    to_mailbox: true,
                });
            } else if fork.peek(Token![=]) && fork.peek2(token::Paren) {
                // attr=(expr) or onclick=(...)
                input.call(Ident::parse_any)?; // consume name
                input.parse::<Token![=]>()?;
                let val_content;
                parenthesized!(val_content in input);
                let raw = val_content.parse::<TokenStream2>()?;

                // Detect event attributes — `on*`, and `measure`, which subscribes on its own
                // wire but relays like any other: a component whose layout its parent needs
                // hands the rect to a callback, exactly as it hands on a click.
                if let Some(channel) = event_channel(&name_str) {
                    // An event handler is `Event -> Option<M>`, not a reactive read — no
                    // `$` desugaring (it has no reactive context).
                    let expr: Expr = syn::parse2(raw)?;
                    events.push(EventAttr { channel, mapper: expr, to_mailbox: false });
                } else {
                    let cx = cx_ident();
                    let expr: Expr = syn::parse2(desugar_dollars(raw, &cx))?;
                    attrs.push(Attr::Value {
                        name: name_ident,
                        expr,
                    });
                }
            } else if fork.peek(token::Bracket) {
                // attr[expr] — boolean
                input.call(Ident::parse_any)?;
                let cond_content;
                bracketed!(cond_content in input);
                let expr: Expr = parse_reactive_expr(&cond_content)?;
                attrs.push(Attr::Bool {
                    name: name_ident,
                    expr,
                });
            } else {
                // A bare ident is never an attribute — attrs are always `name=(expr)`,
                // `name[expr]`, or the `.class`/`#id` shorthands (always-on attrs are
                // `name=("")`). So this ident starts the next node (a sibling after a
                // braceless element, or this element's first child tag), and the grammar
                // stays unambiguous. (Bare attrs used to exist and silently merged
                // `link … script …` into one element.)
                break;
            }
        } else {
            break;
        }
    }

    // Children in braces
    let children = if input.peek(token::Brace) {
        let content;
        braced!(content in input);
        parse_nodes(&content)?
    } else {
        Vec::new()
    };

    Ok(Element {
        tag,
        classes,
        id,
        css,
        attrs,
        events,
        children,
    })
}

// ── Code generation ───────────────────────────────────────────────────────────

struct Codegen {
    slot_counter: u32,
    /// Builder method calls for the non-leaf machinery: fragments, children,
    /// `.with_styles`. Reactive leaves go through `leaves`/`event_leaves` instead.
    builder_calls: Vec<TokenStream2>,
    /// Whether this generator emits into a **synchronous, post-render** context — a fragment
    /// arm/row, a slot recipe. Its component children `spawn_child` fire-and-forget (guards ride
    /// the view's `placement_guards`). The root generator (`false`) emits an async render closure
    /// whose component children `await` `mount_child` (see `child_awaits`).
    fragment: bool,
    /// Root generator only: `let g = mount_child::<C>(…).await?; __idyll_guards.push(g);` for each
    /// top-level component, run in the async render body before the view is built.
    child_awaits: Vec<TokenStream2>,
    /// Monotonic id for this generator's view-embedded children — names the `child_id` local and
    /// the slot local so an anchor, its mount, and its `{ … }` slot share one number.
    child_counter: u32,
    /// `let __idyll_child_N = fresh_child_id();` preludes (and slot builds) for this scope.
    child_preludes: Vec<TokenStream2>,
    /// Preludes that need the inbox in scope — a spliced view is built against it.
    sender_preludes: Vec<TokenStream2>,
    /// The view's reactive leaves, in emission order — compiled into **one**
    /// [`Block`](idyll::live_view::Block): `run` is a single `match` over these, `event`
    /// over `event_leaves`, `fragment` over `fragment_leaves` — every body is plain
    /// code behind one dispatch each.
    leaves: Vec<Leaf>,
    /// The canvases this view draws — their own declaration rather than leaves, and
    /// what tells the block a picture has no HTML to be adopted from an SSR paint.
    paintings: Vec<PaintingLeaf>,
    event_leaves: Vec<EventLeaf>,
    fragment_leaves: Vec<FragmentLeaf>,
    /// The **Template IR** under construction: a stack of children-lists, one per open
    /// element (index 0 = the template root's children). This is the typed parse result
    /// the macro emits as const data — the template as data, not markup.
    tree: Vec<Vec<IrNode>>,
    /// A node carries a runtime expression (a keyed live or `css=[…]` merge), so the
    /// template must be built at view construction instead of as a `static`.
    has_runtime_ir: bool,
    /// Whether each open element's children are in the SVG namespace, innermost last.
    /// The runtime cannot recover this for a fragment — a `@for` row is built while its
    /// anchor is still parked off-tree, and only positioned afterwards — so the lexical
    /// nesting is recorded here and rides out on the template.
    svg_stack: Vec<bool>,
    /// `let __idyll_css_N = merge(…);` statements, emitted before the constructor —
    /// each styled element's class attr and rules read its local.
    style_preludes: Vec<TokenStream2>,
    /// One-shot `(expr)` interpolations, in order — each `(slot, value)` is a
    /// `.place(slot, value)` (text or a `(view)` mount, by the value's `RenderKind`).
    /// Their types are threaded as generics so the template's leaf kinds pick
    /// per-monomorphization; the template stays `&'static`.
    interps: Vec<(u32, Expr)>,
}

/// One reactive leaf of a `live_view!` block: an arm of the block's binding dispatch.
struct Leaf {
    slot: u32,
    kind: LeafKind,
    expr: Expr,
}

enum LeafKind {
    Text,
    Attr(LitStr),
    StyleProp(TokenStream2),
    BoolAttr(LitStr),
}

/// One event mapper: an arm of the block's event dispatch.
struct EventLeaf {
    slot: u32,
    channel: EventChannel,
    /// Written `on…=>(…)`: delivers to this component. `on…=(…)`: relayed to a callback.
    to_mailbox: bool,
    mapper: Expr,
}

/// One structural directive of a `live_view!` block, as arms of the block's fragment
/// dispatch plus its declared kind. `construction` runs in the view expression
/// (eager work: a fixed `@for`'s rows, a keyed source's derivation); `env` runs in
/// the dispatch closure's construction block (capture clones, selector locals).
struct FragmentLeaf {
    slot: u32,
    kind: TokenStream2,
    construction: TokenStream2,
    env: TokenStream2,
    select: Option<TokenStream2>,
    arms: Vec<TokenStream2>,
}

/// The union of several expressions' view captures, deduplicated by identifier —
/// the clone prelude for a dispatch closure whose arms each captured on their own
/// before the leaves shared one closure.
fn union_capture_clones<'a>(exprs: impl Iterator<Item = &'a Expr>) -> TokenStream2 {
    let mut captures: BTreeMap<String, Ident> = BTreeMap::new();
    for expr in exprs {
        let mut collector = CaptureCollector {
            bound: BTreeSet::new(),
            captures: BTreeMap::new(),
        };
        collector.visit_expr(expr);
        for (name, ident) in collector.captures {
            captures.entry(name).or_insert(ident);
        }
    }
    let clones = captures.into_values().map(|ident| {
        quote! {
            let #ident = { #[allow(unused_imports)] use ::idyll::ViewCapture as _; #ident.view_capture() };
        }
    });
    quote! { #(#clones)* }
}

/// Macro-side template IR node (mirrors `idyll::template::TplNode`).
enum IrNode {
    Element {
        tag: String,
        attrs: Vec<(String, String)>,
        /// The element's `css=[…]` merge local and its static `.class` shorthands: the
        /// one class attribute combines both, read from the local at view construction
        /// (which forces the runtime-built template form).
        css_local: Option<(Ident, String)>,
        slot: Option<u32>,
        children: Vec<IrNode>,
    },
    Text(String),
    TextSlot(u32),
    AnchorSlot(u32),
    /// A one-shot `(expr)`: its leaf kind is `<__RenderT{param} as RenderKind>::KIND`,
    /// so the template picks TextSlot/AnchorSlot per-monomorphization. `param` indexes
    /// the interpolant's threaded type generic and its `.place` value.
    Interp { slot: u32, param: usize },
    /// A live marker. `name`/`key` are token streams — a keyed marker's key is
    /// a runtime expression, which forces the whole template into its runtime-built
    /// form.
    Live { name: TokenStream2, key: Option<TokenStream2> },
}

/// Flatten a subtree pre-order into `TplNode` constructor tokens.
fn ir_node_tokens(node: &IrNode, out: &mut Vec<TokenStream2>) {
    match node {
        IrNode::Text(text) => out.push(quote! {
            ::idyll::template::TplNode::Text(::std::borrow::Cow::Borrowed(#text))
        }),
        IrNode::TextSlot(slot) => out.push(quote! {
            ::idyll::template::TplNode::TextSlot(::idyll::driver::SlotId(#slot))
        }),
        IrNode::AnchorSlot(slot) => out.push(quote! {
            ::idyll::template::TplNode::AnchorSlot(::idyll::driver::SlotId(#slot))
        }),
        IrNode::Interp { slot, param } => {
            let tparam = format_ident!("__IdyllRenderT{param}");
            out.push(quote! {
                ::idyll::template::slot_node(#slot, <#tparam as ::idyll::RenderKind>::KIND)
            });
        }
        IrNode::Live { name, key } => {
            let key = match key {
                Some(key) => quote! { ::core::option::Option::Some(#key) },
                None => quote! { ::core::option::Option::None },
            };
            out.push(quote! {
                ::idyll::template::TplNode::Live {
                    name: ::std::borrow::Cow::Borrowed(#name),
                    key: #key,
                    // A `live_view!`/`view!` marker declares no fallback: the need is in the
                    // content plane, where `View::live_mount` carries one.
                    fallback: 0,
                }
            });
        }
        IrNode::Element { tag, attrs, css_local, slot, children } => {
            let attr_tokens = attrs.iter().map(|(name, value)| {
                quote! {
                    ::idyll::template::TplAttr {
                        name: ::std::borrow::Cow::Borrowed(#name),
                        value: ::std::borrow::Cow::Borrowed(#value),
                    }
                }
            });
            // A styled element's class attribute reads the merge local, so its attrs
            // are built (owned) at view construction; unstyled elements stay const.
            let attrs_tokens = match css_local {
                None => quote! { ::std::borrow::Cow::Borrowed(&[#(#attr_tokens),*]) },
                Some((local, static_classes)) => {
                    let class_value = if static_classes.is_empty() {
                        quote! { #local.class_attr.clone() }
                    } else {
                        quote! { ::std::format!("{} {}", #static_classes, #local.class_attr) }
                    };
                    quote! {
                        ::std::borrow::Cow::Owned(::std::vec![
                            #(#attr_tokens,)*
                            ::idyll::template::TplAttr {
                                name: ::std::borrow::Cow::Borrowed("class"),
                                value: ::std::borrow::Cow::Owned(#class_value),
                            },
                        ])
                    }
                }
            };
            let slot_tokens = match slot {
                Some(s) => quote! { ::core::option::Option::Some(::idyll::driver::SlotId(#s)) },
                None => quote! { ::core::option::Option::None },
            };
            let child_count = children.len() as u32;
            out.push(quote! {
                ::idyll::template::TplNode::Element {
                    tag: ::std::borrow::Cow::Borrowed(#tag),
                    attrs: #attrs_tokens,
                    slot: #slot_tokens,
                    children: #child_count,
                }
            });
            for child in children {
                ir_node_tokens(child, out);
            }
        }
    }
}

/// Emit a subtree as `__ir.push(...)` statements — the runtime ctor's form, used when a
/// node carries a runtime expression (a keyed live marker's key).
fn ir_node_stmts(node: &IrNode, out: &mut Vec<TokenStream2>) {
    match node {
        IrNode::Element { tag, attrs, css_local, slot, children } => {
            // The header's direct-child count is structural, so the header builds
            // through the one Element emitter; children recurse in statement form.
            let header_node = IrNode::Element {
                tag: tag.clone(),
                attrs: attrs.clone(),
                css_local: css_local.clone(),
                slot: *slot,
                children: Vec::new(),
            };
            let mut header = Vec::new();
            ir_node_tokens(&header_node, &mut header);
            let count = children.len() as u32;
            let header = &header[0];
            out.push(quote! {
                __ir.push(match (#header) {
                    ::idyll::template::TplNode::Element { tag, attrs, slot, .. } => {
                        ::idyll::template::TplNode::Element {
                            tag,
                            attrs,
                            slot,
                            children: #count,
                        }
                    }
                    node => node,
                });
            });
            for child in children {
                ir_node_stmts(child, out);
            }
        }
        IrNode::Interp { slot, param } => {
            // Runtime-built template: the leaf kind is read from the bound value
            // (the const form reads the type generic instead).
            let local = format_ident!("__idyll_interp{param}");
            out.push(quote! {
                __ir.push(::idyll::template::slot_node(#slot, ::idyll::render_kind_of(&#local)));
            });
        }
        other => {
            let mut flat = Vec::new();
            ir_node_tokens(other, &mut flat);
            for node in flat {
                out.push(quote! { __ir.push(#node); });
            }
        }
    }
}

/// One `@match` arm's generated pieces, threaded from pass 1 (build each arm's view) to pass 2
/// (emit the arm body that selects and returns the view). An arm's component children ride the
/// view's `placement_guards`, so there is no per-arm child tree to thread.
struct ArmPieces {
    pat: TokenStream2,
    capture_clones: TokenStream2,
    view: TokenStream2,
    child_preludes: Vec<TokenStream2>,
}

impl Codegen {
    /// The **root** generator: the top-level `live_view!`, emitting an async render closure whose
    /// component children `await` `mount_child` (the initial tree resolves together).
    fn new() -> Self {
        Self::make(false, false)
    }

    /// A generator for a template written inside another — a `@for` row, an `@if`/`@match` arm,
    /// a slot recipe. It inherits the namespace of the position it was written at and is a
    /// **fragment** scope (synchronous, post-render): its component children `spawn_child`.
    fn nested(svg: bool) -> Self {
        Self::make(svg, true)
    }

    fn make(svg: bool, fragment: bool) -> Self {
        Codegen {
            slot_counter: 0,
            builder_calls: Vec::new(),
            fragment,
            child_awaits: Vec::new(),
            child_counter: 0,
            child_preludes: Vec::new(),
            sender_preludes: Vec::new(),
            leaves: Vec::new(),
            paintings: Vec::new(),
            event_leaves: Vec::new(),
            fragment_leaves: Vec::new(),
            tree: vec![Vec::new()],
            has_runtime_ir: false,
            interps: Vec::new(),
            style_preludes: Vec::new(),
            svg_stack: vec![svg],
        }
    }

    /// The namespace children pushed at this level are created in.
    fn in_svg(&self) -> bool {
        *self.svg_stack.last().expect("an SVG level is always open")
    }

    /// Push an IR node at the current level. Adjacent static text merges at compile
    /// time, so IR node boundaries equal DOM node boundaries in built DOM.
    fn push_ir(&mut self, node: IrNode) {
        let level = self.tree.last_mut().expect("an IR level is always open");
        if let (IrNode::Text(new), Some(IrNode::Text(prev))) = (&node, level.last_mut()) {
            prev.push_str(new);
            return;
        }
        level.push(node);
    }

    /// The right constructor for this template: const `static` IR normally, the
    /// runtime-built form when a node carries a runtime expression (a keyed live).
    fn ctor(&self, static_name: &Ident) -> TokenStream2 {
        let view = if self.has_runtime_ir {
            self.live_view_ctor_runtime()
        } else {
            self.live_view_ctor(static_name)
        };
        // Both forms carry the same fact, so it is said once here: the namespace this
        // template's own nodes are created in, taken from where it was written. A root
        // template is HTML even when its first tag is `svg` — that element enters the
        // namespace itself; this is only about what a *fragment* lands inside.
        match self.in_svg() {
            true => quote! { #view.in_svg() },
            false => view,
        }
    }

    /// The `::idyll::LiveView::new(...)` constructor over this template's const IR.
    /// With one-shot `(expr)` interpolants the template is still `&'static`, but
    /// per-monomorphization: a generic helper's associated const picks each
    /// interpolation leaf (`TextSlot`/`AnchorSlot`) from the interpolant's
    /// `RenderKind` — so text stays a direct `TextSlot`, a view an `AnchorSlot`,
    /// with no runtime build.
    fn live_view_ctor(&self, static_name: &Ident) -> TokenStream2 {
        let mut flat = Vec::new();
        for node in &self.tree[0] {
            ir_node_tokens(node, &mut flat);
        }
        if self.interps.is_empty() {
            return quote! {
                ::idyll::LiveView::new({
                    static #static_name: &[::idyll::template::TplNode] = &[#(#flat),*];
                    #static_name
                })
            };
        }
        let tparams: Vec<Ident> =
            (0..self.interps.len()).map(|i| format_ident!("__IdyllRenderT{i}")).collect();
        let locals: Vec<Ident> =
            (0..self.interps.len()).map(|i| format_ident!("__idyll_interp{i}")).collect();
        quote! {
            ::idyll::LiveView::new({
                struct __IdyllTpl<#(#tparams),*>(::core::marker::PhantomData<(#(#tparams,)*)>);
                trait __IdyllHasNodes {
                    const NODES: &'static [::idyll::template::TplNode];
                }
                impl<#(#tparams: ::idyll::RenderKind),*> __IdyllHasNodes
                    for __IdyllTpl<#(#tparams),*>
                {
                    const NODES: &'static [::idyll::template::TplNode] = &[#(#flat),*];
                }
                fn __idyll_tpl_nodes<#(#tparams: ::idyll::RenderKind),*>(
                    #(_: &#tparams),*
                ) -> &'static [::idyll::template::TplNode] {
                    <__IdyllTpl<#(#tparams),*> as __IdyllHasNodes>::NODES
                }
                __idyll_tpl_nodes(#(&#locals),*)
            })
        }
    }

    /// The runtime-built form: needed when a node carries a runtime expression —
    /// which any `css=[…]` is (the merged class resolves at construction), so in a
    /// styled app most templates build here, not as consts. Keyed live markers build
    /// here too: their wire key is a runtime value.
    fn live_view_ctor_runtime(&self) -> TokenStream2 {
        let mut stmts = Vec::new();
        for node in &self.tree[0] {
            ir_node_stmts(node, &mut stmts);
        }
        // Component splices rebase past the parent's own (post-expansion) slot
        // count, accumulating in document order.
        quote! {
            ::idyll::LiveView::new({
                let mut __ir: ::std::vec::Vec<::idyll::template::TplNode> =
                    ::std::vec::Vec::new();
                #(#stmts)*
                __ir
            })
        }
    }

    fn next_slot(&mut self) -> u32 {
        let s = self.slot_counter;
        self.slot_counter += 1;
        s
    }

    fn gen_nodes(&mut self, nodes: &[Node]) {
        for node in nodes {
            self.gen_node(node);
        }
    }

    fn gen_node(&mut self, node: &Node) {
        match node {
            Node::Text(lit) => {
                // The IR carries decoded meaning; encoding happens only at HTML output.
                self.push_ir(IrNode::Text(lit.value()));
            }

            Node::Splice { expr, reactive } => {
                let slot = self.next_slot();
                if *reactive {
                    // `$sig` — a devirtualised binding, always text (unchanged).
                    self.push_ir(IrNode::TextSlot(slot));
                    self.leaves.push(Leaf { slot, kind: LeafKind::Text, expr: expr.clone() });
                } else {
                    // A one-shot `(expr)`: text or a `(view)` mount, dispatched at
                    // construction by the value's `RenderKind`.
                    let param = self.interps.len();
                    self.interps.push((slot, expr.clone()));
                    self.push_ir(IrNode::Interp { slot, param });
                }
            }


            Node::Element(el) => {
                self.gen_element(el);
            }

            Node::Directive(dir) => {
                self.gen_directive(dir);
            }
        }
    }

    fn gen_element(&mut self, el: &Element) {

        // Dynamic attributes/events bind through a slot on the element; a reactive
        // css entry makes the class attribute itself a binding.
        let has_dynamic_css = el.css.iter().any(|entry| !matches!(entry, CssEntryAst::Static(_)));
        let needs_slot = el
            .attrs
            .iter()
            .any(|a| {
                matches!(
                    a,
                    Attr::Value { .. }
                        | Attr::Bool { .. }
                        | Attr::StyleProp { .. }
                        | Attr::Painting { .. }
                )
            })
            || !el.events.is_empty()
            || has_dynamic_css;
        let slot = if needs_slot { Some(self.next_slot()) } else { None };

        // Dynamic attrs → leaf arms against the slot.
        for attr in &el.attrs {
            match attr {
                Attr::Value { name, expr } => {
                    let s = slot.expect("value attr implies a slot");
                    let attr_name_lit = LitStr::new(&attr_html_name(name), name.span());
                    self.leaves.push(Leaf {
                        slot: s,
                        kind: LeafKind::Attr(attr_name_lit),
                        expr: expr.clone(),
                    });
                }
                Attr::StyleProp { name, expr, .. } => {
                    let s = slot.expect("style prop implies a slot");
                    self.leaves.push(Leaf {
                        slot: s,
                        kind: LeafKind::StyleProp(name.clone()),
                        expr: expr.clone(),
                    });
                }
                Attr::Painting { expr, .. } => {
                    let s = slot.expect("a painting implies a slot");
                    self.paintings.push(PaintingLeaf { slot: s, expr: expr.clone() });
                }
                Attr::Bool { name, expr } => {
                    let s = slot.expect("bool attr implies a slot");
                    let attr_name_lit = LitStr::new(&attr_html_name(name), name.span());
                    self.leaves.push(Leaf {
                        slot: s,
                        kind: LeafKind::BoolAttr(attr_name_lit),
                        expr: expr.clone(),
                    });
                }
            }
        }

        // Events → event arms.
        for ev in &el.events {
            let s = slot.expect("event implies a slot");
            self.event_leaves.push(EventLeaf {
                slot: s,
                channel: ev.channel.clone(),
                to_mailbox: ev.to_mailbox,
                mapper: ev.mapper.clone(),
            });
        }

        // Children — their own IR level, and their own namespace: `svg` enters it,
        // `foreignObject` is the door back out to HTML.
        let tag = el.tag.to_string();
        self.svg_stack.push(match tag.as_str() {
            "svg" => true,
            "foreignObject" => false,
            _ => self.in_svg(),
        });
        self.tree.push(Vec::new());
        self.gen_nodes(&el.children);
        let ir_children = self.tree.pop().expect("IR level balanced");
        self.svg_stack.pop();

        // The element itself: static attrs are the class/id shorthands (dynamic attrs
        // and events bind through `slot` instead; `css=[…]` folds into the one class
        // attribute below).
        let static_classes = el
            .classes
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(" ");

        let static_styles: Vec<&Expr> = el
            .css
            .iter()
            .filter_map(|entry| match entry {
                CssEntryAst::Static(style) => Some(style),
                _ => None,
            })
            .collect();
        let css_local = if static_styles.is_empty() {
            None
        } else {
            // Merge once at view construction; the element's class attr and (on the
            // all-static path) the template's rules read the local, and the view
            // stops being const. Under reactive entries this base merge is only the
            // template attribute — the binding's first paint carries the true state.
            self.has_runtime_ir = true;
            let local = format_ident!("__idyll_css_{}", self.style_preludes.len());
            self.style_preludes.push(quote! {
                let #local = ::idyll_styles::merge(&[#((#static_styles).atoms()),*]);
            });
            if !has_dynamic_css {
                self.builder_calls.push(quote! {
                    .with_styles(#local.rules.clone())
                });
            }
            Some((local, static_classes.clone()))
        };

        if has_dynamic_css {
            self.has_runtime_ir = true;
            // Delivery: every statically nameable entry's rules ride in-band — the
            // union, not the merge, because last-wins losers can activate at runtime.
            // Open entries rest on the document's universe sheet.
            let nameable: Vec<&Expr> = el.css.iter().filter_map(CssEntryAst::nameable_style).collect();
            if !nameable.is_empty() {
                self.builder_calls.push(quote! {
                    .with_styles(::idyll_styles::rule_union(&[#((#nameable).atoms()),*]))
                });
            }
            // The class attribute becomes an ordinary attr binding: re-merge the
            // present entries in list order (the same last-wins law as the static
            // path) and write the whole attribute.
            let values = el.css.iter().map(CssEntryAst::value_tokens);
            let class_attr = if static_classes.is_empty() {
                quote! { __idyll_merged.class_attr }
            } else {
                quote! { ::std::format!("{} {}", #static_classes, __idyll_merged.class_attr) }
            };
            let arm = quote! {{
                let mut __idyll_sel: ::std::vec::Vec<&'static [::idyll_styles::Atom]> =
                    ::std::vec::Vec::new();
                #(
                    if let ::core::option::Option::Some(__idyll_atoms) =
                        ::idyll_styles::CssEntry::atoms_if(#values)
                    {
                        __idyll_sel.push(__idyll_atoms);
                    }
                )*
                let __idyll_merged = ::idyll_styles::merge(&__idyll_sel);
                #class_attr
            }};
            self.leaves.push(Leaf {
                slot: slot.expect("dynamic css implies a slot"),
                kind: LeafKind::Attr(LitStr::new("class", el.tag.span())),
                expr: Expr::Verbatim(arm),
            });
        }

        let mut ir_attrs: Vec<(String, String)> = Vec::new();
        if css_local.is_none() && !static_classes.is_empty() {
            ir_attrs.push(("class".to_string(), static_classes.clone()));
        }
        if let Some(id) = &el.id {
            ir_attrs.push(("id".to_string(), id.to_string()));
        }

        self.push_ir(IrNode::Element {
            tag,
            attrs: ir_attrs,
            css_local,
            slot,
            children: ir_children,
        });
    }

    fn gen_directive(&mut self, dir: &Directive) {
        match dir {
            Directive::LiveCall { expr } => {
                // `path(args)` was the eager view splice; it is gone. A built view
                // interpolates through `(expr)` (the value path), and a component wraps
                // markup with a `{ … }` children block. The error underlines the call
                // itself — the parser held its span, so the refusal keeps it.
                let error = syn::Error::new_spanned(
                    expr,
                    "`Name(args)` (the eager view splice) is gone — interpolate a built \
                     view with `(expr)`, or wrap a component with a `{ … }` children block",
                );
                self.sender_preludes.push(error.to_compile_error());
            }
            Directive::If {
                condition,
                then_nodes,
                else_nodes,
                keep,
            } => {
                let slot = self.next_slot();
                self.push_ir(IrNode::AnchorSlot(slot));
                let frame = format_ident!("__idyll_frame");

                let mut then_gen = Codegen::nested(self.in_svg());
                then_gen.gen_nodes(then_nodes);
                let (then_bindings, then_places) = then_gen.interp_parts();
                let then_ctor = then_gen.ctor(&format_ident!("__IDYLL_TPL"));
                let then_block = then_gen.leaf_block();
                let then_calls = then_gen.builder_calls;
                let then_preludes = {
                    let style = then_gen.style_preludes;
                    let sender = then_gen.sender_preludes;
                    quote! { #(#sender)* #(#style)* }
                };
                let then_capture_clones = branch_capture_clones(then_nodes);
                let then_child_preludes = then_gen.child_preludes;
                let then_view =
                    quote! { #then_ctor #(#then_places)* #(#then_calls)* #then_block };
                // A component-bearing arm `spawn_child`s in its build, so the dispatch closure
                // needs the frame captured.
                let mut needs_frame = then_gen.child_counter > 0;

                // Clones in `env` protect the enclosing scope (the dispatch closure captures
                // clones, not originals); clones INSIDE each arm keep the branch re-buildable
                // (`Fn`) — each build clones afresh. An arm's component children ride the arm
                // view's `placement_guards` (via `.placement_guard` calls in its builder calls),
                // so switching arms drops the old view and reaps its subtree — no branch slot.
                let cond_clones = expr_capture_clones(condition);
                let mut env = quote! { #cond_clones #then_capture_clones };
                let has_else = else_nodes.is_some();

                let then_arm = quote! {
                    {
                        #then_capture_clones
                        #(#then_bindings)*
                        #then_preludes
                        #(#then_child_preludes)*
                        ::idyll::live_view::FragmentOut::LiveView({ #then_view })
                    }
                };
                let mut arms = vec![then_arm];
                if let Some(else_nodes) = else_nodes {
                    let mut else_gen = Codegen::nested(self.in_svg());
                    else_gen.gen_nodes(else_nodes);
                    let (else_bindings, else_places) = else_gen.interp_parts();
                    let else_ctor = else_gen.ctor(&format_ident!("__IDYLL_TPL"));
                    let else_block = else_gen.leaf_block();
                    let else_calls = else_gen.builder_calls;
                    let else_preludes = {
                        let style = else_gen.style_preludes;
                        let sender = else_gen.sender_preludes;
                        quote! { #(#sender)* #(#style)* }
                    };
                    let else_capture_clones = branch_capture_clones(else_nodes);
                    let else_child_preludes = else_gen.child_preludes;
                    needs_frame = needs_frame || else_gen.child_counter > 0;
                    env = quote! { #env #else_capture_clones };
                    let else_view =
                        quote! { #else_ctor #(#else_places)* #(#else_calls)* #else_block };
                    arms.push(quote! {
                        {
                            #else_capture_clones
                            #(#else_bindings)*
                            #else_preludes
                            #(#else_child_preludes)*
                            ::idyll::live_view::FragmentOut::LiveView({ #else_view })
                        }
                    });
                }
                let arm_count = arms.len() as u32;
                if needs_frame {
                    env = quote! { let #frame = #frame.clone(); #env };
                }

                let select_body = quote! {
                    ::idyll::live_view::FragmentOut::Selected(if (#condition) as bool {
                        ::core::option::Option::Some(0)
                    } else if #has_else {
                        ::core::option::Option::Some(1)
                    } else {
                        ::core::option::Option::None
                    })
                };

                self.fragment_leaves.push(FragmentLeaf {
                    slot,
                    kind: quote! {
                        ::idyll::live_view::FragmentSource::Ready(
                            ::idyll::live_view::FragmentKind::Branch { keep: #keep, arms: #arm_count },
                        )
                    },
                    construction: TokenStream2::new(),
                    env,
                    select: Some(select_body),
                    arms,
                });
            }

            Directive::For {
                pat,
                iter,
                key,
                body,
            } => {
                let slot = self.next_slot();
                self.push_ir(IrNode::AnchorSlot(slot));

                let mut body_gen = Codegen::nested(self.in_svg());
                body_gen.gen_nodes(body);
                let (body_bindings, body_places) = body_gen.interp_parts();
                let body_ctor = body_gen.ctor(&format_ident!("__IDYLL_TPL"));
                let body_block = body_gen.leaf_block();
                let body_calls = body_gen.builder_calls;
                let body_preludes = {
                    let style = body_gen.style_preludes;
                    let sender = body_gen.sender_preludes;
                    quote! { #(#sender)* #(#style)* }
                };
                let body_child_preludes = body_gen.child_preludes;
                // A component-bearing row `spawn_child`s in its build, riding the row view's
                // `placement_guards` — so a row removal (the source's `SpliceOp::Remove`) drops
                // the row view and reaps its subtree. The rebuild closure needs the frame.
                let needs_frame = body_gen.child_counter > 0;
                let frame = format_ident!("__idyll_frame");
                let mut bound = BTreeSet::new();
                collect_pat_idents(pat, &mut bound);
                let body_capture_clones = branch_capture_clones_with_bound(body, &bound);

                // The one list form: the binding shape comes from the source — a
                // SignalVec binds `(Row, MutableSignal<T>)` (reactive), a plain
                // iterator binds `T` by value (fixed rows; legal in Static pages).
                // The pattern destructures whichever the source provides.
                // `into_parts` runs at view construction: it declares the fragment's
                // structural kind and — for a fixed source — builds the one-time rows
                // through the eager closure, which is never stored. A live row
                // rebuild goes through the `Row` arm: a typed `read_binding` and the
                // body inlined, plain code in this block's dispatch.
                let index = self.fragment_leaves.len() as u32;
                let kind_local = format_ident!("__idyll_for_kind_{}", index);
                let cx = cx_ident();
                // A keyed `@for`'s pattern binds the row's **cell** (the key is
                // identity, consumed by the source's key fn); an unkeyed one binds
                // whatever the source provides.
                // The source is captured *here*, eagerly, exactly as an unkeyed one is;
                // only the list's construction waits for a scope.
                let (source, bind, keyed) = if let Some(key) = key {
                    (
                        quote! {
                            ::idyll::KeyedVec::derived(
                                __idyll_owner,
                                move |#cx| __idyll_values.get(#cx),
                                move |__idyll_item| {
                                    let #pat = __idyll_item;
                                    (#key)
                                },
                            )
                        },
                        quote! {
                            let (__idyll_key, __idyll_cell) = __idyll_binding;
                            let _ = __idyll_key;
                            let #pat = __idyll_cell;
                        },
                        true,
                    )
                } else {
                    (
                        quote! {
                            { #[allow(unused_imports)] use ::idyll::ViewCapture as _; (#iter).view_capture() }
                        },
                        quote! { let #pat = __idyll_binding; },
                        false,
                    )
                };
                self.fragment_leaves.push(FragmentLeaf {
                    slot,
                    kind: quote! { #kind_local },
                    construction: {
                        // The row builder returns the row body view; its component children ride
                        // the view's `placement_guards`.
                        let rows = quote! {
                            {
                                #body_capture_clones
                                move |__idyll_binding| {
                                    #body_capture_clones
                                    #bind
                                    #(#body_bindings)*
                                    #body_preludes
                                    #(#body_child_preludes)*
                                    #body_ctor #(#body_places)* #(#body_calls)* #body_block
                                }
                            }
                        };
                        let frame_bind = if needs_frame {
                            quote! { let #frame = #frame.clone(); }
                        } else {
                            TokenStream2::new()
                        };
                        let build_parts =
                            quote! { ::idyll::live_view::ForFragmentSource::into_parts(#source, #rows) };
                        if keyed {
                            quote! {
                                let #kind_local = ::idyll::live_view::FragmentSource::Deferred({
                                    #[allow(unused_imports)] use ::idyll::ViewCapture as _;
                                    let __idyll_values = (#iter).view_capture();
                                    #frame_bind
                                    ::std::boxed::Box::new(move |__idyll_owner: &::idyll::Owner| {
                                        #build_parts
                                    })
                                });
                            }
                        } else {
                            quote! {
                                let #kind_local = ::idyll::live_view::FragmentSource::Ready({
                                    #frame_bind
                                    #build_parts
                                });
                            }
                        }
                    },
                    env: TokenStream2::new(),
                    select: None,
                    arms: Vec::new(),
                });
            }

            Directive::Match { expr, arms, keep } => {
                let slot = self.next_slot();
                self.push_ir(IrNode::AnchorSlot(slot));

                let match_value = format_ident!("__idyll_match_{}", slot);
                let cx = cx_ident();

                let selector_arms = arms.iter().enumerate().map(|(idx, arm)| {
                    let pat = &arm.pat;
                    quote! { #pat => #idx }
                });

                let mut env = quote! { let #match_value = (#expr).clone(); };
                let mut needs_frame = false;
                // Pass 1: build each arm's view pieces. A component-bearing arm rides its view's
                // `placement_guards` (spawn_child in its builder calls), so an arm switch drops the
                // old view and reaps its subtree — no branch slot.
                let arm_data: Vec<ArmPieces> = arms
                    .iter()
                    .map(|arm| {
                        let pat = &arm.pat;
                        let mut arm_gen = Codegen::nested(self.in_svg());
                        arm_gen.gen_nodes(&arm.body);
                        let (arm_bindings, arm_places) = arm_gen.interp_parts();
                        let arm_ctor = arm_gen.ctor(&format_ident!("__IDYLL_TPL"));
                        let arm_block = arm_gen.leaf_block();
                        let arm_calls = arm_gen.builder_calls;
                        let arm_preludes = {
                            let style = arm_gen.style_preludes;
                            let sender = arm_gen.sender_preludes;
                            quote! { #(#sender)* #(#style)* }
                        };
                        let mut bound = BTreeSet::new();
                        collect_pat_idents(pat, &mut bound);
                        let arm_capture_clones =
                            branch_capture_clones_with_bound(&arm.body, &bound);
                        env = quote! { #env #arm_capture_clones };
                        needs_frame = needs_frame || arm_gen.child_counter > 0;
                        let view = quote! {
                            #(#arm_bindings)* #arm_preludes #arm_ctor #(#arm_places)* #(#arm_calls)* #arm_block
                        };
                        ArmPieces {
                            pat: quote! { #pat },
                            capture_clones: arm_capture_clones,
                            view,
                            child_preludes: arm_gen.child_preludes,
                        }
                    })
                    .collect();
                let arm_count = arm_data.len() as u32;
                if needs_frame {
                    let frame = format_ident!("__idyll_frame");
                    env = quote! { let #frame = #frame.clone(); #env };
                }
                // Pass 2: arm bodies. Each just returns its view (children ride placement_guards).
                let arm_bodies: Vec<TokenStream2> = arm_data
                    .into_iter()
                    .map(|ArmPieces { pat, capture_clones: cap, view, child_preludes }| {
                        quote! {
                            {
                                #cap
                                match #match_value.get(#cx) {
                                    #pat => {
                                        #(#child_preludes)*
                                        ::idyll::live_view::FragmentOut::LiveView({ #view })
                                    }
                                    _ => ::idyll::live_view::FragmentOut::None,
                                }
                            }
                        }
                    })
                    .collect();

                self.fragment_leaves.push(FragmentLeaf {
                    slot,
                    kind: quote! {
                        ::idyll::live_view::FragmentSource::Ready(
                            ::idyll::live_view::FragmentKind::Branch { keep: #keep, arms: #arm_count },
                        )
                    },
                    construction: TokenStream2::new(),
                    env,
                    select: Some(quote! {
                        ::idyll::live_view::FragmentOut::Selected(::core::option::Option::Some(
                            #[allow(unused_variables)]
                            {
                                match #match_value.get(#cx) {
                                    #(#selector_arms,)*
                                }
                            },
                        ))
                    }),
                    arms: arm_bodies,
                });
            }

            Directive::Component { component, required, optional, children } => {
                let slot = self.next_slot();
                self.push_ir(IrNode::AnchorSlot(slot));
                // `name=>(…)` among a child's props is a way back to *this* component: it
                // binds to this inbox, exactly as an event mapper does.
                let sender = format_ident!("__idyll_sender");
                let frame = format_ident!("__idyll_frame");
                let n = self.child_counter;
                self.child_counter += 1;

                // A `{ … }` children block is the `children` slot: build a recipe from the block
                // (a self-contained view closure over this parent's frame) and pass the receiver
                // as the `children` prop. Each placement spawns its own instance; nothing rides
                // this parent's mount beyond the receiver.
                let mut required = required.clone();
                if let Some(children_nodes) = children {
                    let slot_local = format_ident!("__idyll_slot_{}", n);
                    // The recipe is an ordinary render closure (root/async): `build_slot` spawns
                    // it per placement and awaits it, so its own embedded children resolve just
                    // like a top-level tree.
                    let mut recipe_gen = Codegen::new();
                    recipe_gen.gen_nodes(children_nodes);
                    let recipe = recipe_gen.finish(&format_ident!("__IDYLL_SLOT_TPL"), children_nodes);
                    // Clone the block's free vars (and the sender) into this scope, so the recipe
                    // re-clones them per placement (it is `Fn`) and the parent view keeps its own.
                    let free = free_captures(children_nodes, &BTreeSet::new());
                    let free_clones = free.into_values().map(|ident| {
                        quote! { let #ident = { #[allow(unused_imports)] use ::idyll::ViewCapture as _; #ident.view_capture() }; }
                    });
                    self.child_preludes.push(quote! {
                        let #slot_local = {
                            #(#free_clones)*
                            ::idyll::ctx::build_slot(
                                #sender.clone(),
                                move |__idyll_scope| (#recipe)(__idyll_scope),
                            )
                        };
                    });
                    required.push((format_ident!("children"), syn::parse_quote!(#slot_local), false));
                }
                let required = &required;

                // The props outlive the setup that built them, so every value they read is
                // captured by value — the discipline every other closure in a view follows.
                let req_clones = required.iter().map(|(_, expr, _)| expr_capture_clones(expr));
                let opt_clones = optional.iter().map(|(_, expr, _)| expr_capture_clones(expr));
                let field = |(name, expr, to_mailbox): &(Ident, Expr, bool)| match to_mailbox {
                    true => quote! { #name: ::idyll::callback_from_sender(#sender.clone(), #expr) },
                    false => quote! { #name: #expr },
                };
                let req = required.iter().map(field);
                // An optional prop's field is `Option<T>`; supplying it is `Some`, and the
                // ones the call site left out come from `Default`.
                let opt = optional.iter().map(|(name, expr, to_mailbox)| {
                    let value = match to_mailbox {
                        true => quote! { ::idyll::callback_from_sender(#sender.clone(), #expr) },
                        false => quote! { #expr },
                    };
                    quote! { #name: ::core::option::Option::Some(#value) }
                });
                // Mint the child id, record its anchor on the template. The prop structs are
                // reached through the `Component` trait, so a call site imports only the
                // component — not its `…Required`/`…Optional` pair; a local `type` alias lets the
                // trait-projected type carry struct-literal syntax.
                let cid = format_ident!("__idyll_child_{}", n);
                self.child_preludes.push(quote! { let #cid = ::idyll::fresh_child_id(); });
                self.builder_calls.push(quote! { .child_anchor(#slot, #cid) });
                let build_required = quote! {{
                    type __IdyllRequired = <#component as ::idyll::Component>::Required;
                    #(#req_clones)*
                    __IdyllRequired { #(#req,)* }
                }};
                let build_optional = quote! {{
                    type __IdyllOptional = <#component as ::idyll::Component>::Optional;
                    #(#opt_clones)*
                    __IdyllOptional {
                        #(#opt,)*
                        ..::core::default::Default::default()
                    }
                }};
                if self.fragment {
                    // Post-render (arm/row/recipe): spawn fire-and-forget; the cancel-guard rides
                    // this view's `placement_guards`, so it unmounts with the arm/row.
                    self.builder_calls.push(quote! {
                        .placement_guard(::idyll::spawn_child::<#component>(
                            &#frame, #build_required, #build_optional, #cid,
                        ))
                    });
                } else {
                    // Initial tree: resolve the child to render, propagating a pre-render fault as
                    // this mount's own render failure. The guard is collected into the render's
                    // returned `Vec<MountGuard>`.
                    self.child_awaits.push(quote! {
                        __idyll_guards.push(
                            ::idyll::mount_child::<#component>(
                                &#frame, #build_required, #build_optional, #cid,
                            ).await?,
                        );
                    });
                }
            }

            Directive::Content { .. } => {
                unreachable!("live_view! rejects @content in validate_no_eager_content")
            }

            Directive::Live { name, key } => {
                // A pure IR marker — no slot, no builder machinery; `LiveView::new`
                // derives the live list from the IR itself.
                let name_tokens = island_name_expr(name, key.as_ref());
                let key_tokens = key.as_ref().map(|expr| {
                    quote! { ::idyll::live::wire_key::<#name>(#expr) }
                });
                if key_tokens.is_some() {
                    self.has_runtime_ir = true;
                }
                self.push_ir(IrNode::Live { name: name_tokens, key: key_tokens });
            }
        }
    }

    /// One-shot interpolants as `(bindings, places)`: each `let local = expr;`
    /// (the ctor reads `&local` to pick the leaf kind) and the `.place(slot, local)`
    /// that moves it (text → one-time `SetText`, view → anchor mount). The same parts
    /// serve `finish` and every branch/row arm (each has its own generator).
    fn interp_parts(&self) -> (Vec<TokenStream2>, Vec<TokenStream2>) {
        let bindings = self
            .interps
            .iter()
            .enumerate()
            .map(|(i, (_, expr))| {
                let local = format_ident!("__idyll_interp{i}");
                quote! { let #local = #expr; }
            })
            .collect();
        let places = self
            .interps
            .iter()
            .enumerate()
            .map(|(i, (slot, _))| {
                let local = format_ident!("__idyll_interp{i}");
                quote! { .place(#slot, #local) }
            })
            .collect();
        (bindings, places)
    }

    fn finish(self, template_var: &Ident, nodes: &[Node]) -> TokenStream2 {
        let (bindings, places) = self.interp_parts();
        let ctor = self.ctor(template_var);
        let calls = &self.builder_calls;
        let preludes = &self.style_preludes;
        let sender_preludes = &self.sender_preludes;
        let block = self.leaf_block();
        // The wrapper captures by value, so every name the view uses is cloned here
        // first and the surrounding code keeps its own.
        let outer_clones = {
            // A one-shot `(expr)` is evaluated into its own local just below, which
            // *moves* what it names — a view placed there is not a value to clone.
            let mut placed = BTreeSet::new();
            for (_, expr) in &self.interps {
                let mut collector = CaptureCollector {
                    bound: BTreeSet::new(),
                    captures: BTreeMap::new(),
                };
                collector.visit_expr(expr);
                placed.extend(collector.captures.into_keys());
            }
            let clones = free_captures(nodes, &placed).into_values().map(|ident| {
                quote! {
                    let #ident = { #[allow(unused_imports)] use ::idyll::ViewCapture as _; #ident.view_capture() };
                }
            });
            quote! { #(#clones)* }
        };
        let sender = format_ident!("__idyll_sender");
        let frame = format_ident!("__idyll_frame");
        let child_preludes = &self.child_preludes;
        let child_awaits = &self.child_awaits;
        let view_expr = quote! { #ctor #(#places)* #(#calls)* #block };
        // The closure receives a `RenderScope` (this mount's sender and frame); it binds both so
        // the view's event mappers, child-lambdas, and `spawn_child`/`mount_child` calls reach
        // them. Every name the view uses is cloned outside the closure first (`outer_clones`), so
        // the surrounding code keeps its own.
        let bind_scope = quote! {
            let #sender = __idyll_scope.sender().clone();
            let #frame = __idyll_scope.frame().clone();
            let _ = (&#sender, &#frame);
        };
        let closure = if self.fragment {
            // A synchronous, post-render recipe (a slot instance): its component children
            // `spawn_child` (guards ride the view's `placement_guards`), so it just returns the
            // `LiveView`.
            quote! {
                move |__idyll_scope: ::idyll::RenderScope<_>| {
                    #bind_scope
                    #(#sender_preludes)*
                    #(#child_preludes)*
                    #view_expr
                }
            }
        } else {
            // The root render closure: async and fallible. Resolve each top-level child to its
            // render point (a pre-render fault propagates as this mount's render failure), then fold
            // their cancel-guards onto the view's `placement_guards` — the same channel reactive
            // fragments and slots use, so `wire_view` reaps the whole subtree when the view unmounts.
            let body = if child_awaits.is_empty() {
                quote! { ::core::result::Result::<_, ::idyll::Fault>::Ok(#view_expr) }
            } else {
                quote! {
                    let mut __idyll_guards: ::std::vec::Vec<::idyll::MountGuard> = ::std::vec::Vec::new();
                    #(#child_awaits)*
                    let mut __idyll_view = #view_expr;
                    __idyll_view.placement_guards.extend(__idyll_guards);
                    ::core::result::Result::<_, ::idyll::Fault>::Ok(__idyll_view)
                }
            };
            quote! {
                move |__idyll_scope: ::idyll::RenderScope<_>| async move {
                    #bind_scope
                    #(#sender_preludes)*
                    #(#child_preludes)*
                    #body
                }
            }
        };
        quote! {
            {
                #outer_clones
                #(#bindings)*
                #(#preludes)*
                #closure
            }
        }
    }

    /// The view's one dispatch block: every binding leaf as an arm of `run`, every
    /// event mapper as an arm of `event`, every structural directive as arms of
    /// `fragment` — a monomorphic `match` each, in the component's own code. Empty
    /// when the view has no reactive leaves.
    fn leaf_block(&self) -> TokenStream2 {
        if self.leaves.is_empty()
            && self.event_leaves.is_empty()
            && self.fragment_leaves.is_empty()
            && self.paintings.is_empty()
        {
            return TokenStream2::new();
        }
        let binding_slots = self.leaves.iter().map(|leaf| {
            let s = leaf.slot;
            quote! { ::idyll::driver::SlotId(#s) }
        });
        let paintings = self.paintings.iter().map(|canvas| {
            let s = canvas.slot;
            let expr = &canvas.expr;
            quote! {
                ::idyll::live_view::PaintingDecl {
                    slot: ::idyll::driver::SlotId(#s),
                    layers: ::idyll::canvas::painting(#expr),
                }
            }
        });
        let event_slots = self.event_leaves.iter().map(|ev| {
            let s = ev.slot;
            // The channel was decided at the parse and rides here as a type — the
            // wire never re-derives it from a name.
            let binding = match &ev.channel {
                EventChannel::Measure => quote! { ::idyll::live_view::EventBinding::Measure },
                EventChannel::Dom(name) => quote! { ::idyll::live_view::EventBinding::Dom(#name) },
            };
            quote! { (::idyll::driver::SlotId(#s), #binding) }
        });

        let run = if self.leaves.is_empty() {
            quote! { ::std::rc::Rc::new(|_: &::idyll::Cx, _: &[::idyll::driver::NodeId], _| ::core::option::Option::None) }
        } else {
            let cx = cx_ident();
            let clones = union_capture_clones(self.leaves.iter().map(|leaf| &leaf.expr));
            let arms = self.leaves.iter().enumerate().map(|(position, leaf)| {
                let k = position as u32;
                let node = quote! { __idyll_nodes[#position] };
                let expr = &leaf.expr;
                let op = match &leaf.kind {
                    LeafKind::Text => quote! {
                        ::idyll::driver::DomOp::SetText {
                            node_id: #node,
                            text: ::std::string::ToString::to_string(&(#expr)),
                        }
                    },
                    LeafKind::Attr(name) => quote! {
                        ::idyll::driver::DomOp::SetAttr {
                            node_id: #node,
                            name: #name,
                            value: ::std::string::ToString::to_string(&(#expr)),
                        }
                    },
                    LeafKind::StyleProp(name) => quote! {
                        ::idyll::driver::DomOp::SetStyleProp {
                            node_id: #node,
                            name: #name,
                            value: ::std::string::ToString::to_string(&(#expr)),
                        }
                    },
                    LeafKind::BoolAttr(name) => quote! {
                        ::idyll::driver::DomOp::SetBoolAttr {
                            node_id: #node,
                            name: #name,
                            value: (#expr) as bool,
                        }
                    },
                };
                quote! { #k => #op }
            });
            // `cx` is exposed so an arm opts into reactivity via `.get(cx)`;
            // `let _ = &cx;` keeps it from warning when every leaf is static.
            quote! {{
                #clones
                ::std::rc::Rc::new(
                    move |#cx: &::idyll::Cx, __idyll_nodes: &[::idyll::driver::NodeId], __idyll_idx: u32| {
                        let _ = &#cx;
                        ::core::option::Option::Some(match __idyll_idx {
                            #(#arms,)*
                            _ => return ::core::option::Option::None,
                        })
                    },
                )
            }}
        };

        let event = if self.event_leaves.is_empty() {
            quote! { ::std::rc::Rc::new(|_, _| ::core::option::Option::None) }
        } else {
            let clones = union_capture_clones(self.event_leaves.iter().map(|ev| &ev.mapper));
            let arms = self.event_leaves.iter().enumerate().map(|(position, ev)| {
                let k = position as u32;
                // A closure literal is a fresh temporary in each dispatch — pass it by
                // value; the `Event`-annotated wrapper closure fixes its parameter type.
                // A mapper held in a variable is passed by reference (`&F` is `Fn` when
                // `F: Fn`) so it is not moved out of the reusable dispatch closure.
                let mapper_expr = match &ev.mapper {
                    Expr::Closure(_) => ev.mapper.to_token_stream(),
                    named => quote! { &#named },
                };
                let deliver = if ev.to_mailbox {
                    // `=>`: the mapper names this component's message, so it delivers
                    // here. Whether it yields `M` or `Option<M>` is resolved by type
                    // (see `MapperCall`), not by inspecting the expression.
                    quote! {{
                        use ::idyll::live_view::{DeliverMsg as _, DeliverOption as _};
                        (&::idyll::live_view::mapper(#mapper_expr)).deliver(__idyll_event)
                    }}
                } else {
                    // `=`: a Callback, already bound to whoever wrote the lambda that made
                    // it. The event goes there and this component says nothing of its own.
                    quote! {{
                        (#mapper_expr).call(__idyll_event);
                        ::core::option::Option::None
                    }}
                };
                quote! { #k => #deliver }
            });
            quote! {{
                #clones
                ::std::rc::Rc::new(move |__idyll_idx: u32, __idyll_event: ::idyll::Event| {
                    match __idyll_idx {
                        #(#arms,)*
                        _ => ::core::option::Option::None,
                    }
                })
            }}
        };

        let constructions = self.fragment_leaves.iter().map(|leaf| &leaf.construction);
        let decls = self.fragment_leaves.iter().map(|leaf| {
            let s = leaf.slot;
            let kind = &leaf.kind;
            quote! {
                ::idyll::live_view::FragmentDecl { slot: ::idyll::driver::SlotId(#s), kind: #kind }
            }
        });
        let fragment = if self.fragment_leaves.is_empty() {
            quote! { ::std::rc::Rc::new(|_: &::idyll::Cx, _| ::idyll::live_view::FragmentOut::None) }
        } else {
            let cx = cx_ident();
            let envs = self.fragment_leaves.iter().map(|leaf| &leaf.env);
            let op_arms = self.fragment_leaves.iter().enumerate().flat_map(|(index, leaf)| {
                let f = index as u32;
                let mut arms = Vec::new();
                if let Some(select) = &leaf.select {
                    arms.push(quote! { ::idyll::live_view::FragmentOp::Select(#f) => #select, });
                }
                for (position, arm) in leaf.arms.iter().enumerate() {
                    let a = position as u32;
                    arms.push(quote! {
                        ::idyll::live_view::FragmentOp::Arm { fragment: #f, arm: #a } => #arm,
                    });
                }
                arms
            });
            quote! {{
                #(#envs)*
                ::std::rc::Rc::new(
                    move |#cx: &::idyll::Cx, __idyll_op: ::idyll::live_view::FragmentOp| {
                        let _ = &#cx;
                        match __idyll_op {
                            #(#op_arms)*
                            _ => ::idyll::live_view::FragmentOut::None,
                        }
                    },
                )
            }}
        };

        quote! {
            .block({
                #(#constructions)*
                ::idyll::live_view::Block {
                    binding_slots: ::std::vec![#(#binding_slots),*],
                    paintings: ::std::vec![#(#paintings),*],
                    event_slots: ::std::vec![#(#event_slots),*],
                    fragments: ::std::vec![#(#decls),*],
                    run: #run,
                    event: #event,
                    fragment: #fragment,
                }
            })
        }
    }
}

/// The reactive-context identifier the generated binding closures take. **Hygienic**
/// (`mixed_site`): user code cannot name it, so the only way to read a signal reactively
/// is `$signal` (which the macro expands to `signal.get(<this>)`). That keeps `Cx` — and
/// its "never stored, never smuggled" guarantee — entirely inside the framework.
fn cx_ident() -> Ident {
    // The `__idyll_` prefix keeps the capture collector's exemption exact: a USER
    // local named `cx` is an ordinary capture (cloned like any other), because
    // nothing the user writes can collide with this name by convention — hygiene
    // (`mixed_site`) already prevented collision by resolution.
    Ident::new("__idyll_cx", Span::mixed_site())
}

/// A live's wire name: the table ident, kebab-cased (`queue_viz` → `"queue-viz"`).
/// The marker's name expression — and the marker's **arity check**. A keyed marker
/// resolves its name straight off the def and proves the key type through
/// [`wire_key`](idyll::live::wire_key); a keyless one goes through `keyless_name`,
/// which demands `Key = NoKey`. So omitting `key = …` on a keyed live is a type
/// error at the marker rather than a mount that cannot build the component's args.
fn island_name_expr(name: &syn::Path, key: Option<&Expr>) -> TokenStream2 {
    match key {
        Some(_) => quote! { <#name as ::idyll::live::LiveDef>::NAME },
        None => quote! { ::idyll::live::keyless_name::<#name>() },
    }
}

fn island_name(component: &Ident) -> String {
    component.to_string().replace('_', "-")
}

/// Elements the HTML parser auto-closes a `<p>` for (block-level content in a `<p>` gets
/// hoisted out, so the served page would not parse back to the authored structure).
fn is_p_closing(tag: &str) -> bool {
    matches!(
        tag,
        "address" | "article" | "aside" | "blockquote" | "details" | "div" | "dl" | "fieldset"
            | "figcaption" | "figure" | "footer" | "form" | "h1" | "h2" | "h3" | "h4" | "h5"
            | "h6" | "header" | "hr" | "main" | "menu" | "nav" | "ol" | "p" | "pre" | "section"
            | "table" | "ul"
    )
}

/// Reject the known non-fixed-points of the HTML parser this grammar can produce
/// (see the call site). Deliberately a **blocklist of caught shapes**, not a proof:
/// the parser's restructuring rules are larger than what is checked here (e.g. bare
/// text directly inside `table` foster-parents out), and an uncaught shape surfaces
/// as a hydration mismatch, which the claim machinery reports loudly.
/// `parent` is the enclosing element's tag, if any — control-flow bodies inherit it,
/// since their rows render into the same live parent.
fn validate_parser_fixed_points(nodes: &[Node], parent: Option<&Ident>) -> syn::Result<()> {
    let parent_tag = parent.map(|p| p.to_string());
    for node in nodes {
        match node {
            Node::Element(el) => {
                let tag = el.tag.to_string();
                if parent_tag.as_deref() == Some("table")
                    && matches!(tag.as_str(), "tr" | "td" | "th")
                {
                    return Err(syn::Error::new(
                        el.tag.span(),
                        format!(
                            "`<{tag}>` directly inside `<table>` is restructured by the HTML \
                             parser (it inserts `<tbody>`), which would break hydration — \
                             write the `tbody`/`thead` explicitly"
                        ),
                    ));
                }
                if parent_tag.as_deref() == Some("p") && is_p_closing(&tag) {
                    return Err(syn::Error::new(
                        el.tag.span(),
                        format!(
                            "`<{tag}>` inside `<p>` is hoisted out by the HTML parser \
                             (`<p>` auto-closes), which would break hydration — use an \
                             inline element or move it out of the paragraph"
                        ),
                    ));
                }
                if tag == "pre" {
                    if let Some(Node::Text(first)) = el.children.first() {
                        if first.value().starts_with('\n') {
                            return Err(syn::Error::new(
                                first.span(),
                                "a leading newline inside `<pre>` is dropped by the HTML \
                                 parser, which would break hydration — start the text \
                                 without it",
                            ));
                        }
                    }
                }
                validate_parser_fixed_points(&el.children, Some(&el.tag))?;
            }
            Node::Directive(directive) => match directive {
                Directive::If { then_nodes, else_nodes, .. } => {
                    validate_parser_fixed_points(then_nodes, parent)?;
                    if let Some(else_nodes) = else_nodes {
                        validate_parser_fixed_points(else_nodes, parent)?;
                    }
                }
                Directive::For { body, .. } => validate_parser_fixed_points(body, parent)?,
                Directive::Match { arms, .. } => {
                    for arm in arms {
                        validate_parser_fixed_points(&arm.body, parent)?;
                    }
                }
                // A spliced component's content was validated against its OWN root
                // context when it was built — a parser-fixed-point violation across
                // the splice boundary (a `<div>`-rooted view under a `<p>`) is the
                // stated, accepted blind spot of eager value composition.
                Directive::Component { .. }
                | Directive::Content { .. }
                | Directive::Live { .. }
                | Directive::LiveCall { .. } => {}
            },
            Node::Text(_) | Node::Splice { .. } => {}
        }
    }
    Ok(())
}

/// Clone-prologue for one generated closure: every free ident its expression captures
/// is shadowed by a clone first, so the closure moves the clone and the surrounding
/// scope keeps the original. `Copy` captures (signals) clone for free; plain data (an
/// `@each` row) can then feed any number of bindings/conditions in one body.
fn expr_capture_clones(expr: &Expr) -> TokenStream2 {
    let mut collector = CaptureCollector {
        bound: BTreeSet::new(),
        captures: BTreeMap::new(),
    };
    collector.visit_expr(expr);
    let clones = collector.captures.into_values().map(|ident| {
        quote! {
            let #ident = { #[allow(unused_imports)] use ::idyll::ViewCapture as _; #ident.view_capture() };
        }
    });
    quote! { #(#clones)* }
}

fn branch_capture_clones(nodes: &[Node]) -> TokenStream2 {
    branch_capture_clones_with_bound(nodes, &BTreeSet::new())
}

fn branch_capture_clones_with_bound(nodes: &[Node], bound: &BTreeSet<String>) -> TokenStream2 {
    let captures = free_captures(nodes, bound);
    let clones = captures.into_values().map(|ident| {
        quote! {
            let #ident = { #[allow(unused_imports)] use ::idyll::ViewCapture as _; #ident.view_capture() };
        }
    });
    // A branch that spawns a child needs its own handle on the inbox: the branch is
    // `Fn`, rebuilt on every change, and each build binds fresh callbacks.
    let sender = spawns_child(nodes).then(|| {
        let sender = format_ident!("__idyll_sender");
        quote! { let #sender = #sender.clone(); }
    });
    quote! { #(#clones)* #sender }
}

/// Whether this subtree mounts a component — and so needs the enclosing inbox in scope.
fn spawns_child(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| match node {
        Node::Element(element) => spawns_child(&element.children),
        Node::Directive(Directive::Component { .. }) => true,
        Node::Directive(Directive::If {
            then_nodes,
            else_nodes,
            ..
        }) => {
            spawns_child(then_nodes)
                || else_nodes.as_deref().is_some_and(spawns_child)
        }
        Node::Directive(Directive::For { body, .. }) => spawns_child(body),
        Node::Directive(Directive::Match { arms, .. }) => {
            arms.iter().any(|arm| spawns_child(&arm.body))
        }
        _ => false,
    })
}

fn free_captures(nodes: &[Node], bound: &BTreeSet<String>) -> BTreeMap<String, Ident> {
    let mut collector = CaptureCollector {
        bound: bound.clone(),
        captures: BTreeMap::new(),
    };
    collector.visit_nodes(nodes);
    collector.captures
}

struct CaptureCollector {
    bound: BTreeSet<String>,
    captures: BTreeMap<String, Ident>,
}

impl CaptureCollector {
    fn visit_nodes(&mut self, nodes: &[Node]) {
        for node in nodes {
            self.visit_node(node);
        }
    }

    fn visit_node(&mut self, node: &Node) {
        match node {
            Node::Element(el) => {
                for attr in &el.attrs {
                    match attr {
                        Attr::Value { expr, .. }
                        | Attr::Bool { expr, .. }
                        | Attr::StyleProp { expr, .. }
                        | Attr::Painting { expr, .. } => {
                            self.visit_expr(expr);
                        }
                    }
                }
                for entry in &el.css {
                    match entry {
                        CssEntryAst::Static(style) => self.visit_expr(style),
                        CssEntryAst::Conditional { cond, style } => {
                            self.visit_expr(cond);
                            self.visit_expr(style);
                        }
                        CssEntryAst::Dynamic(expr) => self.visit_expr(expr),
                    }
                }
                for event in &el.events {
                    self.visit_expr(&event.mapper);
                }
                self.visit_nodes(&el.children);
            }
            Node::Splice { expr, .. } => self.visit_expr(expr),
            Node::Directive(Directive::If {
                condition,
                then_nodes,
                else_nodes,
                ..
            }) => {
                self.visit_expr(condition);
                self.visit_nodes(then_nodes);
                if let Some(else_nodes) = else_nodes {
                    self.visit_nodes(else_nodes);
                }
            }
            Node::Directive(Directive::For {
                pat,
                iter,
                key,
                body,
            }) => {
                self.visit_expr(iter);
                // The key is read per row, so the row's own bindings are in scope for it.
                self.with_bound_pat(pat, |this| {
                    if let Some(key) = key {
                        this.visit_expr(key);
                    }
                    this.visit_nodes(body);
                });
            }
            Node::Directive(Directive::Match { expr, arms, .. }) => {
                self.visit_expr(expr);
                for arm in arms {
                    self.with_bound_pat(&arm.pat, |this| this.visit_nodes(&arm.body));
                }
            }
            Node::Directive(Directive::Component { required, optional, children, .. }) => {
                for (_, expr, _) in required.iter().chain(optional.iter()) {
                    self.visit_expr(expr);
                }
                // The children block becomes a slot recipe built in this closure, so its free
                // vars are captures this closure must clone too.
                if let Some(children) = children {
                    self.visit_nodes(children);
                }
            }
            Node::Directive(Directive::Content { expr })
            | Node::Directive(Directive::LiveCall { expr }) => self.visit_expr(expr),
            Node::Directive(Directive::Live { key, .. }) => {
                if let Some(key) = key {
                    self.visit_expr(key);
                }
            }
            Node::Text(_) => {}
        }
    }

    fn with_bound_pat(&mut self, pat: &syn::Pat, f: impl FnOnce(&mut Self)) {
        let previous = self.bound.clone();
        collect_pat_idents(pat, &mut self.bound);
        f(self);
        self.bound = previous;
    }

    fn capture_ident(&mut self, ident: &Ident) {
        let name = ident.to_string();
        // The macro's own reactive token (`__idyll_cx`, inserted by `$` desugaring)
        // is supplied per generated closure, never captured from the surrounding
        // scope. The reserved prefix makes the exemption exact: a user local
        // spelled `cx` is an ordinary capture and clones like any other.
        if name == "__idyll_cx" {
            return;
        }
        if !self.bound.contains(&name)
            && name
                .chars()
                .next()
                .is_some_and(|ch| !ch.is_ascii_uppercase())
        {
            self.captures.entry(name).or_insert_with(|| ident.clone());
        }
    }
}

impl<'ast> Visit<'ast> for CaptureCollector {
    fn visit_expr_block(&mut self, node: &'ast syn::ExprBlock) {
        let previous = self.bound.clone();
        for stmt in &node.block.stmts {
            match stmt {
                syn::Stmt::Local(local) => {
                    if let Some(init) = &local.init {
                        self.visit_expr(&init.expr);
                        if let Some((_else, diverge)) = &init.diverge {
                            self.visit_expr(diverge);
                        }
                    }
                    collect_pat_idents(&local.pat, &mut self.bound);
                }
                syn::Stmt::Item(_) => {}
                syn::Stmt::Expr(expr, _) => self.visit_expr(expr),
                syn::Stmt::Macro(mac) => visit::visit_stmt_macro(self, mac),
            }
        }
        self.bound = previous;
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        let previous = self.bound.clone();
        for input in &node.inputs {
            collect_pat_idents(input, &mut self.bound);
        }
        self.visit_expr(&node.body);
        self.bound = previous;
    }

    fn visit_expr_path(&mut self, node: &'ast syn::ExprPath) {
        if node.qself.is_none() && node.path.segments.len() == 1 {
            self.capture_ident(&node.path.segments[0].ident);
        } else {
            visit::visit_expr_path(self, node);
        }
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        // syn does not descend into a macro's token stream, so a capture inside one
        // (`format!("{}", sig.get(cx))`) would be missed and left un-cloned. Best-effort:
        // parse the tokens as comma-separated expressions and visit each, so the capture
        // is cloned like any other. Method and field names are not `ExprPath`, so they
        // are never mistaken for captures; tokens that don't parse as expressions (a
        // pattern in `matches!`) are simply skipped.
        if let Ok(exprs) = syn::parse2::<CommaExprs>(node.tokens.clone()) {
            for expr in &exprs.0 {
                self.visit_expr(expr);
            }
        }
    }
}

/// Comma-separated expressions — the shape of most macro arguments (`format!`, `vec!`,
/// print-family). Used to recover captures from inside a macro invocation.
struct CommaExprs(syn::punctuated::Punctuated<Expr, syn::Token![,]>);

impl syn::parse::Parse for CommaExprs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        Ok(CommaExprs(syn::punctuated::Punctuated::parse_terminated(input)?))
    }
}

fn collect_pat_idents(pat: &syn::Pat, bound: &mut BTreeSet<String>) {
    match pat {
        syn::Pat::Ident(pat) => {
            bound.insert(pat.ident.to_string());
            if let Some((_at, subpat)) = &pat.subpat {
                collect_pat_idents(subpat, bound);
            }
        }
        syn::Pat::Tuple(pat) => {
            for elem in &pat.elems {
                collect_pat_idents(elem, bound);
            }
        }
        syn::Pat::TupleStruct(pat) => {
            for elem in &pat.elems {
                collect_pat_idents(elem, bound);
            }
        }
        syn::Pat::Struct(pat) => {
            for field in &pat.fields {
                collect_pat_idents(&field.pat, bound);
            }
        }
        syn::Pat::Reference(pat) => collect_pat_idents(&pat.pat, bound),
        syn::Pat::Slice(pat) => {
            for elem in &pat.elems {
                collect_pat_idents(elem, bound);
            }
        }
        syn::Pat::Or(pat) => {
            for case in &pat.cases {
                collect_pat_idents(case, bound);
            }
        }
        syn::Pat::Paren(pat) => collect_pat_idents(&pat.pat, bound),
        syn::Pat::Type(pat) => collect_pat_idents(&pat.pat, bound),
        syn::Pat::Rest(_)
        | syn::Pat::Wild(_)
        | syn::Pat::Lit(_)
        | syn::Pat::Path(_)
        | syn::Pat::Macro(_) => {}
        _ => {}
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

/// Maud-inspired view macro. Produces a `LiveView<M>` value.
///
/// # Syntax
///
/// Reactivity is **explicit**, and its one spelling is `$signal`: a `$`-read
/// tracks the signal, so the binding re-runs when it changes. Everything else —
/// `(expr)` interpolation, a literal, a destructured pattern binding — evaluates
/// once at construction and never again. (There is no ambient `cx` to call
/// `.get` on; the read capability is the macro's own, unnameable by design.)
///
/// ```ignore
/// live_view! {
///     div.card#main {
///         h2 { "Title" }
///         input onkeydown=>(key::enter(|e: idyll::Event| Msg::Submit(e.value())))
///         @if ($count > 0) { p { "Shown" } } else { p { "Hidden" } }
///         span { $count }        // reactive: re-renders when `count` changes
///         span { (started_at) }  // one-shot: painted once at mount
///         @match $mode {         // the scrutinee is a signal, read reactively
///             Mode::Read => { span { "Read" } },
///             Mode::Edit(label) => { span { (label) } },
///         }
///     }
/// }
/// ```
///
/// - `.class` / `#id` — static class/id shortcuts
/// - `attr=($sig)` — string attribute; reactive through the `$` read
/// - `attr[$sig]` — boolean attribute, gated by the signal
/// - `on*=>(mapper)` — event to this inbox; `Fn(Event) -> M` or `-> Option<M>`
/// - `$sig` / `(expr)` — reactive text vs. one-shot interpolation
/// - `"string"` — static text
/// - `@if ($cond) { … } else { … }` — branch; `[keep]` retains the inactive arm
/// - `@match $signal { Pattern => { … } }` — reactive pattern branch
/// - `@for pat in $list [key = …] { … }` — keyed reactive list
#[proc_macro]
pub fn live_view(input: TokenStream) -> TokenStream {
    let ViewInput { nodes } = parse_macro_input!(input as ViewInput);

    // Reject markup shapes the HTML parser would restructure: the served SSR page is
    // re-parsed by the browser, and the claim walks the IR against that parse — so
    // templates must be **fixed points of the parser**. The macro holds the tree, so
    // these are clean compile errors instead of silent hydration corruption.
    if let Err(error) = validate_parser_fixed_points(&nodes, None) {
        return error.to_compile_error().into();
    }
    if let Err(error) = validate_islands_keyed_in_rows(&nodes, false) {
        return error.to_compile_error().into();
    }
    if let Err(error) = validate_no_eager_content(&nodes) {
        return error.to_compile_error().into();
    }

    let mut gen = Codegen::new();
    gen.gen_nodes(&nodes);

    static TMPL_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = TMPL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmpl_ident = format_ident!("__IDYLL_TMPL_{}", n);

    let output = gen.finish(&tmpl_ident, &nodes);
    output.into()
}

/// `view! { … }` — build **content**: [`View`] IR as a plain value, the
/// `vec!` of the content plane. Same markup grammar as `live_view!`, compiled eagerly —
/// `@if`/`@for`/`@match` are ordinary Rust control flow over the data, text and
/// attributes evaluate at construction, `@live(app::live::Name)` markers are IR.
///
/// There are no closures, so there is no `cx`, so nothing here **can** read a signal:
/// content is a pure function of the data in scope. `$` and `on*` handlers are
/// compile errors naming the token — reactivity lives in components (`live_view!`).
#[proc_macro]
pub fn view(input: TokenStream) -> TokenStream {
    let stream: TokenStream2 = input.clone().into();
    if let Some(span) = find_dollar(&stream) {
        return syn::Error::new(
            span,
            "`$` reads a signal — `view!` builds content, a plain value; \
             read the signal in the component (with its `cx`) and pass the data in, \
             or render this markup with `live_view!`",
        )
        .to_compile_error()
        .into();
    }
    let ViewInput { nodes } = parse_macro_input!(input as ViewInput);
    if let Err(error) = validate_parser_fixed_points(&nodes, None) {
        return error.to_compile_error().into();
    }
    match view_nodes(&nodes) {
        Ok(stmts) => quote! {{
            let mut __styles: ::std::vec::Vec<::idyll::StyleRule> = ::std::vec::Vec::new();
            let mut __nodes: ::std::vec::Vec<::idyll::template::TplNode> = ::std::vec::Vec::new();
            #(#stmts)*
            let _ = (&mut __styles, &mut __nodes);
            ::idyll::View::from_ir(__nodes, __styles)
        }}
        .into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Live rows splice: an unkeyed live's `(name, document-order instance)` identity
/// would migrate between rows on every list splice — the buried positional
/// convention, resurrected. Keyed rows demand keyed live. (`view!` is exempt:
/// its `@for` is plain iteration inside one value, replaced wholesale — no splice
/// ever moves identity within content.)
fn validate_islands_keyed_in_rows(nodes: &[Node], in_for: bool) -> Result<()> {
    for node in nodes {
        match node {
            Node::Element(el) => validate_islands_keyed_in_rows(&el.children, in_for)?,
            Node::Directive(Directive::If { then_nodes, else_nodes, .. }) => {
                validate_islands_keyed_in_rows(then_nodes, in_for)?;
                if let Some(nodes) = else_nodes {
                    validate_islands_keyed_in_rows(nodes, in_for)?;
                }
            }
            Node::Directive(Directive::For { body, .. }) => {
                validate_islands_keyed_in_rows(body, true)?;
            }
            Node::Directive(Directive::Match { arms, .. }) => {
                for arm in arms {
                    validate_islands_keyed_in_rows(&arm.body, in_for)?;
                }
            }
            // A component's children block is a slot recipe rebuilt per placement —
            // positional identity inside it is exactly as fatal as anywhere else.
            Node::Directive(Directive::Component { children: Some(children), .. }) => {
                validate_islands_keyed_in_rows(children, in_for)?;
            }
            Node::Directive(Directive::Live { name, key: None, .. }) if in_for => {
                return Err(syn::Error::new_spanned(
                    name,
                    "an unkeyed live inside `@for` is positional identity under a \
                     splicing list — rows demand keyed live: `@live(Def, key = …)`",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// `@content` is `view!`'s eager splice; a live view has no eager children — it
/// *places* content with `(expr)` (a `View`, or a tracked `Signal<View>`).
fn validate_no_eager_content(nodes: &[Node]) -> Result<()> {
    for node in nodes {
        match node {
            Node::Element(el) => validate_no_eager_content(&el.children)?,
            Node::Directive(Directive::If { then_nodes, else_nodes, .. }) => {
                validate_no_eager_content(then_nodes)?;
                if let Some(nodes) = else_nodes {
                    validate_no_eager_content(nodes)?;
                }
            }
            Node::Directive(Directive::For { body, .. }) => validate_no_eager_content(body)?,
            Node::Directive(Directive::Match { arms, .. }) => {
                for arm in arms {
                    validate_no_eager_content(&arm.body)?;
                }
            }
            Node::Directive(Directive::Component { children: Some(children), .. }) => {
                validate_no_eager_content(children)?;
            }
            Node::Directive(Directive::Content { expr }) => {
                return Err(syn::Error::new_spanned(
                    expr,
                    "`@content(…)` is `view!`'s eager splice — a live view places \
                     content with `(expr)` (a `View`, or a tracked `Signal<View>`)",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// A `$` anywhere in the token stream, recursively — content cannot read signals, and
/// the refusal names the exact token.
fn find_dollar(stream: &TokenStream2) -> Option<Span> {
    for tree in stream.clone() {
        match tree {
            proc_macro2::TokenTree::Punct(p) if p.as_char() == '$' => return Some(p.span()),
            proc_macro2::TokenTree::Group(g) => {
                if let Some(span) = find_dollar(&g.stream()) {
                    return Some(span);
                }
            }
            _ => {}
        }
    }
    None
}

/// Statements that push a node list's IR into the `__nodes` vec in scope (and style
/// rules into the one flat `__styles`). Adjacent text literals merge here, exactly as
/// the reactive codegen merges them, so both emitters agree node-for-node.
fn view_nodes(nodes: &[Node]) -> Result<Vec<TokenStream2>> {
    let mut stmts = Vec::new();
    let mut pending_text = String::new();
    let flush_text = |stmts: &mut Vec<TokenStream2>, pending: &mut String| {
        if !pending.is_empty() {
            let text = pending.clone();
            stmts.push(quote! {
                __nodes.push(::idyll::template::TplNode::Text(
                    ::std::borrow::Cow::Borrowed(#text),
                ));
            });
            pending.clear();
        }
    };
    for node in nodes {
        match node {
            Node::Text(lit) => pending_text.push_str(&lit.value()),
            Node::Splice { expr, .. } => {
                flush_text(&mut stmts, &mut pending_text);
                stmts.push(quote! {
                    __nodes.push(::idyll::template::TplNode::Text(
                        ::std::borrow::Cow::Owned(::std::string::ToString::to_string(&(#expr))),
                    ));
                });
            }
            Node::Element(el) => {
                flush_text(&mut stmts, &mut pending_text);
                stmts.push(view_element(el)?);
            }
            Node::Directive(dir) => {
                flush_text(&mut stmts, &mut pending_text);
                stmts.push(view_directive(dir)?);
            }
        }
    }
    flush_text(&mut stmts, &mut pending_text);
    Ok(stmts)
}

fn view_element(el: &Element) -> Result<TokenStream2> {
    if let Some(event) = el.events.first() {
        return Err(syn::Error::new(
            el.tag.span(),
            format!(
                "`{}` is a live binding — content (`view!`) cannot receive events; \
                 render this element in a component with `live_view!`",
                match &event.channel {
                    EventChannel::Measure => "measure".to_string(),
                    EventChannel::Dom(name) => format!("on{name}"),
                }
            ),
        ));
    }

    let tag = el.tag.to_string();
    let static_classes = el
        .classes
        .iter()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ");

    // Attribute order mirrors the mounted fold exactly: the template's static attrs
    // first (class shorthand when unstyled, id, then the merged css class), then the
    // written attributes in source order (the fold applies their SetAttrs in builder
    // order, after the template's own).
    let mut attr_stmts = Vec::new();
    if el.css.is_empty() && !static_classes.is_empty() {
        attr_stmts.push(quote! {
            __attrs.push(::idyll::template::TplAttr {
                name: ::std::borrow::Cow::Borrowed("class"),
                value: ::std::borrow::Cow::Borrowed(#static_classes),
            });
        });
    }
    if let Some(id) = &el.id {
        let id = id.to_string();
        attr_stmts.push(quote! {
            __attrs.push(::idyll::template::TplAttr {
                name: ::std::borrow::Cow::Borrowed("id"),
                value: ::std::borrow::Cow::Borrowed(#id),
            });
        });
    }
    if !el.css.is_empty() {
        let styles = el
            .css
            .iter()
            .map(|entry| match entry {
                CssEntryAst::Static(style) => Ok(style),
                CssEntryAst::Conditional { cond, .. } => Err(syn::Error::new_spanned(
                    cond,
                    "reactive style entries read signals — content (`view!`) is inert; \
                     resolve the variant in the component and pass the chosen `Style` in",
                )),
                CssEntryAst::Dynamic(expr) => Err(syn::Error::new_spanned(
                    expr,
                    "reactive style entries read signals — content (`view!`) is inert; \
                     resolve the variant in the component and pass the chosen `Style` in",
                )),
            })
            .collect::<Result<Vec<_>>>()?;
        let class_value = if static_classes.is_empty() {
            quote! { __css.class_attr.clone() }
        } else {
            quote! { ::std::format!("{} {}", #static_classes, __css.class_attr) }
        };
        attr_stmts.push(quote! {
            let __css = ::idyll_styles::merge(&[#((#styles).atoms()),*]);
            __attrs.push(::idyll::template::TplAttr {
                name: ::std::borrow::Cow::Borrowed("class"),
                value: ::std::borrow::Cow::Owned(#class_value),
            });
            __styles.extend(__css.rules.clone());
        });
    }
    for attr in &el.attrs {
        match attr {
            Attr::Value { name, expr } => {
                let name = attr_html_name(name);
                attr_stmts.push(quote! {
                    __attrs.push(::idyll::template::TplAttr {
                        name: ::std::borrow::Cow::Borrowed(#name),
                        value: ::std::borrow::Cow::Owned(
                            ::std::string::ToString::to_string(&(#expr)),
                        ),
                    });
                });
            }
            Attr::Bool { name, expr } => {
                // HTML boolean semantics, as the fold writes them: present-and-empty
                // when true, absent when false.
                let name = attr_html_name(name);
                attr_stmts.push(quote! {
                    if (#expr) as bool {
                        __attrs.push(::idyll::template::TplAttr {
                            name: ::std::borrow::Cow::Borrowed(#name),
                            value: ::std::borrow::Cow::Borrowed(""),
                        });
                    }
                });
            }
            Attr::StyleProp { span, .. } => {
                return Err(syn::Error::new(
                    *span,
                    "`style:prop` is a live binding — in content, write the declaration \
                     in `style=(…)`",
                ));
            }
            Attr::Painting { span, .. } => {
                return Err(syn::Error::new(
                    *span,
                    "`painting` is a live binding — a picture is drawn by the browser, \
                     and content (`view!`) is what the server paints; render the canvas \
                     in a component with `live_view!`",
                ));
            }
        }
    }

    let children = view_nodes(&el.children)?;
    Ok(quote! {
        {
            let mut __attrs: ::std::vec::Vec<::idyll::template::TplAttr> =
                ::std::vec::Vec::new();
            #(#attr_stmts)*
            let __children: ::std::vec::Vec<::idyll::template::TplNode> = {
                let mut __nodes: ::std::vec::Vec<::idyll::template::TplNode> =
                    ::std::vec::Vec::new();
                #(#children)*
                __nodes
            };
            __nodes.push(::idyll::template::TplNode::Element {
                tag: ::std::borrow::Cow::Borrowed(#tag),
                attrs: ::std::borrow::Cow::Owned(__attrs),
                slot: ::core::option::Option::None,
                children: ::idyll::template::root_count(&__children),
            });
            __nodes.extend(__children);
        }
    })
}

fn view_directive(dir: &Directive) -> Result<TokenStream2> {
    match dir {
        Directive::If { condition, then_nodes, else_nodes, keep } => {
            if *keep {
                // Content is eager Rust control flow — there is no mounted branch to
                // retain, so accepting `[keep]` would silently promise semantics
                // content cannot have.
                return Err(syn::Error::new_spanned(
                    condition,
                    "`@if[keep]` is a live retention semantic — content (`view!`) is \
                     plain control flow; drop the `[keep]`",
                ));
            }
            let then_stmts = view_nodes(then_nodes)?;
            let else_stmts = match else_nodes {
                Some(nodes) => view_nodes(nodes)?,
                None => Vec::new(),
            };
            Ok(quote! {
                if #condition { #(#then_stmts)* } else { #(#else_stmts)* }
            })
        }
        Directive::For { pat, iter, key, body } => {
            if let Some(key) = key {
                return Err(syn::Error::new_spanned(
                    key,
                    "`[key = …]` is a live reconciliation semantic — content (`view!`) \
                     is a plain loop with no rows to key; drop the bracket",
                ));
            }
            let body_stmts = view_nodes(body)?;
            Ok(quote! {
                for #pat in #iter { #(#body_stmts)* }
            })
        }
        Directive::Match { expr, arms, keep } => {
            if *keep {
                return Err(syn::Error::new_spanned(
                    expr,
                    "`@match[keep]` is a live retention semantic — content (`view!`) is \
                     plain control flow; drop the `[keep]`",
                ));
            }
            let arm_tokens = arms
                .iter()
                .map(|arm| {
                    let pat = &arm.pat;
                    let body = view_nodes(&arm.body)?;
                    Ok(quote! { #pat => { #(#body)* } })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(quote! {
                match #expr { #(#arm_tokens)* }
            })
        }
        Directive::Live { name, .. } => Err(syn::Error::new_spanned(
            name,
            "in content, calling the component IS the mount — `live::Check(frag)`, \
             `live::Rows()` — the call form is the one spelling (`@live` remains only \
             for markers placed from a `live_view!`)",
        )),
        Directive::Content { expr } => Ok(quote! {
            {
                let __content: ::idyll::View = #expr;
                let __tpl = __content.into_template();
                __nodes.extend(__tpl.nodes.into_owned());
                __styles.extend(__tpl.styles.into_owned());
            }
        }),
        Directive::Component { component, .. } => Err(syn::Error::new_spanned(
            component,
            "child components are live — mount them from a component (`live_view!`), \
             not from content",
        )),
        // In content the component call IS the mount: the callee is the component's
        // `LiveDef` (its content-plane handle), the one argument its typed key. No
        // component runs here — a first-class `TplNode::Live` marker is placed, and
        // content never crosses it: a live component reads its content field.
        Directive::LiveCall { expr } => {
            let Expr::Call(call) = expr else {
                return Err(syn::Error::new_spanned(
                    expr,
                    "a call in content mounts a live component — the callee is its \
                     `LiveDef` path (`live::Rows()`, `live::Check(frag)`)",
                ));
            };
            let Expr::Path(callee) = call.func.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &call.func,
                    "a live mount's callee is the component's `LiveDef` path",
                ));
            };
            let name = &callee.path;
            let key: Option<Expr> = match call.args.len() {
                0 => None,
                1 => Some(call.args[0].clone()),
                _ => {
                    return Err(syn::Error::new_spanned(
                        &call.args,
                        "a live mount call carries a typed key and nothing else — \
                         everything a live component needs beyond its identity comes \
                         from the store",
                    ));
                }
            };
            let name_tokens = island_name_expr(name, key.as_ref());
            let key_tokens = match &key {
                Some(expr) => quote! {
                    ::core::option::Option::Some(::idyll::live::wire_key::<#name>(#expr))
                },
                None => quote! { ::core::option::Option::None },
            };
            Ok(quote! {
                __nodes.push(::idyll::template::TplNode::Live {
                    name: ::std::borrow::Cow::Borrowed(#name_tokens),
                    key: #key_tokens,
                    fallback: 0,
                });
            })
        }
    }
}

/// `#[styles] mod styles { … }` — the one place typed styles exist (see
/// `idyll-styles`). The module must be named `styles` (one per file — the file is the
/// class identity) and each `pub const NAME: Style = css! {{ … }}` initializer is a
/// JS-object literal rewritten here at expansion.
#[proc_macro_attribute]
pub fn styles(attr: TokenStream, item: TokenStream) -> TokenStream {
    style::styles_impl(attr, item)
}

/// `props! { pub Card { margin_top, … } }` — a constrained property set: a marker type
/// plus `type CardStyle = Style<Card>`, so a `css!` body annotated `CardStyle` may only
/// write the listed properties.
#[proc_macro]
pub fn props(input: TokenStream) -> TokenStream {
    style::props_impl(input)
}

/// `#[component]` on an `async fn` — the only way to write a component.
///
/// ```ignore
/// #[idyll::component]
/// pub async fn Slider(
///     ctx: Ctx<Setup, Never>,
///     at: Signal<f64>,
///     moved: Callback<f64>,
///     #[opt] invite: Signal<bool>,
/// ) -> idyll::Result { … }
/// ```
///
/// A component is a *type* implementing [`Component`](idyll::Component); this fn is its
/// body, and the two prop structs plus the impl are generated together so they cannot
/// drift apart. The message type is read off `ctx`, where a reader already looks for it.
///
/// `#[opt]` marks the half a call site may omit (`?invite=(…)`), and the body receives it
/// as `Option<T>`. It is an attribute rather than a sigil because rustc parses this
/// signature before the macro runs: `?invite` and `~invite` are parse errors there, and an
/// attribute is the one extension point the grammar offers. It is read and dropped — it
/// never reaches name resolution, so nothing needs to define `opt`.
#[proc_macro_attribute]
pub fn component(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as syn::ItemFn);
    match component_decl(item) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// One prop, read off a parameter: its name, its type, and whether `#[opt]` was on it.
struct PropDecl {
    optional: bool,
    name: Ident,
    ty: syn::Type,
}

/// The message type, read out of the first parameter's `Ctx<Setup, M>`.
fn msg_of_ctx(ty: &syn::Type) -> Result<syn::Type> {
    let wrong = || {
        syn::Error::new_spanned(
            ty,
            "a component takes its `ctx: Ctx<Setup, M>` first — M is the message type it \
             handles, or `Never` if it handles none",
        )
    };
    let syn::Type::Path(path) = ty else { return Err(wrong()) };
    let syn::PathArguments::AngleBracketed(args) =
        &path.path.segments.last().ok_or_else(wrong)?.arguments
    else {
        return Err(wrong());
    };
    match args.args.last() {
        Some(syn::GenericArgument::Type(msg)) => Ok(msg.clone()),
        _ => Err(wrong()),
    }
}

fn component_decl(item: syn::ItemFn) -> Result<TokenStream2> {
    let syn::ItemFn { attrs, vis, sig, block: body } = item;
    let name = sig.ident;
    if !name.to_string().starts_with(|c: char| c.is_ascii_uppercase()) {
        return Err(syn::Error::new(
            name.span(),
            "a component's name is capitalised — that is what tells it from an element at a \
             call site",
        ));
    }
    if sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &name,
            "a component is an `async fn` — setup awaits its data before it renders",
        ));
    }

    let mut params = sig.inputs.into_iter();
    let Some(syn::FnArg::Typed(ctx_arg)) = params.next() else {
        return Err(syn::Error::new(
            name.span(),
            "a component takes its `ctx: Ctx<Setup, M>` first, then its props",
        ));
    };
    let msg = msg_of_ctx(&ctx_arg.ty)?;
    // The ctx parameter is emitted as written, not as `Ctx<Setup, Self::Msg>`: it has to
    // name that type to compile, and spelling it back means the author's `use` of `Ctx`
    // and `Setup` is a real use.
    let (ctx_pat, ctx_ty) = (ctx_arg.pat, ctx_arg.ty);

    let props = params
        .map(|arg| {
            let syn::FnArg::Typed(arg) = arg else {
                return Err(syn::Error::new_spanned(
                    arg,
                    "a component is a free fn, not a method — its state is its props",
                ));
            };
            let syn::Pat::Ident(pat) = arg.pat.as_ref() else {
                return Err(syn::Error::new_spanned(
                    &arg.pat,
                    "a prop is named, because the call site names it — destructure it in the body",
                ));
            };
            Ok(PropDecl {
                optional: arg.attrs.iter().any(|a| a.path().is_ident("opt")),
                name: pat.ident.clone(),
                ty: (*arg.ty).clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let (required, optional): (Vec<_>, Vec<_>) = props.iter().partition(|p| !p.optional);
    let required_ty = format_ident!("{}Required", name);
    let optional_ty = format_ident!("{}Optional", name);

    let req_fields = required.iter().map(|p| {
        let (n, t) = (&p.name, &p.ty);
        quote! { #vis #n: #t }
    });
    let opt_fields = optional.iter().map(|p| {
        let (n, t) = (&p.name, &p.ty);
        quote! { #vis #n: ::core::option::Option<#t> }
    });
    let req_names = required.iter().map(|p| &p.name);
    let opt_names = optional.iter().map(|p| &p.name);

    Ok(quote! {
        #vis struct #required_ty { #(#req_fields,)* }

        #[derive(Default)]
        #vis struct #optional_ty { #(#opt_fields,)* }

        #(#attrs)*
        #vis struct #name;

        impl ::idyll::Component for #name {
            type Msg = #msg;
            type Required = #required_ty;
            type Optional = #optional_ty;

            async fn run(
                #ctx_pat: #ctx_ty,
                required: Self::Required,
                optional: Self::Optional,
            ) -> ::idyll::Result {
                let #required_ty { #(#req_names,)* } = required;
                let #optional_ty { #(#opt_names,)* } = optional;
                #body
            }
        }
    })
}

/// `#[node]` marks a struct as a **Node**: a globally-identified entity, fetchable by id
/// and normalized in the cache. It requires an `id` field and emits, alongside the
/// (derive-augmented) struct: the field-marker module + `HasField` impls (so it can be a
/// fragment target), an empty `Record` impl, and a `Node` impl inferred from `id`.
#[proc_macro_attribute]
pub fn node(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::ItemStruct);
    match schema_type_impl(input, true) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// `#[value]` marks a struct as an **embedded value type**: fields but no global identity,
/// not independently fetchable. It emits the field-marker module + `HasField` impls and an
/// empty `Record` impl — but **no** `Node`/`id`, so it can only be reached inline through a
/// parent edge.
#[proc_macro_attribute]
pub fn value(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let result = match parse_macro_input!(item as syn::Item) {
        syn::Item::Struct(input) => schema_type_impl(input, false),
        syn::Item::Enum(input) => value_enum_impl(input),
        other => Err(syn::Error::new_spanned(
            other,
            "#[value] takes a struct (an embedded record) or an enum (a closed sum — \
             each variant carrying its own fields)",
        )),
    };
    match result {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// `#[value] enum` — a closed sum: each variant carries its own fields (a unit variant
/// serializes as its name, a data variant externally tagged), published as an
/// [`EnumDef`](idyll_schema::EnumDef). The vocabulary for a decision the server parsed
/// ONCE crossing the wire with exactly its own payload — a `match` on it is exhaustive
/// with no fallback arm, because the set is closed.
fn value_enum_impl(input: syn::ItemEnum) -> Result<TokenStream2> {
    let name = &input.ident;
    let name_str = name.to_string();
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&input.generics, "#[value] enums are not generic"));
    }

    // A schema sum type: each variant publishes its own field defs (empty = unit) and
    // registers its field types' reachability, exactly like a record's fields.
    let mut variant_defs = Vec::new();
    let mut register_calls = Vec::new();
    let mut all_unit = true;
    for variant in &input.variants {
        let variant_str = variant.ident.to_string();
        let fields = match &variant.fields {
            syn::Fields::Unit => Vec::new(),
            syn::Fields::Named(named) => {
                all_unit = false;
                named
                    .named
                    .iter()
                    .map(|field| {
                        let field_name =
                            field.ident.as_ref().expect("named field").to_string();
                        let ty = schema_field_type(&field.ty)?;
                        if let Some(target) = schema_register_target(&field.ty) {
                            register_calls.push(quote! {
                                <#target as ::idyll_data::SchemaType>::register(schema);
                            });
                        }
                        Ok(quote! {
                            ::idyll_data::FieldDef { name: #field_name.to_string(), ty: #ty }
                        })
                    })
                    .collect::<Result<Vec<_>>>()?
            }
            syn::Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    variant,
                    "#[value] enum variants use named fields (`Article { markdown: String }`) \
                     — the names are the published schema",
                ))
            }
        };
        variant_defs.push(quote! {
            ::idyll_data::VariantDef {
                name: #variant_str.to_string(),
                fields: vec![#(#fields),*],
            }
        });
    }

    // Unit-only enums stay `Copy`; data variants can't be.
    let derives = if all_unit {
        quote! {
            #[derive(
                ::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy,
                ::core::cmp::PartialEq, ::core::cmp::Eq,
                ::serde::Serialize, ::serde::Deserialize,
            )]
        }
    } else {
        quote! {
            #[derive(
                ::core::fmt::Debug, ::core::clone::Clone,
                ::core::cmp::PartialEq,
                ::serde::Serialize, ::serde::Deserialize,
            )]
        }
    };

    Ok(quote! {
        #derives
        #input

        impl ::idyll_data::DescribeEnum for #name {
            fn describe() -> ::idyll_data::EnumDef {
                ::idyll_data::EnumDef {
                    name: #name_str.to_string(),
                    variants: vec![#(#variant_defs),*],
                }
            }
        }

        impl ::idyll_data::SchemaType for #name {
            fn register(schema: &mut ::idyll_data::Schema) {
                if schema.enumeration_def(#name_str).is_none() {
                    schema.enums.push(<Self as ::idyll_data::DescribeEnum>::describe());
                    #(#register_calls)*
                }
            }
        }
    })
}

fn schema_type_impl(input: syn::ItemStruct, is_node: bool) -> Result<TokenStream2> {
    let name = &input.ident;
    let name_str = name.to_string();
    let kind = if is_node { "#[node]" } else { "#[value]" };

    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            format!("{kind} does not support generic schema types"),
        ));
    }

    let fields = match &input.fields {
        syn::Fields::Named(named) => &named.named,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.fields,
                format!("{kind} requires a struct with named fields"),
            ))
        }
    };

    let field_idents: Vec<&Ident> = fields.iter().map(|f| f.ident.as_ref().unwrap()).collect();
    let field_types: Vec<&syn::Type> = fields.iter().map(|f| &f.ty).collect();

    // Only a Node carries identity; a value type rides inline in its parent's JSON.
    // These types are **server-side data modeling** — clients see only schema-generated
    // projections, so there is no client trait machinery to emit at all.
    let node_impl = if is_node {
        let id_ty = fields
            .iter()
            .find(|f| f.ident.as_ref().is_some_and(|i| i == "id"))
            .map(|f| &f.ty)
            .ok_or_else(|| syn::Error::new_spanned(name, "#[node] requires an `id` field"))?;
        quote! {
            impl ::idyll_data::Node for #name {
                type Id = #id_ty;
                fn id(&self) -> Self::Id {
                    ::core::clone::Clone::clone(&self.id)
                }
            }
        }
    } else {
        quote! {}
    };

    // The record's schema descriptor — what `Schema::node::<T>()` / `::value::<T>()`
    // publish into `schema.json`.
    let describe_fields = field_idents
        .iter()
        .zip(field_types.iter())
        .map(|(field, ty)| {
            let field_str = field.to_string();
            let ty_expr = schema_field_type(ty)?;
            Ok(quote! {
                ::idyll_data::FieldDef { name: #field_str.to_string(), ty: #ty_expr }
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Reachability: register self (into the right section), then everything the
    // fields reach — the schema is the closure of the entry points.
    let section = if is_node { quote! { nodes } } else { quote! { values } };
    let field_registers = register_calls(field_types.iter().copied());

    Ok(quote! {
        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::cmp::PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        #input

        impl ::idyll_data::Record for #name {
            const TYPE_NAME: &'static str = #name_str;
        }

        impl ::idyll_data::DescribeRecord for #name {
            fn describe() -> ::idyll_data::RecordDef {
                ::idyll_data::RecordDef {
                    name: #name_str.to_string(),
                    fields: vec![ #(#describe_fields),* ],
                }
            }
        }

        impl ::idyll_data::SchemaType for #name {
            fn register(schema: &mut ::idyll_data::Schema) {
                if schema.record(#name_str).is_some() {
                    return;
                }
                schema.#section.push(<Self as ::idyll_data::DescribeRecord>::describe());
                #(#field_registers)*
            }
        }

        #node_impl
    })
}

/// Map a schema-record field's Rust type to an expression constructing its published
/// [`FieldType`]. The schema is the contract clients codegen from, so the mapping is
/// closed and loud: primitives and `String` are scalars (by exact Rust name — clients
/// map back 1:1), `Ref<N>` is a Node edge, `Vec<…>`/`Option<…>` wrap, and any other
/// bare path is an embedded `#[value]` record. Anything else is a compile error here,
/// not a surprise in a client build.
fn schema_field_type(ty: &syn::Type) -> Result<TokenStream2> {
    const SCALARS: &[&str] = &[
        "String", "bool", "char", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16",
        "i32", "i64", "i128", "isize", "f32", "f64",
    ];

    let syn::Type::Path(path) = ty else {
        return Err(syn::Error::new_spanned(
            ty,
            "schema fields must be plain types: a scalar, `Ref<Node>`, an embedded value \
             record, or `Vec`/`Option` of those",
        ));
    };
    let segment = path.path.segments.last().expect("a type path has a segment");
    let ident = segment.ident.to_string();

    if SCALARS.contains(&ident.as_str()) {
        return Ok(quote! { ::idyll_data::FieldType::scalar(#ident) });
    }

    // A single generic type argument, for Ref/Vec/Option.
    let inner = || -> Result<&syn::Type> {
        if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
            if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                return Ok(inner);
            }
        }
        Err(syn::Error::new_spanned(ty, format!("`{ident}` needs a type argument")))
    };

    match ident.as_str() {
        // Content: source text the data layer's registered mapping turns into View
        // IR during operation execution — the field type IS the transformation site.
        "Content" => Ok(quote! { ::idyll_data::FieldType::content() }),
        "Ref" => {
            let syn::Type::Path(target) = inner()? else {
                return Err(syn::Error::new_spanned(ty, "`Ref<…>` must name a Node type"));
            };
            let node = target.path.segments.last().unwrap().ident.to_string();
            Ok(quote! { ::idyll_data::FieldType::reference(#node) })
        }
        "Vec" => {
            let of = schema_field_type(inner()?)?;
            Ok(quote! { ::idyll_data::FieldType::list(#of) })
        }
        "Option" => {
            let of = schema_field_type(inner()?)?;
            Ok(quote! { ::idyll_data::FieldType::optional(#of) })
        }
        _ if segment.arguments.is_empty() => {
            // A bare path that isn't a scalar: an embedded `#[value]` record.
            Ok(quote! { ::idyll_data::FieldType::value(#ident) })
        }
        _ => Err(syn::Error::new_spanned(
            ty,
            format!("`{ident}` is not expressible in the published schema"),
        )),
    }
}

// ── #[root] — a named root query resolver, from a plain async fn ──────────────────────

/// `#[root]` marks a plain `async fn` as a schema **root entry point**. The function
/// passes through untouched; the macro emits, from the one signature:
///
/// - `<fn>_schema() -> RootDef` — the published descriptor (name, typed args, output
///   Node, list-ness) for the server's `Schema::root(...)`;
/// - `<fn>_resolver() -> RootResolver<Src>` — the typed execution glue for
///   `Resolvers::root(...)`: each named operation variable is decoded into the fn's
///   corresponding typed parameter **before** the body runs. App code never digs in
///   JSON, and descriptor and execution cannot drift — they come from the same fn.
///
/// ```ignore
/// #[root]
/// async fn post(db: &Db, id: u64) -> Result<Post, DbError> { db.post(id) }
/// // schema():    .root(post_schema())
/// // resolvers(): .root(post_resolver())
/// ```
#[proc_macro_attribute]
pub fn root(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    match root_impl(func) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

/// The parts of a resolver fn's signature both `#[root]` and `#[mutation]` read: the
/// owned source type (from the leading `&Src`), the typed args, and the `Result` Ok type.
struct ResolverFn {
    name: Ident,
    vis: syn::Visibility,
    src_ty: syn::Type,
    arg_idents: Vec<Ident>,
    arg_types: Vec<syn::Type>,
    out_ok: syn::Type,
}

/// An attribute's HTML name. HTML attributes are hyphenated (`aria-label`,
/// `data-state`) and Rust identifiers cannot be, so an underscore writes a hyphen —
/// the same mapping `style:` properties already use.
fn attr_html_name(name: &Ident) -> String {
    name.to_string().replace('_', "-")
}

fn parse_resolver_fn(func: &ItemFn, attr_name: &str) -> Result<ResolverFn> {
    if func.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            func.sig.fn_token,
            format!("{attr_name} expects an `async fn`"),
        ));
    }
    if !func.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &func.sig.generics,
            format!("{attr_name} resolvers take a concrete source type"),
        ));
    }

    // First parameter is the source `&Src`; the rest are the operation's arguments.
    let mut inputs = func.sig.inputs.iter();
    let src_ty = match inputs.next() {
        Some(syn::FnArg::Typed(pat)) => match &*pat.ty {
            syn::Type::Reference(reference) => (*reference.elem).clone(),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    format!("{attr_name}'s first parameter must be the source, taken by reference: `&Src`"),
                ))
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &func.sig,
                format!("{attr_name} requires a source parameter first: `async fn f(src: &Src, …)`"),
            ))
        }
    };

    let mut arg_idents: Vec<Ident> = Vec::new();
    let mut arg_types: Vec<syn::Type> = Vec::new();
    for input in inputs {
        let syn::FnArg::Typed(pat) = input else {
            return Err(syn::Error::new_spanned(input, format!("{attr_name} does not take `self`")));
        };
        let syn::Pat::Ident(pat_ident) = &*pat.pat else {
            return Err(syn::Error::new_spanned(
                &pat.pat,
                format!("{attr_name} arguments must be plain identifiers"),
            ));
        };
        arg_idents.push(pat_ident.ident.clone());
        arg_types.push((*pat.ty).clone());
    }

    let output_ty = match &func.sig.output {
        ReturnType::Type(_, ty) => ty.as_ref().clone(),
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &func.sig,
                format!("{attr_name} must return a `Result<T, E>`"),
            ))
        }
    };
    let out_ok = result_ok_type(&output_ty).ok_or_else(|| {
        syn::Error::new_spanned(&output_ty, format!("{attr_name} must return a `Result<T, E>`"))
    })?;

    Ok(ResolverFn {
        name: func.sig.ident.clone(),
        vis: func.vis.clone(),
        src_ty,
        arg_idents,
        arg_types,
        out_ok,
    })
}

/// The named schema type a field/arg/output type reaches, if any — what
/// `SchemaType::register` recursion targets. Scalars are terminal;
/// `Ref`/`Vec`/`Option` delegate to their inner type; any other bare path IS a
/// schema type (node, value, or enum — its own `register` knows which).
fn schema_register_target(ty: &syn::Type) -> Option<&syn::Type> {
    const SCALARS: &[&str] = &[
        "String", "bool", "char", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16",
        "i32", "i64", "i128", "isize", "f32", "f64",
    ];
    let syn::Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    let ident = segment.ident.to_string();
    if SCALARS.contains(&ident.as_str()) {
        return None;
    }
    match ident.as_str() {
        "Ref" | "Vec" | "Option" => {
            if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    return schema_register_target(inner);
                }
            }
            None
        }
        _ => Some(ty),
    }
}

/// Registration calls for every named schema type the given types reach.
fn register_calls<'a>(types: impl Iterator<Item = &'a syn::Type>) -> Vec<TokenStream2> {
    types
        .filter_map(schema_register_target)
        .map(|target| quote! { <#target as ::idyll_data::SchemaType>::register(schema); })
        .collect()
}

/// The schema-descriptor arg list shared by `#[root]` and `#[mutation]`.
fn describe_args(sig: &ResolverFn) -> Result<Vec<TokenStream2>> {
    sig.arg_idents
        .iter()
        .zip(sig.arg_types.iter())
        .map(|(ident, ty)| {
            let arg_str = ident.to_string();
            let ty_expr = schema_field_type(ty)?;
            Ok(quote! {
                ::idyll_data::FieldDef { name: #arg_str.to_string(), ty: #ty_expr }
            })
        })
        .collect()
}

/// The typed execution glue shared by `#[root]` and `#[mutation]`: decode each named
/// variable into its typed parameter, call the fn, box the error.
fn resolver_body(sig: &ResolverFn) -> TokenStream2 {
    let ResolverFn { name, src_ty, arg_idents, arg_types, .. } = sig;
    let arg_strs = arg_idents.iter().map(|ident| ident.to_string()).collect::<Vec<_>>();
    quote! {
        |__src: #src_ty, __vars: ::idyll_data::serde_json::Value| async move {
            #(
                let #arg_idents: #arg_types = ::idyll_data::arg(&__vars, #arg_strs)?;
            )*
            #name(&__src, #(#arg_idents),*)
                .await
                .map_err(|__e| ::std::boxed::Box::new(__e) as ::idyll_data::BoxError)
        }
    }
}

fn root_impl(mut func: ItemFn) -> Result<TokenStream2> {
    let mut sig = parse_resolver_fn(&func, "#[root]")?;

    // The entry's name is the ONE identifier: the fn itself becomes hidden execution
    // glue, and its name becomes the handle constructor a `Query` field holds —
    // `Query { route: route() }`.
    let entry_name = sig.name.clone();
    let hidden = format_ident!("__idyll_root_{}", entry_name);
    func.sig.ident = hidden.clone();
    sig.name = hidden;

    let ResolverFn { vis, src_ty, out_ok, .. } = &sig;
    let name_str = entry_name.to_string();
    let (output_node, is_list) = root_output(out_ok)?;
    let args = describe_args(&sig)?;
    let body = resolver_body(&sig);
    let registers =
        register_calls(sig.arg_types.iter().chain(std::iter::once(&sig.out_ok)));

    Ok(quote! {
        #func

        #vis fn #entry_name() -> ::idyll_data::RootHandle<#src_ty> {
            ::idyll_data::RootHandle {
                entry: ::idyll_data::RootEntry {
                    def: ::idyll_data::RootDef {
                        name: #name_str.to_string(),
                        args: vec![ #(#args),* ],
                        output: #output_node.to_string(),
                        list: #is_list,
                    },
                    register: |schema| { #(#registers)* },
                },
                resolver: ::idyll_data::RootResolver::new(#name_str, #body),
            }
        }
    })
}

/// `#[mutation("add-todo")]` — the mutation twin of `#[root]`: the `async fn` passes
/// through untouched, and the one signature emits both the published descriptor
/// (`<fn>_schema() -> MutationDef`) and the typed execution glue
/// (`<fn>_resolver() -> MutationResolver<Src>`). The fn returns the full output
/// **Node**; the executor masks it to whatever each persisted artifact selected.
///
/// ```ignore
/// #[mutation("add-todo")]
/// async fn add_todo(db: &Db, text: String) -> Result<Todo, Infallible> { Ok(db.add(text)) }
/// // schema():    .mutation(add_todo_schema())
/// // resolvers(): .mutation(add_todo_resolver())
/// ```
#[proc_macro_attribute]
pub fn mutation_handler(attr: TokenStream, item: TokenStream) -> TokenStream {
    let wire = parse_macro_input!(attr as syn::LitStr);
    let func = parse_macro_input!(item as ItemFn);
    match mutation_handler_impl(wire, func) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn mutation_handler_impl(wire: syn::LitStr, mut func: ItemFn) -> Result<TokenStream2> {
    let mut sig = parse_resolver_fn(&func, "#[mutation]")?;

    // As with `#[root]`: the fn becomes hidden execution glue and its name becomes
    // the handle constructor a `Mutation` field holds. The wire name stays in the def.
    let entry_name = sig.name.clone();
    let hidden = format_ident!("__idyll_mutation_{}", entry_name);
    func.sig.ident = hidden.clone();
    sig.name = hidden;

    let ResolverFn { vis, src_ty, out_ok, .. } = &sig;
    let (output_node, is_list) = root_output(out_ok)?;
    if is_list {
        return Err(syn::Error::new_spanned(
            out_ok,
            "a mutation yields one Node, not a list",
        ));
    }

    let args = describe_args(&sig)?;
    let body = resolver_body(&sig);
    let registers =
        register_calls(sig.arg_types.iter().chain(std::iter::once(&sig.out_ok)));

    Ok(quote! {
        #func

        #vis fn #entry_name() -> ::idyll_data::MutationHandle<#src_ty> {
            ::idyll_data::MutationHandle {
                entry: ::idyll_data::MutationEntry {
                    def: ::idyll_data::MutationDef {
                        name: #wire.to_string(),
                        args: vec![ #(#args),* ],
                        output: #output_node.to_string(),
                    },
                    register: |schema| { #(#registers)* },
                },
                resolver: ::idyll_data::MutationResolver::new(#wire, #body),
            }
        }
    })
}

/// `#[derive(Queries)]` — the app's `Query` group: a struct whose fields hold the
/// `RootHandle`s its `#[root]` fns constructed. More queries are more fields; the
/// derive just enumerates them (names are bindings — every entry's identity travels
/// inside its handle).
#[proc_macro_derive(Queries)]
pub fn derive_queries(input: TokenStream) -> TokenStream {
    entry_group_impl(parse_macro_input!(input as syn::DeriveInput), "Queries", "RootHandle")
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

/// `#[derive(Mutations)]` — the mutation twin of [`Queries`](derive_queries). An app
/// with no mutations derives it on a unit struct.
#[proc_macro_derive(Mutations)]
pub fn derive_mutations(input: TokenStream) -> TokenStream {
    entry_group_impl(parse_macro_input!(input as syn::DeriveInput), "Mutations", "MutationHandle")
        .unwrap_or_else(|err| err.to_compile_error())
        .into()
}

fn entry_group_impl(
    input: syn::DeriveInput,
    trait_name: &str,
    handle_name: &str,
) -> Result<TokenStream2> {
    let name = &input.ident;
    let trait_ident = format_ident!("{trait_name}");
    let handle_ident = format_ident!("{handle_name}");
    let syn::Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            format!("#[derive({trait_name})] takes a struct of {handle_name} fields"),
        ));
    };

    let fields: Vec<&syn::Field> = data.fields.iter().collect();
    // An empty group (no mutations yet) serves any data source.
    if fields.is_empty() {
        return Ok(quote! {
            impl<Src: Clone + Send + Sync + 'static> ::idyll_data::#trait_ident<Src> for #name {
                fn entries(self) -> Vec<::idyll_data::#handle_ident<Src>> {
                    Vec::new()
                }
            }
        });
    }

    // The data source is the handles': every field is `Handle<Src>`, so the first
    // field's argument names it (the vec's type holds the rest to it).
    let src = (|| -> Option<&syn::Type> {
        let syn::Type::Path(path) = &fields[0].ty else { return None };
        let segment = path.path.segments.last()?;
        let syn::PathArguments::AngleBracketed(args) = &segment.arguments else { return None };
        args.args.iter().find_map(|arg| match arg {
            syn::GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
    })()
    .ok_or_else(|| {
        syn::Error::new_spanned(
            &fields[0].ty,
            format!("#[derive({trait_name})] fields are `{handle_name}<Src>` values"),
        )
    })?;
    let accessors = fields.iter().enumerate().map(|(i, field)| match &field.ident {
        Some(ident) => quote! { self.#ident },
        None => {
            let index = syn::Index::from(i);
            quote! { self.#index }
        }
    });

    Ok(quote! {
        impl ::idyll_data::#trait_ident<#src> for #name {
            fn entries(self) -> Vec<::idyll_data::#handle_ident<#src>> {
                vec![ #(#accessors),* ]
            }
        }
    })
}

/// A root's published output: the Node name and whether it is a list root. A single
/// root may yield `Option<T>` — **absence is a value** (the executor's typed 404), not
/// part of the published shape.
fn root_output(out_ok: &syn::Type) -> Result<(String, bool)> {
    let node_name = |ty: &syn::Type| -> Result<String> {
        let syn::Type::Path(path) = ty else {
            return Err(syn::Error::new_spanned(ty, "#[root] must yield a Node type"));
        };
        Ok(path.path.segments.last().unwrap().ident.to_string())
    };
    if let syn::Type::Path(path) = out_ok {
        let segment = path.path.segments.last().unwrap();
        if segment.ident == "Vec" || segment.ident == "Option" {
            if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    return Ok((node_name(inner)?, segment.ident == "Vec"));
                }
            }
        }
    }
    Ok((node_name(out_ok)?, false))
}

/// Extract `T` from a `Result<T, E>` return type (the `Ok` variant), if the type is a path
/// ending in `Result` with two type arguments.
fn result_ok_type(ty: &syn::Type) -> Option<syn::Type> {
    let syn::Type::Path(path) = ty else {
        return None;
    };
    let segment = path.path.segments.last()?;
    if segment.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(ty) => Some(ty.clone()),
        _ => None,
    })
}

// ── fragment! — a type-free selection on a record ────────────────────────────────

/// A `fragment!` selection entry. Leaf-vs-edge lives in the *syntax* because it can't be
/// in the (absent) types: bare `title` = leaf; `field: Child` = spread; `field: [Child]`
/// = edge list.
enum FragSel {
    /// `route { Todos { … }, Prose {} }` — a sum-typed field, matched exhaustively.
    Enum { field: Ident, variants: Vec<(Ident, Vec<FragSel>)> },
    Leaf(Ident),
    Spread { edge: Ident, child: Ident },
    List { edge: Ident, child: Ident },
}

struct FragmentInput {
    name: Ident,
    on: Ident,
    selections: Vec<FragSel>,
}

impl Parse for FragmentInput {
    fn parse(input: ParseStream) -> Result<Self> {
        let name: Ident = input.parse()?;
        let on_kw: Ident = input.parse()?;
        if on_kw != "on" {
            return Err(syn::Error::new_spanned(
                &on_kw,
                "expected `on` after the fragment name: `fragment! { Name on Type { … } }`",
            ));
        }
        let on: Ident = input.parse()?;

        let content;
        braced!(content in input);
        let selections = parse_frag_selections(&content)?;

        Ok(FragmentInput {
            name,
            on,
            selections,
        })
    }
}

/// One selection set: leaves, edges, and sum-typed fields matched per variant
/// (`route { Todos { … }, Prose {} }`). Shared by the fragment body and each
/// variant's own selection.
fn parse_frag_selections(content: ParseStream) -> Result<Vec<FragSel>> {
    let mut selections = Vec::new();
    while !content.is_empty() {
        let field: Ident = content.parse()?;
        if content.peek(Token![:]) {
            content.parse::<Token![:]>()?;
            if content.peek(token::Bracket) {
                let inner;
                bracketed!(inner in content);
                let child: Ident = inner.parse()?;
                selections.push(FragSel::List { edge: field, child });
            } else {
                let child: Ident = content.parse()?;
                selections.push(FragSel::Spread { edge: field, child });
            }
        } else if content.peek(token::Brace) {
            // A sum-typed field, matched per variant.
            let arms;
            braced!(arms in content);
            let mut variants = Vec::new();
            while !arms.is_empty() {
                let variant: Ident = arms.parse()?;
                let body;
                braced!(body in arms);
                variants.push((variant, parse_frag_selections(&body)?));
                if arms.peek(Token![,]) {
                    arms.parse::<Token![,]>()?;
                } else {
                    break;
                }
            }
            selections.push(FragSel::Enum { field, variants });
        } else {
            selections.push(FragSel::Leaf(field));
        }
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        } else {
            break;
        }
    }
    Ok(selections)
}

// ── The published schema, read at expansion time ─────────────────────────────────

/// Load `schema.json` — the server's published contract. The dev server sets
/// `IDYLL_SCHEMA` for app builds (so macros validate against the schema the running
/// server defines); otherwise the checked-in artifact is found by upward search from
/// the crate being compiled (bare `cargo build`, rust-analyzer, CI).
///
/// Returns the parsed schema and the artifact's path (forward slashes) — expansions
/// embed an `include_str!` of it so rustc rebuilds when the contract changes.
fn load_schema(span: proc_macro2::Span) -> Result<(idyll_schema::Schema, String)> {
    let path = match std::env::var_os("IDYLL_SCHEMA") {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let mut dir = std::path::PathBuf::from(
                std::env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| {
                    syn::Error::new(span, "CARGO_MANIFEST_DIR is unset — cannot locate schema.json")
                })?,
            );
            loop {
                let candidate = dir.join("schema.json");
                if candidate.is_file() {
                    break candidate;
                }
                if !dir.pop() {
                    return Err(syn::Error::new(
                        span,
                        "no schema.json found upward from this crate (and IDYLL_SCHEMA is \
                         unset) — run the dev server once to publish it, or check the \
                         artifact in",
                    ));
                }
            }
        }
    };
    let text = std::fs::read_to_string(&path)
        .map_err(|err| syn::Error::new(span, format!("reading {}: {err}", path.display())))?;
    let schema = idyll_schema::Schema::from_json(&text).map_err(|err| {
        syn::Error::new(
            span,
            format!("{} does not parse as a published schema: {err}", path.display()),
        )
    })?;
    Ok((schema, path.display().to_string().replace('\\', "/")))
}

/// The Rust type a schema **leaf** field projects to — what a generated accessor
/// returns. Scalar shapes and value **enums** (closed keyword sets, typed as the
/// enum itself — the app has the `#[value] enum` in scope, exactly as it has the
/// scalar types) are leaves; a `Ref`/value *record* (or list of them) must be
/// selected as an edge with a child fragment, and saying so here is the macro's error.
fn leaf_rust_type(
    ty: &idyll_schema::FieldType,
    span: proc_macro2::Span,
    schema: &idyll_schema::Schema,
) -> Result<TokenStream2> {
    use idyll_schema::FieldType;
    match ty {
        FieldType::Scalar { name } => {
            let ident = format_ident!("{name}");
            Ok(quote! { #ident })
        }
        // Content arrives as mapped View IR — the read yields the view itself.
        FieldType::Content => Ok(quote! { ::idyll::View }),
        FieldType::Optional { of } => {
            let inner = leaf_rust_type(of, span, schema)?;
            Ok(quote! { ::core::option::Option<#inner> })
        }
        FieldType::List { of } => {
            let inner = leaf_rust_type(of, span, schema)?;
            Ok(quote! { ::std::vec::Vec<#inner> })
        }
        FieldType::Ref { node } => Err(syn::Error::new(
            span,
            format!("this field is a `{node}` reference — select it as an edge: `field: ChildFragment`"),
        )),
        FieldType::Value { value } if schema.enumeration_def(value).is_some() => {
            let ident = format_ident!("{value}");
            Ok(quote! { #ident })
        }
        FieldType::Value { value } => Err(syn::Error::new(
            span,
            format!("this field is an embedded `{value}` — select it as an edge: `field: ChildFragment`"),
        )),
    }
}


/// One selection set, materialized against its **field scope** (a record's fields or
/// one enum variant's): the `Sel` entries for the DEF, the projected field decls and
/// their `from_record` inits (reading a `record` binding in scope), compile-time edge
/// checks, and any nested items (the generated enum for a sum-typed field). Shared by
/// the fragment body and every variant — one rule set for both.
struct MaterializedSelection {
    sel_entries: Vec<TokenStream2>,
    fields: Vec<TokenStream2>,
    inits: Vec<TokenStream2>,
    edge_checks: Vec<TokenStream2>,
    nested_items: Vec<TokenStream2>,
}

fn materialize_selection(
    schema: &idyll_schema::Schema,
    schema_path: &str,
    scope: &str,
    scope_fields: &[idyll_schema::FieldDef],
    selections: &[FragSel],
    type_prefix: &str,
    vis: TokenStream2,
) -> Result<MaterializedSelection> {
    let mut out = MaterializedSelection {
        sel_entries: Vec::new(),
        fields: Vec::new(),
        inits: Vec::new(),
        edge_checks: Vec::new(),
        nested_items: Vec::new(),
    };
    let field_def = |field: &Ident| -> Result<&idyll_schema::FieldDef> {
        scope_fields.iter().find(|f| f.name == field.to_string()).ok_or_else(|| {
            syn::Error::new(
                field.span(),
                format!("`{scope}` has no field `{field}` in the published schema ({schema_path})"),
            )
        })
    };

    for sel in selections {
        match sel {
            FragSel::Leaf(field) => {
                let def = field_def(field)?;
                let field_str = field.to_string();
                let rust_ty = leaf_rust_type(&def.ty, field.span(), schema)?;
                out.sel_entries.push(quote! {
                    ::idyll_data::Sel::Leaf { field: #field_str }
                });
                out.fields.push(quote! { #vis #field: #rust_ty });
                out.inits.push(quote! {
                    #field: ::idyll_data::field_value(record, #scope, #field_str)
                });
            }
            FragSel::Spread { edge, child } => {
                let def = field_def(edge)?;
                let edge_str = edge.to_string();
                out.sel_entries.push(quote! {
                    ::idyll_data::Sel::Spread {
                        edge: #edge_str,
                        frag: <#child as ::idyll_data::Fragment>::DEF,
                    }
                });
                let (target, field_ty, init) = match &def.ty {
                    // A node edge is a typed reference: the record's id, parsed to the
                    // child fragment's `Id` — never followed here.
                    idyll_schema::FieldType::Ref { node } => (
                        node.clone(),
                        quote! { ::idyll_data::Frag<#child> },
                        quote! {
                            ::idyll_data::Frag::from_id(
                                ::idyll_data::field_value(record, #scope, #edge_str),
                            )
                        },
                    ),
                    // A value edge has no identity: the child fragment embeds directly,
                    // built from the field's JSON inside this record.
                    idyll_schema::FieldType::Value { value } => (
                        value.clone(),
                        quote! { #child },
                        quote! {
                            <#child as ::idyll_data::Fragment>::from_record(
                                &::idyll_data::field_json(record, #scope, #edge_str),
                            )
                        },
                    ),
                    other => {
                        return Err(syn::Error::new(
                            edge.span(),
                            format!("`{scope}.{edge}` is {other:?} in the schema — not a spreadable edge"),
                        ))
                    }
                };
                out.edge_checks.push(edge_target_check(child, &target, scope, &edge_str));
                out.fields.push(quote! { #vis #edge: #field_ty });
                out.inits.push(quote! { #edge: #init });
            }
            FragSel::List { edge, child } => {
                let def = field_def(edge)?;
                let edge_str = edge.to_string();
                let idyll_schema::FieldType::List { of } = &def.ty else {
                    return Err(syn::Error::new(
                        edge.span(),
                        format!("`{scope}.{edge}` is {:?} in the schema — not a list edge", def.ty),
                    ));
                };
                out.sel_entries.push(quote! {
                    ::idyll_data::Sel::List {
                        edge: #edge_str,
                        frag: <#child as ::idyll_data::Fragment>::DEF,
                    }
                });
                let (field_ty, init) = match &**of {
                    idyll_schema::FieldType::Ref { node } => {
                        out.edge_checks.push(edge_target_check(child, node, scope, &edge_str));
                        (
                            quote! { ::std::vec::Vec<::idyll_data::Frag<#child>> },
                            quote! {
                                #edge: {
                                    let ids: ::std::vec::Vec<
                                        <#child as ::idyll_data::NodeFragment>::Id,
                                    > = ::idyll_data::field_value(record, #scope, #edge_str);
                                    ids.into_iter().map(::idyll_data::Frag::from_id).collect()
                                }
                            },
                        )
                    }
                    idyll_schema::FieldType::Value { value } => {
                        out.edge_checks.push(edge_target_check(child, value, scope, &edge_str));
                        (
                            quote! { ::std::vec::Vec<#child> },
                            quote! {
                                #edge: match ::idyll_data::field_json(record, #scope, #edge_str) {
                                    ::idyll_data::serde_json::Value::Array(items) => items
                                        .iter()
                                        .map(<#child as ::idyll_data::Fragment>::from_record)
                                        .collect(),
                                    other => panic!(
                                        "schema violation: `{}.{}` is not an array: {other}",
                                        #scope, #edge_str
                                    ),
                                }
                            },
                        )
                    }
                    other => {
                        return Err(syn::Error::new(
                            edge.span(),
                            format!("`{scope}.{edge}` is a list of {other:?} — select scalar lists as a leaf"),
                        ))
                    }
                };
                out.fields.push(quote! { #vis #edge: #field_ty });
                out.inits.push(init);
            }
            FragSel::Enum { field, variants } => {
                let def = field_def(field)?;
                let field_str = field.to_string();
                let idyll_schema::FieldType::Value { value: enum_name } = &def.ty else {
                    return Err(syn::Error::new(
                        field.span(),
                        format!("`{scope}.{field}` is {:?} in the schema — not a sum-typed field", def.ty),
                    ));
                };
                let enum_def = schema.enumeration_def(enum_name).ok_or_else(|| {
                    syn::Error::new(
                        field.span(),
                        format!("`{enum_name}` is not a schema enum ({schema_path})"),
                    )
                })?;

                // The closed set is matched **whole**, at compile time, both ways.
                for (variant, _) in variants {
                    if enum_def.variant(&variant.to_string()).is_none() {
                        return Err(syn::Error::new(
                            variant.span(),
                            format!("`{enum_name}` has no variant `{variant}` in the published schema"),
                        ));
                    }
                }
                for schema_variant in &enum_def.variants {
                    if !variants.iter().any(|(v, _)| v.to_string() == schema_variant.name) {
                        return Err(syn::Error::new(
                            field.span(),
                            format!(
                                "`{scope}.{field}` does not select `{enum_name}::{}` — a sum \
                                 type is matched exhaustively",
                                schema_variant.name
                            ),
                        ));
                    }
                }

                let enum_ident = format_ident!("{type_prefix}{}", pascal(&field.to_string()));
                let mut variant_decls = Vec::new();
                let mut variant_sels = Vec::new();
                let mut unit_arms = Vec::new();
                let mut object_arms = Vec::new();
                for (variant, selection) in variants {
                    let variant_str = variant.to_string();
                    let schema_variant =
                        enum_def.variant(&variant_str).expect("checked above");
                    let inner = materialize_selection(
                        schema,
                        schema_path,
                        &format!("{enum_name}::{variant_str}"),
                        &schema_variant.fields,
                        selection,
                        &format!("{enum_ident}{variant_str}"),
                        quote! {},
                    )?;
                    let MaterializedSelection {
                        sel_entries,
                        fields,
                        inits,
                        edge_checks,
                        nested_items,
                    } = inner;
                    out.edge_checks.extend(edge_checks);
                    out.nested_items.extend(nested_items);
                    variant_decls.push(quote! { #variant { #(#fields),* } });
                    variant_sels.push(quote! {
                        ::idyll_data::VariantSel {
                            name: #variant_str,
                            selection: &[ #(#sel_entries),* ],
                        }
                    });
                    if schema_variant.fields.is_empty() {
                        unit_arms.push(quote! {
                            #variant_str => #enum_ident::#variant {},
                        });
                    }
                    object_arms.push(quote! {
                        #variant_str => {
                            let record = __payload;
                            #enum_ident::#variant { #(#inits),* }
                        }
                    });
                }

                out.nested_items.push(quote! {
                    /// The projected sum type for a variant-matched field — one arm per
                    /// schema variant, carrying exactly that variant's selection.
                    #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::cmp::PartialEq)]
                    pub enum #enum_ident {
                        #(#variant_decls),*
                    }
                });
                out.sel_entries.push(quote! {
                    ::idyll_data::Sel::Enum {
                        field: #field_str,
                        variants: &[ #(#variant_sels),* ],
                    }
                });
                out.fields.push(quote! { #vis #field: #enum_ident });
                out.inits.push(quote! {
                    #field: {
                        let __value = ::idyll_data::field_json(record, #scope, #field_str);
                        match &__value {
                            ::idyll_data::serde_json::Value::String(__tag) => {
                                match __tag.as_str() {
                                    #(#unit_arms)*
                                    __other => panic!(
                                        "schema violation: `{}` has no unit variant `{__other}`",
                                        #enum_name
                                    ),
                                }
                            }
                            ::idyll_data::serde_json::Value::Object(__map) if __map.len() == 1 => {
                                let (__tag, __payload) =
                                    __map.iter().next().expect("length checked");
                                match __tag.as_str() {
                                    #(#object_arms)*
                                    __other => panic!(
                                        "schema violation: `{}` has no variant `{__other}`",
                                        #enum_name
                                    ),
                                }
                            }
                            __other => panic!(
                                "schema violation: `{}.{}` is not externally tagged: {__other}",
                                #scope, #field_str
                            ),
                        }
                    }
                });
            }
        }
    }
    Ok(out)
}

/// `snake_case` -> `PascalCase` for generated type names.
fn pascal(name: &str) -> String {
    name.split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect()
}

/// `fragment! { PostCard on Post { title, author: Avatar, comments: [CommentRow] } }`
///
/// **Schema-first**: `Post` is a schema record *name*, resolved against the published
/// `schema.json` — not a Rust type, and no server type is involved. The macro validates
/// every selection against the schema (unknown record/field, or selecting an edge as a
/// leaf, is a clean compile error naming the artifact) and generates the **value
/// struct** `PostCard` — exactly the selected fields, scalars typed from the schema,
/// edges as `Frag` keys — plus its normalized `DEF` and the `read`/`resolve`
/// constructors that resolve a key to a `Live<PostCard>`. Masking is physical: an
/// unselected field has nowhere to live.
#[proc_macro]
pub fn fragment(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as FragmentInput);
    match fragment_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn fragment_impl(input: FragmentInput) -> Result<TokenStream2> {
    let FragmentInput { name, on, selections } = input;
    let (schema, schema_path) = load_schema(on.span())?;

    let on_str = on.to_string();
    let record = schema.record(&on_str).ok_or_else(|| {
        syn::Error::new(
            on.span(),
            format!("`{on_str}` is not a record in the published schema ({schema_path})"),
        )
    })?;
    let def_ident = format_ident!("__IDYLL_FRAG_DEF_{}", name);
    let name_str = name.to_string();

    // A fragment on a **node** gets identity: the `NodeFragment` impl (typed `Id`
    // from the schema's `id` field), the `key` constructor, and `read`/`resolve`.
    // A fragment on an embedded value gets none — a value has no identity, and its
    // data arrives inside whatever parent selected it.
    let node_impls = if schema.nodes.iter().any(|node| node.name == on_str) {
        let id_ty = record
            .fields
            .iter()
            .find(|field| field.name == "id")
            .ok_or_else(|| {
                syn::Error::new(
                    on.span(),
                    format!("node `{on_str}` has no `id` field in the published schema ({schema_path})"),
                )
            })
            .and_then(|field| leaf_rust_type(&field.ty, on.span(), &schema))?;
        Some(quote! {
            impl ::idyll_data::NodeFragment for #name {
                type Id = #id_ty;
            }

            impl #name {
                /// A typed reference to the record with this id — the live-key
                /// currency for `@live(Def, key = …)` markers.
                pub fn key(id: #id_ty) -> ::idyll_data::Frag<#name> {
                    ::idyll_data::Frag::from_id(id)
                }

                /// Resolve the reference against the seeded store, suspending until
                /// the record is present. Never starts a load.
                pub async fn read(
                    cache: &::idyll_data::Cache,
                    key: ::idyll_data::Frag<#name>,
                ) -> ::idyll_data::Live<#name> {
                    ::idyll_data::read_fragment::<#name>(cache, key).await
                }

                /// Resolve synchronously against a store that must already hold the
                /// record — the form for tracked scopes, which cannot await.
                pub fn resolve(
                    cache: &::idyll_data::Cache,
                    key: ::idyll_data::Frag<#name>,
                ) -> ::idyll_data::Live<#name> {
                    ::idyll_data::resolve_fragment::<#name>(cache, key)
                }
            }
        })
    } else {
        None
    };

    let materialized = materialize_selection(
        &schema,
        &schema_path,
        &on_str,
        &record.fields,
        &selections,
        &name_str,
        quote! { pub },
    )?;
    let MaterializedSelection { sel_entries, fields: struct_fields, inits: field_inits, edge_checks, nested_items } =
        materialized;

    Ok(quote! {
        /// The selected fields, as plain data: scalars typed from the published schema,
        /// node edges as typed [`Frag`](::idyll_data::Frag) references, value edges as
        /// the embedded child fragment.
        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::cmp::PartialEq)]
        pub struct #name {
            #(#struct_fields),*
        }

        // Rebuild when the published contract changes.
        const _: &str = ::core::include_str!(#schema_path);

        #(#edge_checks)*

        #(#nested_items)*

        #[allow(non_upper_case_globals)]
        static #def_ident: ::idyll_data::FragmentDef = ::idyll_data::FragmentDef {
            name: #name_str,
            on: #on_str,
            selection: &[ #(#sel_entries),* ],
        };

        impl ::idyll_data::Fragment for #name {
            const ON: &'static str = #on_str;
            const DEF: &'static ::idyll_data::FragmentDef = &#def_ident;

            fn from_record(record: &::idyll_data::serde_json::Value) -> Self {
                Self { #(#field_inits),* }
            }
        }

        #node_impls
    })
}

/// A compile-time assertion that a spread's child fragment is on the schema type this
/// edge yields — `fragment! { … author: Avatar }` fails to compile if `Avatar` is not
/// `on` the record the schema declares for `author`.
fn edge_target_check(child: &Ident, target: &str, on: &str, edge: &str) -> TokenStream2 {
    let message = format!(
        "edge `{on}.{edge}` yields `{target}` in the published schema, but this child \
         fragment is on a different record"
    );
    quote! {
        const _: () = {
            let expected = #target.as_bytes();
            let actual = <#child as ::idyll_data::Fragment>::ON.as_bytes();
            if expected.len() != actual.len() {
                panic!(#message);
            }
            let mut i = 0;
            while i < expected.len() {
                if expected[i] != actual[i] {
                    panic!(#message);
                }
                i += 1;
            }
        };
    }
}

// ── query! — a persisted operation: root fields + variables ──────────────────────

struct RootField {
    field: Ident,
    args: Vec<(Ident, Ident)>, // (param, variable)
    child: Ident,
    list: bool, // `field: [Child]` → a list root (Vec<Node>)
}

struct QueryInput {
    name: Ident,
    vars: Vec<(Ident, syn::Type)>,
    roots: Vec<RootField>,
}

impl Parse for QueryInput {
    fn parse(input: ParseStream) -> Result<Self> {
        let name: Ident = input.parse()?;

        let var_content;
        parenthesized!(var_content in input);
        let mut vars = Vec::new();
        while !var_content.is_empty() {
            var_content.parse::<Token![$]>()?;
            let var: Ident = var_content.parse()?;
            var_content.parse::<Token![:]>()?;
            let ty: syn::Type = var_content.parse()?;
            vars.push((var, ty));
            if var_content.peek(Token![,]) {
                var_content.parse::<Token![,]>()?;
            } else {
                break;
            }
        }

        let body;
        braced!(body in input);
        let mut roots = Vec::new();
        while !body.is_empty() {
            let field: Ident = body.parse()?;
            // Args are optional: `posts: [PostCard]` has none, `post(id: $id): PostCard` does.
            let mut args = Vec::new();
            if body.peek(token::Paren) {
                let arg_content;
                parenthesized!(arg_content in body);
                while !arg_content.is_empty() {
                    let param: Ident = arg_content.parse()?;
                    arg_content.parse::<Token![:]>()?;
                    arg_content.parse::<Token![$]>()?;
                    let var: Ident = arg_content.parse()?;
                    args.push((param, var));
                    if arg_content.peek(Token![,]) {
                        arg_content.parse::<Token![,]>()?;
                    } else {
                        break;
                    }
                }
            }
            body.parse::<Token![:]>()?;
            // `: [Child]` is a list root; `: Child` is a single root.
            let (child, list) = if body.peek(token::Bracket) {
                let inner;
                bracketed!(inner in body);
                (inner.parse::<Ident>()?, true)
            } else {
                (body.parse::<Ident>()?, false)
            };
            roots.push(RootField {
                field,
                args,
                child,
                list,
            });
            if body.peek(Token![,]) {
                body.parse::<Token![,]>()?;
            } else {
                break;
            }
        }

        Ok(QueryInput { name, vars, roots })
    }
}

/// `query! { PostRoute($id: u64) { post(id: $id): PostCard } }`
///
/// **Schema-first**: every root field is validated against the published `schema.json`
/// (existence, list-ness, argument names/types, output record vs the child fragment's
/// `ON`). Generates the operation handle — its normalized `DEF`, the content-addressed
/// `query_file()` / `hash()` identity — plus the `…Vars` struct and the `…Roots`
/// projection (raw wire ids; accessors hand out `Frag` keys, never record data).
///
/// There is **no generated execution code**: the server interprets the persisted
/// artifact against its resolver table (`idyll_data::execute`). The registry artifacts
/// are written by the dev server at boot from the route table's `query_file()`s.
#[proc_macro]
pub fn query(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as QueryInput);
    match query_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn query_impl(input: QueryInput) -> Result<TokenStream2> {
    let QueryInput { name, vars, roots } = input;
    let (schema, schema_path) = load_schema(name.span())?;

    let name_str = name.to_string();
    let def_ident = format_ident!("__IDYLL_QUERY_DEF_{}", name);
    let vars_name = format_ident!("{}Vars", name);
    let roots_name = format_ident!("{}Roots", name);

    let declared_vars: std::collections::HashMap<String, &syn::Type> =
        vars.iter().map(|(v, t)| (v.to_string(), t)).collect();

    let mut sel_entries = Vec::new();
    let mut root_struct_fields = Vec::new();
    let mut root_accessors = Vec::new();
    let mut edge_checks = Vec::new();
    let mut route_roots_impl = None;

    for root in &roots {
        let field = &root.field;
        let field_str = field.to_string();
        let child = &root.child;

        let root_def = schema.root_def(&field_str).ok_or_else(|| {
            syn::Error::new(
                field.span(),
                format!("`{field_str}` is not a root in the published schema ({schema_path})"),
            )
        })?;
        if root_def.list != root.list {
            return Err(syn::Error::new(
                field.span(),
                format!(
                    "root `{field_str}` is {} in the published schema, but is selected as {}",
                    if root_def.list { "a list" } else { "single" },
                    if root.list { "a list" } else { "single" },
                ),
            ));
        }
        // Every argument the operation passes must be a schema argument, fed by a
        // declared variable. (The schema may allow more args than an operation uses —
        // the executor sees whatever the persisted artifact recorded.)
        for (param, var) in &root.args {
            let param_str = param.to_string();
            if !root_def.args.iter().any(|arg| arg.name == param_str) {
                return Err(syn::Error::new(
                    param.span(),
                    format!(
                        "root `{field_str}` has no argument `{param_str}` in the published schema"
                    ),
                ));
            }
            if !declared_vars.contains_key(&var.to_string()) {
                return Err(syn::Error::new(
                    var.span(),
                    format!("`${var}` is not declared in this operation's variables"),
                ));
            }
        }
        // The child fragment must be on the record this root yields.
        edge_checks.push(edge_target_check(child, &root_def.output, "Query", &field_str));

        let arg_params = root.args.iter().map(|(param, _)| param.to_string());
        let list = root.list;
        sel_entries.push(quote! {
            ::idyll_data::Sel::Root {
                field: #field_str,
                args: &[ #(#arg_params),* ],
                list: #list,
                frag: <#child as ::idyll_data::Fragment>::DEF,
            }
        });

        // Roots carry the executed records' ids, typed (the executor's roots object
        // deserializes straight into this); the accessors hand out `Frag` references
        // — masking holds.
        if root.list {
            root_struct_fields.push(quote! {
                #field: ::std::vec::Vec<<#child as ::idyll_data::NodeFragment>::Id>
            });
            root_accessors.push(quote! {
                /// The executed root list, as child `Frag` references (masked — no
                /// record data).
                pub fn #field(&self) -> ::std::vec::Vec<::idyll_data::Frag<#child>> {
                    self.#field.iter().cloned().map(::idyll_data::Frag::from_id).collect()
                }
            });
        } else {
            root_struct_fields.push(quote! {
                #field: <#child as ::idyll_data::NodeFragment>::Id
            });
            root_accessors.push(quote! {
                /// The executed root, as a child `Frag` reference (masked — no record
                /// data).
                pub fn #field(&self) -> ::idyll_data::Frag<#child> {
                    ::idyll_data::Frag::from_id(::core::clone::Clone::clone(&self.#field))
                }
            });
            if field_str == "route" {
                route_roots_impl = Some(quote! {
                    impl ::idyll_data::route::RouteRoots for #roots_name {
                        type Page = #child;
                        fn page(&self) -> ::idyll_data::Frag<#child> {
                            self.route()
                        }
                    }
                });
            }
        }
    }

    let var_fields = vars.iter().map(|(var, ty)| {
        quote! { pub #var: #ty }
    });

    Ok(quote! {
        pub struct #name;

        // Rebuild when the published contract changes.
        const _: &str = ::core::include_str!(#schema_path);

        #(#edge_checks)*

        #[allow(non_upper_case_globals)]
        static #def_ident: ::idyll_data::FragmentDef = ::idyll_data::FragmentDef {
            name: #name_str,
            on: "Query",
            selection: &[ #(#sel_entries),* ],
        };

        impl #name {
            /// The normalized root operation (fragments inlined).
            pub const DEF: &'static ::idyll_data::FragmentDef = &#def_ident;

            /// The content-addressed `<hash>.query` artifact for this operation — what
            /// the route table hands the server, and what the server writes into the
            /// persisted registry at boot.
            pub fn query_file() -> ::idyll_data::QueryFile {
                ::idyll_data::QueryFile::from_def(Self::DEF)
            }

            /// The operation's persisted identity — the only thing a client ever sends.
            pub fn hash() -> ::idyll_data::OpHash {
                ::idyll_data::CanonOp::from_def(Self::DEF).op_hash()
            }
        }

        impl ::idyll::Query for #name {
            fn op_hash() -> ::idyll::OpHash {
                Self::hash()
            }
        }

        /// The operation's input variables.
        pub struct #vars_name {
            #(#var_fields),*
        }

        /// This operation's **roots** — the top-level results a route component reads
        /// from its [`Preloaded`](::idyll_data::Preloaded) token. Each field is a raw
        /// wire id (serializable — the executor's output deserializes straight into
        /// this); the accessors hand out `Frag` keys, never record data, so masking
        /// holds.
        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::serde::Serialize, ::serde::Deserialize)]
        pub struct #roots_name {
            #(#root_struct_fields),*
        }

        impl #roots_name {
            #(#root_accessors)*
        }

        #route_roots_impl
    })
}

// ── mutation! — a persisted mutation: schema mutation + response selection ─────────

struct MutationInput {
    name: Ident,
    vars: Vec<(Ident, syn::Type)>,
    wire: syn::LitStr,
    args: Vec<(Ident, Ident)>, // (param, variable)
    selection: Vec<RespSel>,
}

/// One entry in a mutation's response selection: a scalar leaf, or an **inline nested
/// selection** on an embedded value edge (`wallet { amount }`). Responses are transient
/// owned data, so nesting is inline — no fragment refs, no reactive machinery.
enum RespSel {
    Leaf(Ident),
    Nested { field: Ident, children: Vec<RespSel> },
}

fn parse_resp_selection(body: syn::parse::ParseStream) -> Result<Vec<RespSel>> {
    let mut selection = Vec::new();
    while !body.is_empty() {
        let field: Ident = body.parse()?;
        if body.peek(token::Brace) {
            let inner;
            braced!(inner in body);
            selection.push(RespSel::Nested { field, children: parse_resp_selection(&inner)? });
        } else {
            selection.push(RespSel::Leaf(field));
        }
        if body.peek(Token![,]) {
            body.parse::<Token![,]>()?;
        } else {
            break;
        }
    }
    Ok(selection)
}

impl Parse for MutationInput {
    fn parse(input: ParseStream) -> Result<Self> {
        let name: Ident = input.parse()?;

        let var_content;
        parenthesized!(var_content in input);
        let mut vars = Vec::new();
        while !var_content.is_empty() {
            var_content.parse::<Token![$]>()?;
            let var: Ident = var_content.parse()?;
            var_content.parse::<Token![:]>()?;
            let ty: syn::Type = var_content.parse()?;
            vars.push((var, ty));
            if var_content.peek(Token![,]) {
                var_content.parse::<Token![,]>()?;
            } else {
                break;
            }
        }

        input.parse::<Token![=]>()?;
        let wire: syn::LitStr = input.parse()?;

        let mut args = Vec::new();
        if input.peek(token::Paren) {
            let arg_content;
            parenthesized!(arg_content in input);
            while !arg_content.is_empty() {
                let param: Ident = arg_content.parse()?;
                arg_content.parse::<Token![:]>()?;
                arg_content.parse::<Token![$]>()?;
                let var: Ident = arg_content.parse()?;
                args.push((param, var));
                if arg_content.peek(Token![,]) {
                    arg_content.parse::<Token![,]>()?;
                } else {
                    break;
                }
            }
        }

        let body;
        braced!(body in input);
        let selection = parse_resp_selection(&body)?;

        Ok(MutationInput { name, vars, wire, args, selection })
    }
}

/// `mutation! { AddTodoOp($text: String) = "add-todo"(text: $text) { text, done } }`
///
/// A **persisted mutation operation**: validated against the published schema (the
/// mutation exists, the arguments are declared, the response selection typechecks on
/// the output record), canonicalized, and content-addressed — the client only ever
/// sends the resulting `OpHash`. Generates the handle (`mutation_file()`, the
/// [`Mutation`](idyll::Mutation) impl), the typed `<Name>Vars`, and the
/// `<Name>Response` projection struct (exactly the selection's fields, schema-typed).
/// Response selections are flat for now — masking still applies field-for-field.
#[proc_macro]
pub fn mutation(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as MutationInput);
    match mutation_impl(input) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.to_compile_error().into(),
    }
}

fn mutation_impl(input: MutationInput) -> Result<TokenStream2> {
    let MutationInput { name, vars, wire, args, selection } = input;
    let (schema, schema_path) = load_schema(name.span())?;

    let wire_str = wire.value();
    let def = schema.mutation_def(&wire_str).ok_or_else(|| {
        syn::Error::new(
            wire.span(),
            format!("`{wire_str}` is not a mutation in the published schema ({schema_path})"),
        )
    })?;
    let output = def.output.clone();
    let record = schema.record(&output).ok_or_else(|| {
        syn::Error::new(
            wire.span(),
            format!("mutation `{wire_str}` yields `{output}`, which is not in the schema"),
        )
    })?;

    let declared_vars: std::collections::HashMap<String, &syn::Type> =
        vars.iter().map(|(v, t)| (v.to_string(), t)).collect();

    // Validate + type the passed arguments (param name = the wire field of Vars).
    let mut var_fields = Vec::new();
    let mut param_names = Vec::new();
    for (param, var) in &args {
        let param_str = param.to_string();
        if !def.args.iter().any(|a| a.name == param_str) {
            return Err(syn::Error::new(
                param.span(),
                format!("mutation `{wire_str}` has no argument `{param_str}` in the published schema"),
            ));
        }
        let ty = declared_vars.get(&var.to_string()).ok_or_else(|| {
            syn::Error::new(var.span(), format!("`${var}` is not declared in this operation's variables"))
        })?;
        var_fields.push(quote! { pub #param: #ty });
        param_names.push(param_str);
    }

    // Validate + type the response selection against the output record, recursively:
    // leaves are scalars; nested selections follow embedded value edges (owned nested
    // structs). Node references stay out until mutations can fetch.
    let name_str = name.to_string();
    let response_name = format_ident!("{}Response", name);
    let def_ident = format_ident!("__IDYLL_MUTATION_DEF_{}", name);
    let mut nested_items = Vec::new();
    let (sel_entries, response_fields) = resp_codegen(
        &schema,
        &schema_path,
        record,
        &selection,
        &response_name.to_string(),
        &def_ident.to_string(),
        &mut nested_items,
    )?;

    let vars_name = format_ident!("{}Vars", name);
    let response_def_name = format!("{name_str}Response");
    let params = param_names.iter().map(|p| p.as_str()).collect::<Vec<_>>();

    Ok(quote! {
        pub struct #name;

        // Rebuild when the published contract changes.
        const _: &str = ::core::include_str!(#schema_path);

        #[allow(non_upper_case_globals)]
        static #def_ident: ::idyll_data::FragmentDef = ::idyll_data::FragmentDef {
            name: #response_def_name,
            on: #output,
            selection: &[ #(#sel_entries),* ],
        };

        impl #name {
            /// The content-addressed `<hash>.mutation` artifact — what the server's
            /// `.mutations(...)` list publishes into the persisted registry at boot.
            pub fn mutation_file() -> ::idyll_data::MutationFile {
                ::idyll_data::MutationFile::from_parts(#wire, &[ #(#params),* ], &#def_ident)
            }
        }

        impl ::idyll::Mutation for #name {
            fn op_hash() -> ::idyll_data::OpHash {
                ::idyll_data::CanonMutation::new(#wire, &[ #(#params),* ], &#def_ident).op_hash()
            }
            type Vars = #vars_name;
            type Response = #response_name;
        }

        /// The operation's input variables (serialized as the named wire arguments).
        #[derive(::serde::Serialize)]
        pub struct #vars_name {
            #(#var_fields),*
        }

        /// The masked response — exactly the selection's fields, schema-typed.
        #[derive(::core::fmt::Debug, ::core::clone::Clone, ::serde::Deserialize)]
        pub struct #response_name {
            #(#response_fields),*
        }

        #(#nested_items)*
    })
}

/// Recursive response codegen: for each selection level, the artifact's `Sel` entries
/// and the owned struct's fields; nested value selections emit their child
/// `FragmentDef` statics + response structs into `nested_items`.
fn resp_codegen(
    schema: &idyll_schema::Schema,
    schema_path: &str,
    record: &idyll_schema::RecordDef,
    selection: &[RespSel],
    type_prefix: &str,
    def_prefix: &str,
    nested_items: &mut Vec<TokenStream2>,
) -> Result<(Vec<TokenStream2>, Vec<TokenStream2>)> {
    let mut sel_entries = Vec::new();
    let mut fields = Vec::new();
    for sel in selection {
        match sel {
            RespSel::Leaf(field) => {
                let field_str = field.to_string();
                let field_def =
                    record.fields.iter().find(|f| f.name == field_str).ok_or_else(|| {
                        syn::Error::new(
                            field.span(),
                            format!(
                                "`{}` has no field `{field_str}` in the published schema ({schema_path})",
                                record.name
                            ),
                        )
                    })?;
                let rust_ty = leaf_rust_type(&field_def.ty, field.span(), schema)?;
                sel_entries.push(quote! { ::idyll_data::Sel::Leaf { field: #field_str } });
                fields.push(quote! { pub #field: #rust_ty });
            }
            RespSel::Nested { field, children } => {
                let field_str = field.to_string();
                let field_def =
                    record.fields.iter().find(|f| f.name == field_str).ok_or_else(|| {
                        syn::Error::new(
                            field.span(),
                            format!(
                                "`{}` has no field `{field_str}` in the published schema ({schema_path})",
                                record.name
                            ),
                        )
                    })?;
                // Only embedded values nest — the mutation executor doesn't fetch, so a
                // Node reference has nothing to select from.
                let (value_name, is_list) = match &field_def.ty {
                    idyll_schema::FieldType::Value { value } => (value.clone(), false),
                    idyll_schema::FieldType::List { of } => match &**of {
                        idyll_schema::FieldType::Value { value } => (value.clone(), true),
                        other => {
                            return Err(syn::Error::new(
                                field.span(),
                                format!(
                                    "`{}.{field_str}` is a list of {other:?} — mutation responses \
                                     nest embedded values only (references need a fetch)",
                                    record.name
                                ),
                            ))
                        }
                    },
                    other => {
                        return Err(syn::Error::new(
                            field.span(),
                            format!(
                                "`{}.{field_str}` is {other:?} — mutation responses nest \
                                 embedded values only (references need a fetch)",
                                record.name
                            ),
                        ))
                    }
                };
                let child_record = schema.record(&value_name).ok_or_else(|| {
                    syn::Error::new(
                        field.span(),
                        format!("`{value_name}` is not a record in the published schema ({schema_path})"),
                    )
                })?;

                let camel = pascal(&field_str);
                let child_type = format_ident!("{type_prefix}{camel}");
                let child_def = format_ident!("{def_prefix}_{field_str}");
                let (child_sels, child_fields) = resp_codegen(
                    schema,
                    schema_path,
                    child_record,
                    children,
                    &child_type.to_string(),
                    &child_def.to_string(),
                    nested_items,
                )?;
                let child_def_name = child_type.to_string();
                nested_items.push(quote! {
                    #[allow(non_upper_case_globals)]
                    static #child_def: ::idyll_data::FragmentDef = ::idyll_data::FragmentDef {
                        name: #child_def_name,
                        on: #value_name,
                        selection: &[ #(#child_sels),* ],
                    };

                    /// A nested level of the masked response.
                    #[derive(::core::fmt::Debug, ::core::clone::Clone, ::serde::Deserialize)]
                    pub struct #child_type {
                        #(#child_fields),*
                    }
                });

                if is_list {
                    sel_entries.push(quote! {
                        ::idyll_data::Sel::List { edge: #field_str, frag: &#child_def }
                    });
                    fields.push(quote! { pub #field: ::std::vec::Vec<#child_type> });
                } else {
                    sel_entries.push(quote! {
                        ::idyll_data::Sel::Spread { edge: #field_str, frag: &#child_def }
                    });
                    fields.push(quote! { pub #field: #child_type });
                }
            }
        }
    }
    Ok((sel_entries, fields))
}

// ── guest! — the wasm component module, generated from the live table ───────────

/// One `component(bindings…)` entry — the live's name IS the component's ident.
/// Bindings are the mount vocabulary: `seed` (the decoded route query) and
/// `key: Type` (the typed row identity — the marker's `key = expr` must be exactly
/// `Type`, and `Type: FromLiveKey` decodes it here).
struct GuestLive {
    component: Ident,
    bindings: Vec<GuestBinding>,
}

enum GuestBinding {
    Seed,
    Key(syn::Type),
}

/// The mount table's reserved names: the document mounts every app declares
/// (`page` required, `head` optional), distinct from the marker-mountable live.
const RESERVED_MOUNTS: &[&str] = &["page", "head"];

struct GuestInput {
    seed_ty: syn::Type,
    /// The route view — the root component the server (and, on transitions, the
    /// browser) mounts per page. Reserved table name `"page"`.
    page: GuestLive,
    /// Document-head content (meta/link). Reserved table name `"head"`.
    head: Option<GuestLive>,
    live: Vec<GuestLive>,
}

fn parse_guest_entry(content: ParseStream) -> Result<GuestLive> {
    let component: Ident = content.parse()?;
    let args;
    syn::parenthesized!(args in content);
    let mut bindings = Vec::new();
    while !args.is_empty() {
        let binding: Ident = args.parse()?;
        if binding == "seed" {
            bindings.push(GuestBinding::Seed);
        } else if binding == "key" {
            args.parse::<Token![:]>()?;
            bindings.push(GuestBinding::Key(args.parse()?));
        } else {
            return Err(syn::Error::new_spanned(
                &binding,
                "live bindings are `seed` and `key: Type`",
            ));
        }
        if args.peek(Token![,]) {
            args.parse::<Token![,]>()?;
        }
    }
    Ok(GuestLive { component, bindings })
}

impl Parse for GuestInput {
    fn parse(input: ParseStream) -> Result<Self> {
        let seed_kw: Ident = input.parse()?;
        if seed_kw != "seed" {
            return Err(syn::Error::new_spanned(&seed_kw, "expected `seed: <Type>`"));
        }
        input.parse::<Token![:]>()?;
        let seed_ty: syn::Type = input.parse()?;
        input.parse::<Token![,]>()?;

        let page_kw: Ident = input.parse()?;
        if page_kw != "page" {
            return Err(syn::Error::new_spanned(
                &page_kw,
                "expected `page: <component>(seed)` — the route view is the root \
                 component; every app declares one",
            ));
        }
        input.parse::<Token![:]>()?;
        let page = parse_guest_entry(input)?;
        input.parse::<Token![,]>()?;

        let mut head = None;
        let mut next_kw: Ident = input.parse()?;
        if next_kw == "head" {
            input.parse::<Token![:]>()?;
            head = Some(parse_guest_entry(input)?);
            input.parse::<Token![,]>()?;
            next_kw = input.parse()?;
        }
        for entry in std::iter::once(&page).chain(head.as_ref()) {
            if entry.bindings.iter().any(|b| matches!(b, GuestBinding::Key(_))) {
                return Err(syn::Error::new_spanned(
                    &entry.component,
                    "the document mounts (`page`/`head`) take only `seed` — they have no markers to key",
                ));
            }
        }

        if next_kw != "live" {
            return Err(syn::Error::new_spanned(&next_kw, "expected `live: { … }`"));
        }
        input.parse::<Token![:]>()?;
        let content;
        braced!(content in input);
        let mut live = Vec::new();
        while !content.is_empty() {
            let entry = parse_guest_entry(&content)?;
            if RESERVED_MOUNTS.contains(&entry.component.to_string().as_str()) {
                return Err(syn::Error::new_spanned(
                    &entry.component,
                    "`page` and `head` are the reserved document mounts — declare them \
                     with `page:`/`head:`, not in the live table",
                ));
            }
            live.push(entry);
            if content.peek(Token![,]) {
                content.parse::<Token![,]>()?;
            }
        }
        Ok(GuestInput { seed_ty, page, head, live })
    }
}

/// The `idyll:ssr` world, embedded so apps never carry a path to the WIT.
const SSR_WIT: &str = include_str!("../../idyll-host/wit/ssr.wit");

/// `guest! { seed: PageSeed, page: page(seed), head: head(seed), live: { … } }`
///
/// Expands to the app's entire wasm component module: the `idyll:ssr` `app`-world
/// exports (mount/unmount/dispatch/deliver), the shared guest runtime, and the
/// IR/command mirrors — framework machinery, generated from the one thing that is
/// the app's: its mount table. The **page is the root component** (reserved name
/// `"page"`; `"head"` is the optional document-head content) — the server mounts it
/// through the membrane per request, the browser claims that mount on first load and
/// re-mounts it on transitions. Live are the marker-mountable entries.
/// `live()` derives from the table, so a marker the table doesn't know — or a
/// missing page — is caught by the server's validation.
#[proc_macro]
pub fn guest(input: TokenStream) -> TokenStream {
    let GuestInput { seed_ty, page, head, live } = parse_macro_input!(input as GuestInput);

    // The mount table: the reserved document mounts first, then the live. One
    // table, one `mount` — the page is the root component of the same machinery.
    let entries: Vec<(String, &GuestLive)> = std::iter::once(("page".to_string(), &page))
        .chain(head.iter().map(|head| ("head".to_string(), head)))
        .chain(live.iter().map(|live| (island_name(&live.component), live)))
        .collect();

    let island_names: Vec<&String> = entries.iter().map(|(name, _)| name).collect();

    // Per entry: a TAIT for its root future, a `make_*` fn whose body is the TAIT's sole
    // defining use, an `IslandRoot` variant holding `Pin<Box<that future>>`, the Future
    // impl arm that polls it, and the mount arm that builds the variant. Boxing the
    // future keeps `IslandRoot` `Unpin` (safe projection) while the poll still lands on
    // a concrete future — a direct call, so the live's code is reachable.
    let root_types: Vec<_> = (0..entries.len()).map(|i| format_ident!("Root{i}")).collect();
    let make_fns: Vec<_> = (0..entries.len()).map(|i| format_ident!("make_root{i}")).collect();
    let variants: Vec<_> = (0..entries.len()).map(|i| format_ident!("V{i}")).collect();

    // The concrete arg types (the macro has them: the seed type, the key type), so
    // `make_root*` can spell its signature and the live's message type is the only thing
    // left to inference.
    let arg_types_for = |live: &GuestLive| -> Vec<proc_macro2::TokenStream> {
        live
            .bindings
            .iter()
            .map(|binding| match binding {
                GuestBinding::Seed => quote! { #seed_ty },
                GuestBinding::Key(ty) => quote! { #ty },
            })
            .collect()
    };

    let root_defs = entries.iter().enumerate().map(|(i, (_, live))| {
        let component = &live.component;
        let root_ty = &root_types[i];
        let make_fn = &make_fns[i];
        let arg_tys = arg_types_for(live);
        let arg_ns = (0..live.bindings.len()).map(|j| format_ident!("a{j}"));
        let arg_ns2 = (0..live.bindings.len()).map(|j| format_ident!("a{j}"));
        // The defining use must **directly** return the opaque `#root_ty` (a tuple
        // position is not recognised), so the context handle rides an out-param, filled
        // before the future captures `ctx`. The body is an `async move` block — a
        // concrete anonymous future — because a TAIT set equal to another opaque
        // (`spawn_live`'s RPIT) is unconstrained. M is inferred from the live's function.
        let export = idyll_schema::live_make_export(i);
        quote! {
            type #root_ty = impl ::std::future::Future<Output = ()>;
            #[define_opaque(#root_ty)]
            #[cfg_attr(target_arch = "wasm32", export_name = #export)]
            fn #make_fn(
                rt: &::idyll::Runtime,
                parent: Option<&::idyll::ContextHandle>,
                args: (#(#arg_tys,)*),
                report: ::std::boxed::Box<dyn FnOnce(::std::boxed::Box<dyn ::std::error::Error>) + 'static>,
                out_contexts: &mut Option<::idyll::ContextHandle>,
            ) -> #root_ty {
                let ctx = Ctx::for_mount(rt, parent);
                *out_contexts = Some(ctx.context_handle());
                let (#(#arg_ns,)*) = args;
                async move {
                    spawn_live(|ctx| super::#component(ctx, #(#arg_ns2,)*), ctx, report).await
                }
            }
        }
    });

    let root_variant_defs = variants.iter().zip(&root_types).map(|(v, ty)| {
        quote! { #v(::std::pin::Pin<::std::boxed::Box<#ty>>) }
    });
    // Each variant's poll goes through an exported, never-inlined per-live wrapper —
    // the splitter's structural seed, declared to it as a core-module export (the
    // contract is `idyll_schema::live_root_export`, by entry index in the guest-table
    // order the splitter also sees).
    let entry_wrappers = (0..entries.len()).map(|i| {
        let wrapper = format_ident!("__idyll_live_root_{i}");
        let export = idyll_schema::live_root_export(i);
        let root_ty = &root_types[i];
        quote! {
            #[cfg_attr(target_arch = "wasm32", export_name = #export)]
            #[inline(never)]
            fn #wrapper(
                f: ::std::pin::Pin<&mut #root_ty>,
                cx: &mut ::std::task::Context<'_>,
            ) -> ::std::task::Poll<()> {
                ::std::future::Future::poll(f, cx)
            }
        }
    });
    let root_poll_arms = variants.iter().enumerate().map(|(i, v)| {
        let wrapper = format_ident!("__idyll_live_root_{i}");
        quote! { IslandRoot::#v(f) => #wrapper(f.as_mut(), cx) }
    });

    let mount_arms = entries.iter().enumerate().map(|(i, (name, live))| {
        let make_fn = &make_fns[i];
        let variant = &variants[i];
        // Decode args in the arm (a keyed live whose marker passed no key refuses here
        // with `?`), then hand them to the concrete `make_root*`.
        let arg_lets = live.bindings.iter().enumerate().map(|(j, binding)| {
            let var = format_ident!("arg{j}");
            match binding {
                GuestBinding::Seed => quote! { let #var = seed.clone(); },
                GuestBinding::Key(ty) => quote! {
                    let #var = {
                        let wire = live.key.as_deref().ok_or_else(|| {
                            format!("live `{}` is keyed — its marker must pass `key = …`", #name)
                        })?;
                        <#ty as ::idyll::live::FromLiveKey>::from_wire(wire)
                    };
                },
            }
        });
        let arg_vars = (0..live.bindings.len()).map(|j| format_ident!("arg{j}"));
        quote! {
            #name => {
                #(#arg_lets)*
                let setup_failure: ::std::rc::Rc<
                    ::std::cell::RefCell<Option<::std::string::String>>
                > = ::std::default::Default::default();
                let report: ::std::boxed::Box<dyn FnOnce(::std::boxed::Box<dyn ::std::error::Error>) + 'static> = {
                    let setup_failure = ::std::rc::Rc::clone(&setup_failure);
                    ::std::boxed::Box::new(move |error| {
                        let message = error.to_string();
                        if setup_failure.borrow().is_none() {
                            *setup_failure.borrow_mut() = Some(message.clone());
                        }
                        eprintln!("component error: {message}");
                    })
                };
                let mut contexts = None;
                let root = #make_fn(&sh.rt, parent, (#(#arg_vars,)*), report, &mut contexts);
                let contexts = contexts.expect("make_root sets the context handle");
                concrete_mount(sh, IslandRoot::#variant(::std::boxed::Box::pin(root)), contexts, &setup_failure)
            }
        }
    });

    // The typed live defs the server's markers reference: `app::live::Check`.
    let island_defs = live.iter().map(|live| {
        let component = &live.component;
        let name = island_name(&live.component);
        let def_name = format_ident!("{}", pascal(&live.component.to_string()));
        let key_ty = live
            .bindings
            .iter()
            .find_map(|binding| match binding {
                GuestBinding::Key(ty) => Some(quote! { #ty }),
                _ => None,
            })
            .unwrap_or_else(|| quote! { ::idyll::live::NoKey });
        quote! {
            #[doc = "The typed marker handle for the `"]
            #[doc = #name]
            #[doc = "` live — `@live(live::"]
            #[doc = stringify!(#def_name)]
            #[doc = ", key = …)`."]
            pub struct #def_name;

            impl ::idyll::live::LiveDef for #def_name {
                const NAME: &'static str = #name;
                type Key = #key_ty;
            }

            // Existence check: the def compiles on native too, where the entry
            // wrappers (wasm-only) never reference the component — a typo in the
            // live table must fail every build, not just the wasm one.
            const _: () = {
                fn _exists() {
                    let _ = #component;
                }
            };
        }
    });

    quote! {
        /// The typed live defs (`LiveDef`) this app exports — what a server view's
        /// `@live(app::live::…)` marker references. Native and wasm both: the
        /// server type-checks markers against these.
        pub mod live {
            #[allow(unused_imports)]
            use super::*;

            #(#island_defs)*
        }

        #[cfg(target_arch = "wasm32")]
        mod __idyll_guest {
            use super::*;
            use ::idyll::component::spawn_live;
            use ::idyll::driver::{DomDriver as _, HandlerId};
            use ::idyll::{CommandBufferDriver, Ctx, DomCommand, Event, Runtime, Setup};
            use ::std::cell::RefCell;

            ::wit_bindgen::generate!({
                inline: #SSR_WIT,
                world: "app",
            });

            // Concrete live roots. Each entry's root future is a named TAIT, boxed into
            // an `IslandRoot` variant. The executor ([`LiveDriver`]) polls `IslandRoot`,
            // whose `poll` matches to the concrete future — a **direct** call, never a
            // vtable — so a live's reducer (and everything it calls) is reachable from
            // this structural entry. Boxing keeps `IslandRoot` `Unpin` (safe projection).
            #(#root_defs)*

            enum IslandRoot {
                #(#root_variant_defs,)*
            }

            impl ::std::future::Future for IslandRoot {
                type Output = ();
                fn poll(
                    self: ::std::pin::Pin<&mut Self>,
                    cx: &mut ::std::task::Context<'_>,
                ) -> ::std::task::Poll<()> {
                    match self.get_mut() {
                        #(#root_poll_arms,)*
                    }
                }
            }

            #(#entry_wrappers)*

            ::std::thread_local! {
                /// The ONE guest runtime: every live mounts into it. Shared signals
                /// and a shared node-id/handler-id space make cross-live reactivity
                /// ordinary same-thread reactivity. On the server each mount runs in a
                /// fresh store (fresh TLS); in the browser this lives for the page.
                static SHARED: RefCell<Option<Shared>> = RefCell::new(None);
                /// Mounted live roots by mount identity: the guards whose drop
                /// unmounts the root, plus the context frame descendants inherit.
                static ISLANDS: RefCell<::std::collections::HashMap<(String, String), IslandMount>> =
                    RefCell::new(::std::collections::HashMap::new());
            }

            struct Shared {
                rt: Runtime,
                driver: CommandBufferDriver,
                /// Live reducer loops, driven concretely (no boxed tasks). The `rt`
                /// keeps only leaf work (client effects, cleanups).
                live: ::idyll::live_driver::LiveDriver<IslandRoot>,
            }

            struct IslandMount {
                /// The concrete reducer future's slot — dropped on unmount, which drops
                /// the `Ctx` and disposes the component's owner (its signals/effects).
                slot: ::idyll::live_driver::SlotId,
                /// DOM/listener cleanup guards from `mount_root` — dropped on unmount.
                _guards: Vec<::idyll::MountGuard>,
                contexts: ::idyll::ContextHandle,
            }

            fn with_shared<R>(f: impl FnOnce(&mut Shared) -> R) -> R {
                SHARED.with(|shared| {
                    let mut shared = shared.borrow_mut();
                    let shared = shared.get_or_insert_with(|| Shared {
                        rt: Runtime::new(),
                        driver: CommandBufferDriver::new(),
                        live: ::idyll::live_driver::LiveDriver::new(),
                    });
                    f(shared)
                })
            }

            /// The first slice's budget: enough that a typical live's initial paint
            /// completes in one call, small enough to bound a pathological mount. Both
            /// hosts loop `flush` past it, so it never truncates a paint — it only
            /// caps how much rides the first return.
            const INITIAL_FLUSH_BUDGET: u32 = 2048;

            /// One interruptible flush **slice** on the wire: run up to `budget`
            /// effect-step units (highest lane first), hand back the commands produced,
            /// and report whether the graph settled — and if not, the lane still
            /// pending. On `done` the runtime reclaims removed nodes (inside
            /// `flush_budgeted`). The caller loops this until `done`.
            fn slice(sh: &mut Shared, budget: u32) -> FlushResult {
                let status = sh.rt.flush_budgeted(
                    &mut sh.driver,
                    ::idyll::FlushBudget { lane_items: budget as usize },
                );
                let commands = sh.driver.take_commands().into_iter().map(Command::from).collect();
                FlushResult {
                    commands,
                    done: status.complete,
                    pending_lane: sh.rt.pending_lane().map(|lane| match lane {
                        ::idyll::Lane::Input => Lane::Input,
                        ::idyll::Lane::Idle => Lane::Idle,
                    }),
                }
            }

            /// Mount one live root **concretely**: push its future into the driver
            /// (which polls it once — running setup + initial render, posting the view),
            /// refuse if setup failed, then process the posted view into DOM. Does not
            /// flush — the caller drives `slice` so the initial paint streams under a
            /// budget.
            ///
            /// A setup failure (`Store::of(…)?`, a read that could not resolve) surfaces on
            /// that first poll: the live has nothing above it to catch a fault, but it
            /// has a typed `err` arm, and both hosts answer it — the server ships the
            /// region unpainted and says why, the browser leaves the wrapper inert. A
            /// failure after this point has no mount left to refuse; it reports to the log.
            fn concrete_mount(
                sh: &mut Shared,
                root: IslandRoot,
                contexts: ::idyll::ContextHandle,
                setup_failure: &::std::rc::Rc<::std::cell::RefCell<Option<::std::string::String>>>,
            ) -> ::std::result::Result<IslandMount, ::std::string::String> {
                let slot = sh.live.mount(root);
                if let Some(message) = setup_failure.borrow_mut().take() {
                    sh.live.remove(slot);
                    return Err(message);
                }
                let guards = sh.rt.mount_root(&mut sh.driver);
                Ok(IslandMount { slot, _guards: guards, contexts })
            }

            /// Build one live's args from the seed and mount it. Every way this can
            /// refuse — an unknown name, a keyed live whose marker passed no key —
            /// is the typed `err` arm, never a trap: guests are `panic=abort`, and the
            /// browser shares ONE instance across every live on the page, so a panic
            /// here would kill the live's neighbours too.
            fn mount_island(
                sh: &mut Shared,
                live: &LiveRef,
                parent: Option<&::idyll::ContextHandle>,
                seed: #seed_ty,
            ) -> ::std::result::Result<IslandMount, ::std::string::String> {
                match live.name.as_str() {
                    #(#mount_arms)*
                    _ => Err(format!(
                        "unknown live `{}` — is the live table in sync with the pages?",
                        live.name
                    )),
                }
            }

            struct App;

            impl Guest for App {
                fn live() -> Vec<String> {
                    vec![#(#island_names.to_string()),*]
                }


                /// Mount one live instance from the page seed. On the server this
                /// paints; in the browser the identical call claims the paint and the
                /// root stays live for `dispatch`. App-level failure — an undecodable
                /// seed, an unknown marker — is the typed `Err` arm: loud,
                /// attributable, and the instance stays healthy for its neighbours.
                fn mount(
                    live: LiveRef,
                    parent: Option<LiveRef>,
                    seed: Vec<u8>,
                    client: bool,
                ) -> ::std::result::Result<MountResult, String> {
                    let seed = <#seed_ty>::decode(&seed)
                        .map_err(|err| format!("page seed does not decode: {err}"))?;
                    // The caller states the environment; a server paint must not run
                    // client effects (they re-run on the browser mount).
                    if !client {
                        with_shared(|sh| sh.rt.mark_server_paint());
                    }
                    // Remount by identity unseats the old root first; its free-nodes
                    // ride this same stream. Keyed live identify by (name, key);
                    // keyless by (name, document-order instance).
                    let key = (
                        live.name.clone(),
                        live
                            .key
                            .clone()
                            .unwrap_or_else(|| format!("#{}", live.instance)),
                    );
                    // Unseat as `unmount` does: the slot removal drops the old reducer
                    // future — a guard drop alone would leave it polled forever.
                    if let Some(old) = ISLANDS.with(|map| map.borrow_mut().remove(&key)) {
                        with_shared(|sh| sh.live.remove(old.slot));
                    }
                    // The enclosing live's context frame: document order guarantees
                    // it mounted first, so absence is a broken contract, not a race.
                    let parent_contexts = match parent {
                        None => None,
                        Some(p) => {
                            let parent_key = (
                                p.name.clone(),
                                p.key.clone().unwrap_or_else(|| format!("#{}", p.instance)),
                            );
                            Some(
                                ISLANDS
                                    .with(|map| {
                                        map.borrow().get(&parent_key).map(|m| m.contexts.clone())
                                    })
                                    .ok_or_else(|| {
                                        format!(
                                            "parent live `{}`#{} is not mounted",
                                            p.name, p.instance
                                        )
                                    })?,
                            )
                        }
                    };
                    with_shared(|sh| {
                        let root = mount_island(sh, &live, parent_contexts.as_ref(), seed)?;
                        // The island's frame, held to read its static-paint verdict once the
                        // initial flush has run every view (nested rows/branches disqualify
                        // during the flush, not the first poll).
                        let frame = root.contexts.clone();
                        ISLANDS.with(|map| map.borrow_mut().insert(key, root));
                        let flush = slice(sh, INITIAL_FLUSH_BUDGET);
                        Ok(MountResult { flush, static_paint: frame.paint_is_static() })
                    })
                }

                /// Unmount one live root: drop its guards (task cancel,
                /// listener/cell release) and return the resulting stream. Unknown
                /// identity: no-op.
                fn unmount(live: LiveRef) -> FlushResult {
                    let key = (
                        live.name,
                        live.key.unwrap_or_else(|| format!("#{}", live.instance)),
                    );
                    let removed = ISLANDS.with(|map| map.borrow_mut().remove(&key));
                    let Some(mount) = removed else {
                        return FlushResult { commands: Vec::new(), done: true, pending_lane: None };
                    };
                    with_shared(|sh| {
                        // Drop the reducer future (its `Ctx`/owner dispose the component's
                        // cells) and the DOM/listener guards.
                        sh.live.remove(mount.slot);
                        drop(mount);
                        slice(sh, INITIAL_FLUSH_BUDGET)
                    })
                }

                /// Handler ids are global in the shared runtime, so the live ref is
                /// attribution, not routing: fire the handler, run everything woken by
                /// it — including OTHER live reading the same store — and flush one
                /// stream.
                fn dispatch(_live: LiveRef, handler: u32, event: DomEvent) -> FlushResult {
                    let ev = Event {
                        target_value: event.target_value,
                        key: event.key,
                        timestamp: event.timestamp,
                        rect: event.rect.map(|r| ::idyll::Rect {
                            x: r.x,
                            y: r.y,
                            width: r.width,
                            height: r.height,
                        }),
                    };
                    with_shared(|sh| {
                        sh.driver.dispatch_event(HandlerId(handler), ev);
                        // Advance the live reducers woken by the event, then any leaf
                        // task, then flush the reactive effects they triggered.
                        sh.live.drain();
                        sh.rt.run_to_quiescence();
                        slice(sh, INITIAL_FLUSH_BUDGET)
                    })
                }

                /// A server-mutation response coming home: resolve it to a message in
                /// the requesting component's inbox, then pump exactly like `dispatch`.
                fn deliver(
                    _live: LiveRef,
                    request: u32,
                    response: ::std::result::Result<Vec<u8>, RequestError>,
                ) -> FlushResult {
                    let response = response.map_err(|err| match err {
                        RequestError::Transport(detail) => {
                            ::idyll::RequestError::Transport(detail)
                        }
                        RequestError::Http(refusal) => ::idyll::RequestError::Http {
                            status: refusal.status,
                            body: refusal.body,
                        },
                    });
                    with_shared(|sh| {
                        sh.rt.deliver_response(::idyll::driver::RequestId(request), response);
                        sh.live.drain();
                        sh.rt.run_to_quiescence();
                        slice(sh, INITIAL_FLUSH_BUDGET)
                    })
                }

                /// Drive more pending reactive work under a budget and return the next
                /// slice — the interruptible seam. The browser scheduler loops this,
                /// yielding to the event loop (and delivering queued events) between
                /// calls; the SSR host loops it with no yield to drain the full paint.
                fn flush(budget: u32) -> FlushResult {
                    with_shared(|sh| slice(sh, budget))
                }
            }

            export!(App);

            /// Mirror idyll's Template IR onto the WIT `template`. Pure data → data;
            /// the exhaustive match keeps the two in lockstep.
            fn wit_template(template: &::idyll::template::Template) -> Vec<TplNode> {
                template
                    .nodes
                    .iter()
                    .map(|node| match node {
                        ::idyll::template::TplNode::Text(text) => TplNode::Text(text.to_string()),
                        ::idyll::template::TplNode::TextSlot(slot) => TplNode::TextSlot(slot.0),
                        ::idyll::template::TplNode::AnchorSlot(slot) => TplNode::AnchorSlot(slot.0),
                        ::idyll::template::TplNode::Element { tag, attrs, slot, children } => {
                            TplNode::Element(TplElement {
                                tag: tag.to_string(),
                                attrs: attrs
                                    .iter()
                                    .map(|attr| TplAttr {
                                        name: attr.name.to_string(),
                                        value: attr.value.to_string(),
                                    })
                                    .collect(),
                                slot: slot.map(|s| s.0),
                                children: *children,
                            })
                        }
                        ::idyll::template::TplNode::Live { name, key, fallback } => {
                            TplNode::Live(TplLive {
                                name: name.to_string(),
                                key: key.as_ref().map(|k| k.to_string()),
                                fallback: *fallback,
                            })
                        }
                    })
                    .collect()
            }

            /// Mirror idyll's `DomCommand` onto the WIT `command` variant. Adding a
            /// `DomCommand` variant is a compile error here until it's mapped.
            impl From<DomCommand> for Command {
                fn from(c: DomCommand) -> Command {
                    match c {
                        DomCommand::ReplaceTemplate { template_id, template } => {
                            Command::ReplaceTemplate(TemplateCmd {
                                template_id: template_id.0,
                                nodes: wit_template(&template),
                                svg: template.svg,
                                styles: template
                                    .styles
                                    .iter()
                                    .map(|rule| StyleRule {
                                        name: rule.name.to_string(),
                                        css: rule.css.as_ref().map(|css| css.to_string()),
                                    })
                                    .collect(),
                            })
                        }
                        DomCommand::SetText { node_id, text } => {
                            Command::SetText(TextCmd { node: node_id.0, text })
                        }
                        DomCommand::SetAttr { node_id, name, value } => {
                            Command::SetAttr(AttrCmd { node: node_id.0, name: name.into_owned(), value })
                        }
                        DomCommand::SetStyleProp { node_id, name, value } => {
                            Command::SetStyleProp(AttrCmd { node: node_id.0, name: name.into_owned(), value })
                        }
                        DomCommand::RemoveAttr { node_id, name } => {
                            Command::RemoveAttr(NameCmd { node: node_id.0, name: name.into_owned() })
                        }
                        DomCommand::SetBoolAttr { node_id, name, value } => {
                            Command::SetBoolAttr(BoolAttrCmd { node: node_id.0, name: name.into_owned(), value })
                        }
                        DomCommand::MountFragment { anchor_id, template } => {
                            Command::MountFragment(FragmentCmd { anchor: anchor_id.0, template: template.0 })
                        }
                        DomCommand::ReplaceFragment { anchor_id, template } => {
                            Command::ReplaceFragment(FragmentCmd { anchor: anchor_id.0, template: template.0 })
                        }
                        DomCommand::RemoveFragment { anchor_id } => Command::RemoveFragment(anchor_id.0),
                        DomCommand::DetachFragment { anchor_id } => Command::DetachFragment(anchor_id.0),
                        DomCommand::AttachFragment { anchor_id } => Command::AttachFragment(anchor_id.0),
                        DomCommand::MoveFragment { anchor_id, after_anchor } => {
                            Command::MoveFragment(MoveFragmentCmd {
                                anchor: anchor_id.0,
                                after: after_anchor.0,
                            })
                        }
                        DomCommand::AddEventListener { node_id, event_type, handler_id } => {
                            Command::AddEventListener(ListenerCmd {
                                node: node_id.0,
                                event_type: event_type.into_owned(),
                                handler: handler_id.0,
                            })
                        }
                        DomCommand::RemoveEventListener { node_id, event_type, handler_id } => {
                            Command::RemoveEventListener(ListenerCmd {
                                node: node_id.0,
                                event_type: event_type.into_owned(),
                                handler: handler_id.0,
                            })
                        }
                        DomCommand::WatchMeasure { node_id, handler_id } => {
                            Command::WatchMeasure(MeasureCmd { node: node_id.0, handler: handler_id.0 })
                        }
                        DomCommand::UnwatchMeasure { node_id, handler_id } => {
                            Command::UnwatchMeasure(MeasureCmd { node: node_id.0, handler: handler_id.0 })
                        }
                        DomCommand::Paint { node_id, layers, inks, deltas } => {
                            Command::Paint(PaintCmd {
                                node: node_id.0,
                                layers,
                                inks,
                                deltas: deltas
                                    .into_iter()
                                    .map(|(layer, changes, len)| LayerCmd { layer, changes, len })
                                    .collect(),
                            })
                        }
                        DomCommand::ServerRequest { request_id, op, args } => {
                            Command::ServerRequest(ServerRequestCmd {
                                request: request_id.0,
                                msb: op.msb(),
                                lsb: op.lsb(),
                                args,
                            })
                        }
                        DomCommand::Navigate { request_id, op, path } => {
                            Command::Navigate(NavigateCmd {
                                request: request_id.0,
                                msb: op.msb(),
                                lsb: op.lsb(),
                                path,
                            })
                        }
                        DomCommand::WatchNavigation { handler_id } => {
                            Command::WatchNavigation(handler_id.0)
                        }
                        DomCommand::WatchSize { handler_id } => {
                            Command::WatchSize(handler_id.0)
                        }
                        DomCommand::StartTicks { handler_id, interval_ms } => {
                            Command::StartTicks(StartTicksCmd { handler: handler_id.0, interval_ms })
                        }
                        DomCommand::StopTicks { handler_id } => Command::StopTicks(handler_id.0),
                        DomCommand::FreeNodes { node_ids } => {
                            Command::FreeNodes(node_ids.into_iter().map(|n| n.0).collect())
                        }
                        DomCommand::MountRoot { template_id } => Command::MountRoot(template_id.0),
                        DomCommand::BindSlot { slot, node_id } => {
                            Command::BindSlot(SlotNode { slot: slot.0, node: node_id.0 })
                        }
                    }
                }
            }
        }
    }
    .into()
}

// ── #[derive(Route)] — the typed URL codec ──────────────────────────────────────
//
// One `#[route("…")]` pattern per variant emits BOTH directions of `idyll_route::Route`:
// `parse` (url → route) and `url` (route → url). They are inverse by construction, so links
// and request-parsing can never drift. `{seg}` binds one path segment to a `String` field;
// `{seg*}` (last only) binds the rest to a `Vec<String>` field — segments are the unit
// because percent-encoding is per-segment.

/// One segment of a `#[route]` pattern.
enum RoutePart {
    Literal(String),
    Single(Ident),
    Rest(Ident),
}

fn parse_route_pattern(lit: &LitStr) -> Result<Vec<RoutePart>> {
    let s = lit.value();
    let mut parts = Vec::new();
    for tok in s.split('/').filter(|t| !t.is_empty()) {
        if let Some(inner) = tok.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            match inner.strip_suffix('*') {
                Some(name) => parts.push(RoutePart::Rest(Ident::new(name, lit.span()))),
                None => parts.push(RoutePart::Single(Ident::new(inner, lit.span()))),
            }
        } else {
            parts.push(RoutePart::Literal(tok.to_string()));
        }
    }
    if let Some(pos) = parts.iter().position(|p| matches!(p, RoutePart::Rest(_))) {
        if pos != parts.len() - 1 {
            return Err(syn::Error::new(lit.span(), "a catch-all `{name*}` must be the last segment"));
        }
    }
    Ok(parts)
}

fn route_impl(input: syn::DeriveInput) -> Result<TokenStream2> {
    let name = &input.ident;
    let syn::Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(&input, "#[derive(Route)] is for an enum of route variants"));
    };
    let idx = |i: usize| proc_macro2::Literal::usize_unsuffixed(i);
    let (mut parse_arms, mut url_arms) = (Vec::new(), Vec::new());

    for variant in &data.variants {
        let vname = &variant.ident;
        let attr = variant
            .attrs
            .iter()
            .find(|a| a.path().is_ident("route"))
            .ok_or_else(|| syn::Error::new_spanned(variant, "each variant needs `#[route(\"…\")]`"))?;
        let lit: LitStr = attr.parse_args()?;
        let parts = parse_route_pattern(&lit)?;
        let has_rest = matches!(parts.last(), Some(RoutePart::Rest(_)));
        let fixed_len = if has_rest { parts.len() - 1 } else { parts.len() };
        let fixed_lit = idx(fixed_len);

        let len_check = if has_rest {
            quote! { __segs.len() >= #fixed_lit }
        } else {
            let n = idx(parts.len());
            quote! { __segs.len() == #n }
        };

        let mut checks = Vec::new();
        let mut binds = Vec::new();
        let mut fields = Vec::new();
        for (i, part) in parts.iter().enumerate() {
            match part {
                RoutePart::Literal(l) => {
                    let l = LitStr::new(l, Span::call_site());
                    let ii = idx(i);
                    checks.push(quote! { if __segs[#ii] != #l { return ::core::option::Option::None; } });
                }
                RoutePart::Single(id) => {
                    let ii = idx(i);
                    binds.push(quote! { let #id = ::idyll_route::decode_segment(__segs[#ii])?; });
                    fields.push(id.clone());
                }
                RoutePart::Rest(id) => {
                    binds.push(quote! {
                        let #id = {
                            let mut __v: ::std::vec::Vec<::std::string::String> = ::std::vec::Vec::new();
                            for __s in &__segs[#fixed_lit..] {
                                __v.push(::idyll_route::decode_segment(__s)?);
                            }
                            __v
                        };
                    });
                    fields.push(id.clone());
                }
            }
        }

        let ctor = if fields.is_empty() {
            quote! { Self::#vname }
        } else {
            quote! { Self::#vname { #(#fields),* } }
        };
        parse_arms.push(quote! {
            .or_else(|| -> ::core::option::Option<Self> {
                if !(#len_check) { return ::core::option::Option::None; }
                #(#checks)*
                #(#binds)*
                ::core::option::Option::Some(#ctor)
            })
        });

        let mut push_segs = Vec::new();
        for part in &parts {
            match part {
                RoutePart::Literal(l) => {
                    let l = LitStr::new(l, Span::call_site());
                    push_segs.push(quote! { __segs.push(#l); });
                }
                RoutePart::Single(id) => push_segs.push(quote! { __segs.push(#id.as_str()); }),
                RoutePart::Rest(id) => {
                    push_segs.push(quote! { for __s in #id.iter() { __segs.push(__s.as_str()); } })
                }
            }
        }
        let pat = if fields.is_empty() {
            quote! { Self::#vname }
        } else {
            quote! { Self::#vname { #(#fields),* } }
        };
        url_arms.push(quote! {
            #pat => {
                let mut __segs: ::std::vec::Vec<&str> = ::std::vec::Vec::new();
                #(#push_segs)*
                ::idyll_route::Url::from_segments(__segs)
            }
        });
    }

    Ok(quote! {
        impl ::idyll_route::Route for #name {
            fn parse(__path: &str) -> ::core::option::Option<Self> {
                let __segs = ::idyll_route::split_path(__path);
                ::core::option::Option::None #(#parse_arms)*
            }
            fn url(&self) -> ::idyll_route::Url {
                match self {
                    #(#url_arms),*
                }
            }
        }
    })
}

/// `#[derive(Route)]` — see [`idyll_route`] for the model. One `#[route("…")]` per variant
/// emits the `parse`/`url` codec pair from a single spec.
#[proc_macro_derive(Route, attributes(route))]
pub fn derive_route(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as syn::DeriveInput);
    match route_impl(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}
