//! The local request dispatcher: one arm per `IpcRequest`.
//!
//! Split out of `daemon.rs` on 2026-09-20 (CUT-2). `handle_authed` was a
//! single 3,709-line method — by some margin the largest in the tree — and
//! it is one `match`, so the seam is the method boundary itself.

// `use super::*` rather than a hand-written import list: this is a
// continuation of `mod.rs`'s `impl Node`, moved for size alone, and a
// second list of the same imports is a second thing to keep in step.
use super::*;

impl Node {
    /// Handle a single-response request from an authenticated user.
    pub(super) async fn handle_authed(
        &self,
        user: UserId,
        env: &SignedIpc,
        req: IpcRequest,
        source: Source,
    ) -> IpcResponse {
        macro_rules! gate {
            ($roles:expr, $action:expr, $target:expr, $audit:expr) => {
                if let Err(resp) = self.gate(&user, $roles, $action, $target, env, $audit) {
                    return *resp;
                }
            };
        }
        match req {
            IpcRequest::WhoAmI => IpcResponse::WhoAmI(self.whoami(&user)),
            IpcRequest::Status => {
                gate!(rbac::READ, "status", "", false);
                IpcResponse::Status(self.status())
            }
            IpcRequest::IdentityShow => {
                gate!(rbac::READ, "identity", "", false);
                IpcResponse::Identity(self.identity_info())
            }
            IpcRequest::PingPeer { addr } => {
                gate!(rbac::READ, "ping", &addr, false);
                self.ipc_ping(&addr).await
            }
            IpcRequest::PingNode { node } => {
                gate!(rbac::READ, "ping", &node.to_display_string(), false);
                self.ipc_ping_node(node).await
            }
            IpcRequest::InviteBootstrap => {
                gate!(rbac::ADMIN_ONLY, "invite.create", "", true);
                IpcResponse::InviteBootstrap {
                    mesh_id: self.inner.engine.genesis().body.mesh_id,
                    node_id: self.node_id(),
                    addrs: self.reachable_addrs(),
                }
            }
            IpcRequest::InvitePublish { public } => {
                gate!(rbac::ADMIN_ONLY, "invite.create", "", true);
                if public.created_by_user != user {
                    return IpcResponse::Error {
                        message: "invite is signed by a different user".into(),
                    };
                }
                let invite_id = public.invite_id.to_display_string();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::InviteCreate { invite: public })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "invite.create", &invite_id, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- SEC-17: an invite can be taken back ----------------------
            //
            // An invite is a bearer credential with a lifetime, and until
            // now the only way to end one early was to wait. Listing is an
            // admin read: the rows name who minted what, and how many
            // strangers each can still let in.
            IpcRequest::Invites => {
                gate!(rbac::ADMIN_ONLY, "invite.list", "", false);
                IpcResponse::Invites(self.invite_rows())
            }
            IpcRequest::InviteRevoke {
                invite_id,
                by,
                signature,
            } => {
                let target = invite_id.to_display_string();
                gate!(rbac::ADMIN_ONLY, "invite.revoke", &target, true);
                if by != user {
                    return IpcResponse::Denied {
                        message: "an invite revoke must be signed by the user submitting it".into(),
                    };
                }
                let signed = <[u8; 64]>::try_from(signature.as_slice()).is_ok_and(|sig| {
                    verify_signature(
                        &by.0,
                        &mesh_proto::invite_revoke_signing_bytes(&invite_id, &by),
                        &sig,
                    )
                });
                if !signed {
                    return IpcResponse::Error {
                        message: "invalid invite revoke signature".into(),
                    };
                }
                // Refused rather than held when this node has never heard
                // of it: the CLI resolves the id from this node's own list,
                // so an unknown one is a typo, and a typo should say so.
                match self
                    .inner
                    .engine
                    .read(|s| s.invites.get(&invite_id).map(|i| i.revoked))
                {
                    None => {
                        return IpcResponse::Error {
                            message: format!("no invite {target} on this mesh"),
                        };
                    }
                    Some(true) => return IpcResponse::Ok,
                    Some(false) => {}
                }
                match self.inner.engine.author_op(OpBody::InviteRevoke {
                    invite_id,
                    by,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "invite.revoke", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Nodes => {
                gate!(rbac::READ, "nodes", "", false);
                IpcResponse::Nodes(self.node_rows())
            }
            IpcRequest::NodeRevoke { revocation } => {
                gate!(rbac::ADMIN_ONLY, "node.revoke", "", true);
                if revocation.by != user {
                    return IpcResponse::Error {
                        message: "revocation is signed by a different user".into(),
                    };
                }
                let target = revocation.node_id.to_display_string();
                let reason = revocation.reason.clone();
                match membership::revoke_node(&self.inner.engine, &self.inner.transport, revocation)
                {
                    Ok(()) => {
                        self.audit_ok(&user, "node.revoke", &target, &reason, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- the owner's veto over the fleet ---------------------------
            //
            // Admin-only and audited, because it is a statement about what
            // the mesh *is*: a machine that has joined, and whose person
            // may well be a perfectly good user, but whose cores the owner
            // has not agreed to run work on. The refusal it produces names
            // the reason wherever it is shown, so nobody has to guess why a
            // node with sixteen idle cores is never picked.
            IpcRequest::NodeContributeSet { decision } => {
                let target = decision.node_id.to_display_string();
                gate!(rbac::ADMIN_ONLY, "node.contribute", &target, true);
                if decision.decided_by != user {
                    return IpcResponse::Denied {
                        message: "a contribute decision must be signed by the user submitting it"
                            .into(),
                    };
                }
                let known = self
                    .inner
                    .engine
                    .read(|s| s.members.contains_key(&decision.node_id));
                if !known {
                    return IpcResponse::Error {
                        message: format!("no member {target} — nothing to decide about"),
                    };
                }
                let detail = if decision.allowed {
                    "may run work".to_string()
                } else if decision.reason.trim().is_empty() {
                    "may not run work".to_string()
                } else {
                    format!("may not run work: {}", decision.reason)
                };
                match self
                    .inner
                    .engine
                    .author_op(OpBody::NodeExecutionSet { decision: *decision })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "node.contribute", &target, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::UserAdd { record } => {
                gate!(rbac::ADMIN_ONLY, "user.add", &record.username, true);
                if record.added_by != user {
                    return IpcResponse::Error {
                        message: "user record is signed by a different admin".into(),
                    };
                }
                let taken = self.inner.engine.read(|s| {
                    s.users.values().any(|u| {
                        u.record.username == record.username && u.record.user_id != record.user_id
                    })
                });
                if taken {
                    return IpcResponse::Error {
                        message: format!("username {:?} is already taken", record.username),
                    };
                }
                let username = record.username.clone();
                let target = record.user_id.to_display_string();
                match self.inner.engine.author_op(OpBody::UserAdd { record }) {
                    Ok(_) => {
                        self.audit_ok(&user, "user.add", &target, &username, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::UserAttachKey { record } => {
                gate!(rbac::ADMIN_ONLY, "user.attach", &record.username, true);
                if record.added_by != user {
                    return IpcResponse::Error {
                        message: "user record is signed by a different admin".into(),
                    };
                }
                // The username must exist and be somebody's — attaching a
                // key to a name nobody holds is `UserAdd` wearing a hat,
                // and a revoked name is not a person any more.
                let (held, live) = self.inner.engine.read(|s| {
                    let holders: Vec<_> = s
                        .users
                        .values()
                        .filter(|u| u.record.username == record.username)
                        .collect();
                    (
                        !holders.is_empty(),
                        holders.iter().any(|u| u.revoked.is_none()),
                    )
                });
                if !held {
                    return IpcResponse::Error {
                        message: format!(
                            "no user is called {:?} — add them instead",
                            record.username
                        ),
                    };
                }
                if !live {
                    return IpcResponse::Error {
                        message: format!(
                            "{:?} is revoked; a revoked person does not get a new key",
                            record.username
                        ),
                    };
                }
                let already = self
                    .inner
                    .engine
                    .read(|s| s.users.contains_key(&record.user_id));
                if already {
                    return IpcResponse::Error {
                        message: "that key is already a user".into(),
                    };
                }
                let username = record.username.clone();
                let target = record.user_id.to_display_string();
                match self.inner.engine.author_op(OpBody::UserAdd { record }) {
                    Ok(_) => {
                        self.audit_ok(&user, "user.attach", &target, &username, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Users => {
                gate!(rbac::READ, "users", "", false);
                IpcResponse::Users(self.user_rows())
            }
            // -- machine principals (E1-P4) --------------------------------
            IpcRequest::ServiceAccountAdd { account } => {
                gate!(
                    rbac::ADMIN_ONLY,
                    "serviceaccount.add",
                    &account.name,
                    true
                );
                if account.created_by != user {
                    return IpcResponse::Error {
                        message: "service account is signed by a different admin".into(),
                    };
                }
                // The marker describes a principal; it does not create one.
                // Refusing here rather than parking on merge means a typo
                // in the key is a message on the terminal instead of an op
                // that sits unapplied on every node for ever.
                let known = self
                    .inner
                    .engine
                    .read(|s| s.users.contains_key(&account.user_id));
                if !known {
                    return IpcResponse::Error {
                        message: format!(
                            "no user {} — publish the UserAdd first; a service account marks an \
                             existing principal as a machine rather than creating one",
                            account.user_id.short()
                        ),
                    };
                }
                // A principal that already holds admin cannot be demoted
                // into a machine: the grant is live and this would leave
                // exactly the combination the refusal exists to prevent.
                let is_admin = self.inner.engine.read(|s| {
                    rbac::roles_at(
                        s,
                        &self.inner.engine.genesis().body.founder_user,
                        &account.user_id,
                        mesh_proto::now_micros(),
                    )
                    .contains(&mesh_proto::Role::Admin)
                });
                if is_admin {
                    return IpcResponse::Error {
                        message: format!(
                            "{} already holds the admin role, and a service account may not. \
                             Revoke that grant first if this key really should become a machine \
                             principal",
                            account.name
                        ),
                    };
                }
                let name = account.name.clone();
                let target = account.user_id.to_display_string();
                let detail = match account.expires_at_micros {
                    Some(e) => format!("{name} expires {}", crate::localtime::describe_instant(e)),
                    None => format!("{name} never expires"),
                };
                match self
                    .inner
                    .engine
                    .author_op(OpBody::ServiceAccountAdd { account })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "serviceaccount.add", &target, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ServiceAccounts => {
                gate!(rbac::READ, "serviceaccounts", "", false);
                IpcResponse::ServiceAccounts(self.service_account_rows())
            }
            // -- identity-provider bridge (E1-P5) --------------------------
            IpcRequest::OidcProviderPut { provider } => {
                gate!(rbac::ADMIN_ONLY, "oidc.provider.put", &provider.name, true);
                if provider.configured_by != user {
                    return IpcResponse::Error {
                        message: "provider record is signed by a different admin".into(),
                    };
                }
                for (what, url) in [
                    ("jwks_uri", &provider.jwks_uri),
                    (
                        "device_authorization_endpoint",
                        &provider.device_authorization_endpoint,
                    ),
                    ("token_endpoint", &provider.token_endpoint),
                ] {
                    if !url.starts_with("https://") {
                        return IpcResponse::Error {
                            message: format!(
                                "{what} must be https — meshware will not fetch signing keys or \
                                 exchange a token over an insecure transport"
                            ),
                        };
                    }
                }
                let name = provider.name.clone();
                let issuer = provider.issuer.clone();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::OidcProviderPut { provider })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "oidc.provider.put", &name, &issuer, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::OidcProviderRemove {
                name,
                by,
                at_micros,
                signature,
            } => {
                gate!(rbac::ADMIN_ONLY, "oidc.provider.remove", &name, true);
                if by != user {
                    return IpcResponse::Error {
                        message: "removal is signed by a different admin".into(),
                    };
                }
                let target = name.clone();
                match self.inner.engine.author_op(OpBody::OidcProviderRemove {
                    name,
                    by,
                    at_micros,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "oidc.provider.remove", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- quarantine (E2.4) -----------------------------------------
            IpcRequest::NodeQuarantine { quarantine } => {
                let verb = if quarantine.quarantined {
                    "node.quarantine"
                } else {
                    "node.quarantine.lift"
                };
                gate!(rbac::ADMIN_ONLY, verb, "", true);
                if quarantine.by != user {
                    return IpcResponse::Error {
                        message: "quarantine is signed by a different admin".into(),
                    };
                }
                if quarantine.quarantined && quarantine.reason.trim().is_empty() {
                    return IpcResponse::Error {
                        message: "quarantine needs a --reason. A node nobody may use, with no \
                                  reason attached, is a support ticket somebody opens at 3am"
                            .into(),
                    };
                }
                let known = self
                    .inner
                    .engine
                    .read(|s| s.members.contains_key(&quarantine.node_id));
                if !known {
                    return IpcResponse::Error {
                        message: format!("no member {}", quarantine.node_id.short()),
                    };
                }
                let target = quarantine.node_id.to_display_string();
                let detail = quarantine.reason.clone();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::NodeQuarantineSet { quarantine })
                {
                    Ok(_) => {
                        self.audit_ok(&user, verb, &target, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- assigned tags ---------------------------------------------
            IpcRequest::NodeTags { assignment } => {
                gate!(
                    rbac::ADMIN_ONLY,
                    "node.tags.assign",
                    &assignment.node_id.to_display_string(),
                    true
                );
                if assignment.by != user {
                    return IpcResponse::Error {
                        message: "the tag assignment is signed by a different admin".into(),
                    };
                }
                // Refused here, with the reason, rather than filtered
                // silently: an operator who typed `cap.cores=64` believes
                // they have changed something, and a list that quietly
                // came back one shorter is how they find out three weeks
                // later that placement never worked.
                for tag in &assignment.tags {
                    if let Err(why) = mesh_proto::assignable_tag(tag) {
                        return IpcResponse::Error { message: why };
                    }
                    if tag.trim().is_empty() || tag.trim() != tag {
                        return IpcResponse::Error {
                            message: format!(
                                "`{tag}` is not a usable tag — a selector is written `tag={}`, so a tag with surrounding space or none at all can never be selected",
                                tag.trim()
                            ),
                        };
                    }
                }
                let known = self
                    .inner
                    .engine
                    .read(|s| s.members.contains_key(&assignment.node_id));
                if !known {
                    return IpcResponse::Error {
                        message: format!("no member {}", assignment.node_id.short()),
                    };
                }
                let target = assignment.node_id.to_display_string();
                let detail = if assignment.tags.is_empty() {
                    "cleared".to_string()
                } else {
                    assignment.tags.join(",")
                };
                match self
                    .inner
                    .engine
                    .author_op(OpBody::NodeTagsSet { assignment })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "node.tags.assign", &target, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::AssignedTags { node_id } => {
                gate!(rbac::READ, "nodes", &node_id.to_display_string(), false);
                self.inner.engine.read(|s| match s.assigned(&node_id) {
                    Some(a) => IpcResponse::AssignedTags {
                        tags: a.tags.clone(),
                        by: Some(a.by),
                        at_micros: a.at_micros,
                    },
                    None => IpcResponse::AssignedTags {
                        tags: Vec::new(),
                        by: None,
                        at_micros: 0,
                    },
                })
            }
            // -- key rotation (E2.3) ---------------------------------------
            IpcRequest::UserRotateKey {
                username,
                record,
                grants,
                revocation,
            } => {
                gate!(rbac::ADMIN_ONLY, "user.rotate", &username, true);
                for who in [record.added_by, revocation.revoked_by] {
                    if who != user {
                        return IpcResponse::Error {
                            message: "the rotation is signed by a different admin".into(),
                        };
                    }
                }
                if grants.iter().any(|g| g.granted_by != user) {
                    return IpcResponse::Error {
                        message: "a carried grant is signed by a different admin".into(),
                    };
                }
                if record.username != username {
                    return IpcResponse::Error {
                        message: "the new key must be bound to the same username".into(),
                    };
                }
                if record.user_id == revocation.user_id {
                    return IpcResponse::Error {
                        message: "the new key is the old key — nothing to rotate".into(),
                    };
                }
                let founder = self.inner.engine.genesis().body.founder_user;
                if revocation.user_id.0 == founder {
                    return IpcResponse::Error {
                        message: "the founding admin's key is the mesh's trust anchor and \
                                  cannot be rotated this way"
                            .into(),
                    };
                }
                let (holds_old, old_live, new_taken) = self.inner.engine.read(|s| {
                    (
                        s.users
                            .get(&revocation.user_id)
                            .is_some_and(|u| u.record.username == username),
                        s.users
                            .get(&revocation.user_id)
                            .is_some_and(|u| u.revoked.is_none()),
                        s.users.contains_key(&record.user_id),
                    )
                });
                if !holds_old {
                    return IpcResponse::Error {
                        message: format!("{username:?} does not hold that key"),
                    };
                }
                if !old_live {
                    return IpcResponse::Error {
                        message: format!(
                            "{username:?} is already revoked — a revoked person does not get a \
                             new key; add them again"
                        ),
                    };
                }
                if new_taken {
                    return IpcResponse::Error {
                        message: "the new key is already a user".into(),
                    };
                }

                // **Order is the property.** Bind, then grant, then revoke.
                // At no point between the first op and the last is the
                // person unable to act: the old key keeps working until the
                // new one is fully granted, and only then is it retired.
                // The reverse order locks somebody out of their own mesh.
                let old = revocation.user_id.to_display_string();
                let new = record.user_id.to_display_string();
                if let Err(e) = self
                    .inner
                    .engine
                    .author_op(OpBody::UserAdd { record: *record })
                {
                    return IpcResponse::Error {
                        message: format!("binding the new key failed: {e}"),
                    };
                }
                let carried = grants.len();
                for grant in grants {
                    if let Err(e) = self.inner.engine.author_op(OpBody::GrantAdd { grant }) {
                        return IpcResponse::Error {
                            message: format!(
                                "the new key is bound but a grant did not carry over ({e}) — \
                                 the old key still works; grant the new one by hand and revoke \
                                 the old with `meshnode users revoke`"
                            ),
                        };
                    }
                }
                match self.inner.engine.author_op(OpBody::UserRevoke {
                    revocation: *revocation,
                }) {
                    Ok(_) => {
                        self.audit_ok(
                            &user,
                            "user.rotate",
                            &username,
                            &format!("{old} → {new}, {carried} grant(s) carried"),
                            env,
                        );
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: format!(
                            "the new key is bound and granted but the old one is still live \
                             ({e}) — revoke it with `meshnode users revoke`"
                        ),
                    },
                }
            }
            IpcRequest::OidcProviders => {
                gate!(rbac::READ, "oidc.providers", "", false);
                IpcResponse::OidcProviders(self.inner.engine.read(|s| {
                    s.oidc_providers
                        .values()
                        .map(|(p, _)| mesh_proto::OidcProviderRow {
                            name: p.name.clone(),
                            issuer: p.issuer.clone(),
                            client_id: p.client_id.clone(),
                            groups_claim: p.groups_claim.clone(),
                            group_roles: p
                                .group_roles
                                .iter()
                                .map(|gr| format!("{}={}", gr.group, gr.role.as_str()))
                                .collect(),
                            configured_at_micros: p.configured_at_micros,
                        })
                        .collect()
                }))
            }
            IpcRequest::OidcLoginBegin { provider } => {
                // Anyone may *begin* a login: the caller is by definition
                // somebody who is not yet a user, and refusing them would
                // make the feature unreachable by the people it is for.
                // Nothing is authored until a token verifies.
                let Some(p) = self
                    .inner
                    .engine
                    .read(|s| s.oidc_providers.get(&provider).map(|(p, _)| p.clone()))
                else {
                    return IpcResponse::Error {
                        message: format!(
                            "no identity provider named {provider:?} — `meshnode oidc providers` \
                             lists the ones this mesh believes"
                        ),
                    };
                };
                let http = match crate::httpc::Client::new(false, 1 << 20) {
                    Ok(c) => c,
                    Err(e) => {
                        return IpcResponse::Error {
                            message: format!("no https client: {e}"),
                        };
                    }
                };
                // Each begin is a call to the provider, on behalf of a key
                // that may have been made a moment ago (SEC-19). A user
                // with a role is accountable for what they ask for; anybody
                // else draws on a per-address and a node-wide budget.
                if self.is_anonymous(&user)
                    && (!self.inner.anon.login_source.admit(&source.key())
                        || !self.inner.anon.login_all.admit("all"))
                {
                    return IpcResponse::Error {
                        message: format!(
                            "{}sign-ins started — try again in a minute",
                            crate::throttle::TOO_MANY
                        ),
                    };
                }
                match crate::oidc::begin_device_login(&http, &p, &user).await {
                    Ok(d) => {
                        self.inner
                            .anon
                            .login_started(&d.device_code, user, d.expires_in);
                        IpcResponse::OidcLoginStarted {
                            user_code: d.user_code,
                            verification_uri: d.verification_uri,
                            verification_uri_complete: d.verification_uri_complete,
                            device_code: d.device_code,
                            interval: d.interval,
                            expires_in: d.expires_in,
                        }
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::OidcLoginPoll {
                provider,
                device_code,
                interval,
            } => {
                // Only a sign-in this node started, for this key, and no
                // faster than a person's client polls: otherwise a poll is a
                // way to make this node call the provider with any code at
                // all (SEC-19).
                if let Err(message) = self.inner.anon.may_poll(&device_code, &user) {
                    return IpcResponse::Error { message };
                }
                let Some(p) = self
                    .inner
                    .engine
                    .read(|s| s.oidc_providers.get(&provider).map(|(p, _)| p.clone()))
                else {
                    return IpcResponse::Error {
                        message: format!("no identity provider named {provider:?}"),
                    };
                };
                let http = match crate::httpc::Client::new(false, 1 << 20) {
                    Ok(c) => c,
                    Err(e) => {
                        return IpcResponse::Error {
                            message: format!("no https client: {e}"),
                        };
                    }
                };
                let outcome =
                    crate::oidc::poll_device_login(&http, &p, &device_code, interval).await;
                let token = match outcome {
                    Ok(crate::oidc::PollOutcome::Pending { interval }) => {
                        return IpcResponse::OidcLoginPending { interval };
                    }
                    Ok(crate::oidc::PollOutcome::Failed(why)) => {
                        self.inner.anon.login_finished(&device_code);
                        return IpcResponse::Error { message: why };
                    }
                    Ok(crate::oidc::PollOutcome::Token(t)) => {
                        self.inner.anon.login_finished(&device_code);
                        t
                    }
                    Err(e) => {
                        return IpcResponse::Error {
                            message: e.to_string(),
                        };
                    }
                };
                // The **node** verifies, against the provider record an
                // admin signed and keys it fetched itself. A token the CLI
                // handed over is a string until this succeeds.
                let now_secs = mesh_proto::now_micros() / 1_000_000;
                let assertion = match crate::oidc::verify_id_token(
                    &http,
                    &self.inner.jwks,
                    &p,
                    &token,
                    &user,
                    self.inner.engine.node_id(),
                    now_secs,
                )
                .await
                {
                    Ok(a) => a,
                    Err(e) => {
                        return IpcResponse::Error {
                            message: e.to_string(),
                        };
                    }
                };
                let view = IpcResponse::OidcLoginVerified {
                    subject: assertion.subject.clone(),
                    email: assertion.email.clone(),
                    display_name: assertion.display_name.clone(),
                    groups: assertion.groups.clone(),
                    suggested_role: assertion.suggested_role.map(|r| r.as_str().to_string()),
                };
                let detail = format!("{} via {}", assertion.subject, assertion.provider);
                match self
                    .inner
                    .engine
                    .author_op(OpBody::OidcAssertionPut { assertion })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "oidc.login", &detail, "verified", env);
                        view
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::UserRevoke { revocation } => {
                gate!(rbac::ADMIN_ONLY, "user.revoke", "", true);
                if revocation.revoked_by != user {
                    return IpcResponse::Error {
                        message: "revocation is signed by a different admin".into(),
                    };
                }
                if revocation.user_id.0 == self.inner.engine.genesis().body.founder_user {
                    return IpcResponse::Error {
                        message: "the founding admin cannot be revoked".into(),
                    };
                }
                let target = revocation.user_id.to_display_string();
                let reason = revocation.reason.clone();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::UserRevoke { revocation })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "user.revoke", &target, &reason, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::GrantAdd { grant } => {
                gate!(rbac::ADMIN_ONLY, "grant.add", "", true);
                if grant.granted_by != user {
                    return IpcResponse::Error {
                        message: "grant is signed by a different admin".into(),
                    };
                }
                let founder = self.inner.engine.genesis().body.founder_user;
                let known = grant.user_id.0 == founder
                    || self
                        .inner
                        .engine
                        .read(|s| s.users.contains_key(&grant.user_id));
                if !known {
                    return IpcResponse::Error {
                        message: "grantee is not a known user — add them first".into(),
                    };
                }
                // A service account may not hold admin (E1-P4). Refused
                // here as well as at merge: the merge check is the one a
                // modified client cannot get around, and this one is the
                // one that gives a person a sentence instead of an op that
                // silently does nothing on every node.
                if let Err(why) = self
                    .inner
                    .engine
                    .read(|s| rbac::grant_is_permitted(s, &grant.user_id, grant.role))
                {
                    return IpcResponse::Error { message: why };
                }
                let detail = format!(
                    "{} on nodes={} services={}",
                    grant.role.as_str(),
                    grant.nodes,
                    grant.services
                );
                let target = grant.user_id.to_display_string();
                match self.inner.engine.author_op(OpBody::GrantAdd { grant }) {
                    Ok(_) => {
                        self.audit_ok(&user, "grant.add", &target, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Grants => {
                gate!(rbac::READ, "grants", "", false);
                IpcResponse::Grants(self.grant_rows())
            }
            IpcRequest::GrantRevoke { revocation } => {
                gate!(rbac::ADMIN_ONLY, "grant.revoke", "", true);
                if revocation.revoked_by != user {
                    return IpcResponse::Error {
                        message: "revocation is signed by a different admin".into(),
                    };
                }
                if !self
                    .inner
                    .engine
                    .read(|s| s.grants.contains_key(&revocation.grant_id))
                {
                    return IpcResponse::Error {
                        message: "no such grant".into(),
                    };
                }
                let target = revocation.grant_id.to_display_string();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::GrantRevoke { revocation })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "grant.revoke", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SecretPublish { record } => {
                gate!(rbac::ADMIN_ONLY, "secret.set", &record.name, true);
                if record.created_by != user {
                    return IpcResponse::Error {
                        message: "secret record is signed by a different user".into(),
                    };
                }
                let expected = self.inner.engine.read(|s| {
                    s.secrets
                        .get(&record.name)
                        .map(|e| e.record.version + 1)
                        .unwrap_or(1)
                });
                if record.version != expected {
                    return IpcResponse::Error {
                        message: format!(
                            "version conflict: expected {expected} for secret {:?} — retry",
                            record.name
                        ),
                    };
                }
                // Not sealed for a quarantined machine (SEC-16). The client
                // leaves them out; this is where one that did not is told.
                let quarantined: Vec<String> = self.inner.engine.read(|s| {
                    record
                        .wraps
                        .iter()
                        .filter(|w| s.is_quarantined(&w.node_id))
                        .map(|w| {
                            s.members
                                .get(&w.node_id)
                                .map(|m| m.info.name.clone())
                                .unwrap_or_else(|| w.node_id.short())
                        })
                        .collect()
                });
                if !quarantined.is_empty() {
                    return IpcResponse::Error {
                        message: format!(
                            "secret {:?} is sealed for quarantined node(s) {} — seal it again \
                             without them",
                            record.name,
                            quarantined.join(", ")
                        ),
                    };
                }
                let name = record.name.clone();
                let detail = format!(
                    "version {} for {} node(s)",
                    record.version,
                    record.wraps.len()
                );
                match self.inner.engine.author_op(OpBody::SecretPut { record }) {
                    Ok(_) => {
                        self.audit_ok(&user, "secret.set", &name, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Secrets => {
                gate!(rbac::SECRET_READ, "secrets", "", false);
                IpcResponse::Secrets(self.secret_rows())
            }
            // -- SEC-7: where a secret goes, and who may use it ----------------
            IpcRequest::SecretScopeSet { scope } => {
                gate!(rbac::ADMIN_ONLY, "secret.scope", &scope.name, true);
                if scope.by != user {
                    return IpcResponse::Denied {
                        message: "a secret scope must be signed by the user submitting it".into(),
                    };
                }
                let signed = <[u8; 64]>::try_from(scope.signature.as_slice())
                    .is_ok_and(|sig| verify_signature(&user.0, &scope.signing_bytes(), &sig));
                if !signed {
                    return IpcResponse::Error {
                        message: "invalid secret scope signature".into(),
                    };
                }
                if let Err(message) = Selector::check(&scope.nodes) {
                    return IpcResponse::Error { message };
                }
                let name = scope.name.clone();
                let detail = format!(
                    "nodes {:?}, {} user(s), roles {:?}",
                    scope.nodes,
                    scope.users.len(),
                    scope.roles.iter().map(|r| r.as_str()).collect::<Vec<_>>()
                );
                match self
                    .inner
                    .engine
                    .author_op(OpBody::SecretScopeSet { scope })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "secret.scope", &name, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SecretScopes => {
                gate!(rbac::SECRET_READ, "secrets", "", false);
                let founder = self.inner.engine.genesis().body.founder_user;
                IpcResponse::SecretScopes(self.inner.engine.read(|s| {
                    s.secret_scopes
                        .values()
                        .map(|sc| mesh_proto::SecretScopeRow {
                            name: sc.name.clone(),
                            nodes: sc.nodes.clone(),
                            users: sc
                                .users
                                .iter()
                                .map(|u| rbac::username_of(s, &founder, u))
                                .collect(),
                            roles: sc.roles.iter().map(|r| r.as_str().to_string()).collect(),
                            by: rbac::username_of(s, &founder, &sc.by),
                            at_micros: sc.at_micros,
                        })
                        .collect()
                }))
            }
            IpcRequest::SecretRecipients { name, nodes } => {
                gate!(rbac::ADMIN_ONLY, "secret.set", &name, false);
                IpcResponse::SecretRecipients(self.inner.engine.read(|s| {
                    s.members
                        .values()
                        .filter(|m| match &nodes {
                            Some(sel) => secrets::is_recipient_for(s, Some(sel), &m.info),
                            None => secrets::is_recipient(s, &name, &m.info),
                        })
                        .map(|m| m.info.node_id)
                        .collect()
                }))
            }
            IpcRequest::SecretRemove { removal } => {
                gate!(rbac::ADMIN_ONLY, "secret.remove", &removal.name, true);
                if removal.by != user {
                    return IpcResponse::Error {
                        message: "removal is signed by a different user".into(),
                    };
                }
                let exists = self
                    .inner
                    .engine
                    .read(|s| s.secrets.get(&removal.name).is_some_and(|e| !e.removed));
                if !exists {
                    return IpcResponse::Error {
                        message: format!("no secret named {:?}", removal.name),
                    };
                }
                let name = removal.name.clone();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::SecretRemove { removal })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "secret.remove", &name, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SecretProviders => {
                gate!(rbac::SECRET_READ, "secrets.providers", "", false);
                let rows = self
                    .inner
                    .config
                    .secret_providers
                    .iter()
                    .map(|(name, p)| ProviderRow {
                        name: name.clone(),
                        plugin: p.plugin.clone(),
                    })
                    .collect();
                IpcResponse::Providers(rows)
            }
            IpcRequest::AuditQuery {
                user: filter_user,
                action,
                since_micros,
                limit,
            } => {
                gate!(rbac::AUDIT_READ, "audit.query", "", false);
                // Nothing older than the mesh's audit window, wherever it
                // is still held (ENG-3): two nodes answer alike.
                let window_start = self
                    .inner
                    .engine
                    .retention()
                    .window(mesh_proto::RetentionTable::Audit)
                    .cutoff_micros(mesh_proto::now_micros());
                let filter = AuditFilter {
                    user: filter_user,
                    action,
                    since_micros: since_micros.max(window_start),
                    limit: limit as usize,
                };
                let mut entries = self.inner.engine.read(|s| audit::query(s, &filter));
                let want = audit::limit_of(&filter);
                if entries.len() < want {
                    entries.extend(self.older_audit(&filter, want - entries.len()));
                }
                IpcResponse::Audit(entries)
            }
            IpcRequest::Jobs => {
                gate!(rbac::READ, "jobs", "", false);
                IpcResponse::Jobs(self.job_rows())
            }
            IpcRequest::JobsQuery { query } => {
                gate!(rbac::READ, "jobs", "", false);
                IpcResponse::JobsPage(Box::new(self.jobs_page(&query)))
            }
            // **The cheapest request in the protocol, and that is its job**
            // (**H-8**). No collection is built and the state lock is never
            // taken, so a client can ask "did anything change?" on a tight
            // cadence and pay for the answer only when it is yes.
            IpcRequest::StateVersion => {
                gate!(rbac::READ, "state", "", false);
                IpcResponse::StateVersions(self.inner.engine.topic_versions())
            }
            IpcRequest::JobShow { job_id } => {
                gate!(rbac::READ, "jobs", "", false);
                match self.job_row(&job_id) {
                    Some(row) => IpcResponse::JobShow(row),
                    None => IpcResponse::Error {
                        message: format!("no such job: {}", job_id.short()),
                    },
                }
            }
            IpcRequest::JobsClear {
                before_micros,
                by,
                signature,
            } => {
                // Admin-only, and audited, for the reason a cache purge is
                // neither: this one does not cost re-execution, it costs the
                // record. Forgetting what ran is the kind of act the audit
                // log exists to keep, so the entry naming who cleared and
                // how much outlives the rows it removed.
                gate!(rbac::ADMIN_ONLY, "jobs.clear", "all", true);
                if by != user {
                    return IpcResponse::Error {
                        message: "clear is signed by a different admin".into(),
                    };
                }
                // Counted before the op, so the operator is told what was
                // actually forgotten rather than what the map held after
                // gossip had a chance to move.
                let gone = self.inner.engine.read(|s| {
                    s.jobs
                        .values()
                        .filter(|j| j.cleared_by(before_micros))
                        .count()
                });
                match self.inner.engine.author_op(OpBody::JobsClear {
                    before_micros,
                    by,
                    signature,
                }) {
                    Ok(_) => {
                        // This node's own spec and log files. Peers do the
                        // same for theirs when the op reaches them.
                        let records = self.inner.jobs.clear_finished(before_micros);
                        self.audit_ok(
                            &user,
                            "jobs.clear",
                            "all",
                            &format!("{gone} jobs, {records} local records"),
                            env,
                        );
                        IpcResponse::Notice {
                            message: format!(
                                "cleared {gone} finished job{}; running and queued work is untouched",
                                if gone == 1 { "" } else { "s" }
                            ),
                        }
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::JobCancel { job_id } => {
                let target = job_id.to_display_string();
                match self.job_submitter(&job_id) {
                    Some((submitter, submitter_name)) => {
                        if let Err(resp) = self.gate_cancel(
                            &user,
                            &submitter,
                            submitter_name.as_deref(),
                            false,
                            "job.cancel",
                            &target,
                            env,
                        ) {
                            return *resp;
                        }
                    }
                    // No summary yet (a job so young its upsert has not
                    // applied, or an id that never existed): the flat role
                    // gate still stands, so a viewer probing ids learns
                    // nothing, and `ipc_cancel` gives the honest answer.
                    None => {
                        gate!(rbac::RUNNERS, "job.cancel", &target, true);
                    }
                }
                let resp = self.ipc_cancel_as(job_id, Some(env)).await;
                if matches!(resp, IpcResponse::Ok) {
                    self.audit_ok(&user, "job.cancel", &target, "", env);
                }
                resp
            }
            IpcRequest::JobCancelForce {
                job_id,
                by,
                signature,
            } => {
                let target = job_id.to_display_string();
                if by != user {
                    return IpcResponse::Error {
                        message: "force-cancel is signed by a different user".into(),
                    };
                }
                let Some((submitter, submitter_name)) = self.job_submitter(&job_id) else {
                    gate!(rbac::RUNNERS, "job.cancel.force", &target, true);
                    return IpcResponse::Error {
                        message: "unknown job".into(),
                    };
                };
                // Escalate only as far as the caller's role reaches. The
                // verb asks for "cancel, no matter what", but its first
                // move is the *ordinary* cancel — so it is gated as one,
                // and a developer stopping their own healthy job through
                // this door is not asked for a power they do not need.
                if let Err(resp) = self.gate_cancel(
                    &user,
                    &submitter,
                    submitter_name.as_deref(),
                    false,
                    "job.cancel",
                    &target,
                    env,
                ) {
                    return *resp;
                }
                // The ordinary route first: an executor that can be told
                // kills the process and reports its own terminal state,
                // which is strictly the better record. Force exists for
                // when there is nobody left to tell.
                if matches!(self.ipc_cancel_as(job_id, Some(env)).await, IpcResponse::Ok) {
                    self.audit_ok(&user, "job.cancel", &target, "", env);
                    return IpcResponse::Notice {
                        message: "cancel requested — the executor will report it".into(),
                    };
                }
                match self
                    .inner
                    .engine
                    .read(|s| s.jobs.get(&job_id).map(|j| j.state))
                {
                    None => IpcResponse::Error {
                        message: "unknown job".into(),
                    },
                    Some(state) if state.is_terminal() => IpcResponse::Notice {
                        message: format!("job had already finished ({})", state.as_str()),
                    },
                    // Only now is this a *force*: the executor could not be
                    // told, and writing the record anyway is the power the
                    // gate re-checks — with the refusal saying both what
                    // failed and who can finish the job.
                    Some(_) => {
                        if let Err(resp) = self.gate_cancel(
                            &user,
                            &submitter,
                            submitter_name.as_deref(),
                            true,
                            "job.cancel.force",
                            &target,
                            env,
                        ) {
                            return match *resp {
                                IpcResponse::Denied { message } => IpcResponse::Denied {
                                    message: format!(
                                        "the executor could not be reached, and {message}"
                                    ),
                                },
                                other => other,
                            };
                        }
                        match self.inner.engine.author_op(OpBody::JobCancelForce {
                            job_id,
                            by,
                            signature,
                        }) {
                            Ok(_) => {
                                self.audit_ok(
                                    &user,
                                    "job.cancel.force",
                                    &target,
                                    "executor unreachable; recorded as cancelled",
                                    env,
                                );
                                IpcResponse::Notice {
                                    message: "the executor could not be reached — recorded as \
                                              cancelled mesh-wide; if the executor returns with \
                                              a real outcome, that wins"
                                        .into(),
                                }
                            }
                            Err(e) => IpcResponse::Error {
                                message: e.to_string(),
                            },
                        }
                    }
                }
            }
            IpcRequest::Push { path } => {
                gate!(rbac::CONTENT, "bundle.push", &path, true);
                let job = self.inner.jobs.acting_for(&env.user).map(|(job, _)| job);
                let resp = self.ipc_push(&path, job);
                if let IpcResponse::Pushed { bundle_id, .. } = &resp {
                    self.audit_ok(
                        &user,
                        "bundle.push",
                        &bundle_id.to_display_string(),
                        &path,
                        env,
                    );
                }
                resp
            }
            IpcRequest::Bundles => {
                gate!(rbac::READ, "bundles", "", false);
                IpcResponse::Bundles(self.bundle_rows())
            }
            // -- M31 A2 -------------------------------------------------
            //
            // Reading what a node holds is a read. *Reclaiming* is a
            // content act, gated like the door that pushed the bundle in:
            // deleting a node's local copy is the same class of decision as
            // putting it there, and neither of them changes what the mesh
            // believes — `bundles ls` still lists every bundle, holders and
            // all, because the catalogue is replicated and the bytes are
            // not.
            IpcRequest::ContentStatus => {
                gate!(rbac::READ, "content.status", "", false);
                IpcResponse::ContentStatus(Box::new(self.content_status()))
            }
            IpcRequest::ContentPrune {
                dry_run,
                aggressive,
            } => {
                gate!(
                    rbac::CONTENT,
                    if dry_run {
                        "content.prune.preview"
                    } else {
                        "content.prune"
                    },
                    if aggressive { "aggressive" } else { "" },
                    !dry_run
                );
                IpcResponse::ContentPrune(Box::new(self.content_prune(dry_run, aggressive)))
            }
            IpcRequest::DiskReport => {
                gate!(rbac::READ, "disk", "", false);
                IpcResponse::DiskReport(self.disk_rows())
            }
            IpcRequest::ServiceCreate {
                version,
                head_signature,
            } => {
                gate!(rbac::CONTENT, "service.create", &version.name, true);
                if version.created_by != user {
                    return IpcResponse::Error {
                        message: "service version is signed by a different user".into(),
                    };
                }
                if let Err(resp) =
                    self.gate_service_scope(&user, &version.name, "service.create", env)
                {
                    return *resp;
                }
                let name = version.name.clone();
                match services::create_service(&self.inner.engine, *version, head_signature) {
                    Ok(number) => {
                        self.audit_ok(&user, "service.create", &name, &format!("v{number}"), env);
                        IpcResponse::ServiceCreated {
                            name,
                            version: number,
                        }
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Services => {
                gate!(rbac::READ, "services", "", false);
                IpcResponse::Services(self.service_rows())
            }
            IpcRequest::ServiceCurrent { name } => {
                // What the caller is about to do with it is publish the next
                // version, so that is what reading it takes — `env` included,
                // which a mere reader of `ServiceShow` never sees.
                gate!(rbac::CONTENT, "service.create", &name, false);
                if let Err(resp) = self.gate_service_scope(&user, &name, "service.create", env) {
                    return *resp;
                }
                match self
                    .inner
                    .engine
                    .read(|st| st.service_current(&name).cloned())
                {
                    Some(version) => IpcResponse::ServiceCurrent(Box::new(version)),
                    None => IpcResponse::Error {
                        message: format!("no live service named {name}"),
                    },
                }
            }
            IpcRequest::ServiceShow { name } => {
                gate!(rbac::READ, "services", "", false);
                match self.service_row(&name) {
                    Some(row) => IpcResponse::ServiceShow(row),
                    None => IpcResponse::Error {
                        message: format!("no live service named {name}"),
                    },
                }
            }
            IpcRequest::ServiceRollback {
                name,
                version,
                by,
                signature,
            } => {
                gate!(rbac::CONTENT, "service.rollback", &name, true);
                if by != user {
                    return IpcResponse::Error {
                        message: "rollback is signed by a different user".into(),
                    };
                }
                if let Err(resp) = self.gate_service_scope(&user, &name, "service.rollback", env) {
                    return *resp;
                }
                match services::rollback_service(&self.inner.engine, &name, version, by, signature)
                {
                    Ok(()) => {
                        self.audit_ok(
                            &user,
                            "service.rollback",
                            &name,
                            &format!("v{version}"),
                            env,
                        );
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ServiceRemove {
                name,
                by,
                signature,
            } => {
                gate!(rbac::CONTENT, "service.remove", &name, true);
                if by != user {
                    return IpcResponse::Error {
                        message: "removal is signed by a different user".into(),
                    };
                }
                if let Err(resp) = self.gate_service_scope(&user, &name, "service.remove", env) {
                    return *resp;
                }
                match services::remove_service(&self.inner.engine, &name, by, signature) {
                    Ok(()) => {
                        self.audit_ok(&user, "service.remove", &name, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::PairDecide { session, accept } => {
                gate!(rbac::ADMIN_ONLY, "node.pair", "", true);
                let sender = self
                    .inner
                    .pair_sessions
                    .lock()
                    .remove(&session);
                match sender {
                    Some(tx) => {
                        let _ = tx.send(accept);
                        IpcResponse::Ok
                    }
                    None => IpcResponse::Error {
                        message: "no such pairing session (it may have timed out)".into(),
                    },
                }
            }
            IpcRequest::TagsUpdate { add, remove } => {
                gate!(rbac::ADMIN_ONLY, "node.tags", "", true);
                let me = self.node_id();
                let current = self
                    .inner
                    .engine
                    .read(|s| s.members.get(&me).map(|m| m.info.clone()));
                let Some(info) = current else {
                    return IpcResponse::Error {
                        message: "this node has no membership record yet".into(),
                    };
                };
                // Capacity tags are runtime-managed (M9 §5.1): a node
                // describes its own hardware, and nobody edits that by hand.
                if add
                    .iter()
                    .chain(remove.iter())
                    .any(|t| mesh_proto::is_capacity_tag(t))
                {
                    return IpcResponse::Error {
                        message: format!(
                            "{}* tags are managed by the node itself and cannot be set by hand",
                            mesh_proto::CAP_TAG_PREFIX
                        ),
                    };
                }
                let mut tags: Vec<String> = info
                    .tags
                    .into_iter()
                    .filter(|t| !remove.contains(t))
                    .collect();
                for t in add {
                    if !tags.contains(&t) {
                        tags.push(t);
                    }
                }
                let detail = tags.join(",");
                match self.inner.engine.author_op(OpBody::MemberUpdate {
                    node_id: me,
                    name: info.name,
                    addrs: info.addrs,
                    tags,
                    slots: info.slots,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "node.tags", &me.to_display_string(), &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::NodeAddrRepair { node_id, addrs } => {
                gate!(
                    rbac::ADMIN_ONLY,
                    "node.addr",
                    &node_id.to_display_string(),
                    true
                );
                self.ipc_node_addr_repair(&user, node_id, addrs, env)
            }
            IpcRequest::TargetPreview { selector } => {
                gate!(rbac::READ, "nodes", "", false);
                let sel = Selector::parse(&selector);
                let me = self.node_id();
                let matches = self.inner.engine.read(|s| {
                    membership::resolve_targets(s, &sel, me, |id| self.inner.gossip.liveness_of(id))
                });
                let ids: std::collections::BTreeSet<NodeId> =
                    matches.into_iter().map(|m| m.node_id).collect();
                IpcResponse::Nodes(
                    self.node_rows()
                        .into_iter()
                        .filter(|r| ids.contains(&r.node_id))
                        .collect(),
                )
            }
            IpcRequest::Top => {
                gate!(rbac::READ, "top", "", false);
                IpcResponse::Top(self.top_rows())
            }
            IpcRequest::Totals => {
                gate!(rbac::READ, "totals", "", false);
                IpcResponse::Totals(Box::new(self.totals()))
            }
            IpcRequest::Sources => {
                gate!(rbac::READ, "sources", "", false);
                IpcResponse::Sources(self.source_rows())
            }
            IpcRequest::SourceAdd { source } => {
                gate!(rbac::CONTENT, "source.add", &source.name, true);
                if source.created_by != user {
                    return IpcResponse::Error {
                        message: "source is signed by a different user".into(),
                    };
                }
                // The first of three (S5). This one produces the sentence a
                // person reads; the merge rule and `sync_source` repeat the
                // check for the client that skipped this door.
                if let Err(message) = source.validate() {
                    return IpcResponse::Error { message };
                }
                // A plain transport is this node operator's call (SEC-21),
                // and the watcher asks again before it fetches.
                if let Some(message) = gitsync::transport_refusal(&self.inner.config.git, &source.url)
                {
                    return IpcResponse::Error { message };
                }
                let name = source.name.clone();
                let detail = format!("{}@{}", source.url, source.branch);
                match self.inner.engine.author_op(OpBody::SourceAdd { source }) {
                    Ok(_) => {
                        self.audit_ok(&user, "source.add", &name, &detail, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SourceRemove {
                name,
                by,
                signature,
            } => {
                gate!(rbac::CONTENT, "source.remove", &name, true);
                if by != user {
                    return IpcResponse::Error {
                        message: "removal is signed by a different user".into(),
                    };
                }
                let exists = self
                    .inner
                    .engine
                    .read(|s| s.sources.get(&name).is_some_and(|e| !e.removed));
                if !exists {
                    return IpcResponse::Error {
                        message: format!("no git source named {name:?}"),
                    };
                }
                match self.inner.engine.author_op(OpBody::SourceRemove {
                    name: name.clone(),
                    by,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "source.remove", &name, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SourceSyncNow { name } => {
                gate!(rbac::CONTENT, "source.sync", &name, true);
                let source = self.inner.engine.read(|s| {
                    s.sources
                        .get(&name)
                        .filter(|e| !e.removed)
                        .map(|e| e.source.clone())
                });
                let Some(source) = source else {
                    return IpcResponse::Error {
                        message: format!("no git source named {name:?}"),
                    };
                };
                match gitsync::sync_source(
                    &self.inner.engine,
                    &self.inner.content,
                    &self.inner.config.data_dir,
                    &source,
                    &self.git_run(),
                )
                .await
                {
                    Ok(outcome) => {
                        self.audit_ok(&user, "source.sync", &name, &outcome.commit, env);
                        IpcResponse::SourceSync {
                            commit: outcome.commit,
                            bundle_id: outcome.bundle_id,
                            unchanged: outcome.unchanged,
                            bumped: outcome.bumped,
                        }
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Objects => {
                gate!(rbac::READ, "objects", "", false);
                IpcResponse::Objects(self.inner.objects.rows())
            }
            IpcRequest::ResultsCollected { job } => {
                gate!(rbac::READ, "objects", "", false);
                // Idempotent by construction: a run that retained nothing,
                // or was already released, releases nothing and says Ok.
                // The alternative — an error for "already collected" —
                // would make a retry of a partial fetch fail on the half
                // that worked.
                let n = self.inner.objects.release_results(&job);
                if n > 0 {
                    info!(job = %job.short(), objects = n, "results collected; released");
                }
                IpcResponse::Ok
            }
            IpcRequest::Backup { path } => {
                gate!(rbac::ADMIN_ONLY, "backup", &path, true);
                // Never into the node's own directory: a backup written over
                // `identity/` or `state/` would be the node overwriting
                // itself with a file of the caller's naming (SEC-3).
                if let Some(message) = self.backup_target_refusal(&path) {
                    return IpcResponse::Error { message };
                }
                // Streamed, and on a blocking thread (ENG-20). This reads
                // the whole op-log and writes it out; on an async worker it
                // parked one core for the length of a backup, and the
                // previous shape held four to five copies of the log in
                // memory while it did — on a node that is very often taking
                // a backup *because* something is already wrong with it.
                let engine = self.inner.engine.clone();
                let out = std::path::PathBuf::from(&path);
                let data_dir = self.inner.config.data_dir.clone();
                let node_name = self.inner.config.node_name.clone();
                let written = tokio::task::spawn_blocking(move || {
                    let genesis = engine.genesis().clone();
                    crate::backup::write_backup_streaming(
                        &out, &data_dir, &node_name, &genesis, &engine,
                    )
                })
                .await;
                let written = match written {
                    Ok(result) => result,
                    Err(e) => {
                        return IpcResponse::Error {
                            message: format!("backup task failed: {e}"),
                        };
                    }
                };
                match written {
                    Ok(report) => {
                        self.audit_ok(
                            &user,
                            "backup",
                            &path,
                            &format!("{} ops, {} bytes", report.ops, report.bytes),
                            env,
                        );
                        IpcResponse::BackupDone {
                            path: report.path.display().to_string(),
                            ops: report.ops,
                            bytes: report.bytes,
                        }
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::BundleFiles { bundle_id } => {
                gate!(rbac::READ, "bundles", "", false);
                match self.bundle_manifest(&bundle_id).await {
                    Ok(m) => IpcResponse::BundleFiles(
                        m.files
                            .iter()
                            .map(|f| mesh_proto::BundleFileRow {
                                path: f.path.clone(),
                                size: f.size,
                                mode: f.mode,
                                executable: f.mode & 0o111 != 0,
                                symlink: f.symlink.clone(),
                            })
                            .collect(),
                    ),
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::BundleFileRead { bundle_id, path } => {
                gate!(rbac::READ, "bundles", "", false);
                match self.read_bundle_file(&bundle_id, &path).await {
                    Ok(data) => IpcResponse::BundleFile { data },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- local history index (M14 §2) -------------------------------
            //
            // A read of derived local data, gated exactly like `runs` and
            // `jobs` are: the index holds nothing the op-log did not, so
            // anyone who may list runs may search them.
            IpcRequest::HistoryJobs(query) => {
                gate!(rbac::READ, "jobs", "", false);
                match &self.inner.history {
                    Some(h) => match self.history_jobs_spanning(h, &query).await {
                        Ok(page) => IpcResponse::HistoryJobs(Box::new(page)),
                        Err(e) => IpcResponse::Error {
                            message: format!("history query failed: {e}"),
                        },
                    },
                    None => IpcResponse::Error {
                        message: "the history index is disabled on this node \
                                  ([history] enabled = false)"
                            .to_string(),
                    },
                }
            }
            // -- M31 A3: what this node is holding back -------------------
            //
            // A read of this node's own diagnosis, gated like every other
            // read. Deliberately not admin-only: *"why was my workflow
            // refused"* is a question the person whose workflow it was has
            // to be able to answer for themselves, and a control they
            // cannot see is a control they will work around.
            // -- M31 B4: the audit trail, with its proofs ----------------
            //
            // `rbac::AUDIT_READ` rather than `READ`, exactly as `audit.query`
            // is: these rows carry every action every user took, and the
            // gate that governs reading them governs shipping them out.
            IpcRequest::AuditRows {
                since_micros,
                limit,
            } => {
                gate!(rbac::AUDIT_READ, "audit.rows", "", false);
                match self.inner.engine.signed_audit_rows(since_micros, limit) {
                    Ok(rows) => IpcResponse::AuditRows(rows),
                    Err(e) => IpcResponse::Error {
                        message: format!("reading the audit log: {e}"),
                    },
                }
            }
            IpcRequest::Backpressure => {
                gate!(rbac::READ, "state.backpressure", "", false);
                IpcResponse::Backpressure(Box::new(self.backpressure_view()))
            }
            // -- M31 B3 §14.3: reading one payload back ------------------
            //
            // Everything this handler has to get right, and each of them
            // is a test in §14:
            //
            // - **`rbac::READ` on the job**, exactly as a local read of the
            //   same output is. Bytes that left the mesh are still that
            //   job's output, and the grant that governs one governs both.
            // - **One audit entry per *open*, not per range.** `AuditAppend`
            //   is `Grow` and never compacts (§1.1), so a per-scroll entry
            //   would make reading a log permanently more expensive than
            //   running the job that wrote it. The tail read is the open:
            //   scrolling back asks for `from_end > 0` and writes nothing.
            // - **The node that exported it is the node that fetches it**
            //   (§6). Not Director — so a laptop with no route to the
            //   warehouse still opens the log, and the read is governed by
            //   the same `egress_allow` as the write that put it there.
            IpcRequest::HistoryReplay {
                job_id,
                from_end,
                limit,
                search,
            } => {
                gate!(rbac::READ, "jobs", &job_id.to_display_string(), false);
                let reply = self
                    .replay_payload(&job_id, from_end, limit, &search, env, &user)
                    .await;
                IpcResponse::PayloadWindow(Box::new(reply))
            }
            // -- M18 facts (§4.9) ----------------------------------------
            IpcRequest::Facts { subject, collector } => {
                gate!(rbac::READ, "facts", &subject, false);
                self.ipc_facts(&subject, collector.as_deref())
            }
            IpcRequest::FactsQuery {
                expression,
                collector,
            } => {
                gate!(rbac::READ, "facts", &expression, false);
                self.ipc_facts_query(&expression, collector.as_deref())
            }
            IpcRequest::FactsDiff {
                subject,
                collector,
                since_micros,
                limit,
            } => {
                gate!(rbac::READ, "facts", &subject, false);
                self.ipc_facts_diff(&subject, collector.as_deref(), since_micros, limit)
            }
            IpcRequest::FactCollectorPut { collector } => {
                // Deciding what a node reports about itself is operational
                // data, not governance (§4.5).
                gate!(rbac::COLLECTORS, "collector.put", &collector.name, true);
                match self.publish_collector(*collector) {
                    Ok(()) => IpcResponse::Ok,
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::FactCollectors => {
                gate!(rbac::READ, "collectors", "", false);
                IpcResponse::FactCollectors(self.collector_rows())
            }
            IpcRequest::FactsExport {
                since_micros,
                limit,
            } => {
                gate!(rbac::READ, "facts", "export", false);
                self.ipc_facts_export(since_micros, limit)
            }
            // -- M19 results (§5.3) --------------------------------------
            //
            // Answered from this node's own index and nowhere else. There
            // is no authority to ask for a global answer, and pretending
            // otherwise would be the first step toward needing one (M14) —
            // so a node that never owned the group says so, and names the
            // node that did.
            IpcRequest::Results { group, failed_only } => {
                gate!(rbac::READ, "results", &group.to_display_string(), false);
                self.ipc_results(group, failed_only)
            }
            IpcRequest::ParallelRecord { job_id } => {
                gate!(rbac::READ, "results", &job_id.to_display_string(), false);
                match self.inner.history.as_ref() {
                    Some(h) => match h.parallel_record(&job_id) {
                        Ok(Some(record)) => IpcResponse::ParallelRecord(Box::new(record)),
                        Ok(None) => {
                            // Name the owner when the summary is replicated
                            // here, because that is the node that has it.
                            let owner = self.inner.engine.read(|s| {
                                s.jobs.get(&job_id).map(|j| self.member_name(&j.submitter))
                            });
                            IpcResponse::Error {
                                message: match owner {
                                    Some(name) => format!(
                                        "this node holds no record of {} — it was owned by \
                                         {name}; ask there",
                                        job_id.short()
                                    ),
                                    None => format!(
                                        "no parallel run {} is known here",
                                        job_id.short()
                                    ),
                                },
                            }
                        }
                        Err(e) => IpcResponse::Error {
                            message: format!("reading the run record failed: {e}"),
                        },
                    },
                    None => IpcResponse::Error {
                        message: "this node keeps no history ([history] is off), so it has no \
                                  run records to resume from"
                            .into(),
                    },
                }
            }
            IpcRequest::DrainTimed {
                until_micros,
                reason,
                move_pending,
            } => {
                gate!(&[Role::Admin, Role::Operator], "node.drain", &reason, true);
                self.drain_timed(until_micros, reason, move_pending).await
            }
            IpcRequest::ResultsExport {
                since_micros,
                limit,
            } => {
                gate!(rbac::READ, "results", "export", false);
                match self.inner.history.as_ref() {
                    Some(h) => match h.result_sets(since_micros, limit) {
                        Ok(sets) => IpcResponse::ResultSets(sets),
                        Err(e) => IpcResponse::Error {
                            message: format!("reading results failed: {e}"),
                        },
                    },
                    None => IpcResponse::ResultSets(Vec::new()),
                }
            }
            // -- N6 §12 mesh-wide load shedding --------------------------
            IpcRequest::ClockAhead => {
                gate!(rbac::READ, "clock", "", false);
                // A quarter of an hour: long enough to still be there when
                // somebody runs `doctor` about it, short enough that a fixed
                // clock stops being reported without a restart.
                let ahead = self
                    .inner
                    .engine
                    .clock_ahead(Duration::from_secs(15 * 60));
                IpcResponse::ClockAhead(
                    ahead
                        .into_iter()
                        .map(|(node_id, ahead_micros)| mesh_proto::ClockAheadRow {
                            node_id,
                            name: self.member_name(&node_id),
                            ahead_micros,
                        })
                        .collect(),
                )
            }
            IpcRequest::ShedStatus => {
                gate!(rbac::READ, "shed.status", "", false);
                IpcResponse::Shed(Box::new(self.shed_view()))
            }
            IpcRequest::ShedSet { shed } => {
                gate!(
                    rbac::ADMIN_ONLY,
                    "shed.set",
                    if shed.sheds() { "on" } else { "off" },
                    true
                );
                if shed.created_by != user {
                    return IpcResponse::Error {
                        message: "the shed record is signed by a different user".into(),
                    };
                }
                let what = match (shed.submissions, shed.reconcile) {
                    (true, true) => "submissions+reconcile",
                    (true, false) => "submissions",
                    (false, true) => "reconcile",
                    (false, false) => "lifted",
                };
                match self.inner.engine.author_op(OpBody::ShedPut { shed }) {
                    Ok(_) => {
                        self.audit_ok(&user, "shed.set", what, "", env);
                        IpcResponse::Shed(Box::new(self.shed_view()))
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ReleaseCosign { cosign } => {
                gate!(rbac::ADMIN_ONLY, "release.cosign", &cosign.key(), true);
                if cosign.published_by != user {
                    return IpcResponse::Denied {
                        message: "a co-signature must be published by the user submitting it"
                            .into(),
                    };
                }
                // Checked against this mesh's trusted keys *and* against
                // the binary, when this node holds it. Merge checks the
                // first again on every node; only a node with the bytes
                // can check the second, which is why it is advice here and
                // enforcement at install time.
                let trusted = self.inner.engine.read(|s| s.trusted_ssh_release_keys());
                if trusted.is_empty() {
                    return IpcResponse::Denied {
                        message: "this mesh trusts no co-signing keys, so a co-signature would \
                                  verify against nothing. Trust one first: `meshnode upgrade \
                                  --trust-key <ssh-pubkey>`"
                            .into(),
                    };
                }
                if crate::sshsig::match_trusted(&cosign.armored, &trusted).is_none() {
                    return IpcResponse::Denied {
                        message: "that co-signature was not made by a key this mesh trusts for \
                                  releases, under the meshware-release namespace"
                            .into(),
                    };
                }
                let target = cosign.key();
                match self.inner.engine.author_op(OpBody::ReleaseCosign { cosign }) {
                    Ok(_) => {
                        self.audit_ok(&user, "release.cosign", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::AttestRoots => {
                // Which manufacturer CAs this mesh will believe is
                // configuration, and a stranger has no business reading it.
                // Shipped ungated by Y5 landmine 7 while every sibling read
                // is gated; `tests/authorization.rs` now says so.
                gate!(rbac::READ, "attest.roots", "", false);
                IpcResponse::AttestRoots(self.attest_root_rows())
            }
            IpcRequest::AttestRootAdd { root } => {
                gate!(rbac::ADMIN_ONLY, "attest.root.add", &root.label, true);
                if root.added_by != user {
                    return IpcResponse::Denied {
                        message: "an attestation root must be signed by the admin adding it"
                            .into(),
                    };
                }
                // Parsed here so a file that is not a certificate is a
                // sentence rather than an op every node silently drops.
                use x509_parser::prelude::FromDer as _;
                if x509_parser::certificate::X509Certificate::from_der(&root.certificate).is_err()
                {
                    return IpcResponse::Error {
                        message: "that file is not X.509 DER. A PEM file needs converting: \
                                  `openssl x509 -in ca.pem -outform der -out ca.der`"
                            .into(),
                    };
                }
                let label = root.label.clone();
                match self.inner.engine.author_op(OpBody::AttestRootAdd { root }) {
                    Ok(_) => {
                        self.audit_ok(&user, "attest.root.add", &label, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::AttestRootRemove {
                id,
                by,
                at_micros,
                signature,
            } => {
                gate!(rbac::ADMIN_ONLY, "attest.root.remove", &id, true);
                if by != user {
                    return IpcResponse::Denied {
                        message: "a removal must be signed by the admin submitting it".into(),
                    };
                }
                let target = id.clone();
                match self.inner.engine.author_op(OpBody::AttestRootRemove {
                    id,
                    by,
                    at_micros,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "attest.root.remove", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::UserHardwareAttest { attestation } => {
                // Not admin-gated, and deliberately: attesting a key is a
                // statement about your own identity. The op refuses one
                // whose subject is not the signer.
                gate!(rbac::READ, "identity.attest", &attestation.slot, true);
                if attestation.user_id != user {
                    return IpcResponse::Denied {
                        message: "an attestation can only be made about your own key".into(),
                    };
                }
                // Verified before publishing so the refusal names the
                // cause. Merge checks it again on every node, which is the
                // check that actually decides.
                let roots = self.inner.engine.read(|s| s.trusted_attest_roots());
                if let Err(refusal) = crate::attest::verify(&attestation, &roots) {
                    return IpcResponse::Denied {
                        message: refusal.message(),
                    };
                }
                let slot = attestation.slot.clone();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::UserHardwareAttest { attestation })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "identity.attest", &slot, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::UserHardwareRevoke { revocation } => {
                let target = revocation.user_id.to_display_string();
                let own = revocation.user_id == user;
                gate!(
                    if own { rbac::READ } else { rbac::ADMIN_ONLY },
                    "identity.attest.revoke",
                    &target,
                    true
                );
                if revocation.revoked_by != user {
                    return IpcResponse::Denied {
                        message: "a revocation must be signed by the user submitting it".into(),
                    };
                }
                let reason = revocation.reason.clone();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::UserHardwareRevoke { revocation })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "identity.attest.revoke", &target, &reason, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SessionPolicyPut { policy } => {
                gate!(rbac::ADMIN_ONLY, "session.policy.put", &policy.id, true);
                if policy.created_by != user {
                    return IpcResponse::Error {
                        message: "the policy is signed by a different user".into(),
                    };
                }
                if let Some(message) = policy.refusal() {
                    return IpcResponse::Error { message };
                }
                let id = policy.id.clone();
                match self
                    .inner
                    .engine
                    .author_op(OpBody::SessionPolicyPut { policy })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "session.policy.put", &id, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SessionPolicies => {
                gate!(rbac::READ, "session.policies", "", false);
                IpcResponse::SessionPolicies(self.session_policy_rows())
            }
            IpcRequest::SessionPolicyRemove { id, by, signature } => {
                gate!(rbac::ADMIN_ONLY, "session.policy.remove", &id, true);
                if by != user {
                    return IpcResponse::Error {
                        message: "the removal is signed by a different user".into(),
                    };
                }
                match self.inner.engine.author_op(OpBody::SessionPolicyRemove {
                    id: id.clone(),
                    by,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "session.policy.remove", &id, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- M23 enrolment (§3.1, §3.2) --------------------------
            //
            // Ungated on purpose. Every other door in this match denies an
            // unenrolled caller, which is correct and is also why the
            // product looks broken to a new person: they sign a request,
            // it is refused, and nothing tells them what to do. These
            // three doors are what "what to do" is.
            IpcRequest::EnrolSubmit { request } => {
                self.ipc_enrol_submit(&user, *request, env, source)
            }
            IpcRequest::EnrolStatus => {
                let (state, code, expires_at_micros, declined_by) = self.enrolment_status(&user);
                IpcResponse::EnrolStatus {
                    state,
                    code,
                    expires_at_micros,
                    declined_by,
                }
            }
            IpcRequest::Definitions => {
                gate!(rbac::READ, "definitions", "", false);
                IpcResponse::Definitions(self.definition_rows())
            }
            IpcRequest::Queue => {
                gate!(rbac::READ, "queue", "", false);
                IpcResponse::Queue(self.inner.jobs.queue_rows())
            }
            IpcRequest::StateShow => {
                gate!(rbac::READ, "state.show", "", false);
                match self.state_stats() {
                    Ok(stats) => IpcResponse::StateStats(stats),
                    Err(e) => IpcResponse::Error {
                        message: format!("state show: {e}"),
                    },
                }
            }
            // -- M31 B1 sinks ---------------------------------------------
            //
            // `rbac::ADMIN_ONLY` rather than the schedule set, and audited
            // by name. A sink copies every command line, every submitter
            // identity and — once B2 lands — every byte of stdout to a host
            // an operator typed into a text box, so *who pointed our
            // history at that host, and when* has to be a question
            // `meshnode audit` can answer.
            IpcRequest::SinkPut { sink } => {
                gate!(rbac::ADMIN_ONLY, "sink.create", &sink.name, true);
                if sink.created_by != user {
                    return IpcResponse::Denied {
                        message: "a sink must be signed by the user submitting it".into(),
                    };
                }
                if let Some(why) = sink.refusal() {
                    audit::record(
                        &self.inner.engine,
                        user,
                        "sink.create",
                        &sink.name,
                        "denied",
                        &why,
                        &env.signature,
                    );
                    return IpcResponse::Denied { message: why };
                }
                // The allowlist, checked **before signing anything reaches
                // the log**, and named in the refusal. It is checked again
                // on the exporting node at write time, because that node is
                // the only one that knows its own network — but refusing
                // here is what stops a sink existing that no node will ever
                // honour, and what puts the reason in front of the person
                // who typed the URL.
                if !sink.url.trim().is_empty() {
                    let host = url_host(&sink.url);
                    if !self.inner.config.integrations.allows_host(&host) {
                        let why = self.inner.config.integrations.refusal_for(&host);
                        audit::record(
                            &self.inner.engine,
                            user,
                            "sink.create",
                            &sink.name,
                            "denied",
                            &why,
                            &env.signature,
                        );
                        return IpcResponse::Denied { message: why };
                    }
                }
                let name = sink.name.clone();
                let replacing = self
                    .inner
                    .engine
                    .read(|s| s.sinks.get(&name).is_some_and(|e| !e.removed));
                let detail = format!(
                    "{} → {} [{}]",
                    sink.kind.as_str(),
                    if sink.url.trim().is_empty() {
                        sink.database.clone()
                    } else {
                        // The destination host in full, never abbreviated:
                        // this line is what an auditor reads a year later.
                        sink.url.clone()
                    },
                    sink.tables.names().join(",")
                );
                match self
                    .inner
                    .engine
                    .author_op(mesh_proto::OpBody::SinkPut { sink })
                {
                    Ok(_) => {
                        self.audit_ok(
                            &user,
                            "sink.create",
                            &name,
                            &format!(
                                "{} {detail}",
                                if replacing { "replaced" } else { "created" }
                            ),
                            env,
                        );
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::SinkRemove { name, signature } => {
                gate!(rbac::ADMIN_ONLY, "sink.remove", &name, true);
                if self
                    .inner
                    .engine
                    .read(|s| !s.sinks.get(&name).is_some_and(|e| !e.removed))
                {
                    return IpcResponse::Error {
                        message: format!("no live sink named {name}"),
                    };
                }
                match self.inner.engine.author_op(mesh_proto::OpBody::SinkRemove {
                    name: name.clone(),
                    by: user,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "sink.remove", &name, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Sinks => {
                gate!(rbac::READ, "sinks", "", false);
                IpcResponse::Sinks(self.sink_rows())
            }
            IpcRequest::SinkTest { sink } => {
                gate!(rbac::ADMIN_ONLY, "sink.test", &sink.name, false);
                if !sink.kind.leaves_the_machine() {
                    return IpcResponse::SinkTest {
                        ok: false,
                        status: format!(
                            "a {} sink has nothing to connect to — it writes locally",
                            sink.kind.as_str()
                        ),
                    };
                }
                // Postgres is reachable and is not HTTP, so `sink test`
                // cannot POST at it. Saying which command *does* check it
                // beats a 400 from a driver that never ran.
                if !sink.kind.is_http() {
                    return IpcResponse::SinkTest {
                        ok: false,
                        status: format!(
                            "a {} sink is checked by writing to it — `meshnode sinks ddl \
                             {}` prints the schema it needs, and the first export reports \
                             what the server said",
                            sink.kind.as_str(),
                            sink.name
                        ),
                    };
                }
                // The allowlist first, because a refusal here is the answer
                // rather than a network error two seconds later.
                let host = url_host(&sink.url);
                if !self.inner.config.integrations.allows_host(&host) {
                    return IpcResponse::SinkTest {
                        ok: false,
                        status: self.inner.config.integrations.refusal_for(&host),
                    };
                }
                // Resolved here, from the mesh vault, for a record that may
                // never be saved — which is the point: a wrong secret name
                // is exactly the mistake this button exists to catch.
                let credential = match self.sink_credential(&sink).await {
                    Ok(c) => c,
                    Err(e) => {
                        return IpcResponse::SinkTest {
                            ok: false,
                            status: format!("credential: {e}"),
                        };
                    }
                };
                // One synthetic row, marked as one. It lands in the
                // destination like any other and is trivially filtered out
                // — which is better than a request shaped differently from
                // the real ones and therefore able to succeed when they
                // would not.
                let row = serde_json::json!({
                    "mesh_id": self.inner.engine.genesis().body.mesh_id.to_display_string(),
                    "table": "jobs",
                    "job_id": "job-connection-test",
                    "command": "meshware sink connection test",
                    "runtime": "test",
                    "state": "succeeded",
                });
                match crate::sinks::http_write(&sink, &[row], &credential).await {
                    crate::sinks::WriteOutcome::Accepted
                    | crate::sinks::WriteOutcome::PartiallyAccepted { .. } => {
                        IpcResponse::SinkTest {
                            ok: true,
                            status: format!("accepted by {host}"),
                        }
                    }
                    crate::sinks::WriteOutcome::Failed { status } => {
                        IpcResponse::SinkTest { ok: false, status }
                    }
                }
            }
            IpcRequest::SinksResume => {
                gate!(rbac::ADMIN_ONLY, "sinks.resume", "", true);
                match crate::sinks::resume_after_restore(
                    &self.inner.config.layout().sinks_dir(),
                ) {
                    Ok(true) => {
                        self.audit_ok(&user, "sinks.resume", "", "after a restore", env);
                        IpcResponse::Ok
                    }
                    Ok(false) => IpcResponse::Error {
                        message: "this node's sinks are not held — nothing to resume".into(),
                    },
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- M31 A1 retention -----------------------------------------
            //
            // `policy.create`'s shape verbatim (M31 A1 item 2): admin-gated,
            // signed client-side by the admin's own key, audited by name.
            // `meshnode audit` then answers *who shortened our history, and
            // when* — which is the question this op exists to make
            // answerable.
            IpcRequest::RetentionPut { policy } => {
                gate!(rbac::ADMIN_ONLY, "retention.put", "", true);
                if policy.created_by != user {
                    return IpcResponse::Denied {
                        message: "a retention policy must be signed by the user submitting it"
                            .into(),
                    };
                }
                // The floor, checked before signing as well as at merge
                // time on every node. Refusing here is what lets the
                // refusal name the floor to the person who typed it.
                if let Some(why) = policy.refusal() {
                    // Audited as a refusal, not silently dropped: an
                    // attempt to shorten the audit log below its floor is
                    // exactly the event an auditor came for.
                    audit::record(
                        &self.inner.engine,
                        user,
                        "retention.put",
                        "",
                        "denied",
                        &why,
                        &env.signature,
                    );
                    return IpcResponse::Denied { message: why };
                }
                let summary = policy
                    .windows
                    .iter()
                    .map(|(t, w)| {
                        format!("{}={}", t.as_str(), mesh_proto::retention::describe_days(w.days))
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                match self.inner.engine.author_op(mesh_proto::OpBody::RetentionPut {
                    policy: policy.clone(),
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "retention.put", "", &summary, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::RetentionReset {
                at_micros,
                signature,
            } => {
                gate!(rbac::ADMIN_ONLY, "retention.reset", "", true);
                match self
                    .inner
                    .engine
                    .author_op(mesh_proto::OpBody::RetentionReset {
                        by: user,
                        at_micros,
                        signature,
                    }) {
                    Ok(_) => {
                        self.audit_ok(&user, "retention.reset", "", "back to defaults", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::RetentionShow => {
                gate!(rbac::READ, "retention.show", "", false);
                IpcResponse::Retention(Box::new(self.retention_view()))
            }
            // -- M31 P0 measurement ---------------------------------------
            IpcRequest::OpCensus => {
                gate!(rbac::READ, "state.census", "", false);
                // On a blocking thread (ENG-20). A census walks the whole
                // op-log and decodes every body; the hourly compaction tick
                // already ran it that way, and this path — `state census`,
                // `doctor` — did it inline on a reactor worker whenever the
                // five-minute cache had expired. Same work, and the answer
                // is cached either way; what differs is whose core pays.
                let this = self.clone();
                match tokio::task::spawn_blocking(move || this.op_census()).await {
                    Ok(Ok(census)) => IpcResponse::OpCensus(Box::new(census)),
                    Ok(Err(e)) => IpcResponse::Error {
                        message: format!("op-log census: {e}"),
                    },
                    Err(e) => IpcResponse::Error {
                        message: format!("op-log census task failed: {e}"),
                    },
                }
            }
            IpcRequest::StateCompact {
                horizon_secs,
                dry_run,
            } => {
                // Compaction drops bytes this machine is holding on behalf
                // of the mesh, so it is an admin act even though it changes
                // no mesh state — and it is audited, because "where did
                // last month's ops go" deserves an answer.
                gate!(rbac::ADMIN_ONLY, "state.compact", "", !dry_run);
                // Off the async workers (ENG-5): a pass reads this node's
                // whole log, and may rewrite the file while holding the
                // store alone.
                let this = self.clone();
                match tokio::task::spawn_blocking(move || this.state_compact(horizon_secs, dry_run))
                    .await
                {
                    Ok(Ok(stats)) => IpcResponse::StateStats(stats),
                    Ok(Err(e)) => IpcResponse::Error {
                        message: format!("state compact: {e}"),
                    },
                    Err(e) => IpcResponse::Error {
                        message: format!("state compact: {e}"),
                    },
                }
            }
            IpcRequest::UpgradePlans => {
                gate!(rbac::READ, "upgrade.plans", "", false);
                IpcResponse::UpgradePlans(self.upgrade_plan_rows())
            }
            IpcRequest::Releases => {
                gate!(rbac::READ, "releases", "", false);
                IpcResponse::Releases(self.release_rows())
            }
            // -- C2 deployments (C0-C9 §6) ------------------------------
            IpcRequest::DeploymentPut {
                deployment,
                policy,
                dry_run,
                inline_service,
            } => {
                gate!(rbac::DEPLOYMENTS, "deployment.put", &deployment.name, !dry_run);
                if deployment.created_by != user {
                    return IpcResponse::Denied {
                        message: "a deployment must be signed by the user submitting it".into(),
                    };
                }
                // The policy is a second signed record and gets the second
                // check: one submission must not be able to publish a
                // deployment under its own name and a policy under
                // somebody else's.
                if policy.set_by != user {
                    return IpcResponse::Denied {
                        message: "a deployment policy must be signed by the user submitting it"
                            .into(),
                    };
                }
                if policy.deployment != deployment.name {
                    return IpcResponse::Error {
                        message: format!(
                            "policy names deployment {:?}, the record names {:?}",
                            policy.deployment, deployment.name
                        ),
                    };
                }
                self.deployment_put(user, env, *deployment, *policy, dry_run, inline_service)
            }
            // -- C5 devices (§9.1) -------------------------------------
            IpcRequest::Devices { node } => {
                gate!(rbac::READ, "devices", node.as_deref().unwrap_or(""), false);
                IpcResponse::Devices(self.device_rows(node.as_deref()))
            }
            // -- N3 the service registry --------------------------------
            IpcRequest::Registry => {
                gate!(rbac::READ, "registry", "", false);
                IpcResponse::Registry(
                    self.inner
                        .engine
                        .read(|s| crate::registry::rows(s, |id| self.node_is_dead(id))),
                )
            }
            IpcRequest::RegistryShow { service } => {
                gate!(rbac::READ, "registry.show", &service, false);
                IpcResponse::RegistryShow(self.inner.engine.read(|s| {
                    crate::registry::instance_rows(s, &service, self.node_id(), |id| self.node_is_dead(id))
                }))
            }
            IpcRequest::DnsStatus => {
                gate!(rbac::READ, "dns", "", false);
                let net = &self.inner.config.network;
                let (names, known) = self.inner.engine.read(|s| {
                    let catalogue = crate::registry::catalogue(s);
                    (catalogue.len() as u32, catalogue.keys().next().cloned())
                });
                // **Phase A, §2.5d: the probe, not the config.** This view
                // used to say "answering on 127.0.0.1:8600" on the strength
                // of the address being configured — which is also what a
                // Director that lost the port to a `meshnode up` daemon on
                // the same machine looks like, while every `.mesh` answer
                // came from the other process's mesh. `doctor` learned to
                // probe in N7; the panel people actually look at now asks
                // the same question with the same function.
                let probed = match (net.dns, net.dns_bind) {
                    (true, Some(bind)) => {
                        let known = known.clone();
                        tokio::task::spawn_blocking(move || {
                            let found = crate::dns::probe(bind, known.as_deref());
                            crate::dns::probe_problem(&found, bind, known.as_deref())
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                    _ => None,
                };
                IpcResponse::DnsStatus(Box::new(mesh_proto::DnsStatusView {
                    enabled: net.dns,
                    resolver_routed: match net.dns_bind {
                        Some(bind) => tokio::task::spawn_blocking(move || {
                            bind.port() == 53 || crate::doctor::resolver_routes_mesh(bind)
                        })
                        .await
                        .unwrap_or(false),
                        None => false,
                    },
                    bound: net.dns_bind.map(|b| b.to_string()),
                    port: net
                        .dns_bind
                        .map(|b| b.port())
                        .unwrap_or(mesh_proto::DEFAULT_DNS_PORT),
                    privileged: net.dns_bind.is_some_and(|b| b.port() < 1024),
                    suffix: mesh_proto::DNS_SUFFIX.to_string(),
                    names,
                    detail: match (net.dns, net.dns_bind) {
                        (false, _) => "turned off in [network] dns".into(),
                        (true, None) => {
                            "[network] dns_bind is not a usable address".into()
                        }
                        // Empty only when the probe found this mesh's own
                        // responder there (or had nothing to check with).
                        (true, Some(_)) => probed.unwrap_or_default(),
                    },
                }))
            }
            // -- N5 volumes --------------------------------------------
            IpcRequest::Volumes { node } => {
                gate!(rbac::READ, "volumes", node.as_deref().unwrap_or(""), false);
                let me = self.inner.engine.read(|s| {
                    s.members
                        .get(&self.node_id())
                        .map(|m| m.info.name.clone())
                        .unwrap_or_else(|| self.node_id().short())
                });
                match node.as_deref() {
                    None => IpcResponse::Volumes(self.inner.jobs.volume_rows(&me)),
                    Some(name) if name == me => {
                        IpcResponse::Volumes(self.inner.jobs.volume_rows(&me))
                    }
                    Some(name) => IpcResponse::Error {
                        message: format!(
                            concat!(
                                "a volume ledger is node-local: this is {}, not {}. ",
                                "The claim IS that node's own lease table — asking anyone ",
                                "else would be asking something that cannot know, which is ",
                                "the split this design exists to avoid.",
                            ),
                            me, name
                        ),
                    },
                }
            }
            // -- N5 P2 dynamic volumes ---------------------------------
            //
            // **Node-local, like the ledger and for the same reason.** A
            // volume is a directory on one machine; a verb that provisioned
            // one on a machine other than the one you are talking to would
            // be a verb whose failure nobody is standing next to.
            IpcRequest::VolumeCreate {
                name,
                capacity_mb,
                params,
                read_only,
            } => {
                gate!(rbac::ADMIN_ONLY, "volume.create", &name, true);
                let name = name.trim().to_string();
                let Some(plugin) = self.inner.config.runtime.volume_plugin.clone() else {
                    return IpcResponse::Error {
                        message: concat!(
                            "this node has no volume plugin: set `[runtime] volume_plugin` ",
                            "in meshnode.toml. A node without one lends only the ",
                            "[[volume]] blocks it was given, which is the default.",
                        )
                        .to_string(),
                    };
                };
                // Refused before the plugin runs, not after: a plugin that
                // provisioned storage for a name already taken would have
                // made something nothing can reach.
                if self
                    .inner
                    .jobs
                    .volumes()
                    .iter()
                    .any(|v| v.name == name)
                {
                    return IpcResponse::Error {
                        message: format!(
                            "this node already lends {name:?} — `meshnode volumes` says \
                             where it came from"
                        ),
                    };
                }
                let config = self.inner.config.runtime.volume_plugin_config.clone();
                let params: std::collections::BTreeMap<String, String> =
                    params.into_iter().collect();
                let made = match crate::volumes::create(
                    &plugin,
                    config.as_deref(),
                    &name,
                    capacity_mb,
                    &params,
                    None,
                )
                .await
                {
                    Ok(made) => made,
                    Err(e) => {
                        return IpcResponse::Error {
                            message: e.to_string(),
                        };
                    }
                };
                let mut store = crate::volumes::Store::load(&self.inner.config.data_dir);
                store.volumes.retain(|v| v.name != name);
                store.volumes.push(crate::volumes::Created {
                    name: name.clone(),
                    path: made.path.clone(),
                    read_only,
                    plugin: plugin.clone(),
                    capacity_mb: made.capacity_mb,
                    detail: made.detail.clone(),
                    created_at_micros: mesh_proto::now_micros(),
                    params,
                });
                if let Err(e) = store.save(&self.inner.config.data_dir) {
                    // The directory exists and the note does not. Said
                    // plainly rather than swallowed: the volume is real,
                    // this node will not remember it across a restart, and
                    // the operator is the only one who can reconcile that.
                    return IpcResponse::Error {
                        message: format!(
                            "{name:?} was provisioned at {} but could not be recorded: {e} \
                             — it will not be offered after a restart",
                            made.path
                        ),
                    };
                }
                self.reoffer_volumes(&store);
                let me = self.my_display_name();
                let row = self
                    .inner
                    .jobs
                    .volume_rows(&me)
                    .into_iter()
                    .find(|r| r.name == name);
                match row {
                    Some(row) => IpcResponse::VolumeCreated(row),
                    None => {
                        // Rolled back, because a record that survives its
                        // own failure is how a node comes back after a
                        // restart offering something it already told the
                        // operator it could not offer.
                        store.volumes.retain(|v| v.name != name);
                        let _ = store.save(&self.inner.config.data_dir);
                        IpcResponse::Error {
                            message: format!(
                                "{name:?} was provisioned at {} and is not being offered — \
                                 the name or the path is one this node will not lend",
                                made.path
                            ),
                        }
                    }
                }
            }
            IpcRequest::VolumeRemove { name, force } => {
                gate!(rbac::ADMIN_ONLY, "volume.remove", &name, true);
                let name = name.trim().to_string();
                let mut store = crate::volumes::Store::load(&self.inner.config.data_dir);
                let Some(vol) = store.get(&name).cloned() else {
                    // Named precisely, because the likely mistake is
                    // reaching for this to delete a hand-written block.
                    return IpcResponse::Error {
                        message: format!(
                            "this node did not provision {name:?}. A [[volume]] block is \
                             your own line in your own meshnode.toml — remove it there and \
                             reload; nothing here edits that file."
                        ),
                    };
                };
                // Refused while held. The lease is the authority on whether
                // anything is using the bytes, and deleting storage out
                // from under a running job is the one failure a lease
                // exists to make impossible.
                let me = self.my_display_name();
                if let Some(row) = self
                    .inner
                    .jobs
                    .volume_rows(&me)
                    .into_iter()
                    .find(|r| r.name == name)
                    && !row.holders.is_empty()
                {
                    return IpcResponse::Error {
                        message: format!(
                            "{name:?} is held by {} — a lease is the authority on whether \
                             anything is using the bytes, and --force does not override it",
                            row.holders.join(", ")
                        ),
                    };
                }
                let plugin = self.inner.config.runtime.volume_plugin.clone();
                let config = self.inner.config.runtime.volume_plugin_config.clone();
                let mut note = String::new();
                if let Some(plugin) = plugin.as_deref() {
                    if let Err(e) = crate::volumes::delete(plugin, config.as_deref(), &vol).await {
                        if !force {
                            return IpcResponse::Error {
                                message: format!(
                                    "{e} — pass --force to forget it here anyway, which \
                                     leaves whatever the plugin made behind"
                                ),
                            };
                        }
                        note = format!(" (the plugin refused: {e})");
                    }
                } else if !force {
                    return IpcResponse::Error {
                        message: format!(
                            "{name:?} was provisioned by {:?}, which is no longer this \
                             node's `[runtime] volume_plugin` — pass --force to forget it \
                             here and remove the storage by hand",
                            vol.plugin
                        ),
                    };
                }
                store.volumes.retain(|v| v.name != name);
                if let Err(e) = store.save(&self.inner.config.data_dir) {
                    return IpcResponse::Error {
                        message: format!("{name:?} was removed but the record was not: {e}"),
                    };
                }
                self.reoffer_volumes(&store);
                IpcResponse::VolumeRemoved {
                    message: format!("{name:?} is no longer offered{note}"),
                }
            }
            // -- N2 P1 ports -------------------------------------------
            //
            // **This node only, and the request says so.** A dynamic port
            // is decided on the executor at spawn and does not replicate
            // until N3's registry exists, so there is no mesh-wide answer
            // to give — and inventing one by asking peers would be a
            // fan-out on a read that an operator expects to be instant.
            // Naming another node is an error with the reason rather than
            // an empty table.
            IpcRequest::Ports { node } => {
                gate!(rbac::READ, "ports", node.as_deref().unwrap_or(""), false);
                let me = self.inner.engine.read(|s| {
                    s.members
                        .get(&self.node_id())
                        .map(|m| m.info.name.clone())
                        .unwrap_or_else(|| self.node_id().short())
                });
                match node.as_deref() {
                    None => IpcResponse::Ports(Box::new(self.inner.jobs.port_ledger(me))),
                    Some(name) if name == me => {
                        IpcResponse::Ports(Box::new(self.inner.jobs.port_ledger(me)))
                    }
                    Some(name) => IpcResponse::Error {
                        message: format!(
                            "a port ledger is node-local: this is {me}, not {name}. Ask \
                             {name}'s own daemon — allocations do not replicate \
                             until the service registry lands (N3)."
                        ),
                    },
                }
            }
            // -- T4 runtimes (§5) --------------------------------------
            IpcRequest::Runtimes { node } => {
                gate!(rbac::READ, "runtimes", node.as_deref().unwrap_or(""), false);
                self.runtimes_view(node.as_deref())
            }
            // -- C4 gangs (§8.3) ---------------------------------------
            IpcRequest::Gangs { all } => {
                gate!(rbac::READ, "gangs", "", false);
                IpcResponse::Gangs(self.gang_rows(all))
            }
            IpcRequest::GangShow { gang } => {
                gate!(rbac::READ, "gang.show", &gang.to_display_string(), false);
                match self.gang_view(gang) {
                    Some(view) => IpcResponse::GangShow(Box::new(view)),
                    None => IpcResponse::Error {
                        message: format!("no such gang: {}", gang.short()),
                    },
                }
            }
            // -- C7 the action cache (§11.3 P3) ------------------------
            IpcRequest::CacheStats => {
                gate!(rbac::READ, "cache.stats", "", false);
                IpcResponse::CacheStats(Box::new(self.inner.actions.stats()))
            }
            IpcRequest::CachePurge { scope } => {
                // Audited, and open to everyone who can run work rather
                // than to admins alone: a purge is entirely recoverable —
                // it costs re-execution and nothing else — and the person
                // who needs it is the developer who just watched a stale
                // answer come back, at the moment they noticed. Making them
                // find an admin first is how a cache stops being trusted.
                //
                // Audited because the effect is invisible (things get
                // slower) and the cause is highly visible (somebody
                // suspected the cache), so the log has to carry which and
                // who.
                let target = match &scope {
                    mesh_proto::PurgeScope::All => "all".to_string(),
                    mesh_proto::PurgeScope::Key(k) => k.to_display_string(),
                    mesh_proto::PurgeScope::Service(name) => name.clone(),
                };
                gate!(rbac::RUNNERS, "cache.purge", &target, true);
                let (gone, released) = self.inner.actions.purge(&scope);
                // The bytes go back to the ordinary refcount rules on the
                // next sweep. Not deleted here: the job that produced them
                // may still be running, and a purge is a statement about
                // the index, not about somebody's live intermediate.
                self.inner.objects.unpin(&released);
                self.audit_ok(
                    &user,
                    "cache.purge",
                    &target,
                    &format!("{gone} entries"),
                    env,
                );
                IpcResponse::Notice {
                    message: format!("purged {gone} cache entries ({target})"),
                }
            }
            // -- C8 legibility (§12.3 P1, P3) --------------------------
            IpcRequest::JobExplain { job_id } => {
                gate!(rbac::READ, "job.explain", &job_id.to_display_string(), false);
                match self.job_explain(&job_id) {
                    Some(view) => IpcResponse::JobExplain(Box::new(view)),
                    None => IpcResponse::Error {
                        message: format!("no such job: {}", job_id.short()),
                    },
                }
            }
            IpcRequest::Plan {
                on,
                service,
                env: job_env,
                fanout,
            } => {
                gate!(rbac::READ, "plan", on.as_deref().unwrap_or("any"), false);
                IpcResponse::Plan(Box::new(
                    self.plan_estimate(user, on, service, job_env, fanout).await,
                ))
            }
            IpcRequest::Deployments => {
                gate!(rbac::READ, "deployments", "", false);
                IpcResponse::Deployments(self.deployment_rows(None, false))
            }
            // Phase C, U8.
            IpcRequest::PlacementMap => {
                gate!(rbac::READ, "deployments.placement", "", false);
                let me = self.node_id();
                IpcResponse::PlacementMap(self.inner.engine.read(|s| {
                    crate::deployments::placement_map(s, |id| {
                        if *id == me {
                            return "alive".to_string();
                        }
                        match self.inner.gossip.liveness_of(id) {
                            Liveness::Alive => "alive",
                            Liveness::Suspect => "suspect",
                            Liveness::Dead => "dead",
                        }
                        .to_string()
                    })
                }))
            }
            IpcRequest::DeploymentShow { name } => {
                gate!(rbac::READ, "deployment.show", &name, false);
                match self.deployment_rows(Some(&name), true).pop() {
                    Some(row) => IpcResponse::DeploymentShow(Box::new(row)),
                    None => IpcResponse::Error {
                        message: format!("no deployment named {name}"),
                    },
                }
            }
            IpcRequest::DeploymentStatus { name } => {
                gate!(rbac::READ, "deployment.status", &name, false);
                match self.deployment_status_view(&name) {
                    Some(view) => IpcResponse::DeploymentStatus(Box::new(view)),
                    None => IpcResponse::Error {
                        message: format!("no deployment named {name}"),
                    },
                }
            }
            // -- Phase B, E3/U1: one instance up close, and live use --------
            IpcRequest::DeploymentInstance { name, slot } => {
                gate!(rbac::READ, "deployment.instance", &name, false);
                let detail = self.inner.engine.read(|s| {
                    let entry = s.deployments.get(&name).filter(|e| !e.removed)?;
                    crate::deployments::instance_detail(&entry.deployment, s, &slot, |id| {
                        self.inner.gossip.not_alive_since_micros(id).is_some()
                    })
                });
                match detail {
                    None => IpcResponse::Error {
                        message: format!(
                            "no instance {slot:?} in deployment {name} — a replica number, or a node name for a system deployment"
                        ),
                    },
                    Some(mut detail) => {
                        let running = detail
                            .current
                            .as_ref()
                            .filter(|row| row.state == "running")
                            .map(|row| row.job_id);
                        if let Some(job) = running {
                            let executor = self
                                .inner
                                .engine
                                .read(|s| s.jobs.get(&job).map(|j| j.executor));
                            if let Some(node) = executor {
                                detail.stats =
                                    self.instance_stats_for(vec![(job, node)]).await.remove(&job);
                            }
                        }
                        IpcResponse::DeploymentInstance(Box::new(detail))
                    }
                }
            }
            IpcRequest::DeploymentInstanceStats { name } => {
                gate!(rbac::READ, "deployment.status", &name, false);
                match self.deployment_status_view(&name) {
                    None => IpcResponse::Error {
                        message: format!("no deployment named {name}"),
                    },
                    Some(view) => {
                        let live: Vec<(JobId, String, String)> = view
                            .instances
                            .iter()
                            .filter(|i| i.generation == view.generation && i.state == "running")
                            .map(|i| (i.job_id, i.slot.clone(), i.node.clone()))
                            .collect();
                        let targets: Vec<(JobId, mesh_proto::NodeId)> = self.inner.engine.read(|s| {
                            live.iter()
                                .filter_map(|(job, _, _)| s.jobs.get(job).map(|j| (*job, j.executor)))
                                .collect()
                        });
                        let mut stats = self.instance_stats_for(targets).await;
                        IpcResponse::DeploymentInstanceStats(
                            live.into_iter()
                                .map(|(job, slot, node)| mesh_proto::InstanceStatRow {
                                    slot,
                                    node,
                                    stat: stats.remove(&job),
                                })
                                .collect(),
                        )
                    }
                }
            }
            // -- Phase B, E1: history and tags ---------------------------
            IpcRequest::DeploymentHistory { name } => {
                gate!(rbac::READ, "deployment.history", &name, false);
                let founder = self.inner.engine.genesis().body.founder_user;
                match self
                    .inner
                    .engine
                    .read(|s| crate::deployments::history(s, &founder, &name))
                {
                    Some(view) => IpcResponse::DeploymentHistory(Box::new(view)),
                    None => IpcResponse::Error {
                        message: format!("no deployment named {name}"),
                    },
                }
            }
            // -- Phase B, E2: knobs and promotion -------------------------
            IpcRequest::DeploymentKnobs { knobs } => {
                gate!(rbac::DEPLOYMENTS, "deployment.knobs", &knobs.deployment, true);
                if knobs.set_by != user {
                    return IpcResponse::Denied {
                        message: "deployment settings must be signed by the user submitting them"
                            .into(),
                    };
                }
                let name = knobs.deployment.clone();
                let detail = if knobs.auto_promote() {
                    "every default"
                } else {
                    "auto_promote = false"
                };
                match crate::deployments::put_knobs(&self.inner.engine, *knobs) {
                    Ok(true) => {
                        self.audit_ok(&user, "deployment.knobs", &name, detail, env);
                        IpcResponse::Ok
                    }
                    Ok(false) => IpcResponse::Ok,
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::DeploymentPromote { promotion } => {
                gate!(rbac::DEPLOYMENTS, "deployment.promote", &promotion.deployment, true);
                if promotion.by != user {
                    return IpcResponse::Denied {
                        message: "a promotion must be signed by the user submitting it".into(),
                    };
                }
                let (name, generation) = (promotion.deployment.clone(), promotion.generation);
                match crate::deployments::promote(&self.inner.engine, *promotion) {
                    Ok(()) => {
                        self.audit_ok(
                            &user,
                            "deployment.promote",
                            &name,
                            &format!("generation {generation}"),
                            env,
                        );
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::DeploymentTag { tag } => {
                gate!(rbac::DEPLOYMENTS, "deployment.tag", &tag.deployment, true);
                if tag.set_by != user {
                    return IpcResponse::Denied {
                        message: "a deployment tag must be signed by the user submitting it".into(),
                    };
                }
                let (name, label) = (tag.deployment.clone(), tag.tag.clone());
                let action = match &tag.version {
                    Some(v) => ("deployment.tag", format!("{label} → generation {}", v.deployment.generation)),
                    None => ("deployment.untag", label),
                };
                match crate::deployments::put_tag(&self.inner.engine, *tag) {
                    Ok(()) => {
                        self.audit_ok(&user, action.0, &name, &action.1, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::DeploymentRemove {
                name,
                by,
                signature,
            } => {
                gate!(rbac::DEPLOYMENTS, "deployment.remove", &name, true);
                if by != user {
                    return IpcResponse::Denied {
                        message: "a deployment removal must be signed by the user submitting it"
                            .into(),
                    };
                }
                match crate::deployments::remove(&self.inner.engine, &name, by, signature) {
                    Ok(()) => {
                        self.audit_ok(&user, "deployment.remove", &name, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // -- N1 P5 schedules -----------------------------------------
            IpcRequest::SchedulePut { schedule, dry_run } => {
                gate!(rbac::SCHEDULES, "schedule.put", &schedule.name, !dry_run);
                if schedule.created_by != user {
                    return IpcResponse::Denied {
                        message: "a schedule must be signed by the user submitting it".into(),
                    };
                }
                self.schedule_put(user, env, *schedule, dry_run)
            }
            IpcRequest::Schedules => {
                gate!(rbac::READ, "schedules", "", false);
                IpcResponse::Schedules(self.schedule_rows(None, false))
            }
            IpcRequest::ScheduleShow { name } => {
                gate!(rbac::READ, "schedule.show", &name, false);
                match self.schedule_rows(Some(&name), true).into_iter().next() {
                    Some(row) => IpcResponse::ScheduleShow(Box::new(row)),
                    None => IpcResponse::Error {
                        message: format!("no schedule named {name}"),
                    },
                }
            }
            IpcRequest::ScheduleRemove {
                name,
                by,
                signature,
            } => {
                gate!(rbac::SCHEDULES, "schedule.remove", &name, true);
                if by != user {
                    return IpcResponse::Denied {
                        message: "a schedule removal must be signed by the user submitting it"
                            .into(),
                    };
                }
                match crate::schedules::remove(&self.inner.engine, &name, by, signature) {
                    Ok(()) => {
                        self.audit_ok(&user, "schedule.remove", &name, "retired", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            // Never a real request: it exists only as the payload of the
            // envelope inside a scheduled job. Refusing it here keeps that
            // true, exactly as `DeploymentFire` is refused above.
            IpcRequest::ScheduleFire { .. } => IpcResponse::Denied {
                message: "a schedule fire is not something a client submits; it is the \
                          authorization a schedule's own jobs carry"
                    .into(),
            },
            // Never a real request: it exists only as the payload of the
            // envelope inside an instance job (§6.2), exactly like
            // `ScheduleFire`. Refusing it here keeps that true.
            IpcRequest::DeploymentFire { .. } => IpcResponse::Denied {
                message: "a deployment fire is not something a client submits; it is the authorization a deployment's own instances carry"
                    .into(),
            },
            IpcRequest::ReleasePublish { release } => {
                let target = release.version.clone();
                gate!(rbac::ADMIN_ONLY, "release.publish", &target, true);
                if release.published_by != user {
                    return IpcResponse::Error {
                        message: "the release is signed by a different user".into(),
                    };
                }
                // The bytes must already be here: publishing a record that
                // names a bundle nobody holds would put a version in the
                // catalogue that no node can ever fetch.
                if !self.inner.content.has_bundle(&release.bundle_id) {
                    return IpcResponse::Error {
                        message: format!(
                            "this node does not hold bundle {} — push the binary here first",
                            release.bundle_id.short()
                        ),
                    };
                }
                match self
                    .inner
                    .engine
                    .author_op(OpBody::ReleasePublish { release })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "release.publish", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ReleaseKeys => {
                gate!(rbac::READ, "release.keys", "", false);
                IpcResponse::ReleaseKeys(self.release_key_view())
            }
            IpcRequest::ReleaseKeyAdd { key } => {
                let target = key.label.clone();
                gate!(rbac::ADMIN_ONLY, "release.key.add", &target, true);
                if key.added_by != user {
                    return IpcResponse::Error {
                        message: "the release key record is signed by a different user".into(),
                    };
                }
                // Refused here as well as at merge, because *here* is where
                // a person is standing: a pasted comment line or a
                // truncated key would otherwise replicate to the whole
                // fleet and verify nothing, silently.
                if !mesh_proto::ReleaseKey::looks_like_a_key(&key.pubkey) {
                    return IpcResponse::Error {
                        message: "that is not a minisign public key — paste the `RW…` line from \
                                  the .pub file, not the comment above it"
                            .into(),
                    };
                }
                let pubkey = key.pubkey.clone();
                match self.inner.engine.author_op(OpBody::ReleaseKeyPut { key }) {
                    Ok(_) => {
                        self.audit_ok(&user, "release.key.add", &target, &pubkey, env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::ReleaseKeyRevoke {
                pubkey,
                by,
                at_micros,
                signature,
            } => {
                gate!(rbac::ADMIN_ONLY, "release.key.revoke", &pubkey, true);
                if by != user {
                    return IpcResponse::Error {
                        message: "the revocation is signed by a different user".into(),
                    };
                }
                match self.inner.engine.author_op(OpBody::ReleaseKeyRevoke {
                    pubkey: pubkey.clone(),
                    by,
                    at_micros,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "release.key.revoke", &pubkey, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::UpgradeCancel {
                plan_id,
                by,
                at_micros,
                signature,
            } => {
                let target = plan_id.to_display_string();
                gate!(rbac::ADMIN_ONLY, "node.upgrade.cancel", &target, true);
                if by != user {
                    return IpcResponse::Error {
                        message: "the cancellation is signed by a different user".into(),
                    };
                }
                match self.inner.engine.author_op(OpBody::UpgradePlanCancel {
                    plan_id,
                    by,
                    at_micros,
                    signature,
                }) {
                    Ok(_) => {
                        self.audit_ok(&user, "node.upgrade.cancel", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Enrolments => {
                gate!(rbac::ADMIN_ONLY, "enrol.list", "", false);
                IpcResponse::Enrolments(self.enrolment_rows())
            }
            IpcRequest::EnrolDecline { decline } => {
                let target = decline.user_id.to_display_string();
                gate!(rbac::ADMIN_ONLY, "enrol.decline", &target, true);
                if decline.declined_by != user {
                    return IpcResponse::Error {
                        message: "the decline is signed by a different user".into(),
                    };
                }
                match self
                    .inner
                    .engine
                    .author_op(OpBody::EnrolDecline { decline })
                {
                    Ok(_) => {
                        self.audit_ok(&user, "enrol.decline", &target, "", env);
                        IpcResponse::Ok
                    }
                    Err(e) => IpcResponse::Error {
                        message: e.to_string(),
                    },
                }
            }
            IpcRequest::Sessions => {
                gate!(rbac::READ, "sessions", "", false);
                IpcResponse::Sessions(self.inner.sessions.rows())
            }
            IpcRequest::SessionClose { id } => {
                // Closing a session is closing somebody's shell, so it is a
                // runner's act and it is audited — the same bar as opening
                // one, from the other direction.
                gate!(rbac::RUNNERS, "session.close", &id.to_string(), true);
                if self.inner.sessions.close(id, "closed by an operator") {
                    self.audit_ok(&user, "session.close", &id.to_string(), "", env);
                    IpcResponse::Ok
                } else {
                    IpcResponse::Error {
                        message: format!("no live session {id} on this node"),
                    }
                }
            }
            // Never reached through this path: the IPC loop recognises a
            // session request *before* dispatching it and hands over the
            // whole socket instead (R4). A client that sends it as an
            // ordinary request gets told what it did rather than hanging.
            IpcRequest::SessionOpen { .. } => IpcResponse::Error {
                message: "a session takes over the connection — this client sent it as an \
                          ordinary request"
                    .into(),
            },
            IpcRequest::HistoryStatus => {
                gate!(rbac::READ, "jobs", "", false);
                let path = self.inner.config.layout().history();
                match &self.inner.history {
                    Some(h) => match h.stats(&path) {
                        Ok(stats) => IpcResponse::HistoryStatus(Box::new(stats)),
                        Err(e) => IpcResponse::Error {
                            message: format!("history stats failed: {e}"),
                        },
                    },
                    None => IpcResponse::Error {
                        message: "the history index is disabled on this node \
                                  ([history] enabled = false)"
                            .to_string(),
                    },
                }
            }
            // Quotas and the usage report were removed (2026-09-18), and are
            // answered the way the change-control verbs below are.
            IpcRequest::QuotaPut
            | IpcRequest::QuotaRemove
            | IpcRequest::Quotas
            | IpcRequest::Usage
            | IpcRequest::UsageRows => {
                gate!(rbac::READ, "retired", "", false);
                IpcResponse::Error {
                    message: QUOTAS_RETIRED.into(),
                }
            }
            // The host inventory was removed (2026-09-18); treated exactly
            // as the change-control verbs below are.
            IpcRequest::HostPut
            | IpcRequest::HostImport
            | IpcRequest::Hosts
            | IpcRequest::HostShow
            | IpcRequest::HostRemove
            | IpcRequest::HostGroupPut
            | IpcRequest::HostGroups
            | IpcRequest::HostGroupRemove
            | IpcRequest::HostCheck
            | IpcRequest::HostInventory => {
                gate!(rbac::READ, "retired", "", false);
                IpcResponse::Error {
                    message: INVENTORY_RETIRED.into(),
                }
            }
            // Change control was removed (2026-09-18). The variants stay so
            // nothing after them renumbers; an older client that asks is
            // told why rather than handed a decode error.
            IpcRequest::PolicyCreate
            | IpcRequest::Policies
            | IpcRequest::PolicyShow
            | IpcRequest::PolicyRemove
            | IpcRequest::PolicyCheck
            | IpcRequest::Approvals
            | IpcRequest::ApprovalShow
            | IpcRequest::ApprovalDecide
            | IpcRequest::ApprovalCancel
            | IpcRequest::PolicyPresencePut
            | IpcRequest::PolicyPresences
            | IpcRequest::PresenceReadiness
            | IpcRequest::UserTokenAdd
            | IpcRequest::UserTokens
            | IpcRequest::UserTokenRevoke
            | IpcRequest::ApprovalDecidePresent => {
                // A member is told the feature is gone; a stranger learns
                // nothing, exactly as for every live request.
                gate!(rbac::READ, "retired", "", false);
                IpcResponse::Error {
                    message: CHANGE_CONTROL_RETIRED.into(),
                }
            }
            // The run index was removed (2026-09-20), same shape.
            IpcRequest::HistoryRuns => {
                gate!(rbac::READ, "retired", "", false);
                IpcResponse::Error {
                    message: RUN_INDEX_RETIRED.into(),
                }
            }
            // The mesh benchmark was removed (2026-09-20), same shape.
            IpcRequest::BenchRun => {
                gate!(rbac::READ, "retired", "", false);
                IpcResponse::Error {
                    message: BENCH_RETIRED.into(),
                }
            }
            // Streaming requests are handled by handle_signed_stream.
            IpcRequest::Run { .. }
            | IpcRequest::JobLogs { .. }
            | IpcRequest::RunService { .. }
            | IpcRequest::RunScript { .. }
            | IpcRequest::PairApprove { .. }
            | IpcRequest::Parallel { .. }
            | IpcRequest::ObjectGet { .. }
            | IpcRequest::DryRun { .. }
            | IpcRequest::ExplainKey { .. }
            | IpcRequest::BreakGlass { .. }
            | IpcRequest::Drain { .. }
            | IpcRequest::DrainDeadline { .. }
            | IpcRequest::BundlePrefetch { .. }
            | IpcRequest::BundleFileFetch { .. }
            | IpcRequest::UpgradeFleet { .. }
            | IpcRequest::MeshReset { .. } => IpcResponse::Error {
                message: "streaming request routed incorrectly".into(),
            },
        }
    }
}
