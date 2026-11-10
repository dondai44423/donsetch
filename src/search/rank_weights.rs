//! The ranker's weights. **Generated** by `scripts/train_rank.py`; do not edit
//! by hand.
//!
//! The header below is rewritten by the script and is what makes a number here
//! checkable: the corpus it was trained on, the seed, and the held-out score it
//! had to beat. `trained()` is false until the script writes real weights, and
//! the ranker then stays on the measured fusion — a model that does not beat the
//! fusion on held-out rows is not shipped.

/// One trained model: per-feature mean and spread (the standardisation the
/// training used), the weight each feature carries, and the query shapes it
/// ranks. A shape the model did not beat the fusion on, in cross-validation
/// over the training rows, keeps the fusion's order.
pub struct Weights {
    pub mean: [f64; super::ltr::FEATURES],
    pub std: [f64; super::ltr::FEATURES],
    pub w: [f64; super::ltr::FEATURES],
    pub shapes: &'static [&'static str],
}

impl Weights {
    /// All zeros: no model, and [`trained`] says so.
    pub const PLACEHOLDER: Weights = Weights {
        mean: [0.0; super::ltr::FEATURES],
        std: [1.0; super::ltr::FEATURES],
        w: [0.0; super::ltr::FEATURES],
        shapes: &[],
    };

    /// Whether this model orders a query of `shape`.
    pub fn ranks(&self, shape: super::query::Shape) -> bool {
        self.shapes.contains(&shape.label())
    }
}

// ── BEGIN GENERATED ──
/// Trained on bench feature dumps `aa31c343cba367de` (seed 7, l2 0.1, 485 gold rows).
/// Held-out MRR 0.8467 against the fusion's 0.8070 over 400 rows; no shape fell.
pub const WEIGHTS: Weights = Weights {
    mean: [
        0.707094, 0.500061, 0.469091, 0.394419, 0.139023, 0.153515, 1.139304, 0.228648, 0.176889,
        0.005049, 0.079469, 0.037023, 0.013936, 0.981301, 0.006077, 0.466138, 0.312474, 0.922401,
        0.318672, 0.181563, 0.759755, 0.227375,
    ],
    std: [
        0.333881, 0.370460, 0.370658, 0.382023, 0.335921, 0.360484, 0.353743, 0.266414, 0.381574,
        0.070874, 0.270469, 0.188819, 0.105784, 0.135113, 0.073705, 0.311302, 0.189207, 0.205021,
        0.147224, 0.385484, 0.287551, 0.419137,
    ],
    w: [
        0.121620, 0.005557, 0.000000, 0.023645, 0.182276, 0.196642, 0.037017, 0.396370, 0.333487,
        -0.017867, 0.101367, -0.161614, 0.001306, 0.007733, 0.037646, 0.047493, 0.000000, 0.011516,
        -0.165066, 0.337059, 0.000240, 0.002127,
    ],
    shapes: &["code", "howto", "shopping"],
};
/// The dumps the weights were trained on.
pub const TRAINED_ON: &str = "aa31c343cba367de";
// ── END GENERATED ──

/// Whether a trained model is installed. False means every query is ranked by
/// the measured fusion (family RRF + BM25+ tie break + RM3); true, the shapes in
/// `WEIGHTS.shapes` are ranked by the model and the rest by the fusion.
pub fn trained() -> bool {
    !TRAINED_ON.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_placeholder_is_not_a_model() {
        // No weights and no shape: a placeholder ranks nothing.
        let p = Weights::PLACEHOLDER;
        assert_eq!(p.w, [0.0; super::super::ltr::FEATURES]);
        assert!(!p.ranks(super::super::query::Shape::Web));
        assert_eq!(trained(), !TRAINED_ON.is_empty());
    }
}
