//! The build farm, against a real compiler and a real node.
//!
//! `farm.rs`'s unit tests pin the command-line reasoning — what is
//! distributable, which flags belong to which half, that the heredoc cannot
//! be escaped. None of that answers the only question that matters:
//!
//! > **Does a C file compiled through the mesh produce an object file that
//! > links and runs?**
//!
//! So this compiles real C with the real `cc` on this machine, submits it
//! through a real engine over `IpcRequest::RunScript`, links the object it
//! gets back, and runs the program. If the wrapper corrupts a byte, mangles
//! a flag, or loses the object, the linker or the executable says so.
//!
//! It also pins the two behaviours that make the thing usable rather than
//! merely correct: the cache hits on a second identical compile (which is
//! the whole `ccache` half), and a broken source reports the compiler's own
//! diagnostics instead of falling back and building it twice.
//!
//! Skipped where there is no C compiler. A build machine without one is a
//! real thing and failing there would be reporting the environment, not the
//! code.
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::pin::Pin;
use std::process::Command;

use mesh_proto::{IpcRequest, IpcResponse};
use mesh_testkit::{TestNode, founder_user_key, spawn_founder, stream_as};
use meshware_node::farm::{self, How, Options, Submitter};
use tokio::sync::mpsc;

/// A `Submitter` backed by an in-process engine.
struct Mesh {
    node: TestNode,
}

impl Submitter for Mesh {
    fn submit(
        &self,
        req: IpcRequest,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = anyhow::Result<mpsc::Receiver<IpcResponse>>>
                + Send
                + '_,
        >,
    > {
        Box::pin(async move { Ok(stream_as(&self.node, founder_user_key(&self.node), req).await) })
    }
}

/// The C compiler on this machine, if there is one.
fn compiler() -> Option<&'static str> {
    ["cc", "gcc", "clang"].into_iter().find(|cc| {
        Command::new(cc)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

fn opts(argv: Vec<String>) -> Options {
    Options {
        argv,
        on: Some("any".into()),
        verbose: true,
        local_only: false,
        timeout_secs: 120,
    }
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_string()).collect()
}

/// Compile through the mesh, link the result, run it, and check what it
/// printed. Nothing short of this proves the object file is real.
#[tokio::test(flavor = "multi_thread")]
async fn a_c_file_compiled_through_the_mesh_links_and_runs() {
    let Some(cc) = compiler() else {
        eprintln!("no C compiler on this machine — skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("hello.c");
    let obj = dir.path().join("hello.o");
    let bin = dir.path().join("hello");
    std::fs::write(
        &src,
        // An `#include` on purpose: preprocessing is the whole trick, and a
        // source with no headers would pass even if `-E` never ran.
        "#include <stdio.h>\n\
         #ifndef GREETING\n\
         #define GREETING \"unset\"\n\
         #endif\n\
         int main(void) { printf(\"%s %d\\n\", GREETING, ANSWER); return 0; }\n",
    )
    .unwrap();

    let mesh = Mesh {
        node: spawn_founder("builder", "farm-hello").await,
    };
    let done = farm::run_reported(
        &mesh,
        opts(argv(&[
            cc,
            "-c",
            src.to_str().unwrap(),
            "-o",
            obj.to_str().unwrap(),
            // A define proves the preprocessor flags reached `-E`, and an
            // optimisation flag proves the compile flags reached the
            // executor: get either wrong and this fails to build or prints
            // the wrong thing.
            "-DGREETING=\"hello\"",
            "-DANSWER=42",
            "-O2",
        ])),
    )
    .await
    .expect("the wrapper ran");
    assert_eq!(done.code, 0, "the compile succeeded");
    // The claim under test is that the *mesh* built it. Without this the
    // test passes just as happily when every compile silently falls back
    // to the local compiler, which is exactly the failure a build farm
    // hides best.
    assert!(
        !done.how.is_local(),
        "the compile fell back to local: {}",
        done.how.describe()
    );
    assert!(obj.exists(), "an object file came back");
    let size = std::fs::metadata(&obj).unwrap().len();
    assert!(
        size > 100,
        "the object is {size} bytes — that is not an object"
    );

    // The real check: does it link, and does it say the right thing?
    let linked = Command::new(cc)
        .arg(&obj)
        .arg("-o")
        .arg(&bin)
        .output()
        .expect("running the linker");
    assert!(
        linked.status.success(),
        "the object did not link: {}",
        String::from_utf8_lossy(&linked.stderr)
    );
    let out = Command::new(&bin).output().expect("running the program");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "hello 42",
        "the program built through the mesh printed the wrong thing"
    );
}

/// The second identical compile is a cache hit — the `ccache` half.
#[tokio::test(flavor = "multi_thread")]
async fn an_identical_compile_is_served_from_the_cache() {
    let Some(cc) = compiler() else {
        eprintln!("no C compiler on this machine — skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("twice.c");
    std::fs::write(&src, "int twice(int x) { return x * 2; }\n").unwrap();

    let mesh = Mesh {
        node: spawn_founder("builder", "farm-cache").await,
    };
    let build = |out: &Path| {
        opts(argv(&[
            cc,
            "-c",
            src.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ]))
    };

    let first = dir.path().join("a.o");
    let done = farm::run_reported(&mesh, build(&first)).await.unwrap();
    assert_eq!(done.code, 0);
    assert!(
        !done.how.is_local(),
        "the first compile fell back: {}",
        done.how.describe()
    );

    // The entry is published by a background sweep on the executor, so the
    // hit is not instant. Poll for it rather than sleeping a fixed amount:
    // a fixed sleep is either flaky or slow, and usually both.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut published = false;
    while std::time::Instant::now() < deadline {
        if stats(&mesh.node).await.entries > 0 {
            published = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(published, "the compile published a cache entry");

    // …and the identical compile comes back byte-for-byte, from the cache.
    let second = dir.path().join("b.o");
    let again = farm::run_reported(&mesh, build(&second)).await.unwrap();
    assert_eq!(again.code, 0);
    assert_eq!(
        again.how,
        How::Cached,
        "the second identical compile should not have compiled anything"
    );
    assert_eq!(
        std::fs::read(&first).unwrap(),
        std::fs::read(&second).unwrap(),
        "the same source compiled twice produced different objects"
    );

    let after = stats(&mesh.node).await;
    assert!(
        after.hits > 0,
        "the second identical compile was not a cache hit ({} hits, {} misses)",
        after.hits,
        after.misses
    );
}

/// This node's action-cache counters.
async fn stats(node: &TestNode) -> mesh_proto::cache::CacheStats {
    let IpcResponse::CacheStats(stats) =
        mesh_testkit::ipc_as(node, founder_user_key(node), IpcRequest::CacheStats).await
    else {
        panic!("expected CacheStats");
    };
    *stats
}

/// A broken source reports the compiler's own diagnostics and does **not**
/// silently rebuild locally.
#[tokio::test(flavor = "multi_thread")]
async fn a_compile_error_is_reported_rather_than_rebuilt() {
    let Some(cc) = compiler() else {
        eprintln!("no C compiler on this machine — skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("broken.c");
    let obj = dir.path().join("broken.o");
    std::fs::write(&src, "int main(void) { this is not C; }\n").unwrap();

    let mesh = Mesh {
        node: spawn_founder("builder", "farm-broken").await,
    };
    let done = farm::run_reported(
        &mesh,
        opts(argv(&[
            cc,
            "-c",
            src.to_str().unwrap(),
            "-o",
            obj.to_str().unwrap(),
        ])),
    )
    .await
    .expect("the wrapper ran");
    let code = done.code;
    assert!(
        !done.how.is_local(),
        "the broken compile fell back instead of reporting: {}",
        done.how.describe()
    );
    assert_ne!(code, 0, "a broken compile has to fail the build");
    // …and it has to be the *remote* compiler's verdict, not a local
    // rebuild that produced the same errors at twice the cost.
    assert!(
        !obj.exists(),
        "a failed compile must not leave an object behind for make to trust"
    );
}

/// A link step never goes near the mesh — it runs here, unchanged.
#[tokio::test(flavor = "multi_thread")]
async fn a_link_runs_locally_and_still_works() {
    let Some(cc) = compiler() else {
        eprintln!("no C compiler on this machine — skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("lonely.c");
    let bin = dir.path().join("lonely");
    std::fs::write(&src, "int main(void) { return 7; }\n").unwrap();

    let mesh = Mesh {
        node: spawn_founder("builder", "farm-link").await,
    };
    // No `-c`: this is a compile *and* link, which needs the local
    // toolchain and the local filesystem.
    let code = farm::run(
        &mesh,
        opts(argv(&[
            cc,
            src.to_str().unwrap(),
            "-o",
            bin.to_str().unwrap(),
        ])),
    )
    .await
    .expect("the wrapper ran");
    assert_eq!(code, 0);
    assert!(bin.exists(), "the local fallback produced the binary");
    let status = Command::new(&bin).status().expect("running it");
    assert_eq!(status.code(), Some(7));
}

/// With no mesh to reach, the build still builds.
///
/// The contract that matters more than any speed-up: `meshnode cc` is on
/// the critical path of somebody's `make`, and it must never be the reason
/// that `make` fails.
#[tokio::test(flavor = "multi_thread")]
async fn a_mesh_that_refuses_falls_back_instead_of_failing() {
    let Some(cc) = compiler() else {
        eprintln!("no C compiler on this machine — skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("resilient.c");
    let obj = dir.path().join("resilient.o");
    std::fs::write(&src, "int answer(void) { return 42; }\n").unwrap();

    struct Refuses;
    impl Submitter for Refuses {
        fn submit(
            &self,
            _req: IpcRequest,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = anyhow::Result<mpsc::Receiver<IpcResponse>>>
                    + Send
                    + '_,
            >,
        > {
            Box::pin(async { anyhow::bail!("no daemon is listening") })
        }
    }

    let code = farm::run(
        &Refuses,
        opts(argv(&[
            cc,
            "-c",
            src.to_str().unwrap(),
            "-o",
            obj.to_str().unwrap(),
        ])),
    )
    .await
    .expect("the wrapper ran");
    assert_eq!(code, 0, "the build survived a dead mesh");
    assert!(obj.exists(), "and produced the object locally");
}
