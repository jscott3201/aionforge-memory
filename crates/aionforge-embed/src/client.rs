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
    /// When set, the model returns vectors at this native dimension and each is truncated to
    /// `identity.dimension` (Matryoshka) before normalization. `None` means an exact-dimension
    /// response is required.
    native_dimension: Option<u32>,
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
            native_dimension: None,
        })
    }

    /// Enable Matryoshka truncation: the model returns `native`-dimension vectors and each is
    /// truncated to the recorded identity dimension (first-N components) and renormalized.
    ///
    /// # Errors
    /// Returns [`EmbedError::Config`] unless `native` is strictly greater than the identity
    /// dimension — truncation only reduces.
    pub fn with_native_dimension(mut self, native: u32) -> Result<Self, EmbedError> {
        if native <= self.identity.dimension {
            return Err(EmbedError::Config(format!(
                "native dimension {native} must be greater than the output dimension {}",
                self.identity.dimension
            )));
        }
        self.native_dimension = Some(native);
        Ok(self)
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
        let embedder = Self::new(
            &config.endpoint,
            config.model.clone(),
            identity,
            api_key,
            Duration::from_millis(config.timeout_ms),
        )?;
        match config.native_dimension {
            Some(native) => embedder.with_native_dimension(native),
            None => Ok(embedder),
        }
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
        let output = self.identity.dimension as usize;
        for datum in body.data {
            let returned = datum.embedding.len();
            // Accept an exact-dimension response, or a native-dimension response that we
            // truncate to the output dimension (Matryoshka). Any other size is a real
            // mismatch — the dimension hard-check still guards genuine bugs.
            let components = if returned == output {
                datum.embedding
            } else if self.native_dimension == Some(returned as u32) && returned > output {
                datum.embedding[..output].to_vec()
            } else {
                return Err(EmbedError::DimensionMismatch {
                    expected: self.identity.dimension,
                    actual: returned,
                });
            };
            let slot = slots
                .get_mut(datum.index)
                .ok_or_else(|| EmbedError::Decode(format!("index {} out of range", datum.index)))?;
            if slot
                .replace(Embedding::new(components)?.normalized())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{EmbeddingDatum, EmbeddingResponse};

    fn embedder(output_dim: u32) -> HttpEmbedder {
        HttpEmbedder::new(
            "http://localhost:1234/v1",
            "m",
            EmbedderModel {
                family: "m".to_owned(),
                version: String::new(),
                dimension: output_dim,
            },
            None,
            Duration::from_millis(1_000),
        )
        .expect("build embedder")
    }

    fn one(embedding: Vec<f32>) -> EmbeddingResponse {
        EmbeddingResponse {
            data: vec![EmbeddingDatum {
                embedding,
                index: 0,
            }],
        }
    }

    #[test]
    fn matryoshka_truncates_a_native_vector_to_the_output_dimension() {
        // Output 2, native 4: the first two components [3,4] are kept and renormalized to
        // unit length [0.6, 0.8] — proving truncation takes the leading (not trailing) dims.
        let embedder = embedder(2).with_native_dimension(4).expect("native");
        let out = embedder
            .place_in_order(one(vec![3.0, 4.0, 99.0, 99.0]), 1)
            .expect("place");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].dimension(), 2);
        assert!((out[0].as_slice()[0] - 0.6).abs() < 1e-6);
        assert!((out[0].as_slice()[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn an_exact_dimension_response_is_accepted() {
        let out = embedder(3)
            .place_in_order(one(vec![0.0, 3.0, 4.0]), 1)
            .expect("place");
        assert_eq!(out[0].dimension(), 3);
    }

    #[test]
    fn a_wrong_dimension_without_native_is_rejected() {
        let err = embedder(2)
            .place_in_order(one(vec![1.0, 2.0, 3.0, 4.0]), 1)
            .expect_err("mismatch");
        assert!(matches!(
            err,
            EmbedError::DimensionMismatch {
                expected: 2,
                actual: 4
            }
        ));
    }

    #[test]
    fn an_oversize_response_that_is_not_the_native_size_is_still_rejected() {
        // native is 4, but the response is 5 — not the declared native size, so it is a real
        // mismatch, not a truncation candidate.
        let err = embedder(2)
            .with_native_dimension(4)
            .expect("native")
            .place_in_order(one(vec![1.0; 5]), 1)
            .expect_err("mismatch");
        assert!(matches!(
            err,
            EmbedError::DimensionMismatch {
                expected: 2,
                actual: 5
            }
        ));
    }

    #[test]
    fn with_native_dimension_rejects_a_native_not_above_the_output() {
        let err = embedder(4).with_native_dimension(4).expect_err("config");
        assert!(matches!(err, EmbedError::Config(_)));
    }
}
