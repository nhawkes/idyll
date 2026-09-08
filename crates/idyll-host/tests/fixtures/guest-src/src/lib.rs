//! The membrane test fixture: a minimal `idyll:ssr` **world `app`** component with no
//! idyll deps, so the host tests exercise every membrane path without the full app build.
//!
//! Behavior is selected by the **live table** — the typed dispatch channel — never by
//! magic seed bytes (the seed is data; the paint echoes its size). `paint` → a canned
//! stream, `boom` → panic (trap → fault), `spin` → infinite loop (epoch trap →
//! `BlewBudget`), `decline` → a typed mount error (`MountOutcome::Failed`).

wit_bindgen::generate!({ path: "../../../wit", world: "app" });

struct Component;

/// A settled slice: all commands, nothing left pending. This fixture does its whole
/// paint in `mount`, so every entry point returns a settled result and `flush` never
/// has work to do.
fn settled(commands: Vec<Command>) -> FlushResult {
    FlushResult { commands, done: true, pending_lane: None }
}

impl Guest for Component {
    fn live() -> Vec<String> {
        vec![
            "paint".to_string(),
            "boom".to_string(),
            "spin".to_string(),
            "decline".to_string(),
        ]
    }

    fn mount(
        live: LiveRef,
        _parent: Option<LiveRef>,
        seed: Vec<u8>,
        _client: bool,
    ) -> Result<MountResult, String> {
        match live.name.as_str() {
            "boom" => panic!("guest mount fault"),
            "spin" =>
            {
                #[allow(clippy::empty_loop)]
                loop {}
            }
            "decline" => Err("this live politely declines".to_string()),
            // A canned paint with no client work — the static-paint arm, so the host
            // path that threads `static_paint` through `MountOutcome` is exercised.
            _ => Ok(MountResult {
                static_paint: true,
                flush: settled(vec![
                Command::ReplaceTemplate(TemplateCmd {
                    template_id: 0,
                    nodes: vec![
                        TplNode::Element(TplElement {
                            tag: "p".to_string(),
                            attrs: Vec::new(),
                            slot: None,
                            children: 1,
                        }),
                        TplNode::Text(format!(
                            "{}#{} seed {} bytes",
                            live.name,
                            live.instance,
                            seed.len()
                        )),
                    ],
                    styles: Vec::new(),
                    svg: false,
                }),
                Command::MountRoot(0),
                ]),
            }),
        }
    }

    fn unmount(_live: LiveRef) -> FlushResult {
        settled(Vec::new())
    }

    fn dispatch(_live: LiveRef, _handler: u32, _event: DomEvent) -> FlushResult {
        settled(Vec::new())
    }

    fn deliver(_live: LiveRef, _request: u32, _response: Result<Vec<u8>, RequestError>) -> FlushResult {
        settled(Vec::new())
    }

    fn flush(_budget: u32) -> FlushResult {
        settled(Vec::new())
    }
}

export!(Component);
