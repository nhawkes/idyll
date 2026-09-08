//! Data-driven todos — the server is data and infra: the route resolver loads the
//! todos (newest first) and returns a pure-data [`Page`]. The UI — the page
//! component, the store-root list, the per-row `check` live — lives in
//! `todo-live-app`.

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

    fn toggle(&self, id: u64) -> Option<Todo> {
        let mut todos = self.todos.write().unwrap();
        let todo = todos.iter_mut().find(|t| t.id == id)?;
        todo.done = !todo.done;
        Some(todo.clone())
    }
}

/// The node resolver the executor follows `Ref<Todo>` edges with.
impl Fetch<Db> for Todo {
    async fn fetch(db: Db, id: u64) -> Result<Todo, BoxError> {
        db.get(id).ok_or_else(|| format!("no Todo with id {id}").into())
    }
}

fn page(db: &Db) -> Page {
    Page {
        id: "/".to_string(),
        title: "Todos — data-driven".to_string(),
        todos: db.snapshot().iter().map(|todo| Ref::new(todo.id)).collect(),
    }
}

#[root]
async fn route(db: &Db, request: Request) -> Result<Option<Page>, std::convert::Infallible> {
    Ok((request.path == "/").then(|| page(db)))
}

#[mutation_handler("add-todo")]
async fn add_todo(db: &Db, text: String) -> Result<Todo, std::convert::Infallible> {
    Ok(db.add(text))
}

#[mutation_handler("toggle-todo")]
async fn toggle_todo(db: &Db, id: u64) -> Result<Todo, std::io::Error> {
    db.toggle(id)
        .ok_or_else(|| std::io::Error::other(format!("no Todo with id {id}")))
}

#[derive(Queries)]
struct Query {
    route: RootHandle<Db>,
}

#[derive(Mutations)]
struct Mutation {
    add_todo: MutationHandle<Db>,
    toggle_todo: MutationHandle<Db>,
}

fn root() -> AppRoot<Db> {
    AppRoot::from(Root {
        query: Query { route: route() },
        mutation: Mutation { add_todo: add_todo(), toggle_todo: toggle_todo() },
    })
    .fetch::<Todo>()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let port = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(3002);

    Server::builder()
        .app_crate("todo-live-app")
        .route_query(todo_islands_app::RouteQuery::query_file())
        .root(root())
        .mutations(vec![
            todo_islands_app::AddTodoOp::mutation_file(),
            todo_islands_app::ToggleTodoOp::mutation_file(),
        ])
        .data(Db::seeded())
        .manifest_dir(env!("CARGO_MANIFEST_DIR"))
        .mode(idyll_serve::mode_from_args())
        .port(port)
        .build()
        .serve()
        .await
}
