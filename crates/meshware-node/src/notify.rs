//! "Tell me when it is done" — the small half of notifications.
//!
//! Sinks (M31) copy history into a warehouse. This is the other thing: a
//! person who submitted a four-hour job wants one message when it ends,
//! and wants it where they already look. Three destinations, all opt-in
//! through `[notify]` in the node's config, all best-effort: a
//! notification that cannot be delivered is said on stderr and never
//! changes the command's exit code, because the job's outcome is the
//! answer and the notification is only the messenger.

use std::time::Duration;

use mesh_core::config::NotifyConfig;

/// What happened, in the fields every destination gets.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Event {
    pub title: String,
    pub body: String,
    pub job: String,
    pub state: String,
    pub exit_code: Option<i32>,
    pub node: String,
    pub command: String,
    pub seconds: u64,
}

/// Send `event` everywhere `[notify]` points. Returns what was tried and
/// what failed, for stderr.
pub async fn send(config: &NotifyConfig, event: &Event) -> Vec<String> {
    let mut notes = Vec::new();
    if !config.is_configured() {
        notes.push(
            "--notify: nothing is configured under [notify] in meshnode.toml (url, command \
             or desktop), so there was nowhere to send this"
                .into(),
        );
        return notes;
    }
    if let Some(url) = &config.url {
        match post(url, event).await {
            Ok(()) => notes.push(format!("notified {}", host_of(url))),
            Err(e) => notes.push(format!("could not notify {}: {e}", host_of(url))),
        }
    }
    if let Some(command) = &config.command {
        match run_hook(command, event).await {
            Ok(()) => notes.push("notify command ran".into()),
            Err(e) => notes.push(format!("notify command failed: {e}")),
        }
    }
    if config.desktop {
        match desktop(event).await {
            Ok(true) => notes.push("desktop notification shown".into()),
            Ok(false) => {} // no notify-send here; nothing to say
            Err(e) => notes.push(format!("desktop notification failed: {e}")),
        }
    }
    notes
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(url)
        .to_string()
}

/// POST the event. An ntfy topic gets the plain-text shape it renders best;
/// anything else gets JSON — the shape a webhook receiver expects.
async fn post(url: &str, event: &Event) -> anyhow::Result<()> {
    // The same rule as `sinks::client_for`, shared via `httpc` (S-L3).
    // This used to be `Client::new(true, …)` — plain HTTP allowed
    // unconditionally, to anywhere — while its two siblings refused it to a
    // routable address and said why in a comment. The body carries the job
    // id, the node name, the command and the exit status; a webhook is an
    // operator's own URL, so `http://` to something on their own network is
    // fine and `http://` across the internet is a choice made silently.
    let plain = url.starts_with("http://");
    if plain && !mesh_core::httpc::is_private_host(mesh_core::httpc::host_of(url)) {
        anyhow::bail!(
            "{url} is plain HTTP on a public address. A notification carries the job id, the \
             node name and the command; use https://, or point at a destination on your own \
             network."
        );
    }
    let client = mesh_core::httpc::Client::new(plain, 64 * 1024)?;
    let is_ntfy = url.contains("ntfy");
    let (headers, body): (Vec<(String, String)>, Vec<u8>) = if is_ntfy {
        (
            vec![
                ("Title".into(), event.title.clone()),
                ("Content-Type".into(), "text/plain; charset=utf-8".into()),
                (
                    "Tags".into(),
                    if event.exit_code == Some(0) {
                        "white_check_mark".into()
                    } else {
                        "x".into()
                    },
                ),
            ],
            event.body.clone().into_bytes(),
        )
    } else {
        (
            vec![("Content-Type".into(), "application/json".into())],
            serde_json::to_vec(event)?,
        )
    };
    let resp = client.post_json(url, &headers, &body).await?;
    if resp.status >= 300 {
        anyhow::bail!("HTTP {}", resp.status);
    }
    Ok(())
}

/// Run the configured hook with the event in its environment.
async fn run_hook(command: &str, event: &Event) -> anyhow::Result<()> {
    let status = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            // The passphrase a person exported for this command is not the
            // hook's to read (SEC-23).
            .env_remove(mesh_core::keyfile::PASSPHRASE_ENV)
            .env("MESHWARE_NOTIFY_TITLE", &event.title)
            .env("MESHWARE_NOTIFY_BODY", &event.body)
            .env("MESHWARE_NOTIFY_JOB", &event.job)
            .env("MESHWARE_NOTIFY_STATE", &event.state)
            .env(
                "MESHWARE_NOTIFY_EXIT",
                event.exit_code.map(|c| c.to_string()).unwrap_or_default(),
            )
            .env("MESHWARE_NOTIFY_NODE", &event.node)
            .env("MESHWARE_NOTIFY_COMMAND", &event.command)
            .env("MESHWARE_NOTIFY_SECONDS", event.seconds.to_string())
            .stdin(std::process::Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("timed out after 30s"))??;
    if !status.success() {
        anyhow::bail!("exited {}", status.code().unwrap_or(-1));
    }
    Ok(())
}

/// `notify-send`, when there is one. `Ok(false)` = not installed, which is
/// every server and is not worth a line.
async fn desktop(event: &Event) -> anyhow::Result<bool> {
    let Ok(status) = tokio::process::Command::new("notify-send")
        .arg("--app-name=meshware")
        .arg(&event.title)
        .arg(&event.body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
    else {
        return Ok(false);
    };
    if !status.success() {
        anyhow::bail!("notify-send exited {}", status.code().unwrap_or(-1));
    }
    Ok(true)
}

/// The event for a finished job, worded the way the outcome line is.
pub fn job_event(
    job: &str,
    node: &str,
    command: &str,
    state: mesh_proto::JobState,
    exit_code: Option<i32>,
    seconds: u64,
) -> Event {
    let outcome = match (state, exit_code) {
        (mesh_proto::JobState::Succeeded, _) => "succeeded".to_string(),
        (_, Some(code)) => format!("exited {code}"),
        (s, None) => s.as_str().to_string(),
    };
    let dur = humantime::format_duration(Duration::from_secs(seconds)).to_string();
    Event {
        title: format!("meshware: {job} {outcome}"),
        body: format!("{command} on {node} — {outcome} after {dur}"),
        job: job.to_string(),
        state: state.as_str().to_string(),
        exit_code,
        node: node.to_string(),
        command: command.to_string(),
        seconds,
    }
}
