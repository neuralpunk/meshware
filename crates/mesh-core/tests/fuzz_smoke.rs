//! **Every fuzz target survives what it is built for** (SEC-22), on the
//! stable toolchain, on every test run.
//!
//! `fuzz/` drives these targets with coverage guidance on nightly; this is
//! the floor under that: real inputs of each kind, then a few thousand
//! mutations of them and some plain noise. A panic is reported with the
//! target and the input that caused it, so it can be added to `fuzz/`'s
//! corpus and fixed.
#![allow(clippy::unwrap_used)]

use std::panic::{AssertUnwindSafe, catch_unwind};

use mesh_core::fuzz;
use mesh_core::identity::Keypair;
use mesh_proto::{
    Capture, Hlc, IpcRequest, JobId, JobPriority, JobState, JobSummary, MeshId, NodeId, OpBody,
    OpEnvelope, Request, Response, UserId,
};

/// A small, seeded generator: the same inputs every run, so a failure
/// reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// One mutation of `seed`: flips, overwrites with boundary values,
/// insertions, deletions, truncation, splices of another seed.
fn mutate(rng: &mut Rng, seed: &[u8], other: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..=rng.below(4) {
        match rng.below(7) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
            1 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] = [0x00, 0xff, 0x7f, 0x80, 0x01][rng.below(5)];
            }
            2 => {
                let i = rng.below(v.len() + 1);
                let n = rng.below(8) + 1;
                let junk = rng.bytes(n);
                v.splice(i..i, junk);
            }
            3 if !v.is_empty() => {
                let i = rng.below(v.len());
                let n = rng.below(v.len() - i) + 1;
                v.drain(i..i + n);
            }
            4 => {
                let n = rng.below(v.len() + 1);
                v.truncate(n);
            }
            5 if !other.is_empty() => {
                let i = rng.below(v.len() + 1);
                let j = rng.below(other.len());
                let n = rng.below(other.len() - j) + 1;
                v.splice(i..i, other[j..j + n].iter().copied());
            }
            // A varint made huge: the length and count fields postcard
            // trusts are where decoders go wrong.
            _ if !v.is_empty() => {
                let i = rng.below(v.len());
                v.splice(i..i, [0xff, 0xff, 0xff, 0xff, 0x0f]);
            }
            _ => {}
        }
    }
    v
}

fn frame<T: serde::Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(msg).unwrap();
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend(body);
    out
}

fn summary(executor: NodeId) -> JobSummary {
    JobSummary {
        job_id: JobId::generate(),
        cmd_preview: "echo hi".into(),
        runtime: "bash".into(),
        submitter: executor,
        submitter_user: UserId([1; 32]),
        executor,
        state: JobState::Running,
        exit_code: None,
        created_at_micros: mesh_proto::now_micros(),
        started_at_micros: None,
        finished_at_micros: None,
        fanout: None,
        updated_hlc: Hlc::default(),
    }
}

fn seeds() -> Vec<(&'static str, Vec<Vec<u8>>)> {
    let key = Keypair::generate().unwrap();
    let node = key.node_id();
    let op = |body: &OpBody| {
        let bytes = postcard::to_stdvec(body).unwrap();
        let hlc = Hlc {
            wall_micros: 1,
            logical: 0,
        };
        OpEnvelope {
            author: node,
            seq: 1,
            hlc,
            signature: key
                .sign(&mesh_proto::op_signing_bytes_raw(&node, 1, &hlc, &bytes))
                .to_vec(),
            body: bytes,
        }
    };
    let bodies = [
        OpBody::JobUpsert {
            summary: summary(node),
        },
        OpBody::InviteConsume {
            invite_id: mesh_proto::InviteId([4; 8]),
            node_id: node,
        },
        OpBody::SecretRewrap {
            name: "db".into(),
            version: 1,
            wraps: vec![mesh_core::secrets::statement_marker(node, 2)],
        },
        OpBody::MemberUpdate {
            node_id: node,
            name: "alpha".into(),
            addrs: vec!["127.0.0.1:7777".into()],
            tags: vec!["gpu".into()],
            slots: 4,
        },
        OpBody::SourceSynced {
            name: "src".into(),
            commit: "abc".into(),
            bundle_id: mesh_proto::BundleId([5; 32]),
        },
        OpBody::SecretScopeSet {
            scope: Box::new(mesh_proto::SecretScope {
                name: "db".into(),
                nodes: "tag=db|name=x".into(),
                users: vec![UserId([1; 32])],
                roles: vec![mesh_proto::Role::Admin],
                by: UserId([1; 32]),
                at_micros: 1,
                signature: vec![0; 64],
            }),
        },
        OpBody::MeshReset {
            reset: mesh_proto::MeshReset {
                mesh_id: mesh_proto::MeshId([2; 16]),
                by: UserId([1; 32]),
                at_micros: 1,
                signature: vec![0; 64],
            },
        },
    ];
    let status = key.sign_request(&IpcRequest::Status).unwrap();
    let bound = key
        .sign_request_for(
            &IpcRequest::Jobs,
            &mesh_proto::Audience {
                mesh: MeshId([9; 16]),
                node,
            },
        )
        .unwrap();
    let manifest = mesh_proto::content::Manifest {
        files: vec![mesh_proto::content::FileEntry {
            path: "run.sh".into(),
            mode: 0o755,
            symlink: None,
            chunks: vec![],
            size: 0,
        }],
    };
    let identities = {
        let mut blob = Vec::new();
        for field in [b"ssh-ed25519".as_slice(), &[7u8; 32]] {
            blob.extend((field.len() as u32).to_be_bytes());
            blob.extend(field);
        }
        let mut out = 1u32.to_be_bytes().to_vec();
        out.extend((blob.len() as u32).to_be_bytes());
        out.extend(&blob);
        out.extend(4u32.to_be_bytes());
        out.extend(b"yubi");
        out
    };
    let dns = vec![
        0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0, 3, b'w', b'e', b'b', 7, b's', b'e', b'r',
        b'v', b'i', b'c', b'e', 4, b'm', b'e', b's', b'h', 0, 0, 1, 0, 1,
    ];
    let attestation = mesh_proto::HardwareAttestation {
        user_id: UserId([1; 32]),
        kind: mesh_proto::AttestKind::Piv,
        chain: vec![
            vec![0x30, 0x82, 0x01, 0x00, 0x30],
            vec![0x30, 0x03, 0x02, 0x01, 0x01],
        ],
        slot: "9c".into(),
        added_at_micros: 0,
        signature: vec![0; 64],
    };
    vec![
        (
            "wire_frame",
            vec![
                frame(&Request::Ping(mesh_proto::Ping {
                    nonce: 7,
                    sent_at_micros: 1,
                })),
                frame(&Response::Ok),
                frame(&bodies.iter().map(op).collect::<Vec<_>>()),
                frame(&Request::CachePublish {
                    entry: Box::new(mesh_proto::CacheEntry {
                        key: mesh_proto::ActionKey([1; 32]),
                        exit_code: Some(0),
                        stdout: None,
                        stdout_len: 0,
                        stderr: None,
                        stderr_len: 0,
                        outputs: vec![],
                        producer: node,
                        job: JobId::generate(),
                        epoch: 0,
                        hlc: Hlc::default(),
                        at_micros: 0,
                    }),
                }),
            ],
        ),
        (
            "ipc_envelope",
            vec![
                postcard::to_stdvec(&status).unwrap(),
                postcard::to_stdvec(&bound).unwrap(),
                postcard::to_stdvec(&IpcRequest::Parallel {
                    command: "echo {}".into(),
                    service: None,
                    inputs: vec![],
                    item_input: None,
                    items: vec!["a".into()],
                    on: None,
                    env: vec![],
                    timeout_secs: None,
                    chunk: None,
                    capture: Capture::Inline,
                    reduce: None,
                    reduce_on: None,
                })
                .unwrap(),
                postcard::to_stdvec(&IpcRequest::Run {
                    command: "true".into(),
                    on: None,
                    env: vec![],
                    timeout_secs: None,
                    fanout: None,
                    priority: JobPriority::Normal,
                    max_parallel: None,
                    halt_on_fail: None,
                })
                .unwrap(),
            ],
        ),
        (
            "op_envelope",
            bodies
                .iter()
                .map(|b| postcard::to_stdvec(&op(b)).unwrap())
                .chain([postcard::to_stdvec(&bodies.iter().map(op).collect::<Vec<_>>()).unwrap()])
                .collect(),
        ),
        (
            "op_apply",
            bodies
                .iter()
                .map(|b| postcard::to_stdvec(b).unwrap())
                .collect(),
        ),
        ("manifest", vec![postcard::to_stdvec(&manifest).unwrap()]),
        (
            "http_response",
            vec![
                b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 0".to_vec(),
                b"4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n".to_vec(),
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nX: y".to_vec(),
            ],
        ),
        (
            "jwt",
            vec![
                b"eyJhbGciOiJFUzI1NiIsImtpZCI6ImVjIn0.eyJpc3MiOiJodHRwczovL2lkcC5leGFtcGxlIiwiYXVkIjoibWVzaHdhcmUiLCJleHAiOjk5OTk5OTk5OTl9.c2ln".to_vec(),
                b"eyJhbGciOiJSUzI1NiIsImtpZCI6InJzYSJ9.e30.".to_vec(),
            ],
        ),
        (
            "attestation",
            vec![postcard::to_stdvec(&attestation).unwrap()],
        ),
        (
            "agent_reply",
            vec![
                identities,
                b"-----BEGIN SSH SIGNATURE-----\nU1NIU0lHAAAAAQAAADMAAAALc3NoLWVkMjU1MTkAAAAgOYX+nApb0Wtu+BNpzM5ctXC/TR\nJ5cvMtq5RTK3ilrr4AAAAQbWVzaHdhcmUtcmVsZWFzZQAAAAAAAAAGc2hhNTEyAAAAUwAA\nAAtzc2gtZWQyNTUxOQAAAEApYrIdyXMSB5fw7iwK0ajsTdMVtZ6wuzWxpTx1QRywkKUycC\nQ1eRQJw01dNk0A0MrfyJSMsTBJmA3V6Frbf0wH\n-----END SSH SIGNATURE-----".to_vec(),
            ],
        ),
        ("dns_query", vec![dns]),
    ]
}

#[test]
fn every_target_survives_seeds_mutations_and_noise() {
    const ROUNDS: usize = 1500;
    let seeds = seeds();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut failures = Vec::new();
    for (name, target) in fuzz::TARGETS {
        let own = &seeds
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no seeds for target {name}"))
            .1;
        let mut inputs: Vec<Vec<u8>> = own.clone();
        for _ in 0..ROUNDS {
            let seed = &own[rng.below(own.len())];
            let other = &seeds[rng.below(seeds.len())].1[0];
            inputs.push(mutate(&mut rng, seed, other));
        }
        for _ in 0..ROUNDS / 5 {
            let n = rng.below(512);
            inputs.push(rng.bytes(n));
        }
        for input in inputs {
            if catch_unwind(AssertUnwindSafe(|| target(&input))).is_err() {
                failures.push(format!("{name}: {}", hex(&input)));
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} input(s) panicked:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
