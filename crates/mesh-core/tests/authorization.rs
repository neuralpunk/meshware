//! **Nobody the mesh does not know gets to do anything** (MASTERPLAN §7.1,
//! §7.2).
//!
//! Authorization on this system is a property of the *key that signed the
//! request*, never of the socket, the machine, or the OS account — which
//! is what makes "the CLI is on this box" and "this person may act" two
//! entirely separate questions. This file pins the second one down for the
//! whole request surface at once, rather than trusting that each of the
//! hundred-and-some handlers remembered its own gate.
//!
//! The exceptions are deliberate and are asserted *as* exceptions, because
//! an unenrolled person has to have exactly one door:
//!
//!   * `WhoAmI` — "which key am I, and does this mesh know it?". Answering
//!     it is how somebody learns what to send an admin. It discloses their
//!     own grants (none) and the node's own name, and nothing about the
//!     mesh's contents.
//!   * `EnrolStatus` / `EnrolSubmit` — knocking, and hearing whether
//!     anyone answered. Refusing these would make enrolment impossible for
//!     the only people who need it.

use mesh_core::identity::Keypair;
use mesh_proto::{IpcRequest, IpcResponse, JobPriority};

/// Every request whose whole shape is its name. Enumerated by hand and
/// checked against the enum by `every_unit_request_is_covered` below, so a
/// new one cannot be added without deciding which side of the line it is
/// on.
fn unit_requests() -> Vec<(&'static str, IpcRequest)> {
    vec![
        ("Status", IpcRequest::Status),
        ("IdentityShow", IpcRequest::IdentityShow),
        ("InviteBootstrap", IpcRequest::InviteBootstrap),
        ("Nodes", IpcRequest::Nodes),
        ("Users", IpcRequest::Users),
        ("Grants", IpcRequest::Grants),
        ("Secrets", IpcRequest::Secrets),
        ("SecretProviders", IpcRequest::SecretProviders),
        ("Jobs", IpcRequest::Jobs),
        // H-8's change signal. A stranger must not learn even *how much*
        // is happening on a mesh they are not in — a rising counter is a
        // side channel about activity, cheap though it is to answer.
        ("StateVersion", IpcRequest::StateVersion),
        ("Bundles", IpcRequest::Bundles),
        // M31 A2 shipped this without adding it here, so the coverage
        // assertion below had nothing to say about it. Gated on
        // `rbac::READ` in `handle_authed`, exactly as `Bundles` is.
        ("ContentStatus", IpcRequest::ContentStatus),
        ("Services", IpcRequest::Services),
        ("Top", IpcRequest::Top),
        ("Sources", IpcRequest::Sources),
        ("Objects", IpcRequest::Objects),
        ("HistoryStatus", IpcRequest::HistoryStatus),
        ("HistoryRuns", IpcRequest::HistoryRuns),
        ("BenchRun", IpcRequest::BenchRun),
        ("Backpressure", IpcRequest::Backpressure),
        ("FactCollectors", IpcRequest::FactCollectors),
        // Shipped after this list was written (M31 A2 and Y2/Y3), which is
        // what the coverage assertion at the bottom exists to catch.
        ("DiskReport", IpcRequest::DiskReport),
        ("AttestRoots", IpcRequest::AttestRoots),
        ("Sessions", IpcRequest::Sessions),
        ("SessionPolicies", IpcRequest::SessionPolicies),
        ("Enrolments", IpcRequest::Enrolments),
        ("Definitions", IpcRequest::Definitions),
        ("StateShow", IpcRequest::StateShow),
        ("Queue", IpcRequest::Queue),
        ("UpgradePlans", IpcRequest::UpgradePlans),
        ("ReleaseKeys", IpcRequest::ReleaseKeys),
        ("Releases", IpcRequest::Releases),
        ("Deployments", IpcRequest::Deployments),
        ("PlacementMap", IpcRequest::PlacementMap),
        // Gated like every read, and missing from this list since the phases
        // that added them (N1 P5, N3, N6) — which is what kept this test red.
        ("Schedules", IpcRequest::Schedules),
        ("Registry", IpcRequest::Registry),
        ("DnsStatus", IpcRequest::DnsStatus),
        ("ShedStatus", IpcRequest::ShedStatus),
        ("ClockAhead", IpcRequest::ClockAhead),
        ("Invites", IpcRequest::Invites),
        ("SecretScopes", IpcRequest::SecretScopes),
        ("CacheStats", IpcRequest::CacheStats),
        ("Totals", IpcRequest::Totals),
        ("Sinks", IpcRequest::Sinks),
        ("SinksResume", IpcRequest::SinksResume),
        ("RetentionShow", IpcRequest::RetentionShow),
        ("OpCensus", IpcRequest::OpCensus),
        ("ServiceAccounts", IpcRequest::ServiceAccounts),
        ("OidcProviders", IpcRequest::OidcProviders),
        // Retired with change control: payload-free names now, and a
        // stranger is refused before being told the feature is gone.
        ("PolicyCreate", IpcRequest::PolicyCreate),
        ("Policies", IpcRequest::Policies),
        ("PolicyShow", IpcRequest::PolicyShow),
        ("PolicyRemove", IpcRequest::PolicyRemove),
        ("PolicyCheck", IpcRequest::PolicyCheck),
        ("Approvals", IpcRequest::Approvals),
        ("ApprovalShow", IpcRequest::ApprovalShow),
        ("ApprovalDecide", IpcRequest::ApprovalDecide),
        ("ApprovalCancel", IpcRequest::ApprovalCancel),
        ("PolicyPresencePut", IpcRequest::PolicyPresencePut),
        ("PolicyPresences", IpcRequest::PolicyPresences),
        ("PresenceReadiness", IpcRequest::PresenceReadiness),
        ("UserTokenAdd", IpcRequest::UserTokenAdd),
        ("UserTokens", IpcRequest::UserTokens),
        ("UserTokenRevoke", IpcRequest::UserTokenRevoke),
        ("ApprovalDecidePresent", IpcRequest::ApprovalDecidePresent),
        // Retired with the host inventory.
        ("HostPut", IpcRequest::HostPut),
        ("HostImport", IpcRequest::HostImport),
        ("Hosts", IpcRequest::Hosts),
        ("HostShow", IpcRequest::HostShow),
        ("HostRemove", IpcRequest::HostRemove),
        ("HostGroupPut", IpcRequest::HostGroupPut),
        ("HostGroups", IpcRequest::HostGroups),
        ("HostGroupRemove", IpcRequest::HostGroupRemove),
        ("HostCheck", IpcRequest::HostCheck),
        ("HostInventory", IpcRequest::HostInventory),
        // Retired with quotas and the usage report.
        ("QuotaPut", IpcRequest::QuotaPut),
        ("QuotaRemove", IpcRequest::QuotaRemove),
        ("Quotas", IpcRequest::Quotas),
        ("Usage", IpcRequest::Usage),
        ("UsageRows", IpcRequest::UsageRows),
    ]
}

/// The rest of the surface, one representative per family: reads, writes,
/// content, secrets, audit, inventory, governance, day-2 and the streaming
/// submission path. Constructed rather than enumerated because these carry
/// payloads, and a request that will not build is not a request anybody
/// can send either.
fn payload_requests(stranger: &Keypair) -> Vec<(&'static str, IpcRequest)> {
    vec![
        (
            "TargetPreview",
            IpcRequest::TargetPreview {
                selector: "any".into(),
            },
        ),
        ("Devices", IpcRequest::Devices { node: None }),
        (
            "SecretRecipients",
            IpcRequest::SecretRecipients {
                name: "db".into(),
                nodes: None,
            },
        ),
        (
            "SecretScopeSet",
            IpcRequest::SecretScopeSet {
                scope: Box::new(mesh_proto::SecretScope {
                    name: "db".into(),
                    nodes: String::new(),
                    users: vec![],
                    roles: vec![],
                    by: stranger.user_id(),
                    at_micros: 0,
                    signature: vec![0; 64],
                }),
            },
        ),
        (
            "InviteRevoke",
            IpcRequest::InviteRevoke {
                invite_id: mesh_proto::InviteId::generate(),
                by: stranger.user_id(),
                signature: vec![0; 64],
            },
        ),
        ("Gangs", IpcRequest::Gangs { all: true }),
        (
            "Facts",
            IpcRequest::Facts {
                subject: String::new(),
                collector: None,
            },
        ),
        (
            "AuditQuery",
            IpcRequest::AuditQuery {
                user: None,
                action: None,
                since_micros: None,
                limit: 10,
            },
        ),
        (
            "ServiceAccountAdd",
            IpcRequest::ServiceAccountAdd {
                account: mesh_proto::ServiceAccount {
                    user_id: stranger.user_id(),
                    name: "ci".into(),
                    description: String::new(),
                    created_at_micros: 0,
                    created_by: stranger.user_id(),
                    expires_at_micros: None,
                    signature: Vec::new(),
                },
            },
        ),
        (
            "OidcProviderPut",
            IpcRequest::OidcProviderPut {
                provider: mesh_proto::OidcProvider {
                    name: "evil".into(),
                    issuer: "https://evil.example.com".into(),
                    client_id: "x".into(),
                    jwks_uri: "https://evil.example.com/keys".into(),
                    device_authorization_endpoint: "https://evil.example.com/d".into(),
                    token_endpoint: "https://evil.example.com/t".into(),
                    scopes: vec![],
                    groups_claim: "groups".into(),
                    group_roles: vec![],
                    configured_by: stranger.user_id(),
                    configured_at_micros: 0,
                    signature: Vec::new(),
                },
            },
        ),
        (
            "OidcProviderRemove",
            IpcRequest::OidcProviderRemove {
                name: "entra".into(),
                by: stranger.user_id(),
                at_micros: 0,
                signature: Vec::new(),
            },
        ),
        (
            "Plan",
            IpcRequest::Plan {
                on: None,
                service: None,
                env: Vec::new(),
                fanout: None,
            },
        ),
        (
            "Push",
            IpcRequest::Push {
                path: "/tmp/nowhere".into(),
            },
        ),
        (
            "TagsUpdate",
            IpcRequest::TagsUpdate {
                add: vec!["mine".into()],
                remove: Vec::new(),
            },
        ),
        // Labelling somebody else's machine, and reading what it has been
        // labelled. Both are refused to a stranger for the same reason the
        // rest of this list is: the estate's own division of itself is not
        // public, and being able to *write* it from outside would let
        // anyone put `tag=prod` on a box and collect whatever runs there.
        (
            "NodeTags",
            IpcRequest::NodeTags {
                assignment: mesh_proto::NodeTags {
                    node_id: mesh_proto::NodeId([7u8; 32]),
                    tags: vec!["prod".into()],
                    by: stranger.user_id(),
                    at_micros: 0,
                    signature: Vec::new(),
                },
            },
        ),
        (
            "AssignedTags",
            IpcRequest::AssignedTags {
                node_id: mesh_proto::NodeId([7u8; 32]),
            },
        ),
        (
            "Backup",
            IpcRequest::Backup {
                path: "/tmp/nowhere.tar".into(),
            },
        ),
        (
            "StateCompact",
            IpcRequest::StateCompact {
                horizon_secs: None,
                dry_run: true,
            },
        ),
        (
            "DrainTimed",
            IpcRequest::DrainTimed {
                until_micros: None,
                reason: "because".into(),
                move_pending: false,
            },
        ),
        (
            "JobsClear",
            IpcRequest::JobsClear {
                before_micros: mesh_proto::now_micros(),
                by: stranger.user_id(),
                signature: vec![0u8; 64],
            },
        ),
        (
            "RetentionReset",
            IpcRequest::RetentionReset {
                at_micros: mesh_proto::now_micros(),
                signature: vec![0u8; 64],
            },
        ),
        (
            "SinkRemove",
            IpcRequest::SinkRemove {
                name: "warehouse".into(),
                signature: vec![0u8; 64],
            },
        ),
        (
            "MeshReset",
            IpcRequest::MeshReset {
                reset: mesh_proto::MeshReset {
                    mesh_id: mesh_proto::MeshId([0; 16]),
                    by: stranger.user_id(),
                    at_micros: 0,
                    signature: vec![0; 64],
                },
            },
        ),
    ]
}

/// Every unit-shaped variant of `IpcRequest` is either refused below or is
/// one of the two doors. Kept honest by reading the enum's own source: a
/// variant added without a decision fails here rather than shipping open.
#[test]
fn every_unit_request_is_covered() {
    let src = include_str!("../../mesh-proto/src/ipc.rs");
    let body = src
        .split_once("pub enum IpcRequest {")
        .expect("the enum")
        .1
        .split_once("\n}\n")
        .expect("its end")
        .0;
    let declared: Vec<&str> = body
        .lines()
        .filter_map(|l| {
            let t = l.strip_prefix("    ")?;
            let name = t.strip_suffix(',')?;
            (!name.is_empty()
                && name.chars().next().is_some_and(char::is_uppercase)
                && name.chars().all(|c| c.is_ascii_alphanumeric()))
            .then_some(name)
        })
        .collect();
    assert!(
        declared.len() > 30,
        "the enum could not be read — this test would then pass vacuously ({} found)",
        declared.len()
    );
    let covered: Vec<&str> = unit_requests().iter().map(|(n, _)| *n).collect();
    // The doors an unenrolled person must be able to knock on. `EnrolSubmit`
    // and the two `OidcLogin*` verbs carry payloads and so are not in this
    // enum scan; they are asserted open, and asserted harmless, by
    // `the_enrolment_doors_stay_open`.
    let doors = ["WhoAmI", "EnrolStatus"];
    for name in declared {
        assert!(
            covered.contains(&name) || doors.contains(&name),
            "IpcRequest::{name} is neither refused for strangers nor a declared \
             enrolment door — decide which, in tests/authorization.rs"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_is_refused_every_command() {
    let founder = mesh_testkit::spawn_founder("nim", "authz-mesh").await;
    let stranger = Keypair::generate().expect("a key nobody has ever seen");

    let mut all = unit_requests();
    all.extend(payload_requests(&stranger));

    for (name, req) in all {
        let resp = mesh_testkit::ipc_as(&founder, &stranger, req).await;
        assert!(
            matches!(resp, IpcResponse::Denied { .. }),
            "IpcRequest::{name} answered a key the mesh has never seen: {resp:?}"
        );
        if let IpcResponse::Denied { message } = resp {
            assert!(
                message.contains("unknown user"),
                "IpcRequest::{name} refused, but not as an unknown user: {message}"
            );
        }
    }
}

/// The streaming half of the surface goes through a different dispatch, so
/// it gets its own assertion rather than an argument that it must be fine.
#[tokio::test(flavor = "multi_thread")]
async fn a_stranger_cannot_submit_work() {
    let founder = mesh_testkit::spawn_founder("nim", "authz-mesh").await;
    let stranger = Keypair::generate().expect("key");

    let outcome = mesh_testkit::run_to_end(
        &founder,
        &stranger,
        IpcRequest::Run {
            command: "touch /tmp/a-stranger-was-here".into(),
            on: Some("any".into()),
            env: Vec::new(),
            timeout_secs: Some(5),
            fanout: None,
            priority: JobPriority::Normal,
            max_parallel: None,
            halt_on_fail: None,
        },
    )
    .await;
    assert!(
        matches!(outcome, mesh_testkit::RunOutcome::Denied(_)),
        "a key the mesh has never seen was not refused: {outcome:?}"
    );
    assert!(
        !std::path::Path::new("/tmp/a-stranger-was-here").exists(),
        "the command actually ran"
    );
}

/// …and the doors are open, because a mesh nobody can ask to join is a
/// mesh with one member for ever.
#[tokio::test(flavor = "multi_thread")]
async fn the_enrolment_doors_stay_open() {
    let founder = mesh_testkit::spawn_founder("nim", "authz-mesh").await;
    let stranger = Keypair::generate().expect("key");

    let resp = mesh_testkit::ipc_as(&founder, &stranger, IpcRequest::WhoAmI).await;
    let IpcResponse::WhoAmI(me) = resp else {
        panic!("whoami must answer an unenrolled person: {resp:?}");
    };
    assert_eq!(me.user_id, stranger.user_id());
    assert!(!me.registered, "a stranger is not a member");
    assert!(me.roles.is_empty(), "a stranger holds no role");
    assert!(me.grants.is_empty(), "a stranger holds no grant");

    let resp = mesh_testkit::ipc_as(&founder, &stranger, IpcRequest::EnrolStatus).await;
    assert!(
        matches!(resp, IpcResponse::EnrolStatus { .. }),
        "enrolment status must answer the person asking to enrol: {resp:?}"
    );

    // E1-P5: signing in is a third door, for the same reason. A person
    // logging in through the company's identity provider is by definition
    // not yet a user, so refusing them would make the feature unreachable
    // by everybody it is for.
    //
    // What matters is that it is open **and harmless**. This mesh has no
    // provider configured, so the honest answer is "no such provider" — and
    // crucially *not* "permission denied", which is what a stranger gets
    // from every other verb. That distinction is the assertion.
    let resp = mesh_testkit::ipc_as(
        &founder,
        &stranger,
        IpcRequest::OidcLoginBegin {
            provider: "entra".into(),
        },
    )
    .await;
    match resp {
        IpcResponse::Error { message } => assert!(
            message.contains("no identity provider"),
            "login should fail for want of a provider, not for want of a grant: {message}"
        ),
        other => panic!("login must not be refused on authorization: {other:?}"),
    }

    // And the door discloses nothing: a stranger still cannot read which
    // providers a mesh believes, which is mesh configuration.
    let resp = mesh_testkit::ipc_as(&founder, &stranger, IpcRequest::OidcProviders).await;
    assert!(
        matches!(resp, IpcResponse::Denied { .. } | IpcResponse::Error { .. }),
        "listing providers is not a door: {resp:?}"
    );
}
