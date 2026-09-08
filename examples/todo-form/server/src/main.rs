//! Minimal hydration's server — data and infra only: the route resolver loads the
//! todos (newest first) and returns a pure-data [`Page`]. The UI — the page chrome,
//! the one form live, the plain list rows — lives in `todo-form-app`.

use idyll_data::{
    mutation_handler, node, root, AppRoot, BoxError, Fetch, MutationHandle, Mutations, Queries,
    Ref, Request, Root, RootHandle,
};
use idyll_serve::Server;

#[node]
pub struct Todo {
    pub id: u64,
    pub text: String,
    pub done: bool,
}

#[node]
pub struct Page {
    pub id: String,
    pub title: String,
    pub todos: Vec<Ref<Todo>>,
}

#[derive(Clone, Default)]
struct Db {
    todos: std::sync::Arc<std::sync::RwLock<Vec<Todo>>>,
}

impl Db {
    fn seeded() -> Self {
        Db {
            todos: std::sync::Arc::new(std::sync::RwLock::new(vec![
                Todo { id: 3, text: "Render through the membrane".into(), done: false },
                Todo { id: 2, text: "Preload a query".into(), done: true },
                Todo { id: 1, text: "Learn idyll".into(), done: true },
            ])),
        }
    }

    fn snapshot(&self) -> Vec<Todo> {
        self.todos.read().unwrap().clone()
    }

    fn get(&self, id: u64) -> Option<Todo> {
        self.todos.read().unwrap().iter().find(|t| t.id == id).cloned()
    }

    /// Newest first: the insert-at-front shape that shifts every row below it.
    fn add(&self, text: String) -> Todo {
        let mut todos = self.todos.write().unwrap();
        let id = todos.iter().map(|t| t.id).max().unwrap_or(0) + 1;
        let todo = Todo { id, text, done: false };
        todos.insert(0, todo.clone());
        todo
    }
}

/// The node resolver the executor follows `Ref<Todo>` edges with.
impl Fetch<Db> for Todo {
    async fn fetch(db: Db, id: u64) -> Result<Todo, BoxError> {
        db.get(id).ok_or_else(|| format!("no Todo with id {id}").into())
    }
}

#[root]
async fn route(db: &Db, request: Request) -> Result<Option<Page>, std::convert::Infallible> {
    Ok((request.path == "/").then(|| Page {
        id: "/".to_string(),
        title: "Todos — one form live".to_string(),
        todos: db.snapshot().iter().map(|todo| Ref::new(todo.id)).collect(),
    }))
}

#[mutation_handler("add-todo")]
async fn add_todo(db: &Db, text: String) -> Result<Todo, std::convert::Infallible> {
    Ok(db.add(text))
}

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
    let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3003);

    Server::builder()
        .app_crate("todo-form-app")
        .route_query(todo_form_app::RouteQuery::query_file())
        .root(root())
        .mutations(vec![todo_form_app::AddTodoOp::mutation_file()])
        .data(Db::seeded())
        .manifest_dir(env!("CARGO_MANIFEST_DIR"))
        .mode(idyll_serve::mode_from_args())
        .port(port)
        .build()
        .serve()
        .await
}
