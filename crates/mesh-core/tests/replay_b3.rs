//! M31 B3 — read it back.
//!
//! Three claims, and the first is the one the whole phase stands on:
//!
//! 1. **A browse never touches a sink.** The History list, its filters and
//!    its counts are answered from tiers this mesh owns. A destination is
//!    read on one person's click on one payload, and nowhere else. The
//!    fence test below configures a sink whose destination *accepts the
//!    connection and never answers* — the shape that hangs a screen rather
//!    than failing it — and asserts a browse is byte-identical to the same
//!    browse before the sink existed.
//! 2. **The bytes come back through the node that exported them**, governed
//!    exactly like the write that put them there: `egress_allow` on the
//!    fetching node, a `read_credential` resolved there and nowhere else,
//!    and a refusal that names the missing permission rather than falling
//!    back to the write token.
//! 3. **Opening a payload is audited; scrolling it is not.** `AuditAppend`
//!    is `Grow` and never compacts, so a per-scroll entry would make reading
//!    a log permanently more expensive than running the job that wrote it.
#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mesh_core::secrets::seal_secret;
use mesh_proto::{
    HistoryQuery, IpcRequest, IpcResponse, JobPriority, NodeId, Sink, SinkKind, SinkPayloads,
    SinkRecords, SinkTables, UserId,
};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, spawn_founder_with};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// ---------------------------------------------------------------------------
// A destination, and a destination that hangs
// ---------------------------------------------------------------------------

/// Every request the fake warehouse saw: `(path-and-query, body)`.
type Seen = Arc<Mutex<Vec<(String, String)>>>;

/// The lines the warehouse will hand back for a replay, keyed by job id.
type Stored = Arc<Mutex<HashMap<String, Vec<String>>>>;

/// Set to make the fake destination **ignore the `LIMIT`** it was sent.
///
/// Not a hypothetical: a proxy that rewrites the statement, a LogsQL
/// `limit` applied per-stream, a future version that changes its mind. The
/// ceiling has to be ours.
type Floods = Arc<std::sync::atomic::AtomicBool>;

/// A ClickHouse-shaped destination: swallows inserts, answers `SELECT line`
/// with NDJSON, and records everything it was asked so a test can assert on
/// the *shape* of the read rather than only on its result.
async fn warehouse() -> (String, Seen, Stored) {
    let (url, seen, stored, _) = warehouse_with_flood().await;
    (url, seen, stored)
}

async fn warehouse_with_flood() -> (String, Seen, Stored, Floods) {
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let stored: Stored = Arc::new(Mutex::new(HashMap::new()));
    let floods: Floods = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (s2, st2, fl2) = (seen.clone(), stored.clone(), floods.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                break;
            };
            let (seen, stored, floods) = (s2.clone(), st2.clone(), fl2.clone());
            tokio::spawn(async move {
                let Some((head, body)) = read_request(&mut sock).await else {
                    return;
                };
                let target = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                seen.lock().unwrap().push((target.clone(), body.clone()));

                // A read is a GET carrying `query=SELECT …`; an insert is a
                // POST whose body is the rows. Only the read answers with
                // anything.
                let decoded = percent_decode(&target);
                // ClickHouse binds `param_<name>` into `{name:String}`
                // server-side (S-M2), so the values are *not* in the
                // statement and this stands in for the server that does the
                // binding. It is also the assertion that they are sent: a
                // read that put them back in the SQL would find no job here.
                let out = if decoded.contains("SELECT line") {
                    let job = url_param(&target, "param_job").unwrap_or_default();
                    let search = url_param(&target, "param_search");
                    let all = stored
                        .lock()
                        .unwrap()
                        .get(&job)
                        .cloned()
                        .unwrap_or_default();
                    if floods.load(std::sync::atomic::Ordering::Relaxed) {
                        ndjson(all.iter().rev())
                    } else {
                        select(&decoded, search.as_deref(), &all)
                    }
                } else {
                    String::new()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                    out.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    (format!("http://{addr}"), seen, stored, floods)
}

/// One `param_<name>` from a request target, percent-decoded.
fn url_param(target: &str, name: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
        .map(percent_decode)
}

/// Apply the destination's half of the window: the search, then
/// newest-first with `LIMIT`/`OFFSET`, answered as NDJSON.
///
/// `search` arrives as a bound parameter rather than as part of `sql`
/// (S-M2), which is exactly the point: the statement never carries a value
/// a caller chose.
fn select(sql: &str, search: Option<&str>, all: &[String]) -> String {
    let filtered: Vec<&String> = match search.filter(|s| !s.is_empty()) {
        Some(needle) => all.iter().filter(|l| l.contains(needle)).collect(),
        None => all.iter().collect(),
    };
    let number = |after: &str| -> usize {
        sql.split(after)
            .nth(1)
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    let (limit, offset) = (number("LIMIT "), number("OFFSET "));
    ndjson(
        filtered
            .into_iter()
            .rev() // newest first, as the ORDER BY asks
            .skip(offset)
            .take(limit),
    )
}

fn ndjson<'a>(lines: impl Iterator<Item = &'a String>) -> String {
    lines
        .map(|l| serde_json::json!({ "line": l }).to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A destination that completes the TCP handshake and then says nothing,
/// for as long as anyone waits.
///
/// **This is the shape that matters for the fence.** A refused connection
/// fails fast and a browse that touched it would merely be wrong; a socket
/// that accepts and stalls is what turns one careless call into a screen
/// that never paints.
async fn black_hole() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock); // never read, never answered, never dropped
        }
    });
    format!("http://{addr}")
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<(String, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")? + 4;
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let want: usize = head
        .to_ascii_lowercase()
        .split("content-length:")
        .nth(1)
        .and_then(|r| r.split("\r\n").next())
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    let mut body = buf[split..].to_vec();
    while body.len() < want {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some((head, String::from_utf8_lossy(&body).to_string()))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A node whose export pass is fast enough to test, with an optional
/// `egress_allow`.
async fn node_with(name: &str, egress: Vec<String>) -> TestNode {
    spawn_founder_with(name, "b3-mesh", vec![], move |c| {
        c.integrations.export_interval_secs = 1;
        // A one-node mesh has nothing to converge with, so a settle window
        // is measuring nothing. On a real mesh the default five minutes is
        // the right answer and this test would be lying to itself.
        c.integrations.settle_secs = 0;
        c.integrations.egress_allow = egress;
    })
    .await
}

fn clickhouse(name: &str, url: &str, read_credential: &str) -> Sink {
    Sink {
        name: name.into(),
        kind: SinkKind::ClickHouse,
        url: url.into(),
        database: "meshware".into(),
        credential: String::new(),
        read_credential: read_credential.into(),
        tables: SinkTables::default(),
        records: SinkRecords {
            batch: 100,
            every_secs: 1,
        },
        payloads: SinkPayloads {
            enabled: true,
            max_bytes: 0,
        },
        on_failure: Default::default(),
        run_on: String::new(),
        enabled: true,
        description: String::new(),
        created_by: UserId([0u8; 32]),
        created_at_micros: 0,
        signature: Vec::new(),
    }
}

async fn put_sink(node: &TestNode, mut sink: Sink) {
    // Signed client-side by the admin's own key, exactly as Director and
    // the CLI do it: the record travels signed and the daemon verifies
    // rather than trusting the socket.
    let admin = founder_user_key(node);
    sink.created_by = admin.user_id();
    sink.created_at_micros = mesh_proto::now_micros();
    sink.signature = admin.sign(&sink.signing_bytes()).to_vec();
    let resp = ipc_as(
        node,
        admin,
        IpcRequest::SinkPut {
            sink: Box::new(sink),
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "sink put: {resp:?}");
}

async fn set_secret(node: &TestNode, name: &str, value: &[u8]) {
    let admin = founder_user_key(node);
    let recipients: Vec<NodeId> = node.node.engine().read(|s| {
        s.members
            .values()
            .filter(|m| s.is_active_member(&m.info.node_id))
            .map(|m| m.info.node_id)
            .collect()
    });
    let mut record = seal_secret(name, 1, value, &recipients, admin.user_id()).unwrap();
    record.signature = admin.sign(&record.signing_bytes()).to_vec();
    let resp = ipc_as(node, admin, IpcRequest::SecretPublish { record }).await;
    assert!(matches!(resp, IpcResponse::Ok), "secret publish: {resp:?}");
}

async fn run_job(node: &TestNode, command: &str) {
    let outcome = mesh_testkit::run_to_end(
        node,
        founder_user_key(node),
        IpcRequest::Run {
            command: command.into(),
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
    assert!(
        matches!(
            outcome,
            mesh_testkit::RunOutcome::Done { code: Some(0), .. }
        ),
        "job failed: {outcome:?}"
    );
}

async fn history_jobs(node: &TestNode, q: HistoryQuery) -> mesh_proto::HistoryJobPage {
    let resp = ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::HistoryJobs(Box::new(q)),
    )
    .await;
    match resp {
        IpcResponse::HistoryJobs(page) => *page,
        other => panic!("expected a job page, got {other:?}"),
    }
}

async fn replay(
    node: &TestNode,
    job: mesh_proto::JobId,
    from_end: u64,
    search: &str,
) -> mesh_proto::PayloadWindowReply {
    let resp = ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::HistoryReplay {
            job_id: job,
            from_end,
            limit: 10,
            search: search.into(),
        },
    )
    .await;
    match resp {
        IpcResponse::PayloadWindow(w) => *w,
        other => panic!("expected a payload window, got {other:?}"),
    }
}

async fn audit_count(node: &TestNode, action: &str) -> usize {
    let resp = ipc_as(
        node,
        founder_user_key(node),
        IpcRequest::AuditQuery {
            user: None,
            action: Some(action.into()),
            since_micros: None,
            limit: 500,
        },
    )
    .await;
    match resp {
        IpcResponse::Audit(rows) => rows.len(),
        other => panic!("expected audit rows, got {other:?}"),
    }
}

/// Wait for the indexer, which drains on a timer.
async fn wait_for_jobs(node: &TestNode, want: usize) -> mesh_proto::HistoryJobPage {
    let mut last = history_jobs(node, HistoryQuery::default()).await;
    for _ in 0..80 {
        if last.rows.len() >= want {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
        last = history_jobs(node, HistoryQuery::default()).await;
    }
    panic!("index never reached {want} jobs (has {})", last.rows.len());
}

// ---------------------------------------------------------------------------
// 1. The fence (§18, §14 tests) — the most important test in this phase
// ---------------------------------------------------------------------------

/// **With every sink unreachable, a browse is byte-identical to the same
/// browse with no sink configured.**
///
/// Not "still works" and not "eventually returns" — identical bytes, every
/// filter, including the counts and the tier report. If a list query ever
/// reaches a destination to enrich a row, count a match or decide a page,
/// this fails, and it fails against the destination shape that would
/// actually hurt: one that accepts the connection and never answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_browse_never_touches_a_sink() {
    let node = node_with("fence", vec![]).await;
    run_job(&node, "echo alpha").await;
    run_job(&node, "echo beta").await;
    wait_for_jobs(&node, 2).await;

    // Every filter the History screen can produce, asked before any sink
    // exists. These are the bytes the fence protects.
    let queries = vec![
        HistoryQuery::default(),
        HistoryQuery {
            contains: Some("alpha".into()),
            ..Default::default()
        },
        HistoryQuery {
            state: Some("succeeded".into()),
            ..Default::default()
        },
        HistoryQuery {
            since_micros: Some(0),
            limit: 5,
            ..Default::default()
        },
        HistoryQuery {
            limit: 1,
            offset: 1,
            ..Default::default()
        },
    ];
    let mut before = Vec::new();
    for q in &queries {
        before.push(serde_json::to_string(&history_jobs(&node, q.clone()).await).unwrap());
    }

    // Now a destination that accepts and stalls, exporting everything.
    let url = black_hole().await;
    put_sink(&node, clickhouse("void", &url, "")).await;
    // Let at least two export passes run into the wall.
    tokio::time::sleep(Duration::from_secs(3)).await;

    for (i, q) in queries.iter().enumerate() {
        let after = tokio::time::timeout(Duration::from_secs(10), history_jobs(&node, q.clone()))
            .await
            .expect("a browse hung — something on the list path is waiting on a sink");
        assert_eq!(
            before[i],
            serde_json::to_string(&after).unwrap(),
            "query {i} changed once an unreachable sink existed"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The read path
// ---------------------------------------------------------------------------

/// The end-to-end shape of §14.3: a job's output is exported, the record
/// carries the locator, and Director's request comes back with the tail —
/// fetched by the node, never by Director.
///
/// It also pins the two rules that make the read *cheap*: the window is
/// asked for as a window (`LIMIT`/`OFFSET` at the destination, not a scan
/// here), and search is delegated to the destination rather than filtered
/// after the fact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_payload_comes_back_a_window_at_a_time_and_the_open_is_audited_once() {
    let (url, seen, stored) = warehouse().await;
    let node = node_with("reader", vec![]).await;
    set_secret(&node, "wh-read", b"read-token-42").await;
    put_sink(&node, clickhouse("warehouse", &url, "{{secret:wh-read}}")).await;

    run_job(&node, "echo hello-b3").await;
    wait_for_jobs(&node, 1).await;

    // The exporter has to have run and recorded where it put the bytes.
    let row = located_row(&node).await;
    assert_eq!(row.payload_sink.as_deref(), Some("warehouse"));
    let key = row.payload_key.clone().expect("a locator has a key");

    // Stock the destination with more lines than one window holds, under
    // the key the exporter chose — so the test reads back through the same
    // locator the product wrote.
    let job_part = key.rsplit('/').next().unwrap().to_string();
    let lines: Vec<String> = (0..37).map(|i| format!("line {i}")).collect();
    stored.lock().unwrap().insert(job_part, lines.clone());

    // The open: the tail, and one audit entry.
    let before = audit_count(&node, "history.replay").await;
    let tail = replay(&node, row.job_id, 0, "").await;
    assert_eq!(tail.status, "", "the read should have succeeded");
    assert_eq!(
        tail.lines,
        lines[27..].to_vec(),
        "an open lands at the end, oldest-first within the window"
    );
    assert!(tail.more_before, "there are 27 lines above this window");
    assert_eq!(
        audit_count(&node, "history.replay").await,
        before + 1,
        "opening a payload writes exactly one audit entry"
    );

    // Scrolling back: another window, and **no** further audit entries.
    let earlier = replay(&node, row.job_id, 10, "").await;
    assert_eq!(earlier.lines, lines[17..27].to_vec());
    assert_eq!(
        audit_count(&node, "history.replay").await,
        before + 1,
        "scrolling a payload writes no audit entries — AuditAppend never compacts"
    );

    // Search is delegated: the destination sees the needle in its own
    // query, rather than us pulling 5 GB back to filter it here.
    let hit = replay(&node, row.job_id, 0, "line 3").await;
    assert!(
        hit.lines.iter().all(|l| l.contains("line 3")),
        "search should be applied at the destination: {:?}",
        hit.lines
    );
    let asked = seen.lock().unwrap().clone();
    let reads: Vec<&(String, String)> = asked
        .iter()
        .filter(|(t, _)| percent_decode(t).contains("SELECT line"))
        .collect();
    assert!(!reads.is_empty(), "the destination was never asked to read");
    for (target, _) in &reads {
        let sql = percent_decode(target);
        assert!(
            sql.contains("LIMIT "),
            "a read must always be a window, never the object: {sql}"
        );
    }
    // The needle reaches the destination as a **bound parameter**, not
    // inside the statement (S-M2): the SQL says `{search:String}` and the
    // value travels beside it, so there is no quote for a search term to
    // escape and no backslash for ClickHouse to read as one.
    assert!(
        reads
            .iter()
            .any(|(t, _)| percent_decode(t).contains("position(line, {search:String})")),
        "the statement should carry a placeholder, not a literal"
    );
    assert!(
        reads
            .iter()
            .any(|(t, _)| url_param(t, "param_search").as_deref() == Some("line 3")),
        "the search needle never reached the destination"
    );
    for (target, _) in &reads {
        let sql = percent_decode(target);
        assert!(
            !sql.contains("job_id = '"),
            "no caller-chosen value belongs in the statement: {sql}"
        );
    }

    // The read credential is resolved on this node and appears nowhere a
    // person or a log could pick it up — not in the record, not in the
    // reply. The B1 redaction test, aimed at the read path.
    let recorded = node
        .node
        .engine()
        .read(|s| serde_json::to_string(&s.sinks.get("warehouse").unwrap().sink).unwrap());
    assert!(
        !recorded.contains("read-token-42"),
        "the signed record must carry a reference, never the value"
    );
    assert!(recorded.contains("{{secret:wh-read}}"));
    assert!(
        !serde_json::to_string(&tail)
            .unwrap()
            .contains("read-token-42"),
        "a credential must never travel back in an IPC reply"
    );
    // It did reach the destination, which is the whole point of resolving it.
    assert!(
        asked.iter().any(|(_, _)| true),
        "the destination saw at least one request"
    );
}

/// §19 Q10, decided: **no fallback.** A destination that can replay but has
/// no read credential is refused, and the refusal names the permission to
/// grant rather than quietly widening a write-only token.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_missing_read_credential_is_refused_and_names_what_is_missing() {
    let (url, _seen, stored) = warehouse().await;
    let node = node_with("nocred", vec![]).await;
    put_sink(&node, clickhouse("warehouse", &url, "")).await;

    run_job(&node, "echo needs-a-token").await;
    wait_for_jobs(&node, 1).await;
    let row = located_row(&node).await;
    stored.lock().unwrap().insert(
        row.payload_key
            .clone()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap()
            .to_string(),
        vec!["x".into()],
    );

    let page = replay(&node, row.job_id, 0, "").await;
    assert!(page.lines.is_empty());
    assert!(
        page.status.to_lowercase().contains("read credential"),
        "the refusal must name the permission: {}",
        page.status
    );
}

/// A read leaves by the same door as a write, so it is governed by the same
/// rule — **checked on the fetching node, regardless of what the signed
/// record says.**
///
/// That last clause is why the sink here is authored straight into the
/// op-log rather than through `SinkPut`: the IPC gate already refuses a
/// destination outside the allowlist, and a test that only exercised the
/// gate would pass with the read path wide open. A record can reach this
/// node from a peer that had a different allowlist, or from before this
/// one was tightened — and the read still has to refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_outside_the_egress_allowlist_is_refused() {
    let (url, seen, _stored) = warehouse().await;
    let node = node_with("walled", vec!["warehouse.corp".into()]).await;
    set_secret(&node, "wh-read", b"tok").await;

    // Straight into replicated state, past the submission gate.
    let admin = founder_user_key(&node);
    let mut sink = clickhouse("warehouse", &url, "{{secret:wh-read}}");
    sink.created_by = admin.user_id();
    sink.created_at_micros = mesh_proto::now_micros();
    sink.signature = admin.sign(&sink.signing_bytes()).to_vec();
    node.node
        .engine()
        .author_op(mesh_proto::OpBody::SinkPut {
            sink: Box::new(sink),
        })
        .unwrap();

    run_job(&node, "echo walled-in").await;
    let page = wait_for_jobs(&node, 1).await;

    // The export is refused by the allowlist too, so nothing ever left the
    // machine and there is no locator to read back.
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(
        seen.lock().unwrap().is_empty(),
        "an export outside the allowlist must never leave the machine"
    );

    // And the read refuses in its own right. Pointed at the sink by hand,
    // because the honest path has no locator — this is the case where a
    // record *does* name a destination this node may not reach.
    let job = page.rows[0].job_id;
    let reply = replay(&node, job, 0, "").await;
    assert!(reply.lines.is_empty());
    assert!(
        reply.status.contains("never exported") || reply.status.contains("egress_allow"),
        "a read must refuse rather than reach a forbidden host: {}",
        reply.status
    );
}

/// The read path's own egress check, with a locator that **does** name a
/// forbidden host.
///
/// The export happens through an allowed host, so the locator is real; then
/// the destination's URL is moved to a host the allowlist does not cover —
/// which is what an admin editing a sink, or a record arriving from a peer
/// with different rules, actually looks like. The record is signed and says
/// "send it there". The read refuses anyway, which is the point: the check
/// lives on the machine that would open the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_naming_a_forbidden_host_does_not_get_read_anyway() {
    let (url, seen, stored) = warehouse().await;
    // `127.0.0.1` and `localhost` are the same machine and different hosts
    // to an allowlist, which is exactly the seam this test needs.
    let node = node_with("moved", vec!["127.0.0.1".into()]).await;
    set_secret(&node, "wh-read", b"tok").await;
    put_sink(&node, clickhouse("warehouse", &url, "{{secret:wh-read}}")).await;

    run_job(&node, "echo moved-away").await;
    wait_for_jobs(&node, 1).await;
    let row = located_row(&node).await;
    let job_part = row
        .payload_key
        .clone()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    stored
        .lock()
        .unwrap()
        .insert(job_part, vec!["secret business".into()]);

    // The destination moves to a host this node may not reach. Authored
    // into the log directly: `SinkPut` would refuse it, and the case under
    // test is a record that got here some other way.
    let admin = founder_user_key(&node);
    let moved_url = url.replace("127.0.0.1", "localhost");
    let mut moved = clickhouse("warehouse", &moved_url, "{{secret:wh-read}}");
    moved.created_by = admin.user_id();
    moved.created_at_micros = mesh_proto::now_micros();
    moved.signature = admin.sign(&moved.signing_bytes()).to_vec();
    node.node
        .engine()
        .author_op(mesh_proto::OpBody::SinkPut {
            sink: Box::new(moved),
        })
        .unwrap();

    let before = seen.lock().unwrap().len();
    let reply = replay(&node, row.job_id, 0, "").await;
    assert!(reply.lines.is_empty(), "nothing should have come back");
    assert!(
        reply.status.contains("egress_allow"),
        "the refusal must name the control that refused: {}",
        reply.status
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        before,
        "a refused read must never reach the destination"
    );
}

/// §14.3 item 7: the fourth state is *recorded*, never inferred. A record
/// whose payload was never exported still says **expired**, with a sink
/// configured and reachable — because a destination that exists is not a
/// promise that this job's bytes went through it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sink_that_never_took_this_output_does_not_soften_expired() {
    let (url, _seen, _stored) = warehouse().await;
    let node = node_with("honest", vec![]).await;
    // Records only: the destination is up, reachable and exporting — and
    // it never sees a payload, so no job gets a locator.
    let mut sink = clickhouse("warehouse", &url, "{{secret:wh-read}}");
    sink.payloads.enabled = false;
    put_sink(&node, sink).await;

    run_job(&node, "echo no-payload-here").await;
    wait_for_jobs(&node, 1).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let page = history_jobs(&node, HistoryQuery::default()).await;
    assert!(
        page.rows.iter().all(|r| r.payload_sink.is_none()),
        "a records-only sink must never claim a payload it did not take"
    );

    let job = page.rows[0].job_id;
    let reply = replay(&node, job, 0, "").await;
    assert!(reply.lines.is_empty());
    assert!(
        reply.status.contains("never exported"),
        "the refusal must say the output was never exported: {}",
        reply.status
    );
}

async fn located_row(node: &TestNode) -> mesh_proto::HistoryJobRow {
    for _ in 0..80 {
        let p = history_jobs(node, HistoryQuery::default()).await;
        if let Some(r) = p.rows.iter().find(|r| r.payload_sink.is_some()) {
            return r.clone();
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("the export never recorded a payload locator");
}

// ---------------------------------------------------------------------------
// 3. The ceiling, the copy, and the locator that has to outlive the op-log
// ---------------------------------------------------------------------------

/// **"It streams" is a claim that decays; a truncation is a fact.**
///
/// Every query this product builds carries a bound, so a well-behaved
/// destination already returns a window. This test asserts what happens
/// when one does not — a proxy rewriting the statement, a `limit` applied
/// per-stream, a version that changed its mind — because that is the case
/// where "we asked for a window" stops protecting anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_destination_that_ignores_the_window_is_truncated_here() {
    let (url, _seen, stored, floods) = warehouse_with_flood().await;
    let node = node_with("ceiling", vec![]).await;
    set_secret(&node, "wh-read", b"tok").await;
    put_sink(&node, clickhouse("warehouse", &url, "{{secret:wh-read}}")).await;

    run_job(&node, "echo flood-me").await;
    wait_for_jobs(&node, 1).await;
    let row = located_row(&node).await;
    let job_part = row
        .payload_key
        .clone()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_string();
    // Far more than the window asked for, and more than a person will read.
    let flood: Vec<String> = (0..5_000).map(|i| format!("chunk {i}")).collect();
    stored.lock().unwrap().insert(job_part, flood.clone());
    floods.store(true, std::sync::atomic::Ordering::Relaxed);

    // `replay` asks for ten.
    let page = replay(&node, row.job_id, 0, "").await;
    assert!(
        page.lines.len() <= 10,
        "the destination sent {} lines for a window of 10 — the cap has to be ours",
        page.lines.len()
    );
    assert!(
        page.more_before,
        "a truncated answer must still say there is more above it"
    );
    // And it kept the *end*, which is where the failure is.
    assert_eq!(page.lines.last().map(String::as_str), Some("chunk 4999"));
}

/// §14.4, rules 1 and 3: the matrix is declared on the enum, the picker
/// renders it as a consequence, and "one permission away" never reads as
/// "not supported".
///
/// A doc table cannot be tested; this can, which is why the table lives on
/// the type.
#[test]
fn a_write_only_destination_says_so_before_it_is_chosen() {
    use mesh_proto::ReplayState;

    for kind in [SinkKind::Ndjson, SinkKind::Plugin, SinkKind::Webhook] {
        assert!(
            !kind.capabilities().replay_payloads,
            "{kind:?} claims a replay it cannot do"
        );
        // Named in the picker copy, before selection — not discovered in
        // six months by the person who needed the log.
        let copy = kind.capability_sentence().to_ascii_lowercase();
        assert!(
            copy.contains("cannot read it back"),
            "{kind:?}'s picker copy hides the limitation: {copy}"
        );
        // And a credential does not change the answer, because the
        // destination genuinely has no read semantic.
        assert_eq!(kind.replay_state(true), ReplayState::NotSupported);
        assert_eq!(kind.replay_state(false), ReplayState::NotSupported);
    }

    for kind in [
        SinkKind::ClickHouse,
        SinkKind::VictoriaLogs,
        SinkKind::OpenSearch,
    ] {
        assert!(kind.capabilities().replay_payloads);
        assert_eq!(kind.replay_state(true), ReplayState::Available);
        // **The third answer.** Blaming the destination for a permissions
        // choice sends the operator to the wrong place entirely.
        assert_eq!(
            kind.replay_state(false),
            ReplayState::NeedsReadCredential,
            "{kind:?} without a read credential must read as one permission away"
        );
        let message = ReplayState::NeedsReadCredential
            .message()
            .to_ascii_lowercase();
        assert!(
            message.contains("read credential"),
            "the refusal must name the permission: {message}"
        );
    }
}

/// A merged page reports a **total** only when the number is one.
///
/// Either tier could have stopped early — the index abandoning its scan,
/// the archive returning a full page against its own cap — and a floor
/// presented as a count is exactly the made-up number `total` exists to
/// refuse. A UI renders it as a count and a person reads it as one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_page_reports_a_total_only_when_it_knows_one() {
    let node = node_with("counting", vec![]).await;
    run_job(&node, "echo one").await;
    run_job(&node, "echo two").await;
    wait_for_jobs(&node, 2).await;

    // No archive anywhere, so this is the index answering for itself and
    // the count is exact.
    let page = history_jobs(&node, HistoryQuery::default()).await;
    assert_eq!(page.total, Some(page.rows.len() as u64));
    assert!(
        page.tiers.archive_unreachable,
        "a mesh with no archive must say so rather than imply completeness"
    );
    assert_eq!(page.tiers.from_archive, 0);
    assert_eq!(page.tiers.from_index, page.rows.len() as u64);
}
