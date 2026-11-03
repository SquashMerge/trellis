//! Decision 0018 ranking over synthetic, deterministic, already-retrieved arms.
//! No model, cache database, network, or daemon is involved.
use std::collections::BTreeMap;

use trellis::{
    native_evidence::DeclarationRole,
    search_rank::{
        SearchCandidate, SearchCandidateKey, SearchMatrix, SearchPolicy, SearchRole, rank_matrix,
    },
};

const DIMENSIONS: usize = 1024;

#[derive(Clone, Debug)]
struct Row {
    role: DeclarationRole,
    exact: bool,
    lexical: bool,
    lexical_order: u32,
    // None is a missing vector. Otherwise the cosine is exactly 1, 0 or -1.
    vector: Option<i8>,
}

fn key(i: usize) -> SearchCandidateKey {
    SearchCandidateKey {
        version_id: (i % 3) as i64 + 1,
        syntax_id: format!("sid:v1:{i:032x}"),
    }
}

fn accepts(filter: SearchRole, role: DeclarationRole) -> bool {
    match filter {
        SearchRole::Any => true,
        SearchRole::Production => role == DeclarationRole::Production,
        SearchRole::Test => role == DeclarationRole::Test,
    }
}

fn vector(class: i8) -> Vec<i8> {
    let mut result = vec![0; DIMENSIONS];
    if class == 0 {
        result[1] = 1;
    } else {
        result[0] = class;
    }
    result
}

// Straightforward list-based model: no production ranking helpers or heap.
// Lexical input is pre-ranked by the store; generate a consistent lexical tier
// order using synthetic BM25 positions and additive exact membership.
fn verify(rows: &[Row], filter: SearchRole, vector_enabled: bool) {
    let candidates: Vec<_> = rows
        .iter()
        .enumerate()
        .rev()
        .map(|(i, row)| SearchCandidate {
            key: key(i),
            card_hash: format!("{i:064x}"),
            role: row.role,
        })
        .collect();
    let mut lexical: Vec<_> = (0..rows.len())
        .filter(|&i| accepts(filter, rows[i].role) && (rows[i].lexical || rows[i].exact))
        .collect();
    // Exact-only rows have no BM25 score; use SID only to order those ties.
    lexical.sort_by_key(|&i| {
        (
            !rows[i].exact,
            rows[i].role != DeclarationRole::Production,
            !rows[i].lexical,
            if rows[i].lexical {
                rows[i].lexical_order
            } else {
                0
            },
            i,
        )
    });
    let mut policy = SearchPolicy {
        exact: BTreeMap::new(),
        role: filter,
    };
    for (i, row) in rows.iter().enumerate() {
        if row.exact && accepts(filter, row.role) {
            policy.exact.insert(key(i), row.role);
        }
    }
    let lexical_keys: Vec<_> = lexical.iter().map(|&i| key(i)).collect();
    let mut matrix = SearchMatrix::new(candidates.into(), &mut || Ok(())).unwrap();
    for (i, row) in rows.iter().enumerate() {
        if let Some(class) = row.vector {
            assert!(
                matrix
                    .insert(matrix.ordinal(&key(i)).unwrap(), &vector(class))
                    .unwrap()
            );
        }
    }
    let query = vector(1);
    let actual = rank_matrix(
        &matrix,
        &lexical_keys,
        vector_enabled.then_some(query.as_slice()),
        &policy,
        &mut || Ok(()),
    )
    .unwrap();

    let mut l = vec![None; rows.len()];
    for (rank, &i) in lexical.iter().take(200).enumerate() {
        l[i] = Some(rank + 1);
    }
    let mut vector_ids: Vec<_> = (0..rows.len())
        .filter(|&i| vector_enabled && rows[i].vector.is_some() && accepts(filter, rows[i].role))
        .collect();
    vector_ids.sort_by_key(|&i| (std::cmp::Reverse(rows[i].vector.unwrap()), i));
    let mut v = vec![None; rows.len()];
    for (rank, &i) in vector_ids.iter().take(200).enumerate() {
        v[i] = Some(rank + 1);
    }
    let mut expected: Vec<_> = (0..rows.len())
        .filter(|&i| l[i].is_some() || v[i].is_some())
        .collect();
    let score = |i: usize| -> f64 {
        l[i].map_or(0.0, |rank| 1.0 / (20.0 + rank as f64))
            + v[i].map_or(0.0, |rank| 4.0 / (20.0 + rank as f64))
    };
    let tier = |i: usize| match (rows[i].exact, rows[i].role) {
        (true, DeclarationRole::Production) => 0,
        (true, DeclarationRole::Test) => 1,
        _ => 2,
    };
    expected.sort_by(|&a, &b| {
        tier(a)
            .cmp(&tier(b))
            .then_with(|| score(b).total_cmp(&score(a)))
            .then_with(|| {
                l[a].into_iter()
                    .chain(v[a])
                    .min()
                    .cmp(&l[b].into_iter().chain(v[b]).min())
            })
            .then_with(|| l[a].unwrap_or(usize::MAX).cmp(&l[b].unwrap_or(usize::MAX)))
            .then_with(|| a.cmp(&b))
    });
    let observed: Vec<_> = actual
        .hits
        .iter()
        .map(|hit| (hit.key.clone(), hit.lexical_rank, hit.vector_rank))
        .collect();
    let modeled: Vec<_> = expected.iter().map(|&i| (key(i), l[i], v[i])).collect();
    assert_eq!(
        observed, modeled,
        "filter={filter:?}, vector={vector_enabled}, rows={rows:?}"
    );
    assert_eq!(
        actual.embedded,
        rows.iter().filter(|row| row.vector.is_some()).count() as u64
    );
}

// SplitMix64 gives reproducible independent fixture construction without hidden RNG state.
fn next(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e3779b97f4a7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}

fn generated(seed: u64, n: usize) -> Vec<Row> {
    let mut state = seed;
    (0..n)
        .map(|_| Row {
            role: if next(&mut state) & 1 == 0 {
                DeclarationRole::Production
            } else {
                DeclarationRole::Test
            },
            exact: next(&mut state).is_multiple_of(5),
            lexical: next(&mut state) & 1 == 0,
            lexical_order: (next(&mut state) % 6) as u32,
            vector: match next(&mut state) % 4 {
                0 => None,
                1 => Some(-1),
                2 => Some(0),
                _ => Some(1),
            },
        })
        .collect()
}

#[test]
fn generated_overlaps_tiers_and_vector_ties() {
    let cases: u64 = std::env::var("TRELLIS_HYBRID_CASES")
        .ok()
        .map(|s| s.parse().unwrap())
        .unwrap_or(32);
    let mut overlaps = 0;
    let mut exacts = 0;
    let mut lexical_only = 0;
    let mut vector_only = 0;
    for seed in 0..cases {
        let rows = generated(seed + 20261102, (seed as usize * 13) % 36);
        overlaps += rows
            .iter()
            .filter(|r| (r.lexical || r.exact) && r.vector.is_some())
            .count();
        exacts += rows.iter().filter(|r| r.exact).count();
        lexical_only += rows
            .iter()
            .filter(|r| (r.lexical || r.exact) && r.vector.is_none())
            .count();
        vector_only += rows
            .iter()
            .filter(|r| !r.lexical && !r.exact && r.vector.is_some())
            .count();
        for filter in [SearchRole::Any, SearchRole::Production, SearchRole::Test] {
            verify(&rows, filter, true);
            verify(&rows, filter, false);
        }
    }
    eprintln!(
        "cases={cases}, overlaps={overlaps}, exacts={exacts}, lexical_only={lexical_only}, vector_only={vector_only}"
    );
}

#[test]
fn depth_and_fusion_tie_controls() {
    // With 225 rows, both arm cutoffs split a large equal-cosine group.
    let rows = generated(12345, 225);
    for filter in [SearchRole::Any, SearchRole::Production, SearchRole::Test] {
        verify(&rows, filter, true);
    }
    let rows: Vec<_> = (0..225)
        .map(|i| Row {
            role: DeclarationRole::Production,
            exact: false,
            lexical: true,
            lexical_order: (224 - i) as u32,
            vector: Some(1),
        })
        .collect();
    verify(&rows, SearchRole::Any, true);
    // Each role has 210 vectors, so filtering must happen before vector depth.
    let rows: Vec<_> = (0..420)
        .map(|i| Row {
            role: if i % 2 == 0 {
                DeclarationRole::Production
            } else {
                DeclarationRole::Test
            },
            exact: false,
            lexical: true,
            lexical_order: i as u32,
            vector: Some(1),
        })
        .collect();
    for filter in [SearchRole::Production, SearchRole::Test] {
        verify(&rows, filter, true);
    }
    // 1/(20+1) equals 4/(20+64) in binary64; smaller arm rank wins.
    let rows: Vec<_> = (0..64)
        .map(|i| Row {
            role: DeclarationRole::Production,
            exact: false,
            lexical: i == 63,
            lexical_order: 0,
            vector: (i != 63).then_some(1),
        })
        .chain(std::iter::once(Row {
            role: DeclarationRole::Production,
            exact: false,
            lexical: false,
            lexical_order: 0,
            vector: Some(1),
        }))
        .collect();
    verify(&rows, SearchRole::Any, true);
}
