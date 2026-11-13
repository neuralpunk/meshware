//! Service and bundle business logic (MASTERPLAN §8.2, §8.3).
//!
//! - `push`: ingest a directory into the content store and announce it.
//! - `create_service`: register an immutable service version bound to a bundle,
//!   validating the contract schema up front.
//! - `resolve_service`: turn a service name + inputs into a `ServiceExec`,
//!   validating inputs against the contract before anything is scheduled.

use std::path::Path;
use std::sync::Arc;

use mesh_proto::{
    BundleId, MeshInventory, Runtime, ServiceExec, ServiceVersion, content::Manifest,
    service_head_signing_bytes, service_remove_signing_bytes,
};

use crate::content::ContentStore;
use crate::error::{CoreError, Result};
use crate::identity::Keypair;
use crate::state::StateEngine;
use mesh_proto::OpBody;

#[derive(Debug)]
pub struct PushResult {
    pub bundle_id: BundleId,
    pub file_count: u32,
    pub chunk_count: u32,
    pub total_size: u64,
}

/// Ingest a local path as a bundle — a directory tree, or a single file and
/// its definitions companion: store content, save the manifest, and announce
/// it + our hold into replicated state.
pub fn push_dir(
    engine: &Arc<StateEngine>,
    content: &ContentStore,
    dir: &Path,
) -> Result<PushResult> {
    let manifest = content.ingest_path(dir)?;
    let bundle_id = content.save_manifest(&manifest)?;
    let meta = content.meta(&manifest);
    announce_hold(engine, &meta)?;
    Ok(PushResult {
        bundle_id,
        file_count: meta.file_count,
        chunk_count: meta.chunk_count,
        total_size: meta.total_size,
    })
}

/// Author BundleAnnounce (if new) + BundleHold for a locally-complete bundle.
pub fn announce_hold(engine: &Arc<StateEngine>, meta: &mesh_proto::BundleMeta) -> Result<()> {
    let known = engine.read(|s| s.bundles.contains_key(&meta.bundle_id));
    if !known {
        engine.author_op(OpBody::BundleAnnounce { meta: meta.clone() })?;
    }
    let already_hold = engine.read(|s| {
        s.bundles
            .get(&meta.bundle_id)
            .is_some_and(|b| b.holders.contains(&engine.node_id()))
    });
    if !already_hold {
        engine.author_op(OpBody::BundleHold {
            bundle_id: meta.bundle_id,
            node_id: engine.node_id(),
        })?;
    }
    Ok(())
}

/// After fetching a bundle to completion, record that this node now holds it.
pub fn record_hold_from_manifest(
    engine: &Arc<StateEngine>,
    content: &ContentStore,
    manifest: &Manifest,
) -> Result<()> {
    let meta = content.meta(manifest);
    announce_hold(engine, &meta)
}

/// The next version number the mesh expects for a service name.
pub fn next_version(engine: &Arc<StateEngine>, name: &str) -> u32 {
    engine.read(|s| {
        s.services
            .get(name)
            .and_then(|e| e.versions.keys().max().copied())
            .unwrap_or(0)
            + 1
    })
}

/// Sign a service version with the creating user's key (client-side).
pub fn sign_service_version(user: &Keypair, mut version: ServiceVersion) -> ServiceVersion {
    version.created_by = user.user_id();
    version.signature = Vec::new();
    version.signature = user.sign(&version.signing_bytes()).to_vec();
    version
}

/// Sign a head move (create bumps forward; rollback rewinds).
pub fn sign_service_head(user: &Keypair, name: &str, version: u32) -> Vec<u8> {
    user.sign(&service_head_signing_bytes(name, version, &user.user_id()))
        .to_vec()
}

/// Sign a service removal.
pub fn sign_service_remove(user: &Keypair, name: &str) -> Vec<u8> {
    user.sign(&service_remove_signing_bytes(name, &user.user_id()))
        .to_vec()
}

/// Register a client-built, client-signed service version. Validates that
/// the bundle is known, the contract compiles, and the version number is
/// the next one (a concurrent create surfaces as a conflict to retry).
pub fn create_service(
    engine: &Arc<StateEngine>,
    version: ServiceVersion,
    head_signature: Vec<u8>,
) -> Result<u32> {
    if version.name.is_empty() {
        return Err(CoreError::State("service name must not be empty".into()));
    }
    // The bundle must be known to the mesh (announced) — you can't bind a
    // service to content nobody has.
    //
    // **Unless there is no bundle.** A container service is an image
    // reference, and `nginx:alpine` needs nothing from the content store —
    // requiring one meant pushing a folder the image would never read, just
    // to have something to name. `BundleId::NONE` says so explicitly, and
    // only a container may say it: for every other runtime the bundle *is*
    // the thing that runs, and one without would be a service with no code.
    if version.bundle_id.is_none() {
        if version.runtime != mesh_proto::Runtime::Container {
            return Err(CoreError::State(format!(
                "a {} service needs a bundle — its code is what runs. Only \
                 `runtime = \"container\"` can go without one, because its entry point \
                 is an image reference",
                version.runtime.as_str()
            )));
        }
    } else if !engine.read(|s| s.bundles.contains_key(&version.bundle_id)) {
        return Err(CoreError::State(format!(
            "unknown bundle {} — push it first",
            version.bundle_id.short()
        )));
    }
    // Validate the contract schema compiles, so bad schemas fail at create
    // time, not run time.
    if let Some(schema_str) = &version.contract {
        let schema: serde_json::Value = serde_json::from_str(schema_str)
            .map_err(|e| CoreError::State(format!("contract is not valid JSON: {e}")))?;
        jsonschema::validator_for(&schema)
            .map_err(|e| CoreError::State(format!("contract is not a valid JSON Schema: {e}")))?;
    }
    let expected = next_version(engine, &version.name);
    if version.version != expected {
        return Err(CoreError::State(format!(
            "version conflict: expected {} for service {:?}, got {} — retry",
            expected, version.name, version.version
        )));
    }

    let name = version.name.clone();
    let number = version.version;
    let by = version.created_by;
    engine.author_op(OpBody::ServiceVersionAdd { version })?;
    engine.author_op(OpBody::ServiceHead {
        name,
        version: number,
        by,
        signature: head_signature,
    })?;
    Ok(number)
}

/// Build, sign and register a service version in one step, for callers that
/// hold the user key in-process (tests, testkit, the bootstrap founder).
#[allow(clippy::too_many_arguments)]
pub fn create_service_as(
    engine: &Arc<StateEngine>,
    user: &Keypair,
    name: String,
    runtime: Runtime,
    bundle_id: BundleId,
    working_dir: String,
    entrypoint: String,
    contract: Option<String>,
    env: Vec<(String, String)>,
    description: String,
    tags: Vec<String>,
) -> Result<u32> {
    let number = next_version(engine, &name);
    let version = sign_service_version(
        user,
        ServiceVersion {
            name: name.clone(),
            version: number,
            runtime,
            bundle_id,
            working_dir,
            entrypoint,
            contract,
            env,
            description,
            tags,
            created_at_micros: mesh_proto::now_micros(),
            created_by: user.user_id(),
            source: None,
            signature: Vec::new(),
        },
    );
    let head_signature = sign_service_head(user, &name, number);
    create_service(engine, version, head_signature)
}

pub fn rollback_service(
    engine: &Arc<StateEngine>,
    name: &str,
    version: u32,
    by: mesh_proto::UserId,
    signature: Vec<u8>,
) -> Result<()> {
    let exists = engine.read(|s| {
        s.services
            .get(name)
            .is_some_and(|e| e.versions.contains_key(&version))
    });
    if !exists {
        return Err(CoreError::State(format!(
            "service {name} has no version {version}"
        )));
    }
    engine.author_op(OpBody::ServiceHead {
        name: name.to_string(),
        version,
        by,
        signature,
    })?;
    Ok(())
}

pub fn remove_service(
    engine: &Arc<StateEngine>,
    name: &str,
    by: mesh_proto::UserId,
    signature: Vec<u8>,
) -> Result<()> {
    if engine.read(|s| s.service_current(name).is_none()) {
        return Err(CoreError::State(format!("no live service named {name}")));
    }
    engine.author_op(OpBody::ServiceRemove {
        name: name.to_string(),
        by,
        signature,
    })?;
    Ok(())
}

/// Resolve an already-selected service *version* to an execution plan.
///
/// Split out of [`resolve_service`] because a caller may pin a version
/// (`apply-release@12`) and must not silently follow the head: the whole
/// point of pinning is that what runs is what was published.
pub fn resolve_service_version(
    def: &ServiceVersion,
    inputs: Vec<(String, String)>,
    inventory: Option<MeshInventory>,
) -> Result<ServiceExec> {
    validate_inputs(def, &inputs)?;
    Ok(ServiceExec {
        service_name: def.name.clone(),
        version: def.version,
        runtime: def.runtime,
        bundle_id: def.bundle_id,
        working_dir: def.working_dir.clone(),
        entrypoint: def.entrypoint.clone(),
        env: def.env.clone(),
        inputs,
        inventory,
    })
}

/// Resolve a service to an execution plan, validating inputs against the
/// contract. `inventory` is a rendered mesh inventory for ansible services.
pub fn resolve_service(
    engine: &Arc<StateEngine>,
    name: &str,
    inputs: Vec<(String, String)>,
    inventory: Option<MeshInventory>,
) -> Result<ServiceExec> {
    let def = engine
        .read(|s| s.service_current(name).cloned())
        .ok_or_else(|| CoreError::State(format!("no live service named {name}")))?;

    validate_inputs(&def, &inputs)?;

    Ok(ServiceExec {
        service_name: def.name,
        version: def.version,
        runtime: def.runtime,
        bundle_id: def.bundle_id,
        working_dir: def.working_dir,
        entrypoint: def.entrypoint,
        env: def.env,
        inputs,
        inventory,
    })
}

/// Validate inputs against the service's JSON Schema contract, if any.
/// Inputs arrive as strings; we coerce to the schema's declared types where
/// possible so `count=3` validates against `{"type":"integer"}`.
pub fn validate_inputs(def: &ServiceVersion, inputs: &[(String, String)]) -> Result<()> {
    let Some(schema_str) = &def.contract else {
        return Ok(());
    };
    let schema: serde_json::Value = serde_json::from_str(schema_str)
        .map_err(|e| CoreError::State(format!("stored contract invalid: {e}")))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|e| CoreError::State(format!("stored contract invalid: {e}")))?;

    let instance = coerce_inputs(&schema, inputs);
    if let Err(err) = validator.validate(&instance) {
        return Err(CoreError::State(format!("input validation failed: {err}")));
    }
    Ok(())
}

/// Build a JSON object from string inputs, coercing values to the types the
/// schema declares for each property (string stays string; integer/number/
/// boolean are parsed; everything else passes through as a string).
pub(crate) fn coerce_inputs(
    schema: &serde_json::Value,
    inputs: &[(String, String)],
) -> serde_json::Value {
    use serde_json::Value;
    let props = schema.get("properties").and_then(|p| p.as_object());
    let mut map = serde_json::Map::new();
    for (k, v) in inputs {
        let declared = props
            .and_then(|p| p.get(k))
            .and_then(|s| s.get("type"))
            .and_then(|t| t.as_str());
        let value = match declared {
            Some("integer") => v
                .parse::<i64>()
                .map(Value::from)
                .unwrap_or(Value::String(v.clone())),
            Some("number") => v
                .parse::<f64>()
                .map(Value::from)
                .unwrap_or(Value::String(v.clone())),
            Some("boolean") => match v.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => Value::String(v.clone()),
            },
            _ => Value::String(v.clone()),
        };
        map.insert(k.clone(), value);
    }
    Value::Object(map)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn coercion_matches_declared_types() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": {"type": "integer"},
                "name": {"type": "string"},
                "on": {"type": "boolean"}
            },
            "required": ["count", "name"],
            "additionalProperties": false
        });
        let inputs = vec![
            ("count".to_string(), "3".to_string()),
            ("name".to_string(), "edge".to_string()),
            ("on".to_string(), "true".to_string()),
        ];
        let instance = coerce_inputs(&schema, &inputs);
        assert_eq!(instance["count"], serde_json::json!(3));
        assert_eq!(instance["on"], serde_json::json!(true));
        let validator = jsonschema::validator_for(&schema).unwrap();
        assert!(validator.validate(&instance).is_ok());
    }

    #[test]
    fn missing_required_input_rejected() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"count": {"type": "integer"}},
            "required": ["count"]
        });
        let validator = jsonschema::validator_for(&schema).unwrap();
        let instance = coerce_inputs(&schema, &[]);
        assert!(validator.validate(&instance).is_err());
    }
}
