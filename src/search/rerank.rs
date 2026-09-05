//! Cross-encoder semantic reranking for DonSeek.
//!
//! Uses `Xenova/ms-marco-MiniLM-L-6-v2` (22.7M params, 23MB quantized ONNX)
//! to re-score search results by semantic relevance. The cross-encoder
//! reads the query and each document **together** through full attention,
//! capturing relationships that RRF (rank-based) and BM25 (keyword-based)
//! fundamentally cannot : "fast web scraper" matches "high-speed crawler"
//! even with zero word overlap.
//!
//! Same lazy-cache pattern as OCR: model files are downloaded + sha256-
//! verified on first use, then loaded from disk on subsequent calls. If
//! the model is unavailable (download failed, feature disabled, offline),
//! reranking is skipped gracefully : results fall back to RRF+BM25 ranking.
//!
//! The 23MB model runs in ~5ms/pair on CPU. For 50 results: ~250ms,
//! negligible on top of 1-3s multi-engine search time.

#[cfg(feature = "rerank")]
mod inner {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};

    use ndarray::Array2;
    use ort::session::Session;
    use ort::value::TensorRef;
    use sha2::{Digest, Sha256};
    use tokenizers::Tokenizer;

    /// Blend weight: 0.6 = 60% RRF+BM25, 40% cross-encoder.
    /// RRF consensus across 10+ engines is a strong signal; the
    /// cross-encoder is additive, not a replacement.
    const BLEND_ALPHA: f64 = 0.6;

    /// Max sequence length for the MiniLM cross-encoder.
    const MAX_SEQ_LEN: usize = 512;

    /// ONNX Runtime accepts the thread count as a C `int`.
    const MAX_INTRA_THREADS: usize = i32::MAX as usize;

    const MODEL_URL: &str = "https://huggingface.co/Xenova/ms-marco-MiniLM-L-6-v2/resolve/main/onnx/model_quantized.onnx";
    const MODEL_SHA256: &str = "e9d8ebf845c413e981c175bfe49a3bfa9b3dcce2a3ba54875ee5df5a58639fbe";
    const TOKENIZER_URL: &str =
        "https://huggingface.co/Xenova/ms-marco-MiniLM-L-6-v2/resolve/main/tokenizer.json";
    const TOKENIZER_SHA256: &str =
        "d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66";

    struct Reranker {
        session: Mutex<Session>,
        tokenizer: Tokenizer,
    }

    static RERANKER: OnceLock<Option<Reranker>> = OnceLock::new();

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum ThreadSelection {
        Environment {
            threads: usize,
        },
        Automatic {
            threads: usize,
            effective: usize,
            physical: usize,
        },
        OnnxDefault {
            effective: Option<usize>,
            physical: usize,
        },
    }

    impl ThreadSelection {
        fn threads(self) -> Option<usize> {
            match self {
                Self::Environment { threads } | Self::Automatic { threads, .. } => Some(threads),
                Self::OnnxDefault { .. } => None,
            }
        }
    }

    enum ThreadOverride<'a> {
        Unset,
        Value(&'a str),
        InvalidUnicode,
    }

    fn parse_intra_threads(raw: &str) -> Result<usize, String> {
        let raw = raw.trim();
        let threads = raw.parse::<usize>().map_err(|_| {
            format!("DONSEEK_RERANK_THREADS must be an integer between 1 and {MAX_INTRA_THREADS}")
        })?;
        if !(1..=MAX_INTRA_THREADS).contains(&threads) {
            return Err(format!(
                "DONSEEK_RERANK_THREADS must be an integer between 1 and {MAX_INTRA_THREADS}"
            ));
        }
        Ok(threads)
    }

    fn automatic_intra_threads(effective: Option<usize>, physical: usize) -> ThreadSelection {
        match effective.filter(|&t| t > 0 && t < physical) {
            Some(threads) => ThreadSelection::Automatic {
                threads,
                effective: threads,
                physical,
            },
            None => ThreadSelection::OnnxDefault {
                effective,
                physical,
            },
        }
    }

    fn select_intra_threads(
        override_value: ThreadOverride<'_>,
        effective: Option<usize>,
        physical: usize,
    ) -> (ThreadSelection, Option<String>) {
        match override_value {
            ThreadOverride::Value(raw) => match parse_intra_threads(raw) {
                Ok(threads) => (ThreadSelection::Environment { threads }, None),
                Err(e) => (
                    automatic_intra_threads(effective, physical),
                    Some(format!("{e}; using automatic selection")),
                ),
            },
            ThreadOverride::InvalidUnicode => (
                automatic_intra_threads(effective, physical),
                Some(
                    "DONSEEK_RERANK_THREADS must contain valid UTF-8; using automatic selection"
                        .to_string(),
                ),
            ),
            ThreadOverride::Unset => (automatic_intra_threads(effective, physical), None),
        }
    }

    fn configured_intra_threads() -> ThreadSelection {
        let effective = std::thread::available_parallelism().ok().map(|n| n.get());
        let physical = num_cpus::get_physical();
        let raw = std::env::var("DONSEEK_RERANK_THREADS");
        let override_value = match raw.as_deref() {
            Ok(raw) => ThreadOverride::Value(raw),
            Err(std::env::VarError::NotPresent) => ThreadOverride::Unset,
            Err(std::env::VarError::NotUnicode(_)) => ThreadOverride::InvalidUnicode,
        };
        let (selection, warning) = select_intra_threads(override_value, effective, physical);
        if let Some(warning) = warning {
            eprintln!("[rerank] invalid configuration: {warning}");
        }
        selection
    }

    fn log_thread_selection(selection: ThreadSelection) {
        match selection {
            ThreadSelection::Environment { threads } => eprintln!(
                "[rerank] ONNX intra-op threads: {} (source: DONSEEK_RERANK_THREADS)",
                threads
            ),
            ThreadSelection::Automatic {
                threads,
                effective,
                physical,
            } => eprintln!(
                "[rerank] ONNX intra-op threads: {} (source: automatic, effective: {}, physical: {})",
                threads, effective, physical
            ),
            ThreadSelection::OnnxDefault {
                effective,
                physical,
            } => match effective {
                Some(effective) => eprintln!(
                    "[rerank] ONNX intra-op threads: default (effective: {effective}, physical: {})",
                    physical
                ),
                None => eprintln!(
                    "[rerank] ONNX intra-op threads: default (effective parallelism unavailable, physical: {})",
                    physical
                ),
            },
        }
    }

    /// Returns the cache directory for reranker model files.
    fn cache_dir() -> PathBuf {
        let mut p = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("."));
        p.push("donsetch");
        p.push("rerank");
        let _ = std::fs::create_dir_all(&p);
        p
    }

    /// Downloads a file via reqwest blocking, verifying sha256.
    fn download(url: &str, dest: &Path, expected: &str) -> Result<(), String> {
        fn inner(url: &str, dest: &Path, expected: &str) -> Result<(), String> {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .map_err(|e| format!("client build: {e}"))?;
            let resp = client
                .get(url)
                .send()
                .map_err(|e| format!("download {url}: {e}"))?
                .error_for_status()
                .map_err(|e| format!("download {url}: {e}"))?;
            let body = resp.bytes().map_err(|e| format!("read {url}: {e}"))?;
            let got = Sha256::digest(&body)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            if got != expected {
                return Err(format!(
                    "sha256 mismatch for {url}: expected {expected}, got {got}"
                ));
            }
            std::fs::write(dest, &body).map_err(|e| format!("write {dest:?}: {e}"))?;
            Ok(())
        }

        // Dedicated plain thread: `reqwest::blocking` panics when used on
        // a tokio runtime thread : and `panic = "abort"` turns that into a
        // process abort : and first-use downloads are triggered from the
        // async search path.
        let url = url.to_string();
        let dest = dest.to_path_buf();
        let expected = expected.to_string();
        std::thread::Builder::new()
            .name("rerank-download".into())
            .spawn(move || inner(&url, &dest, &expected))
            .map_err(|e| format!("download thread spawn: {e}"))?
            .join()
            .map_err(|_| "download thread panicked".to_string())?
    }

    /// Ensures model + tokenizer files are on disk, returns their paths.
    fn ensure_files() -> Result<(PathBuf, PathBuf), String> {
        let dir = cache_dir();
        let model_path = dir.join("model_quantized.onnx");
        let tok_path = dir.join("tokenizer.json");

        if !model_path.exists() {
            eprintln!("[rerank] downloading model (23MB, first use only)...");
            download(MODEL_URL, &model_path, MODEL_SHA256)?;
            eprintln!("[rerank] model cached.");
        }
        if !tok_path.exists() {
            eprintln!("[rerank] downloading tokenizer (695KB)...");
            download(TOKENIZER_URL, &tok_path, TOKENIZER_SHA256)?;
            eprintln!("[rerank] tokenizer cached.");
        }
        Ok((model_path, tok_path))
    }

    /// Initializes the reranker on first call. Returns `None` on any
    /// failure : caller skips reranking gracefully.
    ///
    /// ONNX Runtime init runs in a separate thread with a 30s timeout.
    /// ONNX's C++ global constructors can deadlock on some platforms
    /// (see pykeio/ort#579); the timeout prevents an infinite hang.
    /// If it fires, reranking is disabled and results fall back to
    /// RRF+BM25.
    fn init() -> Option<Reranker> {
        // Gate: ensure ONNX Runtime is loaded (AVX check + dlopen).
        // If the CPU lacks AVX or the .so is missing, return None;
        // reranking falls back to RRF+BM25.
        if let Err(e) = crate::onnx::ensure_loaded() {
            eprintln!("[rerank] {e}");
            return None;
        }

        let (model_path, tok_path) = match ensure_files() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[rerank] init failed: {e}");
                return None;
            }
        };

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(init_inner(model_path, tok_path));
        });

        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(result) => result,
            Err(_) => {
                eprintln!(
                    "[rerank] ONNX Runtime init timed out (30s) : \
                     reranking disabled, falling back to RRF+BM25"
                );
                None
            }
        }
    }

    fn init_inner(model_path: PathBuf, tok_path: PathBuf) -> Option<Reranker> {
        use tokenizers::{
            PaddingDirection, PaddingParams, PaddingStrategy, TruncationParams, TruncationStrategy,
        };

        let mut tokenizer = match Tokenizer::from_file(&tok_path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[rerank] tokenizer load failed: {e}");
                return None;
            }
        };

        let _ = tokenizer.with_truncation(Some(TruncationParams {
            max_length: MAX_SEQ_LEN,
            strategy: TruncationStrategy::LongestFirst,
            ..Default::default()
        }));
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Right,
            pad_id: 0,
            pad_type_id: 0,
            pad_token: "[PAD]".to_string(),
            ..Default::default()
        }));

        let session = match Session::builder() {
            Ok(mut b) => {
                let selection = configured_intra_threads();
                if let Some(threads) = selection.threads() {
                    b = match b.with_intra_threads(threads) {
                        Ok(b) => {
                            log_thread_selection(selection);
                            b
                        }
                        Err(e) => {
                            eprintln!(
                                "[rerank] failed to set ONNX thread limit: {e}; using ONNX default"
                            );
                            e.recover()
                        }
                    };
                } else {
                    log_thread_selection(selection);
                }
                match b.commit_from_file(&model_path) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!("[rerank] session load failed: {e}");
                        return None;
                    }
                }
            }
            Err(e) => {
                eprintln!("[rerank] session builder failed: {e}");
                return None;
            }
        };

        Some(Reranker {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    /// Returns the initialized reranker, or `None` if unavailable.
    /// First call downloads + loads the model; subsequent calls are free.
    fn get() -> &'static Option<Reranker> {
        RERANKER.get_or_init(init)
    }

    /// Sigmoid function: maps logits to [0, 1].
    fn sigmoid(x: f32) -> f64 {
        1.0 / (1.0 + (-x as f64).exp())
    }

    /// Returns true if the reranker model + tokenizer are already
    /// on disk. Does NOT trigger a download : used by focus
    /// extraction to decide whether semantic scoring is available
    /// without surprising the user with a model download during
    /// a plain fetch.
    /// True when the cross-encoder is loaded and was used for the
    /// last ranking pass in this process (feature on + model ok).
    pub fn active() -> bool {
        get().is_some()
    }

    pub fn is_model_cached() -> bool {
        let dir = cache_dir();
        dir.join("model_quantized.onnx").exists() && dir.join("tokenizer.json").exists()
    }

    /// Runs the cross-encoder on (query, doc) pairs, returning sigmoid
    /// scores in [0, 1]. Returns `None` if the model is unavailable.
    pub fn cross_encoder_scores(query: &str, docs: &[(String, String)]) -> Option<Vec<f64>> {
        if docs.is_empty() || query.trim().is_empty() {
            return None;
        }

        let reranker = get().as_ref()?;
        let tokenizer = &reranker.tokenizer;

        // Build (query, "title snippet") pairs for batch encoding.
        // The tokenizer's Dual mode produces [CLS] query [SEP] doc [SEP]
        // automatically : we just pass (query, doc) tuples.
        let pairs: Vec<(&str, String)> = docs
            .iter()
            .map(|(title, snippet)| {
                if snippet.is_empty() {
                    (query, title.clone())
                } else {
                    (query, format!("{title} {snippet}"))
                }
            })
            .collect();
        let pairs_ref: Vec<(&str, &str)> = pairs.iter().map(|(q, d)| (*q, d.as_str())).collect();

        // Encode all pairs in one batch with automatic padding.
        let encodings = match tokenizer.encode_batch(pairs_ref, true) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[rerank] tokenization failed: {e}");
                return None;
            }
        };

        let batch = encodings.len();
        let seq_len = encodings
            .iter()
            .map(|e| e.get_ids().len())
            .max()
            .unwrap_or(0);
        if seq_len == 0 || batch == 0 {
            return None;
        }

        // Flatten token IDs, attention masks, and type IDs into 2D arrays.
        let mut ids_flat = Vec::with_capacity(batch * seq_len);
        let mut mask_flat = Vec::with_capacity(batch * seq_len);
        let mut type_flat = Vec::with_capacity(batch * seq_len);

        for enc in &encodings {
            let ids = enc.get_ids();
            let mask = enc.get_attention_mask();
            let types = enc.get_type_ids();
            let len = ids.len();
            ids_flat.extend(ids.iter().map(|&v| v as i64));
            mask_flat.extend(mask.iter().map(|&v| v as i64));
            type_flat.extend(types.iter().map(|&v| v as i64));
            // Pad to seq_len (shouldn't happen with BatchLongest, but safe).
            if len < seq_len {
                for _ in len..seq_len {
                    ids_flat.push(0);
                    mask_flat.push(0);
                    type_flat.push(0);
                }
            }
        }

        let input_ids = Array2::from_shape_vec((batch, seq_len), ids_flat).ok()?;
        let attention_mask = Array2::from_shape_vec((batch, seq_len), mask_flat).ok()?;
        let token_type_ids = Array2::from_shape_vec((batch, seq_len), type_flat).ok()?;

        // Run ONNX inference.
        let mut session = reranker.session.lock().ok()?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => TensorRef::from_array_view(&input_ids).ok()?,
                "attention_mask" => TensorRef::from_array_view(&attention_mask).ok()?,
                "token_type_ids" => TensorRef::from_array_view(&token_type_ids).ok()?,
            ])
            .ok()?;

        // Extract logits: shape [batch, 1] via try_extract_array.
        let logits = outputs["logits"].try_extract_array::<f32>().ok()?;

        let scores: Vec<f64> = logits.iter().map(|&logit| sigmoid(logit)).collect();

        Some(scores)
    }

    /// Min-max normalize a slice to [0, 1]. If all values are equal,
    /// returns 0.5 for each (neutral : doesn't perturb the blend).
    fn min_max_normalize(values: &[f64]) -> Vec<f64> {
        let min = values.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let range = max - min;
        if range < 1e-12 {
            return vec![0.5; values.len()];
        }
        values.iter().map(|v| (v - min) / range).collect()
    }

    /// Re-scores results by blending RRF+BM25 scores with cross-encoder
    /// semantic scores. Adjusts `.score` in-place. If the model is
    /// unavailable, scores are left unchanged.
    pub fn rerank(query: &str, results: &mut [crate::search::rank::Merged]) {
        if results.len() < 2 || query.trim().is_empty() {
            return;
        }

        let docs: Vec<(String, String)> = results
            .iter()
            .map(|r| (r.title.clone(), r.snippet.clone()))
            .collect();

        let xenc = match cross_encoder_scores(query, &docs) {
            Some(s) => s,
            None => return,
        };

        if xenc.len() != results.len() {
            return;
        }

        // Collect current RRF+BM25+prior scores.
        let rrf: Vec<f64> = results.iter().map(|r| r.score).collect();

        // Min-max normalize both to [0, 1].
        let rrf_norm = min_max_normalize(&rrf);
        let xenc_norm = min_max_normalize(&xenc);

        // Blend: final = α * rrf + (1-α) * xenc
        for (r, (rn, xn)) in results
            .iter_mut()
            .zip(rrf_norm.iter().zip(xenc_norm.iter()))
        {
            r.score = BLEND_ALPHA * rn + (1.0 - BLEND_ALPHA) * xn;
        }
    }

    /// Additive cross-encoder top-up, run on post-enrichment text.
    ///
    /// The main blend ranks on SERP fragments (title + snippet from
    /// the result page). Enrichment then swaps in the real page
    /// title / meta description for the top slice, which is strictly
    /// better evidence for semantic relevance. A full re-blend here
    /// would let the enrichment rewrite the whole ordering on fresh
    /// text; an additive nudge on the already-final scores breaks
    /// close ties with page truth without re-litigating the merge.
    pub fn topup(query: &str, results: &mut [crate::search::rank::Merged], depth: usize) {
        const NUDGE: f64 = 0.1;
        if results.is_empty() || depth < 2 || query.trim().is_empty() {
            return;
        }
        let n = depth.min(results.len());
        let docs: Vec<(String, String)> = results[..n]
            .iter()
            .map(|r| (r.title.clone(), r.snippet.clone()))
            .collect();
        let Some(scores) = cross_encoder_scores(query, &docs) else {
            return;
        };
        if scores.len() != n {
            return;
        }
        apply_topup_scores(results, n, &scores, NUDGE);
    }

    /// The score-nudge-and-resort half of `topup`, split out so it's
    /// testable without a real cross-encoder model.
    fn apply_topup_scores(
        results: &mut [crate::search::rank::Merged],
        n: usize,
        scores: &[f64],
        nudge: f64,
    ) {
        for (r, s) in results[..n].iter_mut().zip(scores) {
            // scores are probabilities (0=irrelevant, 1=exact); the
            // 0.5-centered product keeps the nudge zero-sum around
            // the midpoint.
            r.score += nudge * (s - 0.5);
        }
        // Sort the whole slice, not just the nudged prefix: the
        // nudge can push results[n-1] below results[n]'s untouched
        // score (a near-tied pair straddling the depth boundary),
        // and callers taking more than `depth` results (merge()
        // always keeps 12; topup runs at depth=8) would otherwise
        // get a slice that isn't sorted by score at that boundary.
        results.sort_by(|a, b| b.score.total_cmp(&a.score));
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::search::rank::Merged;

        #[test]
        fn sigmoid_basic() {
            assert!((sigmoid(0.0) - 0.5).abs() < 1e-6);
            assert!(sigmoid(5.0) > 0.99);
            assert!(sigmoid(-5.0) < 0.01);
        }

        #[test]
        fn min_max_normalize_basic() {
            let v = vec![1.0, 2.0, 3.0, 4.0, 5.0];
            let n = min_max_normalize(&v);
            assert!((n[0] - 0.0).abs() < 1e-6);
            assert!((n[4] - 1.0).abs() < 1e-6);
            assert!((n[2] - 0.5).abs() < 1e-6);
        }

        #[test]
        fn min_max_normalize_identical() {
            let v = vec![3.0, 3.0, 3.0];
            let n = min_max_normalize(&v);
            for val in &n {
                assert!((val - 0.5).abs() < 1e-6);
            }
        }

        #[test]
        fn min_max_normalize_single() {
            let v = vec![42.0];
            let n = min_max_normalize(&v);
            assert!((n[0] - 0.5).abs() < 1e-6);
        }

        #[test]
        fn min_max_normalize_negative() {
            let v = vec![-10.0, 0.0, 10.0];
            let n = min_max_normalize(&v);
            assert!((n[0] - 0.0).abs() < 1e-6);
            assert!((n[1] - 0.5).abs() < 1e-6);
            assert!((n[2] - 1.0).abs() < 1e-6);
        }

        #[test]
        fn rerank_empty_is_noop() {
            let mut results: Vec<Merged> = vec![];
            rerank("test", &mut results);
            assert!(results.is_empty());
        }

        #[test]
        fn rerank_single_is_noop() {
            let mut results = vec![Merged {
                title: "Test".into(),
                url: "https://example.com".into(),
                snippet: "test snippet".into(),
                sources: vec![],
                score: 1.0,
                published: None,
            }];
            rerank("test", &mut results);
            assert!((results[0].score - 1.0).abs() < 1e-6);
        }

        #[test]
        fn rerank_empty_query_is_noop() {
            let mut results = vec![
                Merged {
                    title: "A".into(),
                    url: "https://a.com".into(),
                    snippet: "a".into(),
                    sources: vec![],
                    score: 0.5,
                    published: None,
                },
                Merged {
                    title: "B".into(),
                    url: "https://b.com".into(),
                    snippet: "b".into(),
                    sources: vec![],
                    score: 0.3,
                    published: None,
                },
            ];
            rerank("   ", &mut results);
            assert!((results[0].score - 0.5).abs() < 1e-6);
            assert!((results[1].score - 0.3).abs() < 1e-6);
        }

        #[test]
        fn blend_preserves_rrf_order_when_xenc_uniform() {
            // When cross-encoder gives all results the same score,
            // min_max returns 0.5 for all, so the blend reduces to
            // a constant offset on the RRF ordering : A still > B.
            let rrf = vec![0.8, 0.4];
            let xenc = vec![0.5, 0.5];
            let rrf_n = min_max_normalize(&rrf);
            let xenc_n = min_max_normalize(&xenc);
            let blend: Vec<f64> = rrf_n
                .iter()
                .zip(xenc_n.iter())
                .map(|(r, x)| BLEND_ALPHA * r + (1.0 - BLEND_ALPHA) * x)
                .collect();
            assert!(
                blend[0] > blend[1],
                "RRF ordering preserved when xenc is uniform"
            );
        }

        #[test]
        fn blend_overrides_rrf_when_xenc_strongly_disagrees() {
            // When the cross-encoder strongly disagrees (semantically
            // relevant doc scored low by RRF), the blend should still
            // reflect both signals. If xenc is strong enough, it can
            // flip the ordering.
            let rrf = vec![0.8, 0.4]; // A > B in RRF
            let xenc = vec![0.1, 0.9]; // B >> A in xenc
            let rrf_n = min_max_normalize(&rrf);
            let xenc_n = min_max_normalize(&xenc);
            let blend: Vec<f64> = rrf_n
                .iter()
                .zip(xenc_n.iter())
                .map(|(r, x)| BLEND_ALPHA * r + (1.0 - BLEND_ALPHA) * x)
                .collect();
            // blend[0] = 0.6*1.0 + 0.4*0.0 = 0.6
            // blend[1] = 0.6*0.0 + 0.4*1.0 = 0.4
            // A still wins (0.6 > 0.4) : 40% weight isn't enough to flip
            assert!(
                blend[0] > blend[1],
                "60/40 blend should NOT flip on moderate disagreement"
            );
            // But the gap narrowed significantly: 0.6-0.4=0.2 vs 1.0-0.0=1.0
            assert!(
                blend[0] - blend[1] < rrf_n[0] - rrf_n[1],
                "xenc should narrow the gap even if it doesn't flip"
            );
        }

        fn merged(url: &str, score: f64) -> Merged {
            Merged {
                title: url.into(),
                url: url.into(),
                snippet: String::new(),
                sources: vec![],
                score,
                published: None,
            }
        }

        // apply_topup_scores used to only re-sort results[..n], leaving
        // results[n..] untouched: a nudge that pushes results[n-1]
        // below results[n]'s original score left the overall vector
        // non-monotonic at that boundary. depth=n=2 here; result[1]
        // ("boundary") starts just above result[2] ("untouched") and
        // gets nudged down past it.
        #[test]
        fn apply_topup_scores_keeps_the_whole_vector_sorted() {
            let mut results = vec![
                merged("top", 1.0),
                merged("boundary", 0.60),
                merged("untouched", 0.55),
                merged("tail", 0.1),
            ];
            apply_topup_scores(&mut results, 2, &[1.0, 0.0], 0.2);
            // "top" nudged up (+0.1 -> 1.1), "boundary" nudged down
            // (0.60 - 0.1 = 0.50), which is now below "untouched"
            // (0.55). The whole vector must reflect that, not just
            // the first two entries.
            for pair in results.windows(2) {
                assert!(
                    pair[0].score >= pair[1].score,
                    "not sorted: {} ({}) before {} ({})",
                    pair[0].url,
                    pair[0].score,
                    pair[1].url,
                    pair[1].score
                );
            }
            assert_eq!(results[0].url, "top");
            assert_eq!(results[1].url, "untouched");
            assert_eq!(results[2].url, "boundary");
            assert_eq!(results[3].url, "tail");
        }

        #[test]
        fn rerank_threads_accepts_positive_integers() {
            assert_eq!(parse_intra_threads("1").unwrap(), 1);
            assert_eq!(parse_intra_threads("2").unwrap(), 2);
            assert_eq!(parse_intra_threads(" 64 ").unwrap(), 64);
            assert_eq!(
                parse_intra_threads(&MAX_INTRA_THREADS.to_string()).unwrap(),
                MAX_INTRA_THREADS
            );
        }

        #[test]
        fn rerank_threads_rejects_invalid_values() {
            for raw in ["", "0", "-1", "1.5", "two", "2147483648"] {
                assert!(parse_intra_threads(raw).is_err(), "accepted {raw:?}");
            }
        }

        #[test]
        fn rerank_threads_auto_clamps_constrained_processes() {
            assert_eq!(
                automatic_intra_threads(Some(2), 8),
                ThreadSelection::Automatic {
                    threads: 2,
                    effective: 2,
                    physical: 8,
                }
            );
            assert_eq!(automatic_intra_threads(Some(1), 8).threads(), Some(1));
        }

        #[test]
        fn rerank_threads_auto_preserves_unconstrained_onnx_default() {
            for selection in [
                automatic_intra_threads(Some(8), 4),
                automatic_intra_threads(Some(4), 4),
                automatic_intra_threads(None, 8),
                automatic_intra_threads(Some(0), 8),
            ] {
                assert_eq!(selection.threads(), None);
                assert!(matches!(selection, ThreadSelection::OnnxDefault { .. }));
            }
        }

        #[test]
        fn rerank_threads_environment_override_wins() {
            let (selection, warning) = select_intra_threads(ThreadOverride::Value("1"), Some(2), 8);
            assert_eq!(selection.threads(), Some(1));
            assert!(matches!(
                selection,
                ThreadSelection::Environment { threads: 1 }
            ));
            assert_eq!(warning, None);
        }

        #[test]
        fn rerank_threads_invalid_override_falls_back_to_auto() {
            for override_value in [
                ThreadOverride::Value("invalid"),
                ThreadOverride::InvalidUnicode,
            ] {
                let (selection, warning) = select_intra_threads(override_value, Some(2), 8);
                assert_eq!(selection.threads(), Some(2));
                assert!(matches!(selection, ThreadSelection::Automatic { .. }));
                assert!(warning.is_some());
            }

            let (selection, warning) =
                select_intra_threads(ThreadOverride::Value("invalid"), Some(8), 4);
            assert_eq!(selection.threads(), None);
            assert!(matches!(selection, ThreadSelection::OnnxDefault { .. }));
            assert!(warning.is_some());
        }
    }
}

#[cfg(not(feature = "rerank"))]
mod inner {
    /// No-op stubs when the `rerank` feature is disabled.
    pub fn rerank(_query: &str, _results: &mut [crate::search::rank::Merged]) {}
    pub fn active() -> bool {
        false
    }
    pub fn cross_encoder_scores(_query: &str, _docs: &[(String, String)]) -> Option<Vec<f64>> {
        None
    }
    pub fn is_model_cached() -> bool {
        false
    }
    pub fn topup(_query: &str, _results: &mut [crate::search::rank::Merged], _depth: usize) {}
}

pub use inner::{active, cross_encoder_scores, is_model_cached, rerank, topup};
