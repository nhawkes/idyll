//! The live-chunk split's **surgery**: given the core and each live's assigned (deferred)
//! function set from [`crate::chunks::plan`], emit a primary module plus one lazy chunk per
//! live, in process with `wasm-encoder` — no external `wasm-split`.
//!
//! The mechanism is Binaryen's (module-splitting), reduced to what idyll's core needs:
//! deferred functions stay at their original indices in the primary as `unreachable`
//! placeholders, so element segments referencing them need no repointing — a chunk's active
//! element segment overwrites the slot on instantiation. Only **direct calls** from a primary
//! function to a deferred one (a live entry calling its exclusive code) are rewritten to
//! `call_indirect` through the deferred function's table slot; a slot is appended when the
//! function is not already a table target. Chunk function bodies are re-encoded with every
//! function reference remapped to the chunk's own index space (imports of the primary
//! functions/memory/table/globals it uses, under module name `""`, then its own functions).
//!
//! Load-before-mount is the one contract shared with `runtime.js`: the primary instantiates
//! standalone with each placeholder slot trapping until its chunk lands, and `runtime.js`
//! awaits `ensureChunks(name)` before mounting a live.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{bail, Context as _, Result};
use wasm_encoder::reencode::{Reencode, RoundtripReencoder};
use wasm_encoder::{
    CodeSection, ConstExpr, ElementSection, Elements, EntityType, ExportKind, ExportSection,
    Function, ImportSection, Instruction, Module, RawSection,
};
use wasmparser::{Operator, Parser, Payload};

/// A parse of the core into the pieces the surgery moves. Sections that pass through
/// unchanged (types, imports, memory, globals, data) are kept as raw bytes.
struct Core<'a> {
    types: Option<&'a [u8]>,
    imports: Option<&'a [u8]>,
    memory: Option<&'a [u8]>,
    globals: Option<&'a [u8]>,
    data: Option<&'a [u8]>,
    /// The data-count section's count, when the core carries one (`memory.init`/
    /// `data.drop` in code demand it) — re-emitted so the primary stays valid.
    data_count: Option<u32>,
    start: Option<u32>,
    /// Imported function count — the offset before defined functions in the index space.
    func_imports: u32,
    /// Type index per **defined** function, in order.
    func_types: Vec<u32>,
    /// Function bodies (locals + operators), one per defined function.
    bodies: Vec<wasmparser::FunctionBody<'a>>,
    /// The single table's element type and minimum — grown when slots are appended.
    table: Option<TableDesc>,
    /// Active element segments: `(base slot, function indices)`. Declared/passive kept raw.
    active_elems: Vec<(u32, Vec<u32>)>,
    raw_elems: Vec<&'a [u8]>,
    /// Every existing export, to keep in the primary.
    exports: Vec<(String, ExportKind, u32)>,
    /// Every global's type, in index order (imported globals first) — chunks re-import them.
    global_types: Vec<wasm_encoder::GlobalType>,
    memory_count: u32,
}

impl<'a> Core<'a> {
    /// Defined function `f`'s slot in the defined-only `bodies`/`func_types` vectors — the
    /// index-space shift past the imported functions. `f` must be a defined function.
    fn defined(&self, f: u32) -> usize {
        (f - self.func_imports) as usize
    }
    fn body(&self, f: u32) -> &wasmparser::FunctionBody<'a> {
        &self.bodies[self.defined(f)]
    }
    fn func_type(&self, f: u32) -> u32 {
        self.func_types[self.defined(f)]
    }
}

struct TableDesc {
    element_type: wasm_encoder::RefType,
    minimum: u64,
    maximum: Option<u64>,
}

fn ref_type(rt: wasmparser::RefType) -> wasm_encoder::RefType {
    RoundtripReencoder.ref_type(rt).expect("core ref type re-encodes")
}

fn global_type(gt: wasmparser::GlobalType) -> wasm_encoder::GlobalType {
    wasm_encoder::GlobalType {
        val_type: RoundtripReencoder.val_type(gt.content_type).expect("global val type"),
        mutable: gt.mutable,
        shared: gt.shared,
    }
}

fn parse_core(wasm: &[u8]) -> Result<Core<'_>> {
    let mut c = Core {
        types: None,
        imports: None,
        memory: None,
        globals: None,
        data: None,
        data_count: None,
        start: None,
        func_imports: 0,
        func_types: Vec::new(),
        bodies: Vec::new(),
        table: None,
        active_elems: Vec::new(),
        raw_elems: Vec::new(),
        exports: Vec::new(),
        global_types: Vec::new(),
        memory_count: 0,
    };
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.context("decoding the core module")? {
            Payload::TypeSection(r) => c.types = Some(&wasm[r.range()]),
            Payload::ImportSection(r) => {
                c.imports = Some(&wasm[r.range()]);
                for import in r.clone().into_imports_with_offsets() {
                    let (_, import) = import.context("decoding an import")?;
                    match import.ty {
                        wasmparser::TypeRef::Func(_) => c.func_imports += 1,
                        wasmparser::TypeRef::Global(gt) => c.global_types.push(global_type(gt)),
                        wasmparser::TypeRef::Memory(_) => c.memory_count += 1,
                        _ => {}
                    }
                }
            }
            Payload::FunctionSection(r) => {
                for ty in r {
                    c.func_types.push(ty.context("decoding a function type index")?);
                }
            }
            Payload::TableSection(r) => {
                for (i, table) in r.into_iter().enumerate() {
                    let table = table.context("decoding a table")?;
                    if i == 0 {
                        c.table = Some(TableDesc {
                            element_type: ref_type(table.ty.element_type),
                            minimum: table.ty.initial,
                            maximum: table.ty.maximum,
                        });
                    } else {
                        bail!("core has more than one table — the splitter assumes table 0 only");
                    }
                }
            }
            Payload::MemorySection(r) => {
                c.memory = Some(&wasm[r.range()]);
                c.memory_count += r.count();
            }
            Payload::GlobalSection(r) => {
                c.globals = Some(&wasm[r.range()]);
                for g in r.clone() {
                    c.global_types.push(global_type(g.context("decoding a global")?.ty));
                }
            }
            Payload::ExportSection(r) => {
                for export in r {
                    let export = export.context("decoding an export")?;
                    let kind = match export.kind {
                        wasmparser::ExternalKind::Func => ExportKind::Func,
                        wasmparser::ExternalKind::Table => ExportKind::Table,
                        wasmparser::ExternalKind::Memory => ExportKind::Memory,
                        wasmparser::ExternalKind::Global => ExportKind::Global,
                        wasmparser::ExternalKind::Tag => ExportKind::Tag,
                        other => bail!("unsupported export kind {other:?} in the core"),
                    };
                    c.exports.push((export.name.to_string(), kind, export.index));
                }
            }
            Payload::StartSection { func, .. } => c.start = Some(func),
            Payload::ElementSection(r) => {
                for element in r {
                    let element = element.context("decoding an element segment")?;
                    match &element.kind {
                        wasmparser::ElementKind::Active { offset_expr, .. } => {
                            let base = const_offset(offset_expr)
                                .context("active element segment has a non-const offset")?;
                            let mut funcs = Vec::new();
                            match element.items {
                                wasmparser::ElementItems::Functions(fs) => {
                                    for f in fs {
                                        funcs.push(f.context("decoding an element function")?);
                                    }
                                }
                                wasmparser::ElementItems::Expressions(_, exprs) => {
                                    for expr in exprs {
                                        let expr = expr.context("element expr")?;
                                        funcs.push(ref_func_of(&expr).context(
                                            "active element expression is not a ref.func",
                                        )?);
                                    }
                                }
                            }
                            c.active_elems.push((base, funcs));
                        }
                        _ => c.raw_elems.push(&wasm[element.range]),
                    }
                }
            }
            Payload::CodeSectionEntry(body) => c.bodies.push(body),
            Payload::DataCountSection { count, .. } => c.data_count = Some(count),
            Payload::DataSection(r) => c.data = Some(&wasm[r.range()]),
            _ => {}
        }
    }
    Ok(c)
}

fn const_offset(expr: &wasmparser::ConstExpr) -> Option<u32> {
    let mut ops = expr.get_operators_reader();
    match ops.read().ok()? {
        Operator::I32Const { value } => Some(value as u32),
        _ => None,
    }
}

fn ref_func_of(expr: &wasmparser::ConstExpr) -> Option<u32> {
    let mut ops = expr.get_operators_reader();
    match ops.read().ok()? {
        Operator::RefFunc { function_index } => Some(function_index),
        _ => None,
    }
}

/// Scan a function body's operators, collecting whatever `pick` extracts from each.
fn scan_body<T>(
    body: &wasmparser::FunctionBody,
    mut pick: impl FnMut(&Operator) -> Option<T>,
) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let mut reader = body.get_operators_reader().context("reading a function body")?;
    while !reader.eof() {
        if let Some(v) = pick(&reader.read().context("decoding an operator")?) {
            out.push(v);
        }
    }
    Ok(out)
}

/// The direct-call targets and `ref.func` targets of one function body — the edges the
/// partition follows to remap or rewrite.
fn body_refs(body: &wasmparser::FunctionBody) -> Result<Vec<u32>> {
    scan_body(body, |op| match op {
        Operator::Call { function_index }
        | Operator::ReturnCall { function_index }
        | Operator::RefFunc { function_index } => Some(*function_index),
        _ => None,
    })
}

/// Just the `ref.func` targets — which a module must *declare* (via an element segment)
/// before a body may take their reference.
fn ref_func_targets(body: &wasmparser::FunctionBody) -> Result<Vec<u32>> {
    scan_body(body, |op| match op {
        Operator::RefFunc { function_index } => Some(*function_index),
        _ => None,
    })
}

/// Split `core` given each live's deferred function set. Returns the primary plus
/// `(live index, chunk bytes)` for every live that owns exclusive code.
pub fn split(core: &[u8], assigned: &[Vec<u32>]) -> Result<(Vec<u8>, Vec<(usize, Vec<u8>)>)> {
    let c = parse_core(core)?;

    // func index -> owning live, for every deferred function.
    let mut owner: HashMap<u32, usize> = HashMap::new();
    for (live, funcs) in assigned.iter().enumerate() {
        for &f in funcs {
            owner.insert(f, live);
        }
    }
    if owner.is_empty() {
        // Nothing to move: round-trip the core as the primary, no chunks.
        let mut module = Module::new();
        RoundtripReencoder
            .parse_core_module(&mut module, Parser::new(0), core)
            .map_err(|e| anyhow::anyhow!("re-encoding the core: {e:?}"))?;
        return Ok((module.finish(), Vec::new()));
    }

    let table = c.table.as_ref().context("splitting needs a table; the core has none")?;

    // Table slots. Existing active segments already give some deferred funcs a slot;
    // deferred funcs a primary function calls directly need one appended.
    let mut slot_of: BTreeMap<u32, u32> = BTreeMap::new();
    for (base, funcs) in &c.active_elems {
        for (i, &f) in funcs.iter().enumerate() {
            if owner.contains_key(&f) {
                slot_of.entry(f).or_insert(base + i as u32);
            }
        }
    }
    let mut next_slot = table.minimum as u32;
    let mut appended: Vec<u32> = Vec::new(); // funcs newly given a slot, in slot order
    let is_deferred = |f: u32| owner.contains_key(&f);
    for (di, body) in c.bodies.iter().enumerate() {
        let f = c.func_imports + di as u32;
        if is_deferred(f) {
            continue; // primary callers only
        }
        for target in body_refs(body)? {
            if is_deferred(target) && !slot_of.contains_key(&target) {
                slot_of.insert(target, next_slot);
                appended.push(target);
                next_slot += 1;
            }
        }
    }

    let primary = emit_primary(core, &c, &owner, &slot_of, &appended, next_slot)?;
    let mut chunks = Vec::new();
    for (live, funcs) in assigned.iter().enumerate() {
        if funcs.is_empty() {
            continue;
        }
        chunks.push((live, emit_chunk(&c, funcs, &owner, &slot_of)?));
    }
    Ok((primary, chunks))
}

/// Names the primary exports for the shared entities a chunk imports under `""`.
fn export_name_func(f: u32) -> String {
    format!("f{f}")
}
const EXPORT_MEM: &str = "__idyll_mem";
const EXPORT_TABLE: &str = "__idyll_table";
fn export_name_global(g: u32) -> String {
    format!("g{g}")
}

fn emit_primary(
    core: &[u8],
    c: &Core,
    owner: &HashMap<u32, usize>,
    slot_of: &BTreeMap<u32, u32>,
    appended: &[u32],
    table_size: u32,
) -> Result<Vec<u8>> {
    let mut module = Module::new();
    if let Some(t) = c.types {
        module.section(&RawSection { id: 1, data: t });
    }
    if let Some(i) = c.imports {
        module.section(&RawSection { id: 2, data: i });
    }
    // Function section: unchanged (deferred funcs keep their slots as placeholders).
    raw_from(core, 3).map(|s| module.section(&s));
    // Table: grown to cover appended slots.
    if let Some(t) = &c.table {
        let mut tables = wasm_encoder::TableSection::new();
        tables.table(wasm_encoder::TableType {
            element_type: t.element_type,
            minimum: table_size.max(t.minimum as u32) as u64,
            maximum: t.maximum.map(|m| m.max(table_size as u64)),
            table64: false,
            shared: false,
        });
        module.section(&tables);
    }
    if let Some(m) = c.memory {
        module.section(&RawSection { id: 5, data: m });
    }
    if let Some(g) = c.globals {
        module.section(&RawSection { id: 6, data: g });
    }

    // Exports: keep existing, add the shared surface chunks import.
    let mut exports = ExportSection::new();
    let mut memory_exported: Option<u32> = None;
    let mut table_exported: Option<u32> = None;
    for (name, kind, index) in &c.exports {
        exports.export(name, *kind, *index);
        match kind {
            ExportKind::Memory => memory_exported = Some(*index),
            ExportKind::Table => table_exported = Some(*index),
            _ => {}
        }
    }
    if memory_exported.is_none() && c.memory_count > 0 {
        exports.export(EXPORT_MEM, ExportKind::Memory, 0);
    }
    if table_exported.is_none() {
        exports.export(EXPORT_TABLE, ExportKind::Table, 0);
    }
    for g in 0..c.global_types.len() as u32 {
        exports.export(&export_name_global(g), ExportKind::Global, g);
    }
    // Every primary function a chunk calls must be exported. Compute that set: any target
    // of a deferred body that is not itself deferred.
    let mut needed_funcs: BTreeSet<u32> = BTreeSet::new();
    for (di, body) in c.bodies.iter().enumerate() {
        let f = c.func_imports + di as u32;
        if owner.contains_key(&f) {
            for target in body_refs(body)? {
                if !owner.contains_key(&target) {
                    needed_funcs.insert(target);
                }
            }
        }
    }
    for f in &needed_funcs {
        exports.export(&export_name_func(*f), ExportKind::Func, *f);
    }
    module.section(&exports);

    if let Some(start) = c.start {
        module.section(&wasm_encoder::StartSection { function_index: start });
    }

    // Element segments: keep existing, append a segment initializing the new slots to their
    // (placeholder) deferred functions, so the slot is a valid funcref until the chunk lands.
    let mut elems = ElementSection::new();
    for (base, funcs) in &c.active_elems {
        elems.active(
            None,
            &ConstExpr::i32_const(*base as i32),
            Elements::Functions(funcs.as_slice().into()),
        );
    }
    for raw in &c.raw_elems {
        elems.raw(raw);
    }
    if !appended.is_empty() {
        let base = slot_of[&appended[0]];
        elems.active(
            None,
            &ConstExpr::i32_const(base as i32),
            Elements::Functions(appended.into()),
        );
    }
    module.section(&elems);

    if let Some(count) = c.data_count {
        module.section(&wasm_encoder::DataCountSection { count });
    }

    // Code: deferred -> placeholder; a primary function calling a deferred one ->
    // call rewritten to call_indirect; else copied verbatim.
    let mut code = CodeSection::new();
    for (di, body) in c.bodies.iter().enumerate() {
        let f = c.func_imports + di as u32;
        if owner.contains_key(&f) {
            let mut placeholder = Function::new([]);
            placeholder.instruction(&Instruction::Unreachable);
            placeholder.instruction(&Instruction::End);
            code.function(&placeholder);
        } else if body_refs(body)?.iter().any(|t| owner.contains_key(t)) {
            code.function(&rewrite_entry(body, c, owner, slot_of)?);
        } else {
            let range = body.range();
            code.raw(&core[range]);
        }
    }
    module.section(&code);

    if let Some(d) = c.data {
        module.section(&RawSection { id: 11, data: d });
    }
    Ok(module.finish())
}

/// A primary function that directly calls deferred functions: copy its instructions, but
/// expand each `call D` into `i32.const slot; call_indirect (type of D)` on table 0.
fn rewrite_entry(
    body: &wasmparser::FunctionBody,
    c: &Core,
    owner: &HashMap<u32, usize>,
    slot_of: &BTreeMap<u32, u32>,
) -> Result<Function> {
    let mut func = RoundtripReencoder
        .new_function_with_parsed_locals(body)
        .map_err(|e| anyhow::anyhow!("re-encoding locals: {e:?}"))?;
    let mut reader = body.get_operators_reader().context("reading entry body")?;
    while !reader.eof() {
        let mut peek = reader.clone();
        let op = peek.read().context("decoding an operator")?;
        if let Operator::Call { function_index } = op {
            if owner.contains_key(&function_index) {
                reader.read().ok();
                let slot = slot_of[&function_index];
                let type_index = c.func_type(function_index);
                func.instruction(&Instruction::I32Const(slot as i32));
                func.instruction(&Instruction::CallIndirect { type_index, table_index: 0 });
                continue;
            }
        }
        // A tail call to a deferred function would re-encode verbatim as a direct
        // call into the `unreachable` placeholder — a permanent trap the chunk never
        // repairs (chunks overwrite table slots, not bodies). Refused like
        // `call_ref`: an unhandled call operator is a hard error, never a silent
        // miscompile waiting for rustc to enable tail calls.
        if let Operator::ReturnCall { function_index } = op {
            if owner.contains_key(&function_index) {
                anyhow::bail!(
                    "function tail-calls deferred function {function_index}; the \
                     splitter rewrites plain calls only"
                );
            }
        }
        let instr = RoundtripReencoder
            .parse_instruction(&mut reader)
            .map_err(|e| anyhow::anyhow!("re-encoding an instruction: {e:?}"))?;
        func.instruction(&instr);
    }
    Ok(func)
}

/// One live's chunk: import the shared surface and every primary function its code calls
/// (under `""`), define its functions with all references remapped to the chunk index
/// space, and install them into the shared table with an active element segment.
fn emit_chunk(
    c: &Core,
    funcs: &[u32],
    owner: &HashMap<u32, usize>,
    slot_of: &BTreeMap<u32, u32>,
) -> Result<Vec<u8>> {
    // Primary functions this chunk calls (imported under ""), in a stable order.
    let mut imported_funcs: BTreeSet<u32> = BTreeSet::new();
    let deferred_here: BTreeSet<u32> = funcs.iter().copied().collect();
    for &f in funcs {
        for target in body_refs(c.body(f))? {
            if !deferred_here.contains(&target) {
                if owner.contains_key(&target) {
                    bail!("a chunk function calls another live's deferred function {target}");
                }
                imported_funcs.insert(target);
            }
        }
    }

    // Chunk index space: imported funcs first, then this chunk's functions.
    let mut remap: HashMap<u32, u32> = HashMap::new();
    let mut next = 0u32;
    for &f in &imported_funcs {
        remap.insert(f, next);
        next += 1;
    }
    for &f in funcs {
        remap.insert(f, next);
        next += 1;
    }

    let mut module = Module::new();
    if let Some(t) = c.types {
        module.section(&RawSection { id: 1, data: t });
    }

    let mut imports = ImportSection::new();
    for &f in &imported_funcs {
        imports.import("", &export_name_func(f), EntityType::Function(c.func_type(f)));
    }
    if c.memory_count > 0 {
        imports.import(
            "",
            memory_import_name(c),
            EntityType::Memory(wasm_encoder::MemoryType {
                minimum: 0,
                maximum: None,
                memory64: false,
                shared: false,
                page_size_log2: None,
            }),
        );
    }
    // Import every global in index order, so a body's `global.get N` keeps meaning N.
    for (g, ty) in c.global_types.iter().enumerate() {
        imports.import("", &export_name_global(g as u32), EntityType::Global(*ty));
    }
    let table = c.table.as_ref().expect("chunk needs the shared table");
    imports.import(
        "",
        table_import_name(c),
        EntityType::Table(wasm_encoder::TableType {
            element_type: table.element_type,
            // Permissive limits: the primary's table is grown to fit appended slots, so the
            // chunk must accept whatever size it is handed.
            minimum: 0,
            maximum: None,
            table64: false,
            shared: false,
        }),
    );
    module.section(&imports);

    // Function section: this chunk's functions, by type.
    let mut fsec = wasm_encoder::FunctionSection::new();
    for &f in funcs {
        fsec.function(c.func_type(f));
    }
    module.section(&fsec);

    // Active element segment(s): install each function at its slot.
    let mut elems = ElementSection::new();
    let mut by_slot: BTreeMap<u32, u32> = BTreeMap::new();
    for &f in funcs {
        if let Some(&slot) = slot_of.get(&f) {
            by_slot.insert(slot, remap[&f]);
        }
    }
    for (slot, func) in run_lengths(&by_slot) {
        elems.active(None, &ConstExpr::i32_const(slot as i32), Elements::Functions(func.into()));
    }
    // Declare the functions this chunk's bodies take references to, so `ref.func` is legal.
    let mut declared: BTreeSet<u32> = BTreeSet::new();
    for &f in funcs {
        for target in ref_func_targets(c.body(f))? {
            declared.insert(remap[&target]);
        }
    }
    if !declared.is_empty() {
        let refs: Vec<u32> = declared.into_iter().collect();
        elems.declared(Elements::Functions(refs.as_slice().into()));
    }
    module.section(&elems);

    // Code: each function re-encoded with references remapped into the chunk space.
    let mut code = CodeSection::new();
    let mut reenc = RemapReencoder { remap: &remap, unmapped: Vec::new() };
    for &f in funcs {
        let body = c.body(f).clone();
        reenc
            .parse_function_body(&mut code, body)
            .map_err(|e| anyhow::anyhow!("re-encoding a chunk function: {e:?}"))?;
    }
    if !reenc.unmapped.is_empty() {
        anyhow::bail!(
            "chunk body references functions outside the import cover: {:?} — a \
             primary-space index inside a chunk calls the wrong function",
            reenc.unmapped
        );
    }
    module.section(&code);
    Ok(module.finish())
}

/// Group adjacent slots into contiguous element segments (one `active` per run).
fn run_lengths(by_slot: &BTreeMap<u32, u32>) -> Vec<(u32, Vec<u32>)> {
    let mut runs: Vec<(u32, Vec<u32>)> = Vec::new();
    for (&slot, &func) in by_slot {
        match runs.last_mut() {
            Some((base, funcs)) if *base + funcs.len() as u32 == slot => funcs.push(func),
            _ => runs.push((slot, vec![func])),
        }
    }
    runs
}

fn memory_import_name<'a>(c: &Core<'a>) -> &'a str {
    for (name, kind, _) in &c.exports {
        if matches!(kind, ExportKind::Memory) {
            return leak(name.clone());
        }
    }
    EXPORT_MEM
}
fn table_import_name<'a>(c: &Core<'a>) -> &'a str {
    for (name, kind, _) in &c.exports {
        if matches!(kind, ExportKind::Table) {
            return leak(name.clone());
        }
    }
    EXPORT_TABLE
}
fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn raw_from(wasm: &[u8], id: u8) -> Option<RawSection<'_>> {
    for payload in Parser::new(0).parse_all(wasm) {
        if let Ok(Payload::FunctionSection(r)) = payload {
            if id == 3 {
                return Some(RawSection { id, data: &wasm[r.range()] });
            }
        }
    }
    None
}

/// A `Reencode` that remaps every function reference into the chunk index space.
struct RemapReencoder<'a> {
    remap: &'a HashMap<u32, u32>,
    /// A reference to a function outside the import cover — recorded rather than
    /// passed through, because a primary-space index inside a chunk body calls the
    /// wrong function silently. The caller turns any entry here into a hard error.
    unmapped: Vec<u32>,
}
impl Reencode for RemapReencoder<'_> {
    type Error = std::convert::Infallible;
    fn function_index(&mut self, func: u32) -> Result<u32, wasm_encoder::reencode::Error> {
        match self.remap.get(&func) {
            Some(&mapped) => Ok(mapped),
            None => {
                self.unmapped.push(func);
                Ok(func)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    /// A core with a start section and a data-count section (forced by `data.drop`
    /// over a passive segment): the primary must re-emit both, in spec order —
    /// start (8) before elements (9), data-count (12) before code (10).
    #[test]
    fn a_primary_keeps_the_start_and_data_count_sections_valid() {
        let core = wat::parse_str(
            r#"
            (module
              (memory 1)
              (table 2 funcref)
              (elem (i32.const 0) $deferred)
              (data "passive")
              (func $init (data.drop 0))
              (start $init)
              (export "__idyll_live_root_0" (func $root))
              (func $root (call $deferred))
              (func $deferred))
            "#,
        )
        .expect("fixture assembles");

        let deferred = 2; // $init = 0, $root = 1, $deferred = 2
        let (primary, chunks) = split(&core, &[vec![deferred]]).expect("splits");
        wasmparser::Validator::new()
            .validate_all(&primary)
            .expect("the primary validates with start + data-count in order");
        assert_eq!(chunks.len(), 1);
        for (_, bytes) in &chunks {
            wasmparser::Validator::new().validate_all(bytes).expect("the chunk validates");
        }
    }
}
