//! Cross-commit embedding coalescer (plan-31).
//!
//! `CoreMlFixedProvider` embeds in fixed 32-row batches and pads partial
//! batches with empty strings. The indexer calls `embed_batch`
//! **per commit**, and most commits have only a handful of rows
//! (message + a few hunks), so per-commit batching wastes ~55% of the
//! GPU on padding (measured on questdb — see
//! `docs/perf/v0.11-coreml-fixed-shape.md`).
//!
//! [`BatchingEmbedder`] is an `EmbeddingProvider` decorator that sits in
//! front of the real embedder and coalesces rows **across** concurrent
//! `embed_batch` callers into full batches. Workers call `embed_batch`
//! exactly as before and get back exactly their own rows, in order; the
//! physical ONNX batch is filled from whatever callers are in flight.
//! Under the parallel indexer's ~`num_cpus` concurrent workers the
//! batches stay full, so the padding waste collapses.
//!
//! Mechanism: a single coalescer task owns the inner embedder and a
//! work queue. It dispatches full `batch_rows` batches greedily and
//! flushes a partial batch only when the producers momentarily go idle
//! (so the tail never hangs). Embeddings are independent per row — no
//! cross-row attention — so grouping callers together is semantically
//! transparent.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use ohara_core::{EmbeddingProvider, OhraError, Result as CoreResult};
use tokio::sync::{mpsc, oneshot};

/// One caller's `embed_batch` request: its input rows and a channel to
/// receive the aligned output on.
struct Request {
    texts: Vec<String>,
    reply: oneshot::Sender<CoreResult<Vec<Vec<f32>>>>,
}

/// In-flight state for a request whose rows are spread across the queue.
struct ReqState {
    reply: oneshot::Sender<CoreResult<Vec<Vec<f32>>>>,
    slots: Vec<Option<Vec<f32>>>,
    remaining: usize,
}

/// `EmbeddingProvider` decorator that batches rows across concurrent
/// callers. `dimension()` / `model_id()` mirror the inner embedder.
pub struct BatchingEmbedder {
    tx: mpsc::Sender<Request>,
    dim: usize,
    model_id: String,
}

impl BatchingEmbedder {
    /// Wrap `inner`, coalescing into batches of `batch_rows`. Spawns the
    /// coalescer task on the current tokio runtime; it runs until this
    /// `BatchingEmbedder` (and all clones of its sender) drop.
    pub fn new(inner: Arc<dyn EmbeddingProvider>, batch_rows: usize) -> Self {
        let dim = inner.dimension();
        let model_id = inner.model_id().to_owned();
        let batch_rows = batch_rows.max(1);
        // A small bound is enough: callers block on send only when the
        // coalescer is mid-dispatch, which is exactly when we want
        // backpressure.
        let (tx, rx) = mpsc::channel::<Request>(256);
        tokio::spawn(coalescer(rx, inner, batch_rows));
        Self { tx, dim, model_id }
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for BatchingEmbedder {
    fn dimension(&self) -> usize {
        self.dim
    }
    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Request {
                texts: texts.to_vec(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| OhraError::Embedding("batching embedder task stopped".into()))?;
        reply_rx
            .await
            .map_err(|_| OhraError::Embedding("batching embedder dropped reply".into()))?
    }
}

/// The single coalescer task. Pulls requests, fills full `batch_rows`
/// batches greedily, and flushes a partial batch when the producers go
/// idle so a small tail never stalls.
async fn coalescer(
    mut rx: mpsc::Receiver<Request>,
    inner: Arc<dyn EmbeddingProvider>,
    batch_rows: usize,
) {
    // (req_id, idx_in_req, text) for every row not yet embedded.
    let mut pending: VecDeque<(u64, usize, String)> = VecDeque::new();
    let mut reqs: HashMap<u64, ReqState> = HashMap::new();
    let mut next_id: u64 = 0;

    loop {
        // Dispatch every full batch we can before touching the channel.
        while pending.len() >= batch_rows {
            dispatch(&inner, &mut pending, &mut reqs, batch_rows).await;
        }

        if pending.is_empty() {
            // Nothing buffered — block for the next request, or exit when
            // every sender has dropped.
            match rx.recv().await {
                Some(req) => enqueue(req, &mut pending, &mut reqs, &mut next_id),
                None => break,
            }
            continue;
        }

        // A partial batch is buffered. Pull more without blocking to try
        // to fill it; flush it if the producers are momentarily idle.
        match rx.try_recv() {
            Ok(req) => enqueue(req, &mut pending, &mut reqs, &mut next_id),
            Err(mpsc::error::TryRecvError::Empty) => {
                let n = pending.len();
                dispatch(&inner, &mut pending, &mut reqs, n).await;
            }
            Err(mpsc::error::TryRecvError::Disconnected) => {
                let n = pending.len();
                dispatch(&inner, &mut pending, &mut reqs, n).await;
                break;
            }
        }
    }
}

fn enqueue(
    req: Request,
    pending: &mut VecDeque<(u64, usize, String)>,
    reqs: &mut HashMap<u64, ReqState>,
    next_id: &mut u64,
) {
    let id = *next_id;
    *next_id += 1;
    let n = req.texts.len();
    for (idx, text) in req.texts.into_iter().enumerate() {
        pending.push_back((id, idx, text));
    }
    reqs.insert(
        id,
        ReqState {
            reply: req.reply,
            slots: (0..n).map(|_| None).collect(),
            remaining: n,
        },
    );
}

/// Embed the front `k` rows in one inner call and scatter the results
/// back to their owning requests, completing any whose rows are all in.
async fn dispatch(
    inner: &Arc<dyn EmbeddingProvider>,
    pending: &mut VecDeque<(u64, usize, String)>,
    reqs: &mut HashMap<u64, ReqState>,
    k: usize,
) {
    let k = k.min(pending.len());
    if k == 0 {
        return;
    }
    let mut owners: Vec<(u64, usize)> = Vec::with_capacity(k);
    let mut texts: Vec<String> = Vec::with_capacity(k);
    for _ in 0..k {
        let (id, idx, text) = pending.pop_front().expect("k <= pending.len()");
        owners.push((id, idx));
        texts.push(text);
    }

    match inner.embed_batch(&texts).await {
        Ok(vectors) => {
            for ((id, idx), vector) in owners.into_iter().zip(vectors) {
                let Some(state) = reqs.get_mut(&id) else {
                    continue;
                };
                if state.slots[idx].is_none() {
                    state.slots[idx] = Some(vector);
                    state.remaining -= 1;
                }
                if state.remaining == 0 {
                    complete(reqs, id);
                }
            }
        }
        Err(e) => {
            // Fail every request that had a row in this batch (dedup so
            // each one-shot is used once).
            let mut seen: HashSet<u64> = HashSet::new();
            for (id, _) in owners {
                if !seen.insert(id) {
                    continue;
                }
                if let Some(state) = reqs.remove(&id) {
                    let _ = state.reply.send(Err(OhraError::Embedding(e.to_string())));
                }
            }
        }
    }
}

/// Assemble a completed request's vectors in input order and reply.
fn complete(reqs: &mut HashMap<u64, ReqState>, id: u64) {
    let Some(state) = reqs.remove(&id) else {
        return;
    };
    let vectors: Vec<Vec<f32>> = state
        .slots
        .into_iter()
        .map(|slot| slot.expect("remaining==0 means every slot is filled"))
        .collect();
    let _ = state.reply.send(Ok(vectors));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Deterministic inner embedder: each input string `"<n>"` maps to
    /// the 1-d vector `[n]`, so callers can assert they got exactly
    /// their own rows back regardless of how batches were grouped. Also
    /// records the size of every batch the coalescer dispatched.
    struct StubEmbedder {
        batch_sizes: Arc<Mutex<Vec<usize>>>,
    }

    #[async_trait::async_trait]
    impl EmbeddingProvider for StubEmbedder {
        fn dimension(&self) -> usize {
            1
        }
        fn model_id(&self) -> &str {
            "stub"
        }
        async fn embed_batch(&self, texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
            self.batch_sizes.lock().unwrap().push(texts.len());
            Ok(texts
                .iter()
                .map(|t| vec![t.parse::<f32>().expect("stub inputs are numeric")])
                .collect())
        }
    }

    /// Inner embedder that always errors.
    struct FailingEmbedder;
    #[async_trait::async_trait]
    impl EmbeddingProvider for FailingEmbedder {
        fn dimension(&self) -> usize {
            1
        }
        fn model_id(&self) -> &str {
            "fail"
        }
        async fn embed_batch(&self, _texts: &[String]) -> CoreResult<Vec<Vec<f32>>> {
            Err(OhraError::Embedding("boom".into()))
        }
    }

    fn nums(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|i| i.to_string()).collect()
    }

    #[tokio::test]
    async fn delegates_dimension_and_model_id() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let be = BatchingEmbedder::new(Arc::new(StubEmbedder { batch_sizes: sizes }), 32);
        assert_eq!(be.dimension(), 1);
        assert_eq!(be.model_id(), "stub");
    }

    #[tokio::test]
    async fn empty_input_returns_empty_without_dispatch() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let be = BatchingEmbedder::new(
            Arc::new(StubEmbedder {
                batch_sizes: sizes.clone(),
            }),
            32,
        );
        let out = be.embed_batch(&[]).await.unwrap();
        assert!(out.is_empty());
        assert!(
            sizes.lock().unwrap().is_empty(),
            "no inner call for empty input"
        );
    }

    #[tokio::test]
    async fn single_small_call_returns_its_own_rows() {
        // Flush-on-drain: a lone 2-row call must come back without hanging.
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let be = BatchingEmbedder::new(Arc::new(StubEmbedder { batch_sizes: sizes }), 32);
        let out = be.embed_batch(&nums(5..7)).await.unwrap();
        assert_eq!(out, vec![vec![5.0], vec![6.0]]);
    }

    #[tokio::test]
    async fn large_call_is_split_and_reassembled_in_order() {
        // 70 rows with batch_rows=32 → split across dispatches, but the
        // caller still gets all 70 back in input order.
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let be = BatchingEmbedder::new(
            Arc::new(StubEmbedder {
                batch_sizes: sizes.clone(),
            }),
            32,
        );
        let out = be.embed_batch(&nums(0..70)).await.unwrap();
        let got: Vec<f32> = out.into_iter().map(|v| v[0]).collect();
        assert_eq!(got, (0..70).map(|i| i as f32).collect::<Vec<_>>());
        // 70 = 32 + 32 + 6 (the 6 flushes once producers go idle).
        assert_eq!(*sizes.lock().unwrap(), vec![32, 32, 6]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_callers_each_get_their_own_rows() {
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let be = Arc::new(BatchingEmbedder::new(
            Arc::new(StubEmbedder { batch_sizes: sizes }),
            32,
        ));
        // 20 callers each submit a distinct 6-row block.
        let mut handles = Vec::new();
        for c in 0..20usize {
            let be = be.clone();
            let block: Vec<String> = (0..6).map(|j| (c * 100 + j).to_string()).collect();
            handles.push(tokio::spawn(async move {
                let out = be.embed_batch(&block).await.unwrap();
                let got: Vec<f32> = out.into_iter().map(|v| v[0]).collect();
                let want: Vec<f32> = (0..6).map(|j| (c * 100 + j) as f32).collect();
                assert_eq!(got, want, "caller {c} got the wrong rows");
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fills_full_batches_under_load() {
        // The padding-elimination property: when many callers are in
        // flight, the inner embedder sees full batch_rows batches, not
        // tiny per-caller ones.
        let sizes = Arc::new(Mutex::new(Vec::new()));
        let be = Arc::new(BatchingEmbedder::new(
            Arc::new(StubEmbedder {
                batch_sizes: sizes.clone(),
            }),
            8,
        ));
        let mut handles = Vec::new();
        for c in 0..64usize {
            let be = be.clone();
            handles.push(tokio::spawn(async move {
                be.embed_batch(&[(c).to_string()]).await.unwrap();
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let recorded = sizes.lock().unwrap().clone();
        let total: usize = recorded.iter().sum();
        assert_eq!(total, 64, "every row must be embedded exactly once");
        let full = recorded.iter().filter(|&&s| s == 8).count();
        assert!(
            full >= 1,
            "under 64 concurrent single-row callers the coalescer must fill at \
             least one full 8-row batch; got {recorded:?}"
        );
    }

    #[tokio::test]
    async fn inner_error_propagates_to_caller() {
        let be = BatchingEmbedder::new(Arc::new(FailingEmbedder), 32);
        let err = be.embed_batch(&nums(0..3)).await.unwrap_err();
        assert!(err.to_string().contains("boom"), "got {err}");
    }
}
