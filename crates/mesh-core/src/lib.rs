//! mesh-core — the meshware engine.
//!
//! Shared by the `meshnode` binary and Meshware Director. No CLI or UI
//! dependencies live here (MASTERPLAN §16).

pub mod accounting;
pub mod addrs;
pub mod api;
pub mod archive_read;
pub mod attest;
pub mod audit;
pub mod backpressure;
pub mod backup;
pub mod cache;
pub mod capacity;
pub mod cgroup;
pub mod compact;
pub mod config;
pub mod containers;
pub mod content;
pub mod content_sync;
pub mod daemon;
pub mod deploy;
pub mod deployments;
pub mod devices;
#[cfg(unix)]
pub mod doctor;
pub mod durable;
pub mod error;
pub mod facts;
// The build farm's compiler wrapper (`meshnode cc` / `<Director> cc`).
// In the engine rather than the CLI because *both* front ends have to be
// able to be the wrapper: on a machine with only Director installed there
// is no `meshnode` on PATH to put in `CC`.
pub mod dns;
pub mod dnssetup;
pub mod engines;
pub mod farm;
#[doc(hidden)]
pub mod fuzz;
pub mod gangs;
pub mod genesis;
pub mod gitsync;
pub mod gossip;
pub mod hardware;
pub mod history;
pub mod hlc;
pub mod http;
pub mod httpc;
pub mod identity;
pub mod instance_stats;
#[cfg(unix)]
pub mod ipc;
pub mod jobs;
pub mod keyfile;
/// Dispatch latency, the number MASTERPLAN bet #3 is about (N6 §12).
pub mod latency;
pub mod layout;
pub mod localtime;
pub mod membership;
pub mod meshsock;
pub mod object_sync;
pub mod objects;
pub mod observe;
pub mod oidc;
pub mod outputs;
pub mod pairing;
pub mod parallel;
pub mod qr;
pub mod quiet;
pub mod rbac;
pub mod reach;
pub mod registry;
pub mod relay;
pub mod release;
pub mod reset;
pub mod results;
pub mod retention;
pub mod runtime;
pub mod runtimes;
pub mod sandbox;
pub mod scheduler;
pub mod schedules;
pub mod script_lint;
pub mod script_spec;
pub mod secrets;
pub mod services;
pub mod sessions;
pub mod sinks;
pub mod sshsig;
pub mod state;
pub mod store;
pub mod tasks;
pub mod throttle;
pub mod transport;
pub mod upgrade_guard;
pub mod usage;
#[cfg(unix)]
pub mod usernode;
/// Which loops are alive and making progress (ENG-1, ENG-2).
pub mod vitals;
/// Dynamic volumes, provisioned through a plugin (N5 P2).
pub mod volumes;
pub mod wan;
pub mod wordpair;

pub use error::{CoreError, Result};
