//! Local web memory (v4 phase 5.2): semantic search over pages this
//! machine already fetched, kept entirely on this box. Embeddings
//! come from a locally-run all-MiniLM-L6-v2 onnxruntime session; the
//! store is a bounded, atomic JSON index under the cache dir.
//!
//! The whole module is compiled only with the rerank feature (the
//! same feature that ships the search reranker; every release build
//! carries it). Ingest hooks live at the tool surfaces
//! (fetch/search/crawl), not inside the fetcher internals. The kill
//! switch (`DONSETCH_NO_WEB_MEMORY`) turns the module off at both the
//! ingest and the search layers; nothing writes or reads while it is
//! set.

#[cfg(feature = "rerank")]
pub mod model;
#[cfg(feature = "rerank")]
pub mod store;

#[cfg(feature = "rerank")]
pub use store::{
    MemoryHit, cap, clear, index_path, ingest, ingest_async, ingest_batch, kill_switch, rows,
    search,
};

/// Maximum `limit` accepted by web_memory (schema clamp).
pub const LIMIT_MAX: usize = 50;

/// The implicit title of a markdown page = the first `# ` heading.
#[cfg(feature = "rerank")]
pub fn title_of(md: &str) -> String {
    for line in md.lines() {
        if let Some(t) = line.strip_prefix("# ") {
            return t.trim().to_string();
        }
    }
    String::new()
}

#[cfg(feature = "rerank")]
/// Argument guard shared by the CLI parser and the MCP tool:
/// returns None when the arguments are valid, Some(message) with an
/// honest operator-level reason otherwise. `limit` is optional and
/// clamped to 1..=LIMIT_MAX; the query's presence is checked by the
/// caller so the error can be tool-specific.
pub fn guard(limit: Option<u64>) -> Option<String> {
    match limit {
        None => None,
        Some(n) if (1..=LIMIT_MAX as u64).contains(&n) => None,
        Some(n) => Some(format!("limit {n} out of range (1..={LIMIT_MAX})")),
    }
}

#[cfg(feature = "rerank")]
#[cfg(test)]
mod tests {
    use super::*;

    /// title_of returns the first `# ` heading, else an empty string
    /// (the search snippet path also feeds title-less rows).
    #[test]
    fn title_of_first_heading() {
        assert_eq!(title_of("# Hello\nbody"), "Hello");
        assert_eq!(title_of("## Only h2"), "");
        assert_eq!(title_of("text\n# Later\nmore"), "Later");
        assert_eq!(title_of(""), "");
    }

    /// Guard: the limit bounds the wire contract on both the CLI and
    /// the MCP surface, so junk limit values fail before any embed.
    #[test]
    fn guard_imit_bounds() {
        assert!(guard(None).is_none());
        assert!(guard(Some(1)).is_none());
        assert!(guard(Some(50)).is_none());
        assert!(guard(Some(0)).is_some());
        assert!(guard(Some(51)).is_some());
        assert!(guard(Some(60)).is_some());
        assert!(guard(Some(u64::MAX)).is_some());
    }
}
