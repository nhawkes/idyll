//! The **server** — data and infra, nothing else: the `route(request)` root query
//! parses the path natively (exactly once — the decision crosses as the page's
//! `kind`), loads the data, and returns a pure-data [`Page`] node. The UI — the page
//! component, its head, the live — lives in `todo-app`, mounted through the
//! membrane per request and claimed by the browser.

use idyll_data::{
    mutation_handler, node, root, value, AppRoot, BoxError, Fetch, MutationHandle, Mutations,
    Queries, Ref, Request, Root, RootHandle,
};
use idyll_serve::Server;

// ── The schema's nodes (server-side data modeling; clients see projections) ─────────

#[node]
pub struct Todo {
    pub id: u64,
    pub text: String,
    pub done: bool,
}

/// The parsed route, **as a sum**: each variant carries exactly its own data, so the
/// prose page never fetches (or ships) a todo, and the page component matches the
/// closed set exhaustively.
#[value]
pub enum Route {
    Todos { todos: Vec<Ref<Todo>> },
    Prose,
}

/// The route root's output: the framework contract (`id` = path, `title`) plus the
/// parsed route and its payload.
#[node]
pub struct Page {
    pub id: String,
    pub title: String,
    pub route: Route,
}

/// The app's data source — **the** todo state: an in-memory list behind a lock (a real
/// app would put a db pool here). A shared handle with interior mutability: the one
/// store both reads (the route resolver, the `Ref` fetcher) and writes (mutation
/// handlers) go through.
#[derive(Clone, Default)]
struct Db {
    todos: std::sync::Arc<std::sync::RwLock<Vec<Todo>>>,
}

impl Db {
    fn seeded() -> Self {
        Db {
            todos: std::sync::Arc::new(std::sync::RwLock::new(vec![
                Todo { id: 1, text: "Learn idyll".into(), done: true },
                Todo { id: 2, text: "Preload a query".into(), done: true },
                Todo { id: 3, text: "Render through the membrane".into(), done: false },
            ])),
        }
    }

    fn snapshot(&self) -> Vec<Todo> {
        self.todos.read().unwrap().clone()
    }

    fn get(&self, id: u64) -> Option<Todo> {
        self.todos.read().unwrap().iter().find(|t| t.id == id).cloned()
    }

    fn add(&self, text: String) -> Todo {
        let mut todos = self.todos.write().unwrap();
        let id = todos.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        let todo = Todo { id, text, done: false };
        todos.push(todo.clone());
        todo
    }
}

// ── The route root — the whole routing story, in native Rust ─────────────────────────

fn page(path: &str, title: &str, route: Route) -> Page {
    Page { id: path.to_string(), title: title.to_string(), route }
}

/// `route(request) -> Option<Page>`. **Absence is a value** (`Ok(None)`) — the
/// executor's typed 404. The path parses HERE, once; downstream matches the variant.
#[root]
async fn route(db: &Db, request: Request) -> Result<Option<Page>, std::convert::Infallible> {
    let resolved = match request.path.as_str() {
        "/" => Some(page(
            "/",
            "Todos — idyll",
            Route::Todos {
                todos: db.snapshot().iter().map(|todo| Ref::new(todo.id)).collect(),
            },
        )),
        "/prose" => Some(page("/prose", "Prose — idyll", Route::Prose)),
        _ => None,
    };
    Ok(resolved)
}

/// The node resolver the executor follows `Ref<Todo>` edges with.
impl Fetch<Db> for Todo {
    async fn fetch(db: Db, id: u64) -> Result<Todo, BoxError> {
        db.get(id).ok_or_else(|| format!("no Todo with id {id}").into())
    }
}

/// The `add-todo` mutation's write: append and return the created row (the server's
/// answer is the authority; the executor masks it to each artifact's selection).
#[mutation_handler("add-todo")]
async fn add_todo(db: &Db, text: String) -> Result<Todo, std::convert::Infallible> {
    Ok(db.add(text))
}

// ── Boot ─────────────────────────────────────────────────────────────────────────────

/// The app's **published schema** — the server's contract, emitted as `schema.json` at
/// boot; the app crate's macros validate against it.
/// The app's entry surface — GraphQL's root-type model, structurally: more queries
/// are more fields on `Query`, more mutations on `Mutation`. The ONE object yields
/// both the published schema (the reachability closure of the entries — `Request`,
/// `Page`, `Route`, `Todo` all arrive uninvited) and the resolver table, so the
/// two cannot drift.
#[derive(Queries)]
struct Query {
    route: RootHandle<Db>,
}

#[derive(Mutations)]
struct Mutation {
    add_todo: MutationHandle<Db>,
}

fn root() -> AppRoot<Db> {
    AppRoot::from(Root {
        query: Query { route: route() },
        mutation: Mutation { add_todo: add_todo() },
    })
    .fetch::<Todo>()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3001);

    Server::builder()
        .app_crate("todo-app")
        .route_query(todo_app::RouteQuery::query_file())
        .root(root())
        .mutations(vec![todo_app::AddTodoOp::mutation_file()])
        .data(Db::seeded())
        .manifest_dir(env!("CARGO_MANIFEST_DIR"))
        .mode(idyll_serve::mode_from_args())
        .port(port)
        .build()
        .serve()
        .await
}
