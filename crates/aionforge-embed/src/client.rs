//! The OpenAI-compatible HTTP embedding client.

use std::sync::Once;
use std::time::Duration;

use aionforge_config::EmbedderConfig;
use aionforge_domain::contracts::Embedder;
use aionforge_domain::embedding::{EmbedderModel, Embedding};
use secrecy::{ExposeSecret, SecretString};

use crate::error::EmbedError;
use crate::wire::{EmbeddingRequest, EmbeddingResponse};

/// Install the ring crypto provider as the process default exactly once.
///
/// reqwest is built with `rustls-no-provider`, which carries no crypto provider, so
/// a `Client` constructed before a provider is installed panics. We install ring
/// here rather than aws-lc-rs to keep the aws-lc-sys C/cmake build out of the tree.
/// The install is process-global and first-writer-wins: if the host already set a
/// default provider, that one stands and the returned error is ignored.
fn ensure_crypto_provider() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// An OpenAI-compatible embedding client.
///
/// Batches are sent in one request and returned in input order: the response's per-item
/// index places each vector back into its slot, so a provider that reorders does not
/// scramble results. Every returned vector is checked against the model's dimension,
/// validated, and L2-normalized (cosine is the default metric, and the engine has no
/// normalization primitive, so it is the client's job).
#[derive(Debug)]
pub struct HttpEmbedder {
    client: reqwest::Client,
    embeddings_url: String,
    request_model: String,
    identity: EmbedderModel,
    api_key: Option<SecretString>,
}

impl HttpEmbedder {
    /// Build a client against `endpoint` (an OpenAI-compatible base URL such as
    /// `https://host/v1`).
    ///
    /// `request_model` is the provider's model id sent in each request; `identity` is the
    /// [`EmbedderModel`] recorded on every embedding for provenance and the cross-family
    /// guard. `api_key`, when set, is sent as a bearer token and never logged.
    ///
    /// # Errors
    /// Returns [`EmbedError::Config`] if the HTTP client cannot be built.
    pub fn new(
        endpoint: &str,
        request_model: impl Into<String>,
        identity: EmbedderModel,
        api_key: Option<SecretString>,
        timeout: Duration,
    ) -> Result<Self, EmbedError> {
        // §8.4: enforce the transport rule here too, so constructing a client directly
        // cannot slip past the config-time check.
        if !aionforge_config::endpoint_transport_is_allowed(endpoint) {
            return Err(EmbedError::Config(format!(
                "endpoint {endpoint} must use https:// unless the host is localhost"
            )));
        }
        // reqwest's `rustls-no-provider` ships no crypto provider, so a client built
        // before one is installed panics; install ring as the process default first.
        ensure_crypto_provider();
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| EmbedError::Config(error.to_string()))?;
        let base = endpoint.trim_end_matches('/');
        Ok(Self {
            client,
            embeddings_url: format!("{base}/embeddings"),
            request_model: request_model.into(),
            identity,
            api_key,
        })
    }

    /// Build a client from an [`EmbedderConfig`] and an already-resolved API key.
    ///
    /// The recorded identity takes the configured model id as its family (the id carries
    /// the version) and the configured dimension.
    ///
    /// # Errors
    /// Returns [`EmbedError::Config`] if the HTTP client cannot be built.
    pub fn from_config(
        config: &EmbedderConfig,
        api_key: Option<SecretString>,
    ) -> Result<Self, EmbedError> {
        let identity = EmbedderModel {
            family: config.model.clone(),
            version: String::new(),
            dimension: config.dimension,
        };
        Self::new(
            &config.endpoint,
            config.model.clone(),
            identity,
            api_key,
            Duration::from_millis(config.timeout_ms),
        )
    }

    /// Send the batch and turn the response into input-ordered, normalized embeddings.
    async fn embed_batch(&self, inputs: &[String]) -> Result<Vec<Embedding>, EmbedError> {
        let mut request = self
            .client
            .post(&self.embeddings_url)
            .json(&EmbeddingRequest {
                model: &self.request_model,
                input: inputs,
            });
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key.expose_secret());
        }

        let response = request
            .send()
            .await
            .map_err(|error| EmbedError::Unavailable(error.to_string()))?;

        let status = response.status();
        if status.is_server_error() {
            return Err(EmbedError::Unavailable(format!("HTTP status {status}")));
        }
        if !status.is_success() {
            return Err(EmbedError::Status {
                status: status.as_u16(),
            });
        }

        // Read the body as bytes first: a failure here is a transport read (a severed or
        // truncated stream after the 200), which is an availability problem, not a
        // malformed response. Only a complete-but-unparseable body is a hard decode error.
        let bytes = response
            .bytes()
            .await
            .map_err(|error| EmbedError::Unavailable(error.to_string()))?;
        let body: EmbeddingResponse = serde_json::from_slice(&bytes)
            .map_err(|error| EmbedError::Decode(error.to_string()))?;
        self.place_in_order(body, inputs.len())
    }

    /// Validate, normalize, and order the response data by input index.
    fn place_in_order(
        &self,
        body: EmbeddingResponse,
        expected: usize,
    ) -> Result<Vec<Embedding>, EmbedError> {
        if body.data.len() != expected {
            return Err(EmbedError::WrongCount {
                expected,
                actual: body.data.len(),
            });
        }
        let mut slots: Vec<Option<Embedding>> = (0..expected).map(|_| None).collect();
        for datum in body.data {
            let dimension = datum.embedding.len();
            if dimension as u32 != self.identity.dimension {
                return Err(EmbedError::DimensionMismatch {
                    expected: self.identity.dimension,
                    actual: dimension,
                });
            }
            let slot = slots
                .get_mut(datum.index)
                .ok_or_else(|| EmbedError::Decode(format!("index {} out of range", datum.index)))?;
            if slot
                .replace(Embedding::new(datum.embedding)?.normalized())
                .is_some()
            {
                return Err(EmbedError::Decode(format!(
                    "duplicate index {}",
                    datum.index
                )));
            }
        }
        slots
            .into_iter()
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| EmbedError::Decode("the response skipped an input index".to_owned()))
    }
}

impl Embedder for HttpEmbedder {
    type Error = EmbedError;

    async fn embed(&self, inputs: &[String]) -> Result<Vec<Embedding>, Self::Error> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        self.embed_batch(inputs).await
    }

    fn model(&self) -> &EmbedderModel {
        &self.identity
    }
}
