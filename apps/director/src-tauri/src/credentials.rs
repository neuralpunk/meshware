//! "Remember in my keychain": the OS credential store, opt-in.
//!
//! A person adding machines every week types the same rack password every
//! week, and the alternative — a password in a config file — is the thing
//! this product exists to make unnecessary. The OS already has a place for
//! this: the macOS Keychain, the Windows Credential Manager, the
//! Secret Service on a Linux desktop (GNOME Keyring, KWallet). `keyring`
//! is one API over all three, in pure Rust.
//!
//! What is stored is one record per SSH account **on one machine** — the
//! login method, the password or key path and passphrase, and the sudo
//! password — under the service `io.meshware.director`, keyed
//! `ssh:user@host`. Nothing is stored unless the switch in the credentials
//! form is on, and *which* accounts have a record is kept in Director's own
//! settings file (names only, never secrets) so the form can offer to fill
//! them in. A headless Linux box with no secret service says so in a
//! sentence, and the deploy still works without it.
//!
//! **Per machine, not per user name** (SEC-23). Records used to be keyed by
//! the user name alone, so the `root` password typed for one rack was what
//! Director offered to every machine anyone later reached as `root` — sent
//! to a host that never had it, which may not be a host anybody trusts. A
//! run filled from the keychain now needs a record for every machine in it,
//! and those records must agree; a record from before this change names no
//! machine and is not used for any.
//!
//! Every keyring call blocks on a platform API, so each runs on the
//! blocking pool rather than an async worker.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::view::{DeployAuthDraft, RememberedCredentialView};

/// The keyring service name every record lives under.
const SERVICE: &str = "io.meshware.director";

/// What one record holds. The account and machine are the key, not part
/// of the value.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Stored {
    method: String,
    password: Option<String>,
    key_path: Option<String>,
    passphrase: Option<String>,
    sudo_password: Option<String>,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = |v: &Option<String>| v.as_ref().map(|_| "<redacted>");
        f.debug_struct("Stored")
            .field("method", &self.method)
            .field("password", &shown(&self.password))
            .field("key_path", &self.key_path)
            .field("passphrase", &shown(&self.passphrase))
            .field("sudo_password", &shown(&self.sudo_password))
            .finish()
    }
}

/// One machine a credential is for: the account a run logs in as there,
/// and the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Machine {
    pub user: String,
    pub host: String,
}

impl Machine {
    fn key(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// The machines a run's targets name, each with the account it logs in as
/// — the target's own `user@`, else the form's. `hosts` are the strings a
/// run keys its cards by: a target, or `node|target`.
pub fn machines(default_user: &str, hosts: &[String]) -> Result<Vec<Machine>, String> {
    let mut out: Vec<Machine> = Vec::new();
    for h in hosts {
        let target = h.split_once('|').map_or(h.as_str(), |(_, t)| t);
        let t = mesh_core::deploy::Target::parse(target).map_err(|e| e.to_string())?;
        let user = t
            .user
            .clone()
            .unwrap_or_else(|| default_user.trim().to_string());
        if user.is_empty() {
            continue;
        }
        let m = Machine {
            user,
            host: t.host.to_ascii_lowercase(),
        };
        if !out.contains(&m) {
            out.push(m);
        }
    }
    Ok(out)
}

fn entry(machine: &Machine) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, &format!("ssh:{}", machine.key())).map_err(friendly)
}

/// A record from before records named their machine.
fn legacy_entry(user: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(SERVICE, &format!("ssh:{user}")).map_err(friendly)
}

/// The machines with a remembered record for `user`.
fn remembered_for(user: &str) -> Vec<Machine> {
    crate::engine::Settings::load()
        .map(|s| s.remembered_users)
        .unwrap_or_default()
        .iter()
        .filter_map(|k| k.split_once('@'))
        .filter(|(u, _)| *u == user)
        .map(|(u, h)| Machine {
            user: u.to_string(),
            host: h.to_string(),
        })
        .collect()
}

/// The platform's refusal, in the words a screen can show.
fn friendly(e: keyring::Error) -> String {
    match e {
        keyring::Error::NoStorageAccess(inner) => format!(
            "this machine's keychain refused access ({inner}) — on Linux that usually means no \
             secret service (GNOME Keyring, KWallet) is running"
        ),
        keyring::Error::PlatformFailure(inner) => format!(
            "no keychain is available here ({inner}) — on a headless Linux box there is often \
             none; the password still works for this run"
        ),
        keyring::Error::NoEntry => "nothing is remembered for that user".into(),
        other => other.to_string(),
    }
}

/// **Which** credentials are remembered for `user` — never the credentials
/// themselves (S3).
///
/// This used to hand the window the SSH password, the key passphrase and the
/// sudo password in cleartext, which made one script injection in a page
/// that renders node names, imported inventories, job output and model
/// responses equivalent to reading the operator's passwords. The form does
/// not need them: it needs to know a record exists, which method it used and
/// which fields it filled, so it can say so and leave those inputs blank.
/// The secrets are read on the Rust side by [`resolve_secrets`], at the
/// moment a deploy actually needs them.
#[tauri::command]
pub async fn credential_get(
    state: tauri::State<'_, crate::Director>,
    user: String,
) -> Result<Option<RememberedCredentialView>, String> {
    let user = user.trim().to_string();
    if user.is_empty() {
        return Ok(None);
    }
    let mut found = None;
    for machine in remembered_for(&user) {
        if let Some(s) = stored_for(machine).await? {
            found = Some(s);
            break;
        }
    }
    Ok(found.map(|s| view_of(user, s, &state.picked)))
}

/// A record as the form sees it: which secrets it holds, and its key file
/// as a picker token (SEC-14).
///
/// The token is no wider than what `from_keychain` already allowed — the
/// backend reading this record's key path itself — and the path it names
/// was a person's pick when the record was saved, since `credential_put`
/// only ever stores a path it resolved from a token.
fn view_of(user: String, s: Stored, picked: &crate::picked::Picked) -> RememberedCredentialView {
    let key_token = s
        .key_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .map(|p| picked.remember(std::path::Path::new(p)));
    RememberedCredentialView {
        user,
        method: if s.method.is_empty() {
            "password".into()
        } else {
            s.method
        },
        key_path: s.key_path,
        key_token,
        has_password: s.password.is_some_and(|p| !p.is_empty()),
        has_passphrase: s.passphrase.is_some_and(|p| !p.is_empty()),
        has_sudo_password: s.sudo_password.is_some_and(|p| !p.is_empty()),
    }
}

/// The stored record for one machine, off the async runtime. Not a
/// command: the only callers are in this process, and that is the point.
async fn stored_for(machine: Machine) -> Result<Option<Stored>, String> {
    tokio::task::spawn_blocking(move || {
        let e = entry(&machine)?;
        match e.get_password() {
            Ok(json) => {
                let json = Zeroizing::new(json);
                serde_json::from_str::<Stored>(&json)
                    .map(Some)
                    .map_err(|e| format!("the remembered record could not be read: {e}"))
            }
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(friendly(e)),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Turn the draft's key file from a picker token into the path it names
/// (SEC-14). The window never sends a key path: it sends the token for a
/// file the person picked, or nothing — and a draft filled from the
/// keychain takes the path its record holds (see [`resolve_secrets`]).
pub fn resolve_key_file(
    mut auth: DeployAuthDraft,
    picked: &crate::picked::Picked,
) -> Result<DeployAuthDraft, String> {
    auth.key_path = match auth.key_path.as_deref().map(str::trim) {
        Some(token) if !token.is_empty() => {
            Some(picked.require(token)?.to_string_lossy().into_owned())
        }
        _ => None,
    };
    Ok(auth)
}

/// Fill a draft's empty secret fields from the keychain, when the form said
/// it was filled in from there (S3).
///
/// Typed beats remembered: a field the person actually filled in arrives
/// non-empty and is left exactly as it is. Everything else is fetched here,
/// in this process, on its way into `Creds` — which is the only place it was
/// ever needed.
///
/// Every machine in the run needs its own record, and they must agree: one
/// set of secrets goes to all of them, and it may only be one each of them
/// was remembered with.
pub async fn resolve_secrets(
    mut auth: DeployAuthDraft,
    machines: &[Machine],
) -> Result<DeployAuthDraft, String> {
    if !auth.from_keychain {
        return Ok(auth);
    }
    let mut common: Option<Stored> = None;
    for machine in machines {
        let Some(found) = stored_for(machine.clone()).await? else {
            return Err(format!(
                "nothing is remembered for {} — type the password for this run (tick \
                 \"remember\" to keep it for that machine)",
                machine.key()
            ));
        };
        match &common {
            None => common = Some(found),
            Some(c) if *c == found => {}
            Some(_) => {
                return Err(
                    "the machines in this run were remembered with different credentials — \
                     run them separately, or type the password"
                        .into(),
                );
            }
        }
    }
    let Some(s) = common else {
        return Err("no machine to look a remembered credential up for".into());
    };
    let empty = |v: &Option<String>| v.as_deref().is_none_or(str::is_empty);
    if empty(&auth.password) {
        auth.password = s.password;
    }
    if empty(&auth.passphrase) {
        auth.passphrase = s.passphrase;
    }
    if empty(&auth.sudo_password) {
        auth.sudo_password = s.sudo_password;
    }
    // The key file is the record's, unless one was picked for this run.
    if empty(&auth.key_path) {
        auth.key_path = s.key_path;
    }
    Ok(auth)
}

/// Remember these credentials for the machines a run reached with them.
///
/// `hosts` are the run's own card keys, as its plan event gave them. A
/// draft the form filled from the keychain arrives with its secret fields
/// empty (S3), so it is resolved first: re-saving a record the person did
/// not retype must not blank it out, and a field they *did* retype still
/// wins because a present value is never overwritten by a stored one.
#[tauri::command]
pub async fn credential_put(
    state: tauri::State<'_, crate::Director>,
    auth: DeployAuthDraft,
    hosts: Vec<String>,
) -> Result<(), String> {
    let machines = machines(&auth.user, &hosts)?;
    if machines.is_empty() {
        return Err("a remembered credential needs a user name and a machine".into());
    }
    let auth = resolve_key_file(auth, &state.picked)?;
    let auth = resolve_secrets(auth, &machines).await?;
    let stored = Stored {
        method: auth.method,
        password: auth.password.filter(|p| !p.is_empty()),
        key_path: auth.key_path.filter(|p| !p.is_empty()),
        passphrase: auth.passphrase.filter(|p| !p.is_empty()),
        sudo_password: auth.sudo_password.filter(|p| !p.is_empty()),
    };
    let json = Zeroizing::new(serde_json::to_string(&stored).map_err(|e| e.to_string())?);
    let each = machines.clone();
    tokio::task::spawn_blocking(move || {
        each.iter()
            .try_for_each(|m| entry(m)?.set_password(&json).map_err(friendly))
    })
    .await
    .map_err(|e| e.to_string())??;
    for m in &machines {
        remember_name(&m.key(), true)?;
    }
    Ok(())
}

/// Forget what is remembered for `user`, on every machine.
#[tauri::command]
pub async fn credential_forget(user: String) -> Result<(), String> {
    let user = user.trim().to_string();
    let machines = remembered_for(&user);
    let (each, name) = (machines.clone(), user.clone());
    tokio::task::spawn_blocking(move || {
        let gone = |e: Result<(), keyring::Error>| match e {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(friendly(e)),
        };
        for m in &each {
            gone(entry(m)?.delete_credential())?;
        }
        gone(legacy_entry(&name)?.delete_credential())
    })
    .await
    .map_err(|e| e.to_string())??;
    for m in &machines {
        remember_name(&m.key(), false)?;
    }
    remember_name(&user, false)
}

/// The user names with a remembered record — names only, from Director's
/// own settings; the keychain cannot be listed portably and should not be.
#[tauri::command]
pub fn credential_users() -> Vec<String> {
    let mut users: Vec<String> = crate::engine::Settings::load()
        .map(|s| s.remembered_users)
        .unwrap_or_default()
        .iter()
        .filter_map(|k| k.split_once('@').map(|(u, _)| u.to_string()))
        .collect();
    users.sort();
    users.dedup();
    users
}

fn remember_name(user: &str, on: bool) -> Result<(), String> {
    let Some(mut settings) = crate::engine::Settings::load() else {
        // No settings file yet means no mesh yet; nothing to index into.
        return Ok(());
    };
    let had = settings.remembered_users.iter().any(|u| u == user);
    if on && !had {
        settings.remembered_users.push(user.to_string());
        settings.remembered_users.sort();
    } else if !on && had {
        settings.remembered_users.retain(|u| u != user);
    } else {
        return Ok(());
    }
    settings.save()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_names_each_machine_with_the_account_it_uses_there() {
        let hosts = vec![
            "rack-1:22".to_string(),
            "ops@Rack-2:2222".to_string(),
            "mw1abc|rack-1".to_string(),
            "[fe80::1]:22".to_string(),
        ];
        let got = machines("root", &hosts).unwrap();
        let keys: Vec<String> = got.iter().map(Machine::key).collect();
        assert_eq!(keys, ["root@rack-1", "ops@rack-2", "root@fe80::1"]);
        // No account at all: nothing to key a record on.
        assert!(machines("", &["rack-1".to_string()]).unwrap().is_empty());
    }

    #[test]
    fn a_key_file_arrives_as_a_token_and_leaves_as_a_path() {
        let picked = crate::picked::Picked::default();
        let token = picked.remember(std::path::Path::new("/home/you/.ssh/id_ed25519"));
        let draft = |key: Option<&str>| DeployAuthDraft {
            user: "root".into(),
            method: "key".into(),
            password: None,
            key_path: key.map(str::to_string),
            passphrase: None,
            sudo_password: None,
            remember: false,
            from_keychain: false,
        };
        let got = resolve_key_file(draft(Some(&token)), &picked).unwrap();
        assert_eq!(got.key_path.as_deref(), Some("/home/you/.ssh/id_ed25519"));
        // A path in the token's place is refused, not read.
        assert!(resolve_key_file(draft(Some("/etc/shadow")), &picked).is_err());
        assert_eq!(
            resolve_key_file(draft(Some("")), &picked).unwrap().key_path,
            None
        );
    }

    #[test]
    fn a_remembered_key_file_reaches_the_form_as_a_token_and_no_secret_does() {
        let picked = crate::picked::Picked::default();
        let record = Stored {
            method: "key".into(),
            password: None,
            key_path: Some("/home/you/.ssh/meshware".into()),
            passphrase: Some("swordfish".into()),
            sudo_password: Some("letmein".into()),
        };
        let v = view_of("root".into(), record, &picked);
        assert_eq!(v.key_path.as_deref(), Some("/home/you/.ssh/meshware"));
        // The token is what a run sends back, and it names the record's file —
        // so the key survives the form no longer deferring to the keychain.
        let token = v.key_token.as_deref().expect("a token for the key file");
        let draft = DeployAuthDraft {
            user: "root".into(),
            method: "key".into(),
            password: None,
            key_path: Some(token.into()),
            passphrase: Some("typed".into()),
            sudo_password: None,
            remember: false,
            from_keychain: false,
        };
        assert_eq!(
            resolve_key_file(draft, &picked)
                .unwrap()
                .key_path
                .as_deref(),
            Some("/home/you/.ssh/meshware")
        );
        assert!(v.has_passphrase && v.has_sudo_password && !v.has_password);
        let shown = serde_json::to_string(&v).unwrap();
        for secret in ["swordfish", "letmein"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        // No key file, no token.
        let bare = Stored {
            method: "password".into(),
            password: Some("hunter2".into()),
            ..Stored::default()
        };
        assert_eq!(view_of("root".into(), bare, &picked).key_token, None);
    }

    #[test]
    fn a_record_never_prints_its_secrets() {
        let s = Stored {
            method: "password".into(),
            password: Some("hunter2".into()),
            key_path: None,
            passphrase: Some("swordfish".into()),
            sudo_password: Some("letmein".into()),
        };
        let shown = format!("{s:?}");
        for secret in ["hunter2", "swordfish", "letmein"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }
}
