//! Pure, per-call retrieval ranking over the admitted head and decoded vectors.
use std::cmp::Ordering;
#[cfg(test)]
use std::collections::BTreeSet;
use std::collections::{BTreeMap, BinaryHeap};

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

// The largest heap member is the worst retained vector result. Input order is
// the final tie-break so equal SIDs retain the previous stable-sort behavior.
struct VectorScore<'a> {
    key: &'a SearchCandidateKey,
    cosine: f32,
    ordinal: usize,
}

impl PartialEq for VectorScore<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for VectorScore<'_> {}

impl PartialOrd for VectorScore<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VectorScore<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cosine
            .total_cmp(&self.cosine)
            .then_with(|| {
                self.key
                    .syntax_id
                    .as_bytes()
                    .cmp(other.key.syntax_id.as_bytes())
            })
            .then_with(|| self.ordinal.cmp(&other.ordinal))
    }
}

struct RankingState<'a> {
    candidates: &'a [SearchCandidate],
    fused: BTreeMap<SearchCandidateKey, RankedHit>,
    query_direction: Option<Vec<f32>>,
    vector_scores: BinaryHeap<VectorScore<'a>>,
    embedded: u64,
}

impl<'a> RankingState<'a> {
    fn new(
        candidates: &'a [SearchCandidate],
        lexical: &[SearchCandidateKey],
        query: Option<&[i8]>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<(Self, Vec<usize>)> {
        check()?;
        if query.is_some_and(|query| !valid_vector(query)) {
            return Err(crate::search_vectors::InvalidVector.into());
        }
        let mut ordinals: Vec<_> = (0..candidates.len()).collect();
        ordinals.sort_unstable_by(|a, b| candidates[*a].key.cmp(&candidates[*b].key));
        ensure!(
            ordinals
                .windows(2)
                .all(|pair| { candidates[pair[0]].key != candidates[pair[1]].key }),
            crate::store::SelectedIntegrity("store_unavailable: duplicate search candidate".into())
        );
        let mut fused = BTreeMap::new();
        for (index, key) in lexical.iter().take(CANDIDATE_DEPTH).enumerate() {
            check()?;
            ensure!(
                ordinals
                    .binary_search_by(|ordinal| candidates[*ordinal].key.cmp(key))
                    .is_ok(),
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
        check()?;
        Ok((
            Self {
                candidates,
                fused,
                query_direction: query.map(direction),
                vector_scores: BinaryHeap::with_capacity(CANDIDATE_DEPTH),
                embedded: 0,
            },
            ordinals,
        ))
    }

    fn admit(&mut self, ordinal: usize, cosine: Option<f32>) {
        self.embedded += 1;
        if let Some(cosine) = cosine {
            let scored = VectorScore {
                key: &self.candidates[ordinal].key,
                cosine,
                ordinal,
            };
            if self.vector_scores.len() < CANDIDATE_DEPTH {
                self.vector_scores.push(scored);
            } else if let Some(mut worst) = self.vector_scores.peek_mut()
                && scored < *worst
            {
                *worst = scored;
            }
        }
    }

    fn finish(mut self, check: &mut impl FnMut() -> Result<()>) -> Result<SearchRanking> {
        check()?;
        let _fusion = crate::search_model::SearchPhaseTimer::start("fusion");
        for (index, scored) in self.vector_scores.into_sorted_vec().into_iter().enumerate() {
            let hit = self
                .fused
                .entry(scored.key.clone())
                .or_insert_with(|| RankedHit {
                    key: scored.key.clone(),
                    lexical_rank: None,
                    vector_rank: None,
                });
            hit.vector_rank = Some(index + 1);
        }
        let mut hits: Vec<_> = self.fused.into_values().collect();
        hits.sort_by(compare_hits);
        check()?;
        Ok(SearchRanking {
            hits,
            embedded: self.embedded,
        })
    }
}

/// Lexical-only projection with the same admitted-head identity checks. It never
/// reads the vector cache; callers supply the reason for omitting the vector arm.
pub(crate) fn rank_lexical(
    candidates: &[SearchCandidate],
    lexical: &[SearchCandidateKey],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SearchRanking> {
    let (state, _) = RankingState::new(candidates, lexical, None, check)?;
    state.finish(check)
}

/// Merge the admitted head's sorted hashes with one current-model cache scan.
/// Duplicate hashes share scoring work, but coverage and ties remain per candidate.
pub fn rank_snapshot(
    candidates: &[SearchCandidate],
    lexical: &[SearchCandidateKey],
    snapshot: Option<&crate::search_vectors::VectorSnapshot<'_>>,
    model_id: Option<&str>,
    query: Option<&[i8]>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SearchRanking> {
    let _phase = crate::search_model::SearchPhaseTimer::start("ranking_total");
    let (mut state, mut ordinals) = RankingState::new(candidates, lexical, query, check)?;
    if let (Some(snapshot), Some(model_id)) = (snapshot, model_id) {
        ordinals.sort_unstable_by(|a, b| {
            candidates[*a]
                .card_hash
                .as_bytes()
                .cmp(candidates[*b].card_hash.as_bytes())
                .then_with(|| a.cmp(b))
        });
        check()?;
        let profiling = crate::search_model::profiling_enabled();
        let scan_start = profiling.then(std::time::Instant::now);
        let mut scoring_time = std::time::Duration::ZERO;
        let mut head = 0;
        snapshot.scan_model(model_id, &mut |hash, vector| {
            check()?;
            let Some(hash) = hash else { return Ok(()) };
            while head < ordinals.len() && candidates[ordinals[head]].card_hash.as_bytes() < hash {
                check()?;
                head += 1;
            }
            if head == ordinals.len() || candidates[ordinals[head]].card_hash.as_bytes() != hash {
                return Ok(());
            }
            let start = head;
            while head < ordinals.len() && candidates[ordinals[head]].card_hash.as_bytes() == hash {
                check()?;
                head += 1;
            }
            let Some(vector) = vector else { return Ok(()) };
            if vector.len() != DIMENSIONS || !vector.iter().any(|value| *value != 0) {
                return Ok(());
            }
            let scoring_start = profiling.then(std::time::Instant::now);
            let cosine = state
                .query_direction
                .as_ref()
                .map(|query| cosine_bytes(query, vector));
            if let Some(cosine) = cosine {
                ensure!(cosine.is_finite(), crate::search_vectors::InvalidVector);
            }
            for ordinal in &ordinals[start..head] {
                check()?;
                state.admit(*ordinal, cosine);
            }
            if let Some(start) = scoring_start {
                scoring_time += start.elapsed();
            }
            check()?;
            Ok(())
        })?;
        if let Some(start) = scan_start {
            crate::search_model::SearchPhaseTimer::record(
                "vector_reads",
                start.elapsed().saturating_sub(scoring_time),
            );
            crate::search_model::SearchPhaseTimer::record("vector_scoring", scoring_time);
        }
    }
    state.finish(check)
}

fn cosine_bytes(query: &[f32], vector: &[u8]) -> f32 {
    // At 1024 signed-int8 dimensions, the sum of squares is <= 2^24.
    // Every integer partial sum is exactly representable in F32, so this
    // widened reduction reproduces the original F32 norm before the sqrt.
    let squares: i32 = vector
        .iter()
        .map(|value| {
            let value = i32::from(*value as i8);
            value * value
        })
        .sum();
    let norm = (squares as f32).sqrt();
    #[cfg(target_arch = "aarch64")]
    {
        cosine_neon(query, vector, norm)
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        query
            .iter()
            .zip(vector)
            .map(|(a, b)| a * (f32::from(*b as i8) / norm))
            .sum()
    }
}

#[cfg(target_arch = "aarch64")]
fn cosine_neon(query: &[f32], vector: &[u8], norm: f32) -> f32 {
    use std::arch::aarch64::*;

    let mut total = -0.0_f32;
    let mut offset = 0;
    // SIMD computes independent normalized F32 products, not an integer dot
    // or horizontal float reduction. Add products in the original order.
    while offset + 8 <= vector.len() {
        let mut products = [0.0_f32; 8];
        // SAFETY: callers supply DIMENSIONS query and vector entries. Each
        // load/store addresses a full block within those slices/this scratch.
        // NEON is part of the aarch64 target's baseline instruction set.
        unsafe {
            let bytes = vld1_s8(vector.as_ptr().add(offset).cast());
            let wide = vmovl_s8(bytes);
            let low = vcvtq_f32_s32(vmovl_s16(vget_low_s16(wide)));
            let high = vcvtq_f32_s32(vmovl_high_s16(wide));
            let divisor = vdupq_n_f32(norm);
            let low = vmulq_f32(
                vld1q_f32(query.as_ptr().add(offset)),
                vdivq_f32(low, divisor),
            );
            let high = vmulq_f32(
                vld1q_f32(query.as_ptr().add(offset + 4)),
                vdivq_f32(high, divisor),
            );
            vst1q_f32(products.as_mut_ptr(), low);
            vst1q_f32(products.as_mut_ptr().add(4), high);
        }
        for product in products {
            total += product;
        }
        offset += 8;
    }
    for (a, b) in query[offset..].iter().zip(&vector[offset..]) {
        total += a * (f32::from(*b as i8) / norm);
    }
    total
}

/// Lookup decodes one row from a single caller-owned vector snapshot for the
/// active model. Missing/wrong-dimension rows are cache misses. No vectors are
/// retained between lookups; duplicate hashes are counted once per candidate.
/// Counts cover all candidates, independently of the query and arm cutoffs.
#[cfg(test)]
fn rank(
    candidates: &[SearchCandidate],
    lexical: &[SearchCandidateKey],
    lookup: &mut impl FnMut(&str) -> Result<Option<Vec<i8>>>,
    query: Option<&[i8]>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SearchRanking> {
    let _phase = crate::search_model::SearchPhaseTimer::start("ranking_total");
    let profiling = crate::search_model::profiling_enabled();
    let mut lookup_time = std::time::Duration::ZERO;
    let mut scoring_time = std::time::Duration::ZERO;
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
    let mut vector_scores = BinaryHeap::with_capacity(CANDIDATE_DEPTH);
    for (ordinal, candidate) in candidates.iter().enumerate() {
        check()?;
        let lookup_start = profiling.then(std::time::Instant::now);
        let vector = lookup(&candidate.card_hash)?;
        if let Some(start) = lookup_start {
            lookup_time += start.elapsed();
        }
        check()?;
        let Some(vector) = vector else {
            continue;
        };
        let scoring_start = profiling.then(std::time::Instant::now);
        let vector = vector.as_slice();
        if !valid_vector(vector) {
            continue;
        }
        embedded += 1;
        if let Some(query) = &query_direction {
            // The accepted int8 representation ranks normalized F32 directions.
            // Reduction order is fixed within a call, not promised across runtimes.
            let norm = vector
                .iter()
                .map(|value| f32::from(*value).powi(2))
                .sum::<f32>()
                .sqrt();
            let cosine: f32 = query
                .iter()
                .zip(vector)
                .map(|(a, b)| a * (f32::from(*b) / norm))
                .sum();
            ensure!(cosine.is_finite(), crate::search_vectors::InvalidVector);
            let scored = VectorScore {
                key: &candidate.key,
                cosine,
                ordinal,
            };
            if vector_scores.len() < CANDIDATE_DEPTH {
                vector_scores.push(scored);
            } else if let Some(mut worst) = vector_scores.peek_mut()
                && scored < *worst
            {
                *worst = scored;
            }
        }
        if let Some(start) = scoring_start {
            scoring_time += start.elapsed();
        }
    }
    if profiling {
        crate::search_model::SearchPhaseTimer::record("vector_reads", lookup_time);
        crate::search_model::SearchPhaseTimer::record("vector_scoring", scoring_time);
    }
    check()?;
    let _fusion = crate::search_model::SearchPhaseTimer::start("fusion");
    for (index, scored) in vector_scores.into_sorted_vec().into_iter().enumerate() {
        let hit = fused
            .entry(scored.key.clone())
            .or_insert_with(|| RankedHit {
                key: scored.key.clone(),
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
    fn bulk_snapshot_matches_scalar_order_coverage_and_fusion() {
        let directory = tempfile::tempdir().unwrap();
        let model = format!("{:064x}", 1);
        let other_model = format!("{:064x}", 2);
        let mut writer =
            crate::search_vectors::VectorCacheWriter::open(&directory.path().join("vectors.db"))
                .unwrap();
        let mut candidates: Vec<_> = (1..=205)
            .rev()
            .map(|index| candidate(index, index % 11))
            .collect();
        candidates[0].key.syntax_id = candidates[1].key.syntax_id.clone();
        candidates[0].card_hash = candidates[1].card_hash.clone();
        for hash in 0..11 {
            writer
                .put(&model, &format!("{hash:064x}"), &vector(hash - 5, 6 - hash))
                .unwrap();
        }
        // Neither another model nor an unrelated current-model row enters the head.
        writer
            .put(&other_model, &candidates[0].card_hash, &vector(127, -128))
            .unwrap();
        writer
            .put(&model, &format!("{:064x}", 999), &vector(127, -128))
            .unwrap();
        let lexical: Vec<_> = candidates.iter().take(4).map(|c| c.key.clone()).collect();
        let snapshot = writer.snapshot().unwrap();
        let query = vector(-128, 127);
        let expected = rank(
            &candidates,
            &lexical,
            &mut |hash| snapshot.get(&model, hash),
            Some(&query),
            &mut || Ok(()),
        )
        .unwrap();
        let actual = rank_snapshot(
            &candidates,
            &lexical,
            Some(&snapshot),
            Some(&model),
            Some(&query),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(actual.embedded, candidates.len() as u64);
        assert_eq!(actual.hits, expected.hits);
        let lexical_only = rank_lexical(&candidates, &lexical, &mut || Ok(())).unwrap();
        assert_eq!(lexical_only.embedded, 0);
        assert_eq!(lexical_only.hits.len(), lexical.len());
        assert!(
            lexical_only
                .hits
                .iter()
                .all(|hit| hit.vector_rank.is_none())
        );
        let counted = rank_snapshot(
            &candidates,
            &lexical,
            Some(&snapshot),
            Some(&model),
            None,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(counted.embedded, actual.embedded);
        assert_eq!(counted.hits, lexical_only.hits);
    }

    #[test]
    fn bulk_identity_checks_and_cancel_preserve_typed_errors() {
        let candidate = candidate(1, 1);
        let mut duplicate = candidate.clone();
        duplicate.card_hash = format!("{:064x}", 2);
        assert!(
            rank_snapshot(
                &[candidate.clone(), duplicate],
                &[],
                None,
                None,
                None,
                &mut || Ok(()),
            )
            .unwrap_err()
            .is::<crate::store::SelectedIntegrity>()
        );
        assert!(
            rank_lexical(
                std::slice::from_ref(&candidate),
                &[candidate.key.clone(), candidate.key.clone()],
                &mut || Ok(()),
            )
            .unwrap_err()
            .is::<crate::store::SelectedIntegrity>()
        );
        assert!(
            rank_lexical(&[], std::slice::from_ref(&candidate.key), &mut || Ok(()))
                .unwrap_err()
                .is::<crate::store::SelectedIntegrity>()
        );
        assert!(
            rank_snapshot(&[], &[], None, None, Some(&[1]), &mut || Ok(()))
                .unwrap_err()
                .is::<crate::search_vectors::InvalidVector>()
        );
        let error = rank_snapshot(&[], &[], None, None, None, &mut || {
            Err(crate::store::topology::StorageBusy.into())
        })
        .unwrap_err();
        assert!(error.is::<crate::store::topology::StorageBusy>());
    }

    #[test]
    fn optimized_cosine_is_bitwise_equal_to_normalized_f32_reference() {
        let mut seed = 7_u32;
        for case in 0..64 {
            let mut next = || {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 24) as u8 as i8
            };
            let mut query: Vec<_> = (0..DIMENSIONS).map(|_| next()).collect();
            let mut document: Vec<_> = (0..DIMENSIONS).map(|_| next()).collect();
            if case == 0 {
                query.fill(0);
                document.fill(0);
                query[..2].copy_from_slice(&[1, 1]);
                document[..2].copy_from_slice(&[1, 1]);
            } else if case == 1 {
                query.fill(-128);
                document.fill(127);
            } else if case == 2 {
                document.clone_from(&query);
            }
            let query = direction(&query);
            let norm = document
                .iter()
                .map(|value| f32::from(*value).powi(2))
                .sum::<f32>()
                .sqrt();
            let expected: f32 = query
                .iter()
                .zip(&document)
                .map(|(a, b)| a * (f32::from(*b) / norm))
                .sum();
            let bytes: Vec<_> = document.iter().map(|value| *value as u8).collect();
            assert_eq!(
                cosine_bytes(&query, &bytes).to_bits(),
                expected.to_bits(),
                "case {case}"
            );
        }
    }

    #[test]
    fn weighted_fusion_records_only_bounded_arm_membership() {
        let candidates: Vec<_> = (1..=205).map(|index| candidate(index, index)).collect();
        let lexical: Vec<_> = candidates
            .iter()
            .map(|candidate| candidate.key.clone())
            .collect();
        let vectors: BTreeMap<_, _> = candidates
            .iter()
            .map(|candidate| (candidate.card_hash.clone(), vector(1, 0)))
            .collect();
        let ranking = rank(
            &candidates,
            &lexical,
            &mut |hash| Ok(vectors.get(hash).cloned()),
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
            &mut |hash| Ok(vectors.get(hash).cloned()),
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
        let ranking = rank(
            &candidates,
            &[],
            &mut |hash| Ok(vectors.get(hash).cloned()),
            Some(&vector(1, 0)),
            &mut || Ok(()),
        )
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
    fn streamed_top_depth_matches_full_sort_with_mixed_scores_and_stable_ties() {
        use std::cell::Cell;

        let mut candidates: Vec<_> = (1..=205)
            .rev()
            .map(|index| candidate(index, index % 7))
            .collect();
        // Same SID and cosine, but distinct document versions: keep input order.
        candidates[0].key.syntax_id = candidates[1].key.syntax_id.clone();
        candidates[0].card_hash = format!("{:064x}", 3);
        candidates[1].card_hash = candidates[0].card_hash.clone();
        let decoded = |hash: &str| {
            let index = i8::from_str_radix(hash, 16).unwrap();
            vector(3 - index, index - 2)
        };
        let query = vector(1, 0);
        let query_direction = direction(&query);
        let mut expected: Vec<_> = candidates
            .iter()
            .map(|candidate| {
                let cosine: f32 = query_direction
                    .iter()
                    .zip(direction(&decoded(&candidate.card_hash)))
                    .map(|(a, b)| a * b)
                    .sum();
                (candidate.key.clone(), cosine)
            })
            .collect();
        expected.sort_by(|(a, a_score), (b, b_score)| {
            b_score
                .total_cmp(a_score)
                .then_with(|| a.syntax_id.as_bytes().cmp(b.syntax_id.as_bytes()))
        });
        expected.truncate(CANDIDATE_DEPTH);
        let lookups = Cell::new(0);
        let checked_lookups = Cell::new(0);
        let ranking = rank(
            &candidates,
            &[],
            &mut |hash| {
                assert_eq!(checked_lookups.get(), lookups.get());
                let index = lookups.get();
                assert_eq!(hash, candidates[index].card_hash);
                lookups.set(index + 1);
                Ok(Some(decoded(hash)))
            },
            Some(&query),
            &mut || {
                checked_lookups.set(lookups.get());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(lookups.get(), candidates.len());
        assert_eq!(ranking.embedded, candidates.len() as u64);
        assert_eq!(ranking.hits.len(), CANDIDATE_DEPTH);
        for (index, (hit, (key, _))) in ranking.hits.iter().zip(expected).enumerate() {
            assert_eq!(hit.key, key);
            assert_eq!(hit.vector_rank, Some(index + 1));
            assert_eq!(hit.lexical_rank, None);
        }
    }

    #[test]
    fn lookup_failure_and_cancellation_do_not_return_partial_results() {
        use std::cell::Cell;

        let candidates: Vec<_> = (1..=3).map(|index| candidate(index, index)).collect();
        let lookups = Cell::new(0);
        let error = rank(
            &candidates,
            &[],
            &mut |_| {
                lookups.set(lookups.get() + 1);
                if lookups.get() == 2 {
                    anyhow::bail!("vector cache failure");
                }
                Ok(Some(vector(1, 0)))
            },
            Some(&vector(1, 0)),
            &mut || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "vector cache failure");
        assert_eq!(lookups.get(), 2);
        lookups.set(0);
        let error = rank(
            &candidates,
            &[],
            &mut |_| {
                lookups.set(lookups.get() + 1);
                Ok(Some(vector(1, 0)))
            },
            Some(&vector(1, 0)),
            &mut || {
                if lookups.get() == 1 {
                    anyhow::bail!("deadline_exceeded");
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "deadline_exceeded");
        assert_eq!(lookups.get(), 1);
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
        let mut looked_up = Vec::new();
        let ranking = rank(
            &candidates,
            &[candidates[3].key.clone()],
            &mut |hash| {
                looked_up.push(hash.to_owned());
                Ok(vectors.get(hash).cloned())
            },
            None,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(looked_up.len(), candidates.len());
        assert_eq!(looked_up[0], looked_up[1]);
        assert_eq!(ranking.embedded, 2);
        assert_eq!(ranking.hits.len(), 1);
        assert_eq!(ranking.hits[0].vector_rank, None);
        assert!(
            rank(
                &candidates,
                &[],
                &mut |_| panic!("invalid query must fail before vector lookup"),
                Some(&[1]),
                &mut || Ok(())
            )
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
                &mut |_| Ok(None),
                None,
                &mut || Ok(())
            )
            .is_err()
        );
        assert!(
            rank(
                &[],
                std::slice::from_ref(&candidate.key),
                &mut |_| Ok(None),
                None,
                &mut || Ok(())
            )
            .is_err()
        );
        let error = rank(&[candidate], &[], &mut |_| Ok(None), None, &mut || {
            anyhow::bail!("deadline_exceeded")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "deadline_exceeded");
    }
}
