//! `#[styles]` and `props!` — the style macros.
//!
//! `#[styles] mod styles { … }` is the one place styles exist. The attribute sees the
//! module pre-expansion and rewrites each `css! {{ … }}` const initializer through
//! `idyll-styles-parse` — the same grammar the source extractor reads, so what
//! compiles and what the style table says cannot drift. The class name hashes the
//! **declaration site** (workspace-relative file + const name + property + condition)
//! — never the value — and the rendered rule text is emitted under
//! `#[cfg(not(target_arch = "wasm32"))]`, so wasm carries style identity but no style
//! text. `props!` declares a constrained property set: a marker type whose `Allows`
//! impls are exactly its members, checked wherever a `Style<Marker>` annotation meets
//! a `css!` body.

use std::path::Path;

use idyll_styles_parse as parse;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{braced, Error, Ident, Result, Token};

// ── #[styles] ─────────────────────────────────────────────────────────────────

pub(crate) fn styles_impl(
    attr: proc_macro::TokenStream,
    item: proc_macro::TokenStream,
) -> proc_macro::TokenStream {
    if !attr.is_empty() {
        return Error::new_spanned(TokenStream2::from(attr), "#[styles] takes no arguments")
            .to_compile_error()
            .into();
    }
    let module = syn::parse_macro_input!(item as syn::ItemMod);
    match expand_module(module) {
        Ok(expanded) => expanded.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_module(mut module: syn::ItemMod) -> Result<TokenStream2> {
    if module.ident != "styles" {
        return Err(Error::new(
            module.ident.span(),
            "#[styles] modules are named `styles` — the file is the identity, so one \
             module per file (the compiler enforces it)",
        ));
    }
    let Some((_, items)) = &mut module.content else {
        return Err(Error::new(
            module.ident.span(),
            "#[styles] needs an inline module body (`mod styles { … }`)",
        ));
    };

    let prefix = call_site_class_prefix()?;
    // Var identity is the crate (tokens are crate vocabulary — one Palette, many
    // atom files); classes keep file identity.
    let vars_prefix = parse::vars_prefix(&cargo_package()?);
    for item in items.iter_mut() {
        if let Some(body) = parse::vars_macro(item).map(|mac| mac.mac.tokens.clone()) {
            let group = parse::parse_vars(&vars_prefix, body)?;
            *item = syn::Item::Verbatim(emit_vars(&group));
            continue;
        }
        if let Some(body) = parse::theme_macro(item).map(|mac| mac.mac.tokens.clone()) {
            let themes = parse::parse_theme(&prefix, &vars_prefix, body)?;
            *item = syn::Item::Verbatim(emit_themes(&themes));
            continue;
        }
        if let Some(body) = parse::document_macro(item).map(|mac| mac.mac.tokens.clone()) {
            let rules = parse::parse_document(&prefix, &vars_prefix, body)?;
            *item = syn::Item::Verbatim(emit_document(&rules));
            continue;
        }
        if let Some(body) = parse::keyframes_macro(item).map(|mac| mac.mac.tokens.clone()) {
            let frames = parse::parse_keyframes(&vars_prefix, body)?;
            *item = syn::Item::Verbatim(emit_keyframes(&frames));
            continue;
        }
        let syn::Item::Const(item) = item else { continue };
        let Some(body) = parse::css_macro(item).map(|mac| mac.tokens.clone()) else {
            continue;
        };
        let atoms = parse::parse_style(&prefix, &vars_prefix, &item.ident.to_string(), body)?;
        item.expr = Box::new(syn::Expr::Verbatim(emit_style(&atoms)));
    }
    Ok(quote!(#module))
}

fn cargo_package() -> Result<String> {
    std::env::var("CARGO_PKG_NAME").map_err(|_| {
        Error::new(
            proc_macro2::Span::call_site(),
            "#[styles] needs cargo's CARGO_PKG_NAME to name the crate's var prefix",
        )
    })
}

/// A var group's typed handles: the group struct with one `Var` const per var. The
/// const carries the custom property's *name* (declaration-site identity, wasm-safe);
/// the group's `:root` rule text reaches the sheet through the extracted style table
/// like every other rule.
fn emit_vars(group: &parse::VarGroup) -> TokenStream2 {
    let ident = format_ident!("{}", group.ident);
    let consts = group.vars.iter().map(|var| {
        let field = format_ident!("{}", var.field);
        let name = &var.css_name;
        let kind = format_ident!("{}", var.kind.marker());
        quote! {
            pub const #field: ::idyll_styles::Var<::idyll_styles::kind::#kind> =
                ::idyll_styles::Var::new(#name);
        }
    });
    quote! {
        pub struct #ident;
        #[allow(non_upper_case_globals)]
        impl #ident {
            #(#consts)*
        }
    }
}

/// A `theme!` block's handles: one `Style` const per theme, whose atoms are the
/// custom-property declarations. A theme is applied with `css=[…]` like any other
/// style, so nothing here introduces a way to *use* it — only a way to declare it.
fn emit_themes(themes: &[parse::Theme]) -> TokenStream2 {
    let consts = themes.iter().map(|theme| {
        let ident = format_ident!("{}", theme.ident);
        // The group is never looked up: each overridden field asserts against the
        // group's real const at the kind its value declares, so a renamed field or a
        // colour written where a length belongs fails to compile at the override.
        let asserts = theme.var_refs.iter().map(|(group, field, kind)| {
            let group = format_ident!("{group}");
            let field = format_ident!("{field}");
            let kind = format_ident!("{}", kind.marker());
            quote! {
                const _: ::idyll_styles::Var<::idyll_styles::kind::#kind> = #group::#field;
            }
        });
        // A remap names neither kind: `same_kind` unifies the two handles, so the
        // check holds without the macro knowing the group's fields.
        let group = format_ident!("{}", theme.group);
        let remaps = theme.remaps.iter().map(|(field, source_group, source_field)| {
            let field = format_ident!("{field}");
            let source_group = format_ident!("{source_group}");
            let source_field = format_ident!("{source_field}");
            quote! {
                const _: () = ::idyll_styles::same_kind(
                    #group::#field,
                    #source_group::#source_field,
                );
            }
        });
        let atoms = theme.atoms.iter().map(|atom| {
            let class = &atom.class;
            let property = atom.property.css();
            let condition = condition_tokens(&atom.condition);
            let rule = atom.rule();
            quote! {
                ::idyll_styles::Atom {
                    class: #class,
                    property: #property,
                    condition: #condition,
                    #[cfg(not(target_arch = "wasm32"))]
                    rule: #rule,
                }
            }
        });
        quote! {
            #(#asserts)*
            #(#remaps)*
            pub const #ident: ::idyll_styles::Style = {
                const __IDYLL_ATOMS: &[::idyll_styles::Atom] = &[#(#atoms),*];
                ::idyll_styles::Style::new(__IDYLL_ATOMS)
            };
        }
    });
    quote! { #(#consts)* }
}

/// A `document!` block compiles to nothing but its var assertions — the rule text
/// reaches the sheet through the extracted style table, and no Rust names it (there
/// is no element to put a class on).
fn emit_document(rules: &[parse::DocumentRule]) -> TokenStream2 {
    let asserts = rules.iter().flat_map(|rule| &rule.var_refs).map(|(group, field, kind)| {
        let group = format_ident!("{group}");
        let field = format_ident!("{field}");
        let kind = format_ident!("{}", kind.marker());
        quote! {
            const _: ::idyll_styles::Var<::idyll_styles::kind::#kind> = #group::#field;
        }
    });
    quote! { #(#asserts)* }
}

/// A `keyframes!` block's typed handles: one `Keyframes` const per animation,
/// carrying its declaration-site name. The `@keyframes` rule text reaches the sheet
/// through the extracted style table, like a var group's `:root` rule.
fn emit_keyframes(frames: &[parse::Keyframes]) -> TokenStream2 {
    let consts = frames.iter().map(|kf| {
        let ident = format_ident!("{}", kf.ident);
        let name = &kf.name;
        // A step that animates a var asserts against the group's real const, so a
        // renamed var fails to compile instead of animating nothing.
        let asserts = kf.var_refs.iter().map(|(group, field, kind)| {
            let group = format_ident!("{group}");
            let field = format_ident!("{field}");
            let kind = format_ident!("{}", kind.marker());
            quote! {
                const _: ::idyll_styles::Var<::idyll_styles::kind::#kind> = #group::#field;
            }
        });
        quote! {
            #(#asserts)*
            #[allow(non_upper_case_globals)]
            pub const #ident: ::idyll_styles::Keyframes = ::idyll_styles::Keyframes::new(#name);
        }
    });
    quote! { #(#consts)* }
}

/// The declaring file's class prefix. `Span::file()` is the diagnostics path of the
/// file under expansion — stable across edits (unlike line numbers), which is the
/// property the whole hot-swap design rests on. The extractor derives the same
/// identity from cargo metadata; a pinned test in idyll-styles holds them together.
fn call_site_class_prefix() -> Result<String> {
    let file = proc_macro::Span::call_site().file();
    let package = std::env::var("CARGO_PKG_NAME");
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR");
    let (Ok(package), Ok(manifest_dir)) = (&package, &manifest_dir) else {
        return Err(Error::new(
            proc_macro2::Span::call_site(),
            "#[styles] needs cargo's CARGO_PKG_NAME/CARGO_MANIFEST_DIR to name the declaring file",
        ));
    };
    let identity = parse::path_identity(package, Path::new(manifest_dir), Path::new(&file))
        .ok_or_else(|| {
            Error::new(
                proc_macro2::Span::call_site(),
                format!("cannot place `{file}` under {package}'s manifest dir — #[styles] class identity is the declaring file"),
            )
        })?;
    Ok(parse::class_prefix(&identity))
}

fn emit_style(atoms: &[parse::Atom]) -> TokenStream2 {
    let atom_tokens = atoms.iter().map(|atom| {
        let class = &atom.class;
        let property = atom.property.css();
        let condition = condition_tokens(&atom.condition);
        let rule = atom.rule();
        quote! {
            ::idyll_styles::Atom {
                class: #class,
                property: #property,
                condition: #condition,
                #[cfg(not(target_arch = "wasm32"))]
                rule: #rule,
            }
        }
    });

    // A theme's declarations are custom properties: outside the table, so they carry
    // no marker and constrain nothing.
    let markers: std::collections::BTreeSet<&str> =
        atoms.iter().filter_map(|atom| atom.property.marker()).collect();
    let requires = markers.iter().map(|marker| {
        let marker = format_ident!("{marker}");
        quote! { .requires::<::idyll_styles::props::#marker>() }
    });

    // Each var reference asserts against the group's real const with the kind the
    // property demands, so the reference is a navigable Rust item and a mis-kinded
    // var (a length token in a color property) is a compile error at its span.
    let var_asserts =
        atoms.iter().flat_map(|atom| atom.var_refs.iter()).map(|(group, field, kind)| {
            let group = format_ident!("{group}");
            let field = format_ident!("{field}");
            let kind = format_ident!("{}", kind.marker());
            quote! {
                const _: ::idyll_styles::Var<::idyll_styles::kind::#kind> = #group::#field;
            }
        });

    quote! {{
        #(#var_asserts)*
        const __IDYLL_ATOMS: &[::idyll_styles::Atom] = &[#(#atom_tokens),*];
        ::idyll_styles::Style::new(__IDYLL_ATOMS)#(#requires)*
    }}
}

fn condition_tokens(condition: &parse::Condition) -> TokenStream2 {
    match condition {
        parse::Condition::None => quote!(::idyll_styles::Condition::None),
        parse::Condition::Hover => quote!(::idyll_styles::Condition::Hover),
        parse::Condition::Active => quote!(::idyll_styles::Condition::Active),
        parse::Condition::Checked => quote!(::idyll_styles::Condition::Checked),
        parse::Condition::Disabled => quote!(::idyll_styles::Condition::Disabled),
        parse::Condition::Media(query) => quote!(::idyll_styles::Condition::Media(#query)),
        parse::Condition::Element(tag) => quote!(::idyll_styles::Condition::Element(#tag)),
        parse::Condition::FocusVisible => quote!(::idyll_styles::Condition::FocusVisible),
        parse::Condition::MediaElement(query, tag) => {
            quote!(::idyll_styles::Condition::MediaElement(#query, #tag))
        }
        parse::Condition::SliderThumb => quote!(::idyll_styles::Condition::SliderThumb),
        parse::Condition::ActiveSliderThumb => {
            quote!(::idyll_styles::Condition::ActiveSliderThumb)
        }
        parse::Condition::SliderTrack => quote!(::idyll_styles::Condition::SliderTrack),
        parse::Condition::WebkitSliderThumb => {
            quote!(::idyll_styles::Condition::WebkitSliderThumb)
        }
        parse::Condition::Child(tag) => quote!(::idyll_styles::Condition::Child(#tag)),
        parse::Condition::ChildEdge(tag, edge) => {
            quote!(::idyll_styles::Condition::ChildEdge(#tag, #edge))
        }
        parse::Condition::SiblingNext(stateful, styled, state) => {
            quote!(::idyll_styles::Condition::SiblingNext(#stateful, #styled, #state))
        }
        parse::Condition::HoverChild(tag) => quote!(::idyll_styles::Condition::HoverChild(#tag)),
        parse::Condition::FocusWithinChild(tag) => {
            quote!(::idyll_styles::Condition::FocusWithinChild(#tag))
        }
        parse::Condition::MaxWidth(px) => quote!(::idyll_styles::Condition::MaxWidth(#px)),
        parse::Condition::PointerCoarse => quote!(::idyll_styles::Condition::PointerCoarse),
        parse::Condition::CheckedNthPairs(radio, panel, count) => {
            quote!(::idyll_styles::Condition::CheckedNthPairs(#radio, #panel, #count))
        }
    }
}

// ── props! ────────────────────────────────────────────────────────────────────

pub(crate) struct PropsInput {
    vis: syn::Visibility,
    name: Ident,
    members: Vec<Ident>,
}

impl Parse for PropsInput {
    fn parse(input: ParseStream) -> Result<Self> {
        let vis: syn::Visibility = input.parse()?;
        let name: Ident = input.parse()?;
        let body;
        braced!(body in input);
        let members = syn::punctuated::Punctuated::<Ident, Token![,]>::parse_terminated(&body)?
            .into_iter()
            .collect();
        Ok(PropsInput { vis, name, members })
    }
}

pub(crate) fn props_impl(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let PropsInput { vis, name, members } = syn::parse_macro_input!(input as PropsInput);

    let mut impls = Vec::new();
    for member in &members {
        let Some(property) = parse::property(&member.to_string()) else {
            return Error::new(
                member.span(),
                format!(
                    "`{member}` is not in idyll-styles' property table — the table is the \
                     typed surface; if the property is legitimate, extend it in \
                     idyll-styles-parse and its marker in idyll-styles"
                ),
            )
            .to_compile_error()
            .into();
        };
        let marker = format_ident!("{}", property.marker);
        impls.push(quote! {
            impl ::idyll_styles::Allows<::idyll_styles::props::#marker> for #name {}
        });
    }

    let alias = format_ident!("{name}Style");
    quote! {
        #vis struct #name;
        #(#impls)*
        #vis type #alias = ::idyll_styles::Style<#name>;
    }
    .into()
}
