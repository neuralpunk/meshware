//! ENG-11 — a reload is measured against what is in effect, not against
//! what the node booted with.
//!
//! `apply_config` diffed every section against the boot config, which never
//! changes. A setting changed and then changed back — `sandbox` auto → off →
//! auto — read as "unchanged" the second time and stayed off; the terminal
//! switch and the capacity a node re-announced kept their boot values; and
//! provisioning a volume replayed the boot config over every earlier reload.
#![allow(clippy::unwrap_used)]

use std::time::Duration;

use mesh_core::config::NodeConfig;
use mesh_core::sandbox::Mode;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority, JobState};
use mesh_testkit::{
    RunOutcome, TestNode, founder_user_key, ipc_as, run_to_end, spawn_founder, wait_until,
};

/// The live config, changed by `edit` — what an operator's file says after
/// they touched one thing.
fn edited(node: &TestNode, edit: impl FnOnce(&mut NodeConfig)) -> NodeConfig {
    let mut config = (*node.node.live_config()).clone();
    edit(&mut config);
    config
}

fn announced_tags(node: &TestNode) -> Vec<String> {
    let me = node.node.node_id();
    node.node
        .engine()
        .read(|s| s.members[&me].info.tags.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_setting_changed_and_changed_back_is_applied_both_times() {
    let node = spawn_founder("NIM", "eng11-revert").await;
    let jobs = node.node.jobs();
    let booted = jobs.sandbox_policy().mode;
    assert_ne!(booted, Mode::Off);

    let off = edited(&node, |c| c.runtime.sandbox.mode = Mode::Off);
    let notes = node.node.apply_config(&off);
    assert!(
        notes.iter().any(|n| n.starts_with("job sandbox")),
        "{notes:?}"
    );
    assert_eq!(jobs.sandbox_policy().mode, Mode::Off);

    let back = edited(&node, |c| c.runtime.sandbox.mode = booted);
    let notes = node.node.apply_config(&back);
    assert!(
        notes.iter().any(|n| n.starts_with("job sandbox")),
        "{notes:?}"
    );
    assert_eq!(
        jobs.sandbox_policy().mode,
        booted,
        "changing the sandbox back was read as no change"
    );

    // The same with tags: a → b → a ends announcing a.
    for tags in [vec!["a".to_string()], vec!["b".into()], vec!["a".into()]] {
        let config = edited(&node, |c| c.tags = tags.clone());
        node.node.apply_config(&config);
    }
    let tags = announced_tags(&node);
    assert!(tags.iter().any(|t| t == "a"), "{tags:?}");
    assert!(!tags.iter().any(|t| t == "b"), "{tags:?}");
    node.stop().await;
}

/// A reload of an unchanged file says nothing, however many reloads came
/// before it; a setting only a restart applies is reported every time,
/// until the node restarts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_reports_only_what_differs_from_what_is_running() {
    let node = spawn_founder("NIM", "eng11-notes").await;
    let changed = edited(&node, |c| {
        c.runtime.sandbox.mode = Mode::Off;
        c.limits.queue += 7;
        c.quiet = mesh_core::quiet::QuietHours::default();
        c.disk.job_output_max_mb = 9;
        c.sessions.accept = !c.sessions.accept;
    });
    assert!(!node.node.apply_config(&changed).is_empty());
    let again = node.node.apply_config(&changed);
    assert!(again.is_empty(), "an unchanged file reported {again:?}");

    let renamed = edited(&node, |c| c.node_name = "elsewhere".into());
    for _ in 0..2 {
        let notes = node.node.apply_config(&renamed);
        assert!(
            notes
                .iter()
                .any(|n| n.starts_with("node_name changed — restart required")),
            "{notes:?}"
        );
    }
    assert_eq!(node.node.live_config().node_name, node.config().node_name);
    node.stop().await;
}

/// `[disk]` reloads: a lower output cap applies to the next job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_job_output_cap_reloads() {
    let node = spawn_founder("NIM", "eng11-disk").await;
    let admin = founder_user_key(&node).clone();
    let capped = edited(&node, |c| c.disk.job_output_max_mb = 1);
    let notes = node.node.apply_config(&capped);
    assert!(notes.iter().any(|n| n.starts_with("disk:")), "{notes:?}");

    let outcome = run_to_end(
        &node,
        &admin,
        IpcRequest::Run {
            command: "head -c 3000000 /dev/zero | tr '\\0' x; echo".into(),
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
    let RunOutcome::Done { state, job_id, .. } = outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(state, JobState::Succeeded);
    let kept = std::fs::metadata(
        node.node
            .jobs()
            .job_dir_of(&job_id.unwrap())
            .join("stdout.log"),
    )
    .unwrap()
    .len();
    assert!(kept < 1024 * 1024 + 4096, "kept {kept} bytes");
    node.stop().await;
}

/// Provisioning a volume re-offers the node's volumes through the reload
/// path. It started from the boot config, and so undid every reload before
/// it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_volume_does_not_undo_an_earlier_reload() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let plugin = dir.path().join("plugin.sh");
    std::fs::write(
        &plugin,
        format!(
            "#!/bin/sh\n\
             [ \"$1\" = create ] || exit 0\n\
             d=\"${cfg}/${name}\"\n\
             mkdir -p \"$d\" && printf '{{\"path\":\"%s\"}}' \"$d\"\n",
            cfg = mesh_proto::PLUGIN_CONFIG_ENV,
            name = mesh_proto::PLUGIN_NAME_ENV,
        ),
    )
    .unwrap();
    std::fs::set_permissions(&plugin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let base = dir.path().join("volumes");
    std::fs::create_dir_all(&base).unwrap();

    let node = mesh_testkit::spawn_founder_with("NIM", "eng11-volume", vec![], |c| {
        c.runtime.volume_plugin = Some(plugin.display().to_string());
        c.runtime.volume_plugin_config = Some(base.display().to_string());
    })
    .await;
    let admin = founder_user_key(&node).clone();
    let jobs = node.node.jobs();

    let reloaded = edited(&node, |c| {
        c.limits.slots = 3;
        c.runtime.sandbox.mode = Mode::Off;
        c.tags = vec!["reloaded".into()];
    });
    node.node.apply_config(&reloaded);
    assert_eq!(jobs.limits().slots, 3);

    let resp = ipc_as(
        &node,
        &admin,
        IpcRequest::VolumeCreate {
            name: "scratch".into(),
            capacity_mb: 0,
            params: vec![],
            read_only: false,
        },
    )
    .await;
    assert!(matches!(resp, IpcResponse::VolumeCreated(_)), "{resp:?}");

    assert_eq!(
        jobs.limits().slots,
        3,
        "the volume undid the reload's slots"
    );
    assert_eq!(jobs.sandbox_policy().mode, Mode::Off);
    assert!(
        wait_until(Duration::from_secs(10), || {
            let tags = announced_tags(&node);
            tags.iter().any(|t| t == "reloaded")
                && mesh_proto::Capacity::from_tags(&tags)
                    .is_some_and(|c| c.volumes.iter().any(|v| v == "scratch"))
        })
        .await,
        "announced {:?}",
        announced_tags(&node)
    );
    node.stop().await;
}
