//! ENG-21 — a log line says which request it belongs to.
//!
//! The daemon logged a great deal and correlated none of it. On a node
//! answering four requests and running sixteen jobs, the way to find out
//! what it did for one `meshnode run` was to read the log and guess by
//! timestamp — and the lines that mattered were the ones several layers
//! down, in a spawned task, where not even the job id was in scope.
//!
//! The fix is spans, and the reason there is a test for it is that spans
//! are invisible: nothing fails, no assertion breaks, and no reviewer
//! notices when an `.instrument()` is dropped from a spawned task and the
//! whole subtree silently loses its context. So this installs a subscriber
//! that records the span stack of every event, makes the node do some
//! work, and asserts the context is actually attached.
//!
//! **Why no OTLP exporter.** Spans cost nothing here — `tracing` is
//! already a dependency — but exporting them over OTLP pulls in
//! `opentelemetry`, `opentelemetry-otlp` and a gRPC or HTTP client, which
//! is a large dependency tree and a large binary for a product whose pitch
//! is one small static binary. The decision (2026-09-17) is spans plus
//! structured logs: under systemd the daemon already writes records rather
//! than lines, so `journalctl -o json` hands an operator's collector these
//! fields with nothing new in the build.
#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mesh_proto::{IpcRequest, JobPriority};
use mesh_testkit::{founder_user_key, spawn_founder_with};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

/// One recorded event: its message, and the fields of every span it was
/// emitted inside, innermost last.
#[derive(Debug, Clone)]
struct Record {
    message: String,
    /// `span name` → its fields, flattened across the whole stack. Flat
    /// because that is how a log line reads it: `trace=…` from the request
    /// span and `job=…` from the job span land side by side on the line.
    context: HashMap<String, String>,
}

#[derive(Default)]
struct Fields(HashMap<String, String>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            format!("{value:?}").replace('"', ""),
        );
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_string(), value.to_string());
    }
}

/// Records every event with the fields of the spans it happened inside,
/// and every span as it closes.
///
/// Spans are collected separately because a span that emits no event of
/// its own still has to be *there*: a request the node answers without
/// logging anything has a trace id, and counting only events would judge
/// the correlation on whichever handlers happen to be chatty.
#[derive(Clone, Default)]
struct Capture {
    events: Arc<Mutex<Vec<Record>>>,
    spans: Arc<Mutex<Vec<Record>>>,
}

impl<S> Layer<S> for Capture
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::Id,
        ctx: Context<'_, S>,
    ) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(fields);
        }
    }

    /// `Span::record` after the fact — how `req` and `user` are filled in,
    /// once the envelope has verified and the payload has decoded.
    fn on_record(&self, id: &tracing::Id, values: &tracing::span::Record<'_>, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut ext = span.extensions_mut();
            if let Some(fields) = ext.get_mut::<Fields>() {
                values.record(fields);
            }
        }
    }

    /// Recorded on close, not on creation: `req` and `user` are filled in
    /// after the fact, and a span snapshotted at birth would be missing
    /// exactly the fields this test is about.
    fn on_close(&self, id: tracing::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(&id) {
            let context = span
                .extensions()
                .get::<Fields>()
                .map(|f| f.0.clone())
                .unwrap_or_default();
            self.spans.lock().unwrap().push(Record {
                message: span.name().to_string(),
                context,
            });
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        let mut own = Fields::default();
        event.record(&mut own);
        let mut context = HashMap::new();
        if let Some(scope) = ctx.event_scope(event) {
            // Outermost first, so an inner span's field wins if both
            // carry one — which is what a log line shows.
            for span in scope.from_root() {
                if let Some(fields) = span.extensions().get::<Fields>() {
                    context.extend(fields.0.clone());
                }
            }
        }
        context.extend(own.0.clone());
        self.events.lock().unwrap().push(Record {
            message: own.0.get("message").cloned().unwrap_or_default(),
            context,
        });
    }
}

impl Capture {
    fn install() -> Capture {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(Registry::default().with(capture.clone()))
            .expect("no other subscriber may be installed in this binary");
        capture
    }

    fn records(&self) -> Vec<Record> {
        self.events.lock().unwrap().clone()
    }

    /// Closed spans, by name.
    fn spans(&self, name: &str) -> Vec<Record> {
        self.spans
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.message == name)
            .cloned()
            .collect()
    }
}

/// One test, one binary: the subscriber is process-global, so a second
/// test here would race the first to install it. Everything ENG-21's
/// tracing half claims is asserted in this one run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_and_the_job_it_starts_are_both_correlated() {
    let capture = Capture::install();
    let node = spawn_founder_with("TRACE", "eng21-trace", vec![], |c| c.limits.slots = 2).await;
    let user = founder_user_key(&node);

    // A plain request, and then one that starts real work. The first
    // proves the request span exists at all; the second proves the
    // context survives into the task the work runs in, which is the part
    // that silently breaks.
    let _ = mesh_testkit::ipc_as(&node, user, IpcRequest::Jobs).await;
    let mut frames = mesh_testkit::stream_as(
        &node,
        user,
        IpcRequest::Run {
            command: "true".into(),
            on: None,
            env: vec![],
            timeout_secs: Some(60),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    while frames.recv().await.is_some() {}
    let finished = mesh_testkit::wait_until(Duration::from_secs(30), || {
        node.node.jobs().running_count() == 0 && node.node.jobs().queued_count() == 0
    })
    .await;
    assert!(finished, "the job never finished");
    // The "job finished" line is emitted from the spawned task, after the
    // request that started it has already been answered.
    let saw_finish = mesh_testkit::wait_until(Duration::from_secs(10), || {
        capture
            .records()
            .iter()
            .any(|r| r.message == "job finished")
    })
    .await;
    assert!(saw_finish, "no job ever reported finishing");

    let records = capture.records();

    // 1. Every request is traceable, and says what it was.
    let traced: Vec<&Record> = records
        .iter()
        .filter(|r| r.context.contains_key("trace"))
        .collect();
    assert!(
        !traced.is_empty(),
        "nothing was emitted inside a request span"
    );
    let kinds: Vec<&String> = traced.iter().filter_map(|r| r.context.get("req")).collect();
    assert!(
        kinds.iter().any(|k| k.as_str() == "Run"),
        "no line was attributed to the Run request; saw {kinds:?}"
    );

    // 2. Two different requests do not share a trace id. Without this the
    //    id could be minted once per process and every assertion above
    //    would still pass, while correlating nothing.
    //
    //    Counted over spans rather than over events, because a request
    //    that logs nothing still has to be traceable — and `Jobs` is
    //    exactly such a request.
    //
    //    Waited for: a span is recorded when it closes, and the Run
    //    request's span lives on in the job task it started, so it closes
    //    a moment after "job finished" — on a slow runner, after this line.
    mesh_testkit::wait_until(Duration::from_secs(10), || capture.spans("ipc").len() >= 2).await;
    let requests = capture.spans("ipc");
    assert!(
        requests.len() >= 2,
        "both requests should have opened a span, saw {}",
        requests.len()
    );
    let ids: std::collections::HashSet<&String> = requests
        .iter()
        .filter_map(|r| r.context.get("trace"))
        .collect();
    assert_eq!(
        ids.len(),
        requests.len(),
        "each request needs its own trace id, saw {ids:?}"
    );
    let named: std::collections::HashSet<&String> = requests
        .iter()
        .filter_map(|r| r.context.get("req"))
        .collect();
    assert!(
        named.contains(&"Jobs".to_string()) && named.contains(&"Run".to_string()),
        "both requests should have named themselves, saw {named:?}"
    );

    // 3. The job's own line carries the job id, from a task the request
    //    that started it has already outlived. This is the assertion that
    //    fails if an `.instrument()` is ever dropped from the spawn.
    let finish = records
        .iter()
        .find(|r| r.message == "job finished")
        .expect("checked above");
    assert!(
        finish.context.contains_key("job"),
        "the job's own completion line has no job id: {:?}",
        finish.context
    );
    assert!(
        finish.context.contains_key("user"),
        "…nor who submitted it: {:?}",
        finish.context
    );

    // 4. …and it still carries the trace id of the request that asked for
    //    it, because the job span opens inside the request span and stays
    //    its child for as long as the job runs. This is the whole thing in
    //    one assertion: one id, from `meshnode run` to the process exiting
    //    several layers and two spawned tasks later.
    let run = requests
        .iter()
        .find(|r| r.context.get("req").map(String::as_str) == Some("Run"))
        .expect("checked above");
    assert_eq!(
        finish.context.get("trace"),
        run.context.get("trace"),
        "the job's line should carry the trace of the request that started it"
    );

    node.stop().await;
}
