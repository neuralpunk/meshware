//! ID-token verification and the identity-provider bridge (E1-P5).
//!
//! ## What is here, and what deliberately is not
//!
//! This module verifies an OpenID Connect **ID token** against a provider an
//! admin signed into replicated state, and turns the result into an
//! [`mesh_proto::OidcAssertion`] — a node's signed claim about what it
//! checked. That is the whole of the bridge. There is no session, no
//! cookie, no bearer token and no login screen anywhere in meshware, and
//! nothing here adds one: a person is still a key on their own machine.
//!
//! **Automatic provisioning is not here, and its absence is a decision.**
//! Turning a verified login into a mesh user means authoring a `UserAdd`,
//! and `UserAdd` is only accepted when it carries an *admin's* signature,
//! re-verified at merge on every node (MASTERPLAN §7.1). A node has no
//! admin key and must not: the entire reason a compromised node cannot
//! escalate is that it cannot manufacture governance. So an SSO login
//! produces **evidence, not authority** — the admin approving the
//! enrolment queue now sees a cryptographically verified subject, email and
//! group list with a suggested role, instead of a 52-character key somebody
//! pasted into Slack.
//!
//! Closing that last gap needs a delegation op whose merge rule accepts
//! *"an admin-signed provider record, plus a node's verified assertion"* in
//! place of a direct admin signature. That is a change to the authority
//! model, it is tractable, and it wants its own review rather than being
//! slipped in beside a JWT parser. It is recorded as the open half of
//! E1-P5.
//!
//! ## Why the JWT plumbing is hand-written and the crypto is not
//!
//! The obvious move is `jsonwebtoken`. It was measured and refused: with
//! `rust_crypto` it pulls the `rsa` crate, which carries RUSTSEC-2023-0071
//! (the Marvin timing attack) — and `deny.toml` ignores that advisory on
//! the written condition that **none of those crates reach `meshnode`**.
//! Adding it here would have moved a known-vulnerable RSA implementation
//! into the binary customers install on their fleets. Its `aws_lc_rs`
//! alternative is a C library, which breaks the static musl cross-build and
//! the project's no-C-toolchain rule.
//!
//! So the **signature verification is `ring`'s** — already in the tree via
//! rustls, constant-time, and the part that must not be hand-rolled — and
//! what is written here is the envelope: split on dots, base64url-decode,
//! parse two JSON objects, and check claims. The two classic JWT
//! vulnerabilities live in exactly that envelope, and both are closed
//! structurally rather than by vigilance:
//!
//! - **`alg: none`** cannot be expressed: [`mesh_proto::JwtAlg`] has no such
//!   variant, and an unparseable `alg` is a refusal.
//! - **Algorithm confusion** (handing an RSA public key to an HMAC verifier
//!   so the public key becomes the shared secret) needs a symmetric
//!   algorithm to confuse it with; there is none, and the chosen algorithm
//!   is additionally cross-checked against the JWK's own `kty` before any
//!   key material is touched.
//!
//! Every one of those is a test in this file, because a security property
//! nobody asserted is a security property nobody has.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::error::{CoreError, Result};
use mesh_proto::oidc::{CLOCK_SKEW_SECS, MAX_JWKS_BYTES};
use mesh_proto::{JwtAlg, NodeId, OidcAssertion, OidcProvider, UserId, nonce_for_key};

/// How long a fetched key set is reused before it is fetched again.
///
/// Providers rotate signing keys on the order of days and publish the new
/// one well before they use it, so an hour is comfortable. A `kid` that is
/// not in the cache forces an immediate refetch regardless (see
/// [`Jwks::key_for`]), which is what actually handles rotation — the TTL
/// only bounds how long a *withdrawn* key stays usable.
const JWKS_TTL: Duration = Duration::from_secs(3600);

/// Refuse to even look at a token larger than this. Real ID tokens are 1–4
/// KB; Entra with many group claims can reach 8.
const MAX_TOKEN_BYTES: usize = 64 * 1024;

// ── JWK ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Jwk {
    #[serde(default)]
    kid: String,
    kty: String,
    /// RSA modulus, base64url.
    #[serde(default)]
    n: String,
    /// RSA exponent, base64url.
    #[serde(default)]
    e: String,
    /// EC coordinates, base64url.
    #[serde(default)]
    x: String,
    #[serde(default)]
    y: String,
    #[serde(default)]
    crv: String,
    /// Some providers publish `use` and `alg`; both are advisory and
    /// neither is trusted for algorithm selection.
    #[serde(default, rename = "use")]
    use_: String,
}

#[derive(Debug, Clone, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

/// A provider's key set plus when it was fetched.
#[derive(Debug, Clone)]
struct CachedKeys {
    keys: Vec<Jwk>,
    fetched: Instant,
    /// When an *unknown-`kid`* refetch last went out for this URI (S-L4).
    /// Separate from `fetched`, which is the TTL clock: a rotation should
    /// still be picked up promptly, and a token naming a key that does not
    /// exist should not be able to ask again immediately.
    probed: Option<Instant>,
}

/// How often an unknown `kid` may trigger a refetch of one key set (S-L4).
///
/// The refetch is what makes key rotation invisible: a provider that starts
/// signing with a new key causes one fetch rather than an outage. But there
/// was no bound on *attempts* — every token naming an unknown `kid` sent one
/// request, so a login flood became an outbound request flood aimed at the
/// identity provider, from a node, with no rate limit and no negative cache.
///
/// Thirty seconds is short enough that a real rotation is picked up while
/// the person is still looking at the browser tab, and long enough that a
/// flood costs two requests a minute.
const JWKS_PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Cache of signing keys, keyed by JWKS URI.
///
/// Shared across the daemon so a burst of logins is one fetch, and
/// deliberately in memory only: a key set is public, cheap to refetch, and
/// persisting it would mean a node could verify against keys the provider
/// withdrew before the node last restarted.
#[derive(Debug, Default)]
pub struct Jwks {
    inner: Mutex<HashMap<String, CachedKeys>>,
}

impl Jwks {
    pub fn new() -> Arc<Self> {
        Arc::new(Jwks::default())
    }

    fn cached(&self, uri: &str) -> Option<Vec<Jwk>> {
        let map = self.inner.lock();
        let entry = map.get(uri)?;
        (entry.fetched.elapsed() < JWKS_TTL).then(|| entry.keys.clone())
    }

    fn store(&self, uri: &str, keys: Vec<Jwk>) {
        let mut map = self.inner.lock();
        let probed = map.get(uri).and_then(|e| e.probed);
        map.insert(
            uri.to_string(),
            CachedKeys {
                keys,
                fetched: Instant::now(),
                probed,
            },
        );
    }

    /// May an unknown `kid` send a request to this provider right now?
    /// Records the attempt when it may (S-L4).
    fn may_probe(&self, uri: &str) -> bool {
        let mut map = self.inner.lock();
        let Some(entry) = map.get_mut(uri) else {
            // Nothing cached: this is the first fetch, not a probe.
            return true;
        };
        let now = Instant::now();
        match entry.probed {
            Some(at) if now.duration_since(at) < JWKS_PROBE_INTERVAL => false,
            _ => {
                entry.probed = Some(now);
                true
            }
        }
    }

    /// The key with this `kid`, fetching the set if it is stale or if the
    /// `kid` is unknown.
    ///
    /// The unknown-`kid` refetch is what makes rotation work without an
    /// operator noticing: a provider that starts signing with a new key
    /// causes exactly one extra fetch, not an outage.
    ///
    /// It is bounded twice (S-L4). Once per verification attempt, which was
    /// always true and is not enough — attempts are what an attacker
    /// controls, so a login flood was an outbound request flood aimed at the
    /// identity provider. And once per [`JWKS_PROBE_INTERVAL`] per key set,
    /// which is the bound that was missing: past it, a `kid` that is not in
    /// the cached set is refused from cache without a request.
    async fn key_for(&self, http: &crate::httpc::Client, uri: &str, kid: &str) -> Result<Jwk> {
        if let Some(keys) = self.cached(uri) {
            if let Some(k) = pick(&keys, kid) {
                return Ok(k);
            }
            if !self.may_probe(uri) {
                return Err(CoreError::Identity(format!(
                    "the provider's key set has no key {kid:?} — the token was signed with a \
                     key this issuer does not publish"
                )));
            }
        }
        let keys = self.fetch(http, uri).await?;
        self.store(uri, keys.clone());
        pick(&keys, kid).ok_or_else(|| {
            CoreError::Identity(format!(
                "the provider's key set has no key {kid:?} — the token was signed with a key \
                 this issuer does not publish"
            ))
        })
    }

    async fn fetch(&self, http: &crate::httpc::Client, uri: &str) -> Result<Vec<Jwk>> {
        if !uri.starts_with("https://") {
            return Err(CoreError::Identity(format!(
                "refusing to fetch signing keys over an insecure transport: {uri}"
            )));
        }
        let resp = http
            .get(uri)
            .await
            .map_err(|e| CoreError::Identity(format!("could not fetch {uri}: {e}")))?;
        if resp.status != 200 {
            return Err(CoreError::Identity(format!(
                "{uri} answered {} fetching signing keys",
                resp.status
            )));
        }
        if resp.body.len() > MAX_JWKS_BYTES {
            return Err(CoreError::Identity(format!(
                "{uri} returned {} bytes of key set; the ceiling is {MAX_JWKS_BYTES}",
                resp.body.len()
            )));
        }
        let set: JwkSet = serde_json::from_slice(&resp.body)
            .map_err(|e| CoreError::Identity(format!("{uri} is not a JWK set: {e}")))?;
        if set.keys.is_empty() {
            return Err(CoreError::Identity(format!("{uri} published no keys")));
        }
        Ok(set.keys)
    }
}

fn pick(keys: &[Jwk], kid: &str) -> Option<Jwk> {
    // A `kid` is optional in the spec; when the token omits it and the set
    // holds exactly one key, that key is unambiguous. With several, refuse
    // rather than guess — trying each in turn is how a verifier ends up
    // accepting a signature from the wrong key.
    if kid.is_empty() {
        return (keys.len() == 1).then(|| keys[0].clone());
    }
    keys.iter().find(|k| k.kid == kid).cloned()
}

// ── the token ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct Header {
    alg: String,
    #[serde(default)]
    kid: String,
}

/// Claims meshware reads. Everything else in the token is ignored, which is
/// the correct default: a claim nobody looks at cannot influence a decision.
#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    /// `aud` is a string or an array of strings in the wild.
    aud: Audience,
    exp: u64,
    #[serde(default)]
    nbf: Option<u64>,
    #[serde(default)]
    nonce: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    preferred_username: String,
    #[serde(default)]
    name: String,
    #[serde(flatten)]
    rest: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn contains(&self, want: &str) -> bool {
        match self {
            Audience::One(a) => a == want,
            Audience::Many(v) => v.iter().any(|a| a == want),
        }
    }
}

fn b64url(s: &str) -> Result<Vec<u8>> {
    data_encoding::BASE64URL_NOPAD
        .decode(s.as_bytes())
        .map_err(|e| CoreError::Identity(format!("token segment is not base64url: {e}")))
}

/// Verify an ID token against `provider`, and require it to be bound to
/// `expecting` by the OIDC nonce.
///
/// Every check is a refusal, never a warning. The order is deliberate:
/// cheap structural checks first, the signature before any claim is
/// believed, and the nonce binding last because it is the one that makes
/// the whole thing about *this* key rather than about some key.
pub async fn verify_id_token(
    http: &crate::httpc::Client,
    jwks: &Jwks,
    provider: &OidcProvider,
    token: &str,
    expecting: &UserId,
    verified_by: NodeId,
    now_secs: u64,
) -> Result<OidcAssertion> {
    if token.len() > MAX_TOKEN_BYTES {
        return Err(CoreError::Identity(format!(
            "id token is {} bytes; the ceiling is {MAX_TOKEN_BYTES}",
            token.len()
        )));
    }
    // Only far enough to learn which key to fetch. Everything that decides
    // whether the token is *good* happens in `verify_with_key`, which
    // re-parses from the raw token rather than trusting anything read here
    // — so the I/O path cannot hand the verifier a header it did not check
    // for itself.
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(CoreError::Identity(
            "id token is not a three-part JWS — an unsigned or encrypted token is refused".into(),
        ));
    }
    let header: Header = serde_json::from_slice(&b64url(parts[0])?)
        .map_err(|e| CoreError::Identity(format!("token header is not JSON: {e}")))?;

    let jwk = jwks.key_for(http, &provider.jwks_uri, &header.kid).await?;
    verify_with_key(provider, token, expecting, verified_by, now_secs, &jwk)
}

/// The whole of the verification logic, with the key already in hand.
///
/// Split out from [`verify_id_token`] so that **every security property in
/// this module is testable without a network**: fetching a key set is I/O
/// and has nothing to do with whether a token is good. The tests below sign
/// real ES256 tokens with `ring` and drive this directly, including the
/// attacks.
pub(crate) fn verify_with_key(
    provider: &OidcProvider,
    token: &str,
    expecting: &UserId,
    verified_by: NodeId,
    now_secs: u64,
    jwk: &Jwk,
) -> Result<OidcAssertion> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err(CoreError::Identity(
            "id token is not a three-part JWS — an unsigned or encrypted token is refused".into(),
        ));
    }
    let (h, c, sig) = (parts[0], parts[1], parts[2]);
    let header: Header = serde_json::from_slice(&b64url(h)?)
        .map_err(|e| CoreError::Identity(format!("token header is not JSON: {e}")))?;
    let alg = JwtAlg::parse(&header.alg).ok_or_else(|| {
        CoreError::Identity(format!(
            "unsupported token algorithm {:?} — meshware accepts RS256 and ES256",
            header.alg
        ))
    })?;

    // Cross-check: the algorithm the *token* asked for must match the key
    // type the *provider* published. This is the structural half of the
    // algorithm-confusion defence — the enum makes the symmetric attack
    // unrepresentable, and this makes an RSA/EC mix-up impossible too.
    if jwk.kty != alg.required_kty() {
        return Err(CoreError::Identity(format!(
            "token claims {} but key {:?} is a {} key",
            alg.as_str(),
            jwk.kid,
            jwk.kty
        )));
    }
    if !jwk.use_.is_empty() && jwk.use_ != "sig" {
        return Err(CoreError::Identity(format!(
            "key {:?} is published for {:?}, not for signing",
            jwk.kid, jwk.use_
        )));
    }

    let signed = format!("{h}.{c}");
    let signature = b64url(sig)?;
    verify_signature(alg, jwk, signed.as_bytes(), &signature)?;

    // Only now is anything in the token believed.
    let claims: Claims = serde_json::from_slice(&b64url(c)?)
        .map_err(|e| CoreError::Identity(format!("token claims are not JSON: {e}")))?;

    if claims.iss != provider.issuer {
        return Err(CoreError::Identity(format!(
            "token issuer {:?} is not the configured issuer {:?}",
            claims.iss, provider.issuer
        )));
    }
    if !claims.aud.contains(&provider.client_id) {
        return Err(CoreError::Identity(format!(
            "token audience does not include this mesh's client id {:?}",
            provider.client_id
        )));
    }
    if now_secs.saturating_sub(CLOCK_SKEW_SECS) > claims.exp {
        return Err(CoreError::Identity(
            "id token has expired — run `meshnode login` again".into(),
        ));
    }
    if let Some(nbf) = claims.nbf
        && now_secs.saturating_add(CLOCK_SKEW_SECS) < nbf
    {
        return Err(CoreError::Identity(
            "id token is not valid yet; check this machine's clock".into(),
        ));
    }

    // The binding. Without this the token proves a human authenticated
    // somewhere, and nothing at all about whose key is enrolling.
    let want = nonce_for_key(expecting);
    if claims.nonce.is_empty() {
        return Err(CoreError::Identity(
            "id token carries no nonce, so it cannot be bound to a meshware key. The provider \
             must be configured to echo the nonce into the id token"
                .into(),
        ));
    }
    if !constant_time_eq(claims.nonce.as_bytes(), want.as_bytes()) {
        return Err(CoreError::Identity(
            "id token nonce does not match the enrolling key — this token was issued for a \
             different key and cannot be replayed onto this one"
                .into(),
        ));
    }

    let groups = read_groups(&claims.rest, &provider.groups_claim);
    let suggested_role = provider.suggested_role(&groups);
    let display_name = if !claims.preferred_username.is_empty() {
        claims.preferred_username.clone()
    } else {
        claims.name.clone()
    };

    Ok(OidcAssertion {
        user_id: *expecting,
        verified_by,
        provider: provider.name.clone(),
        issuer: claims.iss,
        subject: claims.sub,
        email: claims.email,
        display_name,
        groups,
        suggested_role,
        verified_at_micros: now_secs.saturating_mul(1_000_000),
        expires_at_micros: now_secs
            .saturating_add(mesh_proto::oidc::ASSERTION_TTL_SECS)
            .saturating_mul(1_000_000),
    })
}

/// Groups from whichever claim the provider names. Accepts an array of
/// strings or a single string; anything else yields none rather than an
/// error, because a mesh should not refuse a valid login over a claim it
/// only uses for a *suggestion*.
fn read_groups(rest: &HashMap<String, serde_json::Value>, claim: &str) -> Vec<String> {
    match rest.get(claim) {
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Compare without leaking where two byte strings first differ.
///
/// The nonce is not a secret — it is a hash of a public key — so this is
/// belt and braces rather than load-bearing. It costs nothing and it means
/// nobody has to reason about whether it mattered.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The one thing here that is not hand-written: `ring` verifies, always.
fn verify_signature(alg: JwtAlg, jwk: &Jwk, signed: &[u8], signature: &[u8]) -> Result<()> {
    use ring::signature;
    let bad = || CoreError::Identity("id token signature is not valid".into());
    match alg {
        JwtAlg::Rs256 => {
            let n = b64url(&jwk.n)?;
            let e = b64url(&jwk.e)?;
            let key = signature::RsaPublicKeyComponents { n: &n, e: &e };
            key.verify(&signature::RSA_PKCS1_2048_8192_SHA256, signed, signature)
                .map_err(|_| bad())
        }
        JwtAlg::Es256 => {
            if jwk.crv != "P-256" {
                return Err(CoreError::Identity(format!(
                    "ES256 requires a P-256 key; this one is {:?}",
                    jwk.crv
                )));
            }
            let x = b64url(&jwk.x)?;
            let y = b64url(&jwk.y)?;
            // SEC1 uncompressed point: 0x04 || X || Y.
            let mut point = Vec::with_capacity(1 + x.len() + y.len());
            point.push(0x04);
            point.extend_from_slice(&x);
            point.extend_from_slice(&y);
            signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, &point)
                .verify(signed, signature)
                .map_err(|_| bad())
        }
    }
}

// ── the device-code flow (RFC 8628) ─────────────────────────────────────
//
// **Device code, and no other flow.** The authorization-code flow needs a
// redirect URI, which means an HTTP listener on the machine the person is
// sitting at. meshware does not run one and is not going to: the product's
// whole shape is that a node listens for QUIC from its mesh and nothing
// else. A device code is printed, approved in a browser on any device, and
// polled for **outbound** — which also happens to be the only flow that
// works over SSH on a headless box, which is where operators actually are.

/// What the provider says when a login begins.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceStart {
    pub device_code: String,
    /// The short code a person types. Printed verbatim.
    pub user_code: String,
    pub verification_uri: String,
    /// Some providers offer a URI with the code already in it.
    #[serde(default)]
    pub verification_uri_complete: String,
    #[serde(default = "default_expires")]
    pub expires_in: u64,
    /// Seconds between polls. Honoured, and raised on `slow_down`.
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_expires() -> u64 {
    600
}
fn default_interval() -> u64 {
    5
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
}

const FORM: &str = "application/x-www-form-urlencoded";

/// Percent-encode a form value. Small and explicit rather than a
/// dependency: the alphabet here is the unreserved set from RFC 3986.
fn form_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn form(pairs: &[(&str, &str)]) -> Vec<u8> {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", form_encode(k), form_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes()
}

/// Begin a login: ask the provider for a device code bound to `key`.
///
/// The `nonce` is what binds the eventual ID token to this meshware key. A
/// provider that ignores it produces a token with no nonce, and
/// [`verify_id_token`] refuses that with a message naming the
/// configuration to fix — which is the right failure: silently accepting an
/// unbound token would make the whole flow meaningless.
pub async fn begin_device_login(
    http: &crate::httpc::Client,
    provider: &OidcProvider,
    key: &UserId,
) -> Result<DeviceStart> {
    let mut scopes = provider.scopes.clone();
    if !scopes.iter().any(|s| s == "openid") {
        scopes.insert(0, "openid".into());
    }
    let scope = scopes.join(" ");
    let nonce = nonce_for_key(key);
    let body = form(&[
        ("client_id", provider.client_id.as_str()),
        ("scope", scope.as_str()),
        ("nonce", nonce.as_str()),
    ]);
    let headers = vec![("Content-Type".to_string(), FORM.to_string())];
    let resp = http
        .post_json(&provider.device_authorization_endpoint, &headers, &body)
        .await
        .map_err(|e| {
            CoreError::Identity(format!(
                "could not reach {}: {e}",
                provider.device_authorization_endpoint
            ))
        })?;
    if !(200..300).contains(&resp.status) {
        return Err(CoreError::Identity(format!(
            "{} refused the login request (HTTP {}): {}",
            provider.name,
            resp.status,
            String::from_utf8_lossy(&resp.body)
        )));
    }
    serde_json::from_slice(&resp.body).map_err(|e| {
        CoreError::Identity(format!(
            "{} sent an unreadable device response: {e}",
            provider.name
        ))
    })
}

/// The outcome of one poll of the token endpoint.
#[derive(Debug)]
pub enum PollOutcome {
    /// The person has not finished approving yet. Wait `interval` and ask
    /// again; `interval` may have been raised by a `slow_down`.
    Pending { interval: u64 },
    /// An ID token, unverified. It is verified by the node, not here.
    Token(String),
    /// Terminal: denied, expired, or a provider error.
    Failed(String),
}

/// Poll once for the token. The caller owns the sleeping, so a CLI can
/// print a countdown and a daemon can do something else.
pub async fn poll_device_login(
    http: &crate::httpc::Client,
    provider: &OidcProvider,
    device_code: &str,
    interval: u64,
) -> Result<PollOutcome> {
    let body = form(&[
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ("device_code", device_code),
        ("client_id", provider.client_id.as_str()),
    ]);
    let headers = vec![("Content-Type".to_string(), FORM.to_string())];
    let resp = http
        .post_json(&provider.token_endpoint, &headers, &body)
        .await
        .map_err(|e| {
            CoreError::Identity(format!("could not reach {}: {e}", provider.token_endpoint))
        })?;

    let parsed: TokenResponse = serde_json::from_slice(&resp.body).map_err(|e| {
        CoreError::Identity(format!(
            "{} sent an unreadable token response: {e}",
            provider.name
        ))
    })?;

    if !parsed.id_token.is_empty() {
        return Ok(PollOutcome::Token(parsed.id_token));
    }
    // RFC 8628 §3.5: these two are "keep waiting", everything else is over.
    Ok(match parsed.error.as_str() {
        "authorization_pending" => PollOutcome::Pending { interval },
        "slow_down" => PollOutcome::Pending {
            interval: interval.saturating_add(5),
        },
        "expired_token" => PollOutcome::Failed("the login expired before it was approved".into()),
        "access_denied" => PollOutcome::Failed("the login was declined".into()),
        "" => PollOutcome::Failed(format!(
            "{} returned neither an id token nor an error (HTTP {}). Check that the client is \
             configured to issue id tokens for the device flow",
            provider.name, resp.status
        )),
        other => PollOutcome::Failed(if parsed.error_description.is_empty() {
            other.to_string()
        } else {
            format!("{other}: {}", parsed.error_description)
        }),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn jwk_rsa(kid: &str) -> Jwk {
        Jwk {
            kid: kid.into(),
            kty: "RSA".into(),
            n: "AQAB".into(),
            e: "AQAB".into(),
            x: String::new(),
            y: String::new(),
            crv: String::new(),
            use_: String::new(),
        }
    }

    #[test]
    fn a_kid_is_matched_exactly() {
        let keys = vec![jwk_rsa("a"), jwk_rsa("b")];
        assert_eq!(pick(&keys, "a").unwrap().kid, "a");
        assert_eq!(pick(&keys, "b").unwrap().kid, "b");
        assert!(pick(&keys, "c").is_none(), "an unknown kid must not match");
    }

    #[test]
    fn an_absent_kid_resolves_only_when_there_is_no_ambiguity() {
        // One key: unambiguous, so a token without a kid can be checked.
        assert!(pick(&[jwk_rsa("only")], "").is_some());
        // Several: refuse rather than try each in turn, which is how a
        // verifier ends up accepting the wrong key's signature.
        assert!(pick(&[jwk_rsa("a"), jwk_rsa("b")], "").is_none());
        assert!(pick(&[], "").is_none());
    }

    #[test]
    fn groups_read_from_an_array_or_a_lone_string() {
        let mut rest = HashMap::new();
        rest.insert(
            "groups".to_string(),
            serde_json::json!(["platform", "eng-web"]),
        );
        assert_eq!(read_groups(&rest, "groups"), vec!["platform", "eng-web"]);

        let mut one = HashMap::new();
        one.insert("groups".to_string(), serde_json::json!("platform"));
        assert_eq!(read_groups(&one, "groups"), vec!["platform"]);

        // A claim that is neither, or missing, yields none rather than an
        // error: groups only feed a *suggestion*, and refusing a valid
        // login over one would be the wrong trade.
        let mut odd = HashMap::new();
        odd.insert("groups".to_string(), serde_json::json!(42));
        assert!(read_groups(&odd, "groups").is_empty());
        assert!(read_groups(&HashMap::new(), "groups").is_empty());
    }

    #[test]
    fn audience_accepts_a_string_or_an_array() {
        let one: Audience = serde_json::from_str("\"meshware\"").unwrap();
        assert!(one.contains("meshware"));
        assert!(!one.contains("something-else"));

        let many: Audience = serde_json::from_str("[\"a\",\"meshware\"]").unwrap();
        assert!(many.contains("meshware"));
        assert!(!many.contains("b"));
    }

    #[test]
    fn constant_time_eq_still_compares_correctly() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    // ── end-to-end, with real signatures ────────────────────────────
    //
    // ES256 rather than RS256 for one practical reason: `ring` can
    // *generate* a P-256 key and sign with it, so these tests exercise the
    // real verifier against real signatures with no fixtures to go stale.
    // The RS256 path shares every line of this file except the four inside
    // `verify_signature`, which are ring's.

    use mesh_proto::{GroupRole, Role};
    use ring::rand::SystemRandom;
    use ring::signature::{self, KeyPair};

    struct Signer {
        pair: signature::EcdsaKeyPair,
        rng: SystemRandom,
    }

    impl Signer {
        fn new() -> Signer {
            let rng = SystemRandom::new();
            let doc = signature::EcdsaKeyPair::generate_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                &rng,
            )
            .expect("generate p256");
            let pair = signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                doc.as_ref(),
                &rng,
            )
            .expect("parse p256");
            Signer { pair, rng }
        }

        /// The public half as the provider would publish it.
        fn jwk(&self, kid: &str) -> Jwk {
            let pt = self.pair.public_key().as_ref();
            assert_eq!(pt[0], 0x04, "uncompressed SEC1 point");
            let (x, y) = pt[1..].split_at(32);
            Jwk {
                kid: kid.into(),
                kty: "EC".into(),
                crv: "P-256".into(),
                x: data_encoding::BASE64URL_NOPAD.encode(x),
                y: data_encoding::BASE64URL_NOPAD.encode(y),
                n: String::new(),
                e: String::new(),
                use_: String::new(),
            }
        }

        fn token(&self, header: serde_json::Value, claims: serde_json::Value) -> String {
            let h = data_encoding::BASE64URL_NOPAD.encode(header.to_string().as_bytes());
            let c = data_encoding::BASE64URL_NOPAD.encode(claims.to_string().as_bytes());
            let signed = format!("{h}.{c}");
            let sig = self.pair.sign(&self.rng, signed.as_bytes()).expect("sign");
            format!(
                "{signed}.{}",
                data_encoding::BASE64URL_NOPAD.encode(sig.as_ref())
            )
        }
    }

    const NOW: u64 = 1_800_000_000;

    fn test_provider() -> OidcProvider {
        OidcProvider {
            name: "entra".into(),
            issuer: "https://login.example.com/v2.0".into(),
            client_id: "meshware".into(),
            jwks_uri: "https://login.example.com/keys".into(),
            device_authorization_endpoint: "https://login.example.com/devicecode".into(),
            token_endpoint: "https://login.example.com/token".into(),
            scopes: vec!["openid".into()],
            groups_claim: "groups".into(),
            group_roles: vec![GroupRole {
                group: "platform".into(),
                role: Role::Operator,
            }],
            configured_by: UserId([9u8; 32]),
            configured_at_micros: 0,
            signature: Vec::new(),
        }
    }

    fn good_claims(key: &UserId) -> serde_json::Value {
        serde_json::json!({
            "iss": "https://login.example.com/v2.0",
            "sub": "00u1abcd",
            "aud": "meshware",
            "exp": NOW + 600,
            "nbf": NOW - 10,
            "nonce": nonce_for_key(key),
            "email": "mia@example.com",
            "preferred_username": "mia",
            "groups": ["platform", "eng-web"],
        })
    }

    fn check(
        signer: &Signer,
        header: serde_json::Value,
        claims: serde_json::Value,
        key: &UserId,
    ) -> Result<OidcAssertion> {
        let token = signer.token(header, claims);
        verify_with_key(
            &test_provider(),
            &token,
            key,
            NodeId([7u8; 32]),
            NOW,
            &signer.jwk("k1"),
        )
    }

    #[test]
    fn a_good_token_verifies_and_carries_what_it_proved() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let a = check(
            &s,
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            good_claims(&key),
            &key,
        )
        .expect("a well-formed token from the configured issuer must verify");

        assert_eq!(a.user_id, key);
        assert_eq!(a.subject, "00u1abcd");
        assert_eq!(a.email, "mia@example.com");
        assert_eq!(a.display_name, "mia");
        assert_eq!(a.groups, vec!["platform", "eng-web"]);
        assert_eq!(a.suggested_role, Some(Role::Operator));
        assert_eq!(a.issuer, "https://login.example.com/v2.0");
        assert!(a.is_fresh(a.verified_at_micros));
    }

    /// **The headline attack.** A token with no signature and `alg: none`
    /// is what a naive verifier accepts.
    #[test]
    fn alg_none_is_refused() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let claims = good_claims(&key);
        let h = data_encoding::BASE64URL_NOPAD
            .encode(serde_json::json!({"alg": "none"}).to_string().as_bytes());
        let c = data_encoding::BASE64URL_NOPAD.encode(claims.to_string().as_bytes());
        // The classic shape: three parts, empty signature.
        let token = format!("{h}.{c}.");
        let err = verify_with_key(
            &test_provider(),
            &token,
            &key,
            NodeId([7u8; 32]),
            NOW,
            &s.jwk("k1"),
        )
        .expect_err("alg=none must be refused");
        assert!(
            format!("{err}").contains("unsupported token algorithm"),
            "wrong reason: {err}"
        );
    }

    /// **Algorithm confusion.** `HS256` would let the public key be used as
    /// an HMAC secret by a verifier that trusts the header.
    #[test]
    fn a_symmetric_algorithm_is_refused() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let err = check(
            &s,
            serde_json::json!({"alg": "HS256", "kid": "k1"}),
            good_claims(&key),
            &key,
        )
        .expect_err("HS256 must be refused");
        assert!(format!("{err}").contains("unsupported token algorithm"));
    }

    /// The header cannot select a verifier the published key does not match.
    #[test]
    fn an_algorithm_the_key_type_does_not_match_is_refused() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let token = s.token(
            serde_json::json!({"alg": "RS256", "kid": "k1"}),
            good_claims(&key),
        );
        // RS256 named, but the provider publishes an EC key.
        let err = verify_with_key(
            &test_provider(),
            &token,
            &key,
            NodeId([7u8; 32]),
            NOW,
            &s.jwk("k1"),
        )
        .expect_err("an RSA algorithm against an EC key must be refused");
        assert!(format!("{err}").contains("is a EC key"), "was: {err}");
    }

    #[test]
    fn a_tampered_payload_is_refused() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let token = s.token(
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            good_claims(&key),
        );
        // Swap the claims for ones the signature does not cover.
        let parts: Vec<&str> = token.split('.').collect();
        let forged = data_encoding::BASE64URL_NOPAD.encode(
            serde_json::json!({
                "iss": "https://login.example.com/v2.0",
                "sub": "attacker",
                "aud": "meshware",
                "exp": NOW + 600,
                "nonce": nonce_for_key(&key),
            })
            .to_string()
            .as_bytes(),
        );
        let token = format!("{}.{forged}.{}", parts[0], parts[2]);
        let err = verify_with_key(
            &test_provider(),
            &token,
            &key,
            NodeId([7u8; 32]),
            NOW,
            &s.jwk("k1"),
        )
        .expect_err("a re-written payload must not verify");
        assert!(
            format!("{err}").contains("signature is not valid"),
            "was: {err}"
        );
    }

    #[test]
    fn a_token_signed_by_the_wrong_key_is_refused() {
        let real = Signer::new();
        let attacker = Signer::new();
        let key = UserId([3u8; 32]);
        let token = attacker.token(
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            good_claims(&key),
        );
        let err = verify_with_key(
            &test_provider(),
            &token,
            &key,
            NodeId([7u8; 32]),
            NOW,
            &real.jwk("k1"),
        )
        .expect_err("a signature from a key the issuer does not publish must be refused");
        assert!(format!("{err}").contains("signature is not valid"));
    }

    /// **The binding.** A token minted for somebody else's key must not be
    /// replayable onto mine, which is the whole point of the nonce.
    #[test]
    fn a_token_bound_to_another_key_is_refused() {
        let s = Signer::new();
        let mine = UserId([3u8; 32]);
        let theirs = UserId([4u8; 32]);
        // Perfectly valid token — for a different key.
        let err = check(
            &s,
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            good_claims(&theirs),
            &mine,
        )
        .expect_err("a token bound to another key must be refused");
        assert!(
            format!("{err}").contains("nonce does not match"),
            "was: {err}"
        );
    }

    #[test]
    fn a_token_with_no_nonce_is_refused() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let mut claims = good_claims(&key);
        claims["nonce"] = serde_json::json!("");
        let err = check(
            &s,
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            claims,
            &key,
        )
        .expect_err("without a nonce the token proves nothing about the key");
        assert!(format!("{err}").contains("carries no nonce"), "was: {err}");
    }

    #[test]
    fn another_issuer_is_refused_even_with_a_valid_signature() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let mut claims = good_claims(&key);
        claims["iss"] = serde_json::json!("https://evil.example.com");
        let err = check(
            &s,
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            claims,
            &key,
        )
        .expect_err("only the configured issuer is believed");
        assert!(format!("{err}").contains("is not the configured issuer"));
    }

    #[test]
    fn a_token_for_another_audience_is_refused() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let mut claims = good_claims(&key);
        claims["aud"] = serde_json::json!("some-other-app");
        let err = check(
            &s,
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            claims,
            &key,
        )
        .expect_err("a token minted for another relying party must be refused");
        assert!(format!("{err}").contains("audience"));
    }

    #[test]
    fn an_expired_token_is_refused_and_a_fresh_one_is_not() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let mut claims = good_claims(&key);
        claims["exp"] = serde_json::json!(NOW - CLOCK_SKEW_SECS - 60);
        let err = check(
            &s,
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            claims,
            &key,
        )
        .expect_err("an expired token must be refused");
        assert!(format!("{err}").contains("expired"));

        // Just inside the skew window still verifies, so a mesh does not
        // reject every login over a two-second clock difference.
        let mut ok = good_claims(&key);
        ok["exp"] = serde_json::json!(NOW - 30);
        assert!(
            check(
                &s,
                serde_json::json!({"alg": "ES256", "kid": "k1"}),
                ok,
                &key
            )
            .is_ok(),
            "a token inside the clock-skew window must still verify"
        );
    }

    #[test]
    fn an_audience_array_containing_us_is_accepted() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let mut claims = good_claims(&key);
        claims["aud"] = serde_json::json!(["someone-else", "meshware"]);
        assert!(
            check(
                &s,
                serde_json::json!({"alg": "ES256", "kid": "k1"}),
                claims,
                &key
            )
            .is_ok()
        );
    }

    #[test]
    fn a_key_published_for_encryption_is_not_used_for_signatures() {
        let s = Signer::new();
        let key = UserId([3u8; 32]);
        let mut jwk = s.jwk("k1");
        jwk.use_ = "enc".into();
        let token = s.token(
            serde_json::json!({"alg": "ES256", "kid": "k1"}),
            good_claims(&key),
        );
        let err = verify_with_key(&test_provider(), &token, &key, NodeId([7u8; 32]), NOW, &jwk)
            .expect_err("a key the issuer published for encryption must not verify signatures");
        assert!(format!("{err}").contains("not for signing"), "was: {err}");
    }

    // ── device flow: the parts that do not need a provider ──────────

    #[test]
    fn form_encoding_escapes_what_a_query_string_cannot_carry() {
        assert_eq!(form_encode("openid profile"), "openid%20profile");
        assert_eq!(form_encode("a+b"), "a%2Bb");
        assert_eq!(form_encode("a&b=c"), "a%26b%3Dc");
        // The unreserved set survives intact.
        assert_eq!(form_encode("aZ0-._~"), "aZ0-._~");
        // A device-code grant type is mostly reserved characters.
        assert_eq!(
            form_encode("urn:ietf:params:oauth:grant-type:device_code"),
            "urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code"
        );
    }

    #[test]
    fn a_form_body_is_joined_and_encoded() {
        let b = form(&[("client_id", "mesh ware"), ("scope", "openid groups")]);
        assert_eq!(
            String::from_utf8(b).unwrap(),
            "client_id=mesh%20ware&scope=openid%20groups"
        );
    }

    #[test]
    fn a_device_response_parses_with_the_optional_fields_absent() {
        let d: DeviceStart = serde_json::from_str(
            r#"{"device_code":"dc","user_code":"ABCD-EFGH",
                "verification_uri":"https://example.com/device"}"#,
        )
        .expect("the required fields are enough");
        assert_eq!(d.user_code, "ABCD-EFGH");
        // Defaults, so a provider that omits them does not break the flow.
        assert_eq!(d.expires_in, 600);
        assert_eq!(d.interval, 5);
        assert!(d.verification_uri_complete.is_empty());
    }

    #[test]
    fn base64url_rejects_standard_base64_padding() {
        // JWT segments are base64url *without* padding; accepting padded or
        // standard-alphabet input would mean accepting a token shape the
        // spec does not define.
        assert!(b64url("aGVsbG8").is_ok());
        assert!(b64url("aGVsbG8=").is_err());
        assert!(b64url("a+b/c").is_err());
    }
}
