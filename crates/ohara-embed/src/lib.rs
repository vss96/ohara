//! fastembed-rs implementations of `ohara_core::EmbeddingProvider`
//! and `ohara_core::RerankProvider`.

pub mod fastembed;
pub use fastembed::{FastEmbedProvider, FastEmbedReranker};
