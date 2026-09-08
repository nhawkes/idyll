//! The **route contract**: routing unified with components.
//!
//! The server publishes one distinguished root query — `route(request) -> Option<Page>`
//! — and everything downstream is the same machinery seen twice: first load executes it
//! natively and mounts the app's **page component** through the membrane with the
//! executed seed; client navigation fires the *same persisted query* over HTTP and
//! mounts the same component against the new seed. The page node is **pure data** —
//! views exist only in the app crate. Path parsing happens exactly once, on the
//! server, in native Rust; the client never sees a route pattern.
//!
//! The vocabulary split:
//! - [`Request`] is **framework** vocabulary — the route root's one argument, published
//!   in the app's schema as an ordinary embedded value (`.value::<Request>()`).
//! - The **page node is the app's**: any `#[node]` satisfying the contract checked by
//!   [`validate_route_contract`] — `id: String` (the request path: the cache normalizes
//!   pages by path, so back/forward replays from cache) and `title: String` (the host
//!   writes the envelope `<title>`). Every other field is the app's business — what
//!   its page component and live read through their fragments; the app's route
//!   *query* selects what its page needs.

use serde::{Deserialize, Serialize};

use crate::error::{PageTitleError, ValidateError};
use crate::schema::{FieldType, RecordDef, Schema};

/// The route root's argument: what the server knows about the incoming request.
/// Deliberately minimal — widen it (query params, headers…) as pages need more; it is
/// an ordinary schema value, so widening is an ordinary schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub path: String,
}

impl Request {
    pub fn new(path: impl Into<String>) -> Self {
        Request { path: path.into() }
    }
}

impl crate::Record for Request {
    const TYPE_NAME: &'static str = "Request";
}

impl crate::schema::DescribeRecord for Request {
    fn describe() -> RecordDef {
        RecordDef {
            name: "Request".to_string(),
            fields: vec![crate::schema::FieldDef {
                name: "path".to_string(),
                ty: FieldType::scalar("String"),
            }],
        }
    }
}

// Reached from the route root's `request` argument — apps never list it.
impl crate::schema::SchemaType for Request {
    fn register(schema: &mut Schema) {
        if schema.record("Request").is_none() {
            schema.values.push(<Request as crate::schema::DescribeRecord>::describe());
        }
    }
}

/// The name the framework finds the route root by.
pub const ROUTE_ROOT: &str = "route";

/// Implemented by `query!`-generated roots whose single root is `route` — the
/// standardized page operation. Gives route-generic library code (the
/// [store](crate::store)) the page key without knowing the app's query type.
pub trait RouteRoots {
    type Page: crate::NodeFragment;

    fn page(&self) -> crate::Frag<Self::Page>;
}

/// Check the published schema honours the route contract; returns the page node's
/// [`RecordDef`]. Boot calls this and fails loud — a drifted contract is a config
/// error, not a request-time surprise.
pub fn validate_route_contract(schema: &Schema) -> Result<RecordDef, ValidateError> {
    let root = schema.root_def(ROUTE_ROOT).ok_or(ValidateError::NoRouteRoot)?;
    if root.list {
        return Err(ValidateError::RouteRootIsList);
    }
    match root.args.as_slice() {
        [arg] if arg.name == "request" && arg.ty == FieldType::value("Request") => {}
        other => return Err(ValidateError::RouteRootArgs { found: other.to_vec() }),
    }
    if schema.record("Request").map(|r| r != &<Request as crate::schema::DescribeRecord>::describe()).unwrap_or(true) {
        return Err(ValidateError::RequestNotPublished);
    }

    let page = schema
        .record(&root.output)
        .ok_or_else(|| ValidateError::UnknownPageRecord { yields: root.output.clone() })?;
    let field = |name: &str| page.fields.iter().find(|f| f.name == name);
    let expect = |name: &str, ty: FieldType| -> Result<(), ValidateError> {
        match field(name) {
            Some(f) if f.ty == ty => Ok(()),
            Some(f) => Err(ValidateError::PageFieldType {
                page: page.name.clone(),
                field: name.to_string(),
                expected: ty,
                found: f.ty.clone(),
            }),
            None => Err(ValidateError::PageFieldMissing {
                page: page.name.clone(),
                field: name.to_string(),
                expected: ty,
            }),
        }
    };
    // `id` is the request path: pages normalize in the cache by path, so a revisited
    // route replays from cache.
    expect("id", FieldType::scalar("String"))?;
    expect("title", FieldType::scalar("String"))?;
    Ok(page.clone())
}

/// Pull the page's `title` out of an executed route query's seed — the one contract
/// field the host consumes (it writes the envelope `<title>`; everything else in the
/// record is data the app's components read). The page's cache id is the request path
/// (the contract), so the lookup is exact; a missing record or a malformed field is a
/// loud error — boot validated the schema, so this can only mean the resolver broke
/// the contract at runtime.
pub fn page_title(
    executed: &crate::Executed,
    page: &RecordDef,
    path: &str,
) -> Result<String, PageTitleError> {
    let record = executed
        .seed
        .records()
        .find(|(tag, json)| *tag == page.name && json["id"] == path)
        .map(|(_, json)| json)
        .ok_or_else(|| PageTitleError::MissingRecord {
            page: page.name.clone(),
            path: path.to_string(),
        })?;
    record
        .get("title")
        .ok_or(PageTitleError::MissingTitle)?
        .as_str()
        .map(str::to_string)
        .ok_or(PageTitleError::TitleNotString)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{FieldDef, RootDef};

    fn page_record() -> RecordDef {
        RecordDef {
            name: "Page".to_string(),
            fields: vec![
                FieldDef { name: "id".into(), ty: FieldType::scalar("String") },
                FieldDef { name: "title".into(), ty: FieldType::scalar("String") },
            ],
        }
    }

    fn route_root(output: &str) -> crate::schema::RootEntry {
        crate::schema::RootEntry {
            def: RootDef {
                name: ROUTE_ROOT.to_string(),
                args: vec![FieldDef { name: "request".into(), ty: FieldType::value("Request") }],
                output: output.to_string(),
                list: false,
            },
            register: |schema| {
                <Request as crate::schema::SchemaType>::register(schema);
            },
        }
    }

    fn conforming() -> Schema {
        let mut schema = Schema::new().root(route_root("Page"));
        schema.nodes.push(page_record());
        schema
    }

    #[test]
    fn a_conforming_schema_passes_and_yields_the_page_record() {
        let page = validate_route_contract(&conforming()).unwrap();
        assert_eq!(page.name, "Page");
    }

    #[test]
    fn every_contract_break_is_loud() {
        // No route root at all.
        assert_eq!(
            validate_route_contract(&Schema::new()).unwrap_err(),
            ValidateError::NoRouteRoot
        );

        // Wrong argument shape — the refusal carries what was actually found.
        let mut wrong_arg = conforming();
        wrong_arg.roots[0].args[0].ty = FieldType::scalar("String");
        assert_eq!(
            validate_route_contract(&wrong_arg).unwrap_err(),
            ValidateError::RouteRootArgs {
                found: vec![FieldDef { name: "request".into(), ty: FieldType::scalar("String") }],
            }
        );

        // Request value not published.
        let mut no_request = conforming();
        no_request.values.clear();
        assert_eq!(
            validate_route_contract(&no_request).unwrap_err(),
            ValidateError::RequestNotPublished
        );

        // Page missing a contract field.
        let mut no_title = conforming();
        no_title.nodes[0].fields.retain(|f| f.name != "title");
        assert_eq!(
            validate_route_contract(&no_title).unwrap_err(),
            ValidateError::PageFieldMissing {
                page: "Page".into(),
                field: "title".into(),
                expected: FieldType::scalar("String"),
            }
        );

        // Page id must be the path (a String).
        let mut wrong_id = conforming();
        wrong_id.nodes[0].fields[0].ty = FieldType::scalar("u64");
        assert_eq!(
            validate_route_contract(&wrong_id).unwrap_err(),
            ValidateError::PageFieldType {
                page: "Page".into(),
                field: "id".into(),
                expected: FieldType::scalar("String"),
                found: FieldType::scalar("u64"),
            }
        );
    }
}
