# Run the end-to-end todo example — the server builds the app crate to ONE wasip2
# component itself (membrane SSR + jco browser bundle) and hot-reloads on change.
todo:
    cargo run -p todo-server

# Run the server-driven todos example (keyed row islands).
todo-islands:
    cargo run -p todo-islands-server

# The SPA posture: one island owns the page.
todo-spa:
    cargo run -p todo-spa-server

# Minimal hydration: one add-form island, server-owned list.
todo-form:
    cargo run -p todo-form-server
