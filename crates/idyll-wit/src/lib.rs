pub const SSR: &str = include_str!("../wit/ssr.wit");

#[cfg(feature = "host")]
wasmtime::component::bindgen!({
    world: "app",
    path: "wit",
});
