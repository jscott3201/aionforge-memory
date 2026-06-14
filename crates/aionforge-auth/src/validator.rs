//! The public [`JwtValidator`]: discovery + JWKS cache + RS256-pinned validation.
//!
//! One validator is built per [`aionforge_config::IssuerConfig`]. Construction
//! resolves the `jwks_uri` (from the config or via OIDC discovery), fetches the JWKS once, and
//! prepares the [`jsonwebtoken::Validation`] for the issuer. Each
//! [`JwtValidator::validate`] call reads the token's `kid`, looks it up in the in-memory cache
//! under a short-held lock, and — on an unknown-`kid` miss — performs **one** bounded JWKS
//! refetch (fetched outside the lock, then swapped in) before failing closed. The validator is
//! `Send + Sync` and may be shared across concurrent calls.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aionforge_config::IssuerConfig;
use jsonwebtoken::Validation;

use crate::discovery::resolve_jwks_uri;
use crate::error::AuthError;
use crate::fetch::jwks_uri_origin_is_allowed;
use crate::jwks::{JwksCache, fetch_jwks};
use crate::validate::{VerifiedClaims, build_validation, decode_and_validate};

/// How long an HTTP request to the discovery or JWKS endpoint may take before failing closed.
/// Bounds each fetch so a slow or hanging issuer cannot stall a validation indefinitely.
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum interval between unknown-`kid` JWKS refetches, across all concurrent `validate`
/// calls. A flood of tokens each carrying a distinct random `kid` would otherwise drive one
/// outbound JWKS fetch per request at full request throughput; the cooldown caps that to one
/// fetch per window, returning [`AuthError::NoMatchingKey`] from the cache in between. Thirty
/// seconds still picks up a genuine key rotation within a request or two of the IdP publishing
/// it, while making the resource server useless as a fetch-amplification lever against the IdP.
const REFETCH_COOLDOWN: Duration = Duration::from_secs(30);

/// Install the ring crypto provider as the process default exactly once.
///
/// reqwest is built with `rustls-no-provider`, which carries no crypto provider, so a `Client`
/// constructed before a provider is installed panics. ring is installed here (never aws-lc-rs)
/// to keep the aws-lc-sys C/cmake build out of the tree; the install is process-global and
/// first-writer-wins, so a host that already set a provider is left untouched.
fn ensure_crypto_provider() {
    use std::sync::Once;
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// An OIDC resource-server JWT validator for a single trusted issuer.
///
/// Verifies RS256/384/512 signatures against the issuer's JWKS, with exact issuer/audience
/// matching and configured clock-skew leeway. Holds no private key — it only ever verifies
/// with public keys. `alg=none` and `HS*` algorithm confusion are rejected by the pinned
/// algorithm allow-list (see the crate-internal `validate` module).
#[derive(Clone)]
pub struct JwtValidator {
    client: reqwest::Client,
    issuer: String,
    audience: String,
    jwks_uri: String,
    validation: Arc<Validation>,
    cache: Arc<Mutex<JwksCache>>,
    /// The instant of the last unknown-`kid` JWKS refetch attempt, used to rate-limit refetches
    /// across calls. `None` until the first refetch. Guarded by its own short-held lock.
    last_refetch: Arc<Mutex<Option<Instant>>>,
}

impl std::fmt::Debug for JwtValidator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // No token, claim, or key material is ever held in plain fields, but we still keep the
        // debug surface to non-secret identifiers only.
        f.debug_struct("JwtValidator")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("jwks_uri", &self.jwks_uri)
            .finish_non_exhaustive()
    }
}

impl JwtValidator {
    /// Build a validator from an [`IssuerConfig`].
    ///
    /// Resolves the JWKS endpoint (the config's `jwks_uri` if set, otherwise via the issuer's
    /// OIDC discovery document), fetches the JWKS once into the cache, and prepares the
    /// RS256-pinned validation. The config is assumed already validated by the config layer
    /// (https/loopback issuer, non-empty audience, RSA-only `allowed_algs`); this constructor
    /// does not re-validate those invariants.
    ///
    /// # Errors
    /// Returns [`AuthError::Discovery`] if discovery is needed and fails,
    /// [`AuthError::JwksRefresh`] if the JWKS cannot be fetched or parsed, or
    /// [`AuthError::AlgorithmNotAllowed`] if `allowed_algs` is empty or non-RSA.
    pub async fn new(config: &IssuerConfig) -> Result<Self, AuthError> {
        ensure_crypto_provider();
        // No redirects: key/discovery material must never be fetched across a redirect, and a
        // redirect would otherwise bypass the same-origin `jwks_uri` guard below (reqwest's
        // default is `Policy::limited(10)`).
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                AuthError::Discovery(format!("could not build the HTTP client: {error}"))
            })?;
        let jwks_uri = match &config.jwks_uri {
            // A config-pinned jwks_uri bypasses discovery, so the config layer never saw it:
            // enforce the same-origin SSRF guard here so a config with a legit issuer but an
            // internal/metadata jwks_uri (e.g. 169.254.169.254) is refused, not fetched.
            Some(uri) => {
                if !jwks_uri_origin_is_allowed(uri, &config.issuer) {
                    return Err(AuthError::JwksRefresh(
                        "the configured jwks_uri is not on the issuer's origin".to_owned(),
                    ));
                }
                uri.clone()
            }
            None => resolve_jwks_uri(&config.issuer, &client).await?,
        };
        let validation = build_validation(
            &config.issuer,
            &config.audience,
            &config.allowed_algs,
            config.leeway_secs,
        )?;
        let cache = fetch_jwks(&jwks_uri, &client).await?;
        Ok(Self {
            client,
            issuer: config.issuer.clone(),
            audience: config.audience.clone(),
            jwks_uri,
            validation: Arc::new(validation),
            cache: Arc::new(Mutex::new(cache)),
            last_refetch: Arc::new(Mutex::new(None)),
        })
    }

    /// Validate a bearer token string, returning its verified claims.
    ///
    /// The flow is: read the token's `kid` from its header; look it up in the cache; on a
    /// miss, perform one bounded JWKS refetch and retry; then decode against the matched key
    /// with the RS256-pinned validation. Every failure is an [`AuthError`] — the method never
    /// panics on a malformed token, an unreachable JWKS, or a crypto error, and never leaks the
    /// token or a claim into the error.
    ///
    /// # Errors
    /// Returns [`AuthError::InvalidToken`] for a malformed/unsigned/expired token,
    /// [`AuthError::NoMatchingKey`] if the `kid` is absent after one refetch,
    /// [`AuthError::JwksRefresh`] if the refetch itself fails, and the issuer/audience/algorithm
    /// verdicts for the corresponding claim mismatches.
    pub async fn validate(&self, token: &str) -> Result<VerifiedClaims, AuthError> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|_| AuthError::InvalidToken("token header is malformed".to_owned()))?;
        let kid = header
            .kid
            .ok_or_else(|| AuthError::InvalidToken("token header has no kid".to_owned()))?;

        // Fast path: the kid is already cached. Clone the key out under a short-held lock so the
        // decode (and any error mapping) happens without holding the mutex.
        if let Some(key) = self.cached_key(&kid)? {
            return decode_and_validate(token, &key, &self.validation, &self.audience);
        }

        // Unknown kid. Refetching is rate-limited across calls: if we are still inside the
        // cooldown since the last refetch attempt, do not touch the network — a flood of tokens
        // with distinct kids must not amplify into one IdP fetch per request. Claim the cooldown
        // window atomically (under the lock) so concurrent unknown-kid calls coalesce onto one
        // fetch instead of all racing through.
        if !self.claim_refetch_slot()? {
            return Err(AuthError::NoMatchingKey(kid));
        }

        // One bounded refetch (outside the lock), swap the cache, retry the lookup.
        let fresh = fetch_jwks(&self.jwks_uri, &self.client).await?;
        let key = {
            let mut guard = self.cache.lock().map_err(|_| {
                AuthError::JwksRefresh("the JWKS cache lock was poisoned".to_owned())
            })?;
            *guard = fresh;
            guard.get(&kid).cloned()
        };
        match key {
            Some(key) => decode_and_validate(token, &key, &self.validation, &self.audience),
            None => Err(AuthError::NoMatchingKey(kid)),
        }
    }

    /// Try to claim the refetch slot, returning `true` if this call may refetch now.
    ///
    /// Returns `false` when the previous refetch was within [`REFETCH_COOLDOWN`], so the caller
    /// answers the unknown `kid` from the cache instead of hitting the network. On `true` the
    /// last-refetch instant is advanced to now under the lock, so a burst of concurrent
    /// unknown-`kid` calls yields exactly one network fetch per cooldown window. The instant is
    /// claimed (not the fetch result) deliberately: even a failing fetch consumes the window, so
    /// a flood cannot retry the network every request.
    fn claim_refetch_slot(&self) -> Result<bool, AuthError> {
        let mut last = self.last_refetch.lock().map_err(|_| {
            AuthError::JwksRefresh("the JWKS refetch clock lock was poisoned".to_owned())
        })?;
        let now = Instant::now();
        if let Some(previous) = *last
            && now.duration_since(previous) < REFETCH_COOLDOWN
        {
            return Ok(false);
        }
        *last = Some(now);
        Ok(true)
    }

    /// Look up a `kid` in the cache, cloning the key out under a short-held lock.
    ///
    /// Returns `Ok(None)` on a clean miss (the caller then refetches) and an error only if the
    /// lock is poisoned (a panic in another holder), which is mapped to a refresh failure
    /// rather than propagating the panic.
    fn cached_key(&self, kid: &str) -> Result<Option<jsonwebtoken::DecodingKey>, AuthError> {
        let guard = self
            .cache
            .lock()
            .map_err(|_| AuthError::JwksRefresh("the JWKS cache lock was poisoned".to_owned()))?;
        Ok(guard.get(kid).cloned())
    }
}
