//! Pure, per-call retrieval ranking over the admitted head and decoded vectors.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};

/// Owner-reviewed Decision 0018 constants; not runtime tuning parameters.
pub const CANDIDATE_DEPTH: usize = 200;
const RRF_K: f64 = 20.0;
const LEXICAL_WEIGHT: f64 = 1.0;
const VECTOR_WEIGHT: f64 = 4.0;
use crate::search_embed::DIMENSIONS;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SearchCandidateKey {
    pub version_id: i64,
    pub syntax_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchCandidate {
    pub key: SearchCandidateKey,
    pub card_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankedHit {
    pub key: SearchCandidateKey,
    pub lexical_rank: Option<usize>,
    pub vector_rank: Option<usize>,
}

#[derive(Debug)]
pub struct SearchRanking {
    pub hits: Vec<RankedHit>,
    pub embedded: u64,
}

fn valid_vector(vector: &[i8]) -> bool {
    vector.len() == DIMENSIONS && vector.iter().any(|value| *value != 0)
}

fn direction(vector: &[i8]) -> Vec<f32> {
    let norm = vector
        .iter()
        .map(|value| f32::from(*value).powi(2))
        .sum::<f32>()
        .sqrt();
    vector
        .iter()
        .map(|value| f32::from(*value) / norm)
        .collect()
}

fn score(hit: &RankedHit) -> f64 {
    // Binary64, lexical contribution first and vector contribution second.
    let lexical = hit
        .lexical_rank
        .map_or(0.0, |rank| LEXICAL_WEIGHT / (RRF_K + rank as f64));
    let vector = hit
        .vector_rank
        .map_or(0.0, |rank| VECTOR_WEIGHT / (RRF_K + rank as f64));
    lexical + vector
}

fn compare_hits(a: &RankedHit, b: &RankedHit) -> std::cmp::Ordering {
    score(b)
        .total_cmp(&score(a))
        .then_with(|| {
            a.lexical_rank
                .into_iter()
                .chain(a.vector_rank)
                .min()
                .cmp(&b.lexical_rank.into_iter().chain(b.vector_rank).min())
        })
        .then_with(|| {
            a.lexical_rank
                .unwrap_or(usize::MAX)
                .cmp(&b.lexical_rank.unwrap_or(usize::MAX))
        })
        .then_with(|| a.key.syntax_id.as_bytes().cmp(b.key.syntax_id.as_bytes()))
}

/// Vectors are decoded rows read in one caller-owned cache transaction, keyed by
/// card hash for its active model. Missing/wrong-dimension rows are cache misses.
/// Counts cover all candidates, independently of the query and arm cutoffs.
pub fn rank(
    candidates: &[SearchCandidate],
    lexical: &[SearchCandidateKey],
    vectors: &BTreeMap<String, Vec<i8>>,
    query: Option<&[i8]>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SearchRanking> {
    check()?;
    if query.is_some_and(|query| !valid_vector(query)) {
        return Err(crate::search_vectors::InvalidVector.into());
    }
    let keys: BTreeSet<_> = candidates.iter().map(|candidate| &candidate.key).collect();
    ensure!(
        keys.len() == candidates.len(),
        crate::store::SelectedIntegrity("store_unavailable: duplicate search candidate".into())
    );
    let mut fused = BTreeMap::<SearchCandidateKey, RankedHit>::new();
    for (index, key) in lexical.iter().take(CANDIDATE_DEPTH).enumerate() {
        check()?;
        ensure!(
            keys.contains(key),
            crate::store::SelectedIntegrity(
                "store_unavailable: lexical candidate outside admitted head".into()
            )
        );
        ensure!(
            !fused.contains_key(key),
            crate::store::SelectedIntegrity(
                "store_unavailable: duplicate lexical candidate".into()
            )
        );
        fused.insert(
            key.clone(),
            RankedHit {
                key: key.clone(),
                lexical_rank: Some(index + 1),
                vector_rank: None,
            },
        );
    }
    let query_direction = query.map(direction);
    let mut embedded = 0;
    let mut vector_scores = Vec::new();
    for candidate in candidates {
        check()?;
        let Some(vector) = vectors
            .get(&candidate.card_hash)
            .filter(|vector| valid_vector(vector))
        else {
            continue;
        };
        embedded += 1;
        if let Some(query) = &query_direction {
            // The accepted int8 representation ranks normalized F32 directions.
            // Reduction order is fixed within a call, not promised across runtimes.
            let vector = direction(vector);
            let cosine: f32 = query.iter().zip(vector).map(|(a, b)| a * b).sum();
            ensure!(cosine.is_finite(), crate::search_vectors::InvalidVector);
            vector_scores.push((candidate.key.clone(), cosine));
        }
    }
    vector_scores.sort_by(|(a, a_score), (b, b_score)| {
        b_score
            .total_cmp(a_score)
            .then_with(|| a.syntax_id.as_bytes().cmp(b.syntax_id.as_bytes()))
    });
    check()?;
    for (index, (key, _)) in vector_scores.into_iter().take(CANDIDATE_DEPTH).enumerate() {
        let hit = fused.entry(key.clone()).or_insert(RankedHit {
            key,
            lexical_rank: None,
            vector_rank: None,
        });
        hit.vector_rank = Some(index + 1);
    }
    let mut hits: Vec<_> = fused.into_values().collect();
    hits.sort_by(compare_hits);
    check()?;
    Ok(SearchRanking { hits, embedded })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(index: usize, hash: usize) -> SearchCandidate {
        SearchCandidate {
            key: SearchCandidateKey {
                version_id: index as i64,
                syntax_id: format!("sid:v1:{index:032x}"),
            },
            card_hash: format!("{hash:064x}"),
        }
    }
    fn vector(x: i8, y: i8) -> Vec<i8> {
        let mut vector = vec![0; DIMENSIONS];
        vector[0] = x;
        vector[1] = y;
        vector
    }

    #[test]
    fn weighted_fusion_records_only_bounded_arm_membership() {
        let candidates: Vec<_> = (1..=205).map(|index| candidate(index, index)).collect();
        let lexical: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.key.clone())
            .collect();
        let vectors = candidates
            .iter()
            .map(|candidate| (candidate.card_hash.clone(), vector(1, 0)))
            .collect();
        let ranking = rank(
            &candidates,
            &lexical,
            &vectors,
            Some(&vector(1, 0)),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(ranking.embedded, 205);
        assert_eq!(ranking.hits.len(), CANDIDATE_DEPTH);
        for (index, hit) in ranking.hits.iter().enumerate() {
            assert_eq!(hit.key, candidates[index].key);
            assert_eq!(hit.lexical_rank, Some(index + 1));
            assert_eq!(hit.vector_rank, Some(index + 1));
        }
        let lexical = vec![candidates[204].key.clone()];
        let ranking = rank(
            &candidates,
            &lexical,
            &vectors,
            Some(&vector(1, 0)),
            &mut || Ok(()),
        )
        .unwrap();
        let last = ranking
            .hits
            .iter()
            .find(|hit| hit.key == candidates[204].key)
            .unwrap();
        assert_eq!(last.lexical_rank, Some(1));
        assert_eq!(last.vector_rank, None);
        assert_eq!(ranking.hits.len(), 201);
    }

    #[test]
    fn cosine_orders_signed_vectors_and_sid_ties() {
        let candidates = vec![
            candidate(3, 3),
            candidate(2, 2),
            candidate(1, 1),
            candidate(4, 4),
        ];
        let vectors = BTreeMap::from([
            (candidates[0].card_hash.clone(), vector(1, 0)),
            (candidates[1].card_hash.clone(), vector(-1, 0)),
            (candidates[2].card_hash.clone(), vector(2, 0)),
            (candidates[3].card_hash.clone(), vector(0, 1)),
        ]);
        let ranking = rank(&candidates, &[], &vectors, Some(&vector(1, 0)), &mut || {
            Ok(())
        })
        .unwrap();
        assert_eq!(
            ranking
                .hits
                .iter()
                .map(|hit| hit.key.version_id)
                .collect::<Vec<_>>(),
            [1, 3, 4, 2]
        );
    }

    #[test]
    fn embedded_counts_each_candidate_and_rejects_dimension_misses() {
        let candidates = vec![
            candidate(1, 1),
            candidate(2, 1),
            candidate(3, 2),
            candidate(4, 3),
            candidate(5, 4),
        ];
        let vectors = BTreeMap::from([
            (format!("{:064x}", 1), vector(1, 0)),
            (format!("{:064x}", 2), vec![1; DIMENSIONS - 1]),
            (format!("{:064x}", 3), vec![0; DIMENSIONS]),
        ]);
        let ranking = rank(
            &candidates,
            &[candidates[3].key.clone()],
            &vectors,
            None,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(ranking.embedded, 2);
        assert_eq!(ranking.hits.len(), 1);
        assert_eq!(ranking.hits[0].vector_rank, None);
        assert!(
            rank(&candidates, &[], &vectors, Some(&[1]), &mut || Ok(()))
                .unwrap_err()
                .is::<crate::search_vectors::InvalidVector>()
        );
    }

    #[test]
    fn fusion_binary64_and_tie_breaks_match_decision() {
        let hit = RankedHit {
            key: candidate(1, 1).key,
            lexical_rank: Some(3),
            vector_rank: Some(7),
        };
        assert_eq!(
            score(&hit).to_bits(),
            (1.0_f64 / 23.0 + 4.0_f64 / 27.0).to_bits()
        );
        let lexical = RankedHit {
            key: candidate(2, 2).key,
            lexical_rank: Some(1),
            vector_rank: None,
        };
        let vector = RankedHit {
            key: candidate(1, 1).key,
            lexical_rank: None,
            vector_rank: Some(64),
        };
        assert_eq!(score(&lexical), score(&vector));
        assert!(compare_hits(&lexical, &vector).is_lt());
        let a = RankedHit {
            key: candidate(1, 1).key,
            lexical_rank: Some(5),
            vector_rank: Some(2),
        };
        let b = RankedHit {
            key: candidate(2, 2).key,
            ..a.clone()
        };
        assert!(compare_hits(&a, &b).is_lt());
    }

    #[test]
    fn failures_never_return_partial_ranking() {
        let candidate = candidate(1, 1);
        assert!(
            rank(
                &[candidate.clone(), candidate.clone()],
                &[],
                &BTreeMap::new(),
                None,
                &mut || Ok(())
            )
            .is_err()
        );
        assert!(
            rank(
                &[],
                std::slice::from_ref(&candidate.key),
                &BTreeMap::new(),
                None,
                &mut || Ok(())
            )
            .is_err()
        );
        let error = rank(&[candidate], &[], &BTreeMap::new(), None, &mut || {
            anyhow::bail!("deadline_exceeded")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "deadline_exceeded");
    }
}
