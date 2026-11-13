//! Facts: what each machine reports about itself, and the queries over it.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2).

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// Resolve a subject argument the way a person types it: a node name, a
    /// node id prefix, or empty for this node.
    pub(super) fn resolve_fact_subject(
        &self,
        text: &str,
    ) -> Option<(mesh_proto::FactSubject, String)> {
        let text = text.trim();
        if text.is_empty() {
            let me = self.node_id();
            return Some((mesh_proto::FactSubject::Node(me), self.member_name(&me)));
        }
        self.inner.engine.read(|s| {
            // **Live members first.** A departed or revoked member keeps
            // its record forever — a revocation tombstone has to persist,
            // or the revoked key could rejoin — so a machine that was
            // revoked and re-paired leaves two members carrying one name,
            // and the second one is the live one. Taking whichever the map
            // happened to yield first meant an operator could be handed the
            // dead identity, which has no facts and never will: the panel
            // said "no facts yet" about a node that had published perfectly
            // good ones, and nothing anywhere said why. This is the same
            // rule the session resolver already applies to `shell <node>`.
            let live = |m: &&crate::state::MemberEntry| s.is_active_member(&m.info.node_id);
            let by_name = |m: &&crate::state::MemberEntry| m.info.name == text;
            let by_prefix = |m: &&crate::state::MemberEntry| {
                m.info.node_id.to_display_string().starts_with(text)
            };
            // A dead member is still answerable by name or id once no live
            // one matches — "what was that machine" is a fair
            // question, and refusing it would lose the history the op-log
            // still holds.
            let found = s
                .members
                .values()
                .filter(live)
                .find(by_name)
                .or_else(|| s.members.values().filter(live).find(by_prefix))
                .or_else(|| s.members.values().find(by_name))
                .or_else(|| s.members.values().find(by_prefix));
            found.map(|entry| {
                (
                    mesh_proto::FactSubject::Node(entry.info.node_id),
                    entry.info.name.clone(),
                )
            })
        })
    }

    /// Turn one stored entry into a row.
    pub(super) fn fact_row(
        &self,
        state: &crate::state::MeshState,
        subject: &mesh_proto::FactSubject,
        subject_name: &str,
        collector: &str,
        entry: &crate::state::FactEntry,
    ) -> mesh_proto::FactRow {
        mesh_proto::FactRow {
            subject: subject.clone(),
            subject_name: subject_name.to_string(),
            collector: collector.to_string(),
            doc: entry.doc.clone(),
            digest: data_encoding::HEXLOWER.encode(&entry.digest),
            collected_at_micros: entry.collected_at_micros,
            published_at_micros: entry.updated_hlc.wall_micros,
            author: entry.author,
            author_name: state
                .members
                .get(&entry.author)
                .map(|m| m.info.name.clone())
                .unwrap_or_else(|| entry.author.short()),
        }
    }

    /// Every observation, as `FactRow`s — the archive pass's read (§4.8).
    ///
    /// Answered from the derived index, which was itself rebuilt from the
    /// op-log at boot. So an archive built here holds every observation
    /// this node ever merged, not merely what it believes now.
    pub(super) fn ipc_facts_export(&self, since_micros: Option<u64>, limit: u32) -> IpcResponse {
        let Some(history) = self.inner.history.as_ref() else {
            return IpcResponse::Facts(Vec::new());
        };
        match history.fact_export(since_micros, limit) {
            Ok(observations) => {
                let me = self.node_id();
                IpcResponse::Facts(
                    observations
                        .into_iter()
                        .map(|(subject, collector, when, doc)| mesh_proto::FactRow {
                            // The index is keyed by label, and a label is
                            // all the archive needs — reconstructing a
                            // `NodeId` for a node that has since left would
                            // be inventing one.
                            subject: mesh_proto::FactSubject::Host(subject.clone()),
                            subject_name: subject,
                            collector,
                            digest: data_encoding::HEXLOWER
                                .encode(blake3::hash(doc.as_bytes()).as_bytes()),
                            doc,
                            collected_at_micros: when,
                            published_at_micros: when,
                            author: me,
                            author_name: self.member_name(&me),
                        })
                        .collect(),
                )
            }
            Err(e) => IpcResponse::Error {
                message: format!("reading fact history failed: {e}"),
            },
        }
    }

    pub(super) fn ipc_facts(&self, subject: &str, collector: Option<&str>) -> IpcResponse {
        let Some((subject, name)) = self.resolve_fact_subject(subject) else {
            return IpcResponse::Error {
                message: format!(
                    "no node named {subject:?} — `meshnode nodes` lists what there is"
                ),
            };
        };
        let rows = self.inner.engine.read(|s| {
            s.facts
                .get(&subject)
                .map(|by| {
                    by.iter()
                        .filter(|(name, _)| collector.is_none_or(|c| *name == c))
                        .map(|(c, e)| self.fact_row(s, &subject, &name, c, e))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        });
        IpcResponse::Facts(rows)
    }

    /// Answer a `fact.*` expression across every subject this node knows.
    ///
    /// The **unknown** list is not an afterthought: "nine of your six
    /// hundred boxes do not say what kernel they run" is the answer
    /// somebody needs *before* patching, and a query that silently omitted
    /// them would read as "all six hundred are fine".
    pub(super) fn ipc_facts_query(&self, expression: &str, collector: Option<&str>) -> IpcResponse {
        let test = match crate::facts::FactTest::parse(expression) {
            Ok(t) => t,
            Err(message) => return IpcResponse::Error { message },
        };
        let (mut subjects, mut unknown) = (Vec::new(), Vec::new());
        self.inner.engine.read(|s| {
            for (subject, by_collector) in &s.facts {
                let name = self.fact_subject_name(s, subject);
                let mut seen = None;
                let mut matched = false;
                for (collector_name, entry) in by_collector {
                    if collector.is_some_and(|c| collector_name != c) {
                        continue;
                    }
                    if let Some(value) = entry.get(&test.key) {
                        if test.matches(&value) {
                            matched = true;
                        }
                        seen = Some(value);
                    }
                }
                // `!fact.<key>?` is the one question a subject without the
                // key answers — and answers *yes* — so it is not unknown.
                match (test.op, matched, seen) {
                    (crate::facts::FactOp::IsNotSet, _, None) => {
                        subjects.push((name, "(not set)".to_string()))
                    }
                    (crate::facts::FactOp::IsNotSet, _, Some(_)) => {}
                    (_, true, Some(value)) => subjects.push((name, value)),
                    (_, false, Some(_)) => {}
                    (_, _, None) => unknown.push(name),
                }
            }
        });
        subjects.sort();
        unknown.sort();
        IpcResponse::FactsMatched {
            expression: expression.to_string(),
            subjects,
            unknown,
        }
    }

    pub(super) fn fact_subject_name(
        &self,
        state: &crate::state::MeshState,
        subject: &mesh_proto::FactSubject,
    ) -> String {
        match subject {
            mesh_proto::FactSubject::Node(id) => state
                .members
                .get(id)
                .map(|m| m.info.name.clone())
                .unwrap_or_else(|| id.short()),
            mesh_proto::FactSubject::Host(name) => name.clone(),
        }
    }

    pub(super) fn ipc_facts_diff(
        &self,
        subject: &str,
        collector: Option<&str>,
        since_micros: Option<u64>,
        limit: u32,
    ) -> IpcResponse {
        let Some((subject, name)) = self.resolve_fact_subject(subject) else {
            return IpcResponse::Error {
                message: format!("no node named {subject:?}"),
            };
        };
        let Some(history) = self.inner.history.as_ref() else {
            return IpcResponse::Error {
                message: "the history index is disabled on this node \
                          ([history] enabled = false), and fact history lives in it"
                    .to_string(),
            };
        };
        match history.fact_diff(&subject, &name, collector, since_micros, limit) {
            Ok(rows) => IpcResponse::FactsDiff(rows),
            Err(e) => IpcResponse::Error {
                message: format!("reading fact history failed: {e}"),
            },
        }
    }
}
