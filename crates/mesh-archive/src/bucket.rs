//! Replicating the archive to an object store, and WORM for the audit
//! trail (M31 B4).
//!
//! ## Why this is the highest value per line in the milestone
//!
//! Nothing here is a connector. The archive is already Parquet, already
//! partitioned by day, already named deterministically — so pushing it to a
//! bucket is a copy, and the copy is readable by **Athena, Trino, DuckDB,
//! Snowflake, BigQuery, Redshift, Databricks and ClickHouse's `s3()`**
//! without meshware writing a line for any of them. Six warehouses arrive
//! as a side effect of a file upload.
//!
//! ## Idempotence is free, and that is not luck
//!
//! Part filenames are `<table>/<day>/part-<n>.parquet`, assigned from
//! `Manifest.next_part` and never reused. A part that is already in the
//! bucket is byte-identical to the one on disk, so re-running the whole
//! replication uploads nothing — no manifest of uploads, no etag
//! bookkeeping, no "resume" mode. The design that made compaction safe made
//! this free.
//!
//! ## WORM, and the word that must not be misused
//!
//! S3 Object Lock has two modes and only one of them is immutability:
//!
//! - **Compliance**: no one can delete or shorten the retention of the
//!   object before its date. Not the bucket owner, not the root account,
//!   not the provider's support desk.
//! - **Governance**: the same, *unless* the caller holds
//!   `s3:BypassGovernanceRetention` — which the account that set it up
//!   usually does.
//!
//! Governance mode is a speed bump with a compliance-sounding name. So
//! `--worm` writes **compliance** mode and nothing else, and a bucket
//! without Object Lock is refused rather than written to with a warning:
//! the entire value of the feature is being able to say "this cannot be
//! altered", and a silent downgrade turns that sentence into a lie an
//! auditor will find.

use std::path::Path;
use std::sync::Arc;

use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::path::Path as ObjectPath;
use object_store::{ObjectStore, ObjectStoreExt};

use crate::Error;

/// Where an archive replicates to, parsed from `s3://bucket/prefix`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketTarget {
    pub bucket: String,
    /// Key prefix, without leading or trailing slashes. Empty is the root.
    pub prefix: String,
    /// `endpoint` for MinIO, Ceph, R2 and every other S3-compatible store.
    /// `None` means AWS itself.
    pub endpoint: Option<String>,
    pub region: Option<String>,
}

impl BucketTarget {
    /// Parse `s3://bucket[/prefix]`.
    ///
    /// `s3://` rather than a URL crate, and rather than accepting `https://`
    /// too: the scheme is the operator's statement of intent, and taking a
    /// bare host would make `--bucket https://warehouse.corp` look like it
    /// might work.
    pub fn parse(url: &str) -> Result<BucketTarget, Error> {
        let rest = url
            .trim()
            .strip_prefix("s3://")
            .ok_or_else(|| {
                Error::Bucket(format!(
                    "`{url}` is not a bucket URL — it looks like `s3://your-bucket/optional/prefix`"
                ))
            })?
            // Trailing slashes only. Trimming the *leading* one would turn
            // `s3:///prefix` — an empty authority, so a URL with no bucket
            // in it — into a bucket called `prefix`, which is a typo
            // silently becoming a destination.
            .trim_end_matches('/');
        let (bucket, prefix) = match rest.split_once('/') {
            Some((b, p)) => (b, p),
            None => (rest, ""),
        };
        if bucket.is_empty() {
            return Err(Error::Bucket(
                "that bucket URL has no bucket name in it".into(),
            ));
        }
        Ok(BucketTarget {
            bucket: bucket.to_string(),
            prefix: prefix.trim_matches('/').to_string(),
            endpoint: None,
            region: None,
        })
    }

    pub fn with_endpoint(mut self, endpoint: Option<String>) -> Self {
        self.endpoint = endpoint.filter(|e| !e.trim().is_empty());
        self
    }

    pub fn with_region(mut self, region: Option<String>) -> Self {
        self.region = region.filter(|r| !r.trim().is_empty());
        self
    }

    /// The object key one archive-relative path lands at.
    fn key(&self, relative: &str) -> ObjectPath {
        let joined = if self.prefix.is_empty() {
            relative.to_string()
        } else {
            format!("{}/{}", self.prefix, relative)
        };
        ObjectPath::from(joined)
    }

    /// Build the client.
    ///
    /// Credentials come from `access_key`/`secret_key` when the caller
    /// resolved a `{{secret:…}}` reference, and otherwise from the
    /// environment — which is what an instance profile, an IRSA token or a
    /// developer's `AWS_PROFILE` already fill in. **Never from argv**: the
    /// same rule the sink credential follows, for the same reason.
    ///
    /// `worm_days`, when set, returns a client whose **every** request
    /// carries the Object Lock headers. That is a deliberate shape: a
    /// locked client is a different object from an unlocked one, so an
    /// audit part cannot be written unlocked by forgetting a flag at a call
    /// site. `object_store` has no per-request header API — only
    /// `ClientOptions::with_default_headers` — and that constraint pushed
    /// the design somewhere better than the one it replaced.
    pub fn client(
        &self,
        access_key: Option<&str>,
        secret_key: Option<&str>,
        allow_http: bool,
        worm_days: Option<u32>,
    ) -> Result<AmazonS3, Error> {
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(&self.bucket);
        if let Some(endpoint) = &self.endpoint {
            builder = builder.with_endpoint(endpoint);
            // Path style, because a custom endpoint is almost always MinIO
            // or Ceph and virtual-host addressing needs DNS nobody set up.
            builder = builder.with_virtual_hosted_style_request(false);
        }
        if let Some(region) = &self.region {
            builder = builder.with_region(region);
        }
        if let (Some(a), Some(s)) = (access_key, secret_key) {
            builder = builder.with_access_key_id(a).with_secret_access_key(s);
        }
        // **One `ClientOptions`, built once.** `with_client_options`
        // *replaces* whatever the builder had, so setting `allow_http` on
        // the builder and then handing over a fresh options object silently
        // drops it — which shows up as a "builder error" on the first
        // request against an `http://` endpoint and looks like a header
        // problem. Everything that belongs to the client goes here.
        let mut options = object_store::ClientOptions::new().with_allow_http(allow_http);
        if let Some(days) = worm_days {
            let mut headers = http::HeaderMap::new();
            // **Compliance, never governance.** Governance mode is
            // bypassable by anyone holding `s3:BypassGovernanceRetention`,
            // which is usually the account that set the bucket up — a speed
            // bump with a compliance-sounding name.
            headers.insert(
                "x-amz-object-lock-mode",
                http::HeaderValue::from_static("COMPLIANCE"),
            );
            headers.insert(
                "x-amz-object-lock-retain-until-date",
                http::HeaderValue::from_str(&retain_until(days))
                    .map_err(|e| Error::Bucket(format!("retain-until date: {e}")))?,
            );
            options = options.with_default_headers(headers);
        }
        builder = builder.with_client_options(options);
        builder
            .build()
            .map_err(|e| Error::Bucket(format!("cannot reach `{}`: {e}", self.bucket)))
    }
}

/// What one replication pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PushReport {
    pub uploaded: u64,
    /// Already in the bucket, byte for byte. On a steady-state archive this
    /// is the large number and `uploaded` is small — which is the whole
    /// idempotence claim, stated as a metric an operator can watch.
    pub already_there: u64,
    pub bytes: u64,
    /// Objects written under Object Lock compliance mode.
    pub locked: u64,
}

/// How long an audit object is locked for, in days.
///
/// Taken from the mesh's own audit retention window rather than invented
/// here: the floor is a year (`AUDIT_FLOOR_DAYS`), and a WORM copy that
/// expired before the mesh's own policy said it could would be the one
/// failure mode worse than not having one.
pub fn lock_days(audit_retention_days: u32) -> u32 {
    audit_retention_days.max(mesh_proto::AUDIT_FLOOR_DAYS)
}

/// Replicate every part under `root` to `target`.
///
/// Deterministic keys mean this is safe to run on any schedule, from any
/// machine, as often as anyone likes. `worm_days`, when set, writes the
/// `audit` table's objects under Object Lock **compliance** mode — and
/// refuses the whole pass if the bucket does not have Object Lock enabled,
/// because a WORM request that quietly became an ordinary upload is worse
/// than an error.
pub async fn push(
    root: &Path,
    target: &BucketTarget,
    store: &AmazonS3,
    worm: Option<(&AmazonS3, u32)>,
) -> Result<PushReport, Error> {
    let mut report = PushReport::default();
    let mut files = walk(root)?;

    // **Audit objects first, and that ordering is the WORM check.**
    //
    // `object_store` exposes no way to read a bucket's Object Lock
    // configuration, and a synthetic probe would be a lie in both
    // directions: S3 ignores lock headers on HEAD, so a HEAD proves
    // nothing, and inventing a marker object would write something the
    // operator did not ask for. What *does* prove it is the real PUT —
    // S3 answers `InvalidRequest: Bucket is missing Object Lock
    // Configuration` — so the honest check is to attempt the first audit
    // object before anything else and let the store's own refusal be the
    // answer. Ordering makes that refusal cheap: nothing else has gone up.
    files.sort_by_key(|(rel, _)| (!is_audit_object(rel), rel.clone()));

    for (relative, path) in files {
        let key = target.key(&relative);
        let bytes = std::fs::read(&path)?;
        let locked = worm.filter(|_| is_audit_object(&relative));

        // Deterministic names make "is it there" the whole of the
        // idempotence check: a part that exists is the part we would write.
        if let Ok(meta) = store.head(&key).await
            && meta.size == bytes.len() as u64
        {
            report.already_there += 1;
            continue;
        }

        let writer = locked.map(|(s, _)| s).unwrap_or(store);
        let len = bytes.len() as u64;
        if let Err(e) = put_object(writer, &key, bytes).await {
            if locked.is_some() {
                return Err(Error::Bucket(format!(
                    "{e}. Writing the audit trail under Object Lock needs a bucket with Object \
                     Lock enabled — and that can only be set when the bucket is created, never \
                     afterwards. Make a bucket for it, or drop --worm and accept that this copy \
                     is deletable."
                )));
            }
            return Err(e);
        }
        report.uploaded += 1;
        report.bytes += len;
        if locked.is_some() {
            report.locked += 1;
        }
    }
    Ok(report)
}

/// Is this object part of the audit trail — the one table WORM applies to?
///
/// Path-prefix rather than a flag threaded through the walk, because the
/// layout *is* the answer: `audit/2026-03-04/part-7.parquet`. If the
/// layout ever stops saying which table an object belongs to, this stops
/// compiling into a correct answer and somebody has to think again.
fn is_audit_object(relative: &str) -> bool {
    relative.starts_with("audit/")
}

async fn put_object(store: &AmazonS3, key: &ObjectPath, bytes: Vec<u8>) -> Result<(), Error> {
    use object_store::PutPayload;

    store
        .put(key, PutPayload::from(bytes))
        .await
        .map(|_| ())
        .map_err(|e| Error::Bucket(format!("uploading {key}: {e}")))
}

/// RFC 3339, UTC, `days` from now — the format S3 wants for a retain-until
/// date. Derived here rather than pulled from a date crate, using the same
/// civil-calendar arithmetic the partitioner already uses in the other
/// direction.
pub fn retain_until(days: u32) -> String {
    let secs = mesh_proto::now_micros() / 1_000_000 + u64::from(days) * 86_400;
    let (y, m, d) = crate::schema::civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Every file under `root`, as `(archive-relative path, absolute path)`.
///
/// Sorted, so a run's output is stable and a diff between two runs is
/// readable. Skips nothing: the manifest goes up too, because a bucket
/// holding parts and no manifest is a bucket a reader cannot tell is
/// complete.
fn walk(root: &Path) -> Result<Vec<(String, std::path::PathBuf)>, Error> {
    fn inner(
        base: &Path,
        dir: &Path,
        out: &mut Vec<(String, std::path::PathBuf)>,
    ) -> Result<(), Error> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                inner(base, &path, out)?;
            } else if let Ok(rel) = path.strip_prefix(base) {
                out.push((rel.to_string_lossy().replace('\\', "/"), path.clone()));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    if root.exists() {
        inner(root, root, &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Wrap a built store so callers do not name `object_store` types.
pub type Store = Arc<dyn ObjectStore>;

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_bucket_url_splits_into_bucket_and_prefix() {
        let t = BucketTarget::parse("s3://acme-archive").expect("bare bucket");
        assert_eq!(t.bucket, "acme-archive");
        assert_eq!(t.prefix, "");

        let t = BucketTarget::parse("s3://acme-archive/meshware/prod/").expect("prefix");
        assert_eq!(t.bucket, "acme-archive");
        assert_eq!(t.prefix, "meshware/prod");
        assert_eq!(
            t.key("runs/2026-03-04/part-1.parquet").as_ref(),
            "meshware/prod/runs/2026-03-04/part-1.parquet"
        );
    }

    /// The scheme is the operator's statement of intent, and a bare host
    /// must not look like it might work.
    #[test]
    fn anything_that_is_not_a_bucket_url_is_refused_by_name() {
        for bad in [
            "https://warehouse.corp/bucket",
            "acme-archive",
            "s3://",
            "s3:///prefix-only",
        ] {
            let err = BucketTarget::parse(bad).expect_err(bad).to_string();
            assert!(
                err.contains("bucket"),
                "the refusal for {bad:?} should name what is missing: {err}"
            );
        }
    }

    /// **WORM applies to the audit trail and to nothing else.** A run
    /// record locked for a year is an operator who cannot fix a mistake;
    /// an audit row locked for a year is the point.
    #[test]
    fn only_the_audit_table_is_locked() {
        assert!(is_audit_object("audit/2026-03-04/part-7.parquet"));
        assert!(!is_audit_object("runs/2026-03-04/part-7.parquet"));
        assert!(!is_audit_object("jobs/2026-03-04/part-7.parquet"));
        assert!(!is_audit_object("manifest.json"));
        // Not a substring match: a table whose name merely contains
        // "audit" is not the audit table.
        assert!(!is_audit_object("preaudit/2026-03-04/part-1.parquet"));
    }

    /// A WORM copy that expired before the mesh's own audit window did
    /// would be the one failure mode worse than not having one.
    #[test]
    fn a_lock_never_expires_before_the_meshs_own_audit_window() {
        assert_eq!(lock_days(0), mesh_proto::AUDIT_FLOOR_DAYS);
        assert_eq!(lock_days(30), mesh_proto::AUDIT_FLOOR_DAYS);
        assert_eq!(lock_days(3650), 3650);
    }

    #[test]
    fn a_retain_until_date_is_rfc3339_utc() {
        let s = retain_until(365);
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(s.len(), 20, "{s}");
        assert!(
            s.chars().nth(4) == Some('-') && s.chars().nth(10) == Some('T'),
            "{s}"
        );
    }
}
