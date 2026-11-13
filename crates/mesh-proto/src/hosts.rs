//! The host inventory's records, **retired** (2026-09-18).
//!
//! The inventory — targets that never run meshnode, rendered as an Ansible
//! inventory for a job — was removed. What is left is the shape of the ops
//! it wrote (`HostPut`, `HostGroupPut`) and of the peer message it sent
//! (`Request::HostProbes`), kept so a log or a peer from before the removal
//! still decodes: postcard is positional. **Change nothing here.**

use serde::{Deserialize, Serialize};

use crate::UserId;

/// Prefix the inventory owned on the job environment, **still reserved**:
/// set only by an origin, refused from user-supplied env. A node from
/// before the removal materializes an inventory from these keys, so an
/// upgraded origin must go on refusing them from a user rather than hand
/// such a node one it did not build.
pub const INV_ENV_PREFIX: &str = "MESHWARE_INV_";

/// Is this an inventory-reserved job environment key?
pub fn is_inventory_env(key: &str) -> bool {
    key.starts_with(INV_ENV_PREFIX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionProfile {
    Ssh,
    Winrm,
    NetworkCli,
    HttpApi,
    Local,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CredentialKind {
    SshKey,
    Password,
    ApiToken,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostCredential {
    pub kind: CredentialKind,
    pub secret: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub name: String,
    pub address: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub platform: String,
    pub connection: ConnectionProfile,
    pub credential: Option<HostCredential>,
    pub become_credential: Option<HostCredential>,
    pub groups: Vec<String>,
    pub vars: Vec<(String, String)>,
    pub tags: Vec<String>,
    pub reachable_from: String,
    pub description: String,
    pub created_at_micros: u64,
    pub created_by: UserId,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostGroup {
    pub name: String,
    pub vars: Vec<(String, String)>,
    pub children: Vec<String>,
    pub description: String,
    pub created_at_micros: u64,
    pub created_by: UserId,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostHealth {
    Unknown,
    Reachable,
    Unreachable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostProbe {
    pub host: String,
    pub health: HostHealth,
    pub rtt_micros: Option<u64>,
    pub detail: String,
    pub at_micros: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostProbeReport {
    pub entries: Vec<HostProbe>,
    pub hlc: crate::Hlc,
}
