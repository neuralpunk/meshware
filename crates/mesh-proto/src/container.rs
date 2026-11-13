//! The container runtime's reserved job-environment keys (N4).
//!
//! Everything a container needs beyond the image travels the way every
//! other resource requirement does — as `MESHWARE_TASK_*` entries — so
//! `service create`, a deployment block and an ad-hoc job all ask for it in
//! exactly the same words, and the executor has one thing to read.

/// Override the image's own ENTRYPOINT: `MESHWARE_TASK_COMMAND`.
///
/// Nomad's `config { command = … }`, and the same semantics: without it the
/// image runs whatever it was built to run, which is right for an image
/// built to be run and useless for a general-purpose base. `podman` and
/// `docker` both spell it `--entrypoint`.
pub const REQ_COMMAND: &str = "MESHWARE_TASK_COMMAND";

/// Arguments for it, as a JSON array: `MESHWARE_TASK_ARGS`.
///
/// **JSON, not a comma-separated list**, because an argument may contain a
/// comma, a space or a quote — `["-c", "echo hello, world"]` is an ordinary
/// thing to want, and a separator-delimited encoding cannot carry it. The
/// same reason [`crate::DeviceRequest`] travels as JSON in one reserved key.
///
/// These come *before* a service's `--key value` inputs, because they are
/// the command line and the inputs are parameters to it.
pub const REQ_ARGS: &str = "MESHWARE_TASK_ARGS";

/// The command override and its arguments, from a job's environment.
///
/// Returns `(command, args)`. Unparseable JSON yields no args rather than a
/// guess, the rule every reserved key in this file follows — and here the
/// stakes are the argument list of a process.
pub fn command_override(env: &[(String, String)]) -> (Option<String>, Vec<String>) {
    let get = |key: &str| {
        env.iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.trim().is_empty())
    };
    let command = get(REQ_COMMAND).map(|c| c.trim().to_string());
    let args = get(REQ_ARGS)
        .and_then(|raw| serde_json::from_str::<Vec<String>>(raw).ok())
        .unwrap_or_default();
    (command, args)
}

/// Render a command override into the reserved entries a job carries.
pub fn command_env(command: Option<&str>, args: &[String]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Some(c) = command.map(str::trim).filter(|c| !c.is_empty()) {
        out.push((REQ_COMMAND.to_string(), c.to_string()));
    }
    if !args.is_empty() {
        out.push((
            REQ_ARGS.to_string(),
            serde_json::to_string(args).unwrap_or_else(|_| "[]".into()),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_override_round_trips() {
        let env = command_env(Some("/bin/sh"), &["-c".into(), "echo hi".into()]);
        let (cmd, args) = command_override(&env);
        assert_eq!(cmd.as_deref(), Some("/bin/sh"));
        assert_eq!(args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    /// **JSON, not a separator.** An argument containing a comma, a space
    /// or a quote is an ordinary thing to want, and a delimited encoding
    /// cannot carry it.
    #[test]
    fn an_argument_may_contain_anything() {
        let tricky = vec![
            "-c".to_string(),
            "echo 'hello, world'".to_string(),
            "a\"b".to_string(),
            "".to_string(),
        ];
        let env = command_env(Some("/bin/sh"), &tricky);
        assert_eq!(command_override(&env).1, tricky);
    }

    /// Absent means "run what the image was built to run", which is the
    /// right default and the common case.
    #[test]
    fn no_override_is_the_default() {
        assert_eq!(command_override(&[]), (None, Vec::new()));
        assert!(command_env(None, &[]).is_empty());
        assert!(command_env(Some("   "), &[]).is_empty());
    }

    /// Junk yields no args rather than a guess — the rule every reserved
    /// key follows, and here the stakes are a process's argument list.
    #[test]
    fn unparseable_args_yield_nothing_rather_than_a_guess() {
        let env = vec![(REQ_ARGS.to_string(), "not json".to_string())];
        assert_eq!(command_override(&env).1, Vec::<String>::new());
    }
}
