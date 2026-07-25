//! Standard IR metrics, macro-averaged per query by the callers. This
//! deliberately differs from anny's bench recall (micro-averaged against
//! ANN ground truth) — here gold is human labeling, so per-query means are
//! the comparable convention.

use fxhash::FxHashSet;

/// |gold ∩ ranked[..k]| / |gold|. With a single positive this is accuracy@k.
pub fn recall_at_k(ranked: &[usize], gold: &FxHashSet<usize>, k: usize) -> f64 {
    if gold.is_empty() {
        return 0.0;
    }
    let hits = ranked.iter().take(k).filter(|i| gold.contains(i)).count();
    hits as f64 / gold.len() as f64
}

/// Reciprocal rank of the first gold hit within the top k, else 0.
pub fn mrr_at_k(ranked: &[usize], gold: &FxHashSet<usize>, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .position(|i| gold.contains(i))
        .map_or(0.0, |pos| 1.0 / (pos as f64 + 1.0))
}

/// Binary-relevance NDCG@k: DCG sums 1/log2(rank+2) over gold hits in the
/// top k; the ideal ranking packs all |gold| positives first.
pub fn ndcg_at_k(ranked: &[usize], gold: &FxHashSet<usize>, k: usize) -> f64 {
    if gold.is_empty() {
        return 0.0;
    }
    let gain = |pos: usize| 1.0 / ((pos as f64) + 2.0).log2();
    let dcg: f64 = ranked
        .iter()
        .take(k)
        .enumerate()
        .filter(|(_, i)| gold.contains(i))
        .map(|(pos, _)| gain(pos))
        .sum();
    let idcg: f64 = (0..k.min(gold.len())).map(gain).sum();
    dcg / idcg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gold(ids: &[usize]) -> FxHashSet<usize> {
        ids.iter().copied().collect()
    }

    #[test]
    fn perfect_ranking_scores_one() {
        let g = gold(&[0]);
        let ranked = [0, 1, 2, 3];
        assert_eq!(recall_at_k(&ranked, &g, 10), 1.0);
        assert_eq!(mrr_at_k(&ranked, &g, 10), 1.0);
        assert_eq!(ndcg_at_k(&ranked, &g, 10), 1.0);
    }

    #[test]
    fn gold_at_rank_three() {
        let g = gold(&[7]);
        let ranked = [1, 2, 7, 3];
        assert_eq!(recall_at_k(&ranked, &g, 10), 1.0);
        assert_eq!(recall_at_k(&ranked, &g, 2), 0.0);
        assert!((mrr_at_k(&ranked, &g, 10) - 1.0 / 3.0).abs() < 1e-12);
        // DCG = 1/log2(4) = 0.5, IDCG = 1/log2(2) = 1
        assert!((ndcg_at_k(&ranked, &g, 10) - 0.5).abs() < 1e-12);
    }

    #[test]
    fn gold_absent_scores_zero() {
        let g = gold(&[99]);
        let ranked = [1, 2, 3];
        assert_eq!(recall_at_k(&ranked, &g, 10), 0.0);
        assert_eq!(mrr_at_k(&ranked, &g, 10), 0.0);
        assert_eq!(ndcg_at_k(&ranked, &g, 10), 0.0);
    }

    #[test]
    fn multi_positive_partial_credit() {
        let g = gold(&[4, 5]);
        let ranked = [4, 1, 2]; // one of two positives found, at rank 1
        assert_eq!(recall_at_k(&ranked, &g, 3), 0.5);
        assert_eq!(mrr_at_k(&ranked, &g, 3), 1.0);
        // DCG = 1, IDCG = 1 + 1/log2(3)
        let want = 1.0 / (1.0 + 1.0 / 3.0f64.log2());
        assert!((ndcg_at_k(&ranked, &g, 3) - want).abs() < 1e-12);
    }
}
