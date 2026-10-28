//! Diagnostic-only 2048-dimensional reference output, not a product embedding mode.
//! `--document-batch-shape bucketed` exercises prepared documents in bounded windows,
//! logs actual forward shapes, and restores input order for check-parity.py.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use trellis::search_embed::{
    Backend, DOCUMENT_BATCH_MAX, DOCUMENT_BATCH_TOKEN_WORK_MAX, Embedder, EmbeddingControl,
    InputKind,
};

const MAX_LINE_BYTES: usize = 64 * 1024;
const BATCH_SIZE: usize = 16;
// Mirrors the production all-missing scan window, not a forward batch size.
const DOCUMENT_WINDOW_SIZE: usize = 512;
const REFERENCE_DIMENSIONS: usize = 2048;

#[derive(Parser)]
#[command(about = "Emit diagnostic voyage-4-nano reference vectors from local model files")]
struct Args {
    #[arg(long)]
    model_dir: PathBuf,
    #[arg(long, value_enum)]
    backend: Device,
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
    /// Bucketed mode uses the production prepared-document path; queries remain batches of 16.
    #[arg(long, value_enum, default_value_t = DocumentBatchShape::Serial)]
    document_batch_shape: DocumentBatchShape,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum DocumentBatchShape {
    Serial,
    Bucketed,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Device {
    Cpu,
    Metal,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Document,
    Query,
}

#[derive(Deserialize)]
struct Input {
    id: String,
    text: String,
    kind: Kind,
}

#[derive(Serialize)]
struct Output<'a> {
    id: &'a str,
    vector: &'a [f32],
}

fn write_batch(
    embedder: &Embedder,
    batch: &mut Vec<Input>,
    output: &mut BufWriter<File>,
    cancel: &AtomicBool,
    batch_number: usize,
    shape: DocumentBatchShape,
    shape_counts: &mut BTreeMap<(usize, usize), usize>,
) -> Result<()> {
    let kind = match batch[0].kind {
        Kind::Document => InputKind::Document,
        Kind::Query => InputKind::Query,
    };
    let texts: Vec<String> = batch.iter().map(|row| row.text.clone()).collect();
    let vectors = if shape == DocumentBatchShape::Bucketed && batch[0].kind == Kind::Document {
        bucketed_vectors(embedder, batch, &texts, cancel, batch_number, shape_counts)?
    } else {
        embedder
            .embed_reference(&texts, kind, &EmbeddingControl::new(cancel))
            .with_context(|| format!("embedding batch {batch_number}"))?
    };
    ensure!(
        vectors.len() == batch.len(),
        "reference output row count mismatch"
    );
    ensure!(
        vectors
            .iter()
            .all(|vector| vector.len() == REFERENCE_DIMENSIONS
                && vector.iter().all(|value| value.is_finite())),
        "reference output must contain 2048 finite values per row"
    );
    for (row, vector) in batch.iter().zip(&vectors) {
        serde_json::to_writer(
            &mut *output,
            &Output {
                id: &row.id,
                vector,
            },
        )
        .with_context(|| format!("writing output for {}", row.id))?;
        output.write_all(b"\n")?;
    }
    output.flush()?;
    eprintln!(
        "batch={batch_number} kind={:?} rows={}",
        batch[0].kind,
        batch.len()
    );
    batch.clear();
    Ok(())
}

fn slice_end(lengths: &[usize], start: usize) -> usize {
    let mut end = start;
    while end < lengths.len()
        && end - start < DOCUMENT_BATCH_MAX
        && (end - start + 1) * lengths[end] <= DOCUMENT_BATCH_TOKEN_WORK_MAX
    {
        end += 1;
    }
    end
}

fn bucketed_vectors(
    embedder: &Embedder,
    rows: &[Input],
    texts: &[String],
    cancel: &AtomicBool,
    window_number: usize,
    shape_counts: &mut BTreeMap<(usize, usize), usize>,
) -> Result<Vec<Vec<f32>>> {
    let control = EmbeddingControl::new(cancel);
    let prepared = embedder.document_tokenizer().prepare(texts, &control)?;
    ensure!(prepared.len() == rows.len(), "prepared row count mismatch");
    let mut ordered: Vec<_> = prepared.into_iter().enumerate().collect();
    ordered.sort_by_key(|(ordinal, document)| (document.token_len(), *ordinal));
    let lengths: Vec<_> = ordered
        .iter()
        .map(|(_, document)| document.token_len())
        .collect();
    let mut restored = vec![None; rows.len()];
    let mut start = 0;
    while start < ordered.len() {
        let end = slice_end(&lengths, start);
        ensure!(end > start, "document exceeds prepared slice bounds");
        let documents: Vec<_> = ordered[start..end]
            .iter()
            .map(|(_, document)| document.clone())
            .collect();
        let vectors = embedder
            .embed_prepared_reference(&documents, &control)
            .with_context(|| {
                format!("embedding prepared window {window_number} slice {start}..{end}")
            })?;
        ensure!(
            vectors.len() == documents.len(),
            "prepared output row count mismatch"
        );
        *shape_counts
            .entry((end - start, lengths[end - 1]))
            .or_default() += 1;
        eprintln!(
            "prepared_slice={}",
            serde_json::json!({
                "window": window_number,
                "max_tokens": lengths[end - 1],
                "padded_token_work": (end - start) * lengths[end - 1],
                "members": ordered[start..end].iter().map(|(ordinal, document)|
                    serde_json::json!({"ordinal": ordinal, "id": rows[*ordinal].id,
                        "tokens": document.token_len()})).collect::<Vec<_>>()
            })
        );
        for ((ordinal, _), vector) in ordered[start..end].iter().zip(vectors) {
            ensure!(
                restored[*ordinal].replace(vector).is_none(),
                "duplicate output ordinal"
            );
        }
        start = end;
    }
    restored
        .into_iter()
        .enumerate()
        .map(|(ordinal, vector)| {
            vector.with_context(|| format!("missing output ordinal {ordinal}"))
        })
        .collect()
}

fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.input != args.output,
        "input and output paths must differ"
    );
    let started = Instant::now();
    let input = File::open(&args.input).context("opening input JSONL")?;
    let backend = match args.backend {
        Device::Cpu => Backend::Cpu,
        Device::Metal => Backend::Metal,
    };
    eprintln!(
        "backend={:?} model=voyageai/voyage-4-nano revision=5fb34e870f49f19e79c94d5e118f5ee730133279 dtype=f32 max_tokens=512 serial_batch_size={BATCH_SIZE} query_batch_size={BATCH_SIZE} diagnostic_dimensions={REFERENCE_DIMENSIONS}",
        args.backend
    );
    eprintln!(
        "document_batch_shape={:?} document_window={} document_batch_max={} padded_token_work_max={}",
        args.document_batch_shape,
        if args.document_batch_shape == DocumentBatchShape::Bucketed {
            DOCUMENT_WINDOW_SIZE
        } else {
            BATCH_SIZE
        },
        DOCUMENT_BATCH_MAX,
        DOCUMENT_BATCH_TOKEN_WORK_MAX,
    );
    let embedder =
        Embedder::load(&args.model_dir, backend).context("loading local pinned model")?;
    let output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)
        .context("creating new output JSONL (existing files are never overwritten)")?;
    let mut output = BufWriter::new(output);
    let mut input = BufReader::new(input);
    let cancel = AtomicBool::new(false);
    let mut batch: Vec<Input> = Vec::with_capacity(DOCUMENT_WINDOW_SIZE);
    let mut shape_counts = BTreeMap::new();
    let mut line = Vec::new();
    let mut line_number = 0;
    let mut batch_number = 0;
    let mut rows = 0;
    loop {
        line.clear();
        let bytes = input
            .by_ref()
            .take((MAX_LINE_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)
            .context("reading input JSONL")?;
        if bytes == 0 {
            break;
        }
        line_number += 1;
        ensure!(
            bytes <= MAX_LINE_BYTES,
            "input line {line_number} exceeds {MAX_LINE_BYTES} bytes (including newline)"
        );
        let row: Input = serde_json::from_slice(&line)
            .with_context(|| format!("invalid input JSON on line {line_number}"))?;
        ensure!(!row.id.is_empty(), "empty id on input line {line_number}");
        if batch
            .last()
            .is_some_and(|previous| previous.kind != row.kind)
        {
            batch_number += 1;
            write_batch(
                &embedder,
                &mut batch,
                &mut output,
                &cancel,
                batch_number,
                args.document_batch_shape,
                &mut shape_counts,
            )?;
        }
        batch.push(row);
        rows += 1;
        let window = if args.document_batch_shape == DocumentBatchShape::Bucketed
            && batch[0].kind == Kind::Document
        {
            DOCUMENT_WINDOW_SIZE
        } else {
            BATCH_SIZE
        };
        if batch.len() == window {
            batch_number += 1;
            write_batch(
                &embedder,
                &mut batch,
                &mut output,
                &cancel,
                batch_number,
                args.document_batch_shape,
                &mut shape_counts,
            )?;
        }
    }
    if !batch.is_empty() {
        batch_number += 1;
        write_batch(
            &embedder,
            &mut batch,
            &mut output,
            &cancel,
            batch_number,
            args.document_batch_shape,
            &mut shape_counts,
        )?;
    }
    output.flush()?;
    eprintln!(
        "prepared_shape_summary={}",
        serde_json::json!({
            "shapes": shape_counts.iter().map(|((rows, tokens), batches)|
                serde_json::json!({"rows": rows, "max_tokens": tokens, "batches": batches}))
                .collect::<Vec<_>>(),
            "slices_at_least_64": shape_counts.iter().filter(|((rows, _), _)| *rows >= 64)
                .map(|(_, batches)| batches).sum::<usize>(),
            "slices_at_128": shape_counts.iter().filter(|((rows, _), _)| *rows == 128)
                .map(|(_, batches)| batches).sum::<usize>(),
        })
    );
    eprintln!(
        "complete rows={rows} batches={batch_number} elapsed_seconds={:.3}",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_slices_obey_row_and_padded_token_work_bounds() {
        assert_eq!(slice_end(&[64; 129], 0), 128);
        assert_eq!(slice_end(&[512; 128], 0), 16);
        let mixed: Vec<_> = (1..=128).map(|length| length * 4).collect();
        let mut start = 0;
        while start < mixed.len() {
            let end = slice_end(&mixed, start);
            assert!(end > start);
            assert!(end - start <= DOCUMENT_BATCH_MAX);
            assert!((end - start) * mixed[end - 1] <= DOCUMENT_BATCH_TOKEN_WORK_MAX);
            if end < mixed.len() && end - start < DOCUMENT_BATCH_MAX {
                assert!((end - start + 1) * mixed[end] > DOCUMENT_BATCH_TOKEN_WORK_MAX);
            }
            start = end;
        }
    }
}
