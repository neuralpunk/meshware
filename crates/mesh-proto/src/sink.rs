//! Sinks: somewhere else to keep the history (M31 B1).
//!
//! A sink is a **replicated, admin-signed record of its own** — never a
//! field on a node's config file. That is C7 rather than tidiness: a sink is
//! the easiest exfiltration channel in the product by construction (command
//! lines, submitter identities, declared outputs, and with payload export
//! the complete stdout of every job), so *who pointed our history at that
//! host, and when* has to be a question `meshnode audit` can answer.
//!
//! ## What this is not
//!
//! **This is not how meshware is used.** The default mesh keeps its own
//! history, answers its own questions, and needs nothing else running; that
//! is the product. A real minority of customers — the regulated, the
//! audited, the ones with a warehouse that predates us — need every run
//! record in a system they already own, and this is for them. Nothing here
//! belongs in a getting-started path.
//!
//! ## The four rules, and every one of them is load-bearing
//!
//! 1. **Never a database driver in the node** (§4 decision 1). One HTTP POST
//!    path reaches ClickHouse, OpenSearch, Loki, VictoriaLogs, Splunk HEC,
//!    every OTLP collector and every webhook. Anything needing a driver, a
//!    columnar encoder or a table format lives out of process.
//! 2. **The escape hatch ships before any connector** (§4 decision 2). A
//!    rotating NDJSON file plus a subprocess exporter buys the entire
//!    ecosystem, because Vector and Fluent Bit already speak to everything
//!    we would otherwise write by hand.
//! 3. **Records and payloads never travel together** (§4 decision 3).
//!    Records are small and ordered and want one cursor from one elected
//!    node; raw stdout is large and must leave from the machine that
//!    produced it.
//! 4. **A failing sink spends spool, never scheduler** (§4 decision 6, C3).
//!    Bounded on-disk spool, then drop and count. Nothing about a full
//!    warehouse queues a job. This is the rule that will erode under
//!    enterprise pressure, and the throughput test is what holds it.
//!
//! ## Why `kind` is an enum carrying a capability triple
//!
//! A destination is an **appended variant** rather than a string, so adding
//! one is a compile error until somebody says what a person can get back out
//! of it: does it *archive*, can you *browse records* in it, can you *replay
//! stdout* from it? A capability table that lives in a document is a
//! capability table that is wrong by the third destination. B3 consumes the
//! triple and B2 prints it in the picker; declaring it here is what stops it
//! drifting from the code.

use serde::{Deserialize, Serialize};

use crate::UserId;

/// The longest a sink may hold deletion back, whatever an operator types.
///
/// Thirty days. A "ceiling" an operator can set to a decade is not a
/// ceiling — it is the unbounded case wearing a number — and §18 decision
/// 03's whole point is that the mesh's health is not negotiable against a
/// warehouse's completeness. A month is long enough to survive any outage
/// somebody is actually going to fix.
pub const MAX_HOLD_FOR_EXPORT_DAYS: u32 = 30;

/// A sink name, as it appears in `meshnode sinks` and in an audit entry.
pub fn is_valid_sink_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Where records go.
///
/// **Appended, never inserted.** postcard encodes a variant by index and a
/// sink record is signed, so inserting one in the middle silently stops
/// every existing signature verifying.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SinkKind {
    /// **Retired.** One JSON object per line, size-rotated under the node's
    /// home. No longer offered and no longer accepted; see
    /// [`SinkKind::retirement`] for what to use instead.
    ///
    /// **The variant stays, and must.** `SinkKind` is a field of [`Sink`],
    /// which is the body of a signed `OpBody::SinkPut` replicated to every
    /// node and kept in the archive. postcard encodes an enum by variant
    /// index, so deleting this one would shift `Plugin` from 1 to 0 and
    /// every kind after it by one — silently re-reading every sink record
    /// ever written as the wrong destination. The ops are signed, so they
    /// cannot be rewritten to compensate. A retired variant is the only
    /// shape this decision can take.
    ///
    /// Existing file sinks keep exporting, deliberately: withdrawing a
    /// running history export the moment a mesh upgrades is the one way an
    /// export must never end. They can be removed, and cannot be recreated.
    Ndjson,
    /// A subprocess that speaks the `plugin write` contract: NDJSON on
    /// stdin, an ack naming the highest row accepted on stdout, non-zero
    /// exit means retry.
    ///
    /// The same shape as `secrets::provider_get`, deliberately — one
    /// plugin contract in the product, not two.
    Plugin,
    // -- M31 B2: four named destinations, one code path ---------------------
    //
    // All four are *the same function*: batch NDJSON, POST, check the
    // status, advance the cursor. What differs is the body encoding and the
    // headers, which is why this is four enum variants and one writer
    // rather than four connectors.
    /// ClickHouse over `JSONEachRow`.
    ///
    /// Deduplicates identical inserts by default, so a retry with the same
    /// batch in the same order is safe — which is what makes at-least-once
    /// delivery (§4 decision 5) cost the operator nothing.
    ClickHouse,
    /// VictoriaLogs' JSON-lines ingest.
    ///
    /// A single zero-config binary, which makes it the right default for
    /// **payloads**: raw stdout wants a log store, not a warehouse, and
    /// this is the one an operator can stand up in an afternoon.
    VictoriaLogs,
    /// OpenSearch or Elasticsearch `_bulk`.
    ///
    /// One action line per document with `_id = <mesh>:<row>`, so a retry
    /// overwrites rather than duplicating. **Set `dynamic: false` on the
    /// index** — the DDL this prints says so, and job output detonates a
    /// dynamic mapping.
    OpenSearch,
    /// Anything else: the same POST with a user-supplied URL and headers.
    ///
    /// The escape hatch's escape hatch. It exists because the list above
    /// will always be missing somebody's destination, and a webhook is a
    /// smaller promise than a connector.
    Webhook,
    // -- M31 B5: Postgres, out of process -----------------------------------
    /// Postgres, via binary `COPY` — **written by `meshware-archive`, never
    /// by the daemon**.
    ///
    /// The most-requested destination, built the way that does not cost the
    /// four-dependency property: a Postgres driver in `meshnode` would be
    /// the first crack in the C1 wall, so the node hands a batch to
    /// `meshware-archive` exactly as it hands one to any `plugin` sink. It
    /// is a plugin whose program meshware happens to ship.
    Postgres,
    // -- M31 B6: the two that reach the most buyers per line of code -------
    /// Splunk HTTP Event Collector: one POST, one token header, JSON
    /// events.
    ///
    /// Technically the easiest destination in the milestone, and its
    /// payload is the **audit log** — a signed, per-user record of who ran
    /// what where, delivered where a security team already looks. HEC is
    /// priced per ingested gigabyte, which is why per-table filtering was
    /// mandatory in B2 rather than nice to have.
    SplunkHec,
    /// OpenTelemetry over HTTP, JSON encoding, `cicd.*` semantic
    /// conventions.
    ///
    /// **The reframe is the point.** This turns *"export your data"* into
    /// *"your mesh is already instrumented"*, and reaches Grafana,
    /// Datadog, Honeycomb, New Relic, Splunk, SigNoz, Elastic and Dynatrace
    /// through one exporter. A workflow is a trace; a step is a span.
    Otlp,
}

impl SinkKind {
    /// The destinations a new sink may name.
    ///
    /// Five kinds are absent and that is the retirement: nothing that
    /// offers a choice offers them, while [`SinkKind::parse`] still answers
    /// for each so records already signed keep decoding.
    ///
    /// What is left is both generic escape hatches — `plugin` and
    /// `webhook` — and the two destinations Director can *read back* from.
    /// Every named connector was the same four lines (batch, POST, check,
    /// advance) wrapped in somebody else's schema, auth shape and version
    /// policy, and this page's own recipe makes the point: four lines of
    /// shell appending to a file lets Vector or Fluent Bit reach S3, Loki,
    /// BigQuery and Kafka with no meshware-side connector to wait for.
    pub const ALL: [SinkKind; 4] = [
        SinkKind::Plugin,
        SinkKind::ClickHouse,
        SinkKind::VictoriaLogs,
        SinkKind::Webhook,
    ];

    /// Why this kind is gone, and what to do instead. `None` for every
    /// kind that is still offered.
    ///
    /// A sentence rather than a bare rejection, because "unsupported" tells
    /// an operator nothing about the export they still have to build.
    pub fn retirement(self) -> Option<&'static str> {
        match self {
            SinkKind::Ndjson => Some(
                "the `ndjson` destination has been retired. It wrote one file on whichever \
                 node the export election happened to land on, so the file moved when the \
                 election did and a mesh's history ended up in fragments across its fleet \
                 with nothing recording which machine held which range. Use `plugin` to \
                 write a file on your own terms — it receives the same NDJSON on stdin — or \
                 a destination that takes the rows directly: clickhouse, victorialogs, \
                 webhook",
            ),
            // -- RETIRED 2026-09-20: four named connectors ----------------
            //
            // Each was the same batch-POST-check-advance loop wearing
            // somebody else's schema, auth shape and version policy —
            // against a server meshware does not version and cannot test.
            // `plugin` reaches all of them through a tool that already
            // speaks them, and keeps working when they change.
            SinkKind::OpenSearch => Some(
                "the `opensearch` destination has been retired. Point a `plugin` sink at \
                 Vector, Fluent Bit or Filebeat, which speak OpenSearch and Elasticsearch \
                 already and track their API changes for you — the plugin receives the same \
                 NDJSON on stdin",
            ),
            SinkKind::Postgres => Some(
                "the `postgres` destination has been retired. It needed a table built to a \
                 schema meshware printed and an operator applied, and could never be read \
                 back into Director. Use `clickhouse` if you want a warehouse meshware can \
                 also read, or a `plugin` sink into your own `COPY`",
            ),
            SinkKind::SplunkHec => Some(
                "the `splunk-hec` destination has been retired. A `webhook` sink is the same \
                 POST — give it your HEC URL and its token header — and a `plugin` sink \
                 reaches Splunk through the forwarder your security team already runs",
            ),
            SinkKind::Otlp => Some(
                "the `otlp` destination has been retired. It was instrumentation rather than \
                 an archive — a trace is not a copy of your history and could never be read \
                 back — so it belongs in an OpenTelemetry collector fed by a `plugin` sink",
            ),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SinkKind::Ndjson => "ndjson",
            SinkKind::Plugin => "plugin",
            SinkKind::ClickHouse => "clickhouse",
            SinkKind::VictoriaLogs => "victorialogs",
            SinkKind::OpenSearch => "opensearch",
            SinkKind::Webhook => "webhook",
            SinkKind::Postgres => "postgres",
            SinkKind::SplunkHec => "splunk-hec",
            SinkKind::Otlp => "otlp",
        }
    }

    /// `elasticsearch` is an accepted spelling of `opensearch`: they speak
    /// the same `_bulk` API, and making an operator discover that by
    /// reading our enum would be a small, avoidable insult.
    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            SinkKind::OpenSearch => &["elasticsearch", "elastic", "es"],
            SinkKind::ClickHouse => &["ch"],
            SinkKind::Postgres => &["postgresql", "pg", "timescaledb", "timescale"],
            SinkKind::SplunkHec => &["splunk", "hec"],
            SinkKind::Otlp => &["opentelemetry", "otel"],
            _ => &[],
        }
    }

    /// Every variant, **retired ones included** — the set [`parse`] answers
    /// for, as distinct from [`ALL`], which is the set a picker offers.
    ///
    /// The two must not be one list. A retired kind still has to parse:
    /// every sink record ever signed carries its name, and a mesh that
    /// cannot read one back cannot say what it is refusing, cannot show the
    /// operator the sink they already have, and cannot let them remove it.
    ///
    /// [`parse`]: SinkKind::parse
    /// [`ALL`]: SinkKind::ALL
    const EVERY: [SinkKind; 9] = [
        SinkKind::Ndjson,
        SinkKind::Plugin,
        SinkKind::ClickHouse,
        SinkKind::VictoriaLogs,
        SinkKind::OpenSearch,
        SinkKind::Webhook,
        SinkKind::Postgres,
        SinkKind::SplunkHec,
        SinkKind::Otlp,
    ];

    pub fn parse(s: &str) -> Option<SinkKind> {
        SinkKind::EVERY.into_iter().find(|k| {
            k.as_str().eq_ignore_ascii_case(s)
                || k.aliases().iter().any(|a| a.eq_ignore_ascii_case(s))
        })
    }

    /// Does this destination leave the machine?
    ///
    /// The question `egress_allow` and the payload path both ask, and
    /// asking it here rather than matching on `url.is_empty()` is what
    /// stops a kind added later being quietly exempt from the allowlist.
    pub fn is_http(self) -> bool {
        match self {
            SinkKind::Ndjson | SinkKind::Plugin => false,
            // Postgres is **not** HTTP and still leaves the machine, which
            // is the whole reason this is its own question rather than
            // `url.starts_with("http")`. `egress_allow` governs it; the
            // HTTP writer does not.
            SinkKind::Postgres => false,
            SinkKind::ClickHouse
            | SinkKind::VictoriaLogs
            | SinkKind::OpenSearch
            | SinkKind::Webhook
            | SinkKind::SplunkHec
            | SinkKind::Otlp => true,
        }
    }

    /// Does this destination leave the machine at all?
    ///
    /// Broader than [`Self::is_http`] and the one `egress_allow` actually
    /// wants: a Postgres sink opens a socket to somebody else's server
    /// without speaking HTTP, and a governance control that only knew about
    /// HTTP would wave it through.
    pub fn leaves_the_machine(self) -> bool {
        match self {
            SinkKind::Ndjson => false,
            // A plugin is a local program. What *it* does is its business
            // and the operator's — meshware cannot govern a subprocess's
            // network, and pretending to would be worse than saying so.
            SinkKind::Plugin => false,
            SinkKind::Postgres
            | SinkKind::ClickHouse
            | SinkKind::VictoriaLogs
            | SinkKind::OpenSearch
            | SinkKind::Webhook
            | SinkKind::SplunkHec
            | SinkKind::Otlp => true,
        }
    }

    /// What a person can get back out of this destination.
    ///
    /// One exhaustive match, exactly as `compact::compaction` resolves
    /// compaction eligibility: **adding a destination without saying what
    /// can be read back out of it does not compile.**
    pub fn capabilities(self) -> SinkCapabilities {
        match self {
            // A file on disk archives perfectly and answers nothing. Vector
            // may forward it into something that does, but that is Vector's
            // capability and not ours to claim — and claiming it is exactly
            // how a picker ends up lying to the person choosing.
            SinkKind::Ndjson => SinkCapabilities {
                archives: true,
                browse_records: false,
                replay_payloads: false,
            },
            // Unknowable from here: the plugin decides. Declared as the
            // weakest thing that is certainly true, because a picker that
            // over-promises on a destination nobody in this tree wrote is
            // the worst version of this table.
            SinkKind::Plugin => SinkCapabilities {
                archives: true,
                browse_records: false,
                replay_payloads: false,
            },
            // A warehouse. It stores, and since B3 **one payload can be
            // fetched back by id, because a person clicked it** — never to
            // render a list, populate a filter or count anything.
            // `browse_records` stays false and is not an oversight: the
            // index is answered from local tiers, always (§18 fence 2), and
            // the moment History is painted from a warehouse we have
            // shipped a control plane.
            SinkKind::ClickHouse => SinkCapabilities {
                archives: true,
                browse_records: false,
                replay_payloads: true,
            },
            // The best replay story in the list, because a log store is
            // what fetching one job's stdout is *for*.
            SinkKind::VictoriaLogs => SinkCapabilities {
                archives: true,
                browse_records: false,
                replay_payloads: true,
            },
            // And the best "grep 5 GB" story, because full text is what it
            // is for — search-within is delegated to `_search` rather than
            // scanned by us.
            SinkKind::OpenSearch => SinkCapabilities {
                archives: true,
                browse_records: false,
                replay_payloads: true,
            },
            // Nothing is knowable about the other end of a webhook.
            // **Write-only, and §14.4's table said otherwise.** That row
            // was written in B3 as a forecast of what B5 would deliver;
            // B5's own work items are the write path — binary COPY,
            // `ON CONFLICT`, printed DDL — and reading back would need a
            // second out-of-process path this phase did not build. The
            // picker renders from *this*, so it tells the truth and the
            // table is the thing that was wrong.
            SinkKind::Postgres => SinkCapabilities {
                archives: true,
                browse_records: false,
                replay_payloads: false,
            },
            // HEC is an ingest endpoint. Reading is a different API, a
            // different token and usually a different licence — so this is
            // not "not built yet", it is a property of the destination.
            SinkKind::SplunkHec => SinkCapabilities {
                archives: true,
                browse_records: false,
                replay_payloads: false,
            },
            // A telemetry pipeline, not a store. Where a span lands is the
            // collector's business, and claiming we could read it back
            // would be claiming to know somebody else's routing config.
            SinkKind::Otlp => SinkCapabilities {
                archives: false,
                browse_records: false,
                replay_payloads: false,
            },
            SinkKind::Webhook => SinkCapabilities {
                archives: false,
                browse_records: false,
                replay_payloads: false,
            },
        }
    }

    /// The sentence a picker shows **at selection**, before the operator
    /// commits (M31 B2 item 8, §14.4).
    ///
    /// Rendered from the capability triple rather than written beside it,
    /// so it cannot drift from what the code does. It appears at selection
    /// and not in a tooltip afterwards, because B2 is where the first
    /// customer chooses and a picker that stays silent has already misled
    /// them.
    pub fn capability_sentence(self) -> &'static str {
        match self {
            SinkKind::Ndjson => {
                "Writes a rotating file on the exporting node. Point Vector or Fluent Bit at \
                 it to reach anything else. Director cannot read it back."
            }
            SinkKind::Plugin => {
                "Hands each batch to your own program. What happens next is yours; Director \
                 cannot read it back."
            }
            SinkKind::ClickHouse => {
                "Stores your history where your analysts already query it, and Director can \
                 open past job output from here."
            }
            SinkKind::VictoriaLogs => {
                "Stores history and job output in a log store built for it. Director can open \
                 past job output from here, and search inside it."
            }
            SinkKind::OpenSearch => {
                "Stores your history in OpenSearch or Elasticsearch. Director can open past \
                 job output from here, and search inside it."
            }
            SinkKind::Webhook => {
                "POSTs each batch to a URL you control. meshware knows nothing about what \
                 happens to it and cannot read it back."
            }
            SinkKind::Postgres => {
                "Writes into a Postgres you already run, where your BI tool already points. \
                 Director cannot read it back — you will query it in Postgres."
            }
            SinkKind::SplunkHec => {
                "Sends events to Splunk, where your security team already looks. Best paired \
                 with the audit table alone: HEC is priced per gigabyte. Director cannot read \
                 it back."
            }
            SinkKind::Otlp => {
                "Sends each workflow to your observability stack as a trace, one span per \
                 step — Grafana, Datadog, Honeycomb and the rest. This is instrumentation \
                 rather than an archive: it is not a copy of your history, and Director \
                 cannot read it back."
            }
        }
    }

    /// Does this destination need a URL?
    pub fn needs_url(self) -> bool {
        self.is_http()
    }

    /// Can a payload actually be read back from this sink *as configured*?
    ///
    /// Three answers rather than two, and the third is the point (§14.4):
    /// a destination that **can** replay but has no `read_credential` is
    /// not "unsupported" — it is one permission away, and saying
    /// "unsupported" would send the operator to the wrong place entirely.
    pub fn replay_state(self, has_read_credential: bool) -> ReplayState {
        match (self.capabilities().replay_payloads, has_read_credential) {
            (false, _) => ReplayState::NotSupported,
            (true, false) => ReplayState::NeedsReadCredential,
            (true, true) => ReplayState::Available,
        }
    }

    /// Is this destination priced per gigabyte, or otherwise a place raw
    /// job output should not be sent without somebody meaning it?
    ///
    /// Drives the **per-kind payload default** (§19 Q2, decided): a log
    /// store is what stdout is for, and a warehouse or an unknown webhook
    /// is not. One global default would have been either too timid for
    /// VictoriaLogs or too reckless for everything else.
    pub fn payloads_by_default(self) -> bool {
        matches!(self, SinkKind::VictoriaLogs)
    }

    /// Can raw job output go here **at all**?
    ///
    /// Different from [`Self::payloads_by_default`], which is about the
    /// default an operator can override. This one cannot be overridden,
    /// because for two destinations the answer is a property of what they
    /// are rather than a preference:
    ///
    /// - **OTLP** is a trace pipeline. A payload line is not a span, and
    ///   shovelling stdout through the span encoder would produce junk
    ///   spans in somebody's Grafana that nobody could explain.
    /// - **Postgres** has a published schema for `jobs` and none for
    ///   payload lines. Writing them would mean inventing a table at export
    ///   time, which is how a warehouse acquires a schema nobody reviewed.
    ///
    /// A file, a plugin and a webhook take anything, because none of them
    /// claims to know what the bytes mean.
    pub fn takes_payloads(self) -> bool {
        match self {
            SinkKind::Otlp | SinkKind::Postgres => false,
            SinkKind::Ndjson
            | SinkKind::Plugin
            | SinkKind::ClickHouse
            | SinkKind::VictoriaLogs
            | SinkKind::OpenSearch
            | SinkKind::Webhook
            | SinkKind::SplunkHec => true,
        }
    }
}

/// Whether a person can open a payload from this sink right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplayState {
    /// The destination can serve it and the credential to do so exists.
    Available,
    /// The destination can serve it; nobody has granted a read token.
    /// **One permission away, not a limitation** — and the message says so
    /// rather than blaming the destination for a choice an admin made.
    NeedsReadCredential,
    /// Nothing to read back: a file, a plugin, a webhook.
    NotSupported,
}

impl ReplayState {
    pub fn as_str(self) -> &'static str {
        match self {
            ReplayState::Available => "available",
            ReplayState::NeedsReadCredential => "needs-read-credential",
            ReplayState::NotSupported => "not-supported",
        }
    }

    /// What the picker and the record panel both say.
    pub fn message(self) -> &'static str {
        match self {
            ReplayState::Available => "Director can open past job output from here.",
            ReplayState::NeedsReadCredential => {
                "replay available — needs a read credential. The token that writes here is not \
                 used for reading, so grant one scoped to read."
            }
            ReplayState::NotSupported => {
                "Director cannot read this back — browse it where it landed."
            }
        }
    }
}

/// The triple B3 consumes and B2 prints (M31 §14.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkCapabilities {
    /// Rows land somewhere durable that outlives the mesh.
    pub archives: bool,
    /// Records can be listed and filtered from the destination.
    ///
    /// **Never used to render Director's History** (§18 fence 2) — the
    /// index is answered from local tiers, always. This exists so a picker
    /// can say what a destination is *good for*, not so anything reads
    /// through it.
    pub browse_records: bool,
    /// One payload can be fetched by id, because a person clicked it.
    pub replay_payloads: bool,
}

/// Which tables a sink carries.
///
/// A set of flags rather than a `Vec<String>`: a table added later is a
/// field with a default, and a table name typo is a compile error rather
/// than a sink that silently exports nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkTables {
    pub jobs: bool,
    pub usage: bool,
    /// The audit log. Off by default and worth one sentence: an audit
    /// trail leaving the mesh is the most sensitive export there is, and
    /// it should be a thing somebody ticked.
    pub audit: bool,
}

impl Default for SinkTables {
    fn default() -> Self {
        SinkTables {
            jobs: true,
            usage: false,
            audit: false,
        }
    }
}

impl SinkTables {
    pub fn any(&self) -> bool {
        self.jobs || self.usage || self.audit
    }

    /// The tables as an operator reads them back.
    pub fn names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.jobs {
            v.push("jobs");
        }
        if self.usage {
            v.push("usage");
        }
        if self.audit {
            v.push("audit");
        }
        v
    }

    /// Parse `jobs,audit`.
    pub fn parse(csv: &str) -> Result<SinkTables, String> {
        let mut t = SinkTables {
            jobs: false,
            usage: false,
            audit: false,
        };
        for name in csv.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match name {
                "jobs" => t.jobs = true,
                "usage" => t.usage = true,
                "audit" => t.audit = true,
                other => {
                    return Err(format!(
                        "'{other}' is not a table — try jobs, usage or audit"
                    ));
                }
            }
        }
        if !t.any() {
            return Err("a sink with no tables would export nothing".into());
        }
        Ok(t)
    }
}

/// How records are batched out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkRecords {
    /// Rows per batch.
    pub batch: u32,
    /// Seconds between passes. A batch goes early when it is full.
    pub every_secs: u64,
}

impl Default for SinkRecords {
    fn default() -> Self {
        SinkRecords {
            batch: 1_000,
            every_secs: 30,
        }
    }
}

/// What a sink does when the destination will not take its rows.
///
/// **The whole of C3, in one struct.** The buyer's request is always
/// *"guarantee delivery — block the job if the sink is unreachable"*, and
/// the answer is no: a sink spends spool, never scheduler. What they
/// actually want is a gate on **deletion**, which A1's retention window owns
/// and §19 Q3 answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SinkOnFailure {
    /// Bytes of on-disk spool before the oldest is dropped and counted.
    pub spool_bytes: u64,
    /// Retry backoff bounds, in seconds. Jittered between them.
    pub retry_min_secs: u64,
    pub retry_max_secs: u64,
    /// **The retention gate, and its ceiling** (§19 Q3, decided in B2).
    ///
    /// Days past the retention window that a closed record is held back
    /// from compaction while this sink has not yet exported it. `0` — the
    /// default — is no gate at all.
    ///
    /// *"Don't delete until it is exported"* is a reasonable request and
    /// this is it. **The ceiling is not optional**, and §18 decision 03 is
    /// why: at the ceiling meshware keeps deleting and says loudly that the
    /// export gap is now permanent. Anything else trades the mesh's health
    /// for a warehouse's completeness — a stalled sink would pin the
    /// op-log open on every node in the fleet, for ever, which is C3
    /// inverted and arriving through the back door.
    ///
    /// It is also what bounds a *hostile* claim. The watermark is
    /// node-authored and unsigned; a compromised exporter that reported no
    /// progress could otherwise deny deletion mesh-wide. With a ceiling the
    /// worst it can do is delay it by this many days.
    #[serde(default)]
    pub hold_for_export_days: u32,
}

impl Default for SinkOnFailure {
    fn default() -> Self {
        SinkOnFailure {
            // 256 MB: enough to ride out an afternoon of a warehouse being
            // down at a busy team's rate, small enough that it is never the
            // reason a disk fills.
            spool_bytes: 256 * 1024 * 1024,
            retry_min_secs: 15,
            retry_max_secs: 600,
            // Off. A gate nobody asked for that quietly grows the op-log is
            // the opposite of what track A is for, so this is something an
            // operator turns on when their compliance story needs it.
            hold_for_export_days: 0,
        }
    }
}

/// Payload export (B2). Declared here so the record's shape does not move
/// when B2 lands; **off by default**, and Q2 decides whether a per-kind
/// default is more honest than one global one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SinkPayloads {
    pub enabled: bool,
    /// Refuse to export a payload larger than this. `0` = no ceiling.
    pub max_bytes: u64,
}

/// A sink: a standing, admin-signed instruction to copy history somewhere
/// else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sink {
    /// Unique. The handle every command and audit entry uses.
    pub name: String,
    pub kind: SinkKind,
    /// Destination URL, for the kinds that have one. Empty for `ndjson`
    /// (which writes under the node's home) and for `plugin` (whose
    /// argument is the executable).
    pub url: String,
    /// For `plugin`: the executable. For a database kind: the database.
    pub database: String,
    /// A `{{secret:…}}` **reference**, never a literal (C6).
    ///
    /// It travels as a reference in the signed record and is resolved at
    /// connect time by the exporting node — so the token is never in the
    /// op-log, never in Director's memory, and never in
    /// `/proc/<pid>/cmdline`. A literal here would put a credential in a
    /// replicated, signed, permanently-audited record.
    pub credential: String,
    /// A second `{{secret:…}}` reference, for **reading a payload back**
    /// (M31 B3).
    ///
    /// Separate because the token that may `INSERT` should not have to
    /// `SELECT`, and an operator who scoped one narrowly should not have
    /// that quietly widened by a feature they did not ask for.
    ///
    /// **Absent means replay is refused, and says so** (§19 Q10, decided).
    /// The alternative — falling back to the write credential — is
    /// convenient and silently grants read to a token an admin deliberately
    /// scoped for writing. The picker reads *"replay available — needs a
    /// read credential"*, which sends the operator to the permission they
    /// have to grant rather than to a limitation they cannot fix.
    #[serde(default)]
    pub read_credential: String,
    pub tables: SinkTables,
    pub records: SinkRecords,
    pub payloads: SinkPayloads,
    pub on_failure: SinkOnFailure,
    /// Tag selector naming the machines that may export this sink.
    ///
    /// **The control that matters in an enterprise.** In most real
    /// deployments only some machines have a route to the warehouse — a
    /// firewall exception, a VPC, a proxy — and `run_on: tag=sink` is how
    /// an operator says *"export from a machine that can actually reach
    /// it"*. Empty means all live executors.
    pub run_on: String,
    /// Paused sinks stay in state, keep their cursor, and export nothing.
    ///
    /// **`meshnode restore` sets this on every recovered sink** (Q5,
    /// decided): a restored mesh must not silently re-push years of history
    /// into a live warehouse, and a *forked* one must not push into a
    /// warehouse that belongs to the mesh it forked from. An admin
    /// re-enables what they meant to keep.
    pub enabled: bool,
    pub description: String,
    pub created_by: UserId,
    pub created_at_micros: u64,
    /// Ed25519 signature by `created_by` over [`Self::signing_bytes`].
    pub signature: Vec<u8>,
}

impl Sink {
    /// The bytes the admin's signature covers: the whole record with the
    /// signature blanked, exactly as a schedule does it.
    ///
    /// `enabled` is inside the signature, like a schedule's `paused`:
    /// enabling a sink is an act with the same consequence as creating one,
    /// and it should be as provable.
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(b"mw-sink-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    pub fn capabilities(&self) -> SinkCapabilities {
        self.kind.capabilities()
    }

    /// Why this sink may not be created, if it may not.
    ///
    /// Everything checkable without the world: naming, the tables, and the
    /// credential being a reference rather than a literal. The
    /// **`egress_allow`** check is deliberately *not* here — it is a
    /// property of the node doing the exporting, checked locally at export
    /// time regardless of what any signed record says, which is what makes
    /// it a governance control rather than a suggestion.
    pub fn refusal(&self) -> Option<String> {
        if !is_valid_sink_name(&self.name) {
            return Some(format!(
                "'{}' is not a sink name — letters, digits, dot, dash and underscore, \
                 up to 64 characters",
                self.name
            ));
        }
        if !self.tables.any() {
            return Some("a sink with no tables would export nothing".into());
        }
        // Ahead of every other check: a retired kind should be told it is
        // retired, not handed a complaint about a field it will never get
        // to use.
        if let Some(why) = self.kind.retirement() {
            return Some(why.to_string());
        }
        if self.kind == SinkKind::Plugin && self.database.trim().is_empty() {
            return Some("a plugin sink needs an executable (--plugin <path>)".into());
        }
        // An NDJSON sink may name the file it writes. It is resolved on the
        // *exporting* node, which is usually not the machine the path was
        // typed on, so a relative path would mean a different place on
        // every node that ever runs the export — refuse it here rather than
        // let one sink scatter files across a fleet.
        if self.kind == SinkKind::Ndjson {
            let chosen = self.database.trim();
            if !chosen.is_empty() && !std::path::Path::new(chosen).is_absolute() {
                return Some(format!(
                    "'{chosen}' is not an absolute path — an NDJSON file is opened by whichever \
                     node exports, so it has to name the same place on every one of them \
                     (leave it blank to write inside the exporting node's own data directory)"
                ));
            }
        }
        if self.kind.needs_url() && self.url.trim().is_empty() {
            return Some(format!("a {} sink needs a --url", self.kind.as_str()));
        }
        // C6, and the check that matters most in this function. A literal
        // here would put a live credential into a replicated, signed,
        // permanently-audited record that every node in the mesh stores.
        if !self.credential.is_empty() && !is_secret_reference(&self.credential) {
            return Some(
                "a sink credential must be a {{secret:name}} reference, never a literal — \
                 the record is replicated to every node and never compacted away"
                    .into(),
            );
        }
        if self.records.batch == 0 {
            return Some("a batch of zero rows would never export anything".into());
        }
        if self.on_failure.hold_for_export_days > MAX_HOLD_FOR_EXPORT_DAYS {
            return Some(format!(
                "a sink may hold deletion back by at most {MAX_HOLD_FOR_EXPORT_DAYS} days \
                 ({} was asked for) — past that the mesh keeps deleting and reports the export \
                 gap as permanent, because a stalled sink must never pin the op-log open on \
                 every node for ever",
                self.on_failure.hold_for_export_days
            ));
        }
        None
    }
}

/// Is this a `{{secret:…}}` reference rather than a literal?
pub fn is_secret_reference(value: &str) -> bool {
    let t = value.trim();
    t.starts_with("{{secret:") && t.ends_with("}}") && t.len() > "{{secret:}}".len()
}

/// Signing bytes for a sink removal. Mirrors
/// [`crate::schedule::schedule_remove_signing_bytes`].
pub fn sink_remove_signing_bytes(name: &str, by: &UserId) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(b"mw-sink-remove-v1");
    buf.extend_from_slice(name.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&by.0);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A valid sink to vary one field of.
    ///
    /// `Plugin` rather than the retired `Ndjson` it used to be: the fixture
    /// has to pass `refusal()` for a test about *some other* field to mean
    /// anything, and plugin is the cheapest kind that needs no URL.
    fn sink(name: &str) -> Sink {
        Sink {
            name: name.into(),
            kind: SinkKind::Plugin,
            url: String::new(),
            database: "/usr/local/bin/mw-ship".into(),
            credential: String::new(),
            read_credential: String::new(),
            tables: SinkTables::default(),
            records: SinkRecords::default(),
            payloads: SinkPayloads::default(),
            on_failure: SinkOnFailure::default(),
            run_on: String::new(),
            enabled: true,
            description: String::new(),
            created_by: UserId([7u8; 32]),
            created_at_micros: 0,
            signature: Vec::new(),
        }
    }

    /// C6, and the most important assertion in this file: a live credential
    /// must never reach a replicated, signed, never-compacted record.
    #[test]
    fn a_literal_credential_is_refused_and_a_reference_is_not() {
        let mut s = sink("warehouse");
        s.credential = "hunter2".into();
        let why = s.refusal().expect("a literal must be refused");
        assert!(why.contains("{{secret:"), "{why}");
        assert!(why.contains("replicated"), "{why}");

        s.credential = "{{secret:warehouse-token}}".into();
        assert!(s.refusal().is_none());

        // Empty is fine: a file sink needs no credential at all.
        s.credential = String::new();
        assert!(s.refusal().is_none());
    }

    #[test]
    fn a_sink_with_no_tables_exports_nothing_and_says_so() {
        let mut s = sink("warehouse");
        s.tables = SinkTables {
            jobs: false,
            usage: false,
            audit: false,
        };
        assert!(s.refusal().is_some());
        assert!(SinkTables::parse("").is_err());
        assert!(SinkTables::parse("nonsense").is_err());
        assert_eq!(
            SinkTables::parse("jobs,audit").expect("parse").names(),
            vec!["jobs", "audit"]
        );
    }

    /// The audit table is off by default. An audit trail leaving the mesh
    /// is the most sensitive export there is, and it should be a thing
    /// somebody ticked.
    #[test]
    fn the_audit_table_is_off_until_somebody_ticks_it() {
        let t = SinkTables::default();
        assert!(t.jobs);
        assert!(!t.audit);
        assert!(!t.usage);
    }

    #[test]
    fn a_plugin_sink_needs_an_executable() {
        let mut s = sink("shipper");
        s.kind = SinkKind::Plugin;
        s.database = String::new();
        assert!(s.refusal().expect("no executable").contains("--plugin"));
        s.database = "/usr/local/bin/mw-ship".into();
        assert!(s.refusal().is_none());
    }

    /// **The retirement contract, in one test.**
    ///
    /// A retired kind is refused on the way in and readable on the way out,
    /// and both halves matter. Refusing it is the decision; still decoding
    /// it is what keeps every sink already signed into a mesh's op-log
    /// legible — including the ones an operator now needs to find and
    /// remove.
    #[test]
    fn a_retired_kind_is_refused_to_new_sinks_and_still_readable_on_old_ones() {
        let mut s = sink("warehouse");
        s.kind = SinkKind::Ndjson;
        let why = s.refusal().expect("a retired kind is refused");
        assert!(why.contains("retired"), "{why}");
        // The refusal names somewhere to go. "Unsupported" would leave an
        // operator with an export to build and no idea what to build it on.
        assert!(why.contains("plugin"), "{why}");

        // Enabled or not, named well or badly, it is still refused: the
        // retirement is not a validation step that a tidier record passes.
        s.enabled = false;
        assert!(s.refusal().is_some());

        // And the record itself still round-trips, which is the half that
        // keeps existing meshes working.
        assert_eq!(SinkKind::parse("ndjson"), Some(SinkKind::Ndjson));
        assert_eq!(SinkKind::Ndjson.as_str(), "ndjson");
        assert!(SinkKind::Ndjson.retirement().is_some());
        assert!(
            SinkKind::ALL.iter().all(|k| k.retirement().is_none()),
            "nothing offered may be retired"
        );
    }

    #[test]
    fn a_bad_name_is_refused_before_anything_is_signed() {
        assert!(sink("").refusal().is_some());
        assert!(sink("has space").refusal().is_some());
        assert!(sink(&"x".repeat(65)).refusal().is_some());
        assert!(sink("warehouse-eu.1").refusal().is_none());
    }

    /// Every destination declares what a person can get back out of it,
    /// **and says so in a sentence a picker can show**. The test derives
    /// from `ALL`, so a variant added without either decision fails here as
    /// well as failing to compile.
    #[test]
    fn every_destination_declares_its_capabilities() {
        for kind in SinkKind::ALL {
            let c = kind.capabilities();

            // **The index is never remote** (§18 fence 2). No destination
            // may claim to browse records, however good its query
            // language — the moment History is painted from a warehouse we
            // have shipped a control plane.
            assert!(
                !c.browse_records,
                "{} claims to browse records; the index is answered locally, always",
                kind.as_str()
            );

            // §14.4, and the reason it is a B2 requirement rather than a B3
            // one: B2 is where the first customer chooses, and a picker
            // that stays silent has already misled them.
            let sentence = kind.capability_sentence();
            assert!(
                sentence.len() > 40,
                "{} says nothing useful at selection",
                kind.as_str()
            );
            // The sentence and the triple must agree, in both directions.
            // A destination that cannot be read must say so; one that can
            // must not hedge.
            if c.replay_payloads {
                assert!(
                    sentence.contains("can open past job output"),
                    "{} can replay and does not say so",
                    kind.as_str()
                );
                assert!(
                    !sentence.contains("cannot read it back"),
                    "{} contradicts its own triple",
                    kind.as_str()
                );
            } else {
                assert!(
                    sentence.contains("cannot read it back"),
                    "{} implies a read it cannot do",
                    kind.as_str()
                );
            }
        }

        // The three B2 destinations that B3 can read back, and the three
        // that never will be — a file, a program and a URL have no read
        // semantic to implement.
        for kind in [
            SinkKind::ClickHouse,
            SinkKind::VictoriaLogs,
            SinkKind::OpenSearch,
        ] {
            assert!(kind.capabilities().replay_payloads, "{}", kind.as_str());
        }
        for kind in [SinkKind::Ndjson, SinkKind::Plugin, SinkKind::Webhook] {
            assert!(!kind.capabilities().replay_payloads, "{}", kind.as_str());
        }

        // Two destinations do not claim to archive, for two different
        // reasons. A webhook claims nothing at all, because nothing is
        // knowable about the other end of it. OTLP claims nothing because
        // it is *instrumentation*: a trace of a workflow is not a copy of
        // the history, spans expire on the observability stack's own
        // schedule, and a mesh that treated it as an archive would let a
        // retention window delete the only durable copy.
        const NOT_ARCHIVES: [SinkKind; 2] = [SinkKind::Webhook, SinkKind::Otlp];
        for kind in NOT_ARCHIVES {
            assert!(!kind.capabilities().archives, "{}", kind.as_str());
        }
        // Everything else at least stores what it is given.
        for kind in SinkKind::ALL
            .into_iter()
            .filter(|k| !NOT_ARCHIVES.contains(k))
        {
            assert!(kind.capabilities().archives, "{}", kind.as_str());
        }
    }

    /// **Q2, decided: a per-kind payload default.** A log store is what raw
    /// stdout is for; a warehouse and an unknown webhook are not. One global
    /// default would have been either too timid for VictoriaLogs or too
    /// reckless for everything else — and the reckless direction ends with a
    /// 4 GB core dump on a per-GB bill.
    #[test]
    fn payload_export_defaults_on_only_where_it_belongs() {
        assert!(SinkKind::VictoriaLogs.payloads_by_default());
        for kind in SinkKind::ALL
            .into_iter()
            .filter(|k| *k != SinkKind::VictoriaLogs)
        {
            assert!(
                !kind.payloads_by_default(),
                "{} would ship raw stdout without anybody asking",
                kind.as_str()
            );
        }
    }

    /// Only the HTTP kinds leave the machine, and `egress_allow` keys off
    /// that rather than off an empty URL — so a kind added later cannot be
    /// quietly exempt from the allowlist.
    #[test]
    fn only_the_http_kinds_leave_the_machine() {
        for kind in [
            SinkKind::ClickHouse,
            SinkKind::VictoriaLogs,
            SinkKind::OpenSearch,
            SinkKind::Webhook,
        ] {
            assert!(kind.is_http(), "{}", kind.as_str());
            assert!(kind.needs_url(), "{}", kind.as_str());
        }
        for kind in [SinkKind::Ndjson, SinkKind::Plugin] {
            assert!(!kind.is_http(), "{}", kind.as_str());
            assert!(!kind.needs_url(), "{}", kind.as_str());
        }
    }

    /// **§19 Q10, decided: refuse rather than fall back.** A destination
    /// that can replay but has no read credential is *one permission away*,
    /// and saying "not supported" would send the operator to the wrong
    /// place — the destination's docs, rather than their own grant.
    #[test]
    fn replay_without_a_read_credential_is_refused_and_says_what_is_missing() {
        assert_eq!(
            SinkKind::ClickHouse.replay_state(false),
            ReplayState::NeedsReadCredential
        );
        assert_eq!(
            SinkKind::ClickHouse.replay_state(true),
            ReplayState::Available
        );
        // A file has nothing to grant a credential for.
        assert_eq!(
            SinkKind::Ndjson.replay_state(true),
            ReplayState::NotSupported
        );

        let msg = ReplayState::NeedsReadCredential.message();
        assert!(msg.contains("read credential"), "{msg}");
        // It must name the permission, not the destination.
        assert!(msg.contains("grant"), "{msg}");
        assert!(!msg.contains("not supported"), "{msg}");
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in SinkKind::ALL {
            assert_eq!(SinkKind::parse(kind.as_str()), Some(kind));
            for alias in kind.aliases() {
                assert_eq!(SinkKind::parse(alias), Some(kind), "alias {alias}");
            }
        }
        // **A retired kind still parses**, and this assertion is now the
        // one holding that promise: every sink record ever signed carries
        // this name, and a mesh that cannot read one back cannot tell an
        // operator what it is refusing or let them remove it.
        assert_eq!(SinkKind::parse("NDJSON"), Some(SinkKind::Ndjson));
        assert!(
            !SinkKind::ALL.contains(&SinkKind::Ndjson),
            "retired, so nothing that offers a choice may offer it"
        );
        // They speak the same `_bulk` API; making an operator discover that
        // by reading our enum would be a small, avoidable insult.
        assert_eq!(SinkKind::parse("elasticsearch"), Some(SinkKind::OpenSearch));
        assert_eq!(SinkKind::parse("nothing-like-this"), None);
    }

    /// A sink is signed over its whole record, `enabled` included — so
    /// enabling one is as provable as creating one, which is what Q5's
    /// "restore disables sinks" answer depends on.
    #[test]
    fn enabling_a_sink_changes_what_must_be_signed() {
        let mut s = sink("warehouse");
        let enabled = s.signing_bytes();
        s.enabled = false;
        assert_ne!(enabled, s.signing_bytes());
    }

    #[test]
    fn a_secret_reference_is_recognised_and_a_near_miss_is_not() {
        assert!(is_secret_reference("{{secret:token}}"));
        assert!(is_secret_reference("  {{secret:a}}  "));
        assert!(!is_secret_reference("{{secret:}}"));
        assert!(!is_secret_reference("{{token}}"));
        assert!(!is_secret_reference("secret:token"));
        assert!(!is_secret_reference(""));
    }
}
