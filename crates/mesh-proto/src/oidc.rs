//! Identity-provider bridge (E1-P5): letting a mesh recognise the identity
//! provider a company already runs, without acquiring a login.
//!
//! ## What this does not change
//!
//! **A person is still a key on their own machine.** meshware has no
//! password, no session, no bearer token and no login screen, and none of
//! that is added here. Enrolment (M23) already had the right shape — a
//! self-signed request sits in a queue and a human with authority approves
//! it — and this fills in the one thing that shape was missing: the admin
//! looking at the queue had no way to know that `mia` was really Mia.
//!
//! So the bridge does exactly one thing: it lets a node **verify an ID
//! token from a provider an admin has named**, and attach what it proves to
//! the enrolment request. The approval is the same admin-signed `UserAdd` +
//! `GrantAdd` pair it always was.
//!
//! That is the reason an IdP outage cannot stop the mesh. Nothing on the
//! request path consults the provider: existing keys keep working, jobs
//! keep running, and the only thing that stops is *new people enrolling
//! through SSO* — who can still enrol the way they always could.
//!
//! ## The binding problem, and how the nonce solves it
//!
//! An ID token says "the provider authenticated this human". It says
//! nothing about an Ed25519 key, and a client that simply asserted "this
//! token is mine" would be trivially forged by a modified client.
//!
//! The standard answer is the OIDC `nonce`. `meshnode login` puts
//! [`nonce_for_key`] — a hash of the meshware public key — into the
//! authorization request; the provider echoes it verbatim into the ID
//! token's `nonce` claim; the verifying node recomputes it from the key in
//! the enrolment request and requires a match. A token minted for a
//! different key does not verify against this one, and a stolen token
//! cannot be replayed onto an attacker's key.
//!
//! ## Why the assertion is a separate op
//!
//! [`crate::EnrolmentRequest`] is signed by the requesting key, so a field
//! added to it would invalidate every request already in flight — and, more
//! importantly, the requester is exactly the party who must **not** be the
//! one asserting their own IdP identity. [`OidcAssertion`] is authored and
//! signed by the *node that did the verification*, which is the honest
//! shape: it is that node's claim about what it checked, and every other
//! node can see whose claim it is.

use serde::{Deserialize, Serialize};

use crate::{NodeId, Role, UserId};

/// Signing algorithms a node will accept on an ID token.
///
/// **An explicit allowlist, matched against the JWK's own key type.** The
/// two classic JWT vulnerabilities are `alg: none` and algorithm confusion
/// (handing an RSA public key to an HMAC verifier so the public key becomes
/// the shared secret). Neither is reachable through an enum that has no
/// `none` and no symmetric variant, and whose selection is cross-checked
/// against the key material rather than taken from the token header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JwtAlg {
    /// RSASSA-PKCS1-v1_5 with SHA-256. What Entra ID, Okta, Keycloak,
    /// Google Workspace and Auth0 all sign with by default.
    Rs256,
    /// ECDSA P-256 with SHA-256.
    Es256,
}

impl JwtAlg {
    pub fn as_str(&self) -> &'static str {
        match self {
            JwtAlg::Rs256 => "RS256",
            JwtAlg::Es256 => "ES256",
        }
    }

    /// Parse the `alg` header. Anything not on the list — `none`, `HS256`,
    /// an unknown string — is `None` and the token is refused.
    pub fn parse(s: &str) -> Option<JwtAlg> {
        match s {
            "RS256" => Some(JwtAlg::Rs256),
            "ES256" => Some(JwtAlg::Es256),
            _ => None,
        }
    }

    /// The JWK `kty` this algorithm's keys must have. Checked so a token
    /// header cannot select a verifier the key material does not match.
    pub fn required_kty(&self) -> &'static str {
        match self {
            JwtAlg::Rs256 => "RSA",
            JwtAlg::Es256 => "EC",
        }
    }
}

/// How long a verified assertion stays good for, in seconds.
///
/// Short on purpose: an assertion exists to inform an approval decision
/// that a human is about to make, not to be a credential. If nobody acts on
/// the enrolment while it is fresh, the person logs in again.
pub const ASSERTION_TTL_SECS: u64 = 3600;

/// Largest JWKS document a node will read. A key set is a few kilobytes;
/// this exists so a hostile or broken endpoint cannot make a node allocate.
pub const MAX_JWKS_BYTES: usize = 256 * 1024;

/// Clock skew tolerated on `exp` / `nbf` / `iat`, in seconds.
pub const CLOCK_SKEW_SECS: u64 = 120;

/// One `group → role` rule.
///
/// Matched against whatever claim [`OidcProvider::groups_claim`] names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRole {
    /// The group value as the provider spells it. A glob: `platform-*`.
    pub group: String,
    pub role: Role,
}

/// An identity provider an admin has told this mesh to trust (E1-P5).
///
/// Admin-signed and replicated like every other governance artifact, so
/// every node verifies for itself which issuer it is willing to believe —
/// there is no provider server to be down and no node that is authoritative
/// about trust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OidcProvider {
    /// Short name an operator types: `entra`, `okta`.
    pub name: String,
    /// The `iss` claim that must appear in the token, verbatim. Compared
    /// exactly, never by prefix or suffix.
    pub issuer: String,
    /// The `aud` claim that must appear. This mesh's registered client id.
    pub client_id: String,
    /// Where the signing keys live. Taken from the provider's discovery
    /// document once, by a person, and pinned here — so a node never has to
    /// trust a redirect chain to find out which keys are authoritative.
    pub jwks_uri: String,
    /// The device-authorization endpoint (RFC 8628). **Device-code flow,
    /// and no other:** the authorization-code flow wants a redirect URI,
    /// which means an HTTP listener on the machine a person is sitting at,
    /// and meshware does not run one. A device code is printed, approved in
    /// a browser anywhere, and polled for outbound.
    pub device_authorization_endpoint: String,
    /// Where the device code is exchanged for tokens.
    pub token_endpoint: String,
    /// Scopes to request. `openid` is mandatory and is added if absent.
    pub scopes: Vec<String>,
    /// Which claim carries group membership. Entra uses `groups`, Okta and
    /// Keycloak are commonly configured for `groups`, Google for `hd` plus
    /// a directory lookup this does not do.
    pub groups_claim: String,
    /// Suggested role per group, shown to the approving admin.
    ///
    /// **A suggestion, and deliberately not an authorization.** Applying it
    /// would mean a node minting a `UserAdd`, which requires an admin
    /// signature a node does not have — see the module docs of
    /// `mesh_core::oidc` for why automatic provisioning is a separate
    /// decision rather than an oversight.
    pub group_roles: Vec<GroupRole>,
    pub configured_by: UserId,
    pub configured_at_micros: u64,
    /// Ed25519 signature by `configured_by`, re-verified at merge on every
    /// node.
    pub signature: Vec<u8>,
}

impl OidcProvider {
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut unsigned = self.clone();
        unsigned.signature = Vec::new();
        let mut buf = Vec::with_capacity(512);
        buf.extend_from_slice(b"mw-oidc-provider-v1");
        buf.extend_from_slice(&postcard::to_stdvec(&unsigned).unwrap_or_default());
        buf
    }

    /// The role this provider suggests for a set of group values, if any.
    /// The **narrowest** match wins where several apply, because a mapping
    /// that silently widens somebody's access is the wrong way to be wrong.
    pub fn suggested_role(&self, groups: &[String]) -> Option<Role> {
        self.group_roles
            .iter()
            .filter(|gr| groups.iter().any(|g| crate::rbac::glob_match(&gr.group, g)))
            .map(|gr| gr.role)
            // `Role` derives Ord with Admin first, so `max` is the least
            // privileged of the matches.
            .max()
    }
}

/// What a node checked, and proved, about an enrolling key (E1-P5).
///
/// Authored and signed by the **verifying node** as an ordinary op, not by
/// the person enrolling: the requester is precisely the party who must not
/// be trusted to assert their own IdP identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OidcAssertion {
    /// The meshware key this is about, bound to the token by the `nonce`.
    pub user_id: UserId,
    /// The node that fetched the keys and checked the signature.
    pub verified_by: NodeId,
    /// Which configured provider ([`OidcProvider::name`]).
    pub provider: String,
    /// The token's `iss`, carried so a reader is not trusting the name.
    pub issuer: String,
    /// The token's `sub` — the provider's stable identifier for the human.
    /// The thing to write down when somebody asks *"who is this really"*.
    pub subject: String,
    /// `email`, when the token carried one. Convenience for the admin, and
    /// never an identifier: emails are reassigned, `sub` is not.
    pub email: String,
    /// `preferred_username` or `name`, when present.
    pub display_name: String,
    /// Group values read from [`OidcProvider::groups_claim`].
    pub groups: Vec<String>,
    /// What [`OidcProvider::suggested_role`] made of those groups. Shown to
    /// the admin as a default; never applied on its own.
    pub suggested_role: Option<Role>,
    pub verified_at_micros: u64,
    /// When this stops being shown as fresh evidence.
    pub expires_at_micros: u64,
}

impl OidcAssertion {
    pub fn is_fresh(&self, at_micros: u64) -> bool {
        at_micros <= self.expires_at_micros
    }
}

/// The `nonce` a client must send and a node must find in the token.
///
/// BLAKE3 of a domain tag and the raw public key, hex-encoded. Domain-tagged
/// so this value can never collide with a hash meshware computes for
/// anything else, and hex rather than base64 because it travels through URL
/// query strings, provider consoles and logs.
pub fn nonce_for_key(user_id: &UserId) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"mw-oidc-nonce-v1");
    hasher.update(&user_id.0);
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_algorithm_allowlist_refuses_none_and_hmac() {
        assert_eq!(JwtAlg::parse("RS256"), Some(JwtAlg::Rs256));
        assert_eq!(JwtAlg::parse("ES256"), Some(JwtAlg::Es256));
        // The two that matter.
        assert_eq!(JwtAlg::parse("none"), None);
        assert_eq!(JwtAlg::parse("None"), None);
        assert_eq!(JwtAlg::parse("HS256"), None);
        assert_eq!(JwtAlg::parse("hs256"), None);
        assert_eq!(JwtAlg::parse(""), None);
    }

    #[test]
    fn an_algorithm_names_the_key_type_it_needs() {
        assert_eq!(JwtAlg::Rs256.required_kty(), "RSA");
        assert_eq!(JwtAlg::Es256.required_kty(), "EC");
    }

    #[test]
    fn the_nonce_binds_one_key_and_is_stable() {
        let a = UserId([1u8; 32]);
        let b = UserId([2u8; 32]);
        assert_eq!(nonce_for_key(&a), nonce_for_key(&a));
        assert_ne!(nonce_for_key(&a), nonce_for_key(&b));
        assert_eq!(nonce_for_key(&a).len(), 64);
    }

    fn provider_with(roles: Vec<GroupRole>) -> OidcProvider {
        OidcProvider {
            name: "entra".into(),
            issuer: "https://login.example.com/v2.0".into(),
            client_id: "meshware".into(),
            jwks_uri: "https://login.example.com/keys".into(),
            device_authorization_endpoint: "https://login.example.com/devicecode".into(),
            token_endpoint: "https://login.example.com/token".into(),
            scopes: vec!["openid".into()],
            groups_claim: "groups".into(),
            group_roles: roles,
            configured_by: UserId([9u8; 32]),
            configured_at_micros: 0,
            signature: Vec::new(),
        }
    }

    #[test]
    fn the_narrowest_matching_role_wins() {
        let p = provider_with(vec![
            GroupRole {
                group: "platform".into(),
                role: Role::Admin,
            },
            GroupRole {
                group: "eng-*".into(),
                role: Role::Viewer,
            },
        ]);
        // Both match; the least privileged is chosen, because a mapping
        // that silently widens access is the wrong way to be wrong.
        assert_eq!(
            p.suggested_role(&["platform".into(), "eng-web".into()]),
            Some(Role::Viewer)
        );
        assert_eq!(p.suggested_role(&["platform".into()]), Some(Role::Admin));
        assert_eq!(p.suggested_role(&["nobody".into()]), None);
        assert_eq!(p.suggested_role(&[]), None);
    }

    #[test]
    fn a_signature_covers_every_field_that_matters() {
        let a = provider_with(vec![]);
        let mut b = a.clone();
        b.issuer = "https://evil.example.com".into();
        assert_ne!(a.signing_bytes(), b.signing_bytes());
        let mut c = a.clone();
        c.jwks_uri = "https://evil.example.com/keys".into();
        assert_ne!(a.signing_bytes(), c.signing_bytes());
        let mut d = a.clone();
        d.client_id = "someone-else".into();
        assert_ne!(a.signing_bytes(), d.signing_bytes());
    }
}
