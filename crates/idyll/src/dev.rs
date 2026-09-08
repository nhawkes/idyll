//! **Determinism + replay primitives** — the devtools foundation of the pure-handler
//! design: because handlers are pure and every effect flows through the message loop,
//! app state is a fold of (seed, message log). Replay is therefore **message-only** —
//! no state checkpoints: [`MessageLog`] records the loop's messages, [`ReplayInputs`]
//! records/replays the nondeterministic inputs (`now`, `random`), and
//! [`replay_component`] drives one end-to-end, re-feeding the recorded messages into a
//! fresh component with side effects inert (see [`Runtime::replay_mode`]). All native,
//! all driver-agnostic; the only serialization requirement is `Msg: Serialize`.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::component::{report_to_log, spawn_live};
use crate::ctx::{Ctx, Setup};
use crate::driver::DomDriver;
use crate::runtime::Runtime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReplayInputKind {
    NowMillis,
    RandomU64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayInputRecord {
    pub sequence: u64,
    pub kind: ReplayInputKind,
    pub json: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MessageRecord {
    pub sequence: u64,
    pub ty: String,
    pub json: serde_json::Value,
}

#[derive(Debug)]
pub enum ReplayInputError {
    Exhausted {
        expected: ReplayInputKind,
    },
    KindMismatch {
        expected: ReplayInputKind,
        actual: ReplayInputKind,
    },
    Decode(serde_json::Error),
}

impl std::fmt::Display for ReplayInputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReplayInputError::Exhausted { expected } => {
                write!(f, "replay input log is exhausted; expected {expected:?}")
            }
            ReplayInputError::KindMismatch { expected, actual } => {
                write!(
                    f,
                    "replay input kind mismatch: expected {expected:?}, got {actual:?}"
                )
            }
            ReplayInputError::Decode(error) => write!(f, "failed to decode replay input: {error}"),
        }
    }
}

impl std::error::Error for ReplayInputError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ReplayInputError::Decode(error) => Some(error),
            ReplayInputError::Exhausted { .. } | ReplayInputError::KindMismatch { .. } => None,
        }
    }
}

#[derive(Clone)]
pub struct ReplayInputs {
    inner: Rc<RefCell<ReplayInputsInner>>,
}

struct ReplayInputsInner {
    mode: ReplayInputMode,
    next_sequence: u64,
    records: Vec<ReplayInputRecord>,
}

enum ReplayInputMode {
    Recording,
    Replaying { cursor: usize },
}

impl Default for ReplayInputs {
    fn default() -> Self {
        Self::recording()
    }
}

impl ReplayInputs {
    pub fn recording() -> Self {
        Self {
            inner: Rc::new(RefCell::new(ReplayInputsInner {
                mode: ReplayInputMode::Recording,
                next_sequence: 0,
                records: Vec::new(),
            })),
        }
    }

    pub fn replay(records: Vec<ReplayInputRecord>) -> Self {
        let next_sequence = records
            .last()
            .map(|record| record.sequence.saturating_add(1))
            .unwrap_or(0);
        Self {
            inner: Rc::new(RefCell::new(ReplayInputsInner {
                mode: ReplayInputMode::Replaying { cursor: 0 },
                next_sequence,
                records,
            })),
        }
    }

    pub fn records(&self) -> Vec<ReplayInputRecord> {
        self.inner.borrow().records.clone()
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self.records()).expect("replay input records serialize")
    }

    pub fn now_millis(&self, produce: impl FnOnce() -> u64) -> Result<u64, ReplayInputError> {
        self.value(ReplayInputKind::NowMillis, produce)
    }

    pub fn random_u64(&self, produce: impl FnOnce() -> u64) -> Result<u64, ReplayInputError> {
        self.value(ReplayInputKind::RandomU64, produce)
    }

    fn value(
        &self,
        kind: ReplayInputKind,
        produce: impl FnOnce() -> u64,
    ) -> Result<u64, ReplayInputError> {
        let mut inner = self.inner.borrow_mut();
        match inner.mode {
            ReplayInputMode::Recording => {
                let value = produce();
                let record = ReplayInputRecord {
                    sequence: inner.next_sequence,
                    kind,
                    json: serde_json::to_value(value).expect("u64 replay input serializes"),
                };
                inner.next_sequence += 1;
                inner.records.push(record);
                Ok(value)
            }
            ReplayInputMode::Replaying { cursor } => {
                let index = cursor;
                let record = inner
                    .records
                    .get(index)
                    .cloned()
                    .ok_or(ReplayInputError::Exhausted { expected: kind })?;
                if record.kind != kind {
                    return Err(ReplayInputError::KindMismatch {
                        expected: kind,
                        actual: record.kind,
                    });
                }
                let value =
                    serde_json::from_value(record.json).map_err(ReplayInputError::Decode)?;
                inner.mode = ReplayInputMode::Replaying { cursor: index + 1 };
                Ok(value)
            }
        }
    }
}

/// The record type tag for a store absorb (a refresh seed applied in the owner's
/// turn) — the one non-app entry a component log carries.
pub const ABSORB_TY: &str = "idyll::store::Absorb";

/// The next delivery-sequence stamp, from the recording runtime's core: dequeue
/// order is execution order (single-threaded), so stamping from one per-runtime
/// counter gives per-component logs a **total order across the live tree** — merge
/// by sequence to replay a tree. Bumped only when recording; a log outliving its
/// runtime records nothing new, so the dead-weak stamp is unreachable in practice.
fn next_delivery_sequence(rt: &std::rc::Weak<crate::runtime::RuntimeCore>) -> u64 {
    rt.upgrade().map_or(0, |core| core.next_delivery_sequence())
}

#[derive(Clone, Default)]
pub struct MessageLog {
    inner: Rc<RefCell<MessageLogInner>>,
}

#[derive(Default)]
struct MessageLogInner {
    records: Vec<MessageRecord>,
}

impl MessageLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn records(&self) -> Vec<MessageRecord> {
        self.inner.borrow().records.clone()
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.inner.borrow_mut().records.clear();
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self.records()).expect("message records serialize")
    }

    pub fn decode<M>(&self) -> Result<Vec<M>, serde_json::Error>
    where
        M: DeserializeOwned,
    {
        self.decode_prefix(self.len())
    }

    pub fn decode_prefix<M>(&self, len: usize) -> Result<Vec<M>, serde_json::Error>
    where
        M: DeserializeOwned,
    {
        self.inner
            .borrow()
            .records
            .iter()
            .take(len)
            .map(|record| serde_json::from_value(record.json.clone()))
            .collect()
    }

    pub(crate) fn recorder<M>(&self, rt: &Rc<crate::runtime::RuntimeCore>) -> Rc<dyn Fn(&M)>
    where
        M: Serialize + 'static,
    {
        let inner = Rc::clone(&self.inner);
        let rt = Rc::downgrade(rt);
        Rc::new(move |msg| {
            let record = MessageRecord {
                sequence: next_delivery_sequence(&rt),
                ty: std::any::type_name::<M>().to_string(),
                json: serde_json::to_value(msg).expect("message value serializes"),
            };
            inner.borrow_mut().records.push(record);
        })
    }

    /// The absorb-side recorder: seed bytes are JSON on the wire, recorded as
    /// [`ABSORB_TY`] entries in the same totally-ordered log.
    pub(crate) fn absorb_recorder(&self, rt: &Rc<crate::runtime::RuntimeCore>) -> Rc<dyn Fn(&[u8])> {
        let inner = Rc::clone(&self.inner);
        let rt = Rc::downgrade(rt);
        Rc::new(move |bytes| {
            let json = serde_json::from_slice(bytes)
                .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned()));
            let record = MessageRecord {
                sequence: next_delivery_sequence(&rt),
                ty: ABSORB_TY.to_string(),
                json,
            };
            inner.borrow_mut().records.push(record);
        })
    }
}

/// Run a render closure to the [`LiveView`](crate::LiveView) it builds — the harness's way to
/// read the IR a `live_view!` compiles to without mounting. The closure (what `live_view!` emits)
/// is driven against a throwaway context; a childless view resolves in one poll. Panics if the
/// view has unresolved async children — IR-shape tests build childless views.
pub fn render_to_view<M, F, Fut>(build: F) -> crate::LiveView<M>
where
    M: 'static,
    F: FnOnce(crate::ctx::RenderScope<M>) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<crate::LiveView<M>, crate::Fault>>,
{
    let rt = Runtime::new();
    let ctx = rt.ctx::<M>();
    let scope = crate::ctx::RenderScope::new(ctx.inbox_sender(), ctx.context_handle().0);
    let mut fut = Box::pin(build(scope));
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(Ok(view)) => view,
        std::task::Poll::Ready(Err(error)) => panic!("render_to_view: the view failed: {error}"),
        std::task::Poll::Pending => {
            panic!("render_to_view: the view has unresolved async children (build a childless view)")
        }
    }
}

/// Mount a view in a throwaway runtime and fold its first paint to [`Template`] IR —
/// the harness's way to see what a mount produces (fold tests, style-merge tests).
/// Deliberately returns `Template`, not `View`: a first paint is a snapshot of a
/// live view, not content — its bindings died with the runtime here, so handing it
/// out as spliceable content would be the exact staleness this crate refuses.
pub fn paint<M: 'static>(view: crate::LiveView<M>) -> crate::template::Template {
    let mut rt = Runtime::new();
    let mut driver = crate::driver::CommandBufferDriver::new();
    let ctx = rt.ctx::<M>();
    // `render` is async now; a childless view resolves in one poll, posting its wired view to
    // the runtime's `pending_view`. Drive it on the throwaway runtime, then fold the first paint.
    rt.spawn(async move {
        let _ = ctx.render(|_| async move {
            ::std::result::Result::<_, crate::Fault>::Ok(view)
        }).await;
    });
    rt.run_once();
    rt.process_pending_view(&mut driver);
    rt.flush(&mut driver);
    let mut fold = crate::html::HtmlFold::new();
    for command in &driver.take_commands() {
        fold.apply(command);
    }
    fold.to_template()
}

pub fn replay_component<M, Fut, D, I>(
    runtime: &mut Runtime,
    driver: &mut D,
    root: impl FnOnce(Ctx<Setup, M>) -> Fut + 'static,
    messages: I,
) -> MessageLog
where
    M: Clone + Serialize + 'static,
    Fut: Future<Output = crate::Result> + 'static,
    D: DomDriver,
    I: IntoIterator<Item = M>,
{
    // Side effects are inert while this guard is held: the world is a fold of
    // `(seed, messages)`, and the recorded messages below stand in for anything a
    // client effect / tick / server response would have produced live.
    let _replay = crate::runtime::ReplayModeGuard::enter(runtime.core());

    let ctx = runtime.ctx::<M>();
    let sender = ctx.inbox_sender();
    let log = ctx.record_messages();

    runtime.spawn(spawn_live(root, ctx, report_to_log));
    runtime.run_once();
    runtime.process_pending_view(driver);
    runtime.flush(driver);

    for msg in messages {
        sender.send(msg);
        runtime.run_to_quiescence();
        runtime.process_pending_view(driver);
        runtime.flush(driver);
    }

    log
}
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    enum Msg {
        Add(u32),
        Done,
    }

    #[test]
    fn message_log_decodes_recorded_messages() {
        let rt = Runtime::new();
        let log = MessageLog::new();
        let recorder = log.recorder::<Msg>(rt.core());
        recorder(&Msg::Add(1));
        recorder(&Msg::Done);

        assert_eq!(log.decode::<Msg>().unwrap(), vec![Msg::Add(1), Msg::Done]);
    }

    #[test]
    fn replay_inputs_record_and_replay_time_and_random() {
        let inputs = ReplayInputs::recording();
        assert_eq!(inputs.now_millis(|| 10).unwrap(), 10);
        assert_eq!(inputs.random_u64(|| 20).unwrap(), 20);
        let records = inputs.records();

        let replay = ReplayInputs::replay(records);

        assert_eq!(replay.now_millis(|| 99).unwrap(), 10);
        assert_eq!(replay.random_u64(|| 99).unwrap(), 20);
    }

    #[test]
    fn replay_inputs_report_kind_mismatch() {
        let inputs = ReplayInputs::recording();
        inputs.now_millis(|| 10).unwrap();
        let replay = ReplayInputs::replay(inputs.records());

        let err = replay.random_u64(|| 20).unwrap_err();

        assert!(matches!(
            err,
            ReplayInputError::KindMismatch {
                expected: ReplayInputKind::RandomU64,
                actual: ReplayInputKind::NowMillis
            }
        ));
    }

    #[test]
    fn replay_inputs_report_exhaustion() {
        let replay = ReplayInputs::replay(Vec::new());

        let err = replay.now_millis(|| 10).unwrap_err();

        assert!(matches!(
            err,
            ReplayInputError::Exhausted {
                expected: ReplayInputKind::NowMillis
            }
        ));
    }
}
