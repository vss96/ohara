//! fastembed-rs implementations of `ohara_core::EmbeddingProvider`
//! and `ohara_core::RerankProvider`.

pub mod batching;
pub mod coreml_fixed;
pub mod fastembed;
pub(crate) mod idle_slot;
pub mod onnx_dims;
pub use batching::BatchingEmbedder;
pub use coreml_fixed::CoreMlFixedProvider;
pub use fastembed::{
    EmbedProvider, FastEmbedProvider, FastEmbedReranker, LazyFastEmbedProvider,
    LazyFastEmbedReranker, DEFAULT_DIM, DEFAULT_MODEL_ID, DEFAULT_RERANKER_ID,
};
