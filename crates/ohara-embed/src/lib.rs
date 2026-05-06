//! fastembed-rs implementations of `ohara_core::EmbeddingProvider`
//! and `ohara_core::RerankProvider`.

pub mod fastembed;
pub use fastembed::{
    EmbedProvider, FastEmbedProvider, FastEmbedReranker, LazyFastEmbedReranker, DEFAULT_DIM,
    DEFAULT_MODEL_ID, DEFAULT_RERANKER_ID,
};
