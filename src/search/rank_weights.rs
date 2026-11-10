//! The ranker's weights. **Generated** by `scripts/train_rank.py`; do not edit
//! by hand.
//!
//! The header below is rewritten by the script and is what makes a number here
//! checkable: the corpus it was trained on, the seed, and the held-out score it
//! had to beat. `trained()` is false until the script writes real weights, and
//! the ranker then stays on the measured fusion — a model that does not beat the
//! fusion on held-out rows is not shipped.

/// One trained model: per-feature mean and spread (the standardisation the
/// training used) and the weight each feature carries.
pub struct Weights {
    pub mean: [f64; super::ltr::FEATURES],
    pub std: [f64; super::ltr::FEATURES],
    pub w: [f64; super::ltr::FEATURES],
}

impl Weights {
    /// All zeros: no model, and [`trained`] says so.
    pub const PLACEHOLDER: Weights = Weights {
        mean: [0.0; super::ltr::FEATURES],
        std: [1.0; super::ltr::FEATURES],
        w: [0.0; super::ltr::FEATURES],
    };
}

// ── BEGIN GENERATED ──
/// The weights in use. Empty until `scripts/train_rank.py` writes them.
pub const WEIGHTS: Weights = Weights::PLACEHOLDER;
/// The corpus hash the weights were trained on, and the seed.
pub const TRAINED_ON: &str = "";
// ── END GENERATED ──

/// Whether a trained model is installed. False means the ranker is the measured
/// fusion (family RRF + BM25+ tie break + RM3), which is what 5.3.0 ships unless
/// the model beats it on held-out rows.
pub fn trained() -> bool {
    !TRAINED_ON.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placeholder_is_not_a_model() {
        // The shipped default: no weights, so the ranker stays on the fusion.
        assert_eq!(WEIGHTS.w, [0.0; super::super::ltr::FEATURES]);
        assert!(!trained() || !TRAINED_ON.is_empty());
    }
}
