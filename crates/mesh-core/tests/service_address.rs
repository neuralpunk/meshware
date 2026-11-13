//! A registered instance is reached where it runs: at its node's address, on
//! the port that node gave it. There is no proxy and no address of its own in
//! front of it — from another machine, the registry (and `web.mesh` SRV) is
//! the whole answer, and a stable name and port is a load balancer's job.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use mesh_proto::{IpcRequest, IpcResponse, RegistryInstanceRow, Runtime};
use mesh_testkit::{founder_user_key, wait_until};

fn bundle(files: &[(&str, &str, u32)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("bundle dir");
    for (path, content, mode) in files {
        let full = dir.path().join(path);
        std::fs::write(&full, content).expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&full, std::fs::Permissions::from_mode(*mode)).expect("chmod");
        }
    }
    dir
}

/// `service`'s instances as `asker` lists them, asked as the mesh's founder.
async fn instances(
    asker: &mesh_testkit::TestNode,
    founder: &mesh_testkit::TestNode,
    service: &str,
) -> Vec<RegistryInstanceRow> {
    match mesh_testkit::ipc_as(
        asker,
        founder_user_key(founder),
        IpcRequest::RegistryShow {
            service: service.into(),
        },
    )
    .await
    {
        IpcResponse::RegistryShow(rows) => rows,
        other => panic!("registry show: {other:?}"),
    }
}

/// GET / over a plain TCP connection to `addr`, and the whole reply.
///
/// Async, and it has to be: a `#[tokio::test]` runs on one thread, and a
/// blocking read there starves the very nodes whose answer it is waiting for.
async fn get(addr: &str) -> Result<String, String> {
    let exchange = async {
        let mut stream = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| e.to_string())?;
        stream
            .write_all(b"GET / HTTP/1.0\r\n\r\n")
            .await
            .map_err(|e| e.to_string())?;
        let mut reply = Vec::new();
        stream
            .read_to_end(&mut reply)
            .await
            .map_err(|e| e.to_string())?;
        Ok::<_, String>(String::from_utf8_lossy(&reply).into_owned())
    };
    tokio::time::timeout(Duration::from_secs(3), exchange)
        .await
        .map_err(|_| "no reply in 3s".to_string())?
}

/// A node clears a record that lists copies it no longer runs (2026-09-14).
///
/// Only a node may replace its own registry record, and a node published
/// only when what it ran changed — so a node that came back from a restart
/// running nothing never said so, and the copies its last record named stood
/// in every catalogue for good. Found on a real mesh as a failed copy of
/// `web` still listed as passing. Stood up here directly: a record naming a
/// copy this node is not running, on a node that is up.
#[tokio::test]
async fn a_node_clears_a_record_of_copies_it_no_longer_runs() {
    let a = mesh_testkit::spawn_founder_with("alpha", "stale-record", vec![], |c| {
        c.network.dns = false;
        c.network.dns_bind = None;
    })
    .await;
    let me = a.node.node_id();
    let engine = a.node.engine();
    engine
        .author_op(mesh_proto::OpBody::RegistryPut {
            registrations: Box::new(mesh_proto::NodeRegistrations {
                node: me,
                services: vec![mesh_proto::Registration {
                    service: "web".into(),
                    instance: mesh_proto::JobId([7; 16]),
                    label: "http".into(),
                    address: "127.0.0.1".into(),
                    port: 20000,
                    tags: vec![],
                    health: mesh_proto::Health::Passing,
                    since_micros: 0,
                    deployment: "web".into(),
                }],
                published_at_micros: mesh_proto::now_micros(),
            }),
        })
        .expect("stand up a stale record");
    assert!(
        engine.read(|s| s.registry.get(&me).is_some_and(|r| r.services.len() == 1)),
        "the stale record should be in state before the node answers it"
    );
    assert!(
        wait_until(Duration::from_secs(45), || engine.read(|s| s
            .registry
            .get(&me)
            .is_some_and(|r| r.services.is_empty())))
        .await,
        "the node never cleared a record of copies it does not run"
    );
}

#[tokio::test]
async fn a_registered_instance_is_reached_at_its_nodes_address_and_port() {
    // Two in-process nodes share one network namespace, so neither runs a
    // DNS responder of its own.
    let a = mesh_testkit::spawn_founder_with("alpha", "addr", vec![], |c| {
        c.network.dns = false;
        c.network.dns_bind = None;
    })
    .await;
    let b = mesh_testkit::join_new_node_with("beta", &a, vec![], |c| {
        c.network.dns = false;
        c.network.dns_bind = None;
    })
    .await;
    let id_b = b.node.node_id();
    assert!(
        wait_until(Duration::from_secs(30), || a
            .node
            .engine()
            .read(|s| s.members.contains_key(&id_b)))
        .await,
        "beta did not join"
    );

    // Binds where the node says to and says what it was told, so the reply
    // proves `MESHWARE_IP_HTTP` and `MESHWARE_ADDR_HTTP` as well as the path.
    let server = concat!(
        "#!/bin/bash\n",
        "exec python3 - <<'PY'\n",
        "import http.server, os\n",
        "class H(http.server.BaseHTTPRequestHandler):\n",
        "    def do_GET(self):\n",
        "        body = ('bound on %s\\n' % os.environ['MESHWARE_ADDR_HTTP']).encode()\n",
        "        self.send_response(200)\n",
        "        self.send_header('Content-Length', str(len(body)))\n",
        "        self.end_headers()\n",
        "        self.wfile.write(body)\n",
        "http.server.HTTPServer((os.environ['MESHWARE_IP_HTTP'], int(os.environ['MESHWARE_PORT_HTTP'])), H).serve_forever()\n",
        "PY\n",
    );
    let files = bundle(&[("run.sh", server, 0o755)]);
    let engine = a.node.engine();
    let content = mesh_core::content::ContentStore::open(a.config().data_dir.as_path()).unwrap();
    let push = mesh_core::services::push_dir(engine, &content, files.path()).expect("push");
    mesh_core::services::create_service_as(
        engine,
        founder_user_key(&a),
        "web".into(),
        Runtime::Bash,
        push.bundle_id,
        String::new(),
        "run.sh".into(),
        None,
        vec![],
        "a page that says where it is bound".into(),
        vec![],
    )
    .expect("create service");

    // On beta only, registered as `web`, on a dynamic port.
    let src = concat!(
        "[deployment]\nname = \"web\"\nservice = \"web@1\"\ncount = 1\ntargets = \"name=beta\"\n\n",
        "[[deployment.network.port]]\nlabel = \"http\"\n\n",
        "[deployment.register]\nname = \"web\"\n"
    );
    let parsed = mesh_core::deployments::parse(src, founder_user_key(&a).user_id()).unwrap();
    let key = founder_user_key(&a);
    let resp = mesh_testkit::ipc_as(
        &a,
        key,
        IpcRequest::DeploymentPut {
            deployment: Box::new(mesh_core::deployments::sign(key, parsed.deployment)),
            policy: Box::new(mesh_core::deployments::sign_policy(key, parsed.policy)),
            dry_run: false,
            inline_service: false,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::Ok), "put failed: {resp:?}");

    // Alpha runs no copy. What alpha's registry says is where to go, and
    // going there reaches beta's instance directly.
    let mut reached: Option<(RegistryInstanceRow, String)> = None;
    let mut last = String::new();
    for _ in 0..90 {
        if let Some(row) = instances(&a, &a, "web")
            .await
            .into_iter()
            .find(|r| r.label == "http" && r.resolvable)
        {
            let addr = format!("{}:{}", row.address, row.port);
            match get(&addr).await {
                Ok(reply) if reply.contains("bound on ") => {
                    reached = Some((row, reply));
                    break;
                }
                Ok(reply) => last = format!("unexpected reply: {reply:?}"),
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let Some((row, reply)) = reached else {
        let rows = instances(&a, &a, "web").await;
        let jobs = b.node.engine().read(|s| {
            s.jobs
                .values()
                .map(|j| {
                    format!(
                        "{} {:?} on {}",
                        j.job_id.short(),
                        j.state,
                        j.executor.short()
                    )
                })
                .collect::<Vec<_>>()
        });
        panic!(
            "web was never reachable at the address alpha's registry gave\n\
             last attempt: {last}\nalpha's rows for web: {rows:?}\njobs beta knows: {jobs:?}"
        );
    };

    assert_eq!(row.node, "beta", "{row:?}");
    // The port the node allocated, not one declared for a name: nothing sits
    // in front of the instance to translate.
    assert!(row.port >= 1024, "{row:?}");
    // Testkit nodes listen on loopback, so that is the bind address a job is
    // handed — on a real node, `0.0.0.0` — and the address it bound matches
    // what the registry tells every other machine.
    assert!(
        reply.contains(&format!("bound on 127.0.0.1:{}", row.port)),
        "the job was not told MESHWARE_IP/ADDR for the port it was given: {reply:?}"
    );

    // Beta's own listing agrees, down to the port.
    let from_beta = instances(&b, &a, "web").await;
    assert!(
        from_beta
            .iter()
            .any(|r| r.label == "http" && r.port == row.port),
        "{from_beta:?}"
    );

    b.node.shutdown();
    a.node.shutdown();
}
