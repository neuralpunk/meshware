//! **The daemon reads no file for anyone that its caller could not**
//! (SEC-3).
//!
//! `Push` has the daemon read a tree it is pointed at. A job's sandbox hides
//! the node's `identity/` from the job, and a job holding its credential
//! could ask the daemon to read `identity/` for it, publish it as a bundle
//! and read it back — the node key opens every secret sealed to the node.
//! Anyone with the content role could do the same over the HTTP API. A job
//! credential could also ask for a backup (the identity, written where the
//! caller says) or a session (a shell running as the daemon, unsandboxed).
//!
//! These tests pin each door shut, and the kernel ruleset behind the path
//! check.
#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use mesh_core::identity::Keypair;
use mesh_proto::{
    IpcRequest, IpcResponse, JobId, JobPriority, SessionKind, SessionServerFrame, read_frame,
};
use mesh_testkit::{TestNode, founder_user_key, ipc_as, spawn_founder, stream_as, wait_until};
use tokio::sync::mpsc;

fn push(path: &Path) -> IpcRequest {
    IpcRequest::Push {
        path: path.to_string_lossy().into_owned(),
    }
}

fn refused(resp: IpcResponse, why: &str) {
    match resp {
        IpcResponse::Error { message } | IpcResponse::Denied { message } => {
            assert!(message.contains(why), "wanted {why:?}, got: {message}")
        }
        other => panic!("not refused ({why}): {other:?}"),
    }
}

fn pushed(resp: IpcResponse) {
    assert!(
        matches!(resp, IpcResponse::Pushed { .. }),
        "an ordinary push was refused: {resp:?}"
    );
}

fn tree(root: &Path) -> PathBuf {
    let dir = root.join("project");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("main.sh"), "echo hi\n").unwrap();
    dir
}

#[tokio::test]
async fn the_daemon_will_not_push_its_own_directory() {
    let founder = spawn_founder("NIM", "push-own-dir").await;
    let admin = founder_user_key(&founder).clone();
    let data = founder.config().data_dir.clone();
    let scratch = tempfile::tempdir().unwrap();

    // The directory itself, a part of it, and a link that leads into it.
    refused(
        ipc_as(&founder, &admin, push(&data)).await,
        "is, or contains",
    );
    refused(
        ipc_as(&founder, &admin, push(&data.join("identity"))).await,
        "is, or contains",
    );
    let link = scratch.path().join("innocent");
    std::os::unix::fs::symlink(data.join("identity"), &link).unwrap();
    refused(
        ipc_as(&founder, &admin, push(&link)).await,
        "is, or contains",
    );
    // …and a directory that contains it.
    if let Some(parent) = data.parent() {
        refused(
            ipc_as(&founder, &admin, push(parent)).await,
            "is, or contains",
        );
    }

    // An ordinary tree still goes in.
    pushed(ipc_as(&founder, &admin, push(&tree(scratch.path()))).await);

    // And a backup may not be written over the node.
    refused(
        ipc_as(
            &founder,
            &admin,
            IpcRequest::Backup {
                path: data
                    .join("identity")
                    .join("x.backup")
                    .to_string_lossy()
                    .into(),
            },
        )
        .await,
        "inside this node's data directory",
    );

    founder.stop().await;
}

/// A script dropped onto Director's window is pushed as that one file
/// (DIR-7): the request Director makes is `Push` naming the file, through
/// the same path check and confined reader as a folder. It goes in with
/// its definitions file and without its neighbours.
#[tokio::test]
async fn a_single_file_goes_in_with_its_definitions_and_nothing_else() {
    let founder = spawn_founder("NIM", "push-one-file").await;
    let admin = founder_user_key(&founder).clone();
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("downloads");
    std::fs::create_dir_all(&home).unwrap();
    let script = home.join("rotate-logs.sh");
    std::fs::write(&script, "#!/bin/sh\necho rotating\n").unwrap();
    std::fs::write(
        mesh_core::script_spec::definitions_path(&script.to_string_lossy()),
        "name: rotate-logs\n",
    )
    .unwrap();
    std::fs::write(home.join("tax-return.pdf"), "not a script").unwrap();

    let IpcResponse::Pushed {
        bundle_id,
        file_count,
        ..
    } = ipc_as(&founder, &admin, push(&script)).await
    else {
        panic!("a single file must be accepted");
    };
    assert_eq!(file_count, 2, "the script and its definitions, no more");
    let IpcResponse::BundleFiles(rows) =
        ipc_as(&founder, &admin, IpcRequest::BundleFiles { bundle_id }).await
    else {
        panic!("the pushed bundle lists its files");
    };
    let mut names: Vec<_> = rows.iter().map(|r| r.path.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["rotate-logs.sh", "rotate-logs.sh.definitions"],
        "at the bundle's root, under their own names"
    );

    founder.stop().await;
}

/// The kernel half: a thread confined to "everything but X" cannot open
/// X, whatever path it takes to get there, and can still do its work.
#[test]
fn a_confined_thread_cannot_reach_what_it_hides() {
    let scratch = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(scratch.path()).unwrap();
    let hidden = root.join("node");
    let store = hidden.join("content");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(hidden.join("node.key"), "secret").unwrap();
    let open = root.join("open");
    std::fs::create_dir_all(&open).unwrap();
    std::fs::write(open.join("file"), "fine").unwrap();
    std::os::unix::fs::symlink(&hidden, root.join("sneaky")).unwrap();

    let read = mesh_core::sandbox::everything_but(std::slice::from_ref(&hidden));
    assert!(
        read.iter().all(|p| !p.starts_with(&hidden)),
        "the allowlist names the hidden tree"
    );
    assert!(
        read.iter().all(|p| p != &root.join("sneaky")),
        "a link into the hidden tree was granted"
    );
    assert!(read.contains(&open));

    let (status, key, via_link, file, wrote) = std::thread::spawn(move || {
        let status = mesh_core::sandbox::confine_this_thread(&read, std::slice::from_ref(&store));
        (
            status,
            std::fs::read(hidden.join("node.key")),
            std::fs::read(root.join("sneaky").join("node.key")),
            std::fs::read(open.join("file")),
            std::fs::write(store.join("blob"), "ok"),
        )
    })
    .join()
    .unwrap();
    if status == "none" {
        eprintln!("no Landlock on this kernel; the path check stands alone");
        return;
    }
    assert!(key.is_err(), "the confined thread read the hidden key");
    assert!(
        via_link.is_err(),
        "the confined thread read it through a link"
    );
    assert_eq!(file.unwrap(), b"fine");
    wrote.unwrap();

    // The confinement was the thread's alone.
    assert!(std::fs::read(scratch.path().join("node").join("node.key")).is_ok());
}

/// Start `command` on `node` and return the job's credential.
async fn holding(node: &TestNode) -> (JobId, mpsc::Receiver<IpcResponse>, Keypair) {
    let req = IpcRequest::Run {
        command: "sleep 60".into(),
        on: Some(format!("name={}", node.config().node_name)),
        env: vec![],
        timeout_secs: Some(90),
        fanout: None,
        priority: JobPriority::Normal,
        max_parallel: None,
        halt_on_fail: None,
    };
    let mut rx = stream_as(node, founder_user_key(node), req).await;
    let job = loop {
        match rx.recv().await.expect("stream ended early") {
            IpcResponse::JobStarted { job_id, .. } => break job_id,
            IpcResponse::Notice { .. } => {}
            other => panic!("unexpected frame: {other:?}"),
        }
    };
    let key = node
        .node
        .jobs()
        .job_dir_of(&job)
        .join("identity")
        .join("user.key");
    assert!(wait_until(Duration::from_secs(20), || key.is_file()).await);
    let (credential, _) = Keypair::load_or_generate(&key).unwrap();
    (job, rx, credential)
}

#[tokio::test]
async fn a_job_credential_stays_within_its_reach() {
    let founder = spawn_founder("NIM", "push-credential").await;
    let (job, _rx, credential) = holding(&founder).await;
    let data = founder.config().data_dir.clone();

    // The node's key, through the daemon: refused.
    refused(
        ipc_as(&founder, &credential, push(&data.join("identity"))).await,
        "is, or contains",
    );
    // What the task staged in its own directory: fine — that is how
    // `mesh.map` ships a project from inside a job.
    let own = founder.node.jobs().job_dir_of(&job);
    pushed(ipc_as(&founder, &credential, push(&tree(&own))).await);

    // A backup, as the job: refused outright.
    let scratch = tempfile::tempdir().unwrap();
    refused(
        ipc_as(
            &founder,
            &credential,
            IpcRequest::Backup {
                path: scratch.path().join("b").to_string_lossy().into(),
            },
        )
        .await,
        "cannot ask for a backup",
    );

    // A session, as the job: refused before anything is opened.
    let open = IpcRequest::SessionOpen {
        node: founder.config().node_name.clone(),
        kind: SessionKind::Exec {
            command: "cat identity/node.key".into(),
        },
        reason: "escape".into(),
    };
    let (mut ours, theirs) = tokio::net::UnixStream::pair().unwrap();
    let env = credential.sign_request(&open).unwrap();
    let node = founder.node.clone();
    let served = tokio::spawn(async move { node.serve_ipc_session(env, theirs).await });
    let frame: SessionServerFrame = read_frame(&mut ours).await.unwrap();
    match frame {
        SessionServerFrame::Error { message } => {
            assert!(message.contains("cannot ask for a session"), "{message}")
        }
        other => panic!("a job credential opened a session: {other:?}"),
    }
    drop(ours);
    served.await.unwrap();

    // The person the credential speaks for is not limited by any of this.
    let admin = founder_user_key(&founder).clone();
    pushed(ipc_as(&founder, &admin, push(&tree(scratch.path()))).await);

    founder.stop().await;
}
