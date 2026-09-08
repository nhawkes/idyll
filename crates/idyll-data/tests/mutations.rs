//! Persisted **mutations** end to end minus HTTP: `#[mutation_handler]` emits the
//! descriptor + typed glue from one signature; the client-side `mutation!` artifact
//! validates against the schema; execution decodes the variables into typed parameters
//! and masks the returned node to the artifact's recorded selection.

use idyll_data::{
    execute_mutation, mutation_handler, node, validate_mutation, CanonMutation, Resolvers, Schema,
};
use std::sync::{Arc, Mutex};

#[node]
pub struct Echo {
    pub id: u64,
    pub text: String,
    /// Deliberately never selected by the artifact under test — must not cross back.
    pub secret: String,
}

/// The app's "data source": a shared handle with interior mutability, like a pool.
#[derive(Clone, Default)]
struct Store(Arc<Mutex<Vec<String>>>);

/// The handler under test: `text` arrives **typed** (the generated resolver decodes the
/// operation variables before this body runs), and the full node goes back.
#[mutation_handler("shout")]
async fn shout(store: &Store, text: String) -> Result<Echo, std::convert::Infallible> {
    let loud = text.to_uppercase();
    store.0.lock().unwrap().push(loud.clone());
    Ok(Echo { id: 1, text: loud, secret: "server-only".into() })
}

// The mutation's reachability closure registers Echo.
fn schema() -> Schema {
    Schema::new().mutation(shout().entry)
}

/// The persisted artifact: `shout(text) { text }` — selects `text`, not `secret`.
fn artifact() -> CanonMutation {
    let json = serde_json::json!({
        "mutation": "shout",
        "args": ["text"],
        "response": { "on": "Echo", "selection": [ { "kind": "leaf", "field": "text" } ] }
    });
    CanonMutation::from_canonical_json(&json.to_string()).expect("artifact parses")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = std::pin::pin!(future);
    loop {
        if let Poll::Ready(out) = future.as_mut().poll(&mut cx) {
            return out;
        }
    }
}

#[test]
fn a_mutation_artifact_validates_executes_typed_and_masks_the_response() {
    let schema = schema();
    let artifact = artifact();
    validate_mutation(&schema, &artifact).expect("artifact typechecks");

    let resolvers: Resolvers<Store> = Resolvers::new().mutation(shout().resolver);
    assert!(resolvers.has_mutation("shout"), "the boot check's handler lookup");

    let store = Store::default();
    let masked = block_on(execute_mutation(
        &schema,
        &artifact,
        &resolvers,
        &store,
        &serde_json::json!({ "text": "hello" }),
    ))
    .expect("executes");

    // Typed handler ran against the shared source…
    assert_eq!(*store.0.lock().unwrap(), vec!["HELLO".to_string()]);
    // …and the response is masked to the selection: id rides, `secret` does not.
    assert_eq!(masked, serde_json::json!({ "id": 1, "text": "HELLO" }));
}

// ── Nested value selections in responses (against the crate-root fixture schema) ──

idyll_data::mutation! {
    RenameUser($name: String) = "rename-user"(name: $name) { name, wallet { amount } }
}

#[node]
pub struct User {
    pub id: u64,
    pub name: String,
    pub wallet: Wallet,
}

#[idyll_data::value]
pub struct Wallet {
    pub amount: u64,
    pub currency: String,
}

#[mutation_handler("rename-user")]
async fn rename_user(_store: &Store, name: String) -> Result<User, std::convert::Infallible> {
    Ok(User { id: 7, name, wallet: Wallet { amount: 5, currency: "GBP".into() } })
}

#[test]
fn nested_value_selections_mask_and_type_recursively() {
    use idyll::Mutation as _;

    // The artifact records the nested selection and validates against the schema.
    let fixture = idyll_data::Schema::from_json(include_str!("../schema.json")).unwrap();
    let artifact =
        CanonMutation::from_canonical_json(&RenameUser::mutation_file().contents).unwrap();
    validate_mutation(&fixture, &artifact).expect("nested artifact typechecks");

    // Execution masks recursively: wallet.currency was not selected and must not cross.
    let resolvers: Resolvers<Store> = Resolvers::new().mutation(rename_user().resolver);
    let masked = block_on(execute_mutation(
        &fixture,
        &artifact,
        &resolvers,
        &Store::default(),
        &serde_json::json!({ "name": "Ada" }),
    ))
    .expect("executes");
    assert_eq!(
        masked,
        serde_json::json!({ "id": 7, "name": "Ada", "wallet": { "amount": 5 } })
    );

    // The typed response projection deserializes the masked shape, nested and owned.
    let response: RenameUserResponse = serde_json::from_value(masked).unwrap();
    assert_eq!(response.name, "Ada");
    assert_eq!(response.wallet.amount, 5);

    // Its identity is the artifact's hash — what ctx.mutate ships.
    assert_eq!(RenameUser::op_hash(), artifact.op_hash());
}

#[test]
fn bad_variables_and_schema_drift_are_loud() {
    let schema = schema();
    let artifact = artifact();
    let resolvers: Resolvers<Store> = Resolvers::new().mutation(shout().resolver);

    // A missing variable is a typed boundary error, not a handler mystery.
    let err = block_on(execute_mutation(
        &schema,
        &artifact,
        &resolvers,
        &Store::default(),
        &serde_json::json!({}),
    ))
    .unwrap_err();
    assert!(err.to_string().contains("text"), "unhelpful error: {err}");

    // An artifact passing an undeclared argument fails boot validation.
    let drifted = CanonMutation::from_canonical_json(
        &serde_json::json!({
            "mutation": "shout",
            "args": ["volume"],
            "response": { "on": "Echo", "selection": [ { "kind": "leaf", "field": "text" } ] }
        })
        .to_string(),
    )
    .unwrap();
    let err = validate_mutation(&schema, &drifted).unwrap_err();
    assert_eq!(
        err,
        idyll_data::ValidateError::UnknownArgument {
            mutation: "shout".into(),
            argument: "volume".into(),
        }
    );
}
