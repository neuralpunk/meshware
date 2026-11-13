//! Content-addressed bundles and the service model (MASTERPLAN §8).
//!
//! A bundle is a directory-tree snapshot: each file is split into chunks
//! (BLAKE3-hashed), and the manifest is the ordered list of files with their
//! chunk hashes. The BundleId is the BLAKE3 hash of the canonical manifest
//! encoding, so identical trees produce identical ids everywhere.

use serde::{Deserialize, Serialize};

use crate::{BundleId, ChunkHash, UserId};

/// One file within a bundle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    /// Path relative to the bundle root, forward-slash separated.
    pub path: String,
    /// Unix mode bits (permission-relevant part); execute bit is preserved.
    pub mode: u32,
    /// If set, this entry is a symlink to the given target (mode/chunks unused).
    pub symlink: Option<String>,
    /// Content chunks in order (empty for empty files and symlinks).
    pub chunks: Vec<ChunkHash>,
    /// Total file size in bytes.
    pub size: u64,
}

/// The full manifest of a bundle. Files are sorted by path for a canonical
/// encoding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub files: Vec<FileEntry>,
}

impl Manifest {
    /// Canonical byte encoding used to compute the BundleId.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        // postcard is deterministic given sorted files.
        postcard::to_stdvec(self).unwrap_or_default()
    }

    /// The BundleId = BLAKE3 of the canonical encoding.
    pub fn bundle_id(&self) -> BundleId {
        BundleId(*blake3::hash(&self.canonical_bytes()).as_bytes())
    }

    /// Every distinct chunk referenced, deduplicated, in first-seen order.
    pub fn distinct_chunks(&self) -> Vec<ChunkHash> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for f in &self.files {
            for c in &f.chunks {
                if seen.insert(*c) {
                    out.push(*c);
                }
            }
        }
        out
    }

    pub fn total_size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Check every entry before anything is written to disk (S2).
    ///
    /// A bundle id is the hash of the manifest, so a hostile manifest is a
    /// *valid* bundle with a *correct* id — content addressing proves the
    /// manifest is the one that was asked for, never that it is safe. The
    /// only thing standing between a crafted file list and
    /// `dest.join("../../etc/cron.d/backdoor")` is this.
    ///
    /// Called where a manifest arrives (`ContentStore::store_manifest`) and
    /// again where it is unpacked (`ContentStore::materialize`), because
    /// ingestion is not the only producer and the write is the step that
    /// actually hurts.
    pub fn validate(&self) -> Result<(), String> {
        for f in &self.files {
            check_entry_path(&f.path)?;
            if let Some(target) = &f.symlink {
                check_symlink_target(&f.path, target)?;
            }
        }
        Ok(())
    }
}

/// Is this a path a bundle may contain?
///
/// Relative, non-empty, no `.` or `..` component, no root or drive prefix, no
/// NUL, no empty component. Deliberately lexical: `Path::join` with an
/// absolute path *discards* the base, and `..` walks out of it, and both are
/// decided before any filesystem call — so there is no window in which a
/// half-created tree exists.
///
/// Backslash is treated as a separator regardless of platform. The field is
/// documented as forward-slash separated, so a backslash in it is either a
/// literal one in a filename (which the tree that produced it can live
/// without) or an attempt to be read differently on Windows than here.
pub fn check_entry_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("bundle entry with an empty path".into());
    }
    if path.contains('\0') {
        return Err(format!("bundle entry path contains NUL: {path:?}"));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("bundle entry path is absolute: {path:?}"));
    }
    // `C:\...`, `C:/...`, and the bare drive-relative `C:x` — all of which
    // `Path::join` treats as rooted on Windows.
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(format!("bundle entry path has a drive prefix: {path:?}"));
    }
    for part in path.split(['/', '\\']) {
        match part {
            "" => {
                return Err(format!(
                    "bundle entry path has an empty component: {path:?}"
                ));
            }
            "." | ".." => {
                return Err(format!(
                    "bundle entry path has a relative component: {path:?}"
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Does this relative path stay inside the directory it will be joined to?
///
/// Looser than [`check_entry_path`] and for a different job. A bundle
/// *entry* is a stored file name, where `.` is meaningless and refusing it
/// costs nothing; a service's `working_dir` is something a person writes,
/// and `.` is the ordinary way to write "here" — a `map` sends
/// exactly that. The invariant is the same and it is the only one that
/// matters: the path may not climb out of the root, and it may not be
/// rooted somewhere else.
///
/// An empty path is contained — it names the root itself — and callers that
/// need a non-empty one say so in their own words.
pub fn check_contained_path(what: &str, path: &str) -> Result<(), String> {
    if path.contains('\0') {
        return Err(format!("{what} contains NUL"));
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(format!("{what} must be relative to the bundle: {path:?}"));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(format!("{what} has a drive prefix: {path:?}"));
    }
    let mut depth = 0usize;
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("{what} points outside the bundle: {path:?}"))?;
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

/// May this symlink be created inside a bundle checkout?
///
/// Only if it still points inside the bundle when resolved. The target is
/// walked lexically against the link's own parent directory: an absolute
/// target is refused outright, and a relative one may not climb above the
/// root. That is enough to make chains safe too — every link in a chain is
/// checked by this same rule, so a contained link can only ever point at
/// another contained path.
///
/// This is the half that compounds the path check rather than duplicating
/// it: `fs::write` **follows** symlinks, and manifest order is whatever the
/// publisher chose, so `link -> /etc` followed by `link/passwd` would write
/// through it.
pub fn check_symlink_target(link_path: &str, target: &str) -> Result<(), String> {
    if target.is_empty() {
        return Err(format!("bundle symlink {link_path:?} has an empty target"));
    }
    if target.contains('\0') {
        return Err(format!("bundle symlink {link_path:?} target contains NUL"));
    }
    if target.starts_with('/') || target.starts_with('\\') {
        return Err(format!(
            "bundle symlink {link_path:?} points outside the bundle: {target:?}"
        ));
    }
    let bytes = target.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(format!(
            "bundle symlink {link_path:?} target has a drive prefix: {target:?}"
        ));
    }
    // Depth of the link's own directory, from the components already proven
    // safe by `check_entry_path`.
    let mut depth = link_path.split(['/', '\\']).count().saturating_sub(1);
    for part in target.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                if depth == 0 {
                    return Err(format!(
                        "bundle symlink {link_path:?} points outside the bundle: {target:?}"
                    ));
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

/// Bundle metadata announced into replicated state (not the manifest bytes,
/// which are fetched on demand).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BundleMeta {
    pub bundle_id: BundleId,
    pub total_size: u64,
    pub file_count: u32,
    pub chunk_count: u32,
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// The execution kind of a service (MASTERPLAN §9.2). bash and exec need no
/// dependency management; python and ansible build cached environments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Runtime {
    Bash,
    Python,
    Ansible,
    Exec,
    /// An OCI container, run by a subprocess plugin (N4).
    ///
    /// **Appended, never inserted.** `Runtime` is a field of
    /// [`crate::ServiceVersion`], which is a signed op body, and postcard
    /// encodes an enum by variant index — so a variant added anywhere but
    /// the end would re-read every signed service version in every existing
    /// data dir as the wrong kind.
    ///
    /// An older node has no code for this variant and cannot decode a
    /// service version that uses it, which is the right degradation: it
    /// simply has no such service, rather than running the image with a
    /// shell.
    ///
    /// The image reference lives in
    /// [`ServiceVersion::entrypoint`](crate::ServiceVersion) —
    /// `docker.io/library/nginx:1.25` — because "what to run" is exactly
    /// what that field means for every other runtime. The bundle, when
    /// there is one, is the *code*, mounted into the image.
    Container,
}

impl Runtime {
    pub fn as_str(&self) -> &'static str {
        match self {
            Runtime::Bash => "bash",
            Runtime::Python => "python",
            Runtime::Ansible => "ansible",
            Runtime::Exec => "exec",
            Runtime::Container => "container",
        }
    }

    /// Does this runtime execute through a plugin rather than in process?
    ///
    /// The question `sandbox_class`, `doctor` and the entrypoint checks all
    /// need, asked once rather than matched on in four places.
    pub fn is_plugin(&self) -> bool {
        matches!(self, Runtime::Container)
    }

    pub fn parse(s: &str) -> Option<Runtime> {
        match s {
            "bash" => Some(Runtime::Bash),
            "python" => Some(Runtime::Python),
            "ansible" => Some(Runtime::Ansible),
            "exec" => Some(Runtime::Exec),
            "container" | "oci" | "docker" | "podman" => Some(Runtime::Container),
            _ => None,
        }
    }
}

/// An immutable service version definition, stored in replicated state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServiceVersion {
    pub name: String,
    pub version: u32,
    pub runtime: Runtime,
    pub bundle_id: BundleId,
    /// Working directory within the bundle (forward-slash, "" = root).
    pub working_dir: String,
    /// Entry point relative to `working_dir` (playbook / script / binary).
    pub entrypoint: String,
    /// Optional JSON Schema (2020-12) for inputs, as a JSON string. Inline in
    /// M2; shareable named contracts are a later refinement.
    pub contract: Option<String>,
    /// Static environment variables applied to every run.
    pub env: Vec<(String, String)>,
    pub description: String,
    pub tags: Vec<String>,
    pub created_at_micros: u64,
    /// The user who registered this version (a developer or admin).
    pub created_by: UserId,
    /// Set when this version was auto-bumped by a Git source watcher
    /// (MASTERPLAN §8.1). Such versions carry an *empty* signature: they are
    /// authorized by the source record's user signature instead — the merge
    /// rule checks that `created_by` created the named source and that the
    /// source's bump list covers this service.
    pub source: Option<String>,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`],
    /// verified at merge time on every node.
    pub signature: Vec<u8>,
}

impl ServiceVersion {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-svc-version-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }
}

/// A fully-resolved plan the submitter ships to the executor. Carries
/// everything needed to run without a second state lookup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceExec {
    pub service_name: String,
    pub version: u32,
    pub runtime: Runtime,
    pub bundle_id: BundleId,
    pub working_dir: String,
    pub entrypoint: String,
    pub env: Vec<(String, String)>,
    /// Validated inputs (already checked against the contract by the submitter).
    pub inputs: Vec<(String, String)>,
    /// For the Ansible runtime: an inventory rendered from mesh members, or
    /// None to use a bundle-provided inventory / localhost.
    pub inventory: Option<MeshInventory>,
}

/// A script written **into the workflow** rather than published to the
/// Library (M10 §4.7).
///
/// The Library exists for code that is shared, versioned and reviewed. A
/// twelve-line Python step that reshapes one API response is none of those
/// things, and making somebody build a bundle, push it, and create a
/// service before they can write it is the reason that step ends up as an
/// unreadable `python3 -c` one-liner instead.
///
/// The executor writes `source` into the job's work directory and runs it
/// through the *same* [`crate::Runtime`] machinery a service uses — so a
/// Python step gets the same cached, dependency-isolated virtualenv, keyed
/// by the hash of its own `requirements`. No bundle, no Library entry, no
/// second execution path to keep in step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineScript {
    /// `Bash` or `Python`. `Ansible` and `Exec` need a bundle on disk and
    /// are refused at parse time rather than failing on the executor.
    pub runtime: Runtime,
    pub source: String,
    /// `requirements.txt` for the Python runtime, verbatim. Empty means no
    /// dependencies, which is the fast path: no install, no cache miss.
    pub requirements: String,
    /// `key=value` inputs, passed as `--key value` argv — the same
    /// convention a service gets, so one habit covers both.
    pub inputs: Vec<(String, String)>,
}

impl InlineScript {
    /// The filename the source is written to. The extension matters: it is
    /// what makes a traceback, an editor, and `file` all agree about what
    /// they are looking at.
    pub fn entrypoint(&self) -> &'static str {
        match self.runtime {
            Runtime::Python => "step.py",
            _ => "step.sh",
        }
    }

    /// One line for a table or a canvas node label.
    pub fn preview(&self) -> String {
        let first = self
            .source
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("");
        let lines = self.source.lines().count();
        if first.is_empty() {
            return format!("{} · empty", self.runtime.as_str());
        }
        let head: String = first.chars().take(48).collect();
        if lines > 1 {
            format!("{head} … {lines} lines")
        } else {
            head
        }
    }
}

// ---------------------------------------------------------------------------
// Ansible invocation flags
// ---------------------------------------------------------------------------
//
// A play's `--limit`, `--tags` and `--check` are decided on the **origin**,
// where the definition and the run's inputs both are, and they reach the
// executor the way an inventory already does: as reserved environment, read
// by the runtime builder rather than by the play. Putting them on
// [`ServiceExec`] instead would have changed a struct that crosses the wire
// inside a job spec, which is a protocol break for every peer that has not
// been upgraded — for three optional strings.

/// Prefix for every Ansible invocation flag. Reserved, so a step cannot set
/// one by hand and talk its way past the `--limit` an operator chose.
pub const ANSIBLE_ENV_PREFIX: &str = "MESHWARE_ANSIBLE_";
/// `--limit <pattern>`.
pub const ANSIBLE_LIMIT: &str = "MESHWARE_ANSIBLE_LIMIT";
/// `--tags <names>`.
pub const ANSIBLE_TAGS: &str = "MESHWARE_ANSIBLE_TAGS";
/// `--check`, when set to `1`. Ansible's own dry run: it connects, and it
/// reports what it would change, but it changes nothing.
pub const ANSIBLE_CHECK: &str = "MESHWARE_ANSIBLE_CHECK";
/// `--forks <n>`: how many hosts the play talks to at once (M25 §4.2).
///
/// **This is the only dial in the product that bounds pressure on the
/// managed estate rather than on the mesh.** Everything else — `[limits]
/// slots`, resource admission, the memory gate — protects the node doing
/// the work. None of it protects the switch on the other end of the SSH
/// session, and a node with 32 slots running a play across 400 devices
/// will open as many sessions as Ansible's default lets it, which is a
/// number nobody in the mesh chose.
///
/// Absent means Ansible's own default (5), which is deliberate: a play
/// that never said how hard to push should not start pushing harder
/// because it moved onto a bigger node.
pub const ANSIBLE_FORKS: &str = "MESHWARE_ANSIBLE_FORKS";

/// Never fork more than this, whatever a definition asks for.
///
/// A guard against the typo, not a policy: `forks: 5000` is a fork bomb on
/// the executor before it is a denial of service on the estate, and one
/// digit is all it takes. Real fleets run at tens; the ceiling is far
/// enough above that to never be in anybody's way.
pub const MAX_ANSIBLE_FORKS: u32 = 500;

/// Read a `forks` value as a count, clamped to [`MAX_ANSIBLE_FORKS`].
///
/// `None` for anything that is not a positive count — an empty string, a
/// zero, a template nobody bound — because the honest answer to "how many
/// at once" is then *Ansible's default*, not zero and not unbounded.
pub fn ansible_forks(value: &str) -> Option<u32> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|n| *n > 0)
        .map(|n| n.min(MAX_ANSIBLE_FORKS))
}

/// A mesh-native Ansible inventory: members rendered as hosts (MASTERPLAN §9.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshInventory {
    /// (hostname, ip) pairs; the executor writes these into an inventory file.
    pub hosts: Vec<(String, String)>,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod path_safety_tests {
    use super::*;

    fn entry(path: &str, symlink: Option<&str>) -> FileEntry {
        FileEntry {
            path: path.into(),
            mode: 0o644,
            symlink: symlink.map(str::to_string),
            chunks: Vec::new(),
            size: 0,
        }
    }

    #[test]
    fn ordinary_paths_are_fine() {
        for p in [
            "hello.txt",
            "sub/data.bin",
            "a/b/c/d.yaml",
            "weird name with spaces.sh",
            "..leading-dots-are-not-a-component",
            "dir/..hidden",
        ] {
            check_entry_path(p).unwrap_or_else(|e| panic!("{p:?} should be allowed: {e}"));
        }
    }

    #[test]
    fn escaping_paths_are_refused() {
        for p in [
            "",
            "/etc/passwd",
            "/home/meshnode/.ssh/authorized_keys",
            "../../../../etc/cron.d/backdoor",
            "a/../../b",
            "./a",
            "a//b",
            "C:/windows/system32/x",
            "C:x",
            "back\\slash\\..\\..\\out",
            "\\\\server\\share",
        ] {
            assert!(check_entry_path(p).is_err(), "{p:?} should be refused");
        }
    }

    /// S-L8: `working_dir` and `entrypoint` may not climb out of the
    /// checkout — and `.` is not climbing out, it is the checkout.
    #[test]
    fn a_contained_path_may_say_here_but_not_above() {
        for ok in [
            "",
            ".",
            "./",
            "src",
            "./src",
            "a/./b",
            "a/b/../c",
            "deep/er/still",
        ] {
            check_contained_path("working_dir", ok)
                .unwrap_or_else(|e| panic!("{ok:?} should be contained: {e}"));
        }
        for bad in [
            "..",
            "../outside",
            "a/../..",
            "/etc",
            "/etc/passwd",
            "C:/windows",
            "a\0b",
        ] {
            assert!(
                check_contained_path("working_dir", bad).is_err(),
                "{bad:?} should be refused"
            );
        }
    }

    #[test]
    fn a_symlink_may_point_within_the_bundle() {
        check_entry_path("bin/tool").unwrap();
        check_symlink_target("bin/tool", "../lib/tool.sh").unwrap();
        check_symlink_target("bin/tool", "./sibling").unwrap();
        check_symlink_target("a/b/c", "../../d").unwrap();
        check_symlink_target("top", "sub/thing").unwrap();
    }

    #[test]
    fn a_symlink_may_not_point_out_of_it() {
        for (link, target) in [
            ("link", "/etc"),
            ("link", ".."),
            ("link", "../outside"),
            ("a/b", "../../../etc/shadow"),
            ("link", "C:/windows"),
            ("link", ""),
        ] {
            assert!(
                check_symlink_target(link, target).is_err(),
                "{link:?} -> {target:?} should be refused"
            );
        }
    }

    #[test]
    fn the_manifest_gate_catches_both_halves() {
        // The zip-slip shape: a valid bundle id over a hostile file list.
        let hostile = Manifest {
            files: vec![entry("../../../../etc/cron.d/backdoor", None)],
        };
        assert!(hostile.validate().is_err());
        // And the two-entry variant that writes *through* a link.
        let two_step = Manifest {
            files: vec![entry("link", Some("/etc")), entry("link/passwd", None)],
        };
        assert!(two_step.validate().is_err());
        // An honest one still passes.
        let ok = Manifest {
            files: vec![entry("run.sh", None), entry("bin/tool", Some("../run.sh"))],
        };
        assert!(ok.validate().is_ok());
    }
}
