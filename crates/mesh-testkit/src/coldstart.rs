//! The cold-start timer (C0–C9 §7.3 P1).
//!
//! **An unmeasured claim is a slogan.** Every competitor's getting-started
//! guide begins *"first, stand up the control plane"*; meshware's begins
//! *"run this on any machine"*, and the row in the operational-cost table
//! that has to be *earned* rather than asserted is the last one — time from
//! zero to scheduling.
//!
//! So this measures it, in phases, from a script anyone can run:
//!
//! | phase | what it covers |
//! |---|---|
//! | install | fetch, verify and install the binary — shell only |
//! | up | `meshnode up`: identity, genesis, an encrypted listener |
//! | invite | minting a join token |
//! | join | a second machine going from nothing to a member |
//! | schedule | submitting work and having an executor accept it |
//!
//! Two harnesses fill the same shape and neither is the other's substitute.
//! This one runs in CI on every change, with a regression gate, and cannot
//! measure `curl | sh` because there is no network and no binary — the
//! `install` phase is `None` here. `scripts/cold-start.sh` runs the real
//! thing on a real host, including the install, and is the number the
//! benchmark page publishes.
//!
//! **What is deliberately not measured:** anything a competitor would have
//! to do that meshware does not. There is no "provision etcd" phase to
//! leave out, and adding one to make the comparison look better would be
//! the kind of benchmark nobody believes. The operational-cost table cites
//! each project's own documented minimum topology instead
//! (`docs/operational-cost.md`).

use std::time::{Duration, Instant};

use mesh_proto::{IpcRequest, IpcResponse, JobPriority};

use crate::{TestNode, founder_user_key, join_new_node, spawn_founder, stream_as};

/// One cold-start measurement, in microseconds per phase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColdStart {
    /// Fetch, verify and install the binary. `None` in-process: there is
    /// nothing to fetch and nothing to verify, and reporting a zero would
    /// be worse than reporting nothing.
    pub install_micros: Option<u64>,
    /// `meshnode up` on a bare host: identity, genesis, listener.
    pub up_micros: u64,
    /// Minting a join token.
    pub invite_micros: u64,
    /// A second machine from nothing to a member of the mesh.
    pub join_micros: u64,
    /// Submitting work and having an executor accept it. **Scheduled, not
    /// finished**: the claim is about how long it takes the mesh to be
    /// ready to run something, and how long the user's own command takes
    /// after that is the user's business.
    pub schedule_micros: u64,
}

impl ColdStart {
    /// Everything the harness measured, end to end.
    pub fn total_micros(&self) -> u64 {
        self.install_micros
            .unwrap_or(0)
            .saturating_add(self.up_micros)
            .saturating_add(self.invite_micros)
            .saturating_add(self.join_micros)
            .saturating_add(self.schedule_micros)
    }

    pub fn total(&self) -> Duration {
        Duration::from_micros(self.total_micros())
    }

    /// The line the script prints and CI gates on.
    pub fn summary(&self) -> String {
        let ms = |micros: u64| format!("{:.0} ms", micros as f64 / 1000.0);
        format!(
            "install {} · up {} · invite {} · join {} · schedule {} · total {}",
            self.install_micros.map_or("-".into(), ms),
            ms(self.up_micros),
            ms(self.invite_micros),
            ms(self.join_micros),
            ms(self.schedule_micros),
            ms(self.total_micros()),
        )
    }

    /// One JSON object per run, for the benchmark page and for a CI
    /// artifact somebody plots later. Hand-written rather than derived:
    /// this crate has no serde dependency and one object does not justify
    /// acquiring one.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"install_micros\":{},\"up_micros\":{},\"invite_micros\":{},\
             \"join_micros\":{},\"schedule_micros\":{},\"total_micros\":{}}}",
            self.install_micros
                .map_or("null".to_string(), |v| v.to_string()),
            self.up_micros,
            self.invite_micros,
            self.join_micros,
            self.schedule_micros,
            self.total_micros(),
        )
    }
}

/// Run one cold start: found a mesh, join a second node, schedule work.
///
/// Every phase is the **real** path — real identity, real genesis, real
/// QUIC on loopback, the real join handshake, the real scheduler. A timer
/// around a mock would measure the mock.
pub async fn measure() -> (ColdStart, TestNode, TestNode) {
    let mut report = ColdStart::default();

    let t = Instant::now();
    let founder = spawn_founder("cold-a", "cold-start").await;
    report.up_micros = micros(t);

    // Minting the token is its own phase because it is its own *step* for
    // the person doing this: `meshnode invite create`, copy, paste. The CLI
    // builds and signs the token on the operator's own machine and only
    // publishes the public half, so this is the round trip that costs
    // anything.
    let t = Instant::now();
    let user = founder_user_key(&founder).clone();
    let bootstrap = crate::ipc_as(&founder, &user, IpcRequest::InviteBootstrap).await;
    assert!(
        matches!(bootstrap, IpcResponse::InviteBootstrap { .. }),
        "cold start could not mint an invite: {bootstrap:?}"
    );
    report.invite_micros = micros(t);

    let t = Instant::now();
    let joiner = join_new_node("cold-b", &founder, vec![]).await;
    report.join_micros = micros(t);

    // Zero to scheduling. Stops at the moment an executor has accepted the
    // work, which is what "ready to run something" means — not when the
    // user's own command happens to finish.
    let t = Instant::now();
    let mut rx = stream_as(
        &founder,
        &user,
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
    while let Some(frame) = rx.recv().await {
        match frame {
            IpcResponse::JobStarted { .. } => break,
            IpcResponse::Error { message } => panic!("cold start could not schedule: {message}"),
            _ => {}
        }
    }
    report.schedule_micros = micros(t);

    (report, founder, joiner)
}

fn micros(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX)
}
