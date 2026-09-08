// Never compiled — parsed by tests/extract.rs to pin the extractor's rejection of a
// nested #[styles] module (it would collide with the file's class identity).

mod inner {
    #[styles]
    mod styles {
        pub const HIDDEN: Style = css! {{ color: "#fff" }};
    }
}
