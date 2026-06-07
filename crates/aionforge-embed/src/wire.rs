//! The OpenAI-compatible embeddings request and response shapes.

use serde::{Deserialize, Serialize};

/// `POST {base}/embeddings` request body.
#[derive(Serialize)]
pub(crate) struct EmbeddingRequest<'a> {
    /// The provider's model id.
    pub model: &'a str,
    /// The batch of inputs, in order.
    pub input: &'a [String],
}

/// The embeddings response body. Fields beyond `data` (model, usage) are ignored.
#[derive(Deserialize)]
pub(crate) struct EmbeddingResponse {
    /// One datum per input, each carrying its input index.
    pub data: Vec<EmbeddingDatum>,
}

/// One embedding in the response, tagged with the index of the input it embeds.
#[derive(Deserialize)]
pub(crate) struct EmbeddingDatum {
    /// The raw vector.
    pub embedding: Vec<f32>,
    /// The index of the corresponding input, used to restore input order.
    pub index: usize,
}
