//! `meshnode support bundle` — one file that answers the first hour of
//! questions (E2.6).
//!
//! ## What this is for
//!
//! Before there is a support team, every problem report is a conversation:
//! *what version, what does `doctor` say, is the node even up, what is in
//! your config, what happened just before it broke.* Six round trips across
//! a timezone before anybody has looked at anything. This collects the
//! answers in one pass and writes them to a file a person can attach.
//!
//! ## Two rules it is built around
//!
//! **Every section degrades on its own.** A support bundle is wanted
//! precisely when something is broken, so the daemon being unreachable is
//! the *normal* case, not the error case. Each section that cannot be
//! collected records why, in [`Bundle::unavailable`], and the rest still
//! goes in the file. A bundle from a node that will not start is still
//! worth reading — it has the config, the host, and every `doctor` check
//! that does not need a socket.
//!
//! **Nothing sensitive leaves the machine.** A bundle that leaks a
//! credential is worse than no bundle at all, because it will be emailed.
//! The config is redacted by *key name*, recursively, rather than by a list
//! of known-sensitive fields — a list goes stale the moment somebody adds
//! `[integrations.foo] token`, and nobody notices until it is in an
//! attachment. Redaction by shape fails safe: a new field called `token`
//! is redacted the day it is added, by code nobody had to remember to
//! update. The count is reported so a reader can see it happened.
//!
//! URLs get a narrower treatment than a blanket redaction, because a
//! webhook's *host* is diagnostic and its path is the secret: a Slack
//! endpoint is `https://hooks.slack.com/services/T00/B00/XXXX`, where
//! everything after the host is the credential. Scheme and host survive;
//! userinfo, path and query do not.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

use mesh_core::config::NodeConfig;
use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse};

/// Key-name fragments whose value is never included.
///
/// Matched case-insensitively as a substring, so `api_token`,
/// `TOKEN`, and `refresh_token_url` are all caught by `token`.
const SENSITIVE: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "bearer",
    "auth",
    "private",
    "cookie",
    "session_id",
];

/// The bundle. Serialized as JSON — one file, greppable, and parseable by
/// whatever tooling grows around it later.
#[derive(Debug, Serialize)]
pub struct Bundle {
    pub meta: Meta,
    /// The resolved config, redacted. `None` if it could not be serialized.
    pub config: Option<serde_json::Value>,
    pub doctor: Vec<mesh_core::doctor::Check>,
    pub node: Option<serde_json::Value>,
    pub nodes: Vec<NodeSummary>,
    pub audit: Option<serde_json::Value>,
    pub metrics: Option<String>,
    /// Sections that could not be collected, and why. The most useful part
    /// of a bundle taken from a broken node.
    pub unavailable: Vec<Unavailable>,
    /// How many values the redactor removed. Zero on a config with no
    /// credentials in it, which is the common case and worth confirming.
    pub redacted_values: usize,
}

#[derive(Debug, Serialize)]
pub struct Meta {
    pub generated_at_micros: u64,
    pub meshnode_version: &'static str,
    pub protocol_major: u16,
    pub protocol_minor: u16,
    pub os: &'static str,
    pub arch: &'static str,
    pub kernel: Option<String>,
    pub data_dir: String,
    pub user_id: String,
    /// Whether this key is sealed (B9) — relevant because a locked key is a
    /// common reason a CLI command "does nothing".
    pub identity_protected: bool,
}

/// One peer, trimmed to what a support question actually needs: who it is,
/// whether it is alive, and what it is running.
#[derive(Debug, Serialize)]
pub struct NodeSummary {
    pub node_id: String,
    pub name: String,
    pub liveness: String,
    pub version: Option<String>,
    pub is_self: bool,
    pub revoked: bool,
    pub left: bool,
    pub quarantined: bool,
}

#[derive(Debug, Serialize)]
pub struct Unavailable {
    pub section: &'static str,
    pub reason: String,
}

/// Replace every sensitive value in `value`, returning how many were hit.
///
/// Walks objects and arrays. A matching key has its value replaced whatever
/// its type — an object under `credentials` goes in its entirety, because
/// the interesting thing about it is exactly the part that must not ship.
pub fn redact(value: &mut serde_json::Value) -> usize {
    fn walk(v: &mut serde_json::Value, count: &mut usize) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, val) in map.iter_mut() {
                    let lower = k.to_ascii_lowercase();
                    if SENSITIVE.iter().any(|s| lower.contains(s)) {
                        // A null, an empty map or an empty list cannot be
                        // hiding a credential, and their emptiness is worth
                        // reading: `"secret_providers": {}` tells support
                        // that none are configured, which `<redacted>`
                        // would not. Counting them would also inflate the
                        // number the summary prints until it meant nothing.
                        if !is_empty(val) {
                            *val = serde_json::Value::String("<redacted>".into());
                            *count += 1;
                        }
                        continue;
                    }
                    // A URL keeps its scheme and host and loses everything
                    // that could be a credential. `notify.url` is the one
                    // that matters — a webhook's path *is* the secret.
                    if (lower == "url" || lower.ends_with("_url") || lower.ends_with("_uri"))
                        && let Some(s) = val.as_str()
                        && let Some(trimmed) = redact_url(s)
                    {
                        *val = serde_json::Value::String(trimmed);
                        *count += 1;
                        continue;
                    }
                    walk(val, count);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items.iter_mut() {
                    walk(item, count);
                }
            }
            _ => {}
        }
    }
    let mut count = 0;
    walk(value, &mut count);
    count
}

/// Null, `{}` or `[]` — nothing a credential could be hiding in.
fn is_empty(v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Null => true,
        serde_json::Value::Object(m) => m.is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// `https://user:pw@hooks.slack.com/services/T/B/XXX` → `https://hooks.slack.com/<redacted>`
///
/// `None` when there is nothing to remove, so an ordinary
/// `http://127.0.0.1:7776` is left alone and not counted as a redaction.
fn redact_url(s: &str) -> Option<String> {
    let (scheme, rest) = s.split_once("://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, Some(p)),
        None => (rest, None),
    };
    // Strip any `user:password@`.
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let had_userinfo = host.len() != authority.len();
    let had_path = path.is_some_and(|p| !p.is_empty());
    if !had_userinfo && !had_path {
        return None;
    }
    Some(if had_path {
        format!("{scheme}://{host}/<redacted>")
    } else {
        format!("{scheme}://{host}")
    })
}

/// Collect everything. Never fails as a whole: what could not be gathered
/// is recorded in `unavailable` and the bundle is still returned.
pub async fn collect(
    ctx: std::result::Result<&crate::Ctx, String>,
    config: &NodeConfig,
    user: &Keypair,
    since_micros: Option<u64>,
    audit_limit: u32,
) -> Bundle {
    let mut unavailable = Vec::new();
    let key_path = mesh_core::identity::cli_user_identity_path(&config.data_dir, None);

    let mut redacted_values = 0;
    let config_json = match serde_json::to_value(config) {
        Ok(mut v) => {
            redacted_values = redact(&mut v);
            Some(v)
        }
        Err(e) => {
            unavailable.push(Unavailable {
                section: "config",
                reason: format!("could not serialize the resolved config: {e}"),
            });
            None
        }
    };

    // `doctor` runs whether or not a daemon answers — several of its checks
    // are about the filesystem and the host, and those are exactly the ones
    // that matter when the node will not start.
    let doctor = mesh_core::doctor::run(config, user).await;

    let mut node = None;
    let mut nodes = Vec::new();
    let mut audit = None;

    match ctx {
        Err(why) => unavailable.push(Unavailable {
            section: "daemon",
            reason: format!(
                "{why} — config, host and doctor were still collected, and are \
                 usually what a node that will not start needs looking at"
            ),
        }),
        Ok(ctx) => {
            // `Ctx::new` resolves a socket path; it does not connect. So the
            // first request is the reachability probe, and when it fails
            // there is no point asking twice more — three copies of one
            // "is `meshnode up` running?" is noise in the file somebody has
            // to read.
            let mut reachable = true;
            match ctx.ipc(IpcRequest::Status).await {
                Ok(IpcResponse::Status(s)) => node = serde_json::to_value(&s).ok(),
                Ok(other) => unavailable.push(Unavailable {
                    section: "status",
                    reason: format!("unexpected response: {other:?}"),
                }),
                Err(e) => {
                    reachable = false;
                    unavailable.push(Unavailable {
                        section: "daemon",
                        reason: format!(
                            "{e} — so status, nodes and audit are all missing. The config, \
                             the host and every doctor check that needs no socket are still \
                             here, and are usually what a node that will not start needs \
                             looking at."
                        ),
                    });
                }
            }
            if reachable {
                match ctx.ipc(IpcRequest::Nodes).await {
                    Ok(IpcResponse::Nodes(rows)) => {
                        nodes = rows
                            .into_iter()
                            .map(|r| NodeSummary {
                                node_id: r.node_id.to_string(),
                                name: r.name,
                                liveness: format!("{:?}", r.liveness),
                                version: r.version,
                                is_self: r.is_self,
                                revoked: r.revoked,
                                left: r.left,
                                quarantined: r.quarantined.is_some(),
                            })
                            .collect();
                    }
                    Ok(other) => unavailable.push(Unavailable {
                        section: "nodes",
                        reason: format!("unexpected response: {other:?}"),
                    }),
                    Err(e) => unavailable.push(Unavailable {
                        section: "nodes",
                        reason: e.to_string(),
                    }),
                }
                // The audit tail is the "what happened just before it broke"
                // question. It is authorized like any other read, so a
                // non-admin gets a refusal here and a bundle without it —
                // which is correct, and says so rather than looking empty.
                let req = IpcRequest::AuditQuery {
                    user: None,
                    action: None,
                    since_micros,
                    limit: audit_limit,
                };
                match ctx.ipc(req).await {
                    Ok(IpcResponse::Audit(rows)) => audit = serde_json::to_value(&rows).ok(),
                    Ok(other) => unavailable.push(Unavailable {
                        section: "audit",
                        reason: format!("unexpected response: {other:?}"),
                    }),
                    // The role hint belongs on a refusal, not on every failure:
                    // appending it to a dead socket would send somebody looking
                    // at their grants for a problem that is a stopped daemon.
                    Err(e) => {
                        let text = e.to_string();
                        let denied = text.contains("denied") || text.contains("permission");
                        unavailable.push(Unavailable {
                            section: "audit",
                            reason: if denied {
                                format!(
                                    "{text} — reading the audit log needs the admin or auditor role"
                                )
                            } else {
                                text
                            },
                        });
                    }
                }
            }
        }
    }

    let metrics = match config.http_addr {
        None => {
            unavailable.push(Unavailable {
                section: "metrics",
                reason: "[http] is disabled on this node, so there is no /metrics to scrape".into(),
            });
            None
        }
        Some(addr) => match scrape_metrics(addr).await {
            Ok(text) => Some(text),
            Err(e) => {
                unavailable.push(Unavailable {
                    section: "metrics",
                    reason: format!("GET http://{addr}/metrics: {e}"),
                });
                None
            }
        },
    };

    Bundle {
        meta: Meta {
            generated_at_micros: now_micros(),
            meshnode_version: env!("CARGO_PKG_VERSION"),
            protocol_major: mesh_proto::PROTOCOL_MAJOR,
            protocol_minor: mesh_proto::PROTOCOL_MINOR,
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            kernel: kernel_release(),
            data_dir: config.data_dir.display().to_string(),
            user_id: user.user_id().to_string(),
            identity_protected: mesh_core::keyfile::is_sealed(&key_path),
        },
        config: config_json,
        doctor,
        node,
        nodes,
        audit,
        metrics,
        unavailable,
        redacted_values,
    }
}

/// A plain `GET /metrics` over TCP.
///
/// Hand-rolled rather than through `httpc`: that one is the TLS client for
/// fetching releases, and this is one unauthenticated request to loopback.
/// Pulling a client stack in for it would be the larger change.
async fn scrape_metrics(addr: std::net::SocketAddr) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let fetch = async {
        let mut stream = tokio::net::TcpStream::connect(addr).await?;
        stream
            .write_all(
                format!("GET /metrics HTTP/1.0\r\nHost: {addr}\r\nConnection: close\r\n\r\n")
                    .as_bytes(),
            )
            .await?;
        let mut body = Vec::new();
        stream.read_to_end(&mut body).await?;
        Ok::<_, std::io::Error>(body)
    };
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), fetch)
        .await
        .map_err(|_| anyhow::anyhow!("timed out after 5s"))??;
    let text = String::from_utf8_lossy(&body).into_owned();
    // Drop the status line and headers; the body is what is wanted.
    Ok(match text.split_once("\r\n\r\n") {
        Some((_, b)) => b.to_string(),
        None => text,
    })
}

fn kernel_release() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .ok()
        .map(|s| s.trim().to_string())
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Write the bundle and tell the person what is in it.
///
/// Mode 0600. It is redacted, not harmless: it still carries node names,
/// addresses, user ids and an audit tail, and it lands in whatever directory
/// somebody happened to be in.
pub fn write(bundle: &Bundle, out: Option<PathBuf>) -> Result<PathBuf> {
    let path = out.unwrap_or_else(|| {
        PathBuf::from(format!(
            "meshware-support-{}.json",
            bundle.meta.generated_at_micros / 1_000_000
        ))
    });
    let json = serde_json::to_string_pretty(bundle).context("serializing the bundle")?;
    mesh_core::keyfile::write_private(&path, json.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// The terminal summary: what went in, what did not, and what to check
/// before sending it.
pub fn print_summary(bundle: &Bundle, path: &Path) {
    println!("Wrote {} (mode 0600)", path.display());
    println!();
    println!(
        "  meshnode      {} · protocol {}.{}",
        bundle.meta.meshnode_version, bundle.meta.protocol_major, bundle.meta.protocol_minor
    );
    println!(
        "  host          {} {}{}",
        bundle.meta.os,
        bundle.meta.arch,
        bundle
            .meta
            .kernel
            .as_deref()
            .map(|k| format!(" · kernel {k}"))
            .unwrap_or_default()
    );
    let (ok, warn, fail) = verdict_counts(bundle);
    println!("  doctor        {ok} ok, {warn} warn, {fail} fail");
    println!("  nodes         {}", bundle.nodes.len());
    println!(
        "  audit         {}",
        match &bundle.audit {
            Some(serde_json::Value::Array(a)) => format!("{} entries", a.len()),
            _ => "not included".into(),
        }
    );
    println!(
        "  metrics       {}",
        match &bundle.metrics {
            Some(m) => format!("{} lines", m.lines().count()),
            None => "not included".into(),
        }
    );
    println!("  redacted      {} values", bundle.redacted_values);

    if !bundle.unavailable.is_empty() {
        println!();
        println!("Not collected:");
        for u in &bundle.unavailable {
            println!("  {:<12}  {}", u.section, u.reason);
        }
    }
    println!();
    println!(
        "Credentials are redacted by key name and webhook paths are stripped. Node names,\n\
         addresses, user ids and the audit tail are not — read it before you send it."
    );
}

fn verdict_counts(bundle: &Bundle) -> (usize, usize, usize) {
    use mesh_core::doctor::Verdict;
    let mut counts = (0, 0, 0);
    for c in &bundle.doctor {
        match c.verdict {
            Verdict::Ok => counts.0 += 1,
            Verdict::Warn => counts.1 += 1,
            Verdict::Fail => counts.2 += 1,
        }
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credentials_are_redacted_by_key_name_at_any_depth() {
        let mut v = json!({
            "node_name": "alpha",
            "integrations": {
                "github": { "api_token": "ghp_realsecret", "owner": "acme" },
                "vault": { "credentials": { "role_id": "r", "secret_id": "s" } }
            },
            "providers": [{ "name": "aws", "AWS_SECRET_ACCESS_KEY": "AKIAsecret" }],
            "listen": "0.0.0.0:7777"
        });
        let n = redact(&mut v);

        let dumped = serde_json::to_string(&v).expect("serialize");
        assert!(
            !dumped.contains("ghp_realsecret"),
            "token survived: {dumped}"
        );
        assert!(!dumped.contains("AKIAsecret"), "aws key survived: {dumped}");
        assert!(
            !dumped.contains("role_id"),
            "the whole credentials object should go"
        );
        // The diagnostic fields are the point of the bundle and must remain.
        assert_eq!(v["node_name"], "alpha");
        assert_eq!(v["listen"], "0.0.0.0:7777");
        assert_eq!(v["integrations"]["github"]["owner"], "acme");
        assert_eq!(n, 3, "expected three redactions, got {n}");
    }

    /// The case that motivated URL handling: for a Slack webhook the host is
    /// diagnostic and the path is the credential.
    #[test]
    fn a_webhook_keeps_its_host_and_loses_its_path() {
        let mut v = json!({
            "notify": { "url": "https://hooks.slack.com/services/T00/B00/XXXXsecret" }
        });
        let n = redact(&mut v);
        assert_eq!(v["notify"]["url"], "https://hooks.slack.com/<redacted>");
        assert_eq!(n, 1);
    }

    #[test]
    fn userinfo_is_stripped_even_without_a_path() {
        assert_eq!(
            redact_url("https://admin:hunter2@internal.example"),
            Some("https://internal.example".into())
        );
    }

    /// A plain loopback address has nothing to hide and must not be counted
    /// as a redaction — otherwise the reported count stops meaning anything.
    #[test]
    fn an_ordinary_url_is_left_alone() {
        assert_eq!(redact_url("http://127.0.0.1:7776"), None);
        let mut v = json!({ "http": { "url": "http://127.0.0.1:7776" } });
        assert_eq!(redact(&mut v), 0);
        assert_eq!(v["http"]["url"], "http://127.0.0.1:7776");
    }

    /// Redaction is by shape, so a field nobody has written yet is covered
    /// the day it is added. This is the property that makes the approach
    /// worth having over a list of known fields.
    #[test]
    fn a_field_that_does_not_exist_yet_is_still_redacted() {
        let mut v = json!({ "some_future_section": { "refresh_token": "s3cr3t" } });
        assert_eq!(redact(&mut v), 1);
        assert_eq!(v["some_future_section"]["refresh_token"], "<redacted>");
    }

    /// A null is an absent value, not a secret. Counting it would inflate
    /// the number the summary prints.
    #[test]
    fn absent_values_are_not_counted_as_redactions() {
        let mut v = json!({ "auth_token": null });
        assert_eq!(redact(&mut v), 0);
    }

    /// An empty map cannot be hiding a credential, and its emptiness is the
    /// answer to a real support question — "are any secret providers
    /// configured?" — that `<redacted>` would destroy.
    #[test]
    fn empty_containers_keep_their_emptiness() {
        let mut v = json!({ "secret_providers": {}, "auth_headers": [] });
        assert_eq!(redact(&mut v), 0);
        assert_eq!(v["secret_providers"], json!({}));
        assert_eq!(v["auth_headers"], json!([]));

        // ...but the moment one has something in it, it goes.
        let mut v = json!({ "secret_providers": { "vault": { "addr": "x" } } });
        assert_eq!(redact(&mut v), 1);
        assert_eq!(v["secret_providers"], "<redacted>");
    }
}
