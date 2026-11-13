//! `meshnode reset`: the founder dissolves the mesh.
//!
//! Every node leaves: it stops its work, passes the reset on, moves its mesh
//! data aside and waits to be adopted into a new mesh (`mesh_core::reset`).
//! Nothing about that can be taken back from inside the mesh, because
//! afterwards there is no mesh — so this verb is built to be hard to reach
//! by accident and impossible to reach from a script:
//!
//! - **The founder's key, nobody else's.** Checked here so the refusal is
//!   immediate and says which key would do, again by the node before it
//!   publishes, and again by every node against its own genesis.
//! - **A person at a terminal.** No `--yes`, no JSON, and stdin must be a
//!   terminal. The confirmation is the mesh's name and a code drawn fresh
//!   each run, so neither a pipe nor a remembered answer can supply it.
//! - **The local socket only.** The node refuses the request over the HTTP
//!   API and from a job's credential.

use std::io::{IsTerminal as _, Write as _};

use anyhow::{Context as _, Result, anyhow, bail};
use mesh_proto::{
    IpcRequest, IpcResponse, Liveness, MeshReset, MeshResetOutcome, MeshResetReport, NodeRow,
    UserId, read_frame,
};

use crate::{Ctx, term};

pub async fn run(ctx: &Ctx) -> Result<i32> {
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        bail!(
            "`meshnode reset` asks a person at a terminal to confirm, and cannot be answered \
             from a pipe or a script. Run it in a terminal on the founder's machine."
        );
    }
    let genesis = mesh_core::genesis::Genesis::load(&ctx.config.data_dir)
        .context("reading this node's genesis")?
        .ok_or_else(|| anyhow!("this machine is not in a mesh, so there is nothing to reset"))?;
    let founder = UserId(genesis.body.founder_user);
    if ctx.user_id() != founder {
        bail!(not_the_founder(ctx, founder));
    }
    let mesh_name = genesis.body.mesh_name.clone();

    let IpcResponse::Nodes(rows) = ctx.ipc(IpcRequest::Nodes).await? else {
        bail!("the node did not answer with its members");
    };
    let members: Vec<&NodeRow> = rows.iter().filter(|r| !r.revoked && !r.left).collect();
    preview(
        &mesh_name,
        &genesis.body.mesh_id.to_display_string(),
        &members,
    );

    let phrase = format!("{mesh_name} {}", confirmation_code()?);
    let p = term::err();
    eprintln!("To confirm, type:  {}", p.bold(&phrase));
    eprint!("> ");
    let _ = std::io::stderr().flush();
    let mut typed = String::new();
    std::io::stdin()
        .read_line(&mut typed)
        .context("reading the confirmation")?;
    if typed.trim() != phrase {
        eprintln!("That does not match. Nothing was reset.");
        return Ok(1);
    }

    let mut reset = MeshReset {
        mesh_id: genesis.body.mesh_id,
        by: ctx.user_id(),
        at_micros: mesh_proto::now_micros(),
        signature: Vec::new(),
    };
    reset.signature = ctx.sign(&reset.signing_bytes())?;
    eprintln!();
    eprintln!("Resetting {mesh_name}, and telling every node…");
    let mut stream = ctx.stream(IpcRequest::MeshReset { reset }).await?;
    let report = loop {
        match read_frame(&mut stream).await {
            Ok(IpcResponse::MeshReset(report)) => break *report,
            Ok(IpcResponse::Error { message }) => bail!("{message}"),
            Ok(IpcResponse::Denied { message }) => bail!("permission denied: {message}"),
            Ok(_) => continue,
            Err(mesh_proto::ProtoError::Io(_)) => {
                bail!("the node stopped answering before it said who it had told")
            }
            Err(e) => return Err(e.into()),
        }
    };
    print_report(&report);
    Ok(0)
}

/// Which key would do, when the one signing is not the founder's.
///
/// The founder's key is often not the everyday one — binding a security key
/// sets the file key aside as `user.key.set-aside-<id>`, and that id is the
/// founder's on the machine that founded the mesh — so look there before
/// telling somebody to go and find it.
fn not_the_founder(ctx: &Ctx, founder: UserId) -> String {
    let mut msg = format!(
        "only the mesh's founder can reset it, and you are signing as {}. The founder is {}.",
        ctx.user_id().short(),
        founder.short()
    );
    let key = mesh_core::identity::cli_user_identity_path(&ctx.config.data_dir, None);
    let found = mesh_core::hardware::set_aside_keys(&key)
        .into_iter()
        .find(|(_, id)| *id == founder.short());
    match found {
        Some((path, _)) => msg.push_str(&format!(
            "\n\nThe founder's key is set aside here:\n  {}\n\nSign with it:\n  \
             meshnode --user-identity {} reset\n\n\
             If it has a passphrase, unlock it first:\n  \
             meshnode --user-identity {} identity unlock",
            path.display(),
            path.display(),
            path.display()
        )),
        None => msg.push_str(
            "\n\nSign with the founder's key file:\n  meshnode --user-identity <FILE> reset",
        ),
    }
    msg
}

fn preview(mesh_name: &str, mesh_id: &str, members: &[&NodeRow]) {
    let p = term::err();
    eprintln!(
        "{} {}  {}",
        p.bold("Reset"),
        p.bold(mesh_name),
        p.dim(mesh_id)
    );
    eprintln!();
    eprintln!(
        "{}",
        p.prose(
            "Every node leaves this mesh. Each one stops its jobs and services, moves the \
             mesh's data into `reset/` in its home — moved, not deleted — and waits to be \
             adopted into a new mesh. Its install, service, config and keys stay as they are."
        )
    );
    eprintln!();
    let width = members.iter().map(|m| m.name.len()).max().unwrap_or(0);
    for m in members {
        let (glyph, state) = if m.is_self {
            (p.good("●"), "this node".to_string())
        } else {
            match m.liveness {
                Liveness::Alive => (p.good("●"), "online".to_string()),
                Liveness::Suspect => (
                    p.warn("◐"),
                    "not answering — it may miss the reset".to_string(),
                ),
                Liveness::Dead => (p.bad("○"), "offline — it will miss the reset".to_string()),
            }
        };
        eprintln!(
            "  {glyph} {:<width$}  {}  {state}",
            m.name,
            p.dim(m.node_id.short())
        );
    }
    eprintln!();
    eprintln!(
        "{}",
        p.bad("This cannot be undone from inside the mesh: afterwards there is no mesh.")
    );
    eprintln!();
}

/// Four digits, fresh every run, so the answer cannot be remembered or
/// written into a script ahead of time.
fn confirmation_code() -> Result<u16> {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("system rng unavailable: {e}"))?;
    Ok(1000 + u16::from_le_bytes(bytes) % 9000)
}

fn print_report(report: &MeshResetReport) {
    let p = term::out();
    let width = report
        .peers
        .iter()
        .map(|peer| peer.name.len())
        .max()
        .unwrap_or(0);
    let mut missed: Vec<&str> = Vec::new();
    for peer in &report.peers {
        let (mark, said) = match &peer.outcome {
            MeshResetOutcome::Delivered => (p.good("✓"), "reset".to_string()),
            MeshResetOutcome::TooOld => (
                p.warn("!"),
                "has the reset, but its meshnode is too old to act on it".to_string(),
            ),
            MeshResetOutcome::Unreachable(why) => (p.bad("✗"), format!("not reached: {why}")),
        };
        if peer.outcome != MeshResetOutcome::Delivered {
            missed.push(&peer.name);
        }
        println!(
            "  {mark} {:<width$}  {}  {said}",
            peer.name,
            p.dim(peer.node_id.short())
        );
    }
    println!("  {} this node is leaving now", p.good("✓"));
    println!();
    println!("{} no longer exists.", p.bold(&report.mesh_name));
    println!();
    println!(
        "{}",
        p.prose(
            "Start the new mesh here: Director's Create a new mesh, or \
             `meshnode up --detach --new-mesh`. Then adopt the other machines into it: \
             Director's Add a machine, or `meshnode adopt <host>`."
        )
    );
    if !missed.is_empty() {
        println!();
        println!(
            "{}",
            p.prose(&format!(
                "Still in the old mesh: {}. Adopt {} with `--force` (Director: Replace an \
                 existing install), which moves the old mesh aside on that machine first.",
                missed.join(", "),
                if missed.len() == 1 { "it" } else { "them" }
            ))
        );
    }
}
