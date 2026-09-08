//! Post-link code splitting **by reachability from declared live entries** over the
//! module's complete reference graph — direct calls plus every place a function's
//! address or a data object's address is taken, read from the linker's own relocations.
//!
//! Every piece rides the existing asset mechanism: the primary and each live chunk are
//! ordinary content-addressed files (immutable, brotli sidecars, HTTP-cached,
//! streaming-compiled). Identity is the content hash; the hash IS the filename.
//!
//! ## The graph
//!
//! `guest!` drives each live's reducer through never-inlined entry wrappers exported
//! under declared names (`idyll_schema::live_root_export`/`live_make_export`, by
//! entry index in guest-table order) — the splitter's seeds are exact export-section
//! lookups, not symbol matching. From those seeds, a function belongs to a live iff
//! it is reachable over:
//!
//! - **direct calls** (`call`/`return_call`, from the operator scan), and
//! - **address-taking** — every way a function's table index is *materialized*:
//!   - a `R_WASM_TABLE_INDEX_*` reloc in `reloc.CODE`/`reloc.DATA` (the app build preserves
//!     them with `--emit-relocs`) — a function pointer in code or in a data object;
//!   - a body `ref.func`;
//!   - a `global.get` of a GOT global — position-independent code materializes a function
//!     pointer as an immutable `i32` global whose value is a table slot, read and dispatched
//!     with no reloc at the use site. The model is structural: *any* immutable `i32` global
//!     whose value lands on an active table slot is a taking site, keyed on the value→slot
//!     mapping, never on the linker's `GOT.func` name (which lives only in the strippable name
//!     section, and rides unstable symbol mangling). Over-including a data global whose address
//!     coincides with a slot only over-attributes — it keeps a function loaded in more places,
//!     never fewer — so the superset is sound.
//!
//!   `R_WASM_MEMORY_ADDR_*` relocs point at data objects (a vtable, a `static`); those are graph
//!   nodes too — the defined data symbols in the `linking` section give their exact extents
//!   (overlapping symbols merge, so aliases cannot split the graph) — and edges flow *through*
//!   them: live code → vtable → its methods.
//!
//! This attribution is sound without seeing any call site: a `call_indirect` can only reach a
//! function whose pointer was materialized, every materialization form is enumerated above, and
//! if all of a function's sites live in one live's code then the pointer cannot exist until that
//! live's chunk — whose element segment fills the table slot — has loaded. Loading is the proof;
//! there is nothing to check at call time. The one trusted axiom is that the backend never
//! *fabricates* a table index by arithmetic — it materializes and selects; [`Analysis::read`]
//! refuses the wasm-visible ways to break that (`call_ref`, `table.get`/`set`/`init`/`copy`/
//! `fill`/`grow`, a funcref global or import, a growable or second table) rather than split.
//!
//! ## What stays in the primary
//!
//! Everything reachable from the module's **other exports** and start function (`mount`,
//! `dispatch`, the WASI/component entries) without entering a live: the host-called surface runs
//! for *every* live, so the walk from it — bounded by the live entries — is the always-loaded
//! set. There is no owner-unknown concession: every indirect-table target has a modelled taking
//! site, and [`Analysis::read`] refuses to split a module with any residue rather than guess.
//!
//! The reloc/linking sections are analysis input only: they are stripped from the core
//! the browser loads. Splitting happens in process ([`crate::split_emit`]), dev and
//! release alike — no external tool.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Range;

use anyhow::{bail, Context as _, Result};

/// The live chunks' map, published in `manifest.json`: live entry → its chunk
/// filenames (empty = lives entirely in the primary).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ChunkManifest {
    pub live: BTreeMap<String, Vec<String>>,
}

/// Split `core` by reachability. `live` is the guest-table order (page, head, then
/// the real live); entry `i` seeds from its two declared exports. Page/head seed the
/// exclusivity check but are never cut (they are the document — always loaded); only
/// real live get chunks.
///
/// The surgery is in process ([`crate::split_emit`]) — no external `wasm-split`. Dev and
/// prod both split, so dev's per-live reload gate has real chunks to diff.
pub fn split_core(core: &[u8], live: &[String]) -> Result<(Vec<u8>, Vec<(String, Vec<u8>)>)> {
    let assigned = plan(core, live)?;
    let stripped = strip_link_sections(core)?;
    split_in_process(&stripped, live, &assigned)
}

/// Split the (link-section-stripped) core into a primary module plus one lazy-loaded
/// chunk per live that owns exclusive code — in process with `wasm-encoder`, so the build
/// needs no external `wasm-split` binary (toolchain law: Binaryen enters as a crate, never
/// a machine install).
///
/// Load-before-mount is the one contract shared with `runtime.js`: the primary
/// instantiates standalone with every moved function's table slot trapping until its
/// chunk's active element segment lands, and `runtime.js` awaits `ensureChunks(name)`
/// before mounting a live — so an unlinked slot traps loudly rather than corrupting.
fn split_in_process(
    core: &[u8],
    live: &[String],
    assigned: &[Vec<u32>],
) -> Result<(Vec<u8>, Vec<(String, Vec<u8>)>)> {
    let (primary, chunks) = crate::split_emit::split(core, assigned)?;
    let named = chunks.into_iter().map(|(live_idx, bytes)| (live[live_idx].clone(), bytes)).collect();
    Ok((primary, named))
}

/// Decide each live's function set: reachable from that live's declared entry
/// exports and from no other entry, and not always-loaded. Pure analysis — everything
/// `split_core` does besides the emit ([`crate::split_emit::split`]).
fn plan(core: &[u8], live: &[String]) -> Result<Vec<Vec<u32>>> {
    let module = Analysis::read(core)?;

    let entry_seeds: Vec<Vec<u32>> = (0..live.len())
        .map(|entry| {
            [idyll_schema::live_root_export(entry), idyll_schema::live_make_export(entry)]
                .iter()
                .map(|export| {
                    module.func_exports.get(export.as_str()).copied().with_context(|| {
                        format!(
                            "core module does not export `{export}` for live `{}` — \
                             app built by a `guest!` that predates declared live exports?",
                            live[entry]
                        )
                    })
                })
                .collect::<Result<Vec<u32>>>()
        })
        .collect::<Result<_>>()?;

    let boundary: HashSet<u32> = entry_seeds.iter().flatten().copied().collect();
    let reach: Vec<HashSet<Node>> = entry_seeds
        .iter()
        .map(|seeds| module.closure(seeds.iter().map(|f| Node::Func(*f)), &HashSet::new()))
        .collect();

    // The always-loaded set: what the host-called surface (every export that is not an
    // live entry, plus the start function) reaches without entering a live. Every indirect
    // table target now has a modelled taking site (`Analysis::read` bails otherwise), so there
    // is no owner-unknown concession — the reference graph accounts for the whole table.
    let surface = module
        .func_exports
        .values()
        .copied()
        .chain(module.start)
        .map(Node::Func)
        .collect::<Vec<_>>();
    let always = module.closure(surface, &boundary);

    let mut assigned: Vec<Vec<u32>> = vec![Vec::new(); live.len()];
    let mut candidates: HashSet<u32> = HashSet::new();
    for set in &reach {
        candidates.extend(set.iter().filter_map(|node| match node {
            Node::Func(f) => Some(*f),
            Node::Data(_) => None,
        }));
    }
    for index in candidates {
        if always.contains(&Node::Func(index)) {
            continue;
        }
        let owners: Vec<usize> =
            (0..live.len()).filter(|&i| reach[i].contains(&Node::Func(index))).collect();
        if owners.len() != 1 {
            continue; // shared across entries
        }
        let owner = owners[0];
        if live[owner] == "page" || live[owner] == "head" {
            continue; // the document — always loaded, never a chunk
        }
        assigned[owner].push(index);
    }
    for set in &mut assigned {
        set.sort_unstable(); // the manifest artifact is byte-stable
    }
    Ok(assigned)
}

/// A node in the reference graph: a function, or a data object (a cluster of
/// overlapping defined data symbols — one allocation's bytes).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Node {
    Func(u32),
    Data(u32),
}

/// The module's complete reference graph: direct calls, plus address-taking edges from
/// the relocation sections, with data objects as first-class nodes.
struct Analysis {
    /// Function index → indices it calls directly (`call`/`return_call`). Imports call none.
    calls: Vec<Vec<u32>>,
    /// Function index → what it takes the address of (functions via table-index relocs
    /// or a body `ref.func`; data objects via memory-address relocs).
    refs: Vec<Vec<Node>>,
    /// Data object → what its initialized bytes point at (vtable slots, static fn
    /// pointers, pointers to other objects).
    object_refs: Vec<Vec<Node>>,
    /// Exported functions by export name — live entries are looked up here; the rest
    /// are the host-called surface.
    func_exports: HashMap<String, u32>,
    start: Option<u32>,
}

impl Analysis {
    /// Reachability over all edge kinds, treating each `boundary` function as a leaf:
    /// included, but its outgoing edges are not followed.
    fn closure(
        &self,
        seeds: impl IntoIterator<Item = Node>,
        boundary: &HashSet<u32>,
    ) -> HashSet<Node> {
        let mut seen: HashSet<Node> = seeds.into_iter().collect();
        let mut queue: Vec<Node> = seen.iter().copied().collect();
        while let Some(node) = queue.pop() {
            let push = |next: Node, seen: &mut HashSet<Node>, queue: &mut Vec<Node>| {
                if seen.insert(next) {
                    queue.push(next);
                }
            };
            match node {
                Node::Func(f) => {
                    if boundary.contains(&f) {
                        continue;
                    }
                    if let Some(callees) = self.calls.get(f as usize) {
                        for callee in callees {
                            push(Node::Func(*callee), &mut seen, &mut queue);
                        }
                    }
                    if let Some(refs) = self.refs.get(f as usize) {
                        for next in refs {
                            push(*next, &mut seen, &mut queue);
                        }
                    }
                }
                Node::Data(object) => {
                    for next in &self.object_refs[object as usize] {
                        push(*next, &mut seen, &mut queue);
                    }
                }
            }
        }
        seen
    }

    /// Decode the reference graph. A missed edge *kind* under-approximates reachability
    /// — a function assigned to a chunk a live that needs it does not load, a
    /// browser trap — so an unhandled call operator or relocation kind is a hard error,
    /// not a silent omission.
    fn read(wasm: &[u8]) -> Result<Analysis> {
        use wasmparser::{
            ElementItems, ElementKind, ExternalKind, KnownCustom, Linking, Operator, Parser,
            Payload, RelocationEntry, RelocationType, SymbolInfo, TypeRef, ValType,
        };

        let mut import_count: u32 = 0;
        // (range, calls, ref_funcs, global_gets) — global_gets feed the PIC function-pointer model.
        let mut bodies: Vec<(Range<usize>, Vec<u32>, Vec<u32>, Vec<u32>)> = Vec::new();
        let mut table_targets: HashSet<u32> = HashSet::new();
        let mut func_exports: HashMap<String, u32> = HashMap::new();
        let mut start = None;

        // The active indirect table as `slot -> function`, and the globals that hold slot
        // indices. PIC materialises a function pointer as `global.get` of an immutable `i32`
        // GOT global whose value is a table slot (no `ref.func`/`TABLE_INDEX` reloc at the
        // use site). See the function-pointer model assembled below.
        let mut slot_to_func: HashMap<u64, u32> = HashMap::new();
        let mut import_global_count: u32 = 0;
        let mut defined_global_inits: Vec<Option<i64>> = Vec::new();
        let mut defined_global_mut: Vec<bool> = Vec::new();
        let mut funcref_global = false; // a global whose value IS a funcref (ref.func in init) — unmodelled
        let mut funcref_import = false; // a funcref crossing the host boundary — unmodelled
        let mut tables: Vec<(u64, Option<u64>)> = Vec::new();

        // Relocation offsets are relative to their target section's contents, and the
        // reloc header names the target by **section ordinal counting every section,
        // custom sections included** (the linking convention's section index).
        let mut ordinal: u32 = 0;
        let mut code: Option<(u32, usize)> = None; // (ordinal, contents base)
        let mut data: Option<(u32, usize)> = None;
        let mut segments: Vec<Range<usize>> = Vec::new(); // each segment's payload bytes
        let mut symbols: Vec<SymbolInfo> = Vec::new();
        let mut relocs: Vec<(u32, Vec<RelocationEntry>)> = Vec::new();

        for payload in Parser::new(0).parse_all(wasm) {
            let payload = payload.context("decoding the core module")?;
            match &payload {
                Payload::ImportSection(reader) => {
                    for import in reader.clone().into_imports_with_offsets() {
                        let (_, import) = import.context("decoding an import")?;
                        match import.ty {
                            TypeRef::Func(_) => import_count += 1,
                            TypeRef::Global(gt) => {
                                import_global_count += 1;
                                if matches!(gt.content_type, ValType::Ref(r) if r.is_func_ref()) {
                                    funcref_import = true;
                                }
                            }
                            TypeRef::Table(tt) if tt.element_type.is_func_ref() => {
                                funcref_import = true;
                            }
                            _ => {}
                        }
                    }
                }
                Payload::TableSection(reader) => {
                    for table in reader.clone() {
                        let table = table.context("decoding a table")?;
                        tables.push((table.ty.initial, table.ty.maximum));
                    }
                }
                Payload::GlobalSection(reader) => {
                    for global in reader.clone() {
                        let global = global.context("decoding a global")?;
                        defined_global_mut.push(global.ty.mutable);
                        if matches!(global.ty.content_type, ValType::Ref(r) if r.is_func_ref()) {
                            funcref_global = true;
                        }
                        let mut ops = global.init_expr.get_operators_reader();
                        let value = match ops.read() {
                            Ok(Operator::I32Const { value }) => Some(value as i64),
                            _ => None,
                        };
                        defined_global_inits.push(value);
                    }
                }
                Payload::ElementSection(reader) => {
                    for element in reader.clone() {
                        let element = element.context("decoding an element segment")?;
                        if matches!(element.kind, ElementKind::Declared) {
                            continue;
                        }
                        // Only an *active* segment installs slots at instantiation, so only its
                        // `slot -> func` mapping is meaningful. A passive segment's funcs are
                        // still table targets (`table.init` could place them — but we bail on
                        // `table.init` below), so they land in `table_targets` without a slot and
                        // are caught by the no-untaken-residue check.
                        let base: Option<u64> = match &element.kind {
                            ElementKind::Active { offset_expr, .. } => {
                                let mut ops = offset_expr.get_operators_reader();
                                match ops.read() {
                                    Ok(Operator::I32Const { value }) => Some(value as u64),
                                    Ok(Operator::I64Const { value }) => Some(value as u64),
                                    _ => None,
                                }
                            }
                            _ => None,
                        };
                        let mut slot = base;
                        // A segment names its functions either as bare indices or as
                        // const expressions (`ref.func` — LLVM's form for the indirect
                        // function table).
                        match element.items {
                            ElementItems::Functions(functions) => {
                                for index in functions {
                                    let index = index.context("decoding an element item")?;
                                    table_targets.insert(index);
                                    if let Some(s) = slot {
                                        slot_to_func.insert(s, index);
                                    }
                                    slot = slot.map(|s| s + 1);
                                }
                            }
                            ElementItems::Expressions(_, exprs) => {
                                for expr in exprs {
                                    let expr = expr.context("decoding an element expression")?;
                                    let mut ops = expr.get_operators_reader();
                                    while !ops.eof() {
                                        if let Operator::RefFunc { function_index } =
                                            ops.read().context("decoding an element operator")?
                                        {
                                            table_targets.insert(function_index);
                                            if let Some(s) = slot {
                                                slot_to_func.insert(s, function_index);
                                            }
                                        }
                                    }
                                    slot = slot.map(|s| s + 1);
                                }
                            }
                        }
                    }
                }
                Payload::CodeSectionStart { range, .. } => {
                    code = Some((ordinal, range.start));
                }
                Payload::CodeSectionEntry(body) => {
                    let mut calls = Vec::new();
                    let mut ref_funcs = Vec::new();
                    let mut global_gets = Vec::new();
                    let mut reader =
                        body.get_operators_reader().context("reading a function body")?;
                    while !reader.eof() {
                        match reader.read().context("decoding an operator")? {
                            Operator::Call { function_index }
                            | Operator::ReturnCall { function_index } => {
                                calls.push(function_index)
                            }
                            Operator::RefFunc { function_index } => {
                                ref_funcs.push(function_index)
                            }
                            // A `global.get` is a taking site when the global holds a table slot
                            // (PIC function pointer) — resolved against the globals below.
                            Operator::GlobalGet { global_index } => global_gets.push(global_index),
                            Operator::CallIndirect { .. } | Operator::ReturnCallIndirect { .. } => {
                            }
                            Operator::CallRef { .. } | Operator::ReturnCallRef { .. } => bail!(
                                "core module uses call_ref — the splitter cannot see its \
                                 targets and would assign code that traps"
                            ),
                            // The table index space must be a set of taken pointers, not a
                            // computed one: any op that reads or mutates the table at a value the
                            // reference graph cannot follow would let a `call_indirect` reach a
                            // deferred slot with no visible owner — a trap. Refuse rather than
                            // split unsoundly.
                            Operator::TableGet { .. }
                            | Operator::TableSet { .. }
                            | Operator::TableInit { .. }
                            | Operator::TableCopy { .. }
                            | Operator::TableFill { .. }
                            | Operator::TableGrow { .. } => bail!(
                                "core module manipulates the function table at runtime \
                                 (table.get/set/init/copy/fill/grow) — the splitter cannot \
                                 prove which slots a live reaches and would risk a trap"
                            ),
                            _ => {}
                        }
                    }
                    bodies.push((body.range(), calls, ref_funcs, global_gets));
                }
                Payload::DataSection(reader) => {
                    data = Some((ordinal, reader.range().start));
                    for segment in reader.clone() {
                        let segment = segment.context("decoding a data segment")?;
                        let end = segment.range.end;
                        segments.push(end - segment.data.len()..end);
                    }
                }
                Payload::ExportSection(reader) => {
                    for export in reader.clone() {
                        let export = export.context("decoding an export")?;
                        if matches!(export.kind, ExternalKind::Func) {
                            func_exports.insert(export.name.to_string(), export.index);
                        }
                    }
                }
                Payload::StartSection { func, .. } => start = Some(*func),
                Payload::CustomSection(section) => match section.as_known() {
                    KnownCustom::Linking(reader) => {
                        for subsection in reader {
                            let subsection = subsection.context("decoding the linking section")?;
                            if let Linking::SymbolTable(map) = subsection {
                                for symbol in map {
                                    symbols.push(symbol.context("decoding a symbol")?);
                                }
                            }
                        }
                    }
                    KnownCustom::Reloc(reader) => {
                        let target = reader.section_index();
                        let mut entries = Vec::new();
                        for entry in reader.entries() {
                            entries.push(entry.context("decoding a relocation")?);
                        }
                        relocs.push((target, entries));
                    }
                    _ => {}
                },
                _ => {}
            }
            if !matches!(
                payload,
                Payload::Version { .. } | Payload::CodeSectionEntry(_) | Payload::End(_)
            ) {
                ordinal += 1;
            }
        }

        // Fail-closed on every funcref shape outside the modelled inventory (`ref.func`,
        // `TABLE_INDEX` reloc, GOT `global.get`). Each is absent from today's builds; these catch
        // toolchain drift as a build error, never a browser trap. Checked before the reloc gate —
        // they are module-shape facts, independent of relocations.
        if funcref_global {
            bail!(
                "core module has a funcref-typed global — an unmodelled function-pointer source; \
                 refusing to split"
            );
        }
        if funcref_import {
            bail!(
                "core module imports or exports a funcref across the host boundary — a \
                 function pointer the reference graph cannot follow; refusing to split"
            );
        }
        match tables.as_slice() {
            [(min, Some(max))] if min == max => {}
            _ => bail!(
                "core module has {} table(s) or a growable table — the splitter models exactly \
                 one fixed indirect table; refusing to split",
                tables.len()
            ),
        }

        let (code_ordinal, code_base) = code.context("core module has no code section")?;
        if symbols.is_empty() || !relocs.iter().any(|(t, _)| *t == code_ordinal) {
            bail!(
                "core module carries no linking/reloc.CODE sections — live splitting \
                 needs the app linked with `--emit-relocs` (idyll-serve sets it; is a \
                 stale build being served?)"
            );
        }

        // Data objects: defined data symbols as byte intervals, overlapping intervals
        // merged into one node. Aliased symbols (ICF, zero-sized markers at an object's
        // start) must not split an object, or an edge in via one name and out via the
        // other would silently disconnect — and a disconnected graph under-assigns
        // reachability, which here means a trap, not just bytes.
        let mut intervals: Vec<(Range<usize>, u32)> = Vec::new(); // (absolute bytes, symbol)
        for (id, symbol) in symbols.iter().enumerate() {
            if let SymbolInfo::Data { symbol: Some(def), .. } = symbol {
                let segment = segments.get(def.index as usize).with_context(|| {
                    format!("data symbol {id} names segment {} of {}", def.index, segments.len())
                })?;
                let at = segment.start + def.offset as usize;
                intervals.push((at..at + def.size as usize, id as u32));
            }
        }
        intervals.sort_by_key(|(range, _)| (range.start, range.end));
        let mut objects: Vec<Range<usize>> = Vec::new();
        let mut object_of_symbol: HashMap<u32, u32> = HashMap::new();
        for (range, symbol) in intervals {
            match objects.last_mut() {
                Some(last) if range.start < last.end || range.start == last.start => {
                    last.end = last.end.max(range.end);
                }
                _ => objects.push(range.clone()),
            }
            object_of_symbol.insert(symbol, objects.len() as u32 - 1);
        }
        let object_at = |site: usize| -> Option<u32> {
            let i = objects.partition_point(|range| range.start <= site);
            (i > 0 && site < objects[i - 1].end).then(|| i as u32 - 1)
        };

        let body_at = |site: usize| -> Result<u32> {
            let i = bodies.partition_point(|(range, ..)| range.start <= site);
            if i == 0 || site >= bodies[i - 1].0.end {
                bail!("relocation site {site} is outside every function body");
            }
            Ok(import_count + i as u32 - 1)
        };

        let mut calls: Vec<Vec<u32>> = vec![Vec::new(); import_count as usize];
        let mut refs: Vec<Vec<Node>> = vec![Vec::new(); import_count as usize];
        let mut taken: HashSet<u32> = HashSet::new();
        for (_, body_calls, ref_funcs, _) in &bodies {
            calls.push(body_calls.clone());
            // A body `ref.func` is a taking site like any relocation: attributed to the
            // function it appears in, not a blanket concession.
            taken.extend(ref_funcs.iter().copied());
            refs.push(ref_funcs.iter().map(|f| Node::Func(*f)).collect());
        }
        let mut object_refs: Vec<Vec<Node>> = vec![Vec::new(); objects.len()];

        use RelocationType::*;
        let func_symbol = |entry: &RelocationEntry| -> Result<u32> {
            match symbols.get(entry.index as usize) {
                Some(SymbolInfo::Func { index, .. }) => Ok(*index),
                other => bail!("table-index relocation names a non-function symbol: {other:?}"),
            }
        };
        let data_symbol = |entry: &RelocationEntry| -> Result<u32> {
            let symbol = symbols.get(entry.index as usize);
            let Some(SymbolInfo::Data { name, .. }) = symbol else {
                bail!("memory-address relocation names a non-data symbol: {symbol:?}");
            };
            object_of_symbol.get(&entry.index).copied().with_context(|| {
                format!("memory-address relocation names undefined data symbol `{name}`")
            })
        };

        for (target, entries) in &relocs {
            if *target == code_ordinal {
                for entry in entries {
                    let holder = || body_at(code_base + entry.offset as usize);
                    match entry.ty {
                        TableIndexSleb | TableIndexSleb64 | TableIndexI32 | TableIndexI64
                        | TableIndexRelSleb | TableIndexRelSleb64 => {
                            let index = func_symbol(entry)?;
                            taken.insert(index);
                            refs[holder()? as usize].push(Node::Func(index));
                        }
                        MemoryAddrLeb | MemoryAddrSleb | MemoryAddrI32 | MemoryAddrLeb64
                        | MemoryAddrSleb64 | MemoryAddrI64 | MemoryAddrRelSleb
                        | MemoryAddrRelSleb64 | MemoryAddrLocrelI32 | MemoryAddrTlsSleb
                        | MemoryAddrTlsSleb64 => {
                            let object = data_symbol(entry)?;
                            refs[holder()? as usize].push(Node::Data(object));
                        }
                        // A direct call's operand — the operator scan already has the
                        // edge; the relocation instead **validates the decoder**: the
                        // immediate at the site must be the symbol's final function
                        // index, or the offset base / index space is misread.
                        FunctionIndexLeb => {
                            let site = code_base + entry.offset as usize;
                            let (value, _) = leb128(wasm, site)?;
                            let index = func_symbol(entry)?;
                            if value != u64::from(index) {
                                bail!(
                                    "reloc self-check failed: site {site} holds {value}, \
                                     symbol says {index} — reloc sections do not match \
                                     this module"
                                );
                            }
                        }
                        TypeIndexLeb | GlobalIndexLeb | GlobalIndexI32 | TableNumberLeb
                        | EventIndexLeb | FunctionIndexI32 => {}
                        FunctionOffsetI32 | FunctionOffsetI64 | SectionOffsetI32 => bail!(
                            "unexpected {:?} relocation in the code section",
                            entry.ty
                        ),
                    }
                }
            } else if let Some((_, data_base)) = data.filter(|(ordinal, _)| ordinal == target) {
                for entry in entries {
                    let site = data_base + entry.offset as usize;
                    let holder = object_at(site).with_context(|| {
                        format!("data relocation site {site} lies outside every data symbol")
                    })?;
                    match entry.ty {
                        TableIndexI32 | TableIndexI64 => {
                            let index = func_symbol(entry)?;
                            taken.insert(index);
                            object_refs[holder as usize].push(Node::Func(index));
                        }
                        MemoryAddrI32 | MemoryAddrI64 => {
                            let object = data_symbol(entry)?;
                            object_refs[holder as usize].push(Node::Data(object));
                        }
                        other => bail!("unexpected {other:?} relocation in the data section"),
                    }
                }
            }
        }

        // The PIC function-pointer model — structural, no symbol name relied on. A function
        // pointer that carries no `TABLE_INDEX` reloc and no `ref.func` is materialised as a
        // `global.get` of an immutable `i32` global whose value is a table slot (the GOT). We do
        // not trust the linker's `GOT.func.*` naming (it lives only in the strippable name
        // section, and mangling is unstable); instead, *any* immutable `i32` global whose value
        // lands on an active table slot is treated as a function-pointer site. That is a
        // **superset** of the real sites — a data global whose address coincides with a slot only
        // over-attributes its "target", which keeps a function loaded in more places, never
        // fewer, so it cannot cause a trap.
        let candidate_target = |global_index: u32| -> Option<u32> {
            let defined = (global_index as usize).checked_sub(import_global_count as usize)?;
            if *defined_global_mut.get(defined)? {
                return None; // a mutable global's runtime value is not its init
            }
            let value = (*defined_global_inits.get(defined)?)?;
            slot_to_func.get(&(value as u64)).copied()
        };
        for (body_index, (_, _, _, global_gets)) in bodies.iter().enumerate() {
            let holder = (import_count as usize + body_index) as usize;
            for &global_index in global_gets {
                if let Some(target) = candidate_target(global_index) {
                    taken.insert(target);
                    refs[holder].push(Node::Func(target));
                }
            }
        }

        // With every materialisation form modelled and the guards clear, a table target with no
        // taking site is genuinely never dispatched — but we cannot *prove* it dead, and the old
        // "keep it always-loaded" concession is the incoherent middle: it distrusts the
        // enumeration for this function while the owner attribution trusts it for every other.
        // Resolve it the one sound way: demand the enumeration be complete. A residue is either a
        // dead entry we cannot certify or an unmodelled form the guards missed — refuse to split.
        let untaken = table_targets.difference(&taken).count();
        if untaken != 0 {
            bail!(
                "{untaken} of {} indirect-table targets have no visible taking site \
                 (reloc, ref.func, or GOT global.get) — an unmodelled function-pointer form; \
                 refusing to split rather than risk a trap",
                table_targets.len()
            );
        }

        Ok(Analysis { calls, refs, object_refs, func_exports, start })
    }
}

/// The module minus its `linking` and `reloc.*` custom sections — analysis input the
/// browser never needs, and whose offsets `wasm-split` would invalidate anyway.
fn strip_link_sections(wasm: &[u8]) -> Result<Vec<u8>> {
    if wasm.len() < 8 || &wasm[0..4] != b"\0asm" {
        bail!("not a wasm module");
    }
    let mut out = wasm[..8].to_vec();
    let mut at = 8;
    while at < wasm.len() {
        let section_start = at;
        let id = wasm[at];
        at += 1;
        let (size, n) = leb128(wasm, at)?;
        at += n;
        let end = at
            .checked_add(size as usize)
            .filter(|&end| end <= wasm.len())
            .ok_or_else(|| anyhow::anyhow!("truncated wasm module: section overruns the file"))?;
        let body = &wasm[at..end];
        at = end;
        let linkage = id == 0
            && matches!(custom_section_name(body), Some(name) if name == "linking" || name.starts_with("reloc."));
        if !linkage {
            out.extend_from_slice(&wasm[section_start..at]);
        }
    }
    Ok(out)
}

fn custom_section_name(body: &[u8]) -> Option<&str> {
    let (len, n) = leb128(body, 0).ok()?;
    std::str::from_utf8(body.get(n..n + len as usize)?).ok()
}

/// Unsigned LEB128 at `at`; returns (value, bytes consumed).
fn leb128(bytes: &[u8], at: usize) -> Result<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0;
    for (i, &b) in bytes[at..].iter().enumerate() {
        // The tenth byte contributes one bit; any higher payload bit OR a
        // continuation past it is an overflowing encoding — refused, never
        // silently truncated into a wrong value (and never a shift overflow).
        if shift == 63 && b & 0xfe != 0 {
            bail!("overlong LEB128 in wasm module");
        }
        value |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok((value, i + 1));
        }
        shift += 7;
        if shift > 63 {
            break;
        }
    }
    bail!("malformed LEB128 in wasm module")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The audit's executed repro: ten continuation bytes then a terminator used to
    /// shift-overflow (debug) or return a silently wrong value (release). Overlong
    /// encodings refuse, in both profiles.
    #[test]
    fn an_overlong_leb128_is_refused_not_truncated() {
        let overlong = [0x80u8, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01];
        assert!(leb128(&overlong, 0).is_err());
        // The widest valid u64: nine full bytes and a tenth carrying only bit 0.
        let max = [0xffu8, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert_eq!(leb128(&max, 0).unwrap(), (u64::MAX, 10));
    }

    /// The function `(index, name)` pairs from the custom `name` section (subsection 1) —
    /// how these tests read a split result back. Production splitting never needs it
    /// (identity is declared exports), which is why it lives here.
    fn function_names(wasm: &[u8]) -> Result<Vec<(u32, String)>> {
        if wasm.len() < 8 || &wasm[0..4] != b"\0asm" {
            bail!("not a wasm module");
        }
        let mut at = 8;
        while at < wasm.len() {
            let id = wasm[at];
            at += 1;
            let (size, n) = leb128(wasm, at)?;
            at += n;
            let body = &wasm[at..at + size as usize];
            at += size as usize;
            if id != 0 {
                continue;
            }
            let (name_len, n) = leb128(body, 0)?;
            let name_end = n + name_len as usize;
            if &body[n..name_end] != b"name" {
                continue;
            }
            let mut sub_at = name_end;
            while sub_at < body.len() {
                let sub_id = body[sub_at];
                sub_at += 1;
                let (sub_size, n) = leb128(body, sub_at)?;
                sub_at += n;
                let sub = &body[sub_at..sub_at + sub_size as usize];
                sub_at += sub_size as usize;
                if sub_id != 1 {
                    continue;
                }
                let (count, mut p) = leb128(sub, 0)?;
                let mut names = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let (index, n) = leb128(sub, p)?;
                    p += n;
                    let (len, n) = leb128(sub, p)?;
                    p += n;
                    let name = std::str::from_utf8(&sub[p..p + len as usize])
                        .context("a function name is not valid UTF-8")?
                        .to_owned();
                    names.push((index as u32, name));
                    p += len as usize;
                }
                return Ok(names);
            }
        }
        bail!("the core module has no function-name section — a stripped build cannot be split")
    }

    /// Three entries — page plus live `a` and `b` — with every fact a wrong answer
    /// ships as a browser crash:
    ///
    /// - `$a_only` takes a vtable's address (memory-address reloc → data symbol) whose
    ///   slot holds `$vt_method` (table-index reloc in DATA): the method and its callee
    ///   belong to live `a`.
    /// - `$a_only` and `$b_only` both take `$shared_target`'s address: two owners, so
    ///   it stays primary.
    /// - `$untaken` sits in the table with **no** relocation naming it. When `cover` is set,
    ///   `$shared_helper` (which `mount`, an export, reaches) `global.get`s a GOT global whose
    ///   value is `$untaken`'s slot — the PIC function-pointer taking site — so `$untaken` is
    ///   attributed to the always-loaded surface, structurally, with no reliance on the global's
    ///   name. When `cover` is clear, `$untaken` has no taking site at all — the residue the
    ///   splitter must refuse rather than silently keep.
    /// - `$a_only` holds a body `ref.func $body_ref`: a taking site attributed to `a`.
    /// - `$a_root` also calls `$shared_helper`, which `mount` (an export) reaches too:
    ///   always-loaded wins over live reach.
    fn fixture_wat(cover: bool) -> Vec<u8> {
        // The GOT global (immutable `i32` = slot 2 = `$untaken`) and the `global.get` that reads
        // it. Deliberately anonymous — the model keys on the value→slot mapping, never the name.
        let got = if cover { "(global i32 (i32.const 2))" } else { "" };
        let helper = if cover {
            "(func $shared_helper (call $log) (global.get 0) (drop))"
        } else {
            "(func $shared_helper (call $log))"
        };
        wat::parse_str(&format!(
            r#"
            (module
              (import "e" "log" (func $log))
              (table 3 3 funcref)
              (memory 1)
              {got}
              (elem (i32.const 0) $vt_method $shared_target)
              (elem (i32.const 2) funcref (ref.func $untaken))
              (elem declare func $body_ref)
              (data (i32.const 16) "0123456789abcdef")
              (export "mount" (func $mount))
              (export "__idyll_live_root_0" (func $page_root))
              (export "__idyll_live_make_0" (func $page_make))
              (export "__idyll_live_root_1" (func $a_root))
              (export "__idyll_live_make_1" (func $a_make))
              (export "__idyll_live_root_2" (func $b_root))
              (export "__idyll_live_make_2" (func $b_make))
              (func $mount (call $shared_helper))
              (func $page_root)
              (func $page_make)
              (func $a_root (call $a_only) (call $shared_helper))
              (func $a_make)
              (func $b_root (call $b_only))
              (func $b_make)
              {helper}
              (func $a_only (call $a_leaf) (ref.func $body_ref) (drop))
              (func $a_leaf)
              (func $b_only)
              (func $vt_method (call $vt_callee))
              (func $vt_callee)
              (func $untaken (call $untaken_callee))
              (func $untaken_callee)
              (func $shared_target)
              (func $body_ref (call $body_ref_callee))
              (func $body_ref_callee))
            "#,
        ))
        .expect("fixture assembles")
    }

    /// Where things sit in the assembled bytes — the same facts the analysis reads via
    /// `wasmparser`, derived independently so the reloc offsets the tests fabricate
    /// exercise the real offset arithmetic.
    struct Layout {
        code_ordinal: u32,
        code_base: usize,
        data_ordinal: u32,
        data_base: usize,
        bodies: Vec<Range<usize>>,
        segment_payloads: Vec<Range<usize>>,
    }

    fn layout(wasm: &[u8]) -> Layout {
        let mut at = 8;
        let mut ordinal = 0u32;
        let mut out = Layout {
            code_ordinal: 0,
            code_base: 0,
            data_ordinal: 0,
            data_base: 0,
            bodies: Vec::new(),
            segment_payloads: Vec::new(),
        };
        while at < wasm.len() {
            let id = wasm[at];
            let (size, n) = leb128(wasm, at + 1).expect("section size");
            let content = at + 1 + n..at + 1 + n + size as usize;
            match id {
                10 => {
                    out.code_ordinal = ordinal;
                    out.code_base = content.start;
                    let (count, mut p) = leb128(wasm, content.start).expect("body count");
                    for _ in 0..count {
                        let (body_size, m) = leb128(wasm, content.start + p).expect("body size");
                        let start = content.start + p + m;
                        out.bodies.push(start..start + body_size as usize);
                        p += m + body_size as usize;
                    }
                }
                11 => {
                    out.data_ordinal = ordinal;
                    out.data_base = content.start;
                    let (count, mut p) = leb128(wasm, content.start).expect("segment count");
                    for _ in 0..count {
                        let (_, m) = leb128(wasm, content.start + p).expect("segment flags");
                        p += m;
                        while wasm[content.start + p] != 0x0b {
                            p += 1; // active offset expr — no 0x0b byte inside i32.const 16
                        }
                        p += 1;
                        let (len, m) = leb128(wasm, content.start + p).expect("payload len");
                        p += m;
                        out.segment_payloads
                            .push(content.start + p..content.start + p + len as usize);
                        p += len as usize;
                    }
                }
                _ => {}
            }
            ordinal += 1;
            at = content.end;
        }
        out
    }

    fn uleb(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    fn custom(name: &str, body: &[u8]) -> Vec<u8> {
        let mut content = uleb(name.len() as u64);
        content.extend(name.as_bytes());
        content.extend(body);
        let mut out = vec![0u8];
        out.extend(uleb(content.len() as u64));
        out.extend(content);
        out
    }

    fn func_sym(index: u32, name: &str) -> Vec<u8> {
        let mut out = vec![0u8, 0u8]; // SYMTAB_FUNCTION, flags: defined
        out.extend(uleb(u64::from(index)));
        out.extend(uleb(name.len() as u64));
        out.extend(name.as_bytes());
        out
    }

    fn data_sym(name: &str, segment: u32, offset: u32, size: u32) -> Vec<u8> {
        let mut out = vec![1u8, 0u8]; // SYMTAB_DATA, flags: defined
        out.extend(uleb(name.len() as u64));
        out.extend(name.as_bytes());
        out.extend(uleb(u64::from(segment)));
        out.extend(uleb(u64::from(offset)));
        out.extend(uleb(u64::from(size)));
        out
    }

    fn linking(symbols: &[Vec<u8>]) -> Vec<u8> {
        let mut payload = uleb(symbols.len() as u64);
        for symbol in symbols {
            payload.extend(symbol);
        }
        let mut body = uleb(2); // version
        body.push(8); // WASM_SYMBOL_TABLE
        body.extend(uleb(payload.len() as u64));
        body.extend(payload);
        custom("linking", &body)
    }

    /// (type, section-relative offset, symbol index, addend if the type carries one)
    struct Reloc(u8, u32, u32, Option<i64>);

    fn reloc_section(name: &str, target: u32, entries: &[Reloc]) -> Vec<u8> {
        let mut body = uleb(u64::from(target));
        body.extend(uleb(entries.len() as u64));
        for Reloc(ty, offset, index, addend) in entries {
            body.push(*ty);
            body.extend(uleb(u64::from(*offset)));
            body.extend(uleb(u64::from(*index)));
            if let Some(addend) = addend {
                assert_eq!(*addend, 0, "tests only fabricate zero addends");
                body.push(0);
            }
        }
        custom(name, &body)
    }

    const TABLE_INDEX_SLEB: u8 = 1;
    const TABLE_INDEX_I32: u8 = 2;
    const MEMORY_ADDR_SLEB: u8 = 4;
    const FUNCTION_INDEX_LEB: u8 = 0;

    fn index_of(names: &[(u32, String)], want: &str) -> u32 {
        names.iter().find(|(_, n)| n == want).map(|(i, _)| *i).unwrap_or_else(|| panic!("{want}"))
    }

    fn live() -> Vec<String> {
        ["page", "a", "b"].map(String::from).to_vec()
    }

    /// The fixture with its linking + reloc sections attached. `vtable_sym_for_code`
    /// picks which data symbol the code-side memory-address reloc names — the aliasing
    /// test points it at a zero-sized alias of the same bytes.
    fn fixture(vtable_sym_for_code: u32) -> Vec<u8> {
        fixture_cover(vtable_sym_for_code, true)
    }

    fn fixture_cover(vtable_sym_for_code: u32, cover: bool) -> Vec<u8> {
        let mut wasm = fixture_wat(cover);
        let names = function_names(&wasm).expect("names");
        let l = layout(&wasm);
        let body = |name: &str| l.bodies[(index_of(&names, name) - 1) as usize].clone();

        // Symbols: 0 = $vt_method, 1 = $shared_target, 2 = the vtable (segment bytes
        // [4, 12)), 3 = a zero-sized alias at the vtable's start.
        let symbols = [
            func_sym(index_of(&names, "vt_method"), "vt_method"),
            func_sym(index_of(&names, "shared_target"), "shared_target"),
            data_sym("vtable", 0, 4, 8),
            data_sym("vtable_alias", 0, 4, 0),
        ];
        wasm.extend(linking(&symbols));

        let a_only = body("a_only").start - l.code_base;
        let b_only = body("b_only").start - l.code_base;
        wasm.extend(reloc_section(
            "reloc.CODE",
            l.code_ordinal,
            &[
                Reloc(MEMORY_ADDR_SLEB, a_only as u32, vtable_sym_for_code, Some(0)),
                Reloc(TABLE_INDEX_SLEB, a_only as u32, 1, None),
                Reloc(TABLE_INDEX_SLEB, b_only as u32, 1, None),
            ],
        ));
        let slot = (l.segment_payloads[0].start + 4 - l.data_base) as u32;
        wasm.extend(reloc_section(
            "reloc.DATA",
            l.data_ordinal,
            &[Reloc(TABLE_INDEX_I32, slot, 0, None)],
        ));
        wasm
    }

    fn assigned_names(wasm: &[u8]) -> Vec<Vec<String>> {
        let names = function_names(wasm).expect("names");
        let name_of: HashMap<u32, &str> = names.iter().map(|(i, n)| (*i, n.as_str())).collect();
        plan(wasm, &live())
            .expect("plan")
            .into_iter()
            .map(|set| set.into_iter().map(|i| name_of[&i].to_string()).collect())
            .collect()
    }

    #[test]
    fn islands_own_their_exclusive_closures() {
        let assigned = assigned_names(&fixture(2));
        assert_eq!(assigned[0], Vec::<String>::new(), "the page is the document");
        assert_eq!(
            assigned[1],
            ["a_only", "a_leaf", "vt_method", "vt_callee", "body_ref", "body_ref_callee"]
                .map(String::from)
                .to_vec(),
            "live a: its call chain, the vtable's methods, and the body ref.func chain"
        );
        assert_eq!(assigned[2], vec!["b_only".to_string()]);
    }

    #[test]
    fn shared_and_got_reached_from_always_stay_primary() {
        // `$shared_target` has two owners; `$untaken` is materialised only through a GOT
        // `global.get` in `$shared_helper`, which `mount` (an export) reaches — so the
        // function-pointer model attributes it to the always-loaded surface. Both stay primary,
        // now for a modelled reason, not a blanket concession.
        let all: Vec<String> = assigned_names(&fixture(2)).into_iter().flatten().collect();
        for kept in ["shared_target", "shared_helper", "untaken", "untaken_callee", "mount"] {
            assert!(!all.contains(&kept.to_string()), "{kept} must stay primary");
        }
    }

    #[test]
    fn a_table_target_with_no_taking_site_is_refused() {
        // Same fixture, but `$untaken`'s GOT global and its `global.get` are gone: nothing
        // materialises its pointer. The old code kept it always-loaded; completeness demands we
        // refuse rather than trust an enumeration we cannot certify.
        let err = plan(&fixture_cover(2, false), &live()).unwrap_err().to_string();
        assert!(err.contains("no visible taking site"), "{err}");
    }

    #[test]
    fn runtime_table_manipulation_is_refused() {
        // `table.get` (and its siblings) could hand a `call_indirect` a slot the reference graph
        // never followed — refuse rather than split unsoundly.
        let wat = wat::parse_str(
            r#"(module (table 1 1 funcref)
                 (func $f (i32.const 0) (table.get 0) (drop))
                 (export "mount" (func $f)))"#,
        )
        .expect("assembles");
        let err = Analysis::read(&wat).err().expect("refused").to_string();
        assert!(err.contains("manipulates the function table"), "{err}");
    }

    #[test]
    fn a_growable_table_is_refused() {
        // No maximum ⇒ the table may grow at runtime into slots the analysis never saw.
        let wat = wat::parse_str(
            r#"(module (table 1 funcref) (func $f) (export "mount" (func $f)))"#,
        )
        .expect("assembles");
        let err = Analysis::read(&wat).err().expect("refused").to_string();
        assert!(err.contains("growable table"), "{err}");
    }

    #[test]
    fn a_funcref_global_is_refused() {
        // A funcref-typed global holds a function pointer through a form the model does not
        // follow (a `ref.func` in the init) — refuse.
        let wat = wat::parse_str(
            r#"(module (table 1 1 funcref)
                 (elem declare func $f)
                 (global funcref (ref.func $f))
                 (func $f) (export "mount" (func $f)))"#,
        )
        .expect("assembles");
        let err = Analysis::read(&wat).err().expect("refused").to_string();
        assert!(err.contains("funcref-typed global"), "{err}");
    }

    #[test]
    fn aliased_data_symbols_are_one_object() {
        // The code-side reloc names the zero-sized alias; the vtable slot's reloc site
        // lies in the full-sized symbol. If aliases split the object, live a loses
        // the vtable → method edge and `vt_method` silently stays primary.
        let assigned = assigned_names(&fixture(3));
        assert!(assigned[1].contains(&"vt_method".to_string()));
    }

    #[test]
    fn missing_relocs_is_a_hard_error() {
        let err = plan(&fixture_wat(true), &live()).unwrap_err().to_string();
        assert!(err.contains("--emit-relocs"), "{err}");
    }

    #[test]
    fn missing_island_export_is_a_hard_error() {
        let mut live = live();
        live.push("phantom".into());
        let err = plan(&fixture(2), &live).unwrap_err().to_string();
        assert!(err.contains("__idyll_live_root_3"), "{err}");
    }

    #[test]
    fn reloc_self_check_rejects_mismatched_sections() {
        // A FunctionIndexLeb whose site (a body's locals count, value 0) cannot equal
        // the named symbol's index: the decoder must refuse the sections wholesale.
        let mut wasm = fixture_wat(true);
        let names = function_names(&wasm).expect("names");
        let l = layout(&wasm);
        wasm.extend(linking(&[func_sym(index_of(&names, "vt_method"), "vt_method")]));
        let site = (l.bodies[0].start - l.code_base) as u32;
        wasm.extend(reloc_section(
            "reloc.CODE",
            l.code_ordinal,
            &[Reloc(FUNCTION_INDEX_LEB, site, 0, None)],
        ));
        let err = plan(&wasm, &live()).unwrap_err().to_string();
        assert!(err.contains("self-check"), "{err}");
    }

    #[test]
    fn stripping_removes_link_sections_and_nothing_else() {
        let full = fixture(2);
        let stripped = strip_link_sections(&full).expect("strip");
        assert!(stripped.len() < full.len());
        let err = Analysis::read(&stripped).err().expect("stripped module refuses").to_string();
        assert!(err.contains("--emit-relocs"), "stripped module has no reloc sections");
        assert_eq!(
            function_names(&stripped).expect("names survive"),
            function_names(&full).expect("names")
        );
    }

    #[test]
    fn split_in_process_partitions_into_valid_primary_and_chunks() {
        // The in-process splitter (no external `wasm-split`) moves each live's exclusive
        // functions into a chunk; the primary and every chunk must validate.
        let full = fixture(2);
        let (primary, chunks) =
            split_core(&full, &live()).expect("in-process split needs no external tool");
        wasmparser::Validator::new()
            .validate_all(&primary)
            .expect("the primary validates");
        // Lives `a` and `b` own exclusive code; the document `page` does not.
        let names: Vec<&str> = chunks.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["a", "b"], "one chunk per live with exclusive functions");
        for (name, bytes) in &chunks {
            wasmparser::Validator::new()
                .validate_all(bytes)
                .unwrap_or_else(|e| panic!("chunk {name} validates: {e}"));
        }
    }
}
