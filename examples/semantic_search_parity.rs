//! Diagnostic-only 2048-dimensional reference output, not a product embedding mode.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use trellis::search_embed::{Backend, Embedder, EmbeddingControl, InputKind};

const MAX_LINE_BYTES: usize = 64 * 1024;
const BATCH_SIZE: usize = 16;
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
) -> Result<()> {
    let kind = match batch[0].kind {
        Kind::Document => InputKind::Document,
        Kind::Query => InputKind::Query,
    };
    let texts: Vec<String> = batch.iter().map(|row| row.text.clone()).collect();
    let vectors = embedder
        .embed_reference(&texts, kind, &EmbeddingControl::new(cancel))
        .with_context(|| format!("embedding batch {batch_number}"))?;
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
        "backend={:?} model=voyageai/voyage-4-nano revision=5fb34e870f49f19e79c94d5e118f5ee730133279 dtype=f32 max_tokens=512 batch_size={BATCH_SIZE} diagnostic_dimensions={REFERENCE_DIMENSIONS}",
        args.backend
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
    let mut batch: Vec<Input> = Vec::with_capacity(BATCH_SIZE);
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
            write_batch(&embedder, &mut batch, &mut output, &cancel, batch_number)?;
        }
        batch.push(row);
        rows += 1;
        if batch.len() == BATCH_SIZE {
            batch_number += 1;
            write_batch(&embedder, &mut batch, &mut output, &cancel, batch_number)?;
        }
    }
    if !batch.is_empty() {
        batch_number += 1;
        write_batch(&embedder, &mut batch, &mut output, &cancel, batch_number)?;
    }
    output.flush()?;
    eprintln!(
        "complete rows={rows} batches={batch_number} elapsed_seconds={:.3}",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
