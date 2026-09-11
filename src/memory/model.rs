//! Local embedding model bridge for web memory (v4 phase 5.2).
//!
//! all-MiniLM-L6-v2, quantized, running on the vendored onnxruntime
//! (the same runtime the search reranker uses). The model and
//! tokenizer download once from HuggingFace, pinned by SHA-256, into
//! the cache dir: the exact same trust shape as the search reranker
//! (known host, exact byte size, exact hash, atomic install).
//!
//! After the first successful download this path is fully local: no
//! call in this module touches the network again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use ndarray::Array2;
use ort::session::Session;
use ort::value::TensorRef;
use sha2::{Digest, Sha256};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams, TruncationStrategy};

// `resolve/main` is a branch reference, not a revision: the host
// serves whatever currently sits at that repo's head, so the hash and
// byte count below are the only guard on what actually arrives. An
// upstream re-push fails the pin on every install until both are
// bumped — a visible outage of web memory rather than a silent model
// swap, which is the trade being made. Pinning the URL to a commit
// would remove the exposure; the sibling downloaders in
// search/rerank.rs and pdf/ocr.rs float the same way.
//
// The file installs under a fixed name rather than one derived from
// its hash, so two pins never coexist: bumping one deletes the other's
// download. That is acceptable while the model is effectively static,
// and content-addressed paths are out of scope here.
pub const MODEL_URL: &str =
    "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/onnx/model_quantized.onnx";
pub const MODEL_SHA256: &str = "afdb6f1a0e45b715d0bb9b11772f032c399babd23bfc31fed1c170afc848bdb1";
pub const MODEL_BYTES: usize = 22_972_370;
pub const TOKENIZER_URL: &str =
    "https://huggingface.co/Xenova/all-MiniLM-L6-v2/resolve/main/tokenizer.json";
pub const TOKENIZER_SHA256: &str =
    "da0e79933b9ed51798a3ae27893d3c5fa4a201126cef75586296df9b4d2c62a0";
pub const TOKENIZER_BYTES: usize = 711_661;
pub const EMBED_DIM: usize = 384;
const MAX_SEQ: usize = 256;

/// Embedded page vectors from the model inside an internal mutex.
pub struct Embedder {
    session: Mutex<Session>,
    tok: Tokenizer,
}

static EMBEDDER: std::sync::OnceLock<Arc<Embedder>> = std::sync::OnceLock::new();

pub fn model_dir() -> PathBuf {
    let mut p = crate::paths::cache_dir();
    p.push("memory");
    p
}

pub fn model_path() -> PathBuf {
    model_dir().join("model_quantized.onnx")
}

pub fn tokenizer_path() -> PathBuf {
    model_dir().join("tokenizer.json")
}

pub fn has_model() -> bool {
    model_path().is_file() && tokenizer_path().is_file()
}

fn verify_bytes(body: &[u8], sha: &str, size: usize) -> bool {
    if body.len() != size {
        if std::env::var_os("DONGHOST_DEBUG").is_some() {
            eprintln!("[memory] verify: len {} != {}", body.len(), size);
        }
        return false;
    }
    let mut h = Sha256::new();
    h.update(body);
    let hex: String = h.finalize().iter().map(|b| format!("{:02x}", b)).collect();
    if std::env::var_os("DONGHOST_DEBUG").is_some() {
        eprintln!("[memory] verify: got={} exp={}", hex, sha);
    }
    hex == sha
}

/// SHA-256 pinned download into dest; a no-op when the file is
/// already present and correct. Atomic install: the write lands at a
/// temp path and renames once the hash verifies.
fn ensure_file(
    url: &str,
    sha: &str,
    size: usize,
    dest: &PathBuf,
    what: &str,
) -> Result<(), String> {
    if let Ok(bytes) = std::fs::read(dest) {
        if verify_bytes(&bytes, sha, size) {
            return Ok(());
        }
        // Delete before redownloading. The rename below would replace
        // it on success, so this is about the failure path: a
        // surviving bad file is reported as present by
        // has_model()/ready(), and every later call pays a full
        // re-read and re-hash before rejecting it again.
        //
        // A concurrent remover reached the state we wanted. Any other
        // error fails fast: a sharing violation here would fail the
        // rename too, after a 23MB download.
        match std::fs::remove_file(dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                eprintln!(
                    "[memory] corrupt {what} at {} already removed by another process",
                    dest.display()
                );
            }
            Err(e) => {
                return Err(format!(
                    "memory: remove corrupt {what} at {}: {e}",
                    dest.display()
                ));
            }
        }
    }
    std::fs::create_dir_all(dest.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|e| format!("memory: mkdir {}: {e}", dest.display()))?;
    let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
    let url_owned = url.to_string();
    let label = what.to_string();
    std::thread::spawn(move || {
        let what = label;
        let run = (|| -> Result<Vec<u8>, String> {
            let client = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(90))
                .build()
                .map_err(|e| format!("memory: download client for {what}: {e}"))?;
            let resp = client
                .get(&url_owned)
                .header("user-agent", "donsetch/4")
                .send()
                .map_err(|e| format!("memory: download {what}: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("memory: download {what}: http {}", resp.status()));
            }
            resp.bytes()
                .map(|b| b.to_vec())
                .map_err(|e| format!("memory: download body {what}: {e}"))
        })();
        let _ = tx.send(run);
    });
    let body = rx
        .recv()
        .map_err(|e| format!("memory: download recv for {what}: {e}"))??;
    #[cfg(feature = "rerank")]
    if std::env::var_os("DONGHOST_DEBUG").is_some() {
        let mut h = Sha256::new();
        h.update(&body);
        let debug_sha: String = h.finalize().iter().map(|b| format!("{:02x}", b)).collect();
        eprintln!(
            "[memory] download {what}: {} bytes sha={debug_sha}",
            body.len()
        );
    }
    if !verify_bytes(&body, sha, size) {
        return Err(format!(
            "memory: download {what}: {} bytes / sha mismatch",
            body.len()
        ));
    }
    let tmp = dest.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&tmp, &body).map_err(|e| format!("memory: tmp write: {e}"))?;
    std::fs::rename(&tmp, dest).map_err(|e| format!("memory: tmp rename: {e}"))?;
    Ok(())
}

/// Download (if needed) both files, then lazily initialize the
/// onnxruntime session + tokenizer. Subsequent calls are cheap.
pub fn ensure_loaded() -> Result<&'static Arc<Embedder>, String> {
    if let Some(e) = EMBEDDER.get() {
        return Ok(e);
    }
    let dir = model_dir();
    ensure_file(MODEL_URL, MODEL_SHA256, MODEL_BYTES, &model_path(), "model")?;
    ensure_file(
        TOKENIZER_URL,
        TOKENIZER_SHA256,
        TOKENIZER_BYTES,
        &tokenizer_path(),
        "tokenizer",
    )?;
    crate::onnx::ensure_loaded()?;
    let mut tok = Tokenizer::from_file(dir.join("tokenizer.json"))
        .map_err(|e| format!("memory: tokenizer: {e}"))?;
    tok.with_truncation(Some(TruncationParams {
        direction: tokenizers::TruncationDirection::Right,
        max_length: MAX_SEQ,
        strategy: TruncationStrategy::LongestFirst,
        stride: 0,
    }))
    .map_err(|e| format!("memory: tokenizer truncation: {e}"))?;
    tok.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        direction: tokenizers::PaddingDirection::Right,
        pad_to_multiple_of: None,
        pad_id: 0,
        pad_type_id: 0,
        pad_token: "[PAD]".to_string(),
    }));
    let sess = Session::builder()
        .map_err(|e| format!("memory: ort: {e}"))?
        .with_intra_threads(1)
        .map_err(|e| format!("memory: ort threads: {e}"))?
        .commit_from_file(model_path())
        .map_err(|e| format!("memory: ort commit: {e}"))?;
    let _ = EMBEDDER.set(Arc::new(Embedder {
        session: Mutex::new(sess),
        tok,
    }));
    EMBEDDER
        .get()
        .ok_or_else(|| "memory: model load".to_string())
}

/// The kill switch: store ingest and store search are both disabled
/// while the flag is present in the environment.
pub fn kill_switch() -> bool {
    std::env::var_os("DONSETCH_NO_WEB_MEMORY").is_some()
}

/// True when the model + tokenizer files are present AND the model
/// is loaded in the current process (never triggers a download).
pub fn ready() -> bool {
    has_model() && EMBEDDER.get().is_some()
}

/// One tokenized row: input ids, attention masks, all i64.
type Encoded = (Vec<Vec<i64>>, Vec<Vec<i64>>, Vec<Vec<i64>>);

fn to_batch(texts: &[String]) -> Result<Encoded, String> {
    let e = ensure_loaded()?;
    let batch = e
        .tok
        .encode_batch(texts.to_vec(), true)
        .map_err(|e| format!("memory: tokenize: {e}"))?;
    let mut ids = Vec::with_capacity(batch.len());
    let mut masks = Vec::with_capacity(batch.len());
    let mut types = Vec::with_capacity(batch.len());
    for enc in batch {
        let masked = enc.get_ids().to_vec();
        ids.push(masked.iter().map(|v| *v as i64).collect());
        masks.push(enc.get_attention_mask().iter().map(|v| *v as i64).collect());
        let t = if enc.get_type_ids().is_empty() {
            vec![0i64; enc.get_ids().len()]
        } else {
            enc.get_type_ids().iter().map(|v| *v as i64).collect()
        };
        types.push(t);
    }
    Ok((ids, masks, types))
}

fn sanitize(texts: &[String]) -> Vec<String> {
    texts
        .iter()
        .map(|s| {
            let s = s.chars().take(8_000).collect::<String>();
            if s.trim().is_empty() {
                ".".to_string()
            } else {
                s
            }
        })
        .collect()
}

/// Embed one text (batch size 1).
pub fn embed(text: &str) -> Result<Vec<f32>, String> {
    embed_batch(&[text.to_string()]).map(|v| v.into_iter().next().unwrap_or_default())
}

/// Embed a batch in one onnxruntime run. All padding inside the batch
/// is longest-first so a single text pad cost stays bounded.
pub fn embed_batch(texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let texts = sanitize(texts);
    let (ids, masks, types) = to_batch(&texts)?;
    let e = ensure_loaded()?;
    let seq = ids.first().map(|v| v.len()).unwrap_or(0);
    if seq == 0 {
        return Ok(vec![vec![0.0; EMBED_DIM]; texts.len()]);
    }
    let flat_ids: Vec<i64> = ids.iter().flatten().copied().collect();
    let flat_mask: Vec<i64> = masks.iter().flatten().copied().collect();
    let flat_types: Vec<i64> = types.iter().flatten().copied().collect();
    let n = texts.len();
    let ids_ty = Array2::from_shape_vec((n, seq), flat_ids)
        .map_err(|e| format!("memory: ort ids shape: {e}"))?;
    let attn_ty = Array2::from_shape_vec((n, seq), flat_mask.clone())
        .map_err(|e| format!("memory: ort mask shape: {e}"))?;
    let types_ty = Array2::from_shape_vec((n, seq), flat_types)
        .map_err(|e| format!("memory: ort types shape: {e}"))?;
    let mut lock = e
        .session
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let outputs = lock
        .run(ort::inputs![
            "input_ids" => TensorRef::from_array_view(ids_ty.view())
                .map_err(|e| format!("memory: ort ids: {e}"))?,
            "attention_mask" => TensorRef::from_array_view(attn_ty.view())
                .map_err(|e| format!("memory: ort mask: {e}"))?,
            "token_type_ids" => TensorRef::from_array_view(types_ty.view())
                .map_err(|e| format!("memory: ort types: {e}"))?,
        ])
        .map_err(|e| format!("memory: ort run: {e}"))?;
    let hidden = outputs["last_hidden_state"]
        .try_extract_array::<f32>()
        .map_err(|e| format!("memory: ort output: {e}"))?
        .into_owned()
        .into_dyn();
    let hidden = hidden.view();
    // (batch, seq, 384) mean pool over the attention mask, then L2 norm.
    let dims = hidden.shape().to_vec();
    if dims.len() != 3 || dims[2] != EMBED_DIM {
        return Err(format!("memory: ort hidden shape {dims:?}"));
    }
    let (b, s, d) = (dims[0], dims[1], dims[2]);
    let mut vecs = vec![vec![0.0f32; EMBED_DIM]; texts.len()];
    for row in 0..b {
        let mut seen = 0usize;
        for pos in 0..s {
            if flat_mask[row * s + pos] == 0 {
                continue;
            }
            for (dd, dst) in vecs[row].iter_mut().enumerate() {
                *dst += hidden[[row, pos, dd]];
            }
            seen += 1;
        }
        if seen == 0 {
            continue;
        }
        let scale = 1.0 / seen as f32;
        let sumsq: f32 = vecs[row].iter().map(|v| v * v * scale * scale).sum();
        let norm = sumsq.sqrt();
        if norm > 1e-12 {
            let inv = 1.0 / norm;
            for dst in vecs[row].iter_mut() {
                *dst *= scale * inv;
            }
        }
    }
    let _ = d;
    Ok(vecs)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fails if the failed-pin path returns to recursing into
    /// `ensure_file` instead of deleting: every frame re-read the same
    /// bytes, so the process dies on a stack overflow rather than the
    /// assertion reporting.
    #[test]
    fn corrupt_existing_file_is_removed_and_does_not_recurse() {
        let dir = std::env::temp_dir().join(format!("donsetch-model-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let dest = dir.join("model_quantized.onnx");
        std::fs::write(&dest, b"not the model").expect("seed");

        // Port 1 refuses instantly, so the redownload fails fast and
        // the test never waits on the network.
        let out = ensure_file(
            "http://127.0.0.1:1/model.onnx",
            "0000000000000000000000000000000000000000000000000000000000000000",
            999_999,
            &dest,
            "test model",
        );

        assert!(out.is_err(), "a failed redownload must report, not pretend");
        assert!(
            !dest.exists(),
            "the corrupt file must be removed, or the next call reads it again"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
