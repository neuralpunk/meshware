//! Security keys: Director's half of Y2/Y3 (rule D-E3).
//!
//! `meshnode identity hardware` moves a person's identity onto a token and
//! publishes the proof that it is there; `meshnode users attest-root` says
//! whose proofs the mesh believes. This is the same, for the window.
//!
//! ## Nothing here is a second implementation
//!
//! What may be bound, and what happens to the key it replaces, is decided
//! by `mesh_core::hardware::check_bind` and done by `bind_file`: the same
//! two functions the CLI calls, so a refusal in a terminal is a refusal
//! here. What Director adds is the step the CLI prints and leaves to the
//! person — letting the key into the mesh first — and it decides that step
//! **before** doing anything: rotating an account onto a key and *then*
//! finding the binding refused would leave the old key revoked and this
//! machine signing with it.
//!
//! ## What 2026-09-18 changed
//!
//! Somebody moved onto "their YubiKey" here, and it was `~/.ssh/meshware`:
//! GNOME's agent offers every key file in `~/.ssh`, the card could not tell
//! them apart, and afterwards it said "on a security key". Then the agent
//! asked for that file's passphrase once per request, every refresh, until
//! they had dismissed about thirty prompts. So:
//!
//! - every key is named for where it is, as far as this machine can tell
//!   ([`hw::KeyOrigin`]), and a key that is a file in `~/.ssh` cannot be
//!   chosen here;
//! - a key signs once, twice, before anything changes
//!   ([`security_key_test`]), which is how a key that wants its PIN for
//!   every signature is caught before it is bound;
//! - signing goes through [`crate::engine::KeyGate`] — one at a time, and
//!   the first refusal stops the asking until the person says try again;
//! - a YubiKey can be connected with its PIN ([`security_key_connect`]),
//!   and a binding can be undone ([`security_key_restore`]).
//!
//! ## Why the agent calls run on the blocking pool
//!
//! The agent is a Unix socket spoken to synchronously, and a key that is
//! waiting for a PIN or a touch holds the reply. That is a thread that must
//! not be one the window is waiting on.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::State;
use ts_rs::TS;
use zeroize::Zeroizing;

use mesh_core::hardware::{self as hw, BindRefusal, HardwareKey, KeyOrigin, OldKey};
use mesh_core::identity::{Keypair, default_user_identity_path};
use mesh_core::keyfile::Stored;
use mesh_proto::{IpcRequest, IpcResponse, Role, UserId, UserRecord, UserRow, now_micros};

use crate::engine::{Director, Engine};

// ---------------------------------------------------------------------------
// views
// ---------------------------------------------------------------------------

/// Where this account's key is, and what the agent is holding.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SecurityKeyStatusView {
    /// The key file this account signs with.
    pub path: String,
    /// What the file holds: `hardware` (it only names a key an agent
    /// holds), `software`, `sealed` (software, under a passphrase) or
    /// `absent`.
    pub held: String,
    /// For `hardware`: where that key actually is — `file`, `device` or
    /// `unknown` ([`hw::KeyOrigin`]). A key an agent holds is *not* on a
    /// security key just because an agent holds it.
    pub origin: Option<String>,
    /// For `origin == "file"`: the key file, written from `~`.
    pub file: Option<String>,
    /// The identity the file names, when that can be read without opening
    /// anything. `None` for a sealed key: its public half is inside the
    /// seal.
    pub identity: Option<String>,
    /// The agent socket that was asked.
    pub agent: Option<String>,
    /// Why no agent could be asked, or why it would not answer.
    pub agent_error: Option<String>,
    /// The Ed25519 keys the agent is holding right now.
    pub keys: Vec<AgentKeyView>,
    /// For a `hardware` key: the agent is holding it, so signing can work.
    pub reachable: bool,
    /// This machine has a PIV smart-card module, so a YubiKey can be
    /// connected with its PIN ([`security_key_connect`]).
    pub can_connect: bool,
    /// Keys a binding set aside, which [`security_key_restore`] can put
    /// back.
    pub set_aside: Vec<SetAsideView>,
    /// Other files beside the key file whose names say they may hold a key
    /// — a copy an older Director made, say. Names only: nothing here opens
    /// them.
    pub beside: Vec<String>,
    /// Signing is paused after the key refused (see `KeyGate`), and why.
    pub paused: Option<String>,
}

/// One key an agent is holding.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AgentKeyView {
    pub user_id: String,
    pub short_id: String,
    /// What the agent calls it — a label, a card serial, a file's comment.
    pub comment: String,
    /// It is the key this account already signs with.
    pub current: bool,
    /// `file`, `device` or `unknown` ([`hw::KeyOrigin`]).
    pub origin: String,
    /// For a `file` key: which file, written from `~`.
    pub file: Option<String>,
}

/// A key a binding set aside beside the key file.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SetAsideView {
    /// The file's name, which is what [`security_key_restore`] takes.
    pub name: String,
    /// The short id its name carries — whose key it is.
    pub short_id: String,
}

/// What moving onto a key would take, decided before anything is done.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SecurityKeyPlanView {
    pub key: String,
    pub comment: String,
    /// `same` — already in use; `bind` — the mesh knows the key, so only
    /// this computer changes; `add-admin` — the founder's key joins as a
    /// second admin first; `rotate` — this admin's account moves to the key
    /// first; `ask-admin` — somebody else has to let the key in;
    /// `sole-admin` — refused, the mesh would have one admin and it would
    /// be on a token; `file` — refused, the key is a file on this computer.
    pub plan: String,
    /// This account's name in the mesh, when it has one.
    pub username: String,
    /// For `add-admin`: the name the key joins under.
    pub new_username: Option<String>,
    /// What happens to the key the file holds now: `same`, `spent` (the
    /// mesh no longer honours it, so it is replaced) or `live` (it is
    /// moved aside, never overwritten).
    pub old_key: Option<String>,
    /// For the plans that stop: what to do instead, in plain words.
    pub message: String,
}

/// What asking a key to sign, twice, showed.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SecurityKeyTestView {
    /// It signed, and the signature checks out.
    pub ok: bool,
    /// It needed the person for the second signature too — a PIN or a
    /// touch every time — which Director, signing every few seconds,
    /// cannot live with.
    pub asks_every_time: bool,
    /// What happened, in plain words.
    pub message: String,
}

/// A binding, done.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct SecurityKeyBoundView {
    pub identity: String,
    /// Where the previous key went, when it was moved rather than replaced.
    pub moved_to: Option<String>,
}

/// A certificate authority the mesh believes about hardware.
#[derive(Debug, Clone, Serialize, TS)]
#[serde(rename_all = "camelCase")]
pub struct AttestRootView {
    pub id: String,
    pub short_id: String,
    pub label: String,
    pub added_by: String,
    #[ts(type = "number")]
    pub added_at_micros: u64,
    pub removed: bool,
    /// How many people's proofs it vouches for now.
    pub attesting: u32,
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

/// `meshnode identity hardware status` and `ls`, together: a person asking
/// where their key is wants to see what they could move it to.
#[tauri::command]
pub async fn security_key_status(
    state: State<'_, Director>,
) -> Result<SecurityKeyStatusView, String> {
    let paused = state
        .inner
        .lock()
        .await
        .engine
        .as_ref()
        .and_then(|e| e.key.paused());
    let mut view =
        tauri::async_runtime::spawn_blocking(|| status_of(&default_user_identity_path()))
            .await
            .map_err(|e| e.to_string())?;
    view.paused = paused;
    Ok(view)
}

fn status_of(path: &Path) -> SecurityKeyStatusView {
    let (held, public) = match Stored::read(path) {
        Ok(Stored::Hardware(public)) => ("hardware", Some(public)),
        Ok(Stored::Plain(seed)) => ("software", Some(Keypair::from_seed(seed).public_bytes())),
        Ok(Stored::Sealed(_)) => ("sealed", None),
        Err(_) => ("absent", None),
    };
    let (agent, agent_error, keys) = match hw::agent_socket(None) {
        Err(e) => (None, Some(e.to_string()), Vec::new()),
        Ok(socket) => match hw::list(&socket) {
            Ok(keys) => (Some(socket), None, keys),
            Err(e) => (Some(socket), Some(e.to_string()), Vec::new()),
        },
    };
    let keys: Vec<AgentKeyView> = keys
        .iter()
        .map(|k| {
            let (origin, file) = origin_words(&k.origin());
            AgentKeyView {
                user_id: k.user_id().to_display_string(),
                short_id: k.user_id().short(),
                comment: k.comment().to_string(),
                current: public == Some(k.public_bytes()),
                origin,
                file,
            }
        })
        .collect();
    let current = keys.iter().find(|k| k.current);
    let (origin, file) = match (held, current) {
        (_, Some(k)) if held == "hardware" => (Some(k.origin.clone()), k.file.clone()),
        // Bound, and the agent is not holding it: where it is cannot be
        // said, except that a `.pub` for it may still be in ~/.ssh.
        ("hardware", None) => {
            let o = public.map(|p| hw::origin_in(ssh_dir().as_deref(), &p, ""));
            match o {
                Some(o) => {
                    let (origin, file) = origin_words(&o);
                    (Some(origin), file)
                }
                None => (None, None),
            }
        }
        _ => (None, None),
    };
    let set_aside: Vec<SetAsideView> = hw::set_aside_keys(path)
        .into_iter()
        .map(|(p, short_id)| SetAsideView {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            short_id,
        })
        .collect();
    SecurityKeyStatusView {
        path: path.display().to_string(),
        held: held.into(),
        origin,
        file,
        identity: public.map(|p| UserId(p).to_display_string()),
        agent: agent.map(|s| s.display().to_string()),
        agent_error,
        reachable: held == "hardware" && current.is_some(),
        keys,
        can_connect: hw::pkcs11_module().is_some(),
        beside: beside(path)
            .into_iter()
            .filter(|n| !set_aside.iter().any(|s| &s.name == n))
            .collect(),
        set_aside,
        paused: None,
    }
}

fn ssh_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".ssh"))
}

/// `file`/`device`/`unknown`, and the file written from `~`.
fn origin_words(origin: &KeyOrigin) -> (String, Option<String>) {
    match origin {
        KeyOrigin::File(f) => ("file".into(), Some(tilde(f))),
        KeyOrigin::Device => ("device".into(), None),
        KeyOrigin::Unknown => ("unknown".into(), None),
    }
}

fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from) {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(&home) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}

/// `user.key.*` beside `user.key`, by name.
fn beside(path: &Path) -> Vec<String> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{}.", name.to_string_lossy());
    let mut found: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(&prefix))
        .collect();
    found.sort();
    found
}

// ---------------------------------------------------------------------------
// connecting, testing, pausing
// ---------------------------------------------------------------------------

/// Connect a YubiKey (any PIV smart card) to the key agent with its PIN —
/// what `ssh-add -s` does, without a terminal.
///
/// One attempt per click, never retried: a PIV applet locks its PIN after
/// three wrong ones.
#[tauri::command]
pub async fn security_key_connect(
    state: State<'_, Director>,
    pin: String,
) -> Result<SecurityKeyStatusView, String> {
    let pin = Zeroizing::new(pin);
    if pin.trim().is_empty() {
        return Err("Type your YubiKey's PIN first.".into());
    }
    let Some(module) = hw::pkcs11_module() else {
        return Err(
            "Director can't talk to smart cards on this computer yet. Install OpenSC (the \
             `opensc` package) and try again."
                .into(),
        );
    };
    let socket = hw::agent_socket(None).map_err(|_| {
        "Director can't find your computer's key agent. Log out and back in, then try again."
            .to_string()
    })?;
    let engine = state.inner.lock().await.engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let connect = || hw::connect_smartcard(&socket, &module, pin.trim());
        match &engine {
            Some(e) => e.key.alone(connect),
            None => connect(),
        }
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|_| {
        "That didn't work. Check that your YubiKey is plugged in and the PIN is right.".to_string()
    })?;
    if let Some(engine) = state.inner.lock().await.engine.as_ref() {
        engine.key.resume();
    }
    security_key_status(state).await
}

/// Ask a key to sign, twice, before anything changes: once to show it
/// can, and again to show it will not ask the person every time.
#[tauri::command]
pub async fn security_key_test(
    state: State<'_, Director>,
    key: String,
) -> Result<SecurityKeyTestView, String> {
    let socket = hw::agent_socket(None).map_err(|e| e.to_string())?;
    let chosen = agent_key(&socket, &key).await?;
    if let KeyOrigin::File(file) = chosen.origin() {
        return Ok(SecurityKeyTestView {
            ok: false,
            asks_every_time: false,
            message: file_message(&file),
        });
    }
    let engine = state.inner.lock().await.engine.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let run = || test_signatures(&chosen);
        match &engine {
            Some(e) => e.key.alone(run),
            None => run(),
        }
    })
    .await
    .map_err(|e| e.to_string())
}

/// A second signature slower than this was waiting for a person.
const ASKED_AGAIN: Duration = Duration::from_millis(2_000);

fn test_signatures(key: &HardwareKey) -> SecurityKeyTestView {
    let msg = b"meshware: checking this key can sign";
    let first = key.sign(msg);
    let verified = first
        .as_ref()
        .is_ok_and(|sig| mesh_core::identity::verify_signature(&key.public_bytes(), msg, sig));
    if !verified {
        return SecurityKeyTestView {
            ok: false,
            asks_every_time: false,
            message: "Your key didn't sign. The PIN or touch may have been cancelled, or the key \
                      isn't connected. Nothing has changed."
                .into(),
        };
    }
    let started = Instant::now();
    let second = key.sign(msg);
    let asks_every_time = second.is_err() || started.elapsed() > ASKED_AGAIN;
    SecurityKeyTestView {
        ok: !asks_every_time,
        asks_every_time,
        message: if asks_every_time {
            "Your key asked for you a second time. Director checks in with your mesh every few \
             seconds, so it would keep asking you. A key that asks for its PIN once per session \
             works well — the commands below make one."
                .into()
        } else {
            "Your key signed, and it's ready to use.".into()
        },
    }
}

/// Start asking the key again, after it refused and Director stopped.
#[tauri::command]
pub async fn security_key_resume(state: State<'_, Director>) -> Result<(), String> {
    if let Some(engine) = state.inner.lock().await.engine.as_ref() {
        engine.key.resume();
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// the plan
// ---------------------------------------------------------------------------

/// What moving onto one of the agent's keys would take. Nothing is written.
#[tauri::command]
pub async fn security_key_plan(
    state: State<'_, Director>,
    key: String,
) -> Result<SecurityKeyPlanView, String> {
    let engine = state.engine().await?;
    let socket = hw::agent_socket(None).map_err(|e| e.to_string())?;
    plan_key(&engine, &default_user_identity_path(), &socket, &key).await
}

async fn plan_key(
    engine: &Engine,
    path: &Path,
    socket: &Path,
    key: &str,
) -> Result<SecurityKeyPlanView, String> {
    let chosen = agent_key(socket, key).await?;
    if let KeyOrigin::File(file) = chosen.origin() {
        return Ok(Plan::File(file).view(&chosen));
    }
    let current = current_identity(path).await?;
    let users = users_for(engine, &chosen).await?;
    Ok(decide(current, chosen.user_id(), &users).view(&chosen))
}

/// Move this account onto one of the agent's keys, letting the key into the
/// mesh first where the plan says that is the way across.
///
/// The plan is worked out again here rather than trusted from the window:
/// the mesh may have changed since it was shown, and a step that was safe
/// then is not necessarily safe now.
#[tauri::command]
pub async fn security_key_use(
    state: State<'_, Director>,
    key: String,
) -> Result<SecurityKeyBoundView, String> {
    let engine = state.engine().await?;
    let socket = hw::agent_socket(None).map_err(|e| e.to_string())?;
    let (bound, rebound) = use_key(&engine, &default_user_identity_path(), &socket, &key).await?;
    // From here this window signs as the key it just bound. The node is the
    // same node; only who is talking to it changed.
    state.inner.lock().await.engine = Some(rebound);
    Ok(bound)
}

/// [`security_key_use`] with its inputs named: the window's engine, the key
/// file, and the agent. Returns the binding and the engine that signs as
/// the bound key.
async fn use_key(
    engine: &Engine,
    path: &Path,
    socket: &Path,
    key: &str,
) -> Result<(SecurityKeyBoundView, Engine), String> {
    let chosen = agent_key(socket, key).await?;
    if let KeyOrigin::File(file) = chosen.origin() {
        return Err(file_message(&file));
    }
    let current = current_identity(path).await?;
    let users = users_for(engine, &chosen).await?;

    let old = match decide(current, chosen.user_id(), &users) {
        Plan::Same => OldKey::Same,
        Plan::Bind(old) => old,
        Plan::AddAdmin { new_username } => {
            add_admin(engine, &new_username, chosen.user_id()).await?;
            OldKey::Live
        }
        Plan::Rotate { username } => {
            crate::rotate_key(
                engine,
                username,
                chosen.user_id(),
                "moved to a security key".into(),
            )
            .await?;
            OldKey::Spent
        }
        stop @ (Plan::AskAdmin { .. } | Plan::SoleAdmin | Plan::File(_)) => {
            return Err(stop.view(&chosen).message);
        }
    };

    let public = chosen.public_bytes();
    let moved = tauri::async_runtime::spawn_blocking({
        let path = path.to_path_buf();
        move || hw::bind_file(&path, &public, old, current)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    let rebound = engine.as_user(Keypair::from_hardware(chosen.clone()));
    Ok((
        SecurityKeyBoundView {
            identity: chosen.user_id().to_display_string(),
            moved_to: moved.map(|p| p.display().to_string()),
        },
        rebound,
    ))
}

/// Go back to a key a binding set aside: it returns to the key file, and
/// this window reconnects as it. A sealed key comes back sealed, so the
/// window asks for its passphrase next.
///
/// Only over a reference to a key an agent holds (`hw::restore_file`): a
/// key file that holds a secret is never overwritten.
#[tauri::command]
pub async fn security_key_restore(state: State<'_, Director>, name: String) -> Result<(), String> {
    let path = default_user_identity_path();
    let set_aside = hw::set_aside_keys(&path)
        .into_iter()
        .map(|(p, _)| p)
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy() == name))
        .ok_or_else(|| "Director can't find that key any more.".to_string())?;
    tauri::async_runtime::spawn_blocking(move || hw::restore_file(&path, &set_aside))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    // The next start connects as the restored key, unlocking it first if
    // it is sealed.
    state.inner.lock().await.engine = None;
    Ok(())
}

enum Plan {
    Same,
    Bind(OldKey),
    AddAdmin { new_username: String },
    Rotate { username: String },
    AskAdmin { username: String },
    SoleAdmin,
    File(PathBuf),
}

/// What choosing a key that is a file on this computer is told.
fn file_message(file: &Path) -> String {
    format!(
        "That's {}, a key file on this computer — not a key on your security key. Moving to it \
         wouldn't protect anything.",
        tilde(file)
    )
}

/// Decide, from the mesh's own rows, what binding `chosen_id` takes.
///
/// The steps that let a key in are **simulated** and checked again before
/// they are offered: the answer is the whole path's, not its first step's.
fn decide(current: Option<UserId>, chosen_id: UserId, users: &[UserRow]) -> Plan {
    let probe = current.unwrap_or(chosen_id);
    match hw::check_bind(probe, chosen_id, users) {
        Ok(OldKey::Same) => Plan::Same,
        Ok(old) => Plan::Bind(old),
        Err(BindRefusal::SoleAdmin) => Plan::SoleAdmin,
        Err(BindRefusal::NotAMember {
            username,
            founder: true,
            ..
        }) => {
            let new_username = free_name(users, &format!("{username}-key"));
            let mut after = users.to_vec();
            after.push(row(chosen_id, &new_username, vec!["admin".into()]));
            match hw::check_bind(probe, chosen_id, &after) {
                Ok(_) => Plan::AddAdmin { new_username },
                Err(_) => Plan::SoleAdmin,
            }
        }
        Err(BindRefusal::NotAMember {
            username,
            admin: true,
            ..
        }) => {
            let mut after = users.to_vec();
            let mut roles = Vec::new();
            for u in &mut after {
                if u.user_id == probe {
                    u.revoked = true;
                    roles = u.roles.clone();
                }
            }
            after.push(row(chosen_id, &username, roles));
            match hw::check_bind(probe, chosen_id, &after) {
                Ok(_) => Plan::Rotate { username },
                Err(_) => Plan::SoleAdmin,
            }
        }
        Err(BindRefusal::NotAMember { username, .. }) => Plan::AskAdmin { username },
    }
}

impl Plan {
    fn view(&self, chosen: &HardwareKey) -> SecurityKeyPlanView {
        let mut v = SecurityKeyPlanView {
            key: chosen.user_id().to_display_string(),
            comment: chosen.comment().to_string(),
            plan: String::new(),
            username: String::new(),
            new_username: None,
            old_key: None,
            message: String::new(),
        };
        let old_word = |old: &OldKey| match old {
            OldKey::Same => "same",
            OldKey::Spent => "spent",
            OldKey::Live => "live",
        };
        match self {
            Plan::Same => {
                v.plan = "same".into();
                v.old_key = Some("same".into());
            }
            Plan::Bind(old) => {
                v.plan = "bind".into();
                v.old_key = Some(old_word(old).into());
            }
            Plan::AddAdmin { new_username } => {
                v.plan = "add-admin".into();
                v.new_username = Some(new_username.clone());
                v.old_key = Some("live".into());
            }
            Plan::Rotate { username } => {
                v.plan = "rotate".into();
                v.username = username.clone();
                v.old_key = Some("spent".into());
            }
            Plan::AskAdmin { username } => {
                v.plan = "ask-admin".into();
                v.username = username.clone();
                v.message = "Only an admin can add a new key to your mesh. Ask one to open \
                             Access › People, find you, and choose Replace key… with the key \
                             shown here. Then come back and finish."
                    .into();
            }
            Plan::SoleAdmin => {
                v.plan = "sole-admin".into();
                v.message = "You're the only admin in this mesh, so if this security key were \
                             ever lost, nobody could get back in. Make someone else an admin \
                             first (Access › Grant a role), then come back."
                    .into();
            }
            Plan::File(file) => {
                v.plan = "file".into();
                v.message = file_message(file);
            }
        }
        v
    }
}

/// `wanted`, or `wanted-2`, `wanted-3`… — the first name no live user has.
fn free_name(users: &[UserRow], wanted: &str) -> String {
    let taken = |n: &str| users.iter().any(|u| !u.revoked && u.username == n);
    if !taken(wanted) {
        return wanted.to_string();
    }
    (2..)
        .map(|i| format!("{wanted}-{i}"))
        .find(|n| !taken(n))
        .unwrap_or_else(|| wanted.to_string())
}

fn row(user_id: UserId, username: &str, roles: Vec<String>) -> UserRow {
    UserRow {
        user_id,
        username: username.to_string(),
        is_founder: false,
        revoked: false,
        roles,
        created_at_micros: now_micros(),
        hardware: None,
    }
}

/// The founder's way across: the key joins as a second admin, everywhere,
/// and the founding key stays the way back in.
async fn add_admin(engine: &Engine, username: &str, key: UserId) -> Result<(), String> {
    let mut record = UserRecord {
        user_id: key,
        username: username.to_string(),
        created_at_micros: now_micros(),
        added_by: engine.user.user_id(),
        signature: Vec::new(),
    };
    record.signature = engine.sign(&record.signing_bytes())?;
    engine.ipc(IpcRequest::UserAdd { record }).await?;
    let grant = crate::build_grant(engine, key, Role::Admin, "any".into(), "*".into(), None)?;
    engine.ipc(IpcRequest::GrantAdd { grant }).await?;
    Ok(())
}

/// One of the agent's keys, by a `mwu1…` id or a prefix of one.
async fn agent_key(socket: &Path, key: &str) -> Result<HardwareKey, String> {
    let wanted = key.trim().to_ascii_lowercase();
    let socket = socket.to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let keys = hw::list(&socket).map_err(|e| e.to_string())?;
        let mut matches = keys.into_iter().filter(|k| {
            k.user_id()
                .to_display_string()
                .to_ascii_lowercase()
                .starts_with(&wanted)
        });
        match (matches.next(), matches.next()) {
            (Some(one), None) => Ok(one),
            (None, _) => Err(
                "Director can't see that key any more. Is your security key still plugged in?"
                    .into(),
            ),
            (Some(_), Some(_)) => Err("That matches more than one key.".into()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Whose key the file holds now — what binding would replace.
async fn current_identity(path: &Path) -> Result<Option<UserId>, String> {
    let path: PathBuf = path.to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        hw::identity_in_file(&path).map_err(|_| {
            "Director couldn't read your current key. If it's protected with a passphrase, \
             unlock it first."
                .to_string()
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// The mesh's user rows, asked as this window's key and then, if that key
/// is no longer honoured, as the chosen one: after a rotation the old key
/// is refused and the new one is a member. Only the second ask can cost a
/// PIN or a touch, so it goes second.
async fn users_for(engine: &Engine, chosen: &HardwareKey) -> Result<Vec<UserRow>, String> {
    if let Ok(IpcResponse::Users(rows)) = engine.ipc(IpcRequest::Users).await {
        return Ok(rows);
    }
    let asking = engine.as_user(Keypair::from_hardware(chosen.clone()));
    match asking.ipc(IpcRequest::Users).await {
        Ok(IpcResponse::Users(rows)) => Ok(rows),
        _ => Err(
            "Director couldn't reach your mesh to check that key, so nothing was changed. Try \
             again in a moment."
                .into(),
        ),
    }
}

// ---------------------------------------------------------------------------
// proof
// ---------------------------------------------------------------------------

/// Publish the certificate chain that proves this account's key was made on
/// a device and cannot leave it. `certs` are picked files, leaf first.
#[tauri::command]
pub async fn security_key_attest(
    state: State<'_, Director>,
    certs: Vec<String>,
    kind: String,
    slot: String,
) -> Result<(), String> {
    let kind = mesh_proto::AttestKind::parse(&kind)
        .ok_or_else(|| format!("unknown kind {kind:?} (piv, openpgp or fido2)"))?;
    if certs.is_empty() {
        return Err("Choose the certificate your security key made for this key first.".into());
    }
    let paths = certs
        .iter()
        .map(|t| state.picked.require(t))
        .collect::<Result<Vec<_>, _>>()?;
    let chain = tauri::async_runtime::spawn_blocking(move || {
        paths
            .iter()
            .map(|p| read_der(p))
            .collect::<Result<Vec<_>, _>>()
    })
    .await
    .map_err(|e| e.to_string())??;

    let engine = state.engine().await?;
    let mut attestation = mesh_proto::HardwareAttestation {
        user_id: engine.user.user_id(),
        kind,
        chain,
        slot: if slot.trim().is_empty() {
            "9a".into()
        } else {
            slot.trim().to_string()
        },
        added_at_micros: now_micros(),
        signature: Vec::new(),
    };
    if !attestation.well_formed() {
        return Err(format!(
            "Those files aren't a certificate chain the mesh can store: it takes 1 to {} \
             certificates, each under {} KiB.",
            mesh_proto::attest::MAX_CHAIN_CERTS,
            mesh_proto::attest::MAX_CERT_BYTES / 1024
        ));
    }
    attestation.signature = engine.sign(&attestation.signing_bytes())?;
    engine
        .ipc(IpcRequest::UserHardwareAttest {
            attestation: Box::new(attestation),
        })
        .await
        .map(|_| ())
}

/// Withdraw a proof: this account's own, or — as an admin — somebody
/// else's, named by their `mwu1…` id.
#[tauri::command]
pub async fn security_key_withdraw(
    state: State<'_, Director>,
    user: Option<String>,
    reason: String,
) -> Result<(), String> {
    let engine = state.engine().await?;
    let owner = match user.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        None => engine.user.user_id(),
        Some(id) => UserId::parse(id).map_err(|e| e.to_string())?,
    };
    let mut revocation = mesh_proto::AttestRevocation {
        user_id: owner,
        reason,
        revoked_by: engine.user.user_id(),
        at_micros: now_micros(),
        signature: Vec::new(),
    };
    revocation.signature = engine.sign(&revocation.signing_bytes())?;
    engine
        .ipc(IpcRequest::UserHardwareRevoke {
            revocation: Box::new(revocation),
        })
        .await
        .map(|_| ())
}

/// A DER certificate from a picked file. PEM is refused with the command
/// that converts it, rather than decoded here: the CLI takes DER, and two
/// front ends accepting different files would be two products.
fn read_der(path: &Path) -> Result<Vec<u8>, String> {
    let der = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    if der.starts_with(b"-----BEGIN") {
        let name = path.file_name().map_or_else(
            || path.display().to_string(),
            |n| n.to_string_lossy().into(),
        );
        return Err(format!(
            "{name} is in PEM format, and this needs DER. Convert it with: openssl x509 -in \
             {name} -outform der -out cert.der"
        ));
    }
    Ok(der)
}

// ---------------------------------------------------------------------------
// the authorities the mesh believes
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn attest_roots_view(state: State<'_, Director>) -> Result<Vec<AttestRootView>, String> {
    match state.engine().await?.ipc(IpcRequest::AttestRoots).await? {
        IpcResponse::AttestRoots(rows) => Ok(rows
            .into_iter()
            .map(|r| AttestRootView {
                short_id: r.id.chars().take(12).collect(),
                id: r.id,
                label: r.label,
                added_by: if r.added_by_username.is_empty() {
                    r.added_by.short()
                } else {
                    r.added_by_username
                },
                added_at_micros: r.added_at_micros,
                removed: r.removed,
                attesting: r.attesting,
            })
            .collect()),
        other => Err(format!("unexpected response: {other:?}")),
    }
}

/// Trust a certificate authority's word about hardware (admin).
#[tauri::command]
pub async fn attest_root_add(
    state: State<'_, Director>,
    cert: String,
    label: String,
) -> Result<(), String> {
    let path = state.picked.require(&cert)?;
    let certificate = {
        let path = path.clone();
        tauri::async_runtime::spawn_blocking(move || read_der(&path))
            .await
            .map_err(|e| e.to_string())??
    };
    // A label is what everybody reads this by, so an empty one falls back
    // to the file's name — never to the id, which is a digest.
    let label = match label.trim() {
        "" => path.file_stem().map_or_else(
            || "attestation authority".into(),
            |f| f.to_string_lossy().into(),
        ),
        l => l.to_string(),
    };
    let engine = state.engine().await?;
    let mut root = mesh_proto::AttestRoot {
        label,
        certificate,
        added_by: engine.user.user_id(),
        added_at_micros: now_micros(),
        signature: Vec::new(),
    };
    root.signature = engine.sign(&root.signing_bytes())?;
    engine
        .ipc(IpcRequest::AttestRootAdd {
            root: Box::new(root),
        })
        .await
        .map(|_| ())
}

/// Stop trusting one (admin). Proofs it vouched for stop verifying.
#[tauri::command]
pub async fn attest_root_remove(state: State<'_, Director>, id: String) -> Result<(), String> {
    let engine = state.engine().await?;
    let at_micros = now_micros();
    let by = engine.user.user_id();
    let signature = engine.sign(&mesh_proto::attest_root_remove_signing_bytes(
        &id, &by, at_micros,
    ))?;
    engine
        .ipc(IpcRequest::AttestRootRemove {
            id,
            by,
            at_micros,
            signature,
        })
        .await
        .map(|_| ())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn user(id: u8, name: &str, roles: &[&str], founder: bool) -> UserRow {
        UserRow {
            user_id: UserId([id; 32]),
            username: name.into(),
            is_founder: founder,
            revoked: false,
            roles: roles.iter().map(|r| (*r).to_string()).collect(),
            created_at_micros: 0,
            hardware: None,
        }
    }

    fn plan_for(current: u8, chosen: u8, users: &[UserRow]) -> Plan {
        decide(Some(UserId([current; 32])), UserId([chosen; 32]), users)
    }

    #[test]
    fn the_founder_crosses_by_adding_a_second_admin() {
        let users = [user(1, "neuralpunk", &["admin"], true)];
        match plan_for(1, 9, &users) {
            Plan::AddAdmin { new_username } => assert_eq!(new_username, "neuralpunk-key"),
            _ => panic!("the founder's way across is a second admin"),
        }
    }

    #[test]
    fn a_taken_name_is_not_reused() {
        let users = [
            user(1, "neuralpunk", &["admin"], true),
            user(2, "neuralpunk-key", &["viewer"], false),
        ];
        match plan_for(1, 9, &users) {
            Plan::AddAdmin { new_username } => assert_eq!(new_username, "neuralpunk-key-2"),
            _ => panic!("expected add-admin"),
        }
    }

    #[test]
    fn an_admin_rotates_when_another_admin_remains() {
        let users = [
            user(1, "neuralpunk", &["admin"], true),
            user(2, "mia", &["admin"], false),
        ];
        assert!(matches!(plan_for(2, 9, &users), Plan::Rotate { username } if username == "mia"));
    }

    #[test]
    fn a_rotation_that_would_leave_one_admin_on_a_token_is_refused_up_front() {
        // Rotating first and finding out after would leave mia's old key
        // revoked and this machine signing with it.
        let users = [
            user(1, "neuralpunk", &["viewer"], true),
            user(2, "mia", &["admin"], false),
        ];
        assert!(matches!(plan_for(2, 9, &users), Plan::SoleAdmin));
    }

    #[test]
    fn someone_who_is_not_an_admin_is_sent_to_one() {
        let users = [
            user(1, "neuralpunk", &["admin"], true),
            user(3, "ada", &["operator"], false),
        ];
        assert!(matches!(plan_for(3, 9, &users), Plan::AskAdmin { username } if username == "ada"));
    }

    #[test]
    fn a_key_the_mesh_knows_is_simply_bound() {
        let users = [
            user(1, "neuralpunk", &["admin"], true),
            user(9, "neuralpunk-key", &["admin"], false),
        ];
        assert!(matches!(plan_for(1, 9, &users), Plan::Bind(OldKey::Live)));
        assert!(matches!(plan_for(9, 9, &users), Plan::Same));
    }

    /// The throwaway agent's first key, or `None` — loudly — when there is
    /// no throwaway agent. `MESHWARE_TEST_AGENT_SOCK` and nothing else, for
    /// the reason `mesh_core::hardware`'s live test gives: a test must never
    /// reach for whatever key the person running it has loaded. Run these
    /// deliberately:
    ///
    /// ```sh
    /// eval "$(ssh-agent -s)"
    /// ssh-keygen -q -t ed25519 -N "" -f /tmp/mw-test-key && ssh-add /tmp/mw-test-key
    /// MESHWARE_TEST_AGENT_SOCK="$SSH_AUTH_SOCK" \
    ///   cargo test -p meshware-director --lib live_ -- --ignored --test-threads=1
    /// ssh-agent -k
    /// ```
    fn throwaway_agent() -> Option<(PathBuf, HardwareKey)> {
        let Some(sock) = std::env::var_os("MESHWARE_TEST_AGENT_SOCK") else {
            eprintln!(
                "\n  skipped: set MESHWARE_TEST_AGENT_SOCK to a throwaway agent's socket.\n  \
                 This test will not use SSH_AUTH_SOCK.\n"
            );
            return None;
        };
        let socket = PathBuf::from(sock);
        let key = hw::list(&socket)
            .expect("list the throwaway agent")
            .into_iter()
            .next()
            .expect("load a throwaway key into that agent");
        Some((socket, key))
    }

    async fn users_as(engine: &Engine) -> Vec<UserRow> {
        match engine.ipc(IpcRequest::Users).await.unwrap() {
            IpcResponse::Users(rows) => rows,
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// The founder's whole path: the key joins as a second admin, the
    /// founding key is set aside unopened, and the window then signs as
    /// the agent's key — and the mesh answers it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs MESHWARE_TEST_AGENT_SOCK: a throwaway agent, never your own"]
    async fn live_the_founder_moves_onto_a_key() {
        let Some((socket, key)) = throwaway_agent() else {
            return;
        };
        let founder = mesh_testkit::spawn_founder("nim", "seckey-founder").await;
        let (engine, _at) = crate::build_run_tests::engine_for(&founder).await;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("user.key");
        engine.user.persist(&path).unwrap();
        let id = key.user_id().to_display_string();

        let plan = plan_key(&engine, &path, &socket, &id).await.unwrap();
        assert_eq!(plan.plan, "add-admin", "{plan:?}");
        let new_name = plan.new_username.clone().unwrap();

        let (bound, rebound) = use_key(&engine, &path, &socket, &id).await.unwrap();
        assert_eq!(bound.identity, id);
        let aside = PathBuf::from(bound.moved_to.expect("the founding key is moved aside"));
        match Stored::read(&aside).unwrap() {
            Stored::Plain(seed) => {
                assert_eq!(Keypair::from_seed(seed).user_id(), engine.user.user_id());
            }
            _ => panic!("the founding key must be set aside as it was"),
        }
        assert!(
            matches!(Stored::read(&path).unwrap(), Stored::Hardware(p) if p == key.public_bytes())
        );

        // Signed by the agent now, and the mesh knows who that is.
        let rows = users_as(&rebound).await;
        let row = rows.iter().find(|u| u.user_id == key.user_id()).unwrap();
        assert_eq!(row.username, new_name);
        assert!(row.roles.iter().any(|r| r == "admin"), "{row:?}");
        assert_eq!(rebound.user.user_id(), key.user_id());

        // And asking again finds nothing left to do.
        let again = plan_key(&rebound, &path, &socket, &id).await.unwrap();
        assert_eq!(again.plan, "same");
    }

    /// An admin who is not the founder rotates onto the key: roles carried,
    /// the old key revoked everywhere and — being spent — simply replaced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs MESHWARE_TEST_AGENT_SOCK: a throwaway agent, never your own"]
    async fn live_an_admin_rotates_onto_a_key() {
        let Some((socket, key)) = throwaway_agent() else {
            return;
        };
        let founder = mesh_testkit::spawn_founder("nim", "seckey-rotate").await;
        let (as_founder, _at) = crate::build_run_tests::engine_for(&founder).await;
        let mia = Keypair::generate().unwrap();
        add_admin(&as_founder, "mia", mia.user_id()).await.unwrap();
        let as_mia = as_founder.as_user(mia.clone());
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("user.key");
        mia.persist(&path).unwrap();
        let id = key.user_id().to_display_string();

        let plan = plan_key(&as_mia, &path, &socket, &id).await.unwrap();
        assert_eq!(plan.plan, "rotate", "{plan:?}");
        assert_eq!(plan.username, "mia");

        let (bound, rebound) = use_key(&as_mia, &path, &socket, &id).await.unwrap();
        assert_eq!(bound.moved_to, None, "a revoked key is replaced, not kept");
        let rows = users_as(&rebound).await;
        let now_mia = rows.iter().find(|u| u.user_id == key.user_id()).unwrap();
        assert_eq!(now_mia.username, "mia");
        assert!(now_mia.roles.iter().any(|r| r == "admin"), "{now_mia:?}");
        assert!(
            rows.iter().any(|u| u.user_id == mia.user_id() && u.revoked),
            "the old key is retired"
        );
    }

    /// The bug of 2026-09-18, end to end: a key that stops signing stops
    /// being asked. The first refusal pauses signing; every request after it
    /// fails at once with the marker the window looks for, without going
    /// near the agent; and after "try again" it signs as before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs MESHWARE_TEST_AGENT_SOCK: a throwaway agent, never your own"]
    async fn live_a_key_that_refuses_is_not_asked_again_until_resumed() {
        let Some((socket, key)) = throwaway_agent() else {
            return;
        };
        let founder = mesh_testkit::spawn_founder("nim", "seckey-pause").await;
        let (engine, _at) = crate::build_run_tests::engine_for(&founder).await;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("user.key");
        engine.user.persist(&path).unwrap();
        let id = key.user_id().to_display_string();
        let (_, rebound) = use_key(&engine, &path, &socket, &id).await.unwrap();
        users_as(&rebound).await;

        // Take the key out of the agent, the way unplugging a token does.
        let removed = std::process::Command::new("ssh-add")
            .arg("-D")
            .env("SSH_AUTH_SOCK", &socket)
            .output()
            .unwrap();
        assert!(removed.status.success(), "ssh-add -D: {removed:?}");

        let first = rebound.ipc(IpcRequest::Users).await.unwrap_err();
        assert!(first.starts_with(crate::engine::KEY_PAUSED), "{first}");
        assert!(rebound.key.paused().is_some());
        // Paused: answered without asking the agent at all.
        let started = Instant::now();
        for _ in 0..20 {
            let again = rebound.ipc(IpcRequest::Status).await.unwrap_err();
            assert!(again.starts_with(crate::engine::KEY_PAUSED), "{again}");
        }
        assert!(started.elapsed() < Duration::from_secs(2));

        // Put it back (the test's key file), resume, and it signs again.
        let key_file = std::env::var("MESHWARE_TEST_AGENT_KEY").ok();
        if let Some(file) = key_file {
            let added = std::process::Command::new("ssh-add")
                .arg(&file)
                .env("SSH_AUTH_SOCK", &socket)
                .output()
                .unwrap();
            assert!(added.status.success(), "ssh-add: {added:?}");
            rebound.key.resume();
            users_as(&rebound).await;
        }
    }

    /// A key that signs without asking twice is ready; nothing is bound.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "needs MESHWARE_TEST_AGENT_SOCK: a throwaway agent, never your own"]
    async fn live_a_key_that_signs_quietly_passes_the_test() {
        let Some((_socket, key)) = throwaway_agent() else {
            return;
        };
        let result = test_signatures(&key);
        assert!(result.ok && !result.asks_every_time, "{result:?}");
    }

    #[test]
    fn a_key_file_is_refused_with_its_name() {
        let plan = Plan::File(PathBuf::from("/home/you/.ssh/meshware"));
        assert!(matches!(plan, Plan::File(_)));
        assert!(
            file_message(Path::new("/nowhere/.ssh/meshware"))
                .contains("a key file on this computer")
        );
    }

    #[test]
    fn only_files_named_after_the_key_are_listed_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("user.key");
        for name in [
            "user.key",
            "user.key.set-aside-mwu1abc",
            "user.key.from-director",
            "other",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        assert_eq!(
            beside(&key),
            vec![
                "user.key.from-director".to_string(),
                "user.key.set-aside-mwu1abc".into()
            ]
        );
    }
}
