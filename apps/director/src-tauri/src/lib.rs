//! Meshware Director — a desktop view onto the mesh (MASTERPLAN §14), and a
//! client of this machine's `meshnode` exactly as the CLI is. The frontend
//! never speaks a special protocol: every command signs an `IpcRequest`
//! with the human's key and sends it over the node's socket, to the same
//! router the CLI talks to. Anything the CLI builds client-side (invites,
//! service versions, grants, sealed secrets, Git sources) is built and
//! signed here, the same way — API parity (§3) is a hard rule, so
//! `meshnode` and Director can do exactly the same things, and Director
//! holds no part of the mesh the CLI cannot reach without it.

mod about;
mod channel;
mod credentials;
mod deeplink;
mod demo;
mod deploy;
mod desktop;
mod engine;
mod filesave;
mod glance;
mod identity;
mod linuxdesktop;
mod localnode;
mod notify;
mod picked;
mod security_key;
mod selfupdate;
mod ssh;
mod view;

use std::path::Path;

use tauri::ipc::Channel;
use tauri::{AppHandle, Manager, State};
use tokio::sync::oneshot;

use engine::{Director, Engine};
use mesh_core::genesis::Genesis;
use mesh_core::{membership, pairing, secrets, services};
use mesh_proto::{
    BundleId, Grant, GrantId, GrantRevocation, HaltPolicy, IpcRequest, IpcResponse, JobId,
    JobPriority, NodeId, NodeRevocation, Role, Runtime, SecretRemoval, ServiceVersion, UserId,
    UserRecord, UserRevocation,
};
use view::{
    ApproveEvent, AuditView, BackpressureView, BackupView, BootstrapView, BumpedView, BundleView,
    CandidateView, DefinitionView, DeviceView, DoctorCheckView, DoctorFixReport, DoctorFixView,
    DrainEvent, EnrolmentView, GrantView, HistoryJobPageView, HistoryJobView, InviteView,
    JobsPageView, LanWatchView, NodeView, PairCodeView, ParallelEvent, PayloadWindowView, PingView,
    PrefetchEvent, ProviderView, PushResultView, RestoreView, RunEvent, SecretSetView, SecretView,
    ServiceView, SourceView, StateVersionsView, StatusView, SyncResultView, TierView, TopView,
    TotalsView, UpgradeEvent, UserView, WhoAmIView, WizardPairEvent,
};

fn hostname() -> String {
    let h = gethostname::gethostname().to_string_lossy().into_owned();
    if h.is_empty() { "director".into() } else { h }
}

fn now_micros() -> u64 {
    mesh_proto::now_micros()
}

fn parse_role(s: &str) -> Result<Role, String> {
    Role::parse(s)
        .ok_or_else(|| format!("unknown role {s:?} (admin|operator|developer|auditor|viewer)"))
}

fn parse_priority(s: &str) -> Result<JobPriority, String> {
    JobPriority::parse(s).ok_or_else(|| format!("unknown priority {s:?} (low|normal|high)"))
}

fn parse_halt(s: Option<String>) -> Result<Option<HaltPolicy>, String> {
    match s.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => HaltPolicy::parse(s).map(Some).ok_or_else(|| {
            format!("halt-on-fail takes a count or a percentage (e.g. 3 or 10%), got {s:?}")
        }),
    }
}

/// Seconds-from-now → absolute expiry micros (`None` = no expiry).
fn expiry_micros(expires_secs: Option<u64>) -> Option<u64> {
    expires_secs
        .filter(|s| *s > 0)
        .map(|s| now_micros().saturating_add(s.saturating_mul(1_000_000)))
}

// ---------------------------------------------------------------------------
// Bootstrap & wizard
// ---------------------------------------------------------------------------
//
// Every way in ends the same way: this machine's `meshnode` running in the
// machine's home, and Director connected to it. The wizard prepares that
// home exactly as `meshnode join`, `meshnode pair` and `meshnode restore`
// would — with the same library calls — and then starts the node the way
// `meshnode up --detach` does. There is no second place a mesh can live.

#[tauri::command]
async fn bootstrap_status(
    app: AppHandle,
    state: State<'_, Director>,
) -> Result<BootstrapView, String> {
    let home = engine::node_home();
    let layout = mesh_core::layout::Layout::new(&home);
    // In a mesh when the home has one — or when an older Director's node is
    // still in its old home, waiting for the first `start_engine` to adopt
    // it. Offering that machine the wizard would found a second mesh, which
    // is the whole bug this layout exists to end.
    let has_mesh = layout.genesis_file().exists()
        || (engine::demo_root().is_none()
            && mesh_core::layout::Layout::new(layout.director_dir())
                .genesis_file()
                .exists());
    let running = state.inner.lock().await.engine.is_some();
    // A locked key must not make this fail. It is the *first* call Director
    // makes, so an error here is a dead window with no way forward — which
    // is exactly what a GUI-only person hit before the unlock gate existed.
    // `identity_lock_status` is what the frontend asks first now; this stays
    // tolerant so the wizard can still draw.
    let user = engine::load_user().ok();
    let resource_dir = app.path().resource_dir().ok();
    Ok(BootstrapView {
        has_mesh,
        running,
        hostname: hostname(),
        user_id: user
            .map(|u| u.user_id().to_display_string())
            .unwrap_or_default(),
        data_dir: home.display().to_string(),
        // Whether this *is* a demo, and whether one could be offered. The
        // first drives the banner that never goes away while a demo runs;
        // the second decides whether the wizard draws the card at all — a
        // path that fails when a stranger picks it is worse than one that
        // was never shown (L1 P3).
        demo: engine::demo_root().is_some(),
        demo_available: demo::available(resource_dir.as_deref()),
    })
}

/// The `meshnode` this machine's node is run with when Director has to
/// install it: one already at `~/.local/bin/meshnode` — somebody's own, or
/// one Director put there — and otherwise the one Director carries.
fn meshnode_binary(app: &AppHandle) -> Option<std::path::PathBuf> {
    mesh_core::usernode::install_path()
        .filter(|p| p.is_file())
        .or_else(|| demo::meshnode_binary(app.path().resource_dir().ok().as_deref()))
}

/// Connect to this machine's node — starting it if it has a mesh and is not
/// running — and remember the connection. `founding` starts it even without
/// a mesh, which is how a mesh is founded.
async fn connect(
    app: &AppHandle,
    state: &State<'_, Director>,
    founding: bool,
) -> Result<Engine, String> {
    // A second caller waits here, then finds the first one's connection.
    let _starting = state.starting.lock().await;
    if let Some(engine) = state.inner.lock().await.engine.clone() {
        return Ok(engine);
    }
    let engine = engine::connect(meshnode_binary(app), founding).await?;
    state.inner.lock().await.engine = Some(engine.clone());
    Ok(engine)
}

/// What the wizard's "let this computer run the mesh's work" said, applied
/// to this machine's node — `meshnode nodes tag` on itself. The op-log keeps
/// it across restarts, and the Settings toggle changes it the same way.
async fn apply_contribute(engine: &Engine, contribute: bool) -> Result<(), String> {
    let tag = vec![mesh_proto::NO_EXECUTE_TAG.to_string()];
    let (add, remove) = if contribute {
        (Vec::new(), tag)
    } else {
        (tag, Vec::new())
    };
    engine
        .ipc(IpcRequest::TagsUpdate { add, remove })
        .await
        .map(|_| ())
}

/// The tags a node joins with for a contribute choice: `no-execute` unless
/// the person said this machine should run the mesh's work. Carried in the
/// join itself, so the mesh never sees this machine without it.
fn tags_for(contribute: bool) -> Vec<String> {
    if contribute {
        Vec::new()
    } else {
        vec![mesh_proto::NO_EXECUTE_TAG.to_string()]
    }
}

/// Connect to this machine's node (app launch; after a drain stopped it) —
/// and bring it up to the `meshnode` this Director carries, if it is behind
/// (`localnode::keep_current`): this machine's node follows Director, as it
/// did when it ran inside it.
#[tauri::command]
async fn start_engine(app: AppHandle, state: State<'_, Director>) -> Result<StatusView, String> {
    let engine = connect(&app, &state, false).await?;
    let carried = localnode::carried(app.path().resource_dir().ok().as_deref());
    if let Some(version) = localnode::keep_current(&engine, carried).await {
        tracing::info!(%version, "this machine's node now runs the meshnode Director carries");
    }
    status_of(&engine).await
}

/// Wizard: found a brand-new mesh; this human becomes admin #1 (§5.1).
///
/// `meshnode up` with a mesh name, as a service: the name goes in the
/// node's `meshnode.toml`, and the node founds the mesh on its first start
/// with `<home>/identity/user.key` — the key Director signs with — as the
/// founder.
#[tauri::command]
async fn create_mesh(
    app: AppHandle,
    state: State<'_, Director>,
    mesh_name: String,
    node_name: String,
    contribute: bool,
) -> Result<StatusView, String> {
    if Genesis::load(&engine::node_home())
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("this machine is already in a mesh".into());
    }
    // Opened here, before the node is asked to found anything, so a locked
    // key is a sentence in this window rather than a mesh with no founder.
    engine::load_user()?;
    engine::save_node_settings(&node_name, Some(&mesh_name))?;
    let engine = connect(&app, &state, true).await?;
    apply_contribute(&engine, contribute).await?;
    status_of(&engine).await
}

/// Wizard: join an existing mesh with an invite token (`meshnode join`).
/// `via` dials these addresses instead of the token's embedded ones
/// (VPN / overlay networks), like `meshnode join --via`.
#[tauri::command]
async fn join_token(
    app: AppHandle,
    state: State<'_, Director>,
    token: String,
    node_name: String,
    contribute: bool,
    via: Vec<String>,
) -> Result<StatusView, String> {
    let config = engine::node_config(Some(&node_name), tags_for(contribute))?;
    membership::join_mesh(&config, &token, &via)
        .await
        .map_err(|e| e.to_string())?;
    engine::save_node_settings(&node_name, None)?;
    let engine = connect(&app, &state, false).await?;
    status_of(&engine).await
}

/// Wizard: restore a `meshnode backup` file into this machine's (empty)
/// home — identity, genesis and the full op-log — then start its node
/// (`meshnode restore`, MASTERPLAN §4.5).
#[tauri::command]
async fn restore_backup(
    app: AppHandle,
    state: State<'_, Director>,
    token: String,
    node_name: String,
    contribute: bool,
) -> Result<RestoreView, String> {
    // The backup the person picked (SEC-14).
    let restore_path = state.picked.require(&token)?;
    if state.inner.lock().await.engine.is_some() {
        return Err(
            "this machine's node is running — a restore needs a machine with no mesh".into(),
        );
    }
    // **Blocking, so `spawn_blocking`.** Restore reads a whole backup and
    // replays every op into a fresh store: seconds of synchronous disk
    // work, and holding an async worker for the duration is how a runtime
    // loses one. The key Director signs with is the one in this home, and
    // restore puts the backup's there with care for whatever was there
    // before (`backup::place_user_key`).
    let home = engine::node_home();
    let report =
        tokio::task::spawn_blocking(move || mesh_core::backup::restore(&restore_path, &home))
            .await
            .map_err(|_| "the restore task panicked — see Director's log".to_string())?
            .map_err(|e| e.to_string())?;
    engine::save_node_settings(&node_name, None)?;
    let engine = connect(&app, &state, false).await?;
    // Best-effort: a restored node writes nothing until it has synced with a
    // peer, and the choice it had when it was backed up is in the op-log it
    // came back with.
    if let Err(e) = apply_contribute(&engine, contribute).await {
        tracing::info!("restore: contribute choice not applied yet ({e})");
    }
    Ok(RestoreView {
        mesh_name: report.mesh_name,
        ops: report.ops,
        members: report.members as u64,
    })
}

/// Wizard: join by LAN pairing — this machine announces itself and waits
/// for a member to approve with the code (MASTERPLAN §5.2), exactly as
/// `meshnode pair` does on a machine with no mesh.
#[tauri::command]
async fn wizard_pair_start(
    app: AppHandle,
    state: State<'_, Director>,
    node_name: String,
    contribute: bool,
    person: Option<String>,
    channel: Channel<WizardPairEvent>,
) -> Result<(), String> {
    if Genesis::load(&engine::node_home())
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("this machine is already in a mesh".into());
    }
    let config = engine::node_config(Some(&node_name), tags_for(contribute))?;

    let (code_tx, mut code_rx) = tokio::sync::mpsc::channel::<pairing::PairPrompt>(1);
    let (decision_tx, decision_rx) = oneshot::channel();
    let (listen_tx, listen_rx) = oneshot::channel::<std::net::SocketAddr>();
    let (announced_tx, announced_rx) = oneshot::channel::<Option<String>>();
    let cancel = tokio_util::sync::CancellationToken::new();
    // Held until the task below is registered, so a cancel that arrives in
    // between finds it rather than nothing, and a second start cannot slip
    // past the check.
    let mut inner = state.inner.lock().await;
    // One wait at a time: a second would announce this machine twice and
    // leave the first one's decision unreachable.
    if inner
        .wizard_pairing
        .as_ref()
        .is_some_and(|p| !p.task.is_finished())
    {
        return Err("already waiting to pair — cancel that first".into());
    }
    inner.wizard_decision = Some(decision_tx);

    let prompt_channel = channel.clone();
    tokio::spawn(async move {
        // The bound address, then — once announcing has been tried — the
        // one event the wizard draws from: every address this machine can
        // be dialled at, and whether mDNS is one of the ways it is being
        // found. An admin whose Director shows nothing can be read one of
        // those addresses and type it in.
        if let Ok(addr) = listen_rx.await {
            let mdns_error = announced_rx.await.unwrap_or(None);
            let addrs: Vec<String> = pairing::local_endpoints(addr.port())
                .into_iter()
                .map(|a| a.to_string())
                .collect();
            let shown = addrs.first().cloned().unwrap_or_else(|| addr.to_string());
            let _ = prompt_channel.send(WizardPairEvent::Listening {
                addr: shown,
                addrs,
                mdns_error,
            });
        }
    });
    let prompt_channel = channel.clone();
    tokio::spawn(async move {
        if let Some(prompt) = code_rx.recv().await {
            // The approver as the handshake proved it rather than as its
            // message named it (SEC-9); an honest approver names itself by
            // exactly this short id, so nothing changes on screen for one.
            let _ = prompt_channel.send(WizardPairEvent::Code {
                code: prompt.code,
                mesh_name: prompt.mesh_name,
                approver_name: prompt.approver_id.short(),
            });
        }
    });

    // Pairing binds a transport and speaks to a stranger over the network,
    // so it runs on the engine's runtime. Only the `channel` sends belong to
    // Director, and a channel send neither blocks nor waits.
    let stop = cancel.clone();
    let task = engine::spawn(async move {
        let ux = pairing::JoinerUx {
            on_code: code_tx,
            decision: decision_rx,
            on_listening: Some(listen_tx),
            person,
            on_announced: Some(announced_tx),
        };
        match pairing::pair_join_until(&config, ux, None, stop.clone().cancelled_owned()).await {
            Ok(report) => {
                if let Err(e) = engine::save_node_settings(&node_name, None) {
                    let _ = channel.send(WizardPairEvent::Failed { message: e });
                    return;
                }
                let director = app.state::<Director>();
                match connect(&app, &director, false).await {
                    Ok(_) => {
                        let _ = channel.send(WizardPairEvent::Joined {
                            mesh_name: report.mesh_name,
                        });
                    }
                    Err(message) => {
                        let _ = channel.send(WizardPairEvent::Failed { message });
                    }
                }
            }
            // Given up from the wizard: nothing failed, so nothing is said.
            Err(_) if stop.is_cancelled() => {
                let _ = channel.send(WizardPairEvent::Cancelled);
            }
            Err(e) => {
                let _ = channel.send(WizardPairEvent::Failed {
                    message: e.to_string(),
                });
            }
        }
    });
    inner.wizard_pairing = Some(engine::WizardPairing { cancel, task });
    Ok(())
}

/// Stop waiting to pair (DIR-6): the wizard's Cancel, and its Back.
///
/// Returns once the pairing has actually ended — its endpoint closed, its
/// announcement withdrawn — so the next way in the person picks does not
/// race it for the data dir. A pairing that completed before the cancel
/// reached it is left joined: the mesh exists by then, and the wizard hears
/// `joined` as usual.
#[tauri::command]
async fn wizard_pair_cancel(state: State<'_, Director>) -> Result<(), String> {
    let pairing = {
        let mut inner = state.inner.lock().await;
        // A code on screen is answered "no", as "They differ" would be;
        // the cancel below covers every other point in the wait.
        if let Some(tx) = inner.wizard_decision.take() {
            let _ = tx.send(false);
        }
        inner.wizard_pairing.take()
    };
    let Some(pairing) = pairing else {
        return Ok(());
    };
    pairing.cancel.cancel();
    // Bounded: a cancel lands at the pairing loop's next wake-up, which is
    // immediate — but a stuck teardown must not become a stuck window.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), pairing.task).await;
    Ok(())
}

/// The wizard human's verdict on the displayed pairing code.
#[tauri::command]
async fn wizard_pair_decide(state: State<'_, Director>, accept: bool) -> Result<(), String> {
    match state.inner.lock().await.wizard_decision.take() {
        Some(tx) => {
            let _ = tx.send(accept);
            Ok(())
        }
        None => Err("no pairing in progress".into()),
    }
}

// ---------------------------------------------------------------------------
// Views (poll-refreshed by the frontend)
// ---------------------------------------------------------------------------

async fn status_of(engine: &Engine) -> Result<StatusView, String> {
    let IpcResponse::Status(info) = engine.ipc(IpcRequest::Status).await? else {
        return Err("unexpected response".into());
    };
    // Whether this machine runs the mesh's work: its own row, as `meshnode
    // nodes` shows it. No row yet — a node a moment old — reads as the
    // default, which is that it does.
    let executing = match engine.ipc(IpcRequest::Nodes).await? {
        IpcResponse::Nodes(rows) => rows
            .iter()
            .find(|r| r.is_self)
            .is_none_or(|me| !me.tags.iter().any(|t| t == mesh_proto::NO_EXECUTE_TAG)),
        _ => true,
    };
    Ok(StatusView::from(info, executing))
}

#[tauri::command]
async fn status_view(state: State<'_, Director>) -> Result<StatusView, String> {
    status_of(&state.engine().await?).await
}

#[tauri::command]
async fn nodes_view(state: State<'_, Director>) -> Result<Vec<NodeView>, String> {
    let IpcResponse::Nodes(rows) = state.engine().await?.ipc(IpcRequest::Nodes).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

// ---------------------------------------------------------------------------
// Content store and disk (M31 A2)
// ---------------------------------------------------------------------------

/// What this Director's own node is holding, and what it would reclaim.
#[tauri::command]
async fn content_status(state: State<'_, Director>) -> Result<view::ContentStatusView, String> {
    let IpcResponse::ContentStatus(v) =
        state.engine().await?.ipc(IpcRequest::ContentStatus).await?
    else {
        return Err("unexpected response".into());
    };
    Ok(view::ContentStatusView::from_ipc(*v))
}

/// Reclaim now. `dry_run` is what the settings screen calls to fill in
/// *"this would free 571 MB"* before anybody presses anything.
#[tauri::command]
async fn content_prune(
    state: State<'_, Director>,
    dry_run: bool,
    aggressive: bool,
) -> Result<view::ContentPruneView, String> {
    let IpcResponse::ContentPrune(v) = state
        .engine()
        .await?
        .ipc(IpcRequest::ContentPrune {
            dry_run,
            aggressive,
        })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok((*v).into())
}

/// Set the tidiness level: `[retention] content` and `content_max_mb` in the
/// node's `meshnode.toml`, then `systemctl --user reload meshnode` — what an
/// operator at a terminal would do, and so what this does.
///
/// Both halves, in that order, and neither is optional: saving without
/// applying means the setting does nothing until a restart, and applying
/// without saving means it does nothing *after* one. A node started by hand
/// in a terminal has no unit to reload through, and picks the file up at its
/// next start; the screen then still shows what is in effect, not what was
/// asked for.
#[tauri::command]
async fn set_content_policy(
    state: State<'_, Director>,
    policy: String,
    max_mb: u64,
) -> Result<view::ContentStatusView, String> {
    use mesh_core::config::Setting;
    let parsed = mesh_proto::ContentPolicy::parse(&policy)
        .ok_or_else(|| format!("{policy:?} is not a content policy"))?;
    let engine = state.engine().await?;
    let config = mesh_core::layout::Layout::new(engine::node_home()).config_file();
    let max = match max_mb {
        0 => Setting::Unset,
        mb => Setting::Number(i64::try_from(mb).unwrap_or(i64::MAX)),
    };
    let edits = [
        (
            "retention.content",
            Setting::Text(parsed.as_str().to_string()),
        ),
        ("retention.content_max_mb", max),
    ];
    let reloaded = tokio::task::spawn_blocking(move || {
        mesh_core::config::set_in_file(&config, &edits)?;
        mesh_core::usernode::reload()
    })
    .await
    .map_err(|_| "saving the setting panicked — see Director's log".to_string())?
    .map_err(|e| e.to_string())?;
    // A reload is a signal, answered a moment later; show the new level
    // once the node is using it rather than the old one a moment early.
    for _ in 0..if reloaded { 20 } else { 0 } {
        if let Ok(IpcResponse::ContentStatus(v)) = engine.ipc(IpcRequest::ContentStatus).await
            && v.policy == parsed.as_str()
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    content_status(state).await
}

/// Free space on every node the mesh can see, worst first.
#[tauri::command]
async fn disk_view(state: State<'_, Director>) -> Result<Vec<view::DiskView>, String> {
    let IpcResponse::DiskReport(rows) = state.engine().await?.ipc(IpcRequest::DiskReport).await?
    else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// `meshnode nodes --devices`: every accelerator in the mesh (C5 P7).
///
/// Answered from replicated state, so Director shows the whole mesh's
/// hardware from whichever node it happens to be talking to.
#[tauri::command]
async fn devices_view(state: State<'_, Director>) -> Result<Vec<DeviceView>, String> {
    let IpcResponse::Devices(rows) = state
        .engine()
        .await?
        .ipc(IpcRequest::Devices { node: None })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// `meshnode top`: per-node slots / running / queued.
#[tauri::command]
async fn top_view(state: State<'_, Director>) -> Result<Vec<TopView>, String> {
    let IpcResponse::Top(rows) = state.engine().await?.ipc(IpcRequest::Top).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// `meshnode totals`: what the whole mesh adds up to (M9 §6.3).
#[tauri::command]
async fn totals_view(state: State<'_, Director>) -> Result<TotalsView, String> {
    let IpcResponse::Totals(totals) = state.engine().await?.ipc(IpcRequest::Totals).await? else {
        return Err("unexpected response".into());
    };
    Ok((*totals).into())
}

/// *"Has anything happened?"* (**H-8**).
///
/// One counter per topic. The frontend keeps the last vector it saw and
/// refetches only the collections whose counter moved — so an idle mesh
/// costs one small request per tick instead of eight to eighteen whole
/// tables built under the engine's state lock.
#[tauri::command]
async fn state_versions_view(state: State<'_, Director>) -> Result<StateVersionsView, String> {
    let IpcResponse::StateVersions(v) = state.engine().await?.ipc(IpcRequest::StateVersion).await?
    else {
        return Err("unexpected response".into());
    };
    Ok(v.into())
}

/// One page of the job table (**H-1**).
///
/// Was `IpcRequest::Jobs` — every job the mesh remembers, rebuilt from
/// scratch every two seconds, with a `stat()` per row inside the engine's
/// state lock, and a payload that stopped fitting in one frame at roughly
/// twenty thousand jobs. Now a bounded query the engine answers by
/// narrowing first, so the cost is proportional to what is on screen.
///
/// `counts` is over the *whole* table regardless of the filter, so the tab
/// counters above a paged list still tell the truth.
#[tauri::command]
async fn jobs_view(
    state: State<'_, Director>,
    // Named `filter` rather than `state` because `state` is the Tauri
    // handle; the wire name is what the frontend sends.
    filter: Option<String>,
    contains: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<JobsPageView, String> {
    let query = mesh_proto::JobsQuery {
        state: filter.filter(|s| !s.is_empty() && s != "all"),
        contains: contains.filter(|s| !s.is_empty()),
        limit: limit.unwrap_or(0),
        offset: offset.unwrap_or(0),
        ..Default::default()
    };
    let IpcResponse::JobsPage(page) = state
        .engine()
        .await?
        .ipc(IpcRequest::JobsQuery {
            query: Box::new(query),
        })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok((*page).into())
}

// -- history and archive (M14) ----------------------------------------------
//
// Two tiers, deliberately visible as two things in the UI.
//
// `history_*` reads the local redb index: instant, always current, covers
// whatever this node has seen. `archive_*` reads Parquet on disk: covers
// everything ever ingested, answers arbitrary SQL, and is a file somebody
// can back up. Collapsing them into one "history" screen would hide the
// distinction that actually matters — one is derived and disposable, the
// other is the copy you keep.

/// Search the local index. Every filter is optional; empty means "recent".
//
// One parameter per filter rather than a struct: Tauri commands take named
// arguments from the JS side, and a struct would make the frontend build an
// object literal whose field names it cannot typo-check.

#[tauri::command]
async fn history_jobs(
    state: State<'_, Director>,
    job_state: Option<String>,
    since_micros: Option<String>,
    until_micros: Option<String>,
    contains: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
) -> Result<HistoryJobPageView, String> {
    let query = mesh_proto::HistoryQuery {
        state: non_empty(job_state),
        user: None,
        node: None,
        since_micros: parse_micros(since_micros),
        until_micros: parse_micros(until_micros),
        contains: non_empty(contains),
        limit: limit.unwrap_or(50),
        offset: offset.unwrap_or(0),
    };
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::HistoryJobs(Box::new(query)))
        .await?;
    let IpcResponse::HistoryJobs(page) = resp else {
        return Err(unexpected(resp));
    };
    Ok(HistoryJobPageView {
        tiers: TierView::of(page.tiers, page.absence),
        rows: page.rows.into_iter().map(HistoryJobView::from).collect(),
        total: page.total.map(|t| t.min(u64::from(u32::MAX)) as u32),
    })
}

/// Read one window of a job's output back from wherever a sink put it
/// (M31 B3 §14.3).
///
/// **Director never opens the connection.** It names a job; the daemon
/// looks up the locator, elects the node that exported it, and that node —
/// the one with the route and the credential — makes the request. So this
/// works on a laptop with no path to the warehouse, and a read is governed
/// by the same `egress_allow` as the write that put the bytes there.
///
/// One call per window. The daemon audits the *open* (`from_end == 0`) and
/// nothing else, so scrolling is free.
#[tauri::command]
async fn history_replay(
    state: State<'_, Director>,
    job: String,
    from_end: Option<String>,
    limit: Option<u32>,
    search: Option<String>,
) -> Result<PayloadWindowView, String> {
    let job_id = mesh_proto::JobId::parse(&job).map_err(|e| e.to_string())?;
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::HistoryReplay {
            job_id,
            from_end: from_end.and_then(|v| v.parse().ok()).unwrap_or(0),
            limit: u64::from(limit.unwrap_or(500)),
            search: search.unwrap_or_default(),
        })
        .await?;
    let IpcResponse::PayloadWindow(window) = resp else {
        return Err(unexpected(resp));
    };
    Ok((*window).into())
}

// -- M31: retention and history export ---------------------------------------

/// How long this mesh keeps things, and what that settles the log at.
#[tauri::command]
async fn retention_view(state: State<'_, Director>) -> Result<view::RetentionView, String> {
    let resp = state.engine().await?.ipc(IpcRequest::RetentionShow).await?;
    match resp {
        IpcResponse::Retention(v) => Ok((*v).into()),
        other => Err(unexpected(other)),
    }
}

/// Sign and publish a retention policy (admin).
///
/// **Edited on top of what the mesh currently has, never built from
/// defaults.** A screen that sends only the tables it rendered would
/// silently reset the ones it did not — and the one it would most likely
/// not render is `audit`.
#[tauri::command]
async fn retention_put(
    state: State<'_, Director>,
    windows: Vec<String>,
) -> Result<view::RetentionView, String> {
    let engine = state.engine().await?;
    let live = match engine.ipc(IpcRequest::RetentionShow).await? {
        IpcResponse::Retention(v) => v,
        other => return Err(unexpected(other)),
    };
    let edited = mesh_core::retention::apply_edits(&live.policy, &windows, &[])
        .map_err(|e| e.to_string())?;
    if let Some(why) = edited.refusal() {
        return Err(why);
    }
    let signed = mesh_core::retention::sign(&engine.user, edited, mesh_proto::now_micros());
    engine
        .ipc(IpcRequest::RetentionPut {
            policy: Box::new(signed),
        })
        .await?;
    retention_view(state).await
}

/// Withdraw the policy, back to the shipped defaults (admin).
#[tauri::command]
async fn retention_reset(state: State<'_, Director>) -> Result<view::RetentionView, String> {
    let engine = state.engine().await?;
    let at = mesh_proto::now_micros();
    let signature = mesh_core::retention::sign_reset(&engine.user, at);
    engine
        .ipc(IpcRequest::RetentionReset {
            at_micros: at,
            signature,
        })
        .await?;
    retention_view(state).await
}

/// Every sink, with how far behind it is and what it last said.
/// What this node is refusing, and what is churning (M31 A3).
///
/// Polled beside the retention view because it answers the other half of
/// the same question: retention says how big the log settles, and this says
/// whether anything is currently trying to outrun it.
#[tauri::command]
async fn backpressure_view(state: State<'_, Director>) -> Result<BackpressureView, String> {
    let resp = state.engine().await?.ipc(IpcRequest::Backpressure).await?;
    let IpcResponse::Backpressure(view) = resp else {
        return Err(unexpected(resp));
    };
    Ok((*view).into())
}

#[tauri::command]
async fn sinks_view(state: State<'_, Director>) -> Result<Vec<view::SinkView>, String> {
    let resp = state.engine().await?.ipc(IpcRequest::Sinks).await?;
    match resp {
        IpcResponse::Sinks(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(unexpected(other)),
    }
}

/// The destination list, and what each one gets you afterwards.
///
/// Derived from the capability triple on the enum rather than written in
/// the UI, so a destination added later cannot ship without a sentence
/// (§14.4). No engine call: it is a property of the build.
#[tauri::command]
fn sink_kinds() -> Vec<view::SinkKindView> {
    mesh_proto::SinkKind::ALL
        .into_iter()
        .map(|k| {
            let caps = k.capabilities();
            view::SinkKindView {
                kind: k.as_str().to_string(),
                label: match k {
                    mesh_proto::SinkKind::Ndjson => "NDJSON file",
                    mesh_proto::SinkKind::Plugin => "Your own program",
                    mesh_proto::SinkKind::ClickHouse => "ClickHouse",
                    mesh_proto::SinkKind::VictoriaLogs => "VictoriaLogs",
                    mesh_proto::SinkKind::OpenSearch => "OpenSearch / Elasticsearch",
                    mesh_proto::SinkKind::Webhook => "Webhook",
                    mesh_proto::SinkKind::Postgres => "Postgres / TimescaleDB",
                    mesh_proto::SinkKind::SplunkHec => "Splunk (HEC)",
                    mesh_proto::SinkKind::Otlp => "OpenTelemetry (OTLP)",
                }
                .to_string(),
                capability: k.capability_sentence().to_string(),
                needs_url: k.needs_url(),
                payloads_by_default: k.payloads_by_default(),
                archives: caps.archives,
                replay_payloads: caps.replay_payloads,
            }
        })
        .collect()
}

/// Create or replace a sink (admin).
///
/// The credential arrives as a secret **name** and leaves as a
/// `{{secret:name}}` reference. No value passes through Director, which is
/// C6 expressed as an absence rather than as a warning.
#[tauri::command]
async fn sink_put(state: State<'_, Director>, draft: view::SinkDraft) -> Result<(), String> {
    let engine = state.engine().await?;
    let kind = mesh_proto::SinkKind::parse(&draft.kind)
        .ok_or_else(|| format!("'{}' is not a destination", draft.kind))?;
    let tables = mesh_proto::SinkTables::parse(&draft.tables.join(","))?;

    let sink = mesh_proto::Sink {
        name: draft.name.trim().to_string(),
        kind,
        url: draft.url.trim().to_string(),
        database: draft.database.trim().to_string(),
        credential: match draft.credential.trim() {
            "" => String::new(),
            name => format!("{{{{secret:{name}}}}}"),
        },
        read_credential: match draft.read_credential.trim() {
            "" => String::new(),
            name => format!("{{{{secret:{name}}}}}"),
        },
        tables,
        records: Default::default(),
        payloads: mesh_proto::SinkPayloads {
            enabled: draft.payloads,
            max_bytes: draft.payload_max_mb.saturating_mul(1024 * 1024),
        },
        on_failure: mesh_proto::SinkOnFailure {
            hold_for_export_days: draft.hold_for_export_days,
            ..Default::default()
        },
        run_on: draft.run_on.trim().to_string(),
        enabled: draft.enabled,
        description: draft.description,
        created_by: engine.user.user_id(),
        created_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    // Refused here as well as on the daemon, so the person who typed it
    // gets the reason rather than a rejection two layers down.
    if let Some(why) = sink.refusal() {
        return Err(why);
    }
    let signed = mesh_core::sinks::sign(&engine.user, sink);
    engine
        .ipc(IpcRequest::SinkPut {
            sink: Box::new(signed),
        })
        .await?;
    Ok(())
}

/// Retire a sink (admin). Rows already exported are untouched.
#[tauri::command]
async fn sink_remove(state: State<'_, Director>, name: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let signature = mesh_core::sinks::sign_remove(&engine.user, &name);
    engine
        .ipc(IpcRequest::SinkRemove { name, signature })
        .await?;
    Ok(())
}

/// POST one synthetic row and return what came back, verbatim.
///
/// **Takes the form, not a name**, so the operator learns about a wrong
/// token *in the dialog* rather than from a lag graph two hours later —
/// which is what "test before save" has to mean. Nothing is authored and
/// nothing is signed: the draft is used for its URL, its kind and its
/// credential reference, then discarded.
#[tauri::command]
async fn sink_test(state: State<'_, Director>, draft: view::SinkDraft) -> Result<String, String> {
    let engine = state.engine().await?;
    let kind = mesh_proto::SinkKind::parse(&draft.kind)
        .ok_or_else(|| format!("'{}' is not a destination", draft.kind))?;
    let sink = mesh_proto::Sink {
        name: draft.name.trim().to_string(),
        kind,
        url: draft.url.trim().to_string(),
        database: draft.database.trim().to_string(),
        credential: match draft.credential.trim() {
            "" => String::new(),
            name => format!("{{{{secret:{name}}}}}"),
        },
        read_credential: match draft.read_credential.trim() {
            "" => String::new(),
            name => format!("{{{{secret:{name}}}}}"),
        },
        tables: mesh_proto::SinkTables::parse(&draft.tables.join(",")).unwrap_or_default(),
        records: Default::default(),
        payloads: Default::default(),
        on_failure: Default::default(),
        // **From the form, not blank.** A storage probe assembles the pool
        // from `run_on`, so testing with an empty selector would check the
        // whole mesh and report on a sink the operator is not creating —
        // passing a form that will fail, or failing one that would work.
        run_on: draft.run_on.trim().to_string(),
        enabled: true,
        description: String::new(),
        created_by: engine.user.user_id(),
        created_at_micros: 0,
        signature: Vec::new(),
    };
    let resp = engine
        .ipc(IpcRequest::SinkTest {
            sink: Box::new(sink),
        })
        .await?;
    match resp {
        IpcResponse::SinkTest { ok: true, status } => Ok(status),
        IpcResponse::SinkTest { ok: false, status } => Err(status),
        other => Err(unexpected(other)),
    }
}

/// Release the restore latch on this node (admin).
#[tauri::command]
async fn sinks_resume(state: State<'_, Director>) -> Result<(), String> {
    state.engine().await?.ipc(IpcRequest::SinksResume).await?;
    Ok(())
}

// A SQL surface used to live here — `archive_query(sql)` and its
// `archive_examples` companion — registered in `invoke_handler` and invoked
// by nothing. Both are gone (S4).
//
// Two reasons, and the second is the one that matters. It had no caller: no
// Svelte component sent a query, so removing it costs the product nothing.
// And it was a *query engine* behind a command any script running in the
// window could call — the archive's `SessionContext` shipped with DDL and
// `COPY` on, which made "ask a question about history" a way to read and
// write files as the Director user. `Archive::sql` refuses those now, but
// the right shape for a loaded surface with no user is not a safer version
// of it.
//
// The supported SQL surface is the `meshware-archive` binary, which is
// where a query engine belongs: a separate process, run by a person, over
// the same read-only `Archive::sql`.

/// A blank filter box means "no filter", not "match the empty string".
fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Timestamps cross this boundary as strings (JSON numbers are f64).
fn parse_micros(value: Option<String>) -> Option<u64> {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
}

fn unexpected(resp: IpcResponse) -> String {
    match resp {
        IpcResponse::Error { message } => message,
        other => format!("unexpected response: {other:?}"),
    }
}

#[tauri::command]
async fn services_view(state: State<'_, Director>) -> Result<Vec<ServiceView>, String> {
    let IpcResponse::Services(rows) = state.engine().await?.ipc(IpcRequest::Services).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

#[tauri::command]
async fn bundles_view(state: State<'_, Director>) -> Result<Vec<BundleView>, String> {
    let IpcResponse::Bundles(rows) = state.engine().await?.ipc(IpcRequest::Bundles).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

#[tauri::command]
async fn sources_view(state: State<'_, Director>) -> Result<Vec<SourceView>, String> {
    let IpcResponse::Sources(rows) = state.engine().await?.ipc(IpcRequest::Sources).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

#[tauri::command]
async fn secrets_view(state: State<'_, Director>) -> Result<Vec<SecretView>, String> {
    let IpcResponse::Secrets(rows) = state.engine().await?.ipc(IpcRequest::Secrets).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

#[tauri::command]
async fn secret_providers_view(state: State<'_, Director>) -> Result<Vec<ProviderView>, String> {
    let IpcResponse::Providers(rows) = state
        .engine()
        .await?
        .ipc(IpcRequest::SecretProviders)
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

#[tauri::command]
async fn users_view(state: State<'_, Director>) -> Result<Vec<UserView>, String> {
    let IpcResponse::Users(rows) = state.engine().await?.ipc(IpcRequest::Users).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

#[tauri::command]
async fn grants_view(state: State<'_, Director>) -> Result<Vec<GrantView>, String> {
    let IpcResponse::Grants(rows) = state.engine().await?.ipc(IpcRequest::Grants).await? else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// `meshnode audit`: filters are optional; `since_secs` is relative to now.
#[tauri::command]
async fn audit_query(
    state: State<'_, Director>,
    user: Option<String>,
    action: Option<String>,
    since_secs: Option<u64>,
    limit: u32,
) -> Result<Vec<AuditView>, String> {
    let since_micros = since_secs
        .filter(|s| *s > 0)
        .map(|s| now_micros().saturating_sub(s.saturating_mul(1_000_000)));
    let IpcResponse::Audit(entries) = state
        .engine()
        .await?
        .ipc(IpcRequest::AuditQuery {
            user: user.filter(|u| !u.trim().is_empty()),
            action: action.filter(|a| !a.trim().is_empty()),
            since_micros,
            limit: limit.max(1),
        })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(entries.into_iter().map(Into::into).collect())
}

#[tauri::command]
async fn whoami_view(state: State<'_, Director>) -> Result<WhoAmIView, String> {
    let IpcResponse::WhoAmI(info) = state.engine().await?.ipc(IpcRequest::WhoAmI).await? else {
        return Err("unexpected response".into());
    };
    Ok(info.into())
}

/// Live "this will hit N nodes" preview for the Run view (§14.2).
#[tauri::command]
async fn target_preview(
    state: State<'_, Director>,
    selector: String,
) -> Result<Vec<NodeView>, String> {
    let IpcResponse::Nodes(rows) = state
        .engine()
        .await?
        .ipc(IpcRequest::TargetPreview { selector })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// The owner's decision about whether one node may be given work.
///
/// **The other half of the contribute question, and the half that belongs
/// to the mesh rather than to the machine.** `set_contribute` below is this
/// peer talking about itself, which any machine may do about its own
/// cores; this is an admin saying whether the offer is accepted, and it is
/// the control that makes "somebody joined from a laptop I have never seen"
/// a survivable event. Signed here and enforced on every peer.
#[tauri::command]
async fn node_contribute_set(
    state: State<'_, Director>,
    node_id: String,
    allowed: bool,
    reason: String,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let node_id = mesh_proto::NodeId::parse(&node_id).map_err(|e| e.to_string())?;
    let mut decision = mesh_proto::NodeExecution {
        node_id,
        allowed,
        decided_by: engine.user.user_id(),
        decided_at_micros: now_micros(),
        reason: reason.trim().to_string(),
        signature: Vec::new(),
    };
    decision.signature = engine.sign(&decision.signing_bytes())?;
    engine
        .ipc(IpcRequest::NodeContributeSet {
            decision: Box::new(decision),
        })
        .await?;
    Ok(())
}

/// Isolate a suspected node, or lift it (E2.4).
///
/// Distinct from `node_contribute_set` above, and the distinction is the
/// whole feature: that one is a capacity decision and still lets somebody
/// shell in, this one says *nobody go near it*.
#[tauri::command]
async fn node_quarantine_set(
    state: State<'_, Director>,
    node_id: String,
    quarantined: bool,
    reason: String,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let node_id = mesh_proto::NodeId::parse(&node_id).map_err(|e| e.to_string())?;
    if quarantined && reason.trim().is_empty() {
        return Err(
            "quarantine needs a reason — the next person reads it before deciding \
                    whether to lift it"
                .into(),
        );
    }
    let mut quarantine = mesh_proto::NodeQuarantine {
        node_id,
        quarantined,
        reason: reason.trim().to_string(),
        by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    quarantine.signature = engine.sign(&quarantine.signing_bytes())?;
    engine
        .ipc(IpcRequest::NodeQuarantine { quarantine })
        .await?;
    Ok(())
}

/// Assign tags to another node, in the mesh.
///
/// Distinct from `tags_update` above, and the distinction is which half of
/// a node's tag list is being written: that one edits *this* peer's own
/// announcement, which is the only list a machine can speak for; this one
/// is the operator's label on somebody else's machine, signed here,
/// replicated, and kept through that machine's reboots and rebuilds.
///
/// The assignment is total — Director sends the list it wants that node to
/// have, and the caller has computed it from `assignedTags` on the row.
#[tauri::command]
async fn node_tags_assign(
    state: State<'_, Director>,
    node_id: String,
    tags: Vec<String>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let node_id = mesh_proto::NodeId::parse(&node_id).map_err(|e| e.to_string())?;
    // Trimmed here so a stray space in a text field cannot become a tag
    // nothing can ever select, and de-duplicated so the list is the set it
    // looks like.
    let mut tags: Vec<String> = tags
        .into_iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    tags.dedup_by(|a, b| a == b);
    let mut seen = std::collections::BTreeSet::new();
    tags.retain(|t| seen.insert(t.clone()));
    // Refused before the round trip, with the daemon's own words, so the
    // field says why rather than the toast saying it a second later.
    for tag in &tags {
        mesh_proto::assignable_tag(tag)?;
    }
    let mut assignment = mesh_proto::NodeTags {
        node_id,
        tags,
        by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    assignment.signature = engine.sign(&assignment.signing_bytes())?;
    engine.ipc(IpcRequest::NodeTags { assignment }).await?;
    Ok(())
}

/// `meshnode nodes drain` (Phase C, E9/U9): empty another node without
/// stopping it, or lift the drain.
///
/// The same two assigned tags the CLI writes — `drain.by=` and, when asked,
/// `drain.keep-system` — computed here against what that node is assigned
/// now, so every other assigned tag survives. `deadline_secs` is from now;
/// `None` is no deadline.
#[tauri::command]
async fn node_drain_set(
    state: State<'_, Director>,
    node_id: String,
    deadline_secs: Option<u64>,
    keep_system: bool,
    lift: bool,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let node_id = mesh_proto::NodeId::parse(&node_id).map_err(|e| e.to_string())?;
    let IpcResponse::AssignedTags { tags: current, .. } =
        engine.ipc(IpcRequest::AssignedTags { node_id }).await?
    else {
        return Err("could not read the tags assigned to that node".into());
    };
    let mut tags: Vec<String> = current
        .into_iter()
        .filter(|t| !mesh_proto::is_drain_tag(t))
        .collect();
    if !lift {
        let at =
            deadline_secs.map(|secs| now_micros().saturating_add(secs.saturating_mul(1_000_000)));
        tags.extend(mesh_proto::drain_tags(at, keep_system));
    }
    let mut assignment = mesh_proto::NodeTags {
        node_id,
        tags,
        by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    assignment.signature = engine.sign(&assignment.signing_bytes())?;
    engine.ipc(IpcRequest::NodeTags { assignment }).await?;
    Ok(())
}

/// `meshnode deployment placement` (Phase C, U8): every machine with the
/// deployment instances on it and what each asked for — Mesh → Placement.
#[tauri::command]
async fn placement_view(
    state: State<'_, Director>,
) -> Result<Vec<view::PlacementNodeView>, String> {
    let resp = state.engine().await?.ipc(IpcRequest::PlacementMap).await?;
    match resp {
        IpcResponse::PlacementMap(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Mint a machine principal and hand back its private key **once** (E1-P4).
///
/// The key is generated here, in Director, and returned to the caller for
/// the one moment it exists outside this function. Director writes it to a
/// file the operator chooses and never keeps a copy — the same contract
/// `meshnode serviceaccounts create` has.
#[tauri::command]
async fn service_account_create(
    state: State<'_, Director>,
    name: String,
    role: String,
    nodes: String,
    services: String,
    expires_in_days: Option<u32>,
    description: String,
) -> Result<crate::view::ServiceAccountCreated, String> {
    let engine = state.engine().await?;
    let role = mesh_proto::Role::parse(&role).ok_or("unknown role")?;
    if role == mesh_proto::Role::Admin {
        return Err(
            "a service account may not hold the admin role — an admin key can mint \
                    users and grants, so a leaked pipeline credential holding it is permanent \
                    mesh compromise"
                .into(),
        );
    }
    let expires_at_micros =
        expires_in_days.map(|d| now_micros() + u64::from(d) * 86_400 * 1_000_000);

    let kp = mesh_core::identity::Keypair::generate().map_err(|e| e.to_string())?;
    let user_id = kp.user_id();

    let mut record = mesh_proto::UserRecord {
        user_id,
        username: name.clone(),
        created_at_micros: now_micros(),
        added_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    record.signature = engine.sign(&record.signing_bytes())?;
    engine.ipc(IpcRequest::UserAdd { record }).await?;

    let mut account = mesh_proto::ServiceAccount {
        user_id,
        name: name.clone(),
        description,
        created_at_micros: now_micros(),
        created_by: engine.user.user_id(),
        expires_at_micros,
        signature: Vec::new(),
    };
    account.signature = engine.sign(&account.signing_bytes())?;
    engine
        .ipc(IpcRequest::ServiceAccountAdd { account })
        .await?;

    let mut grant = mesh_proto::Grant {
        grant_id: mesh_proto::GrantId::generate(),
        user_id,
        role,
        nodes,
        services,
        expires_at_micros,
        created_at_micros: now_micros(),
        granted_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    grant.signature = engine.sign(&grant.signing_bytes())?;
    engine.ipc(IpcRequest::GrantAdd { grant }).await?;

    Ok(crate::view::ServiceAccountCreated {
        name,
        user_id: user_id.to_display_string(),
        // Raw seed bytes, exactly what `identity/user.key` holds, so the
        // receiving system needs no decoding step.
        // A service account is generated here and is a software key by
        // construction (landmine 2: a job has no finger to press), so this
        // cannot be the hardware case — but it says so rather than
        // assuming it.
        key: kp.seed_bytes().map_err(|e| e.to_string())?.to_vec(),
        expires_at_micros,
    })
}

/// Write a freshly minted machine credential to a file the operator picks,
/// mode 0600 (E1-P4).
///
/// **A file, never the clipboard and never the screen.** A private key in a
/// screenshot, a scrollback buffer or a paste history is a private key
/// somebody else has. Director holds it only as long as this call.
#[tauri::command]
async fn save_key_file(
    app: tauri::AppHandle,
    suggested: String,
    key: Vec<u8>,
) -> Result<Option<String>, String> {
    use tauri_plugin_dialog::DialogExt as _;
    if key.len() != 32 {
        return Err("that is not a meshware identity key".into());
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_file_name(&suggested)
        .set_title("Save the service account's key")
        .save_file(move |p| {
            let _ = tx.send(p);
        });
    let Some(path) = rx.await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let path = path
        .into_path()
        .map_err(|e| format!("that path cannot be written: {e}"))?;
    std::fs::write(&path, &key).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    use std::os::unix::fs::PermissionsExt as _;
    // Best effort: on a filesystem that cannot express it (a FAT USB
    // stick), the write still happened and the operator should hear about
    // the permissions rather than lose the key.
    if let Err(e) = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)) {
        tracing::warn!("could not set 0600 on {}: {e}", path.display());
    }
    Ok(Some(path.display().to_string()))
}

/// Ask the person where to save something, from **Rust** (S3).
///
/// The window is a web page, and a path that arrives from a web page is a
/// path any script running in that page can choose. `save_key_file` already
/// had this shape and the six sibling commands did not; this is that shape,
/// factored out, so "where does this go" is answered by the desktop's own
/// chooser and the backend only ever sees what the chooser returned.
///
/// `None` means the person cancelled — not an error, and every caller
/// reports it as nothing having happened.
pub(crate) async fn ask_where_to_save(
    app: &tauri::AppHandle,
    title: &str,
    suggested: &str,
    filter: Option<(&str, &[&str])>,
) -> Result<Option<std::path::PathBuf>, String> {
    use tauri_plugin_dialog::DialogExt as _;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog().file().set_title(title);
    // A suggested name a caller composed from mesh or workflow names, so it
    // is sanitised rather than trusted: a separator in it would move the
    // dialog somewhere the person did not ask to be.
    builder = builder.set_file_name(safe_file_name(suggested));
    if let Some((name, exts)) = filter {
        builder = builder.add_filter(name, exts);
    }
    builder.save_file(move |p| {
        let _ = tx.send(p);
    });
    let Some(path) = rx.await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    path.into_path()
        .map(Some)
        .map_err(|e| format!("that path cannot be written: {e}"))
}

/// Ask the person which file to open, from Rust. The counterpart of
/// [`ask_where_to_save`] for the read half.
async fn ask_which_file(
    app: &tauri::AppHandle,
    title: &str,
    filter: Option<(&str, &[&str])>,
) -> Result<Option<std::path::PathBuf>, String> {
    use tauri_plugin_dialog::DialogExt as _;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog().file().set_title(title);
    if let Some((name, exts)) = filter {
        builder = builder.add_filter(name, exts);
    }
    builder.pick_file(move |p| {
        let _ = tx.send(p);
    });
    let Some(path) = rx.await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    path.into_path()
        .map(Some)
        .map_err(|e| format!("that path cannot be read: {e}"))
}

/// Open the native file dialog **from here** and hand the window a token
/// for what was picked (SEC-14).
#[tauri::command]
async fn pick_file(
    state: State<'_, Director>,
    app: AppHandle,
    title: String,
    extensions: Option<Vec<String>>,
) -> Result<Option<view::PickedView>, String> {
    let exts: Vec<&str> = extensions
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(String::as_str)
        .collect();
    let filter = (!exts.is_empty()).then_some((title.as_str(), exts.as_slice()));
    Ok(ask_which_file(&app, &title, filter)
        .await?
        .map(|p| state.picked.remember_view(&p)))
}

/// [`pick_file`], for a folder.
#[tauri::command]
async fn pick_directory(
    state: State<'_, Director>,
    app: AppHandle,
    title: String,
) -> Result<Option<view::PickedView>, String> {
    use tauri_plugin_dialog::DialogExt as _;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title(&title).pick_folder(move |p| {
        let _ = tx.send(p);
    });
    let Some(path) = rx.await.map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let path = path
        .into_path()
        .map_err(|e| format!("that folder cannot be read: {e}"))?;
    Ok(Some(state.picked.remember_view(&path)))
}

/// Reduce a suggested filename to a filename.
///
/// Suggestions are composed from names the mesh holds — a mesh name, a
/// workflow name, a node name — which are strings other people chose. The
/// dialog is the authority on *where*; this makes sure the suggestion can
/// only ever influence *what it is called*.
fn safe_file_name(suggested: &str) -> String {
    let cleaned: String = suggested
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim().to_string();
    if cleaned.is_empty() {
        "meshware".into()
    } else {
        cleaned
    }
}

/// Every machine principal, with the clock each one is on.
#[tauri::command]
async fn service_accounts(
    state: State<'_, Director>,
) -> Result<Vec<crate::view::ServiceAccountView>, String> {
    let engine = state.engine().await?;
    match engine.ipc(IpcRequest::ServiceAccounts).await? {
        IpcResponse::ServiceAccounts(rows) => Ok(rows
            .iter()
            .map(crate::view::ServiceAccountView::of)
            .collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Replace a person's key and carry their roles across (E2.3).
///
/// Director does the same four ops in the same order the CLI does, and for
/// the same reason: bind, grant, then revoke, so the person can act at
/// every moment in between.
#[tauri::command]
async fn user_rotate_key(
    state: State<'_, Director>,
    username: String,
    new_key: String,
    reason: String,
) -> Result<u32, String> {
    let engine = state.engine().await?;
    let new_id = mesh_proto::UserId::parse(&new_key).map_err(|e| e.to_string())?;
    rotate_key(&engine, username, new_id, reason).await
}

/// [`user_rotate_key`]'s ops, signed by `engine`'s key: the person keeps
/// their name and roles, and `new_id` replaces the key they hold. Returns
/// how many grants were carried. Also how Settings moves its own account
/// onto a security key (`security_key::security_key_use`).
async fn rotate_key(
    engine: &Engine,
    username: String,
    new_id: UserId,
    reason: String,
) -> Result<u32, String> {
    let IpcResponse::Users(users) = engine.ipc(IpcRequest::Users).await? else {
        return Err("could not read the user list".into());
    };
    let old = users
        .iter()
        .find(|u| u.username == username && !u.revoked)
        .ok_or_else(|| format!("no active user called {username:?}"))?;
    if old.user_id == new_id {
        return Err(format!("that is the key {username} already holds"));
    }
    let IpcResponse::Grants(grants) = engine.ipc(IpcRequest::Grants).await? else {
        return Err("could not read grants".into());
    };
    let carried: Vec<mesh_proto::Grant> = grants
        .iter()
        .filter(|g| g.user_id == old.user_id && !g.revoked && !g.expired)
        .map(|g| -> Result<mesh_proto::Grant, String> {
            let mut grant = mesh_proto::Grant {
                grant_id: mesh_proto::GrantId::generate(),
                user_id: new_id,
                role: mesh_proto::Role::parse(&g.role).unwrap_or(mesh_proto::Role::Viewer),
                nodes: g.nodes.clone(),
                services: g.services.clone(),
                expires_at_micros: g.expires_at_micros,
                created_at_micros: now_micros(),
                granted_by: engine.user.user_id(),
                signature: Vec::new(),
            };
            grant.signature = engine.sign(&grant.signing_bytes())?;
            Ok(grant)
        })
        .collect::<Result<_, String>>()?;
    let n = carried.len() as u32;

    let mut record = mesh_proto::UserRecord {
        user_id: new_id,
        username: username.clone(),
        created_at_micros: now_micros(),
        added_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    record.signature = engine.sign(&record.signing_bytes())?;

    let mut revocation = mesh_proto::UserRevocation {
        user_id: old.user_id,
        reason,
        revoked_by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = engine.sign(&revocation.signing_bytes())?;

    engine
        .ipc(IpcRequest::UserRotateKey {
            username,
            record: Box::new(record),
            grants: carried,
            revocation: Box::new(revocation),
        })
        .await?;
    Ok(n)
}

/// The identity providers this mesh believes (E1-P5).
#[tauri::command]
async fn oidc_providers(
    state: State<'_, Director>,
) -> Result<Vec<crate::view::OidcProviderView>, String> {
    let engine = state.engine().await?;
    match engine.ipc(IpcRequest::OidcProviders).await? {
        IpcResponse::OidcProviders(rows) => {
            Ok(rows.iter().map(crate::view::OidcProviderView::of).collect())
        }
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// The contribute-compute toggle: whether this machine's node takes the
/// mesh's work — the reserved `no-execute` tag, set on itself exactly as
/// `meshnode nodes tag` would. The op-log keeps it across restarts.
#[tauri::command]
async fn set_contribute(
    state: State<'_, Director>,
    contribute: bool,
) -> Result<StatusView, String> {
    let engine = state.engine().await?;
    apply_contribute(&engine, contribute).await?;
    status_of(&engine).await
}

/// `meshnode nodes tag --add … --rm …` for this machine's node. The
/// reserved `no-execute` tag stays under the contribute toggle's control.
#[tauri::command]
async fn tags_update(
    state: State<'_, Director>,
    add: Vec<String>,
    remove: Vec<String>,
) -> Result<StatusView, String> {
    let clean = |v: Vec<String>| -> Vec<String> {
        v.into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty() && t != mesh_proto::NO_EXECUTE_TAG)
            .collect()
    };
    let (add, remove) = (clean(add), clean(remove));
    if add.is_empty() && remove.is_empty() {
        return Err("nothing to change".into());
    }
    let engine = state.engine().await?;
    engine.ipc(IpcRequest::TagsUpdate { add, remove }).await?;
    status_of(&engine).await
}

/// `meshnode ping <node>`: reach a member by id. The daemon races every
/// address that node announced and falls back to a relay, which is the
/// only way to get a true answer — the announced list leads with the
/// peer's own loopback, and dialing that reaches this machine, not it.
#[tauri::command]
async fn ping_node(state: State<'_, Director>, node: String) -> Result<PingView, String> {
    let node = NodeId::parse(&node).map_err(|e| e.to_string())?;
    let IpcResponse::Ping(p) = state
        .engine()
        .await?
        .ipc(IpcRequest::PingNode { node })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(p.into())
}

// ---------------------------------------------------------------------------
// Runs & jobs (streamed)
// ---------------------------------------------------------------------------

fn spawn_run_stream(engine: Engine, req: IpcRequest, channel: Channel<RunEvent>) {
    spawn_run_stream_notifying(None, engine, req, channel, None);
}

/// [`spawn_run_stream`] that stops when `cancel` fires (**H-10**).
fn spawn_run_stream_cancellable(
    engine: Engine,
    req: IpcRequest,
    channel: Channel<RunEvent>,
    cancel: tokio_util::sync::CancellationToken,
) {
    spawn_run_stream_notifying(None, engine, req, channel, Some(cancel));
}

/// [`spawn_run_stream`], and — given the app — an OS notification when the
/// run ends while Director is not in front. The one thing worth
/// interrupting for is *it finished*, the rule `notify::attention` already
/// applies to deploys and upgrades; a job somebody launched and walked
/// away from is the same moment.
fn spawn_run_stream_notifying(
    app: Option<AppHandle>,
    engine: Engine,
    req: IpcRequest,
    channel: Channel<RunEvent>,
    cancel: Option<tokio_util::sync::CancellationToken>,
) {
    let what = match &req {
        IpcRequest::Run { command, .. } => command.clone(),
        IpcRequest::RunService { name, .. } => name.clone(),
        _ => String::new(),
    };
    tokio::spawn(async move {
        let mut rx = match engine.ipc_stream(req).await {
            Ok(rx) => rx,
            Err(message) => {
                let _ = channel.send(RunEvent::Failed { message });
                return;
            }
        };
        let started = std::time::Instant::now();
        let mut executor: Option<String> = None;
        let mut targets: usize = 0;
        // Lives for the whole stream, which is the point (**M-21**): the
        // partial character it holds back is finished by the *next* frame.
        let mut text = view::StreamText::default();
        loop {
            // Dropping `rx` closes the receiving half, which is what makes
            // the engine's sender give up — so a cancel really does stop
            // the work rather than just stop the listening (**H-10**).
            let frame = match &cancel {
                Some(token) => tokio::select! {
                    biased;
                    () = token.cancelled() => return,
                    frame = rx.recv() => frame,
                },
                None => rx.recv().await,
            };
            let Some(frame) = frame else { return };
            let Some(ev) = RunEvent::from_frame_with(frame, &mut text) else {
                continue;
            };
            if let Some(app) = &app {
                match &ev {
                    RunEvent::Started { executor_name, .. } => {
                        executor = Some(executor_name.clone());
                    }
                    RunEvent::FanoutStarted { targets: t, .. } => targets = t.len(),
                    RunEvent::Done {
                        index: None,
                        state,
                        exit_code,
                    } if targets == 0 => {
                        let secs = started.elapsed().as_secs();
                        let outcome = match (state.as_str(), exit_code) {
                            ("succeeded", _) => "succeeded".to_string(),
                            (_, Some(code)) => format!("exited {code}"),
                            (s, None) => s.to_string(),
                        };
                        notify::attention(
                            app,
                            &format!("Job {outcome}"),
                            &format!(
                                "{} on {} — {outcome} after {secs}s",
                                if what.is_empty() {
                                    "a run"
                                } else {
                                    what.as_str()
                                },
                                executor.as_deref().unwrap_or("the mesh")
                            ),
                        );
                    }
                    _ => {}
                }
            }
            if channel.send(ev).is_err() {
                return;
            }
        }
    });
}

/// `meshnode run`: ad-hoc bash with env (secret refs allowed in values),
/// timeout, priority, and the fan-out controls.
/// `--gang N` and friends, as reserved environment (C4 §8.2).
///
/// The same refusals the CLI makes, made here so the message can be shown
/// next to the field rather than arriving as a stream error: a gang of one
/// is a job, and `--gang` and `--fanout` ask different questions.
fn gang_env(
    gang: Option<u32>,
    placement: Option<&str>,
    restart: Option<bool>,
    fanout: Option<u32>,
) -> Result<Vec<(String, String)>, String> {
    let Some(size) = gang else {
        return Ok(Vec::new());
    };
    if size < 2 {
        return Err("a gang is two or more nodes; drop it to run one job".into());
    }
    if fanout.is_some() {
        return Err(
            "a fan-out runs one independent job per node and a gang runs N at \
             once or none — pick one"
                .into(),
        );
    }
    let mut env = vec![(mesh_proto::REQ_GANG.to_string(), size.to_string())];
    if let Some(p) = placement.filter(|p| !p.trim().is_empty()) {
        let parsed = mesh_proto::GangPlacement::parse(p)
            .ok_or_else(|| format!("placement must be `pack` or `spread`, not {p:?}"))?;
        env.push((
            mesh_proto::REQ_GANG_PLACEMENT.to_string(),
            parsed.as_str().to_string(),
        ));
    }
    if restart.unwrap_or(false) {
        env.push((mesh_proto::REQ_GANG_RESTART.to_string(), "1".to_string()));
    }
    Ok(env)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn run_command(
    app: AppHandle,
    state: State<'_, Director>,
    command: String,
    on: Option<String>,
    env: Vec<(String, String)>,
    fanout: Option<u32>,
    timeout_secs: Option<u64>,
    priority: String,
    max_parallel: Option<u32>,
    halt_on_fail: Option<String>,
    dry_run: bool,
    // M19 §5.2: `capture_outputs` asks for each target's *answer* and not
    // only its log; `outputs` names extra `::meshware-output::` keys;
    // `expect` is evaluated row by row on the origin, and is the reason a
    // run can fail without any target failing. All three travel as
    // reserved environment keys the origin daemon reads and strips, so an
    // executor's spec is byte-identical to a pre-M19 one.
    capture_outputs: Option<bool>,
    outputs: Option<Vec<String>>,
    expect: Option<String>,
    // C4 §8.2: a gang is N members that start at the same moment or not at
    // all. It travels as reserved environment for the same reason capture
    // does — the executor re-decodes this request out of the signed
    // envelope, so the request type cannot grow a field.
    gang: Option<u32>,
    gang_placement: Option<String>,
    gang_restart: Option<bool>,
    // C5 §9.1: accelerators, as one JSON-encoded reserved environment key.
    // Same carrier as the gang and capture keys and for the same reason —
    // and JSON rather than three more scalars because this is the
    // requirement shape that will keep growing.
    gpus: Option<u32>,
    vram_mb: Option<u64>,
    device: Option<String>,
    gpu_share: Option<bool>,
    // `["http:9095", "admin:8080!"]` (N2 P2). The same reserved key
    // `meshnode run --port` produces, so a port request has one encoding
    // however it was asked for. A line comment, not a doc comment: rustc
    // allows no doc attributes on a function parameter.
    ports: Option<Vec<String>>,
    // Register this job under NAME while it runs (N3), so
    // `NAME.mesh` resolves to it. Needs a port: a name with no
    // address resolves to nothing.
    register: Option<String>,
    // `["data", "cache:/var/cache:rw"]` (N5). The same reserved key
    // `meshnode run --volume` produces: a claim on a volume this node
    // lends, taken as a lease before the job starts.
    volumes: Option<Vec<String>>,
    channel: Channel<RunEvent>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let priority = parse_priority(&priority)?;
    let halt_on_fail = parse_halt(halt_on_fail)?;
    if fanout.is_none() && (max_parallel.is_some() || halt_on_fail.is_some()) {
        return Err("max-parallel and halt-on-fail need a fan-out".into());
    }
    let gang_env = gang_env(gang, gang_placement.as_deref(), gang_restart, fanout)?;
    let expect = expect
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty());
    // Refused here, where the message can be shown next to the field, not
    // silently dropped: a guard that becomes a no-op is worse than none.
    if let Some(expr) = &expect {
        mesh_core::results::Expectation::parse(expr)?;
    }
    let extra: Vec<String> = outputs.unwrap_or_default();
    let want = if !extra.is_empty() {
        mesh_core::outputs::Wanted::keys(extra)
    } else if capture_outputs.unwrap_or(false) || expect.is_some() {
        mesh_core::outputs::Wanted::implicit()
    } else {
        mesh_core::outputs::Wanted::none()
    };
    let mut env = env;
    env.extend(want.to_env_with(expect.as_deref()));
    env.extend(gang_env);
    let mut devices = mesh_proto::DeviceRequest {
        count: gpus.unwrap_or(0),
        vram_mb: vram_mb.unwrap_or(0),
        share: gpu_share.unwrap_or(false),
        ..Default::default()
    };
    if let Some(spec) = device.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        devices.apply_selector(spec)?;
    }
    env.extend(devices.to_env());
    env.extend(port_env(ports.as_deref())?);
    env.extend(register_env(register.as_deref(), ports.as_deref())?);
    env.extend(volume_env(volumes.as_deref())?);
    let req = wrap_submission(
        IpcRequest::Run {
            command,
            on,
            env,
            timeout_secs,
            fanout,
            priority,
            max_parallel,
            halt_on_fail,
        },
        dry_run,
    );
    spawn_run_stream_notifying(Some(app), engine, req, channel, None);
    Ok(())
}

/// `meshnode parallel` / `meshnode map`: one command per input item, spread
/// across the mesh by pull-based work stealing (MASTERPLAN M9 §5.4).
/// Streams the same frames the CLI renders, mapped to [`ParallelEvent`].
///
/// `keep_output` is the `map` half (M9 §5.5): each item's output stays on
/// the node that produced it and only a reference comes back, which is what
/// keeps intermediate data off the node that launched the job. `reduce`
/// then merges those outputs where most of them already are, and needs
/// `keep_output` because there is nothing to merge otherwise.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn parallel_run(
    state: State<'_, Director>,
    command: String,
    service: Option<String>,
    inputs: Vec<(String, String)>,
    item_input: Option<String>,
    items: Vec<String>,
    on: Option<String>,
    env: Vec<(String, String)>,
    timeout_secs: Option<u64>,
    cpus: Option<u32>,
    mem_mb: Option<u64>,
    disk_mb: Option<u64>,
    chunk: Option<u32>,
    keep_output: bool,
    reduce: Option<String>,
    reduce_on: Option<String>,
    dry_run: bool,
    channel: Channel<ParallelEvent>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    if items.is_empty() {
        return Err("nothing to do: the item list is empty".into());
    }
    let reduce = reduce.filter(|r| !r.trim().is_empty());
    if reduce.is_some() && !keep_output {
        return Err("a reduce needs outputs to reduce over: keep them in the mesh first".into());
    }
    // Resource requirements ride reserved job env (M9 §5.3), so the UI
    // declares them exactly as the CLI's --cpus/--mem/--disk do.
    let mut env = env;
    env.retain(|(k, _)| !mesh_proto::is_reserved_env(k));
    env.extend(
        mesh_proto::Requirements {
            cpus: cpus.unwrap_or(0),
            mem_mb: mem_mb.unwrap_or(0),
            disk_mb: disk_mb.unwrap_or(0),
        }
        .to_env(),
    );
    let req = IpcRequest::Parallel {
        command,
        service,
        inputs,
        item_input,
        items,
        on,
        env,
        timeout_secs,
        chunk,
        capture: if keep_output {
            mesh_proto::Capture::Object
        } else {
            mesh_proto::Capture::Inline
        },
        reduce,
        reduce_on: reduce_on.filter(|r| !r.trim().is_empty()),
    };
    let req = wrap_submission(req, dry_run);
    tokio::spawn(async move {
        let mut rx = match engine.ipc_stream(req).await {
            Ok(rx) => rx,
            Err(message) => {
                let _ = channel.send(ParallelEvent::Failed { message });
                return;
            }
        };
        while let Some(frame) = rx.recv().await {
            if let Some(ev) = ParallelEvent::from_frame(frame)
                && channel.send(ev).is_err()
            {
                return;
            }
        }
    });
    Ok(())
}

/// `meshnode service run`.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn run_service(
    state: State<'_, Director>,
    name: String,
    on: Option<String>,
    inputs: Vec<(String, String)>,
    fanout: Option<u32>,
    timeout_secs: Option<u64>,
    priority: String,
    max_parallel: Option<u32>,
    halt_on_fail: Option<String>,
    dry_run: bool,
    // The same three as `run_command` (M19 §5.2). They ride `inputs`
    // rather than a job environment, because `RunService` has no `env`
    // field and growing the variant would break a mixed-version mesh —
    // the origin daemon reads them off and strips them before the
    // service contract is validated (amendment 8).
    capture_outputs: Option<bool>,
    outputs: Option<Vec<String>>,
    expect: Option<String>,
    // C5's device request rides `inputs` for exactly the same reason, and
    // is stripped at exactly the same point. A service is the shape that
    // gets a contract and a cache, so it is the shape GPU work wants —
    // and until now it was the one run shape that could not ask for a card.
    gpus: Option<u32>,
    vram_mb: Option<u64>,
    device: Option<String>,
    gpu_share: Option<bool>,
    // `["http:9095", "admin:8080!"]` (N2 P2). The same reserved key
    // `meshnode run --port` produces, so a port request has one encoding
    // however it was asked for. A line comment, not a doc comment: rustc
    // allows no doc attributes on a function parameter.
    ports: Option<Vec<String>>,
    // Register this job under NAME while it runs (N3), so
    // `NAME.mesh` resolves to it. Needs a port: a name with no
    // address resolves to nothing.
    register: Option<String>,
    // `["data", "cache:/var/cache:rw"]` (N5). The same reserved key
    // `meshnode run --volume` produces: a claim on a volume this node
    // lends, taken as a lease before the job starts.
    volumes: Option<Vec<String>>,
    // C4's gang, for the same reason and by the same route as the card
    // above it (R1 P4). A gang is requested through reserved environment
    // keys, never through a payload, so a service could always have been
    // one — the CLI and the Together tab were the only things saying
    // otherwise, and the things people actually run as a gang are exactly
    // the ones that deserve a contract and a version.
    gang: Option<u32>,
    gang_placement: Option<String>,
    gang_restart: Option<bool>,
    channel: Channel<RunEvent>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let priority = parse_priority(&priority)?;
    let halt_on_fail = parse_halt(halt_on_fail)?;
    if fanout.is_none() && (max_parallel.is_some() || halt_on_fail.is_some()) {
        return Err("max-parallel and halt-on-fail need a fan-out".into());
    }
    let expect = expect
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty());
    // Refused here, where the message can be shown next to the field: a
    // guard that becomes a no-op is worse than none.
    if let Some(expr) = &expect {
        mesh_core::results::Expectation::parse(expr)?;
    }
    let extra: Vec<String> = outputs.unwrap_or_default();
    let want = if !extra.is_empty() {
        mesh_core::outputs::Wanted::keys(extra)
    } else if capture_outputs.unwrap_or(false) || expect.is_some() {
        mesh_core::outputs::Wanted::implicit()
    } else {
        mesh_core::outputs::Wanted::none()
    };
    let mut inputs = inputs;
    inputs.extend(want.to_env_with(expect.as_deref()));
    let mut devices = mesh_proto::DeviceRequest {
        count: gpus.unwrap_or(0),
        vram_mb: vram_mb.unwrap_or(0),
        share: gpu_share.unwrap_or(false),
        ..Default::default()
    };
    if let Some(spec) = device.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        devices.apply_selector(spec)?;
    }
    inputs.extend(devices.to_env());
    inputs.extend(port_env(ports.as_deref())?);
    inputs.extend(register_env(register.as_deref(), ports.as_deref())?);
    inputs.extend(volume_env(volumes.as_deref())?);
    if let Some(size) = gang {
        if size < 2 {
            return Err(
                "a gang is an all-or-nothing reservation over two or more nodes — \
                 drop it to run one job"
                    .into(),
            );
        }
        if fanout.is_some() {
            return Err(
                "a gang and a fan-out ask different questions: a fan-out runs one \
                 independent job per matching node, a gang runs N nodes at once or none"
                    .into(),
            );
        }
        inputs.push((mesh_proto::REQ_GANG.to_string(), size.to_string()));
        if let Some(p) = gang_placement
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            let parsed = mesh_proto::GangPlacement::parse(p)
                .ok_or_else(|| format!("gang layout must be pack or spread, not {p:?}"))?;
            inputs.push((
                mesh_proto::REQ_GANG_PLACEMENT.to_string(),
                parsed.as_str().to_string(),
            ));
        }
        if gang_restart.unwrap_or(false) {
            inputs.push((mesh_proto::REQ_GANG_RESTART.to_string(), "1".into()));
        }
    }
    let req = wrap_submission(
        IpcRequest::RunService {
            name,
            on,
            inputs,
            inventory: None,
            timeout_secs,
            fanout,
            priority,
            max_parallel,
            halt_on_fail,
        },
        dry_run,
    );
    spawn_run_stream(engine, req, channel);
    Ok(())
}

#[tauri::command]
async fn job_logs(
    state: State<'_, Director>,
    job_id: String,
    follow: bool,
    channel: Channel<RunEvent>,
) -> Result<(), String> {
    let parsed = JobId::parse(&job_id).map_err(|e| e.to_string())?;
    let engine = state.engine().await?;
    // Registered so the window can stop it (**H-10**). Replacing an entry
    // cancels the stream it replaces: selecting the same job twice must
    // not leave the first tail running.
    let token = tokio_util::sync::CancellationToken::new();
    if let Some(prev) = state
        .inner
        .lock()
        .await
        .log_follows
        .insert(job_id, token.clone())
    {
        prev.cancel();
    }
    spawn_run_stream_cancellable(
        engine,
        IpcRequest::JobLogs {
            job_id: parsed,
            follow,
        },
        channel,
        token,
    );
    Ok(())
}

/// Stop following a job's logs (**H-10**).
///
/// Idempotent and forgiving: a job that is not being followed is not an
/// error, because the window calls this on every deselect and on unmount
/// without tracking whether a stream was ever opened.
#[tauri::command]
async fn job_logs_stop(state: State<'_, Director>, job_id: String) -> Result<(), String> {
    if let Some(token) = state.inner.lock().await.log_follows.remove(&job_id) {
        token.cancel();
    }
    Ok(())
}

/// `meshnode jobs clear` — forget every job that has already finished.
///
/// The watermark is stamped here, on the machine the operator is sitting
/// at, and signed with their user key: the daemon authors the op but never
/// picks the moment, so "past" means past as of the click.
#[tauri::command]
async fn jobs_clear(state: State<'_, Director>) -> Result<String, String> {
    let engine = state.engine().await?;
    let by = engine.user.user_id();
    let before_micros = mesh_proto::now_micros();
    let signature = engine
        .user
        .sign(&mesh_proto::jobs_clear_signing_bytes(before_micros, &by))
        .to_vec();
    let IpcResponse::Notice { message } = engine
        .ipc(IpcRequest::JobsClear {
            before_micros,
            by,
            signature,
        })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(message)
}

/// Cancel means cancel: the daemon tries the executor first, and when the
/// executor cannot be told (offline, wiped, gone from the mesh) it authors
/// the user-signed force op instead, so the job never sits at `running`
/// with a cancel nobody can deliver. The returned string is the daemon's
/// account of which of the two happened, for the flash.
#[tauri::command]
async fn job_cancel(state: State<'_, Director>, job_id: String) -> Result<String, String> {
    let job_id = JobId::parse(&job_id).map_err(|e| e.to_string())?;
    let engine = state.engine().await?;
    let by = engine.user.user_id();
    let signature = engine
        .user
        .sign(&mesh_proto::job_cancel_force_signing_bytes(&job_id, &by))
        .to_vec();
    match engine
        .ipc(IpcRequest::JobCancelForce {
            job_id,
            by,
            signature,
        })
        .await?
    {
        IpcResponse::Notice { message } => Ok(message),
        _ => Ok("cancel requested".into()),
    }
}

// ---------------------------------------------------------------------------
// Library: bundles, services, Git sources (MASTERPLAN §8)
// ---------------------------------------------------------------------------

/// `meshnode push <dir>`: the engine ingests a directory on this machine.
///
/// `token` names a folder the person picked, or a file or folder they
/// dropped (SEC-14) — the window never names a path to read into the mesh.
/// A dropped single file is pushed as its own folder's worth of it, as it
/// always was.
#[tauri::command]
async fn push_bundle(state: State<'_, Director>, token: String) -> Result<PushResultView, String> {
    let picked = state.picked.require(&token)?;
    let abs = std::fs::canonicalize(&picked)
        .map_err(|e| format!("cannot access {}: {e}", picked.display()))?;
    if !abs.is_dir() && !abs.is_file() {
        return Err(format!("{} is not a folder or a file", abs.display()));
    }
    let IpcResponse::Pushed {
        bundle_id,
        file_count,
        chunk_count,
        total_size,
    } = state
        .engine()
        .await?
        .ipc(IpcRequest::Push {
            path: abs.to_string_lossy().into_owned(),
        })
        .await?
    else {
        return Err("unexpected response".into());
    };
    // Where it came from, for writing its definitions back later.
    if let Some(mut settings) = engine::Settings::load() {
        settings.remember_source(&bundle_id.to_display_string(), &abs);
        let _ = settings.save();
    }
    Ok(PushResultView {
        bundle_id: bundle_id.to_display_string(),
        short_id: bundle_id.short(),
        file_count,
        chunk_count,
        total_size,
    })
}

/// `meshnode bundles show`: the files in a bundle, typed for the picker.
/// Costs a manifest fetch at most — never the chunks.
#[tauri::command]
async fn bundle_files(
    state: State<'_, Director>,
    bundle_id: String,
) -> Result<Vec<view::BundleFileView>, String> {
    let bundle_id =
        BundleId::parse(bundle_id.trim()).map_err(|e| format!("invalid bundle id: {e}"))?;
    let IpcResponse::BundleFiles(rows) = state
        .engine()
        .await?
        .ipc(IpcRequest::BundleFiles { bundle_id })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// `meshnode bundles cat`: one text file out of a bundle (a contract, a
/// script to preview). Fetches the bundle if this peer lacks it.
async fn bundle_file_text(
    state: State<'_, Director>,
    bundle_id: String,
    path: String,
) -> Result<String, String> {
    let bundle_id =
        BundleId::parse(bundle_id.trim()).map_err(|e| format!("invalid bundle id: {e}"))?;
    let IpcResponse::BundleFile { data } = state
        .engine()
        .await?
        .ipc(IpcRequest::BundleFileRead { bundle_id, path })
        .await?
    else {
        return Err("unexpected response".into());
    };
    String::from_utf8(data).map_err(|_| "file is not valid UTF-8 text".to_string())
}

/// What a script in a bundle says about itself (R2 P4).
///
/// **Director could not read a `#meshware:` header until now.** `meshnode
/// add` inferred a contract from the script and registered it in one
/// command; the wizard set a name, a runtime and an entrypoint and left the
/// Arguments box empty, so the same script added through the GUI produced
/// *"None declared — whatever you pass is forwarded as-is"* and a Run screen
/// with no fields. Two doors, two answers, and the friendlier-looking door
/// was the one that did less.
///
/// Same parser as the CLI (`mesh_core::script_spec`), deliberately, so a
/// script added here and one added with `meshnode add` register identically.
///
/// Two files are read, not one: the script, and the `.definitions` file
/// beside it if it has one. The definitions file wins where the two
/// disagree, and the header is still read for whatever the JSON is silent
/// about — see [`ScriptSpec::read_with_definitions`].
///
/// [`ScriptSpec::read_with_definitions`]: mesh_core::script_spec::ScriptSpec::read_with_definitions
#[tauri::command]
async fn script_inspect(
    state: State<'_, Director>,
    bundle_id: String,
    path: String,
) -> Result<view::ScriptSpecView, String> {
    use mesh_core::script_spec::{Provenance, ScriptSpec, definitions_path};

    let defs_path = definitions_path(&path);
    // A script that cannot be read is reported, not shrugged at. It used to
    // be `unwrap_or_default()`, and the result was that every failure here —
    // an unreachable holder, a renamed file, a bundle this peer did not have
    // — arrived on screen as a script that simply declared no arguments.
    let source = bundle_file_text(state.clone(), bundle_id.clone(), path.clone())
        .await
        .map_err(|e| format!("cannot read {path}: {e}"))?;
    // A missing definitions file is the normal case and not an error, so
    // this one *is* a shrug — but only this one.
    let definitions = bundle_file_text(state, bundle_id, defs_path.clone())
        .await
        .ok();
    let has_definitions = definitions.is_some();

    let (spec, from, error) =
        match ScriptSpec::read_with_definitions(&source, definitions.as_deref()) {
            Ok((spec, from)) => (spec, from, None),
            // Malformed JSON: fall back to what the script itself says, and
            // say plainly why the file was not used.
            Err(e) => {
                let spec = ScriptSpec::read(&source);
                let from = if spec.has_header() {
                    Provenance::Header
                } else if spec.args.is_empty() {
                    Provenance::None
                } else {
                    Provenance::Inferred
                };
                (spec, from, Some(format!("{defs_path}: {e}")))
            }
        };

    Ok(view::ScriptSpecView {
        name: spec.name.clone(),
        about: spec.about.clone(),
        item: spec.item.clone(),
        declared: matches!(from, Provenance::Definitions | Provenance::Header),
        source: from.as_str().to_string(),
        inferred_count: spec.args.iter().filter(|a| a.inferred).count() as u32,
        arg_count: spec.args.len() as u32,
        contract: spec.contract_json(),
        // When the file is broken, hand back its own text rather than a
        // generated replacement — the person is about to fix it, and
        // overwriting the thing they are fixing is not a fix.
        definitions: match (&error, definitions) {
            (Some(_), Some(text)) => text,
            _ => spec.to_definitions(),
        },
        has_definitions,
        definitions_path: defs_path,
        error,
    })
}

/// Compile a definitions file the person is editing, without touching disk.
///
/// The Definitions editor and the argument rows are two views of one thing,
/// and they must not disagree. Rather than reimplement the format in
/// TypeScript to keep the form in sync — a second parser that would agree
/// with this one right up until it did not — the editor asks the same Rust
/// that will read the file back off disk.
#[tauri::command]
async fn script_definitions_preview(definitions: String) -> Result<view::ScriptSpecView, String> {
    use mesh_core::script_spec::{Provenance, ScriptSpec};

    let spec = ScriptSpec::from_definitions(&definitions).map_err(|e| e.to_string())?;
    Ok(view::ScriptSpecView {
        name: spec.name.clone(),
        about: spec.about.clone(),
        item: spec.item.clone(),
        declared: true,
        source: Provenance::Definitions.as_str().to_string(),
        inferred_count: 0,
        arg_count: spec.args.len() as u32,
        contract: spec.contract_json(),
        definitions,
        has_definitions: true,
        definitions_path: String::new(),
        error: None,
    })
}

/// Write a script's definitions file back to the folder it came from (R2 P5).
///
/// A bundle is content-addressed and immutable, so there is no "save into
/// the bundle": the file goes back to the local directory the bundle was
/// pushed from, which is the copy a person edits and version-controls
/// anyway. The next push picks it up.
///
/// `source_path` is that local directory — or the script itself, for a
/// single file dropped onto the window, since that is what got pushed.
///
/// The folder is the one this Director added `bundle_id` from (SEC-14);
/// the window names the bundle, never a place to write.
#[tauri::command]
async fn script_definitions_save(
    bundle_id: String,
    entry: String,
    definitions: String,
) -> Result<String, String> {
    use mesh_core::script_spec::{ScriptSpec, definitions_path};

    // Refused before anything is written: a definitions file that does not
    // parse is worse than none, because the script had been registering
    // fine until it appeared.
    ScriptSpec::from_definitions(&definitions).map_err(|e| e.to_string())?;

    let root = source_folder(&bundle_id)?;
    let script = if root.is_file() {
        root
    } else {
        // `entry` comes from this bundle's own file list, but it is still a
        // path being joined to a directory, and the check costs nothing.
        let rel = std::path::Path::new(entry.trim());
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("{entry} is not a path inside {}", root.display()));
        }
        root.join(rel)
    };

    let target = std::path::PathBuf::from(definitions_path(&script.to_string_lossy()));
    let mut text = definitions;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    std::fs::write(&target, text).map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(target.to_string_lossy().into_owned())
}

/// The folder this Director added a bundle from (SEC-14).
fn source_folder(bundle_id: &str) -> Result<std::path::PathBuf, String> {
    engine::Settings::load()
        .and_then(|s| s.source_of(bundle_id.trim()))
        .ok_or_else(|| {
            "this Director does not know which folder that script came from — add the folder \
             again, then save"
                .to_string()
        })
}

/// Remember how this script was just run, as the way it runs from now on
/// (2026-09).
///
/// The complaint this answers: setting a script up to run *properly* —
/// once per item, over a tag, four cores an item, results left where they
/// land, a 45-minute limit — is a dozen decisions across four disclosures,
/// and every one of them was thrown away the moment the run finished. The
/// definitions file beside a script already held what its arguments are.
/// It now also holds how it is run, and the Run screen reads that back.
///
/// **Both halves are written, and they are not the same half:**
///
///   * the **contract**, as a new signed service version. This is the one
///     that makes the feature work — it replicates, so the settings follow
///     the script to every machine and every person rather than living in
///     one folder on one laptop. Nothing is written when the settings are
///     already exactly what the script said: a version per button press
///     would turn the version list into a log of button presses.
///   * the **definitions file**, when `source_path` names the folder this
///     bundle was pushed from. That is the copy a person edits and commits,
///     and the reason this is optional is that a bundle is
///     content-addressed and immutable — there is no "save into the
///     bundle", only back into the directory it came from.
///
/// A failure to write the file is reported and does not fail the call: the
/// contract is already saved by then, and returning an error over the
/// convenience half would say the whole thing failed when it did not.
#[tauri::command]
async fn service_save_run_defaults(
    state: State<'_, Director>,
    name: String,
    run: serde_json::Value,
    arg_defaults: Vec<(String, String)>,
) -> Result<view::RunDefaultsSavedView, String> {
    use mesh_core::script_spec::{RunDefaults, contract_with_defaults};

    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("which script?".into());
    }
    let run = RunDefaults::from_json(&run);
    let engine = state.engine().await?;

    // The *whole* live version, rather than one rebuilt from what the Run
    // screen happens to know. `ServiceRow` — the shape `ServiceShow` and the
    // UI get — carries no `env`, so re-registering from it would silently
    // drop every environment key the service was published with.
    let current = match engine
        .ipc(IpcRequest::ServiceCurrent { name: name.clone() })
        .await?
    {
        IpcResponse::ServiceCurrent(version) => *version,
        other => return Err(format!("unexpected response: {other:?}")),
    };

    let contract = contract_with_defaults(current.contract.as_deref(), &run, &arg_defaults)
        .map_err(|e| e.to_string())?;

    // Compared as JSON and not as text. Both sides are pretty-printed by
    // the same writer today, but a contract that arrived from `meshnode
    // add`, from a hand-edited definitions file, or from a later build
    // need not be byte-identical to say the same thing — and a difference
    // in whitespace is not a reason to mint a version.
    let same = |a: Option<&str>, b: Option<&str>| -> bool {
        match (a, b) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                match (
                    serde_json::from_str::<serde_json::Value>(a),
                    serde_json::from_str::<serde_json::Value>(b),
                ) {
                    (Ok(a), Ok(b)) => a == b,
                    // One of them is not JSON: fall back to the text, which
                    // is the only comparison left that means anything.
                    _ => a == b,
                }
            }
            _ => false,
        }
    };
    let version = if same(contract.as_deref(), current.contract.as_deref()) {
        None
    } else {
        Some(
            register_service_version(
                &engine,
                ServiceVersion {
                    contract,
                    ..current.clone()
                },
            )
            .await?,
        )
    };

    // -- and the copy on disk, when we know where it is --------------------
    let mut definitions_written = None;
    let mut definitions_error = None;
    // The folder this Director added the script's bundle from, if it did
    // (SEC-14) — never a folder the window names.
    if let Some(root) =
        engine::Settings::load().and_then(|s| s.source_of(&current.bundle_id.to_display_string()))
    {
        let root = root.to_string_lossy().into_owned();
        let root = root.as_str();
        let entry = if current.working_dir.is_empty() {
            current.entrypoint.clone()
        } else {
            format!("{}/{}", current.working_dir, current.entrypoint)
        };
        match save_run_defaults_file(root, &entry, &run, &arg_defaults) {
            Ok(path) => definitions_written = Some(path),
            Err(e) => definitions_error = Some(e),
        }
    }
    Ok(view::RunDefaultsSavedView {
        version,
        definitions_path: definitions_written,
        definitions_error,
    })
}

/// The disk half of [`service_save_run_defaults`], kept separate so its
/// failures can be reported without failing the save.
///
/// Reads whatever is beside the script already — the definitions file if
/// there is one, the script's own `#meshware:` header if there is not —
/// so a first save produces a complete file rather than one holding only
/// the run block, and a later save does not delete the arguments somebody
/// wrote by hand.
fn save_run_defaults_file(
    root: &str,
    entry: &str,
    run: &mesh_core::script_spec::RunDefaults,
    arg_defaults: &[(String, String)],
) -> Result<String, String> {
    use mesh_core::script_spec::{ScriptSpec, definitions_path, definitions_with_defaults};

    let root = std::path::PathBuf::from(root);
    if !root.is_absolute() {
        return Err(format!("{} is not an absolute path", root.display()));
    }
    let script = if root.is_file() {
        root
    } else {
        // `entry` is this service's own entrypoint, but it is still a path
        // being joined to a directory and the check costs nothing.
        let rel = std::path::Path::new(entry.trim());
        if rel.is_absolute()
            || rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("{entry} is not a path inside {}", root.display()));
        }
        root.join(rel)
    };
    if !script.is_file() {
        return Err(format!(
            "{} is not there any more — the folder this was pushed from has moved or been tidied away",
            script.display()
        ));
    }

    let source = std::fs::read_to_string(&script)
        .map_err(|e| format!("cannot read {}: {e}", script.display()))?;
    let target = std::path::PathBuf::from(definitions_path(&script.to_string_lossy()));
    let existing = std::fs::read_to_string(&target).unwrap_or_default();

    let fallback = ScriptSpec::read(&source);
    let text = definitions_with_defaults(&existing, &fallback, run, arg_defaults)
        .map_err(|e| format!("{}: {e}", target.display()))?;
    // Refused before anything is written, exactly as `script_definitions_save`
    // does: a definitions file that does not parse is worse than none,
    // because the script had been registering fine until it appeared.
    ScriptSpec::from_definitions(&text).map_err(|e| format!("{}: {e}", target.display()))?;
    std::fs::write(&target, &text)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(target.to_string_lossy().into_owned())
}

/// Pull the expected version out of the engine's conflict message
/// ("version conflict: expected N for service …").
fn parse_expected_version(message: &str) -> Option<u32> {
    let rest = message.split("expected ").nth(1)?;
    rest.split_whitespace().next()?.parse().ok()
}

/// `meshnode service create`: a new immutable version, built and signed
/// here with the human's key; the engine validates and moves the head.
#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn service_create(
    state: State<'_, Director>,
    name: String,
    runtime: String,
    bundle_id: String,
    working_dir: String,
    entrypoint: String,
    contract: Option<String>,
    mut env: Vec<(String, String)>,
    // Container only (N4): override the image's ENTRYPOINT and its
    // arguments, as Nomad's `config { command, args }` does. These
    // become reserved `MESHWARE_TASK_*` entries on the service's own
    // environment — the same ones `service create --command` writes,
    // so a service made here and one made in the terminal are the
    // same record.
    command: Option<String>,
    args: Option<Vec<String>>,
    description: String,
    tags: Vec<String>,
) -> Result<u32, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("service name must not be empty".into());
    }
    let runtime = Runtime::parse(runtime.trim()).ok_or_else(|| {
        format!("unknown runtime {runtime:?} (bash|python|ansible|exec|container)")
    })?;
    let bundle_id =
        BundleId::parse(bundle_id.trim()).map_err(|e| format!("invalid bundle id: {e}"))?;
    let args = args.unwrap_or_default();
    if (command.is_some() || !args.is_empty()) && runtime != mesh_proto::Runtime::Container {
        return Err(concat!(
            "a command override applies to the container runtime; every other ",
            "runtime takes its entry point from the bundle",
        )
        .into());
    }
    env.extend(mesh_proto::command_env(command.as_deref(), &args));
    let entrypoint = entrypoint.trim().to_string();
    if entrypoint.is_empty() {
        return Err("entrypoint must not be empty".into());
    }
    let contract = contract
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty());
    if let Some(c) = &contract {
        serde_json::from_str::<serde_json::Value>(c)
            .map_err(|e| format!("contract is not valid JSON: {e}"))?;
    }
    let engine = state.engine().await?;
    register_service_version(
        &engine,
        ServiceVersion {
            name: name.clone(),
            version: 0, // filled in below
            runtime,
            bundle_id,
            working_dir: working_dir.trim().trim_matches('/').to_string(),
            entrypoint: entrypoint.clone(),
            contract: contract.clone(),
            env: env.clone(),
            description: description.trim().to_string(),
            tags: tags
                .iter()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect(),
            created_at_micros: 0, // as above
            created_by: engine.user.user_id(),
            source: None,
            signature: Vec::new(),
        },
    )
    .await
}

/// Sign and register the next version of a service, retrying once past a
/// version conflict.
///
/// Extracted from [`service_create`] when saving run defaults became a
/// second caller. `version` and `created_at_micros` on the template are
/// ignored — the next free number is read from replicated state here, and
/// the clock is read here, because both are properties of *this* write and
/// not of whatever the caller happened to be holding.
async fn register_service_version(
    engine: &engine::Engine,
    template: ServiceVersion,
) -> Result<u32, String> {
    let name = template.name.clone();
    let mut number = {
        let IpcResponse::Services(rows) = engine.ipc(IpcRequest::Services).await? else {
            return Err("unexpected response".into());
        };
        rows.iter()
            .find(|r| r.name == name)
            .and_then(|r| r.versions.iter().max().copied())
            .unwrap_or(0)
            + 1
    };
    for _attempt in 0..2 {
        let version = services::sign_service_version(
            &engine.user,
            ServiceVersion {
                version: number,
                created_at_micros: now_micros(),
                ..template.clone()
            },
        );
        let head_signature = services::sign_service_head(&engine.user, &name, number);
        // Raw rather than `Engine::ipc`, because the retry below reads the
        // *error*, which `ipc` turns into a `String`.
        let resp = engine
            .request(IpcRequest::ServiceCreate {
                version: Box::new(version),
                head_signature,
            })
            .await?;
        match resp {
            IpcResponse::ServiceCreated { version, .. } => return Ok(version),
            IpcResponse::Error { message } if message.contains("version conflict") => {
                match parse_expected_version(&message) {
                    Some(expected) if expected != number => number = expected,
                    _ => return Err(message),
                }
            }
            IpcResponse::Denied { message } => return Err(format!("permission denied: {message}")),
            IpcResponse::Error { message } => return Err(message),
            other => return Err(format!("unexpected response: {other:?}")),
        }
    }
    Err("could not register the service version after a retry".into())
}

// ---------------------------------------------------------------------------
// Workflows (M10)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// M28 the drafting agent
// ---------------------------------------------------------------------------
//
// Six commands, and every one of them is a thin pass-through to the same
// `mesh-core` router `meshnode ai` calls. Nothing about the agent is
// implemented in the desktop app: Director *is* a peer, so the call leaves
// from this machine either way, and a second implementation here would be
// a second place for the prompt, the retry loop and the validator gate to
// drift (§3 API parity).

/// `meshnode service rollback NAME VERSION` (also rolls forward).
#[tauri::command]
async fn service_rollback(
    state: State<'_, Director>,
    name: String,
    version: u32,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let signature = services::sign_service_head(&engine.user, &name, version);
    engine
        .ipc(IpcRequest::ServiceRollback {
            name,
            version,
            by: engine.user.user_id(),
            signature,
        })
        .await?;
    Ok(())
}

/// `meshnode service remove NAME`.
#[tauri::command]
async fn service_remove(state: State<'_, Director>, name: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let signature = services::sign_service_remove(&engine.user, &name);
    engine
        .ipc(IpcRequest::ServiceRemove {
            name,
            by: engine.user.user_id(),
            signature,
        })
        .await?;
    Ok(())
}

/// `meshnode sources add-git`.
#[tauri::command]
async fn source_add(
    state: State<'_, Director>,
    name: String,
    url: String,
    branch: String,
    interval_secs: u64,
    bump: Vec<String>,
) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() || url.trim().is_empty() {
        return Err("name and URL are required".into());
    }
    let engine = state.engine().await?;
    let source = mesh_core::gitsync::build_source(
        &engine.user,
        name,
        url.trim().to_string(),
        if branch.trim().is_empty() {
            "main".into()
        } else {
            branch.trim().to_string()
        },
        interval_secs.max(10),
        bump.into_iter()
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .collect(),
    );
    engine.ipc(IpcRequest::SourceAdd { source }).await?;
    Ok(())
}

/// `meshnode sources rm NAME` (bundles it produced stay).
#[tauri::command]
async fn source_remove(state: State<'_, Director>, name: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let by = engine.user.user_id();
    let signature = engine
        .user
        .sign(&mesh_proto::source_remove_signing_bytes(&name, &by))
        .to_vec();
    engine
        .ipc(IpcRequest::SourceRemove {
            name,
            by,
            signature,
        })
        .await?;
    Ok(())
}

/// `meshnode sources sync NAME` — pull now, on this peer.
#[tauri::command]
async fn source_sync(state: State<'_, Director>, name: String) -> Result<SyncResultView, String> {
    let IpcResponse::SourceSync {
        commit,
        bundle_id,
        unchanged,
        bumped,
    } = state
        .engine()
        .await?
        .ipc(IpcRequest::SourceSyncNow { name })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(SyncResultView {
        commit,
        bundle_id: bundle_id.to_display_string(),
        bundle_short: bundle_id.short(),
        unchanged,
        bumped: bumped
            .into_iter()
            .map(|(service, version)| BumpedView { service, version })
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// Secrets (MASTERPLAN §7.3) — sealed here, never seen by the engine
// ---------------------------------------------------------------------------

/// `meshnode secrets set`: seal for the nodes the secret is for, sign,
/// publish. Which nodes those are is the engine's answer (SEC-7): a scoped
/// secret's selector, else every member but consoles, no-execute
/// and quarantined nodes.
#[tauri::command]
async fn secret_set(
    state: State<'_, Director>,
    name: String,
    value: String,
) -> Result<SecretSetView, String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("secret name must not be empty".into());
    }
    if value.is_empty() {
        return Err("refusing to store an empty secret".into());
    }
    let engine = state.engine().await?;
    let IpcResponse::SecretRecipients(recipients) = engine
        .ipc(IpcRequest::SecretRecipients {
            name: name.clone(),
            nodes: None,
        })
        .await?
    else {
        return Err("unexpected response".into());
    };
    if recipients.is_empty() {
        return Err("no node is in this secret's scope — nothing could ever decrypt it".into());
    }
    let IpcResponse::Secrets(rows) = engine.ipc(IpcRequest::Secrets).await? else {
        return Err("unexpected response".into());
    };
    let version = rows
        .iter()
        .filter(|r| r.name == name)
        .map(|r| r.version)
        .max()
        .unwrap_or(0)
        + 1;
    let mut record = secrets::seal_secret(
        &name,
        version,
        value.as_bytes(),
        &recipients,
        engine.user.user_id(),
    )
    .map_err(|e| e.to_string())?;
    record.signature = engine.sign(&record.signing_bytes())?;
    engine.ipc(IpcRequest::SecretPublish { record }).await?;
    Ok(SecretSetView {
        name,
        version,
        recipients: recipients.len() as u32,
    })
}

/// `meshnode secrets rm NAME`.
#[tauri::command]
async fn secret_remove(state: State<'_, Director>, name: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let mut removal = SecretRemoval {
        name,
        by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    removal.signature = engine.sign(&removal.signing_bytes())?;
    engine.ipc(IpcRequest::SecretRemove { removal }).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Access: users, grants (MASTERPLAN §7.2)
// ---------------------------------------------------------------------------

fn build_grant(
    engine: &Engine,
    user_id: UserId,
    role: Role,
    nodes: String,
    services_glob: String,
    expires_at_micros: Option<u64>,
) -> Result<Grant, String> {
    let mut grant = Grant {
        grant_id: GrantId::generate(),
        user_id,
        role,
        nodes: if nodes.trim().is_empty() {
            "any".into()
        } else {
            nodes.trim().to_string()
        },
        services: if services_glob.trim().is_empty() {
            "*".into()
        } else {
            services_glob.trim().to_string()
        },
        expires_at_micros,
        created_at_micros: now_micros(),
        granted_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    grant.signature = engine.sign(&grant.signing_bytes())?;
    Ok(grant)
}

/// Username, full id, or unique id prefix → user id.
async fn resolve_user(engine: &Engine, arg: &str) -> Result<UserId, String> {
    let arg = arg.trim();
    if let Ok(id) = UserId::parse(arg) {
        return Ok(id);
    }
    let IpcResponse::Users(rows) = engine.ipc(IpcRequest::Users).await? else {
        return Err(format!("could not list users to resolve {arg:?}"));
    };
    let matches: Vec<UserId> = rows
        .iter()
        .filter(|r| r.username == arg || r.user_id.to_display_string().starts_with(arg))
        .map(|r| r.user_id)
        .collect();
    match matches.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!("no user matches {arg:?}")),
        many => Err(format!("{arg:?} is ambiguous ({} users match)", many.len())),
    }
}

/// `meshnode users add`: bind a public key to a username + initial grant.
#[tauri::command]
async fn user_add(
    state: State<'_, Director>,
    username: String,
    key: String,
    role: String,
    nodes: String,
    services: String,
    expires_secs: Option<u64>,
) -> Result<(), String> {
    let username = username.trim().to_string();
    if username.is_empty() {
        return Err("username must not be empty".into());
    }
    let user_id =
        UserId::parse(key.trim()).map_err(|e| format!("invalid key (expected mwu1…): {e}"))?;
    let role = parse_role(role.trim())?;
    let engine = state.engine().await?;
    let mut record = UserRecord {
        user_id,
        username,
        created_at_micros: now_micros(),
        added_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    record.signature = engine.sign(&record.signing_bytes())?;
    engine.ipc(IpcRequest::UserAdd { record }).await?;
    let grant = build_grant(
        &engine,
        user_id,
        role,
        nodes,
        services,
        expiry_micros(expires_secs),
    )?;
    engine.ipc(IpcRequest::GrantAdd { grant }).await?;
    Ok(())
}

// -- M23 enrolment (§3.2) ----------------------------------------------------

/// The pending queue. Admin-only on the daemon side; Director simply shows
/// what it is allowed to see.
#[tauri::command]
async fn enrolments_view(state: State<'_, Director>) -> Result<Vec<EnrolmentView>, String> {
    let IpcResponse::Enrolments(rows) = state.engine().await?.ipc(IpcRequest::Enrolments).await?
    else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Approve a pending request: `UserAdd` + `GrantAdd`, unchanged.
///
/// Deliberately not a new verb on the daemon. Approving is the act that
/// already existed; the queue only made it possible to reach the key
/// without Slack.
#[tauri::command]
async fn enrol_approve(
    state: State<'_, Director>,
    user: String,
    username: String,
    role: String,
    nodes: String,
    services: String,
    expires_secs: Option<u64>,
) -> Result<(), String> {
    let username = username.trim().to_string();
    if username.is_empty() {
        return Err("username must not be empty".into());
    }
    let user_id =
        UserId::parse(user.trim()).map_err(|e| format!("invalid key (expected mwu1…): {e}"))?;
    let role = parse_role(role.trim())?;
    let engine = state.engine().await?;
    let mut record = UserRecord {
        user_id,
        username,
        created_at_micros: now_micros(),
        added_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    record.signature = engine.sign(&record.signing_bytes())?;
    engine.ipc(IpcRequest::UserAdd { record }).await?;
    let grant = build_grant(
        &engine,
        user_id,
        role,
        nodes,
        services,
        expiry_micros(expires_secs),
    )?;
    engine.ipc(IpcRequest::GrantAdd { grant }).await?;
    Ok(())
}

/// Decline one request. A tombstone, so it stops reappearing everywhere.
#[tauri::command]
async fn enrol_decline(
    state: State<'_, Director>,
    user: String,
    reason: String,
) -> Result<(), String> {
    let user_id =
        UserId::parse(user.trim()).map_err(|e| format!("invalid key (expected mwu1…): {e}"))?;
    let engine = state.engine().await?;
    let mut decline = mesh_proto::EnrolmentDecline {
        user_id,
        reason: reason.trim().to_string(),
        declined_by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    decline.signature = engine.sign(&decline.signing_bytes())?;
    engine
        .ipc(IpcRequest::EnrolDecline {
            decline: Box::new(decline),
        })
        .await?;
    Ok(())
}

/// Approve a pending request as a *second key* for somebody who already
/// exists — the same person at a new machine.
///
/// The new key is bound to the existing username (`UserAttachKey`, the
/// one place a held name may be reused) and then given every grant the
/// existing key holds, so "this is still Mia" means Mia can still do what
/// Mia could. Each copied grant is a new signed record, revocable on its
/// own like any other.
#[tauri::command]
async fn enrol_attach(
    state: State<'_, Director>,
    user: String,
    username: String,
) -> Result<u32, String> {
    let username = username.trim().to_string();
    if username.is_empty() {
        return Err("pick who this is".into());
    }
    let user_id =
        UserId::parse(user.trim()).map_err(|e| format!("invalid key (expected mwu1…): {e}"))?;
    let engine = state.engine().await?;

    // What the existing key(s) may do, before anything is written.
    let IpcResponse::Grants(grants) = engine.ipc(IpcRequest::Grants).await? else {
        return Err("unexpected response".into());
    };
    let inherited: Vec<_> = grants
        .into_iter()
        .filter(|g| g.username == username && !g.revoked && !g.expired)
        .collect();

    let mut record = UserRecord {
        user_id,
        username: username.clone(),
        created_at_micros: now_micros(),
        added_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    record.signature = engine.sign(&record.signing_bytes())?;
    engine.ipc(IpcRequest::UserAttachKey { record }).await?;

    let mut copied = 0u32;
    for g in inherited {
        let Some(role) = Role::parse(&g.role) else {
            continue;
        };
        let grant = build_grant(
            &engine,
            user_id,
            role,
            g.nodes,
            g.services,
            g.expires_at_micros,
        )?;
        engine.ipc(IpcRequest::GrantAdd { grant }).await?;
        copied += 1;
    }
    Ok(copied)
}

/// "Request access" from Director's own first-run screen (M23 §3.1).
///
/// The same op `meshnode enrol` authors, from the surface a person handed a
/// Director build and an address will actually look at.
#[tauri::command]
async fn enrol_request(state: State<'_, Director>, username: String) -> Result<String, String> {
    let engine = state.engine().await?;
    let IpcResponse::WhoAmI(me) = engine.ipc(IpcRequest::WhoAmI).await? else {
        return Err("unexpected response".into());
    };
    if me.registered {
        return Err("you are already enrolled on this mesh".into());
    }
    if let IpcResponse::EnrolStatus {
        state: mesh_proto::EnrolmentState::Pending,
        code: Some(code),
        ..
    } = engine.ipc(IpcRequest::EnrolStatus).await?
    {
        // Re-asking should show the *same* code — an admin may already be
        // holding it.
        return Ok(code);
    }
    let username = username.trim().to_string();
    if username.len() > mesh_proto::enrol::MAX_SUGGESTED_USERNAME {
        return Err("that name is too long".into());
    }
    let mut nonce = [0u8; 16];
    getrandom::fill(&mut nonce).map_err(|e| format!("system rng unavailable: {e}"))?;
    let now = now_micros();
    let mut request = mesh_proto::EnrolmentRequest {
        user_id: engine.user.user_id(),
        suggested_username: username,
        from_node: me.node_id,
        nonce,
        created_at_micros: now,
        expires_at_micros: now + mesh_proto::enrol::DEFAULT_TTL_SECS * 1_000_000,
        signature: Vec::new(),
    };
    request.signature = engine.sign(&request.signing_bytes())?;
    engine
        .ipc(IpcRequest::EnrolSubmit {
            request: Box::new(request),
        })
        .await?;
    Ok(mesh_proto::enrolment_code(&engine.user.user_id(), &nonce))
}

/// `meshnode users revoke` — permanent.
#[tauri::command]
async fn user_revoke(
    state: State<'_, Director>,
    user: String,
    reason: String,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let user_id = resolve_user(&engine, &user).await?;
    let mut revocation = UserRevocation {
        user_id,
        reason: if reason.trim().is_empty() {
            "revoked from Director".into()
        } else {
            reason
        },
        revoked_by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = engine.sign(&revocation.signing_bytes())?;
    engine.ipc(IpcRequest::UserRevoke { revocation }).await?;
    Ok(())
}

/// `meshnode grants add`.
#[tauri::command]
async fn grant_add(
    state: State<'_, Director>,
    user: String,
    role: String,
    nodes: String,
    services: String,
    expires_secs: Option<u64>,
) -> Result<String, String> {
    let role = parse_role(role.trim())?;
    let engine = state.engine().await?;
    let user_id = resolve_user(&engine, &user).await?;
    let grant = build_grant(
        &engine,
        user_id,
        role,
        nodes,
        services,
        expiry_micros(expires_secs),
    )?;
    let grant_id = grant.grant_id.to_string();
    engine.ipc(IpcRequest::GrantAdd { grant }).await?;
    Ok(grant_id)
}

/// `meshnode grants revoke`.
#[tauri::command]
async fn grant_revoke(state: State<'_, Director>, grant_id: String) -> Result<(), String> {
    let grant_id = GrantId::parse(grant_id.trim()).map_err(|e| format!("invalid grant id: {e}"))?;
    let engine = state.engine().await?;
    let mut revocation = GrantRevocation {
        grant_id,
        revoked_by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = engine.sign(&revocation.signing_bytes())?;
    engine.ipc(IpcRequest::GrantRevoke { revocation }).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Membership actions
// ---------------------------------------------------------------------------

/// Wrap a submission in the dry-run verb.
///
/// The wrapper carries the request that would otherwise have been sent,
/// untouched, so a preview describes the real submission rather than a
/// second one assembled for previewing.
fn wrap_submission(inner: IpcRequest, dry_run: bool) -> IpcRequest {
    if dry_run {
        IpcRequest::DryRun {
            request: Box::new(inner),
        }
    } else {
        inner
    }
}

// ---------------------------------------------------------------------------
// Schedules (M8 §3.10)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Facts (M18 §4.10 P6)
// ---------------------------------------------------------------------------

/// Everything the Facts modal shows about one node, in one round trip.
///
/// One command rather than three: the modal opens on a click, and three
/// sequential IPC calls is three chances to render half a panel.
///
/// `subject` is a node name, a node id prefix, or a host name — the same
/// resolution `meshnode facts <subject>` does, so a person who learns one
/// has learned both.
#[tauri::command]
async fn node_facts(
    state: State<'_, Director>,
    subject: String,
    history_limit: Option<u32>,
) -> Result<view::NodeFactsView, String> {
    let engine = state.engine().await?;
    let docs = match engine
        .ipc(IpcRequest::Facts {
            subject: subject.clone(),
            collector: None,
        })
        .await?
    {
        IpcResponse::Facts(rows) => rows,
        IpcResponse::Error { message } => return Err(message),
        other => return Err(unexpected(other)),
    };
    let subject_name = docs
        .first()
        .map(|r| r.subject_name.clone())
        .unwrap_or_else(|| subject.clone());

    // History is best-effort: a node with `[history] enabled = false` still
    // has facts, it just cannot say when they changed — and the panel says
    // *that* rather than implying nothing ever did.
    let (history, history_available) = match engine
        .ipc(IpcRequest::FactsDiff {
            subject,
            collector: None,
            since_micros: None,
            limit: history_limit.unwrap_or(50),
        })
        .await?
    {
        IpcResponse::FactsDiff(rows) => {
            (rows.into_iter().map(Into::into).collect::<Vec<_>>(), true)
        }
        _ => (Vec::new(), false),
    };

    Ok(view::NodeFactsView {
        subject_name,
        docs: docs.into_iter().map(Into::into).collect(),
        history,
        history_available,
    })
}

#[tauri::command]
async fn fact_collectors_view(
    state: State<'_, Director>,
) -> Result<Vec<view::FactCollectorView>, String> {
    match state
        .engine()
        .await?
        .ipc(IpcRequest::FactCollectors)
        .await?
    {
        IpcResponse::FactCollectors(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(unexpected(other)),
    }
}

/// Pause or resume a collector. Pausing is an **edit**, so the record is
/// re-parsed from its own source, re-signed and re-published — there is no
/// verb that changes what a fleet reports without a signature behind it.
#[tauri::command]
async fn collector_set_paused(
    state: State<'_, Director>,
    name: String,
    paused: bool,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let rows = match engine.ipc(IpcRequest::FactCollectors).await? {
        IpcResponse::FactCollectors(rows) => rows,
        other => return Err(unexpected(other)),
    };
    let row = rows
        .iter()
        .find(|r| r.name == name)
        .ok_or_else(|| format!("no collector named {name:?}"))?;
    if row.built_in {
        return Err(format!(
            "{name:?} is the built-in collector every node runs in process — it cannot be paused, because a node with no facts cannot be selected, scheduled against, or diagnosed"
        ));
    }
    let mut collector = mesh_core::facts::parse_collector(&row.source, engine.user.user_id())
        .map_err(|e| e.to_string())?;
    collector.paused = paused;
    let signed = mesh_core::facts::sign_collector(&engine.user, collector);
    match engine
        .ipc(IpcRequest::FactCollectorPut {
            collector: Box::new(signed),
        })
        .await?
    {
        IpcResponse::Ok => Ok(()),
        other => Err(unexpected(other)),
    }
}

// ---------------------------------------------------------------------------
// Results (M19 §5.7 P6)
// ---------------------------------------------------------------------------

/// Every result set this node owns, newest first — the Results tab's list.
#[tauri::command]
async fn result_sets_view(
    state: State<'_, Director>,
    limit: Option<u32>,
) -> Result<Vec<view::ResultSetView>, String> {
    match state
        .engine()
        .await?
        .ipc(IpcRequest::ResultsExport {
            since_micros: None,
            limit: limit.unwrap_or(50),
        })
        .await?
    {
        IpcResponse::ResultSets(sets) => Ok(sets.into_iter().map(Into::into).collect()),
        other => Err(unexpected(other)),
    }
}

// ---------------------------------------------------------------------------
// M17 the fabric (§3.12 P6)
// ---------------------------------------------------------------------------
//
// A terminal is the only surface in Director where the browser has to talk
// *into* a live stream. A Tauri `Channel` carries frames Rust→JS and
// nothing the other way, so the pattern here is: one command opens the
// session and returns an id, the channel streams output, and two small
// commands (`terminal_send`, `terminal_resize`) reach the writing half by
// that id. Closing is either end hanging up.

/// Live sessions this peer is party to.
#[tauri::command]
async fn sessions_view(state: State<'_, Director>) -> Result<Vec<view::SessionView>, String> {
    match state.engine().await?.ipc(IpcRequest::Sessions).await? {
        IpcResponse::Sessions(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(unexpected(other)),
    }
}

/// Who may open a terminal, and where.
#[tauri::command]
async fn session_policies_view(
    state: State<'_, Director>,
) -> Result<Vec<view::SessionPolicyView>, String> {
    match state
        .engine()
        .await?
        .ipc(IpcRequest::SessionPolicies)
        .await?
    {
        IpcResponse::SessionPolicies(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(unexpected(other)),
    }
}

/// Publish a session policy: who may open a terminal, and where (M17 §3.4).
///
/// Signed here with the operator's own user key, exactly as `grant_add`
/// signs a grant — every node re-verifies it at merge time, so a
/// compromised node cannot manufacture one.
#[tauri::command]
async fn session_policy_put(
    state: State<'_, Director>,
    id: String,
    description: String,
    users: String,
    nodes: String,
    allow: Vec<String>,
    expires_secs: Option<u64>,
) -> Result<(), String> {
    let mut permissions = Vec::new();
    for name in &allow {
        match mesh_proto::SessionPermission::parse(name) {
            Some(p) if !permissions.contains(&p) => permissions.push(p),
            Some(_) => {}
            None => return Err(format!("{name:?} is not a session permission")),
        }
    }
    let engine = state.engine().await?;
    let mut policy = mesh_proto::SessionPolicy {
        id: id.trim().to_string(),
        description: description.trim().to_string(),
        users: users.trim().to_string(),
        nodes: nodes.trim().to_string(),
        allow: permissions,
        expires_at_micros: expiry_micros(expires_secs),
        created_at_micros: mesh_proto::now_micros(),
        created_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    if let Some(message) = policy.refusal() {
        return Err(message);
    }
    policy.signature = engine.sign(&policy.signing_bytes())?;
    engine
        .ipc(IpcRequest::SessionPolicyPut {
            policy: Box::new(policy),
        })
        .await?;
    Ok(())
}

/// Retire one.
#[tauri::command]
async fn session_policy_remove(state: State<'_, Director>, id: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let by = engine.user.user_id();
    let signature = engine
        .user
        .sign(&mesh_proto::session_policy_remove_signing_bytes(&id, &by))
        .to_vec();
    engine
        .ipc(IpcRequest::SessionPolicyRemove { id, by, signature })
        .await?;
    Ok(())
}

/// Close a live session from the panel — the same act, and the same audit
/// entry, as `meshnode sessions close`.
#[tauri::command]
async fn session_close(state: State<'_, Director>, id: u64) -> Result<(), String> {
    match state
        .engine()
        .await?
        .ipc(IpcRequest::SessionClose { id })
        .await?
    {
        IpcResponse::Ok => Ok(()),
        other => Err(unexpected(other)),
    }
}

/// Open an interactive terminal on `node` and stream it to `channel`.
///
/// Returns the id the panel uses to type into it. `command` is `None` for
/// the login shell — the same choice `meshnode shell hermes` and
/// `meshnode shell hermes -- htop` make.
#[tauri::command]
#[allow(clippy::too_many_arguments)] // a Tauri command's arguments are its IPC payload
async fn terminal_open(
    state: State<'_, Director>,
    node: String,
    command: Option<String>,
    reason: String,
    cols: u16,
    rows: u16,
    // Phase B, E3: a container instance's job id — a terminal inside it
    // rather than on the node. `meshnode deployment exec`.
    job: Option<String>,
    channel: Channel<view::TerminalEvent>,
) -> Result<u64, String> {
    let engine = state.engine().await?;
    // Refused here as well as on the far node, so the dialog can say so
    // beside the field rather than after a round trip.
    if reason.trim().is_empty() {
        return Err("a terminal needs a reason — it is written to the audit log".into());
    }
    let command = command.filter(|c| !c.trim().is_empty());
    // The panel's emulator is xterm.js, which is what this names.
    let term = "xterm-256color".to_string();
    let kind = match job.filter(|j| !j.trim().is_empty()) {
        Some(job) => mesh_proto::SessionKind::Instance {
            job,
            command,
            term,
            cols: cols.max(1),
            rows: rows.max(1),
        },
        None => mesh_proto::SessionKind::Shell {
            command,
            term,
            cols: cols.max(1),
            rows: rows.max(1),
        },
    };
    let stream = engine.open_session(&node, kind, &reason).await?;
    Ok(attach_terminal(&state, stream, channel).await)
}

/// Hand a session's stream to the panel: its writing half registered under
/// a fresh id for `terminal_send` / `_resize` / `_close`, and everything it
/// prints pumped to `channel`. A mesh terminal and a local build share it.
async fn attach_terminal(
    state: &State<'_, Director>,
    stream: tokio::net::UnixStream,
    channel: Channel<view::TerminalEvent>,
) -> u64 {
    let (mut from_host, to_host) = stream.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<mesh_proto::SessionFrame>(64);

    let id = {
        let mut inner = state.inner.lock().await;
        inner.next_terminal += 1;
        let id = inner.next_terminal;
        inner.terminals.insert(id, tx);
        id
    };

    // Writer: everything the panel types or resizes to.
    tokio::spawn(async move {
        let mut to_host = to_host;
        while let Some(frame) = rx.recv().await {
            if mesh_proto::write_frame(&mut to_host, &frame).await.is_err() {
                return;
            }
        }
    });

    // Reader: everything the far shell prints, plus the ending.
    tokio::spawn(async move {
        loop {
            match mesh_proto::read_frame::<_, mesh_proto::SessionServerFrame>(&mut from_host).await
            {
                Ok(mesh_proto::SessionServerFrame::Opened) => {
                    let _ = channel.send(view::TerminalEvent::Opened);
                }
                Ok(mesh_proto::SessionServerFrame::Data { bytes, .. }) => {
                    // Base64 rather than a JSON number array (**M-20**).
                    let b64 = data_encoding::BASE64.encode(&bytes);
                    if channel.send(view::TerminalEvent::Data { b64 }).is_err() {
                        return;
                    }
                }
                Ok(mesh_proto::SessionServerFrame::Exit { code, .. }) => {
                    let _ = channel.send(view::TerminalEvent::Exit { code });
                    return;
                }
                Ok(mesh_proto::SessionServerFrame::Error { message }) => {
                    let _ = channel.send(view::TerminalEvent::Error { message });
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    let _ = channel.send(view::TerminalEvent::Exit { code: None });
                    return;
                }
            }
        }
    });
    id
}

/// Keystrokes. A closed terminal is not an error the panel needs to raise —
/// the `exit` frame already told it — so this is quietly a no-op once the
/// session is gone.
#[tauri::command]
async fn terminal_send(state: State<'_, Director>, id: u64, text: String) -> Result<(), String> {
    let tx = state.inner.lock().await.terminals.get(&id).cloned();
    if let Some(tx) = tx {
        let _ = tx
            .send(mesh_proto::SessionFrame::Data(text.into_bytes()))
            .await;
    }
    Ok(())
}

#[tauri::command]
async fn terminal_resize(
    state: State<'_, Director>,
    id: u64,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    let tx = state.inner.lock().await.terminals.get(&id).cloned();
    if let Some(tx) = tx {
        let _ = tx
            .send(mesh_proto::SessionFrame::Resize {
                cols: cols.max(1),
                rows: rows.max(1),
            })
            .await;
    }
    Ok(())
}

#[tauri::command]
async fn terminal_close(state: State<'_, Director>, id: u64) -> Result<(), String> {
    // Dropping the sender closes the writer task, which drops the socket,
    // which is what the host reads as "the initiator is gone" — and the
    // host kills the process group, exactly as a job cancel does.
    state.inner.lock().await.terminals.remove(&id);
    Ok(())
}

// -- schedules (N1 P5) ------------------------------------------------------

#[tauri::command]
async fn schedules_view(
    state: State<'_, Director>,
) -> Result<Vec<view::ServiceScheduleView>, String> {
    let resp = state.engine().await?.ipc(IpcRequest::Schedules).await?;
    match resp {
        IpcResponse::Schedules(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[tauri::command]
async fn schedule_detail(
    state: State<'_, Director>,
    name: String,
) -> Result<view::ServiceScheduleView, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::ScheduleShow { name })
        .await?;
    match resp {
        IpcResponse::ScheduleShow(row) => Ok((*row).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Publish a schedule from the definition text an editor holds, or check it.
///
/// The **same parser the CLI uses**, so a file that works in one front end
/// works in the other — D-E3 is about parity of capability, and two parsers
/// would make it parity in name only.
#[tauri::command]
async fn schedule_put(
    state: State<'_, Director>,
    source: String,
    dry_run: bool,
) -> Result<Option<view::ServiceScheduleCheckView>, String> {
    let engine = state.engine().await?;
    let parsed =
        mesh_core::schedules::parse(&source, engine.user.user_id()).map_err(|e| e.to_string())?;
    let signed = mesh_core::schedules::sign(&engine.user, parsed);
    let resp = engine
        .ipc(IpcRequest::SchedulePut {
            schedule: Box::new(signed),
            dry_run,
        })
        .await?;
    match resp {
        IpcResponse::ScheduleCheck(v) => Ok(Some((*v).into())),
        IpcResponse::Ok => Ok(None),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Pause or resume: fetch, flip, re-sign, publish — the discipline every
/// signed record is edited by.
#[tauri::command]
async fn schedule_set_paused(
    state: State<'_, Director>,
    name: String,
    paused: bool,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let resp = engine
        .ipc(IpcRequest::ScheduleShow { name: name.clone() })
        .await?;
    let IpcResponse::ScheduleShow(row) = resp else {
        return Err(format!("no schedule named {name}"));
    };
    let source = row
        .source
        .ok_or_else(|| "the daemon did not return the schedule's definition".to_string())?;
    let mut sched =
        mesh_core::schedules::parse(&source, engine.user.user_id()).map_err(|e| e.to_string())?;
    if sched.paused == paused {
        return Ok(());
    }
    sched.paused = paused;
    let signed = mesh_core::schedules::sign(&engine.user, sched);
    engine
        .ipc(IpcRequest::SchedulePut {
            schedule: Box::new(signed),
            dry_run: false,
        })
        .await?;
    Ok(())
}

#[tauri::command]
async fn schedule_remove(state: State<'_, Director>, name: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let signature = mesh_core::schedules::sign_remove(&engine.user, &name);
    engine
        .ipc(IpcRequest::ScheduleRemove {
            name,
            by: engine.user.user_id(),
            signature,
        })
        .await?;
    Ok(())
}

/// `--register webapp` as the reserved registration key (N3).
///
/// Refused without a port, here rather than on the executor: a registered
/// name with no address resolves to nothing, and the failure is a service
/// that appears in the registry and answers no query — which reads as a
/// broken service rather than an incomplete request.
fn register_env(
    name: Option<&str>,
    ports: Option<&[String]>,
) -> Result<Vec<(String, String)>, String> {
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Ok(Vec::new());
    };
    if !mesh_core::registry::is_valid_service_name(name) {
        return Err(format!(
            "{name:?} must be a DNS label: 1-63 letters, digits or '-', not starting \
             or ending with '-', and not all digits — it becomes {name}.{}",
            mesh_proto::DNS_SUFFIX
        ));
    }
    if ports.unwrap_or(&[]).iter().all(|p| p.trim().is_empty()) {
        return Err(
            "registering a name needs a port: a name with no address resolves to nothing"
                .to_string(),
        );
    }
    Ok(vec![(
        mesh_proto::REQ_SERVICE.to_string(),
        name.to_string(),
    )])
}

/// `["http:9095", "admin:8080!"]` as the reserved port request (N2 P2).
///
/// Validated here rather than leaning on the tolerant parser in
/// `mesh-proto`: that one drops junk silently because it reads an
/// environment a job may have been handed by anything, and a person typing
/// into a field deserves to be told which entry was wrong.
fn port_env(ports: Option<&[String]>) -> Result<Vec<(String, String)>, String> {
    let ports: Vec<&String> = ports
        .unwrap_or(&[])
        .iter()
        .filter(|p| !p.trim().is_empty())
        .collect();
    if ports.is_empty() {
        return Ok(Vec::new());
    }
    if ports.len() > mesh_proto::MAX_PORTS_PER_JOB {
        return Err(format!(
            "at most {} ports (got {})",
            mesh_proto::MAX_PORTS_PER_JOB,
            ports.len()
        ));
    }
    for one in &ports {
        if mesh_proto::PortRequests::parse(one).is_empty() {
            return Err(format!(
                "{one:?} is not LABEL:PORT — try `http:9095`, or `http:8080!` to demand \
                 that exact host port. A label is letters, digits, '-' or '_', and becomes \
                 MESHWARE_PORT_<LABEL>."
            ));
        }
    }
    let joined = ports
        .iter()
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let parsed = mesh_proto::PortRequests::parse(&joined);
    if parsed.len() != ports.len() {
        return Err(format!("a port label is repeated: {joined}"));
    }
    Ok(parsed.to_env())
}

/// `["data", "cache:/var/cache:ro"]` as the reserved volume claim (N5).
///
/// Validated here for the same reason `port_env` is: the parser in
/// `mesh-proto` reads an environment a job may have been handed by
/// anything and drops what it cannot understand, which is right there and
/// wrong in front of a person who has just typed a line into a field.
fn volume_env(volumes: Option<&[String]>) -> Result<Vec<(String, String)>, String> {
    let specs: Vec<&str> = volumes
        .unwrap_or(&[])
        .iter()
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .collect();
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    if specs.len() > mesh_proto::MAX_VOLUMES_PER_JOB {
        return Err(format!(
            "at most {} volumes (got {})",
            mesh_proto::MAX_VOLUMES_PER_JOB,
            specs.len()
        ));
    }
    let mut out: Vec<mesh_proto::VolumeRequest> = Vec::with_capacity(specs.len());
    for spec in specs {
        let parts: Vec<&str> = spec.split(':').map(str::trim).collect();
        let (name, path, mode) = match parts.as_slice() {
            [name] => (*name, "", "rw"),
            [name, second] if mesh_proto::volumes::Mode::parse(second).is_some() => {
                (*name, "", *second)
            }
            [name, path] => (*name, *path, "rw"),
            [name, path, mode] => (*name, *path, *mode),
            _ => {
                return Err(format!(
                    "{spec:?} is NAME, NAME:MODE or NAME:PATH:MODE — try `data`, \
                     `data:ro`, or `data:/mnt/data:rw`"
                ));
            }
        };
        if !mesh_proto::volumes::is_valid_volume_name(name) {
            return Err(format!(
                "{spec:?}: {name:?} must be 1-48 letters, digits, '-' or '_', starting \
                 with a letter or digit"
            ));
        }
        let Some(mode) = mesh_proto::volumes::Mode::parse(mode) else {
            return Err(format!("{spec:?}: mode must be `rw` or `ro`"));
        };
        if out.iter().any(|v| v.name == name) {
            return Err(format!(
                "{name:?} is claimed twice; one claim cannot be two modes"
            ));
        }
        out.push(mesh_proto::VolumeRequest {
            name: name.to_string(),
            path: path.to_string(),
            mode,
        });
    }
    Ok(mesh_proto::VolumeRequests(out).to_env())
}

/// Read a definition file the operator picked or dropped, as text.
///
/// **Bounded, and text only.** A definition is a page of TOML; anything
/// past 1 MB is somebody having dropped the wrong file, and reading it into
/// a textarea would wedge the webview rather than say so. Refusing with the
/// size is the fast, legible failure.
///
/// Tauri's drag-drop hands over a real *path* rather than a browser `File`
/// (the idiom `Library.svelte` already uses), so a dropped spec and a
/// picked one take exactly the same route and there is no second
/// implementation to drift.
#[tauri::command]
async fn read_spec_file(state: State<'_, Director>, token: String) -> Result<String, String> {
    const MAX: u64 = 1024 * 1024;
    // A file the person picked or dropped (SEC-14): this read anything up
    // to a megabyte that the window named, `~/.ssh/id_*` included.
    let path = state.picked.require(&token)?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if meta.len() > MAX {
        return Err(format!(
            "{} is {:.1} MB — a definition is a page of TOML, so this is almost certainly \
             the wrong file",
            path.display(),
            meta.len() as f64 / (1024.0 * 1024.0)
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    String::from_utf8(bytes).map_err(|_| {
        format!(
            "{} is not UTF-8 text — a definition is TOML, YAML or JSON",
            path.display()
        )
    })
}

// -- volumes (N5) -----------------------------------------------------------

/// This node's volumes, and who is holding them.
///
/// Node-local by construction: the claim *is* this node's lease table, and
/// Director embeds a node — so this is the ledger of the machine the app is
/// running on, which is the one a person sitting at it is asking about.
#[tauri::command]
async fn volumes_view(state: State<'_, Director>) -> Result<Vec<view::VolumeRowView>, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::Volumes { node: None })
        .await?;
    match resp {
        IpcResponse::Volumes(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Ask this node's volume plugin to provision a volume (N5 P2).
///
/// The parameters arrive as `key=value` lines from one textarea, which is
/// the shape a plugin's own documentation will be written in — and they are
/// split here rather than sent as a blob, so a line without an `=` is
/// refused next to the field instead of confusing a shell script.
#[tauri::command]
async fn volume_create(
    state: State<'_, Director>,
    name: String,
    capacity_mb: Option<u64>,
    params: Option<String>,
    read_only: Option<bool>,
) -> Result<view::VolumeRowView, String> {
    let name = name.trim().to_string();
    if !mesh_proto::volumes::is_valid_volume_name(&name) {
        return Err(format!(
            "{name:?} must be 1-48 letters, digits, '-' or '_', starting with a letter \
             or digit"
        ));
    }
    let mut parsed: Vec<(String, String)> = Vec::new();
    for line in params.as_deref().unwrap_or("").lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("{line:?} is not key=value"));
        };
        let (k, v) = (k.trim(), v.trim());
        if k.is_empty() {
            return Err(format!("{line:?} has an empty key"));
        }
        parsed.push((k.to_string(), v.to_string()));
    }
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::VolumeCreate {
            name,
            capacity_mb: capacity_mb.unwrap_or(0),
            params: parsed,
            read_only: read_only.unwrap_or(false),
        })
        .await?;
    match resp {
        IpcResponse::VolumeCreated(row) => Ok(row.into()),
        IpcResponse::Error { message } | IpcResponse::Denied { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Stop offering a volume this node provisioned, and ask the plugin to
/// remove it (N5 P2).
#[tauri::command]
async fn volume_remove(
    state: State<'_, Director>,
    name: String,
    force: Option<bool>,
) -> Result<String, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::VolumeRemove {
            name: name.trim().to_string(),
            force: force.unwrap_or(false),
        })
        .await?;
    match resp {
        IpcResponse::VolumeRemoved { message } => Ok(message),
        IpcResponse::Error { message } | IpcResponse::Denied { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

// -- the mesh-wide shed (N6 §12) --------------------------------------------

/// What is shed right now, and how much of the mesh will honour it.
#[tauri::command]
async fn shed_view(state: State<'_, Director>) -> Result<view::ShedView, String> {
    let resp = state.engine().await?.ipc(IpcRequest::ShedStatus).await?;
    match resp {
        IpcResponse::Shed(v) => Ok((*v).into()),
        IpcResponse::Error { message } | IpcResponse::Denied { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Set both switches at once (N6 §12).
///
/// **Absolute, not a delta** — the two booleans are the state to be in.
/// Two admins flipping different halves from stale views would otherwise
/// produce a record neither asked for, and the one thing this switch has to
/// be is unambiguous. The panel therefore sends both every time, reading
/// the current record first exactly as `meshnode shed` does.
#[tauri::command]
async fn shed_set(
    state: State<'_, Director>,
    submissions: bool,
    reconcile: bool,
    reason: Option<String>,
) -> Result<view::ShedView, String> {
    let engine = state.engine().await?;
    let mut shed = mesh_proto::shed::Shed {
        submissions,
        reconcile,
        reason: reason.unwrap_or_default().trim().to_string(),
        created_by: engine.user.user_id(),
        created_at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    shed.signature = engine.sign(&shed.signing_bytes())?;
    let resp = engine
        .ipc(IpcRequest::ShedSet {
            shed: Box::new(shed),
        })
        .await?;
    match resp {
        IpcResponse::Shed(v) => Ok((*v).into()),
        IpcResponse::Error { message } | IpcResponse::Denied { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

// -- the service registry (N3) ----------------------------------------------

/// Every registered service, with healthy-of-total.
///
/// Answered from local replicated state: every node holds the whole
/// catalogue, so Director's embedded node has the same answer any other
/// node would give and there is no round trip to a leader.
#[tauri::command]
async fn registry_view(state: State<'_, Director>) -> Result<Vec<view::RegistryRowView>, String> {
    let resp = state.engine().await?.ipc(IpcRequest::Registry).await?;
    match resp {
        IpcResponse::Registry(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// One service's instances, and whether discovery hands each out.
#[tauri::command]
async fn registry_service(
    state: State<'_, Director>,
    service: String,
) -> Result<Vec<view::RegistryInstanceView>, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::RegistryShow { service })
        .await?;
    match resp {
        IpcResponse::RegistryShow(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// What this node's DNS responder is doing.
#[tauri::command]
async fn dns_status(state: State<'_, Director>) -> Result<view::DnsStatusViewT, String> {
    let resp = state.engine().await?.ipc(IpcRequest::DnsStatus).await?;
    match resp {
        IpcResponse::DnsStatus(v) => Ok((*v).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

// -- ports (N2 P1) ----------------------------------------------------------

/// This node's port ledger.
///
/// Node-local by construction, and Director embeds a node — so this is the
/// ledger of the machine the app is running on, which is the one a person
/// sitting at it is asking about.
#[tauri::command]
async fn ports_view(state: State<'_, Director>) -> Result<view::PortLedgerViewT, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::Ports { node: None })
        .await?;
    match resp {
        IpcResponse::Ports(v) => Ok((*v).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// What each node can run (T4, D-E3).
///
/// The Director half of `meshnode runtimes`, and the panel that answers
/// "why did my job not run here" without anyone reading a log.
#[tauri::command]
async fn runtimes_view(state: State<'_, Director>) -> Result<view::RuntimesViewT, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::Runtimes { node: None })
        .await?;
    match resp {
        IpcResponse::Runtimes(v) => Ok((*v).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

// -- deployments (C2 §6) ----------------------------------------------------

#[tauri::command]
async fn deployments_view(state: State<'_, Director>) -> Result<Vec<view::DeploymentView>, String> {
    let resp = state.engine().await?.ipc(IpcRequest::Deployments).await?;
    match resp {
        IpcResponse::Deployments(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// One deployment with its definition, so an editor can load it.
///
/// The list deliberately fetches without the source — a definition per row
/// on a screen that polls every two seconds is a lot of TOML across the
/// webview boundary for text nobody is reading. The editor asks for one.
#[tauri::command]
async fn deployment_detail(
    state: State<'_, Director>,
    name: String,
) -> Result<view::DeploymentView, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::DeploymentShow { name })
        .await?;
    match resp {
        IpcResponse::DeploymentShow(row) => Ok((*row).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Publish a deployment from the definition text an editor holds, or check
/// it (N1, D-E3).
///
/// The **same parser and the same two records the CLI produces**: one file
/// to a person, a `Deployment` and a `DeploymentPolicy` on the wire. A
/// second parser here would make parity a claim rather than a fact — a
/// `[deployment.disconnect]` block that worked in the terminal and was
/// silently dropped by the app is exactly the failure D-E3 exists to
/// prevent.
///
/// A dry run answers with the check; a real put answers with `null`.
#[tauri::command]
async fn deployment_put(
    state: State<'_, Director>,
    source: String,
    dry_run: bool,
) -> Result<Option<view::DeploymentCheckView>, String> {
    let engine = state.engine().await?;
    let user = engine.user.user_id();
    let mut parsed = mesh_core::deployments::parse(&source, user).map_err(|e| e.to_string())?;
    let inline_service = parsed.inline.is_some();

    // `[deployment.container]`: publish the service this file describes,
    // then the deployment that names it — the same two steps, in the same
    // order, that `meshnode deployment put` takes. First, because it is
    // what settles `service_version`, which the generation below is decided
    // by comparing.
    publish_inline_container(&engine, &mut parsed, dry_run).await?;

    // Continue the deployment's generation sequence (N6). Without it a
    // re-publish carries the generation already running: the reconciler
    // sees its instances as current, replaces nothing, and Director reports
    // a successful roll over a mesh that did not change.
    set_deployment_generation(&engine, &mut parsed).await;

    // Phase B, E2: the file's knobs first, as `meshnode deployment put` sends
    // them, so a hold on promotion is in place before the roll it holds.
    if !dry_run {
        let knobs = mesh_core::deployments::sign_knobs(&engine.user, parsed.knobs.clone());
        match engine
            .ipc(IpcRequest::DeploymentKnobs {
                knobs: Box::new(knobs),
            })
            .await?
        {
            IpcResponse::Ok => {}
            IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                return Err(message);
            }
            other => return Err(format!("unexpected response: {other:?}")),
        }
    }
    let signed = mesh_core::deployments::sign(&engine.user, parsed.deployment);
    let policy = mesh_core::deployments::sign_policy(&engine.user, parsed.policy);
    let resp = engine
        .ipc(IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run,
            inline_service,
        })
        .await?;
    match resp {
        IpcResponse::DeploymentCheck(v) => Ok(Some((*v).into())),
        IpcResponse::Ok => Ok(None),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Parse a definition and say what it would do, with no mesh asked (Phase B,
/// E4) — `meshnode deployment validate`, for the editor's inline check.
/// The rows come from `mesh_core::deployments::describe`, so the terminal
/// and this screen word one file the same way.
#[tauri::command]
async fn deployment_validate(source: String) -> Result<Vec<(String, String)>, String> {
    let parsed = mesh_core::deployments::parse(&source, mesh_proto::UserId([0u8; 32]))
        .map_err(|e| e.to_string())?;
    Ok(mesh_core::deployments::describe(&parsed))
}

/// The commented starting definition (Phase B, E4 and U14) —
/// `meshnode deployment init`, and *Deploy an example*.
#[tauri::command]
async fn deployment_example(name: String, image: String) -> Result<String, String> {
    Ok(mesh_core::deployments::example(&name, &image))
}

/// The JSON Schema for definition files (Phase B, E4) —
/// `meshnode deployment schema`, for the editor's copy button.
#[tauri::command]
async fn deployment_schema() -> Result<String, String> {
    Ok(mesh_core::deployments::SCHEMA.to_string())
}

/// Publish the service an inline `[deployment.container]` describes.
///
/// The Director half of what `publish_inline` does in the CLI, over the
/// same `mesh_core::deployments::resolve_inline` — one rule for what
/// version an image becomes, so a file published from here and the same
/// file published from a terminal land on the same number.
async fn publish_inline_container(
    engine: &crate::engine::Engine,
    parsed: &mut mesh_core::deployments::Parsed,
    dry_run: bool,
) -> Result<(), String> {
    let Some(inline) = parsed.inline.clone() else {
        return Ok(());
    };
    let existing = match engine
        .ipc(IpcRequest::ServiceShow {
            name: inline.service.clone(),
        })
        .await
    {
        Ok(IpcResponse::ServiceShow(row)) => Some(mesh_core::deployments::Existing::from(&row)),
        _ => None,
    };
    let outcome =
        mesh_core::deployments::resolve_inline(existing.as_ref(), parsed, engine.user.user_id())
            .map_err(|e| e.to_string())?;
    match outcome {
        mesh_core::deployments::InlineOutcome::Named
        | mesh_core::deployments::InlineOutcome::Reuse { .. } => Ok(()),
        // A rehearsal publishes nothing; `resolve_inline` has already put
        // the version it *would* take on the deployment, so the plan reads
        // as it will run.
        mesh_core::deployments::InlineOutcome::Publish { .. } if dry_run => Ok(()),
        mesh_core::deployments::InlineOutcome::Publish { version } => {
            let signed = mesh_core::services::sign_service_version(&engine.user, *version);
            let head_signature =
                mesh_core::services::sign_service_head(&engine.user, &signed.name, signed.version);
            match engine
                .ipc(IpcRequest::ServiceCreate {
                    version: Box::new(signed),
                    head_signature,
                })
                .await?
            {
                IpcResponse::Ok | IpcResponse::ServiceCreated { .. } => Ok(()),
                IpcResponse::Error { message } | IpcResponse::Denied { message } => Err(message),
                other => Err(format!("publishing the container: unexpected {other:?}")),
            }
        }
    }
}

/// Give a put the right generation, against what is published now.
///
/// The same two calls the CLI's `set_generation` makes, in the same order —
/// which record to compare against is
/// [`mesh_core::deployments::previous_from_row`]'s decision, so one file
/// published from this window and from a terminal lands on one number.
///
/// Silent on failure on purpose: a deployment that cannot be read is a
/// first publish as far as this is concerned, and generation 0 is right for
/// that. Refusing to publish because the *previous* record was unreadable
/// would turn a stale row into an outage.
async fn set_deployment_generation(
    engine: &crate::engine::Engine,
    parsed: &mut mesh_core::deployments::Parsed,
) {
    let name = parsed.deployment.name.clone();
    let prev = match engine.ipc(IpcRequest::DeploymentShow { name }).await {
        Ok(IpcResponse::DeploymentShow(row)) => {
            mesh_core::deployments::previous_from_row(&row, engine.user.user_id())
        }
        _ => None,
    };
    parsed.deployment.generation = mesh_core::deployments::generation_for(
        prev.as_ref().map(|(d, e)| (d, e.as_slice())),
        &parsed.deployment,
        &parsed.policy.instance_env,
    );
}

/// Why a job landed where it did, term by term (C8 P2 §14.3).
///
/// The placement chart is *the* view to build if only one gets built:
/// meshware already computed this on every placement since M5 and threw it
/// away, and no competitor answers the question at all.
///
/// `compare` names another job whose action key is diffed against this
/// one's, which is what turns "this was a cache miss" into "this was a miss
/// because `env` changed".
#[tauri::command]
async fn job_explain(
    state: State<'_, Director>,
    job: String,
    compare: Option<String>,
) -> Result<view::JobExplainView, String> {
    let engine = state.engine().await?;
    let job_id = mesh_proto::JobId::parse(&job).map_err(|e| e.to_string())?;
    let other = match compare {
        Some(other) => {
            let other = mesh_proto::JobId::parse(&other).map_err(|e| e.to_string())?;
            match engine.ipc(IpcRequest::JobExplain { job_id: other }).await {
                // A comparison that cannot be fetched is not an error: the
                // panel simply has nothing to highlight, which is exactly
                // how it looks before anyone picks a run to compare with.
                Ok(IpcResponse::JobExplain(view)) => Some(view.key_fields),
                _ => None,
            }
        }
        None => None,
    };
    let resp = engine.ipc(IpcRequest::JobExplain { job_id }).await?;
    match resp {
        IpcResponse::JobExplain(view) => Ok(view::JobExplainView::of(*view, other.as_deref())),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Hit rate, bytes saved, and which services are paying for the cache
/// (C7 P5 §14.3).
#[tauri::command]
async fn cache_stats(state: State<'_, Director>) -> Result<view::CacheStatsView, String> {
    let resp = state.engine().await?.ipc(IpcRequest::CacheStats).await?;
    match resp {
        IpcResponse::CacheStats(stats) => Ok((*stats).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Compose the one shell line that runs a build through the mesh.
///
/// ## Why the frontend cannot build this itself
///
/// Two of the four pieces are facts about this machine, not about the page:
///
///   * **the wrapper.** `make` calls `CC` by exec'ing it, so `CC` has to
///     name a file on disk: `meshnode cc`, the same wrapper a build typed in
///     a terminal uses, found where the machine's node was installed from
///     (`demo::meshnode_binary`). Director used to name itself here, because
///     a machine with only Director had no `meshnode` — its node was inside
///     the app. Every machine with Director has one now.
///   * **the home.** `MESHWARE_HOME` is pinned to the home of the node this
///     window is showing — this machine's, or a demo's founder — because a
///     wrapper that found no daemon would compile every unit locally, and
///     silently: falling back is the wrapper's contract.
///
/// ## Exported *and* assigned, for make
///
/// A Makefile that hardcodes `CC = gcc` beats an exported `CC`, which is why
/// distcc tells people to pass the assignment on the command line. A
/// `ninja` invocation given a trailing `CC=…` would go looking for a target
/// by that name. So: always export, and append the assignments only when
/// the command is make-shaped. The composed line is returned and shown, so
/// which of the two happened is on screen rather than in this comment.
#[tauri::command]
async fn farm_build_line(dir: String, command: String) -> Result<view::FarmBuildLine, String> {
    compose_build_line(&dir, &command)
}

/// [`farm_build_line`]'s work, for it and for [`build_run`] — so the line
/// shown and the line run are one string from one place.
fn compose_build_line(dir: &str, command: &str) -> Result<view::FarmBuildLine, String> {
    let command = command.trim().to_string();
    if command.is_empty() {
        return Err("a build needs a command to run".into());
    }
    let dir = std::path::PathBuf::from(shellexpand_home(dir));
    if !dir.is_dir() {
        return Err(format!("{} is not a directory", dir.display()));
    }
    let wrapper = demo::meshnode_binary(None).ok_or_else(|| {
        "no meshnode found to use as the compiler wrapper — start this machine's node \
         from Director once, which installs one at ~/.local/bin/meshnode"
            .to_string()
    })?;
    let home = engine::node_home();

    let mut warnings = Vec::new();
    let make_style = is_make_style(&command);
    if !make_style {
        warnings.push(
            "CC and CXX are exported for this command rather than passed as arguments — \
             a build that hardcodes its compiler will ignore them"
                .to_string(),
        );
    }

    let cc = format!("{} cc gcc", sh_quote(&wrapper.to_string_lossy()));
    let cxx = format!("{} cc g++", sh_quote(&wrapper.to_string_lossy()));
    let mut line = format!(
        "cd {} && export MESHWARE_HOME={} CC={} CXX={} && {}",
        sh_quote(&dir.to_string_lossy()),
        sh_quote(&home.to_string_lossy()),
        sh_quote(&cc),
        sh_quote(&cxx),
        command,
    );
    if make_style {
        line.push_str(&format!(" CC={} CXX={}", sh_quote(&cc), sh_quote(&cxx)));
    }

    Ok(view::FarmBuildLine {
        line,
        wrapper: wrapper.to_string_lossy().into_owned(),
        home: home.to_string_lossy().into_owned(),
        make_style,
        warnings,
    })
}

/// Run a build from the Build tab, on this computer (DIR-8).
///
/// The Run button used to open a mesh terminal on Director's own peer, and
/// a real Director refuses one: its node takes the default `[sessions]`
/// settings, which accept no terminals; past that it would want a session
/// policy naming this person and this node; a peer that does not
/// contribute compute is `no-execute`, which refuses a shell outright. The
/// mock engine never went down that path, so the headline action for
/// build-farm buyers had never worked outside a browser.
///
/// Those rules are right for a terminal somebody opens on a machine from
/// elsewhere. A build started here is not that: it is this person's source
/// tree and toolchain, run as them on their own computer — what `make` in
/// their own terminal does. It could not even run confined, as a session
/// would be: the compiler wrapper reads the key and socket in the node's
/// home, which a session's sandbox hides, and would quietly compile every
/// unit locally. So Director hosts it itself, with the host end a mesh
/// session uses (`mesh_core::sessions::host`) — `meshnode` as the PTY shim,
/// the same environment, the same process-group kill — in a session table
/// of its own. It is a terminal on this computer, not a session of the
/// mesh, and the machine's node does not list it.
///
/// **What stands in for the admission it skips** is the person: the first
/// time a given line is run, the desktop asks, natively, with the line in
/// full. A script in the page can compose any line it likes; it cannot
/// press that button. "Run again" on the same line does not ask twice.
///
/// `Ok(None)` is the person saying no.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn build_run(
    app: AppHandle,
    state: State<'_, Director>,
    dir: String,
    command: String,
    cols: u16,
    rows: u16,
    channel: Channel<view::TerminalEvent>,
) -> Result<Option<u64>, String> {
    let line = compose_build_line(&dir, &command)?.line;
    let engine = state.engine().await?;

    if !state.inner.lock().await.builds_approved.contains(&line) {
        use tauri_plugin_dialog::{DialogExt as _, MessageDialogButtons, MessageDialogKind};
        let (tx, rx) = oneshot::channel();
        app.dialog()
            .message(format!(
                "This runs on this computer, as you, in the project folder — the way it \
                 would in your own terminal:\n\n{line}"
            ))
            .title("Run this build?")
            .kind(MessageDialogKind::Info)
            .buttons(MessageDialogButtons::OkCancelCustom(
                "Run".into(),
                "Cancel".into(),
            ))
            .show(move |ok| {
                let _ = tx.send(ok);
            });
        if !rx.await.unwrap_or(false) {
            return Ok(None);
        }
        state
            .inner
            .lock()
            .await
            .builds_approved
            .insert(line.clone());
    }

    let reason = format!("build farm: {}", command.trim());
    let stream = start_local_build(&engine, line, reason, cols, rows).await?;
    Ok(Some(attach_terminal(&state, stream, channel).await))
}

/// The build's terminal, hosted here: `line` in a PTY on this computer, and
/// the initiator's end of it returned (DIR-8, [`build_run`]). No admission —
/// see there for why, and for what stands in for it.
async fn start_local_build(
    engine: &Engine,
    line: String,
    reason: String,
    cols: u16,
    rows: u16,
) -> Result<tokio::net::UnixStream, String> {
    // The PTY shim is a binary that answers to `__meshware-pty` before it
    // does anything else; `meshnode` is one. Director no longer is — it
    // hosts no mesh sessions, so it no longer re-execs itself as one.
    let shim =
        demo::meshnode_binary(None).ok_or("no meshnode found to host the build's terminal with")?;
    start_local_build_with(engine, line, reason, cols, rows, shim).await
}

/// The builds this window is running: Director's own session table, since a
/// terminal on this computer is not one of the node's.
fn local_builds() -> &'static std::sync::Arc<mesh_core::sessions::Registry> {
    static BUILDS: std::sync::OnceLock<std::sync::Arc<mesh_core::sessions::Registry>> =
        std::sync::OnceLock::new();
    BUILDS.get_or_init(|| std::sync::Arc::new(mesh_core::sessions::Registry::new()))
}

/// [`start_local_build`] with the shim named, for a test that has no
/// `meshnode` to hand.
async fn start_local_build_with(
    engine: &Engine,
    line: String,
    reason: String,
    cols: u16,
    rows: u16,
    shim: std::path::PathBuf,
) -> Result<tokio::net::UnixStream, String> {
    let kind = mesh_proto::SessionKind::Shell {
        command: Some(line),
        term: "xterm-256color".to_string(),
        cols: cols.max(1),
        rows: rows.max(1),
    };
    // Who this is, as the mesh knows them, for the row the build occupies —
    // asked of the node rather than read out of it.
    let IpcResponse::Status(status) = engine.ipc(IpcRequest::Status).await? else {
        return Err("unexpected response".into());
    };
    let username = match engine.ipc(IpcRequest::WhoAmI).await {
        Ok(IpcResponse::WhoAmI(me)) => me.username.unwrap_or_default(),
        _ => String::new(),
    };
    let user = engine.user.user_id();
    // On the engine's runtime, like `open_session`: the socket pair and the
    // host task stay off the window's threads.
    engine::on_engine("starting a build", async move {
        let os_user = std::env::var("USER").unwrap_or_default();
        let guard = local_builds().admit(
            kind.clone(),
            true,
            status.node_id,
            "this computer".to_string(),
            user,
            username,
            u32::MAX,
            reason,
            os_user,
            // Nobody walked away from this one: it ends when the build
            // does, or when the panel is closed.
            mesh_core::sessions::Limits {
                idle: None,
                max: None,
            },
        )?;
        let (client, server) = tokio::net::UnixStream::pair()
            .map_err(|e| format!("cannot open a local terminal: {e}"))?;
        let opts = mesh_core::sessions::host::Opts {
            record_dir: None,
            node_name: status.node_name,
            shim: Some(shim),
            instance: None,
            confine: None,
        };
        tokio::spawn(async move {
            let (reader, writer) = server.into_split();
            let ending = mesh_core::sessions::host::run(kind, reader, writer, &guard, opts).await;
            tracing::info!(?ending, "build farm run ended");
        });
        Ok::<_, String>(client)
    })
    .await?
}

/// Will this command take `CC=…` as an argument, or does it have to be
/// exported?
///
/// The first word decides, because that is the program the trailing
/// assignments would be handed to. `make` reads them as variable
/// overrides that beat even a hardcoded `CC = gcc` in the Makefile;
/// `ninja` would read them as the name of a target to build and fail.
fn is_make_style(command: &str) -> bool {
    command
        .split_whitespace()
        .next()
        // Whole word, not a prefix: `makepkg` is not make.
        .is_some_and(|w| matches!(w, "make" | "gmake" | "bmake"))
}

/// `~` and `~/…`, because a path field people type into gets a tilde.
fn shellexpand_home(path: &str) -> String {
    let path = path.trim();
    let Some(rest) = path.strip_prefix('~') else {
        return path.to_string();
    };
    let Some(home) = dirs::home_dir() else {
        return path.to_string();
    };
    if rest.is_empty() {
        return home.to_string_lossy().into_owned();
    }
    match rest.strip_prefix('/') {
        Some(tail) => home.join(tail).to_string_lossy().into_owned(),
        // `~other` is another user's home and not ours to guess at.
        None => path.to_string(),
    }
}

/// Single-quote for `bash -lc`. The POSIX trick: end the quote, add an
/// escaped quote, start a new one — safe for every byte a path can hold.
///
/// **The engine's, not a second copy** (S-Misc). This was byte-identical to
/// `mesh_core::deploy::shell_quote`, in a binary that already links
/// `mesh-core`. Two implementations of "make this safe for a shell" is the
/// shape a quoting bug hides in: the one that gets fixed and the one that
/// does not.
use mesh_core::deploy::shell_quote as sh_quote;

/// Forget cache entries: everything, one key, or one service's.
#[tauri::command]
async fn cache_purge(state: State<'_, Director>, target: Option<String>) -> Result<String, String> {
    let scope = match target.as_deref() {
        None => mesh_proto::PurgeScope::All,
        Some(t) if t.starts_with("act-") => {
            mesh_proto::PurgeScope::Key(mesh_proto::ActionKey::parse(t).map_err(|e| e.to_string())?)
        }
        Some(t) => mesh_proto::PurgeScope::Service(t.to_string()),
    };
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::CachePurge { scope })
        .await?;
    match resp {
        IpcResponse::Notice { message } => Ok(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// The what-if planner: where this would land, when it would finish, and
/// whose work it would delay — submitting nothing (C8 P3).
#[tauri::command]
async fn plan_view(
    state: State<'_, Director>,
    on: Option<String>,
    service: Option<String>,
    env: Vec<(String, String)>,
    fanout: Option<u32>,
) -> Result<view::PlanView, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::Plan {
            on,
            service,
            env,
            fanout,
        })
        .await?;
    match resp {
        IpcResponse::Plan(plan) => Ok((*plan).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Reservations in flight, most senior first (C4 §14.3).
///
/// The cluster view's answer to "why is that node refusing work when it has
/// free slots": something is holding it for a gang that has not started
/// yet, and this says which one and who is ahead of it.
#[tauri::command]
async fn gangs_view(
    state: State<'_, Director>,
    all: bool,
) -> Result<Vec<view::GangRowView>, String> {
    let resp = state.engine().await?.ipc(IpcRequest::Gangs { all }).await?;
    match resp {
        IpcResponse::Gangs(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[tauri::command]
async fn deployment_status(
    state: State<'_, Director>,
    name: String,
) -> Result<view::DeploymentStatusView, String> {
    let resp = state
        .engine()
        .await?
        .ipc(IpcRequest::DeploymentStatus { name })
        .await?;
    match resp {
        IpcResponse::DeploymentStatus(view) => Ok((*view).into()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Fetch a deployment's current record so an edit (rollout / rollback /
/// pause) can be built from it, signed, and re-published — the same
/// fetch-flip-re-sign discipline schedules use.
async fn fetch_deployment(
    engine: &engine::Engine,
    name: &str,
) -> Result<mesh_core::deployments::Parsed, String> {
    let resp = engine
        .ipc(IpcRequest::DeploymentShow {
            name: name.to_string(),
        })
        .await?;
    let IpcResponse::DeploymentShow(row) = resp else {
        return Err(format!("no deployment named {name}"));
    };
    let source = row
        .source
        .ok_or_else(|| "the daemon did not return the deployment's definition".to_string())?;
    mesh_core::deployments::parse(&source, engine.user.user_id()).map_err(|e| e.to_string())
}

/// Re-publish a deployment with its rollout policy (N1 P3/P4).
///
/// The policy travels with the record because a Director edit is the same
/// fetch-flip-re-sign the CLI does, and dropping the policy on the way
/// through would lift somebody's canary every time they clicked pause.
async fn put_deployment(
    engine: &engine::Engine,
    parsed: mesh_core::deployments::Parsed,
) -> Result<(), String> {
    let signed = mesh_core::deployments::sign(&engine.user, parsed.deployment);
    let policy = mesh_core::deployments::sign_policy(&engine.user, parsed.policy);
    engine
        .ipc(IpcRequest::DeploymentPut {
            deployment: Box::new(signed),
            policy: Box::new(policy),
            dry_run: false,
            inline_service: false,
        })
        .await?;
    Ok(())
}

/// Roll a deployment forward to a new pinned service version. A rollout is
/// an edit: fetch, bump the version and generation, re-sign, publish.
#[tauri::command]
async fn deployment_rollout(
    state: State<'_, Director>,
    name: String,
    version: u32,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let mut parsed = fetch_deployment(&engine, &name).await?;
    let dep = &mut parsed.deployment;
    if version == dep.service_version {
        return Ok(());
    }
    dep.service_version = version;
    dep.generation = dep.generation.saturating_add(1);
    put_deployment(&engine, parsed).await
}

/// Point a deployment back at a previous version — the rollback button.
#[tauri::command]
///
/// With `generation` or `tag` (Phase B, E1), back to a whole kept version —
/// `meshnode deployment rollback --generation|--tag`, through the same
/// `find_version` and `revert_to`.
async fn deployment_rollback(
    state: State<'_, Director>,
    name: String,
    to: Option<u32>,
    generation: Option<u32>,
    tag: Option<String>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let mut parsed = fetch_deployment(&engine, &name).await?;
    if generation.is_some() || tag.is_some() {
        let history = fetch_history(&engine, &name).await?;
        let version = mesh_core::deployments::find_version(&history, generation, tag.as_deref())
            .map_err(|e| e.to_string())?;
        mesh_core::deployments::revert_to(&mut parsed, &version).map_err(|e| e.to_string())?;
        return put_deployment(&engine, parsed).await;
    }
    let dep = &mut parsed.deployment;
    let target = match to {
        Some(v) => v,
        None => dep
            .service_version
            .checked_sub(1)
            .filter(|v| *v >= 1)
            .ok_or_else(|| "already at the earliest version; name a target".to_string())?,
    };
    if target == dep.service_version {
        return Ok(());
    }
    dep.service_version = target;
    dep.generation = dep.generation.saturating_add(1);
    put_deployment(&engine, parsed).await
}

async fn fetch_history(
    engine: &engine::Engine,
    name: &str,
) -> Result<mesh_proto::DeploymentHistoryView, String> {
    match engine
        .ipc(IpcRequest::DeploymentHistory {
            name: name.to_string(),
        })
        .await?
    {
        IpcResponse::DeploymentHistory(view) => Ok(*view),
        IpcResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// The versions a deployment keeps (Phase B, E1 and U2) — `meshnode
/// deployment history`.
#[tauri::command]
async fn deployment_history(
    state: State<'_, Director>,
    name: String,
) -> Result<view::DeploymentHistoryView, String> {
    let engine = state.engine().await?;
    fetch_history(&engine, &name).await.map(Into::into)
}

/// Name a version — the running one unless `generation` says otherwise — or
/// remove a name (Phase B, E1): `meshnode deployment tag` and `untag`.
#[tauri::command]
async fn deployment_tag(
    state: State<'_, Director>,
    name: String,
    tag: String,
    generation: Option<u32>,
    description: Option<String>,
    remove: bool,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let version = if remove {
        None
    } else {
        let history = fetch_history(&engine, &name).await?;
        Some(
            mesh_core::deployments::find_version(&history, generation, None)
                .map_err(|e| e.to_string())?,
        )
    };
    let signed = mesh_core::deployments::sign_tag(
        &engine.user,
        mesh_proto::DeploymentTag {
            deployment: name,
            tag,
            version,
            description: description.unwrap_or_default(),
            set_by: engine.user.user_id(),
            set_at_micros: 0,
            signature: Vec::new(),
        },
    );
    match engine
        .ipc(IpcRequest::DeploymentTag {
            tag: Box::new(signed),
        })
        .await?
    {
        IpcResponse::Ok => Ok(()),
        IpcResponse::Error { message } | IpcResponse::Denied { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Let a held canary set cut over (Phase B, E2) — `meshnode deployment
/// promote`. Names the generation the screen showed, so a click on a stale
/// screen cannot promote a roll nobody has looked at.
#[tauri::command]
async fn deployment_promote(
    state: State<'_, Director>,
    name: String,
    generation: u32,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let promotion = mesh_core::deployments::sign_promotion(&engine.user, &name, generation);
    match engine
        .ipc(IpcRequest::DeploymentPromote {
            promotion: Box::new(promotion),
        })
        .await?
    {
        IpcResponse::Ok => Ok(()),
        IpcResponse::Error { message } | IpcResponse::Denied { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Resize a fixed deployment without restarting what stays (Phase B, E3) —
/// `meshnode deployment scale`, through the same `deployments::scale` and
/// its refusals.
#[tauri::command]
async fn deployment_scale(
    state: State<'_, Director>,
    name: String,
    count: u32,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let mut parsed = fetch_deployment(&engine, &name).await?;
    let previous = mesh_core::deployments::scale(&mut parsed, count).map_err(|e| e.to_string())?;
    if previous == count {
        return Ok(());
    }
    put_deployment(&engine, parsed).await
}

/// Replace instances without changing the definition (Phase B, E3) —
/// `meshnode deployment restart [--slot]`. Every instance is a generation
/// bump under the rollout policy; one slot is a cancel of that instance,
/// which the reconciler replaces.
#[tauri::command]
async fn deployment_restart(
    state: State<'_, Director>,
    name: String,
    slot: Option<String>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let Some(slot) = slot else {
        let mut parsed = fetch_deployment(&engine, &name).await?;
        mesh_core::deployments::restart(&mut parsed).map_err(|e| e.to_string())?;
        return put_deployment(&engine, parsed).await;
    };
    let view = match engine
        .ipc(IpcRequest::DeploymentStatus { name: name.clone() })
        .await?
    {
        IpcResponse::DeploymentStatus(v) => v,
        other => return Err(format!("unexpected response: {other:?}")),
    };
    let instance =
        mesh_core::deployments::instance_in_slot(&view, &slot).map_err(|e| e.to_string())?;
    match engine
        .ipc(IpcRequest::JobCancel {
            job_id: instance.job_id,
        })
        .await?
    {
        IpcResponse::Error { message } => Err(message),
        _ => Ok(()),
    }
}

/// One instance slot up close (Phase B, E3 and U1) — `meshnode deployment
/// instance`: every incarnation, its health, and what it uses now.
#[tauri::command]
async fn deployment_instance(
    state: State<'_, Director>,
    name: String,
    slot: String,
) -> Result<view::DeploymentInstanceDetailView, String> {
    match state
        .engine()
        .await?
        .ipc(IpcRequest::DeploymentInstance { name, slot })
        .await?
    {
        IpcResponse::DeploymentInstance(detail) => Ok((*detail).into()),
        IpcResponse::Error { message } => Err(message),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Pause / resume is an edit, exactly like a schedule's.
#[tauri::command]
async fn deployment_set_paused(
    state: State<'_, Director>,
    name: String,
    paused: bool,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let mut parsed = fetch_deployment(&engine, &name).await?;
    let dep = &mut parsed.deployment;
    if dep.paused == paused {
        return Ok(());
    }
    dep.paused = paused;
    put_deployment(&engine, parsed).await
}

#[tauri::command]
async fn deployment_remove(state: State<'_, Director>, name: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let signature = mesh_core::deployments::sign_remove(&engine.user, &name);
    engine
        .ipc(IpcRequest::DeploymentRemove {
            name,
            by: engine.user.user_id(),
            signature,
        })
        .await?;
    Ok(())
}

// -- C1 arbitration (C0-C9 §5.4 P6) ---------------------------------------
//
// What is waiting on this node, behind whom. Quotas and the usage report
// that once shared this section were removed; fair share still orders the
// queue, from the same replicated counters.

#[tauri::command]
async fn queue_view(state: State<'_, Director>) -> Result<Vec<view::QueueRowView>, String> {
    let resp = state.engine().await?.ipc(IpcRequest::Queue).await?;
    match resp {
        IpcResponse::Queue(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

#[tauri::command]
async fn node_revoke(
    state: State<'_, Director>,
    node_id: String,
    reason: String,
) -> Result<(), String> {
    let node_id = NodeId::parse(&node_id).map_err(|e| e.to_string())?;
    let engine = state.engine().await?;
    let mut revocation = NodeRevocation {
        node_id,
        reason,
        by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = engine.sign(&revocation.signing_bytes())?;
    engine.ipc(IpcRequest::NodeRevoke { revocation }).await?;
    Ok(())
}

/// Repair another node's recorded addresses (`meshnode nodes addr --set`).
///
/// Founder only, and the engine enforces that — this is the way back from a
/// mesh where every recorded address went stale at once (a renumbered VLAN),
/// so no node can dial any other and the usual self-healing, which works by
/// each node re-announcing itself, has nowhere to start.
///
/// Deliberately reachable from here rather than only from the CLI: the
/// operator who just moved three machines is looking at a Director full of
/// nodes gone grey, and telling them the fix is a terminal command on the
/// same desktop is telling them the app cannot do the one thing they came
/// to it for.
#[tauri::command]
async fn node_addr_repair(
    state: State<'_, Director>,
    node_id: String,
    addrs: Vec<String>,
) -> Result<(), String> {
    let node_id = NodeId::parse(&node_id).map_err(|e| e.to_string())?;
    let engine = state.engine().await?;
    engine
        .ipc(IpcRequest::NodeAddrRepair { node_id, addrs })
        .await?;
    Ok(())
}

/// Mint an invite token for remote joins (`meshnode invite create`). Built
/// and signed here — the engine never holds the human key (§7.1). `addrs`
/// are extra bootstrap endpoints put first in the token (`--addr`).
#[tauri::command]
async fn invite_create(
    state: State<'_, Director>,
    ttl_secs: u64,
    uses: u32,
    tags: Vec<String>,
    addrs: Vec<String>,
) -> Result<String, String> {
    let engine = state.engine().await?;
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs: known,
    } = engine.ipc(IpcRequest::InviteBootstrap).await?
    else {
        return Err("unexpected response".into());
    };
    let mut bootstrap: Vec<String> = addrs
        .into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();
    for a in known {
        if !bootstrap.contains(&a) {
            bootstrap.push(a);
        }
    }
    let params = membership::InviteParams {
        ttl_secs: ttl_secs.max(60),
        uses: uses.max(1),
        tags: tags
            .into_iter()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect(),
        bootstrap,
    };
    let (token, public) = membership::build_invite(mesh_id, &engine.user, &params, node_id)
        .map_err(|e| e.to_string())?;
    engine.ipc(IpcRequest::InvitePublish { public }).await?;
    Ok(token.encode())
}

/// Every invite this mesh knows about (`meshnode invite ls --all`).
#[tauri::command]
async fn invites_list(state: State<'_, Director>) -> Result<Vec<InviteView>, String> {
    let engine = state.engine().await?;
    match engine.ipc(IpcRequest::Invites).await? {
        IpcResponse::Invites(rows) => Ok(rows.into_iter().map(Into::into).collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Take an invite back before it runs out (`meshnode invite revoke`,
/// SEC-17). `invite` is an invite id or the token itself, so the panel can
/// revoke the one it has just shown without a round trip for its id.
#[tauri::command]
async fn invite_revoke(state: State<'_, Director>, invite: String) -> Result<(), String> {
    let invite = invite.trim();
    let invite_id = match mesh_proto::InviteId::parse(invite) {
        Ok(id) => id,
        Err(_) => {
            mesh_proto::InviteToken::decode(invite)
                .map_err(|_| "not an invite id or an invite token".to_string())?
                .invite_id
        }
    };
    let engine = state.engine().await?;
    let by = engine.user.user_id();
    let signature = engine.sign(&mesh_proto::invite_revoke_signing_bytes(&invite_id, &by))?;
    engine
        .ipc(IpcRequest::InviteRevoke {
            invite_id,
            by,
            signature,
        })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// LAN pairing, approver side (Mesh view: "pairing requests surface here")
// ---------------------------------------------------------------------------

#[tauri::command]
async fn discover_candidates(wait_ms: u64) -> Result<Vec<CandidateView>, String> {
    // An mDNS browse holds a daemon open for `wait_ms` and answers from
    // the network. The engine's threads, like every other listener.
    let found = engine::on_engine("browsing for candidates", async move {
        pairing::browse(std::time::Duration::from_millis(wait_ms)).await
    })
    .await?
    .map_err(|e| e.to_string())?;
    Ok(found.into_iter().map(Into::into).collect())
}

// -- the background LAN watch --------------------------------------------------
//
// The Access page used to *sweep*: a fresh mDNS browse every 2.5 seconds
// from the browser's poll loop, forgotten between sweeps, paused whenever
// the window was hidden. A joiner that answered between two sweeps, or
// whose network dropped multicast, simply never appeared — and the page
// could not say why, because it did not know. Now one listener runs in
// the engine for as long as an admin is around (`pairing::LanWatch`), on
// mDNS *and* a UDP broadcast beacon, and the page reads a table.

#[tauri::command]
async fn lan_watch_start(state: State<'_, Director>) -> Result<(), String> {
    if state.inner.lock().await.lan_watch.is_some() {
        return Ok(());
    }
    // `LanWatch::start` is where the comment above becomes true: it spawns
    // a UDP beacon listener, and a spawned task belongs to the runtime
    // that spawned it. Started from a Tauri command it would be a mesh
    // listener living on the window's threads for as long as an admin has
    // the page open.
    let watch = engine::on_engine("starting the LAN watch", async move {
        pairing::LanWatch::start()
    })
    .await?;
    let mut inner = state.inner.lock().await;
    if inner.lan_watch.is_none() {
        inner.lan_watch = Some(watch);
    }
    // Otherwise a concurrent call won the race and ours is dropped here,
    // which stops it — `LanWatch`'s own `Drop` shuts the browse down.
    Ok(())
}

#[tauri::command]
async fn lan_watch_stop(state: State<'_, Director>) -> Result<(), String> {
    state.inner.lock().await.lan_watch = None;
    Ok(())
}

#[tauri::command]
async fn lan_watch_view(state: State<'_, Director>) -> Result<LanWatchView, String> {
    let inner = state.inner.lock().await;
    let local_addrs: Vec<String> = pairing::local_endpoints(0)
        .into_iter()
        .map(|a| a.ip().to_string())
        .collect();
    Ok(match &inner.lan_watch {
        Some(w) => {
            let r = w.report();
            LanWatchView {
                running: true,
                candidates: r.candidates.into_iter().map(Into::into).collect(),
                mdns_error: r.mdns_error,
                beacon_error: r.beacon_error,
                beacon_port: pairing::PAIR_BEACON_PORT,
                local_addrs,
            }
        }
        None => LanWatchView {
            running: false,
            candidates: Vec::new(),
            mdns_error: None,
            beacon_error: None,
            beacon_port: pairing::PAIR_BEACON_PORT,
            local_addrs,
        },
    })
}

/// Every rendering of a pairing code: digits, word pair, QR path.
///
/// Synchronous and engine-free on purpose — it is pure formatting over six
/// digits the caller already has, so the pairing dialog does not wait on a
/// node that may not be started yet (the wizard shows a code *before* this
/// peer is a member of anything).
#[tauri::command]
fn pair_code_view(code: String) -> PairCodeView {
    PairCodeView::of(&code)
}

/// Let a machine in: dial it, show the code, wait for both verdicts.
///
/// Takes every address the candidate was heard at rather than one. A
/// laptop with a VPN or Docker announces several, and the first one
/// listed used to be the one dialled — which on the wrong day was
/// `172.17.0.1`. They are tried best-first (`pairing::rank_addrs`), moving
/// on only while the failure is a failure to *connect*; once a code has
/// been shown, that session is the session.
#[tauri::command]
async fn pair_approve_start(
    state: State<'_, Director>,
    addrs: Vec<String>,
    tags: Vec<String>,
    channel: Channel<ApproveEvent>,
) -> Result<(), String> {
    let addrs: Vec<String> = addrs
        .into_iter()
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .collect();
    if addrs.is_empty() {
        return Err("no address to dial".into());
    }
    let engine = state.engine().await?;
    // Single-use invite for provenance, exactly like the CLI approver.
    let IpcResponse::InviteBootstrap {
        mesh_id,
        node_id,
        addrs: bootstrap,
    } = engine.ipc(IpcRequest::InviteBootstrap).await?
    else {
        return Err("unexpected response".into());
    };
    let params = membership::InviteParams {
        ttl_secs: 600,
        uses: 1,
        tags,
        bootstrap,
    };
    let (token, public) = membership::build_invite(mesh_id, &engine.user, &params, node_id)
        .map_err(|e| e.to_string())?;
    engine.ipc(IpcRequest::InvitePublish { public }).await?;
    let token = token.encode();

    tokio::spawn(async move {
        let last = addrs.len() - 1;
        for (i, addr) in addrs.into_iter().enumerate() {
            if channel
                .send(ApproveEvent::Trying { addr: addr.clone() })
                .is_err()
            {
                return;
            }
            let mut rx = match engine
                .ipc_stream(IpcRequest::PairApprove {
                    addr: addr.clone(),
                    token: token.clone(),
                })
                .await
            {
                Ok(rx) => rx,
                Err(message) => {
                    let _ = channel.send(ApproveEvent::Failed { message });
                    return;
                }
            };
            // Until the first code frame, a failure is a failure to reach
            // this address and the next one deserves a go. After it, the
            // exchange is under way and its verdict is final.
            let mut reached = false;
            while let Some(frame) = rx.recv().await {
                let ev = match frame {
                    IpcResponse::PairCode {
                        session,
                        code,
                        joiner_name,
                        joiner_id,
                    } => {
                        reached = true;
                        // Shown beside the code the person compares; direction
                        // controls reorder text even where HTML is escaped
                        // (SEC-9).
                        ApproveEvent::Code {
                            session,
                            code,
                            joiner_name: mesh_proto::text::clean(
                                &joiner_name,
                                mesh_proto::text::MAX_NAME_CHARS,
                            ),
                            joiner_id: joiner_id.to_display_string(),
                        }
                    }
                    IpcResponse::PairDone { node_id, node_name } => ApproveEvent::Done {
                        node_id: node_id.to_display_string(),
                        node_name: mesh_proto::text::clean(
                            &node_name,
                            mesh_proto::text::MAX_NAME_CHARS,
                        ),
                    },
                    IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                        if !reached && i < last {
                            tracing::info!(
                                addr,
                                message,
                                "pairing dial failed; trying the next address"
                            );
                            break;
                        }
                        ApproveEvent::Failed { message }
                    }
                    _ => continue,
                };
                let done = !matches!(ev, ApproveEvent::Code { .. });
                if channel.send(ev).is_err() || done {
                    return;
                }
            }
            if reached {
                // The stream ended without a verdict: the other side went
                // away mid-exchange.
                let _ = channel.send(ApproveEvent::Failed {
                    message: "the other side went away before the exchange finished".into(),
                });
                return;
            }
        }
        let _ = channel.send(ApproveEvent::Failed {
            message: "could not reach the machine at any of its addresses — is it still on the pairing screen, and on this network?".into(),
        });
    });
    Ok(())
}

#[tauri::command]
async fn pair_approve_decide(
    state: State<'_, Director>,
    session: u64,
    accept: bool,
) -> Result<(), String> {
    state
        .engine()
        .await?
        .ipc(IpcRequest::PairDecide { session, accept })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Day-2 (MASTERPLAN §12): doctor, config, backup, drain
// ---------------------------------------------------------------------------

/// `meshnode doctor` against this peer's engine.
#[tauri::command]
async fn doctor_run(state: State<'_, Director>) -> Result<Vec<DoctorCheckView>, String> {
    let engine = state.engine().await?;
    // `meshnode doctor`, run from here: this machine's node's configuration
    // as `meshnode` resolves it, probed from Director's side of the socket.
    // Slow by nature — sockets, ports, clocks, disks — so off the window's
    // threads.
    let config = node_config_on_disk()?;
    let user = engine.user.clone();
    let checks = engine::on_engine("running doctor", async move {
        mesh_core::doctor::run(&config, &user).await
    })
    .await?;
    Ok(checks
        .into_iter()
        .map(|c| DoctorCheckView {
            name: c.name.to_string(),
            verdict: match c.verdict {
                mesh_core::doctor::Verdict::Ok => "ok",
                mesh_core::doctor::Verdict::Warn => "warn",
                mesh_core::doctor::Verdict::Fail => "fail",
            }
            .to_string(),
            detail: c.detail,
        })
        .collect())
}

/// `meshnode doctor --fix`: apply the safe fixes, then look again.
#[tauri::command]
async fn doctor_fix(state: State<'_, Director>) -> Result<DoctorFixReport, String> {
    let engine = state.engine().await?;
    // As `doctor_run`, and more so: `fix` writes as well as probes.
    let config = node_config_on_disk()?;
    let user = engine.user.clone();
    // Only `fixes` and `after` leave the hop: `before` exists to tell `fix`
    // what is wrong, and the report says what was done and what is true now.
    let (fixes, after) = engine::on_engine("running doctor --fix", async move {
        let before = mesh_core::doctor::run(&config, &user).await;
        let fixes = mesh_core::doctor::fix(&config, &user, &before).await;
        let after = mesh_core::doctor::run(&config, &user).await;
        (fixes, after)
    })
    .await?;
    let check_view = |c: mesh_core::doctor::Check| DoctorCheckView {
        name: c.name.to_string(),
        verdict: match c.verdict {
            mesh_core::doctor::Verdict::Ok => "ok",
            mesh_core::doctor::Verdict::Warn => "warn",
            mesh_core::doctor::Verdict::Fail => "fail",
        }
        .to_string(),
        detail: c.detail,
    };
    Ok(DoctorFixReport {
        fixes: fixes
            .into_iter()
            .map(|f| DoctorFixView {
                name: f.name.to_string(),
                applied: f.applied,
                detail: f.detail,
            })
            .collect(),
        after: after.into_iter().map(check_view).collect(),
    })
}

/// `meshnode drain --until` / `--for` / `--lift`: refuse new work until a
/// deadline, keep what is running, and rejoin on your own. Unlike `drain`
/// the engine stays up. `until_secs` = `None` lifts a timed drain early.
#[tauri::command]
async fn drain_timed(
    state: State<'_, Director>,
    until_secs: Option<u64>,
    reason: String,
) -> Result<String, String> {
    let engine = state.engine().await?;
    match engine
        .ipc(IpcRequest::DrainTimed {
            until_micros: until_secs.map(|s| s.saturating_mul(1_000_000)),
            reason,
            move_pending: false,
        })
        .await?
    {
        IpcResponse::Notice { message } => Ok(message),
        IpcResponse::Denied { message } | IpcResponse::Error { message } => Err(message),
        other => Err(format!("unexpected reply: {other:?}")),
    }
}

/// `meshnode config show`: this peer's resolved engine configuration.
#[tauri::command]
async fn config_view(state: State<'_, Director>) -> Result<String, String> {
    state.engine().await?;
    serde_json::to_string_pretty(&node_config_on_disk()?).map_err(|e| e.to_string())
}

/// This machine's node's configuration as `meshnode config show` resolves
/// it — its `meshnode.toml` over the defaults — for the screens that show or
/// check it. What the running node has *applied* can lag a reload behind;
/// this is what it will run with, which is the question those screens ask.
fn node_config_on_disk() -> Result<mesh_core::config::NodeConfig, String> {
    engine::node_config(None, Vec::new())
}

/// `meshnode backup`: one restorable file (identity + genesis + op-log).
///
/// The chooser opens here (S3). A backup holds this node's private keys, so
/// "write these bytes wherever the page says" was the wrong shape for it
/// twice over. `None` is a cancel.
#[tauri::command]
async fn backup_write(
    state: State<'_, Director>,
    app: tauri::AppHandle,
    suggested: String,
) -> Result<Option<BackupView>, String> {
    let Some(abs) = ask_where_to_save(
        &app,
        "Save a mesh backup",
        &suggested,
        Some(("Meshware backup", &["mwb", "mwbak"])),
    )
    .await?
    else {
        return Ok(None);
    };
    let IpcResponse::BackupDone { path, ops, bytes } = state
        .engine()
        .await?
        .ipc(IpcRequest::Backup {
            path: abs.to_string_lossy().into_owned(),
        })
        .await?
    else {
        return Err("unexpected response".into());
    };
    Ok(Some(BackupView { path, ops, bytes }))
}

// -- M24 definition distribution ---------------------------------------------

/// Everything this mesh distributes (§3.1).
#[tauri::command]
async fn definitions_view(state: State<'_, Director>) -> Result<Vec<DefinitionView>, String> {
    let IpcResponse::Definitions(rows) = state.engine().await?.ipc(IpcRequest::Definitions).await?
    else {
        return Err("unexpected response".into());
    };
    Ok(rows.into_iter().map(Into::into).collect())
}

/// "Push to nodes" (§3.2): ask a set of nodes to fetch a bundle now.
///
/// It does not change what is authoritative — the bundle is already
/// addressed by digest, already published, already resolvable. It decides
/// *when* each node pulls, before somebody is waiting on it.
#[tauri::command]
async fn bundle_prefetch(
    state: State<'_, Director>,
    bundle: String,
    selector: String,
    channel: Channel<PrefetchEvent>,
) -> Result<(), String> {
    let bundle_id = mesh_proto::BundleId::parse(bundle.trim())
        .map_err(|e| format!("invalid bundle id: {e}"))?;
    let engine = state.engine().await?;
    let mut rx = engine
        .ipc_stream(IpcRequest::BundlePrefetch {
            bundle_id,
            selector,
        })
        .await?;
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            match frame {
                IpcResponse::PrefetchProgress {
                    node,
                    progress,
                    unreachable,
                } => {
                    let _ = channel.send(PrefetchEvent::Node {
                        node,
                        have_chunks: progress.have_chunks,
                        total_chunks: progress.total_chunks,
                        have_bytes: progress.have_bytes,
                        total_bytes: progress.total_bytes,
                        complete: progress.complete,
                        unreachable,
                    });
                }
                IpcResponse::PrefetchDone {
                    complete,
                    fetching,
                    skipped,
                } => {
                    let _ = channel.send(PrefetchEvent::Done {
                        complete,
                        fetching,
                        skipped,
                    });
                    return;
                }
                IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                    let _ = channel.send(PrefetchEvent::Failed { message });
                    return;
                }
                _ => {}
            }
        }
        let _ = channel.send(PrefetchEvent::Failed {
            message: "the push stream ended without a summary".into(),
        });
    });
    Ok(())
}

/// Widen (or narrow) the ring a collector runs on (§3.3).
///
/// An edit, so the record is re-signed and re-published — there is no verb
/// that changes what a fleet runs without a signature behind it. It is here
/// rather than folded into the collector editor because widening is a
/// *deployment*, and a deployment deserves its own button.
#[tauri::command]
async fn collector_widen(
    state: State<'_, Director>,
    name: String,
    selector: String,
) -> Result<(), String> {
    let selector = selector.trim().to_string();
    if selector.is_empty() {
        return Err("a ring needs a selector".into());
    }
    let engine = state.engine().await?;
    let IpcResponse::FactCollectors(rows) = engine.ipc(IpcRequest::FactCollectors).await? else {
        return Err("unexpected response".into());
    };
    let row = rows
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| format!("no collector named {name:?}"))?;
    if row.built_in {
        return Err(
            "the built-in collector runs on every node in process — its ring is not editable"
                .into(),
        );
    }
    let mut collector = mesh_core::facts::parse_collector(&row.source, engine.user.user_id())
        .map_err(|e| e.to_string())?;
    collector.on = selector;
    let signed = mesh_core::facts::sign_collector(&engine.user, collector);
    engine
        .ipc(IpcRequest::FactCollectorPut {
            collector: Box::new(signed),
        })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Releases: trust, and publishing a build (M24 §9)
// ---------------------------------------------------------------------------
//
// The manual sequence this replaces was: generate a key, edit meshnode.toml
// on every machine, restart every daemon, build, sign by hand, find the
// artifact path, run a CLI with the right --home. Every step of that is a
// place to be wrong quietly, and the last one is the only step that was
// ever actually about *this mesh*.
//
// Trust replicates now (`ReleaseKeyPut`), so the middle four steps are one
// admin act. What is left is: point at a build, and publish it.

/// What this fleet verifies releases against.
#[tauri::command]
async fn release_trust(state: State<'_, Director>) -> Result<view::ReleaseTrustView, String> {
    let IpcResponse::ReleaseKeys(v) = state.engine().await?.ipc(IpcRequest::ReleaseKeys).await?
    else {
        return Err("unexpected response".into());
    };
    let live = v.keys.iter().any(|k| !k.revoked);
    // Y6. The first SSH-form key trusted turns the co-signature on for
    // every release, so the screen shows each one's standing next to it.
    let cosign_required = v
        .keys
        .iter()
        .any(|k| !k.revoked && mesh_proto::ReleaseKey::looks_like_ssh(&k.pubkey));
    let releases = match state.engine().await?.ipc(IpcRequest::Releases).await {
        Ok(IpcResponse::Releases(rows)) => rows
            .into_iter()
            .map(|r| view::ReleaseCosignView {
                version: r.version,
                arch: r.arch,
                digest: r.digest,
                cosigned_by: r.cosigned_by,
                user_present: r.cosign_user_present,
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(view::ReleaseTrustView {
        cosign_required,
        releases,
        nothing_would_verify: !live && v.local_override.is_none() && v.embedded_is_placeholder,
        keys: v
            .keys
            .into_iter()
            .map(|k| view::ReleaseKeyView {
                pubkey: k.pubkey,
                label: k.label,
                added_by: k.added_by,
                added_at_micros: k.added_at_micros.to_string(),
                revoked: k.revoked,
                in_use_here: k.in_use_here,
            })
            .collect(),
        local_override: v.local_override,
        embedded_is_placeholder: v.embedded_is_placeholder,
    })
}

/// Trust a release key, mesh-wide. `source` is a `.pub` file or the bare
/// `RW…` line — whichever the operator has in front of them.
#[tauri::command]
async fn release_key_trust(
    state: State<'_, Director>,
    source: String,
    // A picked `.pub` file (SEC-14). `source` is pasted text only: it used
    // to be read as a path whenever one existed by that name.
    file: Option<String>,
    label: Option<String>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let raw = match file.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => {
            let path = state.picked.require(token)?;
            let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
            if meta.len() > 4096 {
                return Err("that is not a minisign public key file".into());
            }
            std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?
        }
        None => source.clone(),
    };
    let pubkey = raw
        .lines()
        .map(str::trim)
        .rfind(|l: &&str| !l.is_empty() && !l.starts_with("untrusted comment"))
        .unwrap_or("")
        .to_string();
    if !mesh_proto::ReleaseKey::looks_like_a_key(&pubkey) {
        return Err(
            "that is not a release key. Paste the RW… line of a minisign .pub (the second \
             line), or an ssh-ed25519 / sk-ssh-ed25519 line for a security key's co-signature \
             — or pick the .pub file itself."
                .into(),
        );
    }
    let mut key = mesh_proto::ReleaseKey {
        pubkey,
        label: label
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "release signing key".into()),
        added_by: engine.user.user_id(),
        added_at_micros: now_micros(),
        signature: Vec::new(),
    };
    key.signature = engine.sign(&key.signing_bytes())?;
    engine
        .ipc(IpcRequest::ReleaseKeyAdd { key: Box::new(key) })
        .await?;
    Ok(())
}

/// Withdraw trust in a release key.
#[tauri::command]
async fn release_key_revoke(state: State<'_, Director>, pubkey: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let pubkey = pubkey.trim().to_string();
    let at_micros = now_micros();
    let signature = engine
        .user
        .sign(&mesh_proto::release_key_revoke_signing_bytes(
            &pubkey,
            &engine.user.user_id(),
            at_micros,
        ))
        .to_vec();
    engine
        .ipc(IpcRequest::ReleaseKeyRevoke {
            pubkey,
            by: engine.user.user_id(),
            at_micros,
            signature,
        })
        .await?;
    Ok(())
}

/// Publish a security key's co-signature for one published binary
/// (`meshnode upgrade --cosign`, Y6).
///
/// `sig` is a picked file: the document `ssh-keygen -Y sign -n
/// meshware-release <binary>` wrote. `digest` names the binary, as the
/// release list gives it — one version can be several architectures, and
/// each binary is its own signature. Parsed here so a wrong file is a
/// sentence rather than an op every node quietly declines to record; the
/// signature itself is verified by every node, against the keys it trusts.
#[tauri::command]
async fn release_cosign(
    state: State<'_, Director>,
    sig: String,
    digest: String,
) -> Result<String, String> {
    let path = state.picked.require(&sig)?;
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    if meta.len() > 16 * 1024 {
        return Err("that is not an SSH signature — one is a few hundred bytes".into());
    }
    let armored = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let parsed = mesh_core::sshsig::parse(&armored).map_err(|e| e.to_string())?;
    if parsed.namespace != mesh_core::sshsig::RELEASE_NAMESPACE {
        return Err(format!(
            "that signature was made for {:?}. Releases are signed under {:?}: ssh-keygen -Y \
             sign -f <key> -n {} meshnode",
            parsed.namespace,
            mesh_core::sshsig::RELEASE_NAMESPACE,
            mesh_core::sshsig::RELEASE_NAMESPACE,
        ));
    }
    let digest = mesh_proto::parse_hex32(digest.trim())
        .ok_or_else(|| "that release has no readable digest".to_string())?;
    let engine = state.engine().await?;
    let mut cosign = mesh_proto::ReleaseCosignature {
        digest,
        armored,
        published_by: engine.user.user_id(),
        published_at_micros: now_micros(),
        signature: Vec::new(),
    };
    cosign.signature = engine.sign(&cosign.signing_bytes())?;
    engine
        .ipc(IpcRequest::ReleaseCosign {
            cosign: Box::new(cosign),
        })
        .await?;
    Ok(match (parsed.is_token_backed(), parsed.user_present()) {
        (true, true) => "Co-signed by a security key, with a touch recorded.".into(),
        (true, false) => "Co-signed by a security key.".into(),
        (false, _) => "Co-signed.".into(),
    })
}

/// Versions this mesh has published, newest first.
///
/// The rollout screen needs these to answer "is this fleet up to date",
/// which is otherwise measured against *the newest build some node
/// happens to be running* — a reading that calls a fleet current the
/// moment nobody has upgraded yet, which is exactly when it is not.
#[tauri::command]
async fn release_versions(state: State<'_, Director>) -> Result<Vec<String>, String> {
    let IpcResponse::Releases(rows) = state.engine().await?.ipc(IpcRequest::Releases).await? else {
        return Err("unexpected response".into());
    };
    // One version may be published once per architecture; the screen
    // asks "which versions exist", so the rows collapse to that.
    let mut versions: Vec<String> = Vec::new();
    for r in rows {
        if !versions.contains(&r.version) {
            versions.push(r.version);
        }
    }
    Ok(versions)
}

/// Look at a directory of build output and say, for each meshnode binary
/// in it, whether this mesh could publish it — *before* anyone commits to
/// one.
///
/// Every refusal the publish path can produce is worked out here instead:
/// unsigned, signed by a key nobody trusts, a binary that reports a
/// different version than its name claims, already published. A list of
/// builds with one blocked and a sentence saying why beats a file picker
/// and an error afterwards.
#[tauri::command]
async fn release_candidates(
    state: State<'_, Director>,
    // A folder just picked, which becomes the remembered one; `None` scans
    // the remembered one (SEC-14).
    token: Option<String>,
) -> Result<Vec<view::BuildCandidateView>, String> {
    let engine = state.engine().await?;
    let dir = match token.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(token) => {
            let dir = state.picked.require(token)?;
            if let Some(mut settings) = engine::Settings::load() {
                settings.release_build_dir = Some(dir.clone());
                let _ = settings.save();
            }
            dir
        }
        None => match engine::Settings::load().and_then(|s| s.release_build_dir) {
            Some(dir) => dir,
            None => return Ok(Vec::new()),
        },
    };
    let dir = std::fs::canonicalize(&dir).map_err(|e| format!("cannot read that folder: {e}"))?;

    let trusted: Vec<String> = match engine.ipc(IpcRequest::ReleaseKeys).await {
        Ok(IpcResponse::ReleaseKeys(v)) => match v.local_override {
            Some(local) => vec![local],
            None => v
                .keys
                .into_iter()
                .filter(|k| !k.revoked)
                .map(|k| k.pubkey)
                .collect(),
        },
        _ => Vec::new(),
    };
    // **Off the UI worker** (**M-23**). This reads a directory, then
    // reads and minisign-verifies *every* binary in it — tens of
    // megabytes apiece — with `std::fs` on an async worker. Pointing
    // Updates at a folder on a network drive stalled the window for
    // seconds. The mesh was never at risk (Director's engine has its own
    // runtime), but the window is the thing the person is looking at.
    //
    // The one IPC stays out here: it is a message to the engine, not disk
    // work, and moving it inside would mean blocking a thread on a reply.
    //
    // `(version, arch)` pairs: a version is one binary *per architecture*,
    // so an aarch64 build of a version already published for x86_64 is
    // exactly what a mixed fleet is waiting for, not a duplicate.
    let published: Vec<(String, String)> = match engine.ipc(IpcRequest::Releases).await {
        Ok(IpcResponse::Releases(rows)) => rows.into_iter().map(|r| (r.version, r.arch)).collect(),
        _ => Vec::new(),
    };
    let scan_dir = dir.clone();
    let mut out = tokio::task::spawn_blocking(move || {
        let dir = scan_dir;
        let mut out = Vec::new();
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("cannot read that folder: {e}"))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // The bare binary, which is what a node installs — not the
            // tarball, not the .deb, not the signature.
            if !name.starts_with("meshnode-")
                || name.contains(".tar")
                || name.ends_with(".minisig")
                || name.ends_with(".deb")
                || name.ends_with(".rpm")
                || !path.is_file()
            {
                continue;
            }
            // meshnode-v0.5.2-x86_64-unknown-linux-musl
            let rest = name.trim_start_matches("meshnode-").to_string();
            let Some((tag, target)) = rest.split_once('-') else {
                continue;
            };
            let target = target.to_string();
            let version = tag.trim_start_matches(['v', 'V']).to_string();
            if version.is_empty() || !version.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                continue;
            }
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let sig_path = path.with_file_name(format!("{name}.minisig"));
            let minisig = std::fs::read_to_string(&sig_path).ok();
            let signed = minisig.is_some();
            let bytes = std::fs::read(&path).unwrap_or_default();
            let verified = match &minisig {
                Some(sig) => mesh_core::release::verify_minisign_any(&bytes, sig, &trusted).is_ok(),
                None => false,
            };
            let arch = target.split('-').next().unwrap_or("").to_string();
            let already = published
                .iter()
                .any(|(v, a)| *v == version && (a.is_empty() || *a == arch));
            let blocked = if !signed {
                Some(format!(
                    "not signed — no node will install it. Sign it with `minisign -HSm {name} -s \
                     <your key>`, or rebuild with packaging/build-release.sh."
                ))
            } else if trusted.is_empty() {
                Some(
                    "this mesh trusts no release key yet — add one above, then this becomes \
                     publishable."
                        .into(),
                )
            } else if !verified {
                Some(
                    "signed by a key this mesh does not trust. Add that key above, or sign the \
                     build with a key the mesh already trusts."
                        .into(),
                )
            } else if already {
                Some(format!(
                    "{version} is already published in this mesh for {arch} — a version names one \
                     binary per architecture for the life of the mesh."
                ))
            } else {
                None
            };
            out.push(view::BuildCandidateView {
                path: path.to_string_lossy().into_owned(),
                token: String::new(),
                file_name: name,
                version,
                arch,
                size: size.to_string(),
                signed,
                verified,
                published: already,
                blocked,
            });
        }
        Ok::<_, String>(out)
    })
    .await
    .map_err(|e| format!("scanning that folder failed: {e}"))??;
    out.sort_by(|a, b| b.version.cmp(&a.version).then(a.arch.cmp(&b.arch)));
    for c in &mut out {
        c.token = state.picked.remember(std::path::Path::new(&c.path));
    }
    Ok(out)
}

/// The remembered build folder, to show (SEC-14).
#[tauri::command]
fn release_build_dir() -> Option<String> {
    engine::Settings::load()
        .and_then(|s| s.release_build_dir)
        .map(|p| p.to_string_lossy().into_owned())
}

/// Forget the remembered build folder.
#[tauri::command]
fn release_build_dir_forget() -> Result<(), String> {
    match engine::Settings::load() {
        Some(mut settings) => {
            settings.release_build_dir = None;
            settings.save()
        }
        None => Ok(()),
    }
}

/// Publish one build into the mesh: stage it under its release artifact
/// name, push it as a content-addressed bundle, and append the
/// admin-signed record that names it (§9).
///
/// The same sequence `meshnode upgrade --publish` performs, with the same
/// checks in the same order — this is a second door onto one path, not a
/// second path.
#[tauri::command]
async fn release_publish(
    state: State<'_, Director>,
    // A build `release_candidates` listed (SEC-14).
    token: String,
    version: String,
) -> Result<String, String> {
    let engine = state.engine().await?;
    let binary = std::fs::canonicalize(state.picked.require(&token)?)
        .map_err(|e| format!("cannot read that file: {e}"))?;
    let bare = version.trim().trim_start_matches(['v', 'V']).to_string();
    let bytes = std::fs::read(&binary).map_err(|e| format!("reading the binary: {e}"))?;

    // Refuse early rather than on every node at once: the upgrader runs
    // `--version` on what it installs and rejects a mismatch, so a wrong
    // binary here is a fleet-wide no-op discovered one node at a time.
    // Only where this machine can run it: a build for another architecture
    // is trusted by its file name, and every node still checks.
    let arch = arch_of(&binary);
    if arch == std::env::consts::ARCH {
        let printed = std::process::Command::new(&binary)
            .arg("--version")
            .output()
            .map_err(|e| format!("running {} --version: {e}", binary.display()))?;
        let printed = String::from_utf8_lossy(&printed.stdout).trim().to_string();
        if !printed.contains(&bare) {
            return Err(format!(
                "that binary reports {printed:?}, not {bare} — every node would fetch it, refuse \
                 it, and leave itself untouched."
            ));
        }
    }

    let sig_path = binary.with_file_name(format!(
        "{}.minisig",
        binary.file_name().unwrap_or_default().to_string_lossy()
    ));
    let minisig = std::fs::read_to_string(&sig_path).map_err(|_| {
        format!(
            "no signature beside that binary ({}). A release must carry one, or no node will \
             install it.",
            sig_path.display()
        )
    })?;
    channel::publish_bytes(&engine, &bytes, &minisig, &bare, &arch).await
}

/// The architecture a release artifact name should carry. Read from the
/// binary being published rather than from this machine: a Director on
/// aarch64 publishing an x86_64 build must name it x86_64, or every node
/// fetches an artifact that is not in the bundle.
fn arch_of(binary: &Path) -> String {
    channel::arch_of_file(binary)
}

/// `meshnode upgrade --on` from Director (§4).
///
/// Serial, health-gated, driver last, stop on first failure — every one of
/// those lives in the daemon that drives it, not here. Director's job is to
/// show what it is doing, because a rollout you cannot watch is a rollout
/// you cannot stop.
// A Tauri command's arguments are its call shape from the frontend, so
// they stay flat rather than being folded into a struct nothing else has a
// use for.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn upgrade_fleet(
    app: AppHandle,
    state: State<'_, Director>,
    selector: String,
    version: String,
    batch: u32,
    move_pending: bool,
    force: bool,
    channel: Channel<UpgradeEvent>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    // Client-built and client-signed, like every other governance body: the
    // signature on this plan is what every node in the ring re-verifies
    // against its own state before it touches its own binary.
    let now = now_micros();
    let mut plan = mesh_proto::UpgradePlan {
        plan_id: mesh_proto::UpgradeId::generate(),
        selector,
        version: version.trim().trim_start_matches(['v', 'V']).to_string(),
        move_pending,
        batch,
        created_by: engine.user.user_id(),
        created_at_micros: now,
        expires_at_micros: now + mesh_proto::DEFAULT_PLAN_TTL_SECS * 1_000_000,
        signature: Vec::new(),
    };
    // Signed over the forced domain when the plan is a forced one:
    // forcing is not a field of the record but which op body carries it,
    // so the signature has to cover it separately or an ordinary plan
    // could be lifted off the gossip stream and republished as forced.
    plan.signature = engine.sign(&plan.signing_bytes_forced(force))?;
    let mut rx = engine
        .ipc_stream(IpcRequest::UpgradeFleet {
            plan: Box::new(plan),
            force,
        })
        .await?;
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            match frame {
                IpcResponse::UpgradeProgress {
                    node,
                    state,
                    detail,
                } => {
                    let _ = channel.send(UpgradeEvent::Node {
                        node,
                        state,
                        detail,
                    });
                }
                IpcResponse::UpgradeDone {
                    upgraded,
                    already,
                    driver_restarting,
                    remaining,
                    stopped_at,
                } => {
                    notify::attention(
                        &app,
                        "Rollout finished",
                        &match &stopped_at {
                            Some(node) => format!(
                                "stopped at {node}: {upgraded} updated, {remaining} not started"
                            ),
                            None => format!("{upgraded} updated, {already} already current"),
                        },
                    );
                    let _ = channel.send(UpgradeEvent::Done {
                        upgraded,
                        already,
                        driver_restarting,
                        remaining,
                        stopped_at,
                    });
                    return;
                }
                IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                    let _ = channel.send(UpgradeEvent::Failed { message });
                    return;
                }
                _ => {}
            }
        }
        // The driver upgrades itself last and then drains, so a stream that
        // simply ends is the expected ending of a rollout that included
        // this node.
        let _ = channel.send(UpgradeEvent::Done {
            upgraded: 0,
            already: 0,
            driver_restarting: true,
            remaining: 0,
            stopped_at: None,
        });
    });
    Ok(())
}

/// `meshnode drain [--move]`: stop accepting work, finish (or move) what
/// was accepted, then stop this machine's node. Director stays open; the
/// Settings view offers to start the node again.
#[tauri::command]
async fn drain(
    app: AppHandle,
    state: State<'_, Director>,
    move_pending: bool,
    channel: Channel<DrainEvent>,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let mut rx = engine
        .ipc_stream(IpcRequest::Drain { move_pending })
        .await?;
    tokio::spawn(async move {
        let mut drained = false;
        while let Some(frame) = rx.recv().await {
            match frame {
                IpcResponse::DrainProgress {
                    running,
                    queued,
                    moved,
                    devices_held,
                } => {
                    let _ = channel.send(DrainEvent::Progress {
                        running,
                        queued,
                        moved,
                        devices_held,
                    });
                }
                IpcResponse::Ok => {
                    drained = true;
                    break;
                }
                IpcResponse::Denied { message } | IpcResponse::Error { message } => {
                    let _ = channel.send(DrainEvent::Failed { message });
                    return;
                }
                _ => {}
            }
        }
        if !drained {
            let _ = channel.send(DrainEvent::Failed {
                message: "the drain stream ended before the engine reported idle".into(),
            });
            return;
        }
        // The node exits here, as a drained daemon does. Run as the user
        // unit it would be back two seconds later (`Restart=always`), which
        // is right for a crash and wrong for "drain this machine before
        // maintenance" — so the unit is stopped too, and stays stopped
        // until someone starts it: this window's Start, or `systemctl
        // --user start meshnode`.
        if engine::demo_root().is_none()
            && let Err(e) = tokio::task::spawn_blocking(mesh_core::usernode::stop)
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()))
        {
            tracing::warn!("drained, but stopping the unit failed: {e}");
        }
        let director = app.state::<Director>();
        director.inner.lock().await.engine = None;
        let _ = channel.send(DrainEvent::Done);
    });
    Ok(())
}

// ---------------------------------------------------------------------------

/// Where Director's rolling log lives: `<meshware home>/director/logs`.
pub(crate) fn log_dir() -> std::path::PathBuf {
    engine::app_dir().join("logs")
}

/// The non-blocking log writer's guard — dropped at exit, which is what
/// flushes the last lines. Held for the life of the process.
static LOG_GUARD: std::sync::OnceLock<tracing_appender::non_blocking::WorkerGuard> =
    std::sync::OnceLock::new();

fn init_logging() {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_writer(std::io::stderr));
    // A daily file, seven kept: the thing a bug report attaches. A desktop
    // app's stderr goes nowhere anyone looks, and "what did Director see
    // yesterday" is the first question about a deploy that went wrong.
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("director")
        .filename_suffix("log")
        .max_log_files(7)
        .build(log_dir());
    match file {
        Ok(file) => {
            let (writer, guard) = tracing_appender::non_blocking(file);
            let _ = LOG_GUARD.set(guard);
            let _ = registry
                .with(fmt::layer().with_ansi(false).with_writer(writer))
                .try_init();
        }
        Err(e) => {
            let _ = registry.try_init();
            tracing::warn!("no log file ({e}) — logging to stderr only");
        }
    }
}

pub fn run() {
    init_logging();

    tauri::Builder::default()
        // One Director per machine. A second launch hands its arguments to
        // the first and exits, and the first comes to the front — a second
        // window would be harmless to the node, but it would be a second
        // tray icon, a second log follow per job and a `director://` link
        // opened in whichever one happened to get it. Registered first, as
        // the plugin requires.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            // A `director://` link opened while Director is already running
            // arrives here on Linux and Windows: the desktop launches a
            // second copy with the URL in `argv`, single-instance hands the
            // arguments to the first and exits (L2 P5).
            if let Some(state) = app.try_state::<Director>() {
                deeplink::absorb_args(&state.deep_links, argv.iter().cloned());
            }
            glance::reveal(app);
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        // Window size, position and maximised state come back next launch.
        // Not visibility: a window hidden to the tray at quit must not come
        // back hidden, with no tray yet to find it from.
        .plugin(
            tauri_plugin_window_state::Builder::new()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        // Launch at login starts with `--hidden`: into the tray, not over
        // the desktop (`desktop::autostart_set`).
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        // The release key comes from **one** file (S-L5).
        //
        // `tauri.conf.json`'s `plugins.updater.pubkey` is deliberately the
        // empty string: the plugin's config requires the field, so it cannot
        // be removed, and it used to hold a second, base64'd copy of
        // `packaging/minisign.pub`. `selfupdate.rs` says "a key rotation is
        // one file, not two", and the config file was the second one. It was
        // never authoritative — this override has always won — but a stale
        // copy that only matters if somebody deletes one line is exactly the
        // kind of thing that is discovered by a bad update.
        //
        // Empty also fails *closed*: with the override gone the plugin has
        // no key and verifies nothing rather than verifying against
        // something out of date.
        .plugin(
            tauri_plugin_updater::Builder::new()
                .pubkey(selfupdate::pubkey())
                .build(),
        )
        .manage(Director::default())
        .setup(|app| {
            // The `director://` scheme (L2 P5).
            //
            // Registered at runtime because a *development* build has no
            // installer to do it — on a packaged build the desktop entry is
            // written by the bundler from `tauri.conf.json`, and this is
            // idempotent.
            {
                use tauri_plugin_deep_link::DeepLinkExt as _;
                if let Err(e) = app.deep_link().register_all() {
                    tracing::info!("could not register the director:// scheme ({e})");
                }
            }
            {
                use tauri_plugin_deep_link::DeepLinkExt as _;
                let handle = app.handle().clone();
                // A link that *started* this Director: the URL was in the
                // command line before any of this ran.
                if let Ok(Some(urls)) = app.deep_link().get_current()
                    && let Some(state) = handle.try_state::<Director>()
                {
                    deeplink::absorb_args(
                        &state.deep_links,
                        urls.into_iter().map(|u| u.to_string()),
                    );
                }
                // …and every link opened afterwards. macOS delivers these
                // as an Opened event rather than as arguments, so this is
                // the path that matters there.
                app.deep_link().on_open_url(move |event| {
                    if let Some(state) = handle.try_state::<Director>() {
                        for url in event.urls() {
                            state.deep_links.set(url.as_str());
                        }
                    }
                    glance::reveal(&handle);
                });
            }
            // The glance (`glance.rs`): Meshware in the menu bar. Failing
            // to build a tray is not failing to start — a compositor with
            // no tray is a desktop where Director still works, and the
            // window keeps close-quits there because hiding to a tray that
            // does not exist makes the app unrecoverable.
            // The dock, the alt-tab strip and the overview show a generic
            // glyph for a Director run from a build directory unless a
            // desktop entry names its icon (`linuxdesktop`).
            linuxdesktop::ensure();
            match glance::install(app.handle()) {
                Ok(()) => {
                    // …and *building* a tray is not the same as having one.
                    // On Linux the item is registered on the session bus and
                    // a panel draws it; stock GNOME Shell has no
                    // StatusNotifierItem host, so the build succeeds and
                    // nothing appears. Hiding into that is how a window
                    // vanishes with no way back that anybody would guess, so
                    // `--hidden` is honoured only where the icon is real.
                    if std::env::args().any(|a| a == "--hidden") {
                        if glance::is_visible(app.handle()) {
                            if let Some(window) = app.get_webview_window("main") {
                                let _ = window.hide();
                            }
                        } else {
                            tracing::warn!(
                                "--hidden ignored: the tray icon would not be visible — {}",
                                glance::missing_host_hint()
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::info!("no system tray available ({e}) — running without the glance");
                }
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            // Close hides, if there is a tray to hide to, so the glance in
            // it keeps showing the mesh. Nothing else rides on it any more:
            // this machine's node is its own process, and quitting Director
            // — Quit is on the tray menu — leaves it running.
            //
            // "A tray to hide to" is `glance::is_visible`, not
            // `tray_by_id`. The latter only says an item was registered,
            // which on stock GNOME is true while nothing draws it — and
            // hiding there leaves a running Director with no window and no
            // icon. `is_visible` is re-read every few seconds, so it also
            // starts saying yes the moment somebody enables the extension.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event
                && glance::is_visible(window.app_handle())
            {
                api.prevent_close();
                let _ = window.hide();
            }
            // A drop is a pick (SEC-14): the paths stay here, and the page
            // is told the tokens. It still hears Tauri's own drop event for
            // the hover highlight, and never uses the paths in it.
            if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
                use tauri::Emitter as _;
                let picked = &window.state::<Director>().picked;
                let views: Vec<view::PickedView> =
                    paths.iter().map(|p| picked.remember_view(p)).collect();
                let _ = window.emit("mw-dropped", views);
            }
        })
        .invoke_handler(tauri::generate_handler![
            // B9 — the key-at-rest gate and its Settings panel.
            identity::identity_lock_status,
            identity::identity_unlock,
            identity::identity_lock_now,
            identity::identity_protect,
            identity::identity_unprotect,
            // Y2/Y3 — the security key, and the authorities that vouch for one.
            security_key::security_key_status,
            security_key::security_key_plan,
            security_key::security_key_use,
            security_key::security_key_attest,
            security_key::security_key_withdraw,
            security_key::security_key_connect,
            security_key::security_key_test,
            security_key::security_key_resume,
            security_key::security_key_restore,
            security_key::attest_roots_view,
            security_key::attest_root_add,
            security_key::attest_root_remove,
            bootstrap_status,
            node_facts,
            fact_collectors_view,
            collector_set_paused,
            result_sets_view,
            sessions_view,
            session_policies_view,
            session_policy_put,
            session_policy_remove,
            session_close,
            terminal_open,
            terminal_send,
            terminal_resize,
            terminal_close,
            start_engine,
            create_mesh,
            join_token,
            restore_backup,
            wizard_pair_start,
            wizard_pair_decide,
            wizard_pair_cancel,
            build_run,
            demo::demo_start,
            demo::demo_stop,
            status_view,
            nodes_view,
            content_status,
            content_prune,
            set_content_policy,
            disk_view,
            devices_view,
            top_view,
            totals_view,
            jobs_view,
            job_logs_stop,
            state_versions_view,
            history_jobs,
            history_replay,
            // `archive_query` and `archive_examples` are gone, not merely
            // unregistered — see above (S4).
            retention_view,
            backpressure_view,
            retention_put,
            retention_reset,
            sinks_view,
            sink_kinds,
            sink_put,
            sink_remove,
            sink_test,
            sinks_resume,
            services_view,
            bundles_view,
            sources_view,
            secrets_view,
            secret_providers_view,
            users_view,
            grants_view,
            audit_query,
            whoami_view,
            target_preview,
            node_contribute_set,
            node_quarantine_set,
            node_tags_assign,
            node_drain_set,
            service_account_create,
            save_key_file,
            service_accounts,
            user_rotate_key,
            oidc_providers,
            set_contribute,
            tags_update,
            ping_node,
            run_command,
            run_service,
            parallel_run,
            job_logs,
            job_cancel,
            jobs_clear,
            push_bundle,
            bundle_files,
            service_create,
            service_rollback,
            service_remove,
            source_add,
            source_remove,
            source_sync,
            secret_set,
            secret_remove,
            user_add,
            enrolments_view,
            enrol_approve,
            enrol_decline,
            enrol_request,
            definitions_view,
            bundle_prefetch,
            collector_widen,
            upgrade_fleet,
            release_trust,
            release_key_trust,
            release_key_revoke,
            release_cosign,
            release_versions,
            release_candidates,
            release_build_dir,
            release_build_dir_forget,
            pick_file,
            pick_directory,
            release_publish,
            channel::release_check,
            channel::release_fetch,
            deploy::deploy_preview,
            deploy::deploy_start,
            deploy::deploy_decide,
            deploy::deploy_cancel,
            deploy::upgrade_ssh_start,
            credentials::credential_get,
            credentials::credential_put,
            credentials::credential_forget,
            credentials::credential_users,
            selfupdate::director_update_check,
            selfupdate::director_update_install,
            desktop::autostart_get,
            desktop::autostart_set,
            desktop::copy_text,
            desktop::open_url,
            about::about_view,
            desktop::director_log_dir,
            desktop::open_log_dir,
            desktop::frontend_log,
            desktop::tray_status,
            filesave::file_save_begin,
            filesave::file_save_write,
            filesave::file_save_finish,
            filesave::file_save_abort,
            user_revoke,
            grant_add,
            grant_revoke,
            deployments_view,
            gangs_view,
            job_explain,
            cache_stats,
            farm_build_line,
            cache_purge,
            plan_view,
            deployment_status,
            deployment_rollout,
            deployment_rollback,
            deployment_set_paused,
            deployment_remove,
            deployment_detail,
            deployment_put,
            ports_view,
            runtimes_view,
            placement_view,
            volumes_view,
            volume_create,
            volume_remove,
            shed_view,
            shed_set,
            registry_view,
            registry_service,
            dns_status,
            read_spec_file,
            schedules_view,
            schedule_detail,
            schedule_put,
            schedule_set_paused,
            schedule_remove,
            queue_view,
            node_revoke,
            node_addr_repair,
            invite_create,
            invites_list,
            invite_revoke,
            discover_candidates,
            lan_watch_start,
            lan_watch_stop,
            lan_watch_view,
            enrol_attach,
            pair_approve_start,
            pair_approve_decide,
            pair_code_view,
            doctor_run,
            doctor_fix,
            config_view,
            backup_write,
            drain,
            drain_timed,
            deep_link_take,
            deployment_validate,
            deployment_example,
            deployment_schema,
            deployment_scale,
            deployment_restart,
            deployment_instance,
            deployment_history,
            deployment_tag,
            deployment_promote,
            parallel_save_outputs,
            script_inspect,
            script_definitions_save,
            script_definitions_preview,
            service_save_run_defaults,
            snapshot_save,
        ])
        .build(tauri::generate_context!())
        .expect("error while starting Meshware Director")
        .run(|_app, event| {
            // A demo must not outlive the window (L1 P3). Its nodes already
            // carry `up --supervised`, so the kernel takes them whatever
            // happens here; this is what also removes the scratch
            // directory, so a person who tried the demo and quit is left
            // with a machine in exactly the state they found it.
            if matches!(event, tauri::RunEvent::Exit) {
                demo::stop_on_exit();
            }
        });
}

/// Take the `director://` link waiting to be acted on, if any (L2 P5).
///
/// Polled on the frontend's own tick rather than pushed as an event,
/// because everything else in this app is a poll over `invoke` and one
/// transport is easier to reason about than two. A second of latency on
/// "jump to the job somebody linked" is not a second anybody notices.
#[tauri::command]
async fn deep_link_take(
    state: State<'_, Director>,
) -> Result<Option<deeplink::DeepLinkView>, String> {
    Ok(state.deep_links.take())
}

/// One item's output, as the Run screen already holds it.
#[derive(Debug, serde::Deserialize)]
pub struct SaveOutput {
    /// The item that produced it — the name the saved file takes.
    pub item: String,
    /// `obj-…`
    pub id: String,
    /// The node that reported holding it, used as a fetch hint.
    pub holder: String,
}

/// Save a per-item run's outputs to a directory the person picked (R3 P1).
///
/// **This is the control whose absence made "keep output in the mesh" a
/// data-loss setting.** Director could show that a run had produced 412
/// objects and offer no way whatsoever to obtain them; the objects then
/// expired on the fifteen-minute clock. `meshnode map --out` could fetch
/// them and the GUI could not, which meant the documented advice for anyone
/// using Director was "do not keep your output in the mesh".
///
/// Naming is [`mesh_core::outputs::fetch_filename`] — the same function the
/// CLI uses, deliberately shared rather than reimplemented, so a file saved
/// here and a file saved by `--out` have the same name.
#[tauri::command]
async fn parallel_save_outputs(
    state: State<'_, Director>,
    // The folder the person picked (SEC-14).
    token: String,
    outputs: Vec<SaveOutput>,
    // `job`: the run these came from, so the origin can stop holding them
    // once they are safely here (R3 P3). Optional — a caller that does not
    // know simply leaves them to age out as they always did.
    job: Option<String>,
) -> Result<usize, String> {
    let engine = state.engine().await?;
    let root = state.picked.require(&token)?;
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;

    let mut taken = std::collections::HashSet::new();
    let mut saved = 0usize;
    for (ordinal, out) in outputs.iter().enumerate() {
        let id = mesh_proto::ObjectId::parse(&out.id).map_err(|e| format!("{}: {e}", out.item))?;
        let hints = match mesh_proto::NodeId::parse(&out.holder) {
            Ok(n) => vec![n],
            Err(_) => vec![],
        };
        let name = mesh_core::outputs::fetch_filename(ordinal, &out.item, &mut taken);
        let path = root.join(&name);

        let mut stream = engine
            .ipc_stream(IpcRequest::ObjectGet { id, hints })
            .await?;
        let mut bytes: Vec<u8> = Vec::new();
        let mut announced: Option<u64> = None;
        while let Some(frame) = stream.recv().await {
            match frame {
                IpcResponse::ObjectChunk { data } => bytes.extend_from_slice(&data),
                IpcResponse::ObjectEnd { size } => {
                    announced = Some(size);
                    break;
                }
                IpcResponse::Error { message } | IpcResponse::Denied { message } => {
                    return Err(format!("{}: {message}", out.item));
                }
                _ => {}
            }
        }
        // The same check the CLI grew for the same reason: a transfer that
        // stopped early used to be written out as a short file and called a
        // success. Half a video is worse than no video, because only one of
        // them tells you something went wrong.
        match announced {
            Some(size) if size == bytes.len() as u64 => {}
            Some(size) => {
                return Err(format!(
                    "{}: transfer incomplete — {} bytes arrived, {size} expected. Nothing was \
                     written for this item.",
                    out.item,
                    bytes.len()
                ));
            }
            None => return Err(format!("{}: the object stream ended early", out.item)),
        }
        std::fs::write(&path, &bytes)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        saved += 1;
    }
    // Best effort, after the bytes are on disk. Failing a completed save
    // because a bookkeeping call did not land would be the wrong trade.
    if let Some(job) = job
        .as_deref()
        .and_then(|j| mesh_proto::JobId::parse(j).ok())
    {
        let _ = engine.ipc(IpcRequest::ResultsCollected { job }).await;
    }
    Ok(saved)
}

/// Write a shared mesh snapshot to a file the user picked (L2 P2).
///
/// The PNG is composed in the frontend, from the live topology SVG: the
/// diagram already exists on screen, the totals strip is already the
/// sentence `mesh-proto` composed, and re-rendering either one here would
/// be a second drawing of the same mesh that could disagree with the first.
///
/// The payload arrives base64 rather than as a `Vec<u8>` because Tauri
/// serialises a byte vector as a JSON array of numbers — roughly four
/// bytes of transport per byte of image, for a file that is routinely
/// several hundred kilobytes.
#[tauri::command]
async fn snapshot_save(
    app: tauri::AppHandle,
    suggested: String,
    png_base64: String,
) -> Result<Option<String>, String> {
    let bytes = data_encoding::BASE64
        .decode(png_base64.as_bytes())
        .map_err(|e| format!("the snapshot could not be decoded: {e}"))?;
    // A blank export is the failure mode this phase was written around
    // (§2.4: `var(--…)` does not resolve in a detached SVG document), and
    // it is the one failure that looks like success — a file appears, and
    // nobody opens it until it is somewhere public. An 8-byte PNG header
    // with nothing behind it never reaches the disk.
    if bytes.len() < 1024 {
        return Err("the snapshot came out empty — nothing was written".into());
    }

    let Some(abs) = ask_where_to_save(
        &app,
        "Share this mesh",
        &suggested,
        Some(("PNG image", &["png"])),
    )
    .await?
    else {
        return Ok(None);
    };
    // As `bench_save_report` (DIR-10).
    let abs = filesave::with_default_extension(abs, Some("png"));
    let abs = filesave::write_replacing(&abs, &bytes)?;
    Ok(Some(abs.to_string_lossy().into_owned()))
}

// ---------------------------------------------------------------------------
// D-E3: the parity gate
// ---------------------------------------------------------------------------

/// **Every operator-facing capability ships a Director surface and a CLI
/// verb** — the owner's instruction of 2026-09-06, and cross-cutting
/// landmine 4 of `docs/N1-N6-THE-NOMAD-TRACK.md`.
///
/// It was a rule nobody could check, and it had already been broken: until
/// N1, Director could roll, pause and delete a deployment but not *make*
/// one, and its own empty state sent the reader to the terminal. Nothing
/// failed, because nothing was looking.
///
/// ## Why this is checked at the verb, and not at the wire
///
/// The obvious gate — *does Director send every `IpcRequest` variant the
/// CLI sends* — was written first and **did not catch the bug it was
/// written for**, which was verified by reintroducing the bug and watching
/// it pass. `IpcRequest::DeploymentPut` is what publishes a definition
/// *and* what a rollout, a rollback and a pause each send after
/// fetch-flip-re-sign, so Director sent it the whole time it could not
/// publish anything. A gate one level too coarse reads exactly like a
/// passing one.
///
/// So the unit here is the **verb**: each subcommand of `meshnode
/// deployment` and `meshnode schedule` names the Director command that
/// answers it, and the test asserts that command is both defined and
/// *registered* — an unregistered `#[tauri::command]` is unreachable, which
/// is the same gap wearing a different hat.
///
/// It proves reachability, not equivalence. Two front ends will always
/// present a capability differently — the CLI blocks on a redrawing table
/// where Director opens a panel that polls — and a test demanding identical
/// shapes would be wrong about both. What it refuses is a capability that
/// exists in one and cannot be reached at all from the other.
#[cfg(test)]
mod d_e3_parity {
    /// The CLI subcommand enums this gate governs.
    ///
    /// **A phase that adds a noun adds it here.** N2's ports, N3's registry
    /// and DNS, N4's containers and N5's volumes will each bring verbs, and
    /// a gate that silently did not cover them would be worse than no gate.
    const GOVERNED_ENUMS: &[&str] = &[
        "DeploymentCmd",
        "ScheduleCmd",
        "VolumeCmd",
        "ShedCmd",
        "HardwareCmd",
        "AttestRootCmd",
    ];

    /// CLI verb → the Director command that answers it.
    ///
    /// Two verbs may share one command where the capability genuinely is
    /// one (pause and resume are a boolean), and that is fine; what is not
    /// fine is a verb with nothing on the other side.
    const VERBS: &[(&str, &str, &str)] = &[
        ("DeploymentCmd", "Put", "deployment_put"),
        ("DeploymentCmd", "Ls", "deployments_view"),
        ("DeploymentCmd", "Show", "deployment_detail"),
        ("DeploymentCmd", "Status", "deployment_status"),
        ("DeploymentCmd", "Rollout", "deployment_rollout"),
        ("DeploymentCmd", "Rollback", "deployment_rollback"),
        ("DeploymentCmd", "Pause", "deployment_set_paused"),
        ("DeploymentCmd", "Resume", "deployment_set_paused"),
        ("DeploymentCmd", "Rm", "deployment_remove"),
        // Phase B, E4: the editor's inline check, *Deploy an example*, and
        // the schema copy button — the file verbs, in the idiom Director has.
        ("DeploymentCmd", "Validate", "deployment_validate"),
        ("DeploymentCmd", "Init", "deployment_example"),
        ("DeploymentCmd", "Schema", "deployment_schema"),
        // Phase B, E3 / U7: *Scale…*, *Restart…*, and a row's own *Restart*.
        ("DeploymentCmd", "Scale", "deployment_scale"),
        ("DeploymentCmd", "Restart", "deployment_restart"),
        // Phase B, E3: an instance row's *Logs*, streamed through the same
        // command Activity uses for any job — an instance *is* a job.
        ("DeploymentCmd", "Logs", "job_logs"),
        // Phase B, E3: an instance row's *Shell*, the terminal panel opened
        // with the instance's job — the same command a node's terminal uses.
        ("DeploymentCmd", "Exec", "terminal_open"),
        // Phase B, E3/U1: the instance panel — timeline, health, live use.
        ("DeploymentCmd", "Instance", "deployment_instance"),
        ("DeploymentCmd", "History", "deployment_history"),
        // A tag and its removal are one capability: a name, set or cleared.
        ("DeploymentCmd", "Tag", "deployment_tag"),
        ("DeploymentCmd", "Untag", "deployment_tag"),
        ("DeploymentCmd", "Promote", "deployment_promote"),
        ("DeploymentCmd", "Placement", "placement_view"),
        ("ScheduleCmd", "Put", "schedule_put"),
        ("ScheduleCmd", "Ls", "schedules_view"),
        ("ScheduleCmd", "Show", "schedule_detail"),
        ("ScheduleCmd", "Pause", "schedule_set_paused"),
        ("ScheduleCmd", "Resume", "schedule_set_paused"),
        ("ScheduleCmd", "Rm", "schedule_remove"),
        ("VolumeCmd", "Create", "volume_create"),
        ("VolumeCmd", "Rm", "volume_remove"),
        ("ShedCmd", "Status", "shed_view"),
        // Three verbs, one command: setting both switches *is* the
        // capability, and the verbs are three ways of naming a state to be
        // in. The gate allows that where the capability genuinely is one.
        ("ShedCmd", "Submissions", "shed_set"),
        ("ShedCmd", "Reconcile", "shed_set"),
        ("ShedCmd", "Clear", "shed_set"),
        // Y2/Y3. `ls` and `status` are one card: somebody asking where their
        // key is wants to see what they could move it to. `use --force` stays
        // in the terminal on purpose — binding a key the mesh cannot vouch
        // for is not a button.
        ("HardwareCmd", "Ls", "security_key_status"),
        ("HardwareCmd", "Status", "security_key_status"),
        ("HardwareCmd", "Use", "security_key_use"),
        ("HardwareCmd", "Attest", "security_key_attest"),
        ("HardwareCmd", "Unattest", "security_key_withdraw"),
        ("AttestRootCmd", "Ls", "attest_roots_view"),
        ("AttestRootCmd", "Add", "attest_root_add"),
        ("AttestRootCmd", "Rm", "attest_root_remove"),
    ];

    fn read(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    }

    /// The whole CLI surface as text.
    ///
    /// **Two files since the split.** `cli.rs` holds the subcommand enums
    /// and `main.rs` the `Command` variants and the arms that run them, so
    /// a gate reading either one alone reads half the CLI — and the half it
    /// misses passes silently, which is how this test spent a commit
    /// asserting nothing.
    fn cli_source() -> String {
        format!(
            "{}\n{}",
            read("crates/meshware-node/src/main.rs"),
            read("crates/meshware-node/src/cli.rs")
        )
    }

    /// The variant names of `enum <name>`, by brace matching from its
    /// opening `{` — so a variant holding a struct body is not mistaken for
    /// the end of the enum.
    fn variants_of(src: &str, name: &str) -> Vec<String> {
        let head = format!("enum {name} {{");
        let start = src
            .find(&head)
            .unwrap_or_else(|| panic!("no `{head}` in the CLI — has it been renamed?"))
            + head.len();
        let mut depth = 1usize;
        let mut end = start;
        for (i, c) in src[start..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = start + i;
                        break;
                    }
                }
                _ => {}
            }
        }
        src[start..end]
            .lines()
            // A variant is at the enum's own indentation; a field inside one
            // is deeper, and an attribute or doc comment is neither.
            .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
            .map(str::trim)
            .filter(|l| l.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
            .map(|l| {
                l.chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
            })
            .filter(|v| !v.is_empty())
            .collect()
    }

    #[test]
    fn every_cli_verb_names_a_director_command() {
        let cli = cli_source();
        for e in GOVERNED_ENUMS {
            let found = variants_of(&cli, e);
            assert!(
                !found.is_empty(),
                "no variants parsed out of `{e}` — the gate matches nothing, which \
                 passes everything"
            );
            for v in found {
                assert!(
                    VERBS.iter().any(|(en, vn, _)| en == e && *vn == v),
                    "D-E3: `meshnode {}` gained the verb `{v}`, and no Director command \
                     is named for it.\nAdd the Tauri command and the Svelte work, then \
                     the row to VERBS. A capability landing in one front end is a \
                     capability half the product does not have.",
                    e.trim_end_matches("Cmd").to_lowercase()
                );
            }
        }
    }

    /// Top-level verbs and flags the N-track adds, which the subcommand
    /// walk above cannot see.
    ///
    /// `meshnode ports` is a `Command` variant, not a `DeploymentCmd` one,
    /// and `--port` is a *flag* on `run` — neither lives in an enum this
    /// gate enumerates. Listed by hand because the alternative is parsing
    /// clap's whole `Command` enum to guess which variants are
    /// operator-facing, and a guess that guessed wrong would either nag
    /// about daemon-only verbs or quietly cover none of them.
    const EXTRA: &[(&str, &str)] = &[
        ("meshnode ports", "ports_view"),
        ("meshnode runtimes", "runtimes_view"),
        ("meshnode registry", "registry_view"),
        ("meshnode registry <service>", "registry_service"),
        ("meshnode run --register", "register_env"),
        ("meshnode registry --dns", "dns_status"),
        ("meshnode volumes", "volumes_view"),
        ("meshnode run --volume", "volume_env"),
        ("meshnode service create --command/--arg", "service_create"),
        // `--port` rides `run_command` and `run_service`; the gate checks
        // the flag reaches a Director command at all, and `port_env` is
        // where both of them turn it into the reserved key.
        ("meshnode run --port", "port_env"),
        // Phase C, E9/U9 — the node dialog's Drain.
        ("meshnode nodes drain", "node_drain_set"),
        // Y6 — a security key's co-signature on a published release.
        ("meshnode upgrade --cosign", "release_cosign"),
    ];

    #[test]
    fn every_extra_n_track_verb_reaches_director() {
        let dir = read("apps/director/src-tauri/src/lib.rs");
        let cli = cli_source();
        // The CLI really has them — otherwise this passes by describing a
        // product that does not exist.
        assert!(
            cli.contains("Command::Ports {"),
            "`meshnode ports` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("Command::Runtimes {"),
            "`meshnode runtimes` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("parse_port_request("),
            "`run --port` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("Command::Registry {"),
            "`meshnode registry` is gone from the CLI; drop the EXTRA rows too"
        );
        assert!(
            cli.contains("REQ_SERVICE.to_string()"),
            "`run --register` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("Command::Volumes {"),
            "`meshnode volumes` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("parse_volume_request("),
            "`run --volume` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("mesh_proto::command_env("),
            "`service create --command` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("NodesCmd::Drain {"),
            "`meshnode nodes drain` is gone from the CLI; drop the EXTRA row too"
        );
        assert!(
            cli.contains("publish_cosignature("),
            "`meshnode upgrade --cosign` is gone from the CLI; drop the EXTRA row too"
        );
        for (verb, item) in EXTRA {
            assert!(
                dir.contains(&format!("fn {item}(")),
                "D-E3: `{verb}` has no Director counterpart (`{item}`)"
            );
        }
    }

    #[test]
    fn every_named_director_command_is_defined_and_registered() {
        let lib = read("apps/director/src-tauri/src/lib.rs");
        // Commands that live in a module of their own are registered by
        // path (`security_key::security_key_use,`), and defined there.
        let dir = lib.clone() + &read("apps/director/src-tauri/src/security_key.rs");
        // The `generate_handler!` list is what makes a command reachable
        // from the webview. A command defined and not listed there is dead
        // code that type-checks — the same gap, wearing a different hat.
        let handlers_at = lib
            .find("generate_handler!")
            .expect("no generate_handler! in Director — has the bridge moved?");
        let handlers = &lib[handlers_at..];
        for (e, v, cmd) in VERBS {
            assert!(
                dir.contains(&format!("async fn {cmd}(")),
                "D-E3: `{e}::{v}` names Director command `{cmd}`, which does not exist"
            );
            assert!(
                handlers.contains(&format!("\n            {cmd},"))
                    || handlers.contains(&format!("::{cmd},\n")),
                "D-E3: Director command `{cmd}` exists but is not in `generate_handler!`, \
                 so the webview cannot call it — `{e}::{v}` is still CLI-only"
            );
        }
    }
}

#[cfg(test)]
mod build_run_tests {
    //! DIR-8: the Build tab's Run, against a real engine configured the way
    //! Director configures its own.
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::time::Duration;

    /// The PTY shim as a script, since a test harness is not a binary that
    /// answers to `__meshware-pty` (the same stand-in `fabric_m17` uses).
    fn stand_in_shim(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("shim.sh");
        std::fs::write(&path, "#!/bin/sh\nshift\nexec \"$@\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Everything a session prints, how it exited, and any refusal.
    async fn drain(mut stream: tokio::net::UnixStream) -> (String, Option<i32>, Option<String>) {
        let mut out = Vec::new();
        let read = async {
            loop {
                match mesh_proto::read_frame::<_, mesh_proto::SessionServerFrame>(&mut stream).await
                {
                    Ok(mesh_proto::SessionServerFrame::Data { bytes, .. }) => {
                        out.extend_from_slice(&bytes)
                    }
                    Ok(mesh_proto::SessionServerFrame::Exit { code, .. }) => return (code, None),
                    Ok(mesh_proto::SessionServerFrame::Error { message }) => {
                        return (None, Some(message));
                    }
                    Ok(_) => {}
                    Err(e) => return (None, Some(format!("stream ended: {e}"))),
                }
            }
        };
        let (code, err) = tokio::time::timeout(Duration::from_secs(30), read)
            .await
            .expect("the session ends");
        (String::from_utf8_lossy(&out).into_owned(), code, err)
    }

    /// Serve `node`'s socket the way the daemon does, and connect a client
    /// `Engine` to it: Director as it now is, a view onto a running node.
    pub(crate) async fn engine_for(node: &mesh_testkit::TestNode) -> (Engine, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("run").join("meshnode.sock");
        let (served, at) = (node.node.clone(), socket.clone());
        tokio::spawn(async move {
            let _ = mesh_core::ipc::serve(at, served).await;
        });
        for _ in 0..100 {
            if mesh_core::ipc::connect(&socket).await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let user = mesh_testkit::founder_user_key(node).clone();
        let engine = Engine::new(mesh_core::ipc::Client::new(socket, user.clone()), user);
        (engine, dir)
    }

    #[tokio::test]
    async fn run_build_works_where_a_terminal_on_this_machines_node_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let shim = stand_in_shim(dir.path());
        let founder = mesh_testkit::spawn_founder_with("nim", "build-mesh", vec![], |c| {
            // A node's defaults: no terminals accepted.
            c.sessions = Default::default();
        })
        .await;
        let (engine, _socket_dir) = engine_for(&founder).await;

        // What "Run build" used to do: a mesh terminal on this machine's
        // node. Refused, even for the mesh's founder.
        let kind = mesh_proto::SessionKind::Shell {
            command: Some("echo built".into()),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
        };
        let old = engine
            .open_session("nim", kind, "build farm: make")
            .await
            .unwrap();
        let (_, _, refused) = drain(old).await;
        let refused = refused.expect("a refusal");
        assert!(refused.contains("does not accept terminals"), "{refused}");

        // What it does now: the line, hosted here, with its output and its
        // exit status — listed in Director's own table meanwhile, and not in
        // the node's, since it is not a session of the mesh.
        let run = start_local_build_with(
            &engine,
            "printf 'built-%s\\n' ok; sleep 0.3; exit 3".into(),
            "build farm: make".into(),
            80,
            24,
            shim,
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        let listed = local_builds().rows();
        assert!(
            listed.iter().any(|r| r.reason == "build farm: make"),
            "{listed:?}"
        );
        assert!(
            !founder
                .node
                .sessions()
                .rows()
                .iter()
                .any(|r| r.reason == "build farm: make"),
            "a local build is not the node's session"
        );
        let (out, code, err) = drain(run).await;
        assert_eq!(err, None, "output so far: {out}");
        assert!(out.contains("built-ok"), "{out}");
        assert_eq!(code, Some(3));

        founder.stop().await;
    }
}
