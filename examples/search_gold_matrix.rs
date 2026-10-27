//! Corrected #214 relevance replay using saved CPU or Metal raw F32 output.
//! No model loading, inference, network, latency or publication-performance claims.
//!
//! Run once per backend with --backend cpu|metal --docs DOCS.jsonl
//! --queries QUERIES.jsonl --lexical LEXICAL.json --output NEW_DIRECTORY.
//! Both backends must contain all corrected 50 official queries. Evaluate exact
//! Fraction MRR from first_gold_rank outside Rust; summary F64 is diagnostic only.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use trellis::{
    model::IndexPin,
    search_embed::quantize,
    search_rank::{
        SearchCandidate, SearchCandidateKey, SearchMatrix, SearchRanking, rank_matrix,
        rank_snapshot,
    },
    search_vectors::{VectorCacheReader, VectorCacheWriter},
};

#[derive(Clone, Copy, Debug, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
enum Backend {
    Cpu,
    Metal,
}
#[derive(Parser)]
struct Args {
    /// Receipt label for the saved raw inputs, not a request for inference.
    #[arg(long, value_enum)]
    backend: Backend,
    #[arg(long)]
    docs: PathBuf,
    #[arg(long)]
    queries: PathBuf,
    #[arg(long)]
    lexical: PathBuf,
    /// New directory; existing output is refused.
    #[arg(long)]
    output: PathBuf,
}
#[derive(Deserialize)]
struct FloatRow {
    id: String,
    vector: Vec<f32>,
}
#[derive(Deserialize)]
struct LexicalCard {
    id: String,
}
#[derive(Deserialize)]
struct Query {
    id: String,
    category: String,
    query: String,
    answers: Vec<String>,
    bm25: Vec<String>,
}
#[derive(Deserialize)]
struct Lexical {
    cards: Vec<LexicalCard>,
    queries: Vec<Query>,
}
#[derive(Serialize)]
struct Outcome {
    id: String,
    category: String,
    first_gold_rank: Option<usize>,
    hit5: bool,
    hit10: bool,
    reciprocal_rank: f64,
    vector_top200: Vec<String>,
    top100: Vec<String>,
}
fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    serde_json::to_writer_pretty(&mut file, value)?;
    writeln!(file)?;
    file.flush()?;
    Ok(())
}
fn vectors(path: &Path) -> Result<BTreeMap<String, Vec<i8>>> {
    let mut result = BTreeMap::new();
    for line in BufReader::new(File::open(path)?).lines() {
        let row: FloatRow = serde_json::from_str(&line?)?;
        ensure!(
            row.vector.len() == 2048,
            "expected full diagnostic F32 vector"
        );
        let value = quantize(&row.vector)?.values;
        ensure!(
            value.len() == 1024 && value.iter().any(|v| *v != 0),
            "invalid quantized vector"
        );
        ensure!(
            result.insert(row.id, value).is_none(),
            "duplicate vector ID"
        );
    }
    Ok(result)
}
fn ranking_json(ranking: &SearchRanking) -> Value {
    json!({"embedded": ranking.embedded, "hits": ranking.hits.iter().map(|hit| json!({
        "key": {"version_id": hit.key.version_id, "syntax_id": hit.key.syntax_id},
        "lexical_rank": hit.lexical_rank, "vector_rank": hit.vector_rank,
    })).collect::<Vec<_>>()})
}
fn outcome(query: &Query, vector: &SearchRanking, ranking: &SearchRanking) -> Outcome {
    let top100: Vec<_> = ranking
        .hits
        .iter()
        .take(100)
        .map(|hit| hit.key.syntax_id.clone())
        .collect();
    let first = top100
        .iter()
        .position(|sid| query.answers.contains(sid))
        .map(|i| i + 1);
    Outcome {
        id: query.id.clone(),
        category: query.category.clone(),
        first_gold_rank: first,
        hit5: first.is_some_and(|rank| rank <= 5),
        hit10: first.is_some_and(|rank| rank <= 10),
        reciprocal_rank: first.map_or(0.0, |rank| 1.0 / rank as f64),
        vector_top200: vector
            .hits
            .iter()
            .map(|hit| hit.key.syntax_id.clone())
            .collect(),
        top100,
    }
}
fn metrics(rows: &[Outcome]) -> Value {
    let categories: BTreeSet<_> = rows.iter().map(|row| row.category.as_str()).collect();
    let mut output = serde_json::Map::new();
    for category in std::iter::once("all").chain(categories) {
        let selected: Vec<_> = rows
            .iter()
            .filter(|row| category == "all" || row.category == category)
            .collect();
        let n = selected.len();
        let h5 = selected.iter().filter(|row| row.hit5).count();
        let h10 = selected.iter().filter(|row| row.hit10).count();
        let mut reciprocals: Vec<_> = selected.iter().map(|row| row.reciprocal_rank).collect();
        reciprocals.sort_by(f64::total_cmp);
        let mrr = reciprocals.iter().sum::<f64>() / n as f64;
        output.insert(category.into(), json!({"n":n, "success5":h5, "success10":h10,
            "recall@5":h5 as f64/n as f64, "recall@10":h10 as f64/n as f64, "mrr":mrr,
            "missesAt10":selected.iter().filter(|row| !row.hit10).map(|row| &row.id).collect::<Vec<_>>() }));
    }
    Value::Object(output)
}
// Retain detailed per-query gold ranks alongside the established Outcome JSONL shape.
fn query_report(query: &Query, values: &[i8], ranking: &SearchRanking) -> Value {
    let top100: Vec<_> = ranking.hits.iter().take(100).collect();
    let gold_ranks: Vec<_> = query
        .answers
        .iter()
        .map(|answer| {
            json!({"syntax_id":answer,"rank":top100.iter()
            .position(|hit| &hit.key.syntax_id == answer).map(|index| index + 1)})
        })
        .collect();
    let first = top100
        .iter()
        .position(|hit| query.answers.contains(&hit.key.syntax_id))
        .map(|index| index + 1);
    json!({
        "id":query.id,"category":query.category,"text":query.query,"answers":query.answers,
        "query_vector_sha256":digest(&values.iter().map(|v| *v as u8).collect::<Vec<_>>()),
        "first_gold_rank":first,"gold_ranks":gold_ranks,
        "missed_at_10":first.is_none_or(|rank| rank > 10),
        "top100":top100.iter().map(|hit| json!({
            "key":{"version_id":hit.key.version_id,"syntax_id":hit.key.syntax_id},
            "lexical_rank":hit.lexical_rank,"vector_rank":hit.vector_rank,
        })).collect::<Vec<_>>(),
    })
}
fn evaluation(rows: &[Outcome], reports: &[Value]) -> Value {
    let summary = metrics(rows);
    let mut categories = summary.as_object().unwrap().clone();
    let all = categories.remove("all").unwrap();
    let mut queries = reports.to_vec();
    queries.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    json!({"all":all,"per_category":categories,
        "misses10":queries.iter().filter(|query| query["missed_at_10"] == true)
            .map(|query| &query["id"]).collect::<Vec<_>>(),"queries":queries})
}
const REPLAY_README: &str = r#"# Production SearchMatrix corrected #214 gold replay

Run the already-built `search_gold_matrix` example once per saved raw backend:

```
search_gold_matrix --backend cpu --docs CPU_DOCS.jsonl --queries CPU_QUERIES.jsonl --lexical CORRECTED_LEXICAL.json --output NEW_CPU_DIR
search_gold_matrix --backend metal --docs METAL_DOCS.jsonl --queries METAL_QUERIES.jsonl --lexical CORRECTED_LEXICAL.json --output NEW_METAL_DIR
```

The backend flag labels provenance; it never loads a model. Input rows are
`{"id":"...","vector":[2048 F32 values]}`. Lexical schema is `cards:[{id}],
queries:[{id,category,query,answers,bm25}]`. All three input SHA256s must match the
saved backend's corrected official fixture. There are 6722 cards, 50 queries,
82 answer slots and 73 distinct gold SIDs. IDs and answer joins are checked.

Both sides use production `search_embed::quantize`. `current_*` calls public
`rank_snapshot` with the saved lexical top200 or an empty lexical arm.
`matrix_cold_*` ranks the actual cold `SearchMatrix::from_snapshot` result.
`matrix_*` ranks that same matrix after a two-VID identity-preserving delta,
including one temporary hole, and a committed same-byte cache batch published
through `update_vectors` to both a missing and a present candidate. All ranking
uses production `rank_matrix`, including its own hybrid fusion. No custom scorer,
prototype engine or rank-equality gate is used.

Each Outcome JSONL retains first_gold_rank, hit5/hit10 and ranked SID lists.
Report JSON also retains per-category summaries and detailed per-query gold ranks.
The parent computes exact rational MRR@100 from first_gold_rank (zero for missing),
not diagnostic F64 metrics: sum(Fraction(1, rank) for nonmissing ranks) / 50.
For CPU AND Metal, require each matrix hybrid R@5, R@10 and exact MRR to be at
least both its fresh `current_hybrid` control and the frozen historical hybrid:
20/50, 31/50, 30600636226641/107637010943000. Vector-only results are reported
alongside historical 24/50, 30/50, 1826786986109/6171453288000; they are not an
additional quality gate. No epsilon or bit-identical ranking gate applies.

Pin, ordinal+1 version IDs, SHA256(SID) card hashes and an arbitrary SHA256 model
identity are fixture-only, not published product identities. Saved raw vectors
come from voyage-4-nano revision 5fb34e870f49f19e79c94d5e118f5ee730133279.
This replay claims no inference, latency, publication, owner lifecycle or
concurrency coverage. No ranking, inference, budget or residency constants change.
"#;

fn main() -> Result<()> {
    let args = Args::parse();
    let (docs_sha256, queries_sha256) = match args.backend {
        Backend::Cpu => (
            "b1af9408c696d3cbae2798273e82f6a8ad1f316ebfd670467b7d3fd12fc97c32",
            "385540a2f656a4b122d07c9724f6b05367642583a600bd94123e369856e7bca9",
        ),
        Backend::Metal => (
            "22eacf88900bea9efd9021093b8ec2ce829bd47aeb4c1f617a7dd68c8398d823",
            "b8842c3fcbe1e00abaa4460a8d2fce3b2d6993cf8c715de016201fa05112a39b",
        ),
    };
    let mut input_hashes = BTreeMap::new();
    for (name, path, expected) in [
        ("docs", &args.docs, docs_sha256),
        ("queries", &args.queries, queries_sha256),
        (
            "lexical",
            &args.lexical,
            "910d4d51cf2ea8fca3df6b934c60426d096f148862390d6b8a055d2b0054cdb3",
        ),
    ] {
        let actual = digest(&fs::read(path)?);
        ensure!(
            actual == expected,
            "{name} does not match corrected #214 fixture"
        );
        input_hashes.insert(name, json!({"path":path,"sha256":actual}));
    }
    ensure!(!args.output.exists(), "output must not exist");
    let lexical: Lexical = serde_json::from_slice(&fs::read(&args.lexical)?)?;
    let documents = vectors(&args.docs)?;
    let queries = vectors(&args.queries)?;
    let candidate_ids: BTreeSet<_> = lexical.cards.iter().map(|card| card.id.as_str()).collect();
    let query_ids: BTreeSet<_> = lexical
        .queries
        .iter()
        .map(|query| query.id.as_str())
        .collect();
    ensure!(
        lexical.cards.len() == 6722 && candidate_ids.len() == 6722,
        "gold candidate count"
    );
    ensure!(
        lexical.queries.len() == 50 && query_ids.len() == 50,
        "gold query count"
    );
    ensure!(
        documents
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>()
            == candidate_ids,
        "document ID join"
    );
    ensure!(
        queries.keys().map(String::as_str).collect::<BTreeSet<_>>() == query_ids,
        "query ID join"
    );
    ensure!(
        lexical
            .queries
            .iter()
            .map(|query| query.answers.len())
            .sum::<usize>()
            == 82,
        "gold answer slot count"
    );
    ensure!(
        lexical
            .queries
            .iter()
            .flat_map(|query| &query.answers)
            .collect::<BTreeSet<_>>()
            .len()
            == 73,
        "gold declaration count"
    );
    for query in &lexical.queries {
        ensure!(
            !query.answers.is_empty()
                && query.answers.iter().all(|sid| documents.contains_key(sid)),
            "missing gold answer"
        );
        ensure!(
            query.bm25.iter().collect::<BTreeSet<_>>().len() == query.bm25.len(),
            "duplicate lexical candidate"
        );
        ensure!(
            query.bm25.iter().all(|sid| documents.contains_key(sid)),
            "lexical ID join"
        );
    }
    fs::create_dir_all(&args.output)?;
    let output = fs::canonicalize(&args.output)?;
    let model_id = digest(b"trellis-358-production-matrix-gold-fixture-not-product-identity");
    let pin: IndexPin = serde_json::from_value(
        json!({"indexGeneration":"00000000-0000-4000-8000-000000000358", "indexRevision":1}),
    )?;
    let cards: Arc<[SearchCandidate]> = documents
        .keys()
        .enumerate()
        .map(|(ordinal, sid)| SearchCandidate {
            key: SearchCandidateKey {
                version_id: (ordinal + 1) as i64,
                syntax_id: sid.clone(),
            },
            card_hash: digest(sid.as_bytes()),
        })
        .collect::<Vec<_>>()
        .into();
    let key_by_id: BTreeMap<_, _> = cards
        .iter()
        .map(|card| (card.key.syntax_id.as_str(), &card.key))
        .collect();
    let entries: Vec<_> = cards
        .iter()
        .map(|card| {
            (
                model_id.as_str(),
                card.card_hash.as_str(),
                documents[&card.key.syntax_id].as_slice(),
            )
        })
        .collect();
    let db_path = output.join("vectors.db");
    let mut writer = VectorCacheWriter::open(&db_path)?;
    writer.put_many(&entries)?;
    drop(writer);
    let mut reader =
        VectorCacheReader::open_existing(&db_path)?.context("new fixture cache missing")?;
    let snapshot = reader.snapshot()?;
    ensure!(snapshot.entry_count() == 6722, "fixture cache count");
    let mut matrix =
        SearchMatrix::from_snapshot(cards.clone(), Some(&snapshot), &model_id, &mut || Ok(()))?;
    ensure!(
        matrix.active_count() == cards.len() && matrix.embedded_count() == 6722,
        "cold matrix coverage"
    );
    drop(snapshot);
    drop(reader);
    let mut cold_rankings = Vec::new();
    for query in &lexical.queries {
        let lexical_keys: Vec<_> = query
            .bm25
            .iter()
            .take(200)
            .map(|sid| (*key_by_id[sid.as_str()]).clone())
            .collect();
        let values = &queries[&query.id];
        cold_rankings.push((
            rank_matrix(&matrix, &[], Some(values), &mut || Ok(()))?,
            rank_matrix(&matrix, &lexical_keys, Some(values), &mut || Ok(()))?,
        ));
    }
    // Two existing VIDs return with identical keys and hashes. One temporarily
    // lacks bytes so the committed cache update covers holes and replacements.
    let changed: Vec<_> = cards.iter().take(2).cloned().collect();
    let delta = matrix.apply_delta(
        &changed
            .iter()
            .map(|card| card.key.version_id)
            .collect::<Vec<_>>(),
        vec![
            (changed[0].clone(), None),
            (
                changed[1].clone(),
                Some(documents[&changed[1].key.syntax_id].clone()),
            ),
        ],
        &mut || Ok(()),
    )?;
    ensure!(
        delta.deleted == 2 && delta.added == 2 && delta.present == 1,
        "matrix fixture delta coverage"
    );
    ensure!(
        matrix.active_count() == cards.len()
            && matrix.embedded_count() == 6721
            && matrix.missing_hashes().count() == 1,
        "matrix fixture hole coverage"
    );
    let committed_entries: Vec<_> = changed
        .iter()
        .map(|card| {
            (
                model_id.as_str(),
                card.card_hash.as_str(),
                documents[&card.key.syntax_id].as_slice(),
            )
        })
        .collect();
    let mut writer = VectorCacheWriter::open(&db_path)?;
    writer.put_many(&committed_entries)?;
    drop(writer);
    let mut reader =
        VectorCacheReader::open_existing(&db_path)?.context("committed fixture cache missing")?;
    let snapshot = reader.snapshot()?;
    let mut committed = Vec::new();
    for card in &changed {
        let values = snapshot
            .get(&model_id, &card.card_hash)?
            .context("committed fixture vector missing")?;
        ensure!(
            values == documents[&card.key.syntax_id],
            "fixture byte preservation"
        );
        committed.push((card.card_hash.clone(), values));
    }
    let updated = matrix.update_vectors(&committed, &mut || Ok(()))?;
    ensure!(
        updated == 2
            && matrix.active_count() == cards.len()
            && matrix.embedded_count() == 6722
            && matrix.missing_hashes().count() == 0,
        "committed cache update coverage"
    );
    for card in cards.iter() {
        ensure!(
            matrix.candidate_for_key(&card.key).as_ref() == Some(card),
            "fixture identity preservation"
        );
    }
    let raw: Vec<u8> = cards
        .iter()
        .flat_map(|card| documents[&card.key.syntax_id].iter().map(|v| *v as u8))
        .collect();
    fs::write(output.join("vectors.i8"), &raw)?;
    fs::write(output.join("valid.u8"), vec![1_u8; cards.len()])?;
    let mut bitmap = vec![255_u8; cards.len().div_ceil(8)];
    if !cards.len().is_multiple_of(8) {
        *bitmap.last_mut().unwrap() = (1 << (cards.len() % 8)) - 1;
    }
    fs::write(output.join("present.bitmap"), &bitmap)?;
    let metadata = json!({"pin":pin,"model_id":model_id,"rows":cards.len(),"valid_vectors":cards.len(),"dimensions":1024,
        "candidates":cards.iter().map(|card| json!({"version_id":card.key.version_id,"syntax_id":card.key.syntax_id,"card_hash":card.card_hash})).collect::<Vec<_>>(),
        "scope":"frozen214_corrected50_gold_relevance_not_timing", "backend":args.backend, "model_revision":"5fb34e870f49f19e79c94d5e118f5ee730133279",
        "identity_policy":"synthetic pin, ordinal+1 VIDs, SHA256(SID) cache keys, arbitrary SHA256 model ID; not published product identity"});
    write_json(&output.join("metadata.json"), &metadata)?;
    let mut frozen_queries = BufWriter::new(File::create(output.join("queries.jsonl"))?);
    let mut query_bytes = Vec::new();
    let mut records: BTreeMap<String, Vec<Outcome>> = BTreeMap::new();
    let mut detailed: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (ordinal, (query, (cold_vector, cold_hybrid))) in
        lexical.queries.iter().zip(cold_rankings).enumerate()
    {
        let values = &queries[&query.id];
        query_bytes.extend(values.iter().map(|v| *v as u8));
        let lexical_keys: Vec<_> = query
            .bm25
            .iter()
            .take(200)
            .map(|sid| (*key_by_id[sid.as_str()]).clone())
            .collect();
        let baseline = rank_snapshot(
            &cards,
            &[],
            Some(&snapshot),
            Some(&model_id),
            Some(values),
            &mut || Ok(()),
        )?;
        let baseline_hybrid = rank_snapshot(
            &cards,
            &lexical_keys,
            Some(&snapshot),
            Some(&model_id),
            Some(values),
            &mut || Ok(()),
        )?;
        let resident = rank_matrix(&matrix, &[], Some(values), &mut || Ok(()))?;
        let resident_hybrid = rank_matrix(&matrix, &lexical_keys, Some(values), &mut || Ok(()))?;
        let oracle = ranking_json(&baseline);
        let fused = ranking_json(&baseline_hybrid);
        for (name, vector, hybrid) in [
            ("current", baseline, baseline_hybrid),
            ("matrix_cold", cold_vector, cold_hybrid),
            ("matrix", resident, resident_hybrid),
        ] {
            ensure!(
                vector.embedded == 6722 && vector.hits.len() == 200 && hybrid.embedded == 6722,
                "incomplete vector arm"
            );
            records
                .entry(format!("{name}_vector"))
                .or_default()
                .push(outcome(query, &vector, &vector));
            records
                .entry(format!("{name}_hybrid"))
                .or_default()
                .push(outcome(query, &vector, &hybrid));
            detailed
                .entry(format!("{name}_vector"))
                .or_default()
                .push(query_report(query, values, &vector));
            detailed
                .entry(format!("{name}_hybrid"))
                .or_default()
                .push(query_report(query, values, &hybrid));
        }
        serde_json::to_writer(
            &mut frozen_queries,
            &json!({"ordinal":ordinal,"id":query.id,"category":query.category,"answers":query.answers,
            "text":query.query,"vector":values,"lexical_keys":lexical_keys.iter().map(|key| json!({"version_id":key.version_id,"syntax_id":key.syntax_id})).collect::<Vec<_>>(),
            "oracle_vector200":oracle,"fused_all":fused,"pin":pin,"model_id":model_id}),
        )?;
        writeln!(frozen_queries)?;
    }
    frozen_queries.flush()?;
    fs::write(output.join("queries.i8"), &query_bytes)?;
    let mut summary = serde_json::Map::new();
    for (name, rows) in &records {
        ensure!(rows.len() == 50, "incomplete gold evaluation");
        let mut file = BufWriter::new(File::create(output.join(format!("{name}.jsonl")))?);
        for row in rows {
            serde_json::to_writer(&mut file, row)?;
            writeln!(file)?;
        }
        file.flush()?;
        summary.insert(name.clone(), metrics(rows));
    }
    write_json(&output.join("summary.json"), &summary)?;
    let evaluations: BTreeMap<_, _> = records
        .iter()
        .map(|(name, rows)| (name.clone(), evaluation(rows, &detailed[name])))
        .collect();
    let mut engines = BTreeMap::new();
    for name in ["matrix_cold", "matrix"] {
        engines.insert(
            name,
            json!({
                "vector_only":evaluations[&format!("{name}_vector")],
                "hybrid":evaluations[&format!("{name}_hybrid")],
                "acceptance":{"gate":"exact_fraction_outside_rust", "rounding_or_epsilon":false},
            }),
        );
    }
    write_json(
        &output.join("report.json"),
        &json!({
            "scope":"corrected50_frozen214_gold_no_inference_no_timing",
            "backend":args.backend,"query_count":50,"candidate_count":cards.len(),
            "embedded":matrix.embedded_count(),
            "pin":pin,"model_id":model_id,
            "fixed_ranking":{"rrf_k":20,"candidate_depth":200,"lexical_weight":1,
                "vector_weight":4,"evaluation_depth":100},
            "current_native_rust_fixture_baseline":{
                "vector_only":evaluations["current_vector"],"hybrid":evaluations["current_hybrid"],
            },
            "engines":engines,
            "acceptance_definition":"Outside Rust: exact Fraction MRR@100; both cold and updated matrix hybrid success5, success10 and MRR must not regress versus BOTH this backend fresh rank_snapshot control AND frozen historical hybrid baseline; CPU AND Metal must each have all 50 corrected official queries. No epsilon or score/order-equality gate.",
            "historical_214_baseline":{
                "vector_only":{"n":50,"success5":24,"success10":30,
                    "mrr_fraction":"1826786986109/6171453288000"},
                "hybrid":{"n":50,"success5":20,"success10":31,
                    "mrr_fraction":"30600636226641/107637010943000"},
            },
            "engine_notes":{
                "matrix_cold":"Production SearchMatrix::from_snapshot and rank_matrix over the full cold fixture",
                "matrix":"Same production matrix after two identity-preserving VID replacements and committed same-byte cache update_vectors to both a hole and a present candidate",
            },
            "operation_coverage":{
                "cold_embedded":6722,"delta_deleted":delta.deleted,
                "delta_added":delta.added,"delta_present":delta.present,
                "after_delta_embedded":6721,"committed_cache_updated":updated,
                "final_embedded":matrix.embedded_count(),"final_active":matrix.active_count(),
                "identity_and_bytes_preserved":true,
            },
            "scope_limit":"Historical gold-only synthetic head, not MEDIUM publication, latency or lifecycle evidence",
            "score_bits_and_tolerance":"Informational only; no score-equality acceptance",
        }),
    )?;
    fs::write(output.join("README.md"), REPLAY_README)?;
    drop(snapshot);
    drop(reader);
    let mut output_hashes = BTreeMap::new();
    for entry in fs::read_dir(&output)? {
        let path = entry?.path();
        if path.is_file() {
            let bytes = fs::read(&path)?;
            output_hashes.insert(
                path.file_name().unwrap().to_string_lossy().into_owned(),
                json!({"bytes":bytes.len(),"sha256":digest(&bytes)}),
            );
        }
    }
    write_json(
        &output.join("manifest.json"),
        &json!({"scope":"corrected50_gold_relevance_not_timing", "backend":args.backend,
        "inputs":input_hashes,"outputs":output_hashes,
        "runner_source_sha256":digest(include_bytes!("search_gold_matrix.rs")),
        "ranking_source_sha256":digest(include_bytes!("../src/search_rank.rs")),
        "quantization_source_sha256":digest(include_bytes!("../src/search_embed/mod.rs")),
        "cache_source_sha256":digest(include_bytes!("../src/search_vectors.rs")),
        "binary_sha256":digest(&fs::read(std::env::current_exe()?)?),
        "quantization":"public production quantize: F32 parse, prefix1024, F64 maxabs/127 and divide, ties-even int8 both sides",
        "metrics":"any gold success@5/@10; first gold reciprocal rank through100; arms200; existing20/200/1/4 RRF",
        "historical_hybrid_baseline":{"success5":20,"success10":31,"n":50,"mrr":0.28429474173010805},
        "acceptance":"exact Fraction outside Rust against BOTH this backend fresh rank_snapshot and frozen historical hybrid baseline, all50 CPU AND Metal; summary F64 is diagnostic only; no bit-identical gate",
        "historical_vector_mrr_fraction":"1826786986109/6171453288000",
        "historical_hybrid_mrr_fraction":"30600636226641/107637010943000",
        "constants_changed":false}),
    )?;
    println!("{}", serde_json::to_string(&summary)?);
    Ok(())
}
