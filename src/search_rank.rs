//! Pure, per-call retrieval ranking over the admitted head and decoded vectors.
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

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
            self.offer_score(ordinal, cosine);
        }
    }

    fn offer_score(&mut self, ordinal: usize, cosine: f32) {
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

/// Fuse a lexical arm whose head membership was validated by the admitted store
/// read. Used when the optional vector arm exhausts its budget without a matrix.
pub(crate) fn fuse_lexical(
    lexical: &[SearchCandidateKey],
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SearchRanking> {
    check()?;
    let mut seen = BTreeSet::new();
    let mut hits = Vec::with_capacity(lexical.len().min(CANDIDATE_DEPTH));
    for (index, key) in lexical.iter().take(CANDIDATE_DEPTH).enumerate() {
        check()?;
        ensure!(
            seen.insert(key),
            crate::store::SelectedIntegrity(
                "store_unavailable: duplicate lexical candidate".into()
            )
        );
        hits.push(RankedHit {
            key: key.clone(),
            lexical_rank: Some(index + 1),
            vector_rank: None,
        });
    }
    hits.sort_by(compare_hits);
    check()?;
    Ok(SearchRanking { hits, embedded: 0 })
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

// Rows stay int8; SIMD lanes are independent candidates.
const MATRIX_LANES: usize = 16;

struct MatrixBlock {
    dimensions: Box<[[i8; MATRIX_LANES]]>,
    inverse_norms: [f32; MATRIX_LANES],
    present: u16,
    active: u16,
}

/// Counts for the changed rows only; `present` is added rows with valid vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatrixDeltaStats {
    pub deleted: usize,
    pub added: usize,
    pub present: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MatrixRow {
    version_id: i64,
    syntax_id: [u8; 16],
    card_hash: [u8; 32],
}

fn compact_hex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != N * 2 {
        return None;
    }
    fn nibble(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        }
    }
    let mut bytes = [0; N];
    for (out, pair) in bytes.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        *out = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(bytes)
}

fn expand_hex(bytes: &[u8], prefix: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(prefix.len() + bytes.len() * 2);
    text.push_str(prefix);
    for byte in bytes {
        text.push(HEX[(byte >> 4) as usize] as char);
        text.push(HEX[(byte & 15) as usize] as char);
    }
    text
}

impl MatrixRow {
    fn parse(candidate: &SearchCandidate) -> Result<Self> {
        let syntax_id = candidate
            .key
            .syntax_id
            .strip_prefix("sid:v1:")
            .and_then(compact_hex);
        let card_hash = compact_hex(&candidate.card_hash);
        match (syntax_id, card_hash) {
            (Some(syntax_id), Some(card_hash)) => Ok(Self {
                version_id: candidate.key.version_id,
                syntax_id,
                card_hash,
            }),
            _ => Err(crate::store::SelectedIntegrity(
                "store_unavailable: noncanonical search candidate".into(),
            )
            .into()),
        }
    }

    fn key(&self) -> (i64, [u8; 16]) {
        (self.version_id, self.syntax_id)
    }

    fn candidate(&self) -> SearchCandidate {
        SearchCandidate {
            key: SearchCandidateKey {
                version_id: self.version_id,
                syntax_id: expand_hex(&self.syntax_id, "sid:v1:"),
            },
            card_hash: expand_hex(&self.card_hash, ""),
        }
    }
}

/// Resident matrix with stable integer row IDs. The two indexes contain only
/// u32 IDs; canonical wire strings are decoded only for returned candidates.
pub struct SearchMatrix {
    rows: Vec<MatrixRow>,
    key_ordinals: Vec<u32>,
    hash_ordinals: Vec<u32>,
    free_slots: Vec<u32>,
    blocks: Vec<MatrixBlock>,
    embedded: u64,
}

impl SearchMatrix {
    pub fn new(
        candidates: std::sync::Arc<[SearchCandidate]>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        check()?;
        ensure!(
            candidates.len() <= u32::MAX as usize,
            crate::store::SelectedIntegrity("store_unavailable: matrix capacity overflow".into())
        );
        let count = candidates.len();
        let mut matrix = Self {
            rows: Vec::new(),
            key_ordinals: Vec::new(),
            hash_ordinals: Vec::new(),
            free_slots: Vec::new(),
            blocks: Vec::new(),
            embedded: 0,
        };
        matrix.rows.try_reserve_exact(count)?;
        matrix.key_ordinals.try_reserve_exact(count)?;
        matrix.hash_ordinals.try_reserve_exact(count)?;
        matrix
            .blocks
            .try_reserve_exact(count.div_ceil(MATRIX_LANES))?;
        for candidate in candidates.iter() {
            check()?;
            matrix.rows.push(MatrixRow::parse(candidate)?);
        }
        matrix.key_ordinals.extend(0..count as u32);
        matrix.hash_ordinals.extend(0..count as u32);
        matrix.sort_indexes();
        ensure!(
            matrix.key_ordinals.windows(2).all(|pair| {
                matrix.rows[pair[0] as usize].key() != matrix.rows[pair[1] as usize].key()
            }),
            crate::store::SelectedIntegrity("store_unavailable: duplicate search candidate".into())
        );
        for block_index in 0..count.div_ceil(MATRIX_LANES) {
            check()?;
            let mut block = Self::empty_block()?;
            let lanes = (count - block_index * MATRIX_LANES).min(MATRIX_LANES);
            block.active = ((1_u32 << lanes) - 1) as u16;
            matrix.blocks.push(block);
        }
        check()?;
        Ok(matrix)
    }

    /// Cold loading walks the resident hash index; no second ordinal list or
    /// candidate/vector staging is allocated.
    pub fn from_snapshot(
        candidates: std::sync::Arc<[SearchCandidate]>,
        snapshot: Option<&crate::search_vectors::VectorSnapshot<'_>>,
        model_id: &str,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut matrix = Self::new(candidates, check)?;
        let Some(snapshot) = snapshot else {
            return Ok(matrix);
        };
        let mut head = 0;
        let mut decoded = [0_i8; DIMENSIONS];
        snapshot.scan_model(model_id, &mut |hash, vector| {
            check()?;
            let Some(hash) = hash
                .and_then(|hash| std::str::from_utf8(hash).ok())
                .and_then(compact_hex::<32>)
            else {
                return Ok(());
            };
            while head < matrix.hash_ordinals.len()
                && matrix.rows[matrix.hash_ordinals[head] as usize].card_hash < hash
            {
                check()?;
                head += 1;
            }
            let start = head;
            while head < matrix.hash_ordinals.len()
                && matrix.rows[matrix.hash_ordinals[head] as usize].card_hash == hash
            {
                check()?;
                head += 1;
            }
            let Some(vector) = vector else { return Ok(()) };
            if vector.len() != DIMENSIONS || !vector.iter().any(|value| *value != 0) {
                return Ok(());
            }
            for (value, byte) in decoded.iter_mut().zip(vector) {
                *value = *byte as i8;
            }
            for index in start..head {
                check()?;
                matrix.store_vector(matrix.hash_ordinals[index] as usize, &decoded);
            }
            check()
        })?;
        check()?;
        Ok(matrix)
    }

    fn empty_block() -> Result<MatrixBlock> {
        let mut dimensions = Vec::new();
        dimensions.try_reserve_exact(DIMENSIONS)?;
        dimensions.resize(DIMENSIONS, [0; MATRIX_LANES]);
        Ok(MatrixBlock {
            dimensions: dimensions.into_boxed_slice(),
            inverse_norms: [1.0; MATRIX_LANES],
            present: 0,
            active: 0,
        })
    }

    fn sort_indexes(&mut self) {
        self.key_ordinals
            .sort_unstable_by_key(|row| self.rows[*row as usize].key());
        self.hash_ordinals
            .sort_unstable_by_key(|row| (self.rows[*row as usize].card_hash, *row));
    }

    fn active(&self, ordinal: usize) -> bool {
        ordinal < self.rows.len()
            && self.blocks[ordinal / MATRIX_LANES].active & (1 << (ordinal % MATRIX_LANES)) != 0
    }

    fn present(&self, ordinal: usize) -> bool {
        self.blocks[ordinal / MATRIX_LANES].present & (1 << (ordinal % MATRIX_LANES)) != 0
    }

    pub fn candidate(&self, ordinal: usize) -> Option<SearchCandidate> {
        self.active(ordinal).then(|| self.rows[ordinal].candidate())
    }

    pub fn candidate_for_key(&self, key: &SearchCandidateKey) -> Option<SearchCandidate> {
        self.ordinal(key)
            .map(|ordinal| self.rows[ordinal].candidate())
    }

    pub fn ordinal(&self, key: &SearchCandidateKey) -> Option<usize> {
        let sid = key
            .syntax_id
            .strip_prefix("sid:v1:")
            .and_then(compact_hex::<16>)?;
        self.key_ordinals
            .binary_search_by_key(&(key.version_id, sid), |row| self.rows[*row as usize].key())
            .ok()
            .map(|index| self.key_ordinals[index] as usize)
    }

    pub fn active_count(&self) -> usize {
        self.key_ordinals.len()
    }

    pub fn slot_count(&self) -> usize {
        self.rows.len()
    }

    pub fn embedded_count(&self) -> u64 {
        self.embedded
    }

    pub fn missing_hashes(&self) -> impl Iterator<Item = String> + '_ {
        let mut previous = None;
        self.hash_ordinals.iter().filter_map(move |ordinal| {
            let ordinal = *ordinal as usize;
            let hash = self.rows[ordinal].card_hash;
            if self.present(ordinal) || previous == Some(hash) {
                return None;
            }
            previous = Some(hash);
            Some(expand_hex(&hash, ""))
        })
    }

    /// Exact owned payload capacity, including tombstones and SIMD padding.
    /// Allocator metadata and the separately admitted caller slice are not owned.
    pub fn storage_bytes(&self) -> usize {
        self.rows.capacity() * std::mem::size_of::<MatrixRow>()
            + (self.key_ordinals.capacity()
                + self.hash_ordinals.capacity()
                + self.free_slots.capacity())
                * std::mem::size_of::<u32>()
            + self.blocks.capacity() * std::mem::size_of::<MatrixBlock>()
            + self.blocks.len() * DIMENSIONS * MATRIX_LANES
    }

    /// Resident bytes after cold construction. Canonical IDs always have fixed
    /// widths; source string bytes belong to the caller, not the matrix.
    pub fn estimated_new_bytes(candidates: &[SearchCandidate]) -> usize {
        Self::estimated_new_bytes_for_count(candidates.len())
    }

    pub fn estimated_new_bytes_for_count(count: usize) -> usize {
        Self::estimated_new_bytes_for_counts(count, 0, 0)
    }

    pub fn estimated_new_bytes_for_counts(
        count: usize,
        _syntax_id_bytes: usize,
        _card_hash_bytes: usize,
    ) -> usize {
        count
            .saturating_mul(std::mem::size_of::<MatrixRow>() + 2 * std::mem::size_of::<u32>())
            .saturating_add(
                count
                    .div_ceil(MATRIX_LANES)
                    .saturating_mul(DIMENSIONS * MATRIX_LANES + std::mem::size_of::<MatrixBlock>()),
            )
    }

    /// Additional cold-build scratch, beyond resident bytes and the caller's
    /// candidate slice, is only one decoded vector. Sorting is in place.
    pub fn estimated_new_build_bytes_for_counts(
        count: usize,
        syntax_id_bytes: usize,
        card_hash_bytes: usize,
    ) -> usize {
        Self::estimated_new_bytes_for_counts(count, syntax_id_bytes, card_hash_bytes)
            .saturating_add(DIMENSIONS)
    }

    /// Additional peak reservation while the existing matrix stays live. Source
    /// candidates, the caller's prepared vectors, compact validation staging,
    /// new blocks and sequential realloc overlaps are all bounded here.
    pub fn estimated_delta_candidate_bytes(
        &self,
        removed_vids: &[i64],
        added: &[SearchCandidate],
    ) -> usize {
        let mut deleted = 0;
        // No allocation in admission. Duplicate/unsorted removals may overcount,
        // so only use their count to reserve free slots, never to discount growth.
        for vid in removed_vids {
            let start = self
                .key_ordinals
                .partition_point(|row| self.rows[*row as usize].version_id < *vid);
            let end = self
                .key_ordinals
                .partition_point(|row| self.rows[*row as usize].version_id <= *vid);
            deleted += end - start;
        }
        let reusable = if removed_vids.windows(2).all(|pair| pair[0] < pair[1]) {
            deleted
        } else {
            0
        };
        let extra = added
            .len()
            .saturating_sub(self.free_slots.len().saturating_add(reusable));
        let slots = self.rows.len().saturating_add(extra);
        let active = self.active_count().saturating_add(added.len());
        let blocks = slots.div_ceil(MATRIX_LANES);
        let free = self.free_slots.len().saturating_add(deleted);
        let mut growth = blocks
            .saturating_sub(self.blocks.len())
            .saturating_mul(DIMENSIONS * MATRIX_LANES);
        let mut overlap = 0;
        for (required, capacity, width) in [
            (
                slots,
                self.rows.capacity(),
                std::mem::size_of::<MatrixRow>(),
            ),
            (
                active,
                self.key_ordinals.capacity(),
                std::mem::size_of::<u32>(),
            ),
            (
                active,
                self.hash_ordinals.capacity(),
                std::mem::size_of::<u32>(),
            ),
            (free, self.free_slots.capacity(), std::mem::size_of::<u32>()),
            (
                blocks,
                self.blocks.capacity(),
                std::mem::size_of::<MatrixBlock>(),
            ),
        ] {
            if required > capacity {
                growth = growth.saturating_add((required - capacity).saturating_mul(width));
                overlap = overlap.max(capacity.saturating_mul(width));
            }
        }
        let strings = added.iter().fold(0_usize, |total, candidate| {
            total
                .saturating_add(candidate.key.syntax_id.capacity())
                .saturating_add(candidate.card_hash.capacity())
        });
        growth
            .saturating_add(overlap)
            .saturating_add(
                removed_vids
                    .len()
                    .saturating_mul(std::mem::size_of::<i64>()),
            )
            .saturating_add(added.len().saturating_mul(
                std::mem::size_of::<MatrixRow>()
                    + std::mem::size_of::<u32>()
                    + std::mem::size_of::<(SearchCandidate, Option<Vec<i8>>)>()
                    + DIMENSIONS,
            ))
            .saturating_add(strings)
            .saturating_add(
                blocks
                    .saturating_sub(self.blocks.len())
                    .saturating_mul(std::mem::size_of::<MatrixBlock>()),
            )
            // One streamed missing lookup, hash cursor and decoded vector.
            .saturating_add(DIMENSIONS + 64)
    }

    fn hash_range(&self, hash: &[u8; 32]) -> std::ops::Range<usize> {
        let start = self
            .hash_ordinals
            .partition_point(|row| self.rows[*row as usize].card_hash < *hash);
        let end = self
            .hash_ordinals
            .partition_point(|row| self.rows[*row as usize].card_hash <= *hash);
        start..end
    }

    fn store_vector(&mut self, ordinal: usize, values: &[i8]) {
        let norm = values
            .iter()
            .map(|value| f32::from(*value).powi(2))
            .sum::<f32>()
            .sqrt();
        let block = &mut self.blocks[ordinal / MATRIX_LANES];
        let lane = ordinal % MATRIX_LANES;
        block.inverse_norms[lane] = 1.0 / norm;
        for (dimension, value) in block.dimensions.iter_mut().zip(values) {
            dimension[lane] = *value;
        }
        let bit = 1_u16 << lane;
        if block.present & bit == 0 {
            self.embedded += 1;
        }
        block.present |= bit;
    }

    /// Invalid vectors remain misses; tombstone ordinals are rejected.
    pub fn insert(&mut self, ordinal: usize, values: &[i8]) -> Result<bool> {
        ensure!(
            self.active(ordinal),
            crate::store::SelectedIntegrity(
                "store_unavailable: matrix ordinal outside admitted head".into()
            )
        );
        if !valid_vector(values) {
            return Ok(false);
        }
        self.store_vector(ordinal, values);
        Ok(true)
    }

    /// Only changed rows are staged. Every error/cancellation precedes the
    /// synchronous commit; unchanged row IDs and vector allocations stay intact.
    pub fn apply_delta(
        &mut self,
        removed_vids: &[i64],
        added: Vec<(SearchCandidate, Option<Vec<i8>>)>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<MatrixDeltaStats> {
        check()?;
        let mut removed = Vec::new();
        removed.try_reserve_exact(removed_vids.len())?;
        removed.extend_from_slice(removed_vids);
        removed.sort_unstable();
        removed.dedup();
        let mut deleted = 0;
        for ordinal in &self.key_ordinals {
            check()?;
            deleted += usize::from(
                removed
                    .binary_search(&self.rows[*ordinal as usize].version_id)
                    .is_ok(),
            );
        }
        let mut prepared = Vec::new();
        prepared.try_reserve_exact(added.len())?;
        let mut order = Vec::new();
        order.try_reserve_exact(added.len())?;
        ensure!(
            added.len() <= u32::MAX as usize,
            crate::store::SelectedIntegrity("store_unavailable: matrix capacity overflow".into())
        );
        let mut present = 0;
        for (candidate, vector) in &added {
            check()?;
            let row = MatrixRow::parse(candidate)?;
            ensure!(
                self.ordinal(&candidate.key).is_none()
                    || removed.binary_search(&row.version_id).is_ok(),
                crate::store::SelectedIntegrity(
                    "store_unavailable: duplicate search candidate".into()
                )
            );
            prepared.push(row);
            present += usize::from(vector.as_deref().is_some_and(valid_vector));
        }
        order.extend(0..added.len() as u32);
        order.sort_unstable_by_key(|index| prepared[*index as usize].key());
        ensure!(
            order
                .windows(2)
                .all(|pair| prepared[pair[0] as usize].key() != prepared[pair[1] as usize].key()),
            crate::store::SelectedIntegrity("store_unavailable: duplicate search candidate".into())
        );
        let extra = added.len().saturating_sub(self.free_slots.len() + deleted);
        let slots = self
            .rows
            .len()
            .checked_add(extra)
            .filter(|slots| *slots <= u32::MAX as usize)
            .ok_or_else(|| {
                crate::store::SelectedIntegrity(
                    "store_unavailable: matrix capacity overflow".into(),
                )
            })?;
        self.rows.try_reserve_exact(extra)?;
        // Reserve before removing old index entries so publication cannot allocate.
        self.key_ordinals.try_reserve_exact(added.len())?;
        self.hash_ordinals.try_reserve_exact(added.len())?;
        self.free_slots.try_reserve_exact(deleted)?;
        let block_count = slots
            .div_ceil(MATRIX_LANES)
            .saturating_sub(self.blocks.len());
        self.blocks.try_reserve_exact(block_count)?;
        let mut new_blocks = Vec::new();
        new_blocks.try_reserve_exact(block_count)?;
        for _ in 0..block_count {
            check()?;
            new_blocks.push(Self::empty_block()?);
        }
        check()?;
        // Publication: no callbacks or recoverable failures below this point.
        self.blocks.extend(new_blocks);
        for ordinal in &self.key_ordinals {
            let ordinal = *ordinal as usize;
            if removed
                .binary_search(&self.rows[ordinal].version_id)
                .is_ok()
            {
                let bit = 1 << (ordinal % MATRIX_LANES);
                let block = &mut self.blocks[ordinal / MATRIX_LANES];
                if block.present & bit != 0 {
                    self.embedded -= 1;
                }
                block.present &= !bit;
                block.active &= !bit;
                self.free_slots.push(ordinal as u32);
            }
        }
        self.key_ordinals.retain(|ordinal| {
            removed
                .binary_search(&self.rows[*ordinal as usize].version_id)
                .is_err()
        });
        self.hash_ordinals.retain(|ordinal| {
            removed
                .binary_search(&self.rows[*ordinal as usize].version_id)
                .is_err()
        });
        let added_count = added.len();
        for (row, (_, vector)) in prepared.into_iter().zip(added) {
            let ordinal = if let Some(ordinal) = self.free_slots.pop() {
                self.rows[ordinal as usize] = row;
                ordinal as usize
            } else {
                let ordinal = self.rows.len();
                self.rows.push(row);
                ordinal
            };
            self.blocks[ordinal / MATRIX_LANES].active |= 1 << (ordinal % MATRIX_LANES);
            self.key_ordinals.push(ordinal as u32);
            self.hash_ordinals.push(ordinal as u32);
            if let Some(vector) = vector.filter(|values| valid_vector(values)) {
                self.store_vector(ordinal, &vector);
            }
        }
        self.sort_indexes();
        Ok(MatrixDeltaStats {
            deleted,
            added: added_count,
            present,
        })
    }

    /// A hash-index cursor advances through absent and shared hashes alike.
    /// Only one cache lookup/vector and one decoded hash are live at a time.
    pub fn refresh_missing(
        &mut self,
        snapshot: Option<&crate::search_vectors::VectorSnapshot<'_>>,
        model_id: &str,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Some(snapshot) = snapshot else {
            return Ok(());
        };
        let mut index = 0;
        while index < self.hash_ordinals.len() {
            check()?;
            let start = index;
            let hash = self.rows[self.hash_ordinals[index] as usize].card_hash;
            let mut missing = false;
            while index < self.hash_ordinals.len()
                && self.rows[self.hash_ordinals[index] as usize].card_hash == hash
            {
                check()?;
                missing |= !self.present(self.hash_ordinals[index] as usize);
                index += 1;
            }
            if missing && let Some(vector) = snapshot.get(model_id, &expand_hex(&hash, ""))? {
                if !valid_vector(&vector) {
                    continue;
                }
                for _ in start..index {
                    check()?;
                }
                check()?;
                for offset in start..index {
                    let ordinal = self.hash_ordinals[offset] as usize;
                    if !self.present(ordinal) {
                        self.store_vector(ordinal, &vector);
                    }
                }
            }
        }
        check()
    }

    /// Fill only current missing rows. The two-pass operation allocates no
    /// staging, and every error/cancellation precedes publication.
    pub fn fill(
        &mut self,
        vectors: &[(String, Vec<i8>)],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        check()?;
        for (hash, vector) in vectors {
            check()?;
            if valid_vector(vector)
                && let Some(hash) = compact_hex(hash)
            {
                for _ in self.hash_range(&hash) {
                    check()?;
                }
            }
        }
        check()?;
        let mut filled = 0;
        for (hash, vector) in vectors {
            if valid_vector(vector)
                && let Some(hash) = compact_hex(hash)
            {
                for index in self.hash_range(&hash) {
                    let ordinal = self.hash_ordinals[index] as usize;
                    if !self.present(ordinal) {
                        self.store_vector(ordinal, vector);
                        filled += 1;
                    }
                }
            }
        }
        Ok(filled)
    }

    /// Publish the last valid committed value for each hash, including already
    /// present rows. Staging is compact and bounded by the caller's batch size.
    pub fn update_vectors(
        &mut self,
        vectors: &[(String, Vec<i8>)],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        check()?;
        let mut values = Vec::new();
        values.try_reserve_exact(vectors.len())?;
        for (index, (hash, vector)) in vectors.iter().enumerate() {
            check()?;
            if valid_vector(vector)
                && let Some(hash) = compact_hex::<32>(hash)
            {
                values.push((hash, index));
            }
        }
        values.sort_unstable();
        for (hash, _) in &values {
            for _ in self.hash_range(hash) {
                check()?;
            }
        }
        check()?;
        let mut updated = 0;
        for index in 0..values.len() {
            let (hash, vector) = values[index];
            if values.get(index + 1).is_some_and(|next| next.0 == hash) {
                continue;
            }
            for offset in self.hash_range(&hash) {
                self.store_vector(self.hash_ordinals[offset] as usize, &vectors[vector].1);
                updated += 1;
            }
        }
        Ok(updated)
    }

    fn visit_direction(
        &self,
        query: &[f32],
        check: &mut impl FnMut() -> Result<()>,
        visitor: &mut impl FnMut(usize, f32) -> Result<()>,
    ) -> Result<()> {
        for (index, block) in self.blocks.iter().enumerate() {
            check()?;
            if block.present == 0 {
                continue;
            }
            let scores = matrix_scores(query, block);
            for (lane, cosine) in scores.into_iter().enumerate() {
                if block.present & (1_u16 << lane) != 0 {
                    ensure!(cosine.is_finite(), crate::search_vectors::InvalidVector);
                    visitor(index * MATRIX_LANES + lane, cosine)?;
                }
            }
        }
        check()
    }
}

#[cfg(not(target_arch = "aarch64"))]
fn matrix_scores(query: &[f32], block: &MatrixBlock) -> [f32; MATRIX_LANES] {
    matrix_scores_scalar(query, block)
}

#[cfg(any(test, not(target_arch = "aarch64")))]
fn matrix_scores_scalar(query: &[f32], block: &MatrixBlock) -> [f32; MATRIX_LANES] {
    let mut scores = [-0.0; MATRIX_LANES];
    for (query, dimension) in query.iter().zip(block.dimensions.iter()) {
        for (score, value) in scores.iter_mut().zip(dimension) {
            *score += query * f32::from(*value);
        }
    }
    for (score, inverse) in scores.iter_mut().zip(&block.inverse_norms) {
        *score *= inverse;
    }
    scores
}

#[cfg(target_arch = "aarch64")]
fn matrix_scores(query: &[f32], block: &MatrixBlock) -> [f32; MATRIX_LANES] {
    use std::arch::aarch64::*;

    let mut scores = [-0.0; MATRIX_LANES];
    // SAFETY: every dimension, inverse_norms and scores has exactly 16 lanes;
    // loads and stores remain within those arrays. NEON is baseline on aarch64.
    unsafe {
        let mut s0 = vdupq_n_f32(-0.0);
        let mut s1 = s0;
        let mut s2 = s0;
        let mut s3 = s0;
        for (query, dimension) in query.iter().zip(block.dimensions.iter()) {
            let bytes = vld1q_s8(dimension.as_ptr());
            let low = vmovl_s8(vget_low_s8(bytes));
            let high = vmovl_high_s8(bytes);
            let d0 = vcvtq_f32_s32(vmovl_s16(vget_low_s16(low)));
            let d1 = vcvtq_f32_s32(vmovl_high_s16(low));
            let d2 = vcvtq_f32_s32(vmovl_s16(vget_low_s16(high)));
            let d3 = vcvtq_f32_s32(vmovl_high_s16(high));
            let q = vdupq_n_f32(*query);
            // Separate multiplication/addition, sequential dimensions per row.
            s0 = vaddq_f32(s0, vmulq_f32(q, d0));
            s1 = vaddq_f32(s1, vmulq_f32(q, d1));
            s2 = vaddq_f32(s2, vmulq_f32(q, d2));
            s3 = vaddq_f32(s3, vmulq_f32(q, d3));
        }
        s0 = vmulq_f32(s0, vld1q_f32(block.inverse_norms.as_ptr()));
        s1 = vmulq_f32(s1, vld1q_f32(block.inverse_norms.as_ptr().add(4)));
        s2 = vmulq_f32(s2, vld1q_f32(block.inverse_norms.as_ptr().add(8)));
        s3 = vmulq_f32(s3, vld1q_f32(block.inverse_norms.as_ptr().add(12)));
        vst1q_f32(scores.as_mut_ptr(), s0);
        vst1q_f32(scores.as_mut_ptr().add(4), s1);
        vst1q_f32(scores.as_mut_ptr().add(8), s2);
        vst1q_f32(scores.as_mut_ptr().add(12), s3);
    }
    scores
}

// Heap entries contain integer row IDs and fixed-width SID tie keys, never
// candidate strings. Larger members are worse retained vector results.
#[derive(Eq, PartialEq)]
struct MatrixVectorScore {
    row: u32,
    syntax_id: [u8; 16],
    cosine_bits: u32,
}

impl PartialOrd for MatrixVectorScore {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MatrixVectorScore {
    fn cmp(&self, other: &Self) -> Ordering {
        f32::from_bits(other.cosine_bits)
            .total_cmp(&f32::from_bits(self.cosine_bits))
            .then_with(|| self.syntax_id.cmp(&other.syntax_id))
            .then_with(|| self.row.cmp(&other.row))
    }
}

struct MatrixFusedHit {
    row: u32,
    lexical_rank: Option<usize>,
    vector_rank: Option<usize>,
}

impl MatrixFusedHit {
    fn score(&self) -> f64 {
        self.lexical_rank
            .map_or(0.0, |rank| LEXICAL_WEIGHT / (RRF_K + rank as f64))
            + self
                .vector_rank
                .map_or(0.0, |rank| VECTOR_WEIGHT / (RRF_K + rank as f64))
    }
}

/// Rank one coherent published matrix. Both arms and fusion retain integer row
/// IDs; only the bounded final ranking is decoded to wire IDs.
pub fn rank_matrix(
    matrix: &SearchMatrix,
    lexical: &[SearchCandidateKey],
    query: Option<&[i8]>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SearchRanking> {
    let _phase = crate::search_model::SearchPhaseTimer::start("ranking_total");
    check()?;
    ensure!(
        query.is_none_or(valid_vector),
        crate::search_vectors::InvalidVector
    );
    let mut fused: Vec<MatrixFusedHit> = Vec::with_capacity(2 * CANDIDATE_DEPTH);
    for (index, key) in lexical.iter().take(CANDIDATE_DEPTH).enumerate() {
        check()?;
        let row = matrix.ordinal(key).ok_or_else(|| {
            crate::store::SelectedIntegrity(
                "store_unavailable: lexical candidate outside admitted head".into(),
            )
        })? as u32;
        ensure!(
            !fused.iter().any(|hit| hit.row == row),
            crate::store::SelectedIntegrity(
                "store_unavailable: duplicate lexical candidate".into()
            )
        );
        fused.push(MatrixFusedHit {
            row,
            lexical_rank: Some(index + 1),
            vector_rank: None,
        });
    }
    let mut scores = BinaryHeap::with_capacity(CANDIDATE_DEPTH);
    if let Some(query) = query {
        let query = direction(query);
        let _scoring = crate::search_model::SearchPhaseTimer::start("vector_scoring");
        matrix.visit_direction(&query, check, &mut |ordinal, cosine| {
            let scored = MatrixVectorScore {
                row: ordinal as u32,
                syntax_id: matrix.rows[ordinal].syntax_id,
                cosine_bits: cosine.to_bits(),
            };
            if scores.len() < CANDIDATE_DEPTH {
                scores.push(scored);
            } else if let Some(mut worst) = scores.peek_mut()
                && scored < *worst
            {
                *worst = scored;
            }
            Ok(())
        })?;
    }
    check()?;
    let _fusion = crate::search_model::SearchPhaseTimer::start("fusion");
    for (index, scored) in scores.into_sorted_vec().into_iter().enumerate() {
        if let Some(hit) = fused.iter_mut().find(|hit| hit.row == scored.row) {
            hit.vector_rank = Some(index + 1);
        } else {
            fused.push(MatrixFusedHit {
                row: scored.row,
                lexical_rank: None,
                vector_rank: Some(index + 1),
            });
        }
    }
    fused.sort_by(|a, b| {
        b.score()
            .total_cmp(&a.score())
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
            .then_with(|| {
                matrix.rows[a.row as usize]
                    .syntax_id
                    .cmp(&matrix.rows[b.row as usize].syntax_id)
            })
            // The old BTreeMap's stable sort used key order when all contract
            // tie-breaks were equal (the same SID at distinct versions).
            .then_with(|| {
                matrix.rows[a.row as usize]
                    .version_id
                    .cmp(&matrix.rows[b.row as usize].version_id)
            })
    });
    let hits = fused
        .into_iter()
        .map(|hit| RankedHit {
            key: SearchCandidateKey {
                version_id: matrix.rows[hit.row as usize].version_id,
                syntax_id: expand_hex(&matrix.rows[hit.row as usize].syntax_id, "sid:v1:"),
            },
            lexical_rank: hit.lexical_rank,
            vector_rank: hit.vector_rank,
        })
        .collect();
    check()?;
    Ok(SearchRanking {
        hits,
        embedded: matrix.embedded,
    })
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
    fn resident_capacity_matches_estimate_with_missing_and_complete_vectors() {
        assert_eq!(std::mem::size_of::<MatrixRow>(), 56);
        for count in [1, 16, 17, 257, 1024] {
            let cards: Vec<_> = (0..count).map(|i| candidate(i, i)).collect();
            let mut matrix = SearchMatrix::new(cards.clone().into(), &mut || Ok(())).unwrap();
            let estimate =
                SearchMatrix::estimated_new_bytes_for_counts(count, count * 39, count * 64);
            let missing = matrix.storage_bytes();
            assert_eq!(estimate, missing);
            for ordinal in 0..count {
                matrix.insert(ordinal, &vector(1, 2)).unwrap();
            }
            assert_eq!(matrix.storage_bytes(), missing);
            assert_eq!(matrix.embedded_count(), count as u64);
            assert!(estimate.abs_diff(matrix.storage_bytes()) * 10 <= estimate);
        }
        let large = 756_140;
        let resident = SearchMatrix::estimated_new_bytes_for_count(large);
        assert!(resident < 850_000_000, "LARGE resident bytes: {resident}");
        assert!(resident > large * DIMENSIONS);
    }

    #[test]
    fn matrix_rejects_noncanonical_identity_without_string_fallback() {
        for (sid, hash) in [
            ("sid:v1:1".into(), format!("{:064x}", 1)),
            (format!("sid:v1:{:032x}", 1), "hash".into()),
            (format!("sid:v1:{:032X}", 255), format!("{:064x}", 1)),
        ] {
            let card = SearchCandidate {
                key: SearchCandidateKey {
                    version_id: 1,
                    syntax_id: sid,
                },
                card_hash: hash,
            };
            let result = SearchMatrix::new([card].into(), &mut || Ok(()));
            assert!(
                result
                    .err()
                    .unwrap()
                    .is::<crate::store::SelectedIntegrity>()
            );
        }
    }

    #[test]
    fn integer_matrix_fusion_matches_reference_with_shared_sid_versions() {
        let mut cards: Vec<_> = (0..CANDIDATE_DEPTH + 5)
            .rev()
            .map(|i| candidate(i, i % 7))
            .collect();
        cards[0].key.syntax_id = cards[1].key.syntax_id.clone();
        cards[0].card_hash = cards[1].card_hash.clone();
        let mut matrix = SearchMatrix::new(cards.clone().into(), &mut || Ok(())).unwrap();
        let values = vector(1, 0);
        for ordinal in 0..cards.len() {
            matrix.insert(ordinal, &values).unwrap();
        }
        let lexical: Vec<_> = cards.iter().take(7).map(|card| card.key.clone()).collect();
        let expected = rank(
            &cards,
            &lexical,
            &mut |_| Ok(Some(values.clone())),
            Some(&values),
            &mut || Ok(()),
        )
        .unwrap();
        let actual = rank_matrix(&matrix, &lexical, Some(&values), &mut || Ok(())).unwrap();
        assert_eq!(actual.embedded, expected.embedded);
        assert_eq!(actual.hits, expected.hits);
    }

    #[test]
    fn cold_estimate_covers_physical_layout_and_large_card_count() {
        for count in [0, 1, 10, 11, 16, 17, 257] {
            let cards: Vec<_> = (0..count).map(|i| candidate(i, i)).collect();
            let estimate = SearchMatrix::estimated_new_bytes(&cards);
            let mut matrix = SearchMatrix::new(cards.clone().into(), &mut || Ok(())).unwrap();
            assert!(estimate >= matrix.storage_bytes(), "missing rows: {count}");
            for ordinal in 0..count {
                matrix.insert(ordinal, &vector(1, 2)).unwrap();
            }
            assert!(estimate >= matrix.storage_bytes(), "present rows: {count}");
        }
        // LARGE is a card count, not a byte-derived count. Exercise arithmetic,
        // not a gigabyte-sized allocation in the ordinary test suite.
        let large = 75_614 * 10;
        let exemplar = candidate(0, 0);
        assert_eq!(exemplar.key.syntax_id.len(), 39);
        assert_eq!(exemplar.card_hash.len(), 64);
        let input = large * (2 * std::mem::size_of::<SearchCandidate>() + 39 + 64) + 16;
        let peak = SearchMatrix::estimated_new_bytes_for_count(large) + input;
        assert!(peak < 2 * 1024 * 1024 * 1024, "LARGE peak: {peak}");
    }

    #[test]
    fn delta_reserves_transient_reallocation_of_identity_buffers_not_old_vectors() {
        let candidates: std::sync::Arc<[_]> = (0..64)
            .map(|index| candidate(index, index))
            .collect::<Vec<_>>()
            .into();
        let mut matrix = SearchMatrix::new(candidates, &mut || Ok(())).unwrap();
        for ordinal in 0..64 {
            matrix.insert(ordinal, &vector(1, 2)).unwrap();
        }
        let appended = candidate(1000, 1000);
        let reserve = matrix.estimated_delta_candidate_bytes(&[], std::slice::from_ref(&appended));
        let identity_buffer = matrix.rows.capacity() * std::mem::size_of::<MatrixRow>();
        let before = matrix.storage_bytes();
        let replacement =
            matrix.estimated_delta_candidate_bytes(&[0], std::slice::from_ref(&appended));
        assert!(replacement < reserve);
        matrix
            .apply_delta(&[], vec![(appended, Some(vector(1, 2)))], &mut || Ok(()))
            .unwrap();
        let growth = matrix.storage_bytes().saturating_sub(before);
        assert!(reserve >= growth + identity_buffer);
        assert!(reserve < before);
    }

    #[test]
    fn incremental_delta_reservation_covers_reuse_growth_and_duplicate_removals() {
        for (removed, additions) in [(vec![1], 1), (vec![], 19), (vec![1, 1], 19)] {
            let original: Vec<_> = (0..17).map(|i| candidate(i, i)).collect();
            let mut matrix = SearchMatrix::new(original.into(), &mut || Ok(())).unwrap();
            for ordinal in 0..17 {
                matrix.insert(ordinal, &vector(1, 2)).unwrap();
            }
            let added: Vec<_> = (100..100 + additions).map(|i| candidate(i, i)).collect();
            let old = matrix.storage_bytes();
            let increment = matrix.estimated_delta_candidate_bytes(&removed, &added);
            let prepared: Vec<_> = added
                .into_iter()
                .map(|card| (card, Some(vector(1, 2))))
                .collect();
            matrix
                .apply_delta(&removed, prepared, &mut || Ok(()))
                .unwrap();
            assert!(old + increment >= matrix.storage_bytes());
            if additions == 1 {
                assert_eq!(matrix.slot_count(), 17);
                assert!(
                    increment < old / 2,
                    "in-place delta reserved old matrix: {increment}/{old}"
                );
            }
        }
    }

    #[test]
    fn delta_refresh_reservation_does_not_charge_present_rows_or_all_missing_vectors() {
        let cards: Vec<_> = (0..257).map(|i| candidate(i, i)).collect();
        let mut matrix = SearchMatrix::new(cards.into(), &mut || Ok(())).unwrap();
        let missing = matrix.estimated_delta_candidate_bytes(&[], &[]);
        for ordinal in 0..257 {
            matrix.insert(ordinal, &vector(1, 2)).unwrap();
        }
        let present = matrix.estimated_delta_candidate_bytes(&[], &[]);
        assert!(missing < 4 * DIMENSIONS);
        assert!(present < 4 * DIMENSIONS);
        assert!(missing - present <= 3 * 64);
    }

    #[test]
    fn missing_refresh_streams_past_cache_misses_and_shared_hashes() {
        let directory = tempfile::tempdir().unwrap();
        let model = format!("{:064x}", 1);
        let mut writer =
            crate::search_vectors::VectorCacheWriter::open(&directory.path().join("vectors.db"))
                .unwrap();
        let cards = [
            candidate(0, 0),
            candidate(1, 1),
            candidate(2, 1),
            candidate(3, 2),
        ];
        writer
            .put(&model, &cards[1].card_hash, &vector(1, 2))
            .unwrap();
        let snapshot = writer.snapshot().unwrap();
        let mut matrix = SearchMatrix::new(cards.clone().into(), &mut || Ok(())).unwrap();
        matrix
            .refresh_missing(Some(&snapshot), &model, &mut || Ok(()))
            .unwrap();
        assert_eq!(matrix.embedded_count(), 2);
        assert_eq!(
            matrix.missing_hashes().collect::<Vec<_>>(),
            [cards[0].card_hash.as_str(), cards[3].card_hash.as_str()]
        );
    }

    #[test]
    fn admitted_lexical_fallback_is_bounded_and_rejects_duplicates() {
        let keys: Vec<_> = (0..CANDIDATE_DEPTH + 1)
            .map(|index| candidate(index, index).key)
            .collect();
        let ranking = fuse_lexical(&keys, &mut || Ok(())).unwrap();
        assert_eq!(ranking.embedded, 0);
        assert_eq!(ranking.hits.len(), CANDIDATE_DEPTH);
        for (index, hit) in ranking.hits.iter().enumerate() {
            assert_eq!(hit.key, keys[index]);
            assert_eq!(hit.lexical_rank, Some(index + 1));
            assert_eq!(hit.vector_rank, None);
        }
        assert!(
            fuse_lexical(&[keys[0].clone(), keys[0].clone()], &mut || Ok(()))
                .unwrap_err()
                .is::<crate::store::SelectedIntegrity>()
        );
        assert!(fuse_lexical(&keys, &mut || anyhow::bail!("cancelled")).is_err());
    }

    #[test]
    fn matrix_delta_reuses_slots_and_keeps_unchanged_vectors() {
        let original = [candidate(0, 0), candidate(1, 1), candidate(2, 2)];
        let mut matrix = SearchMatrix::new(original.clone().into(), &mut || Ok(())).unwrap();
        for ordinal in 0..3 {
            matrix
                .insert(ordinal, &vector(ordinal as i8 + 1, 1))
                .unwrap();
        }
        let unchanged = matrix.ordinal(&original[2].key).unwrap();
        let before = matrix.blocks[0].dimensions[0][unchanged];
        let inverse = matrix.blocks[0].inverse_norms[unchanged];
        for index in 3..23 {
            let replacement = candidate(index, index);
            let removed = if index == 3 { 1 } else { index as i64 - 1 };
            let stats = matrix
                .apply_delta(
                    &[removed],
                    vec![(replacement, Some(vector(1, 2)))],
                    &mut || Ok(()),
                )
                .unwrap();
            assert_eq!(
                stats,
                MatrixDeltaStats {
                    deleted: 1,
                    added: 1,
                    present: 1
                }
            );
            assert_eq!(matrix.active_count(), 3);
            assert_eq!(matrix.slot_count(), 3);
            assert_eq!(matrix.embedded_count(), 3);
        }
        assert_eq!(matrix.blocks[0].dimensions[0][unchanged], before);
        assert_eq!(matrix.blocks[0].inverse_norms[unchanged], inverse);
        assert!(matrix.storage_bytes() >= 3 * DIMENSIONS);
    }

    #[test]
    fn matrix_vid_removal_and_late_fill_do_not_resurrect_reused_slots() {
        let first = candidate(0, 4);
        let mut sibling = candidate(1, 4);
        sibling.key.version_id = first.key.version_id;
        let mut matrix =
            SearchMatrix::new([first.clone(), sibling.clone()].into(), &mut || Ok(())).unwrap();
        let replacement = candidate(2, 5);
        let stats = matrix
            .apply_delta(
                &[first.key.version_id],
                vec![(replacement.clone(), None)],
                &mut || Ok(()),
            )
            .unwrap();
        assert_eq!(stats.deleted, 2);
        assert_eq!(matrix.active_count(), 1);
        assert!(matrix.candidate_for_key(&sibling.key).is_none());
        assert_eq!(
            matrix
                .fill(&[(first.card_hash, vector(1, 2))], &mut || Ok(()))
                .unwrap(),
            0
        );
        assert_eq!(matrix.embedded_count(), 0);
        assert_eq!(
            matrix
                .fill(&[(replacement.card_hash, vector(1, 2))], &mut || Ok(()))
                .unwrap(),
            1
        );
        assert_eq!(matrix.embedded_count(), 1);
    }

    #[test]
    fn matrix_malformed_vectors_are_misses_and_shared_hash_fill_counts_candidates() {
        let cards = [candidate(0, 4), candidate(1, 4), candidate(2, 5)];
        let mut matrix = SearchMatrix::new(cards.clone().into(), &mut || Ok(())).unwrap();
        assert!(!matrix.insert(0, &[1]).unwrap());
        assert!(!matrix.insert(1, &vec![0; DIMENSIONS]).unwrap());
        assert_eq!(matrix.embedded_count(), 0);
        assert_eq!(
            matrix
                .fill(
                    &[(cards[0].card_hash.clone(), vector(-128, 127))],
                    &mut || Ok(())
                )
                .unwrap(),
            2
        );
        assert_eq!(matrix.embedded_count(), 2);
        let ranking = rank_matrix(
            &matrix,
            std::slice::from_ref(&cards[2].key),
            Some(&vector(1, 1)),
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(ranking.embedded, 2);
        assert_eq!(ranking.hits.len(), 3);
        assert!(
            matrix
                .insert(100, &vector(1, 1))
                .unwrap_err()
                .is::<crate::store::SelectedIntegrity>()
        );
    }

    #[test]
    fn matrix_delta_and_fill_cancellation_leave_published_state_unchanged() {
        let original = candidate(0, 0);
        let mut matrix = SearchMatrix::new([original.clone()].into(), &mut || Ok(())).unwrap();
        let replacement = candidate(1, 1);
        // Force cancellation after preflight has inspected keys and reserved capacity.
        let mut checks = 0;
        let error = matrix
            .apply_delta(&[0], vec![(replacement, Some(vector(1, 2)))], &mut || {
                checks += 1;
                anyhow::ensure!(checks < 4, "cancelled");
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        assert_eq!(matrix.active_count(), 1);
        assert_eq!(matrix.embedded_count(), 0);
        assert!(matrix.candidate_for_key(&original.key).is_some());
        let mut checks = 0;
        assert!(
            matrix
                .fill(&[(original.card_hash.clone(), vector(1, 2))], &mut || {
                    checks += 1;
                    anyhow::ensure!(checks < 4, "cancelled");
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(matrix.embedded_count(), 0);
        assert_eq!(
            matrix.missing_hashes().collect::<Vec<_>>(),
            vec![original.card_hash.as_str()]
        );
        assert!(
            matrix
                .apply_delta(&[], vec![(original.clone(), None)], &mut || Ok(()))
                .is_err()
        );
        assert_eq!(matrix.active_count(), 1);
    }

    #[test]
    fn matrix_committed_hash_updates_replace_present_and_fill_missing_atomically() {
        let cards = [candidate(0, 4), candidate(1, 4), candidate(2, 5)];
        let mut matrix = SearchMatrix::new(cards.clone().into(), &mut || Ok(())).unwrap();
        matrix.insert(0, &vector(1, 0)).unwrap();
        let batch = [(cards[0].card_hash.clone(), vector(0, 1))];
        let mut checks = 0;
        assert!(
            matrix
                .update_vectors(&batch, &mut || {
                    checks += 1;
                    anyhow::ensure!(checks < 5, "cancelled");
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(matrix.embedded_count(), 1);
        assert_eq!(matrix.blocks[0].dimensions[0][0], 1);
        assert_eq!(matrix.update_vectors(&batch, &mut || Ok(())).unwrap(), 2);
        assert_eq!(matrix.embedded_count(), 2);
        assert_eq!(matrix.blocks[0].dimensions[0][0], 0);
        assert_eq!(matrix.blocks[0].dimensions[1][0], 1);
        assert_eq!(
            matrix.missing_hashes().collect::<Vec<_>>(),
            vec![cards[2].card_hash.as_str()]
        );
        matrix.apply_delta(&[0, 1], vec![], &mut || Ok(())).unwrap();
        assert_eq!(matrix.update_vectors(&batch, &mut || Ok(())).unwrap(), 0);
        assert_eq!(matrix.embedded_count(), 0);
    }

    #[test]
    fn matrix_rank_rejects_invalid_query_and_lexical_identity() {
        let card = candidate(0, 0);
        let matrix = SearchMatrix::new([card.clone()].into(), &mut || Ok(())).unwrap();
        assert!(
            rank_matrix(&matrix, &[], Some(&[1]), &mut || Ok(()))
                .unwrap_err()
                .is::<crate::search_vectors::InvalidVector>()
        );
        assert!(
            rank_matrix(&matrix, &[candidate(1, 1).key], None, &mut || Ok(()))
                .unwrap_err()
                .is::<crate::store::SelectedIntegrity>()
        );
        assert!(
            rank_matrix(&matrix, &[card.key.clone(), card.key], None, &mut || Ok(()))
                .unwrap_err()
                .is::<crate::store::SelectedIntegrity>()
        );
    }

    #[test]
    fn matrix_top_depth_and_sid_ties_remain_bounded() {
        let cards: Vec<_> = (0..CANDIDATE_DEPTH + 3)
            .map(|index| candidate(index, index))
            .collect();
        let mut matrix = SearchMatrix::new(cards.clone().into(), &mut || Ok(())).unwrap();
        for ordinal in 0..cards.len() {
            matrix.insert(ordinal, &vector(1, 2)).unwrap();
        }
        let ranking = rank_matrix(&matrix, &[], Some(&vector(1, 2)), &mut || Ok(())).unwrap();
        assert_eq!(ranking.hits.len(), CANDIDATE_DEPTH);
        assert_eq!(ranking.embedded, cards.len() as u64);
        for (index, hit) in ranking.hits.iter().enumerate() {
            assert_eq!(hit.key, cards[index].key);
            assert_eq!(hit.vector_rank, Some(index + 1));
        }
    }

    #[test]
    fn matrix_simd_and_scalar_follow_stored_inverse_norm_formula() {
        let cards: Vec<_> = (0..19).map(|index| candidate(index, index)).collect();
        let mut matrix = SearchMatrix::new(cards.into(), &mut || Ok(())).unwrap();
        for ordinal in 0..19 {
            let values: Vec<_> = (0..DIMENSIONS)
                .map(|dimension| ((ordinal * 17 + dimension * 31) % 256) as u8 as i8)
                .collect();
            matrix.insert(ordinal, &values).unwrap();
        }
        let query = direction(&vector(1, 3));
        for block in &matrix.blocks {
            let actual = matrix_scores(&query, block);
            let reference = matrix_scores_scalar(&query, block);
            for (lane, (actual, reference)) in actual.iter().zip(reference).enumerate() {
                if block.present & (1 << lane) != 0 {
                    assert!(actual.is_finite());
                    assert!((*actual - reference).abs() <= 1e-6);
                }
            }
        }
        let ranking = rank_matrix(&matrix, &[], Some(&vector(1, 3)), &mut || Ok(())).unwrap();
        assert_eq!(ranking.embedded, 19);
        assert_eq!(ranking.hits.len(), 19);
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
