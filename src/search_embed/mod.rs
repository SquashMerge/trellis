//! Offline embeddings from the pinned voyage-4-nano snapshot.
//!
//! Loading is explicit and local-only. No model files are downloaded and no Python
//! model code is executed. The normal API returns only 1024-dimensional int8 vectors.

mod qwen3;

use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use sha2::{Digest, Sha256};
use tokenizers::{
    PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationDirection,
    TruncationParams,
};

const MAX_BATCH: usize = 16;
const MAX_TOKENS: usize = 512;
// Bounds text tokenization work before adding the fixed retrieval prompt.
const MAX_INPUT_BYTES: usize = 16_384;
const FULL_DIMENSIONS: usize = 2048;
const DIMENSIONS: usize = 1024;
const QUERY_PROMPT: &str = "Represent the query for retrieving supporting documents: ";
const DOCUMENT_PROMPT: &str = "Represent the document for retrieval: ";

// voyageai/voyage-4-nano@5fb34e870f49f19e79c94d5e118f5ee730133279.
// The Python file identifies the reproduced architecture; it is never executed.
const MODEL_FILES: &[(&str, &str)] = &[
    (
        "1_Pooling/config.json",
        "2bc529695125f68de57d1fd347e3d2920b993bc635c5f4f09a45e130c102a989",
    ),
    (
        "config.json",
        "bcba1ed5644d9bc592aa6bf0e36e11c6642294b5aef0fe65fdc2d0c8cf098cae",
    ),
    (
        "config_sentence_transformers.json",
        "4b0f3b9dea0ccb018b705084289d09ad1c4a02ddd277bf2e80bde9887539ef08",
    ),
    (
        "model.safetensors",
        "3dae0c63c81dcab79ac213af331940a1bf2b8a53ec8646be878552890291ad30",
    ),
    (
        "modeling_qwen3_bidirectional.py",
        "425304da2571d17bf04add3b70fab46d4b7d4466abbe118ef553ccb6fe8fadfe",
    ),
    (
        "modules.json",
        "84e40c8e006c9b1d6c122e02cba9b02458120b5fb0c87b746c41e0207cf642cf",
    ),
    (
        "sentence_bert_config.json",
        "ae8658c7cf91db1a3ceee800af0f9bda2c7ad60a88b5c2b4d6d5cb0a4394c9c1",
    ),
    (
        "tokenizer.json",
        "c0382117ea329cdf097041132f6d735924b697924d6f6fc3945713e96ce87539",
    ),
    (
        "tokenizer_config.json",
        "58c4abbc36eeccd8b3c8453f262225e8f2803855c88790760b618f4cd9e43be9",
    ),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Backend {
    Cpu,
    Metal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputKind {
    Query,
    Document,
}

#[derive(Debug)]
pub enum EmbedError {
    ModelUnavailable(String),
    InvalidInput(String),
    Cancelled,
    Runtime(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelUnavailable(message) => write!(f, "model unavailable: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid embedding input: {message}"),
            Self::Cancelled => f.write_str("embedding cancelled"),
            Self::Runtime(message) => write!(f, "embedding runtime: {message}"),
        }
    }
}

impl std::error::Error for EmbedError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantizedVector {
    pub values: Vec<i8>,
}

pub struct Embedder {
    model: qwen3::Model,
    tokenizer: Tokenizer,
    device: Device,
}

impl Embedder {
    /// Loads only the supported, hash-pinned snapshot from a local directory.
    /// Metal is explicit: unavailable Metal never falls back to CPU.
    pub fn load(path: &Path, backend: Backend) -> Result<Self, EmbedError> {
        let device = match backend {
            Backend::Cpu => Device::Cpu,
            Backend::Metal => metal_device()?,
        };
        let mut config = Vec::new();
        let mut tokenizer = Vec::new();
        let mut weights = Vec::new();
        for &(name, expected_hash) in MODEL_FILES {
            let bytes = read_verified(path, name, expected_hash)?;
            match name {
                "config.json" => config = bytes,
                "tokenizer.json" => tokenizer = bytes,
                "model.safetensors" => weights = bytes,
                _ => {}
            }
        }
        let config: qwen3::Config = serde_json::from_slice(&config)
            .map_err(|error| EmbedError::ModelUnavailable(format!("config.json: {error}")))?;
        let mut tokenizer = Tokenizer::from_bytes(&tokenizer)
            .map_err(|error| EmbedError::ModelUnavailable(format!("tokenizer.json: {error}")))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                direction: TruncationDirection::Right,
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|error| {
                EmbedError::ModelUnavailable(format!("tokenizer truncation: {error}"))
            })?;
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            direction: PaddingDirection::Right,
            pad_to_multiple_of: None,
            pad_id: 151_643,
            pad_type_id: 0,
            pad_token: "<|endoftext|>".into(),
        }));
        // The same owned bytes that passed SHA-256 verification are decoded here.
        // No unsafe mmap and no second open of a potentially changed weights file.
        let builder = VarBuilder::from_buffered_safetensors(weights, DType::F32, &device)
            .map_err(|error| EmbedError::ModelUnavailable(format!("model.safetensors: {error}")))?;
        let model = qwen3::Model::new(&config, builder)
            .map_err(|error| EmbedError::ModelUnavailable(format!("model tensors: {error}")))?;
        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embeds at most 16 texts, preserving input order. Cancellation is cooperative
    /// before and after the bounded forward pass; results are never partial.
    pub fn embed(
        &self,
        texts: &[String],
        kind: InputKind,
        cancel: &AtomicBool,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        batch_boundary(|| {
            let vectors = self.encode_float(texts, kind, cancel)?;
            let result = vectors.iter().map(|vector| quantize(vector)).collect();
            check_cancel(cancel)?;
            result
        })
    }

    /// Full-dimensional normalized output only for numerical reference evaluation.
    #[cfg(feature = "test-support")]
    pub fn embed_reference(
        &self,
        texts: &[String],
        kind: InputKind,
        cancel: &AtomicBool,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        batch_boundary(|| self.encode_float(texts, kind, cancel))
    }

    fn encode_float(
        &self,
        texts: &[String],
        kind: InputKind,
        cancel: &AtomicBool,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        check_cancel(cancel)?;
        if texts.len() > MAX_BATCH {
            return Err(EmbedError::InvalidInput(format!(
                "batch exceeds {MAX_BATCH} texts"
            )));
        }
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let inputs: Vec<String> = texts.iter().map(|text| prepare_input(text, kind)).collect();
        // This model's tokenizer adds neither BOS nor EOS in the reference path.
        let encodings = self
            .tokenizer
            .encode_batch(inputs, false)
            .map_err(|error| EmbedError::Runtime(format!("tokenization: {error}")))?;
        let tokens = encodings[0].len();
        if tokens == 0 || tokens > MAX_TOKENS {
            return Err(EmbedError::Runtime(
                "invalid tokenized sequence length".into(),
            ));
        }
        let ids: Vec<u32> = encodings
            .iter()
            .flat_map(|encoding| encoding.get_ids().iter().copied())
            .collect();
        let masks: Vec<u32> = encodings
            .iter()
            .flat_map(|encoding| encoding.get_attention_mask().iter().copied())
            .collect();
        let ids = Tensor::from_vec(ids, (texts.len(), tokens), &self.device).map_err(runtime)?;
        let mask = Tensor::from_vec(masks, (texts.len(), tokens), &self.device).map_err(runtime)?;
        check_cancel(cancel)?;
        let hidden = self.model.forward(&ids, &mask).map_err(runtime)?;
        check_cancel(cancel)?;
        if hidden.dims() != [texts.len(), tokens, FULL_DIMENSIONS] {
            return Err(EmbedError::Runtime("unexpected model output shape".into()));
        }
        // Pool all non-padding tokens, including the retrieval prompt. Keep the
        // full token tensor on the device and transfer only the pooled vectors.
        let mask = mask
            .to_dtype(DType::F32)
            .map_err(runtime)?
            .unsqueeze(2)
            .map_err(runtime)?;
        let summed = hidden
            .to_dtype(DType::F32)
            .map_err(runtime)?
            .broadcast_mul(&mask)
            .map_err(runtime)?
            .sum(1)
            .map_err(runtime)?;
        let counts = mask.sum(1).map_err(runtime)?;
        let pooled = summed.broadcast_div(&counts).map_err(runtime)?;
        let norms = pooled
            .sqr()
            .map_err(runtime)?
            .sum_keepdim(1)
            .map_err(runtime)?
            .sqrt()
            .map_err(runtime)?;
        let normalized = pooled.broadcast_div(&norms).map_err(runtime)?;
        let vectors = normalized.to_vec2::<f32>().map_err(runtime)?;
        if vectors.iter().flatten().any(|value| !value.is_finite()) {
            return Err(EmbedError::Runtime("non-finite model embedding".into()));
        }
        check_cancel(cancel)?;
        Ok(vectors)
    }
}

fn read_verified(path: &Path, name: &str, expected_hash: &str) -> Result<Vec<u8>, EmbedError> {
    let bytes = std::fs::read(path.join(name))
        .map_err(|error| EmbedError::ModelUnavailable(format!("{name}: {error}")))?;
    if hex::encode(Sha256::digest(&bytes)) != expected_hash {
        return Err(EmbedError::ModelUnavailable(format!(
            "{name}: SHA-256 mismatch"
        )));
    }
    Ok(bytes)
}

#[cfg(feature = "metal")]
fn metal_device() -> Result<Device, EmbedError> {
    Device::new_metal(0).map_err(|error| EmbedError::ModelUnavailable(format!("Metal: {error}")))
}

#[cfg(not(feature = "metal"))]
fn metal_device() -> Result<Device, EmbedError> {
    Err(EmbedError::ModelUnavailable(
        "Metal support is not enabled".into(),
    ))
}

fn runtime(error: candle_core::Error) -> EmbedError {
    EmbedError::Runtime(error.to_string())
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), EmbedError> {
    if cancel.load(Ordering::Acquire) {
        Err(EmbedError::Cancelled)
    } else {
        Ok(())
    }
}

fn batch_boundary<T>(operation: impl FnOnce() -> Result<T, EmbedError>) -> Result<T, EmbedError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation))
        .unwrap_or_else(|_| Err(EmbedError::Runtime("embedding batch panicked".into())))
}

fn prepare_input(text: &str, kind: InputKind) -> String {
    let prompt = match kind {
        InputKind::Query => QUERY_PROMPT,
        InputKind::Document => DOCUMENT_PROMPT,
    };
    let mut end = text.len().min(MAX_INPUT_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let input = format!("{prompt}{}", &text[..end]);
    // CPython str.strip also treats U+001C..U+001F as whitespace.
    input
        .trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
        .to_owned()
}

/// Truncates to 1024 coordinates and quantizes with a per-row maxabs/127 scale.
/// Scale and division use f64, followed by round-to-nearest, ties-to-even. Prefix
/// L2 normalization cancels in this scale; retrieval normalizes the int8 direction.
/// This is not NumPy/BLAS byte parity with the historical full-int8 arm.
pub fn quantize(vector: &[f32]) -> Result<QuantizedVector, EmbedError> {
    if !matches!(vector.len(), DIMENSIONS | FULL_DIMENSIONS) {
        return Err(EmbedError::InvalidInput(
            "expected 1024 or 2048 dimensions".into(),
        ));
    }
    if vector.iter().any(|value| !value.is_finite()) {
        return Err(EmbedError::InvalidInput("non-finite embedding".into()));
    }
    let prefix = &vector[..DIMENSIONS];
    let maximum = prefix
        .iter()
        .map(|value| f64::from(*value).abs())
        .fold(0.0_f64, f64::max);
    if maximum == 0.0 {
        return Err(EmbedError::InvalidInput(
            "embedding has a zero prefix".into(),
        ));
    }
    let scale = maximum / 127.0;
    let values = prefix
        .iter()
        .map(|value| (f64::from(*value) / scale).round_ties_even() as i8)
        .collect();
    Ok(QuantizedVector { values })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_quantization_truncates_and_uses_per_vector_scale() {
        let mut reference = vec![0.0; FULL_DIMENSIONS];
        reference[0] = -2.0;
        reference[1] = 1.0;
        reference[DIMENSIONS] = 1000.0;
        let actual = quantize(&reference).unwrap();
        assert_eq!(actual.values.len(), DIMENSIONS);
        assert_eq!(&actual.values[..2], &[-127, 64]);
        assert!(actual.values[2..].iter().all(|value| *value == 0));
        assert_eq!(actual, quantize(&reference[..DIMENSIONS]).unwrap());
    }

    #[test]
    fn quantization_rounds_half_steps_to_even() {
        let mut vector = vec![0.0; DIMENSIONS];
        vector[..7].copy_from_slice(&[127.0, 0.5, 1.5, 2.5, -0.5, -1.5, -2.5]);
        assert_eq!(
            &quantize(&vector).unwrap().values[..7],
            &[127, 0, 2, 2, 0, -2, -2]
        );
    }

    #[test]
    fn quantization_rejects_invalid_vectors() {
        assert!(matches!(quantize(&[1.0]), Err(EmbedError::InvalidInput(_))));
        assert!(matches!(
            quantize(&vec![0.0; DIMENSIONS]),
            Err(EmbedError::InvalidInput(_))
        ));
        assert!(matches!(
            quantize(&vec![f32::NAN; DIMENSIONS]),
            Err(EmbedError::InvalidInput(_))
        ));
    }

    #[test]
    fn prompt_preparation_matches_strip_and_bounds_utf8() {
        assert_eq!(
            prepare_input("  text \u{1c}", InputKind::Query),
            format!("{QUERY_PROMPT}  text")
        );
        assert_eq!(
            prepare_input("\n", InputKind::Document),
            DOCUMENT_PROMPT.trim()
        );
        let bounded = prepare_input(&"界".repeat(MAX_INPUT_BYTES), InputKind::Document);
        assert!(bounded.len() <= MAX_INPUT_BYTES + DOCUMENT_PROMPT.len());
        assert!(bounded.starts_with(DOCUMENT_PROMPT));
        assert!(MAX_INPUT_BYTES + DOCUMENT_PROMPT.len() - bounded.len() < 3);
    }

    #[test]
    fn cancellation_and_panics_are_typed() {
        assert!(matches!(
            check_cancel(&AtomicBool::new(true)),
            Err(EmbedError::Cancelled)
        ));
        let result: Result<(), EmbedError> = batch_boundary(|| panic!("runtime failure"));
        assert!(matches!(result, Err(EmbedError::Runtime(_))));
    }
}
