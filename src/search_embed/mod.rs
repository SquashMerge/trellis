//! Offline embeddings from the pinned voyage-4-nano snapshot.
//!
//! Loading is explicit and local-only. No model files are downloaded and no Python
//! model code is executed. The normal API returns only 1024-dimensional int8 vectors.

mod qwen3;

use std::fmt;
use std::io::Read;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use candle_core::{DType, Device, Tensor};
use candle_nn::VarBuilder;
use sha2::{Digest, Sha256};
use tokenizers::{
    PaddingDirection, PaddingParams, PaddingStrategy, Tokenizer, TruncationDirection,
    TruncationParams,
};

/// Absolute query cutoff using the request's clock; never mutates global cancellation.
#[derive(Clone)]
pub struct QueryBudget {
    cutoff: Instant,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl QueryBudget {
    pub fn new(cutoff: Instant, clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        Self { cutoff, clock }
    }

    pub fn now(&self) -> Instant {
        (self.clock)()
    }

    pub fn remaining(&self) -> std::time::Duration {
        self.cutoff.saturating_duration_since(self.now())
    }

    pub fn check(&self) -> Result<(), EmbedError> {
        if self.now() >= self.cutoff {
            Err(EmbedError::BudgetExceeded)
        } else {
            Ok(())
        }
    }
}

pub struct EmbeddingControl<'a> {
    cancel: &'a AtomicBool,
    budget: Option<&'a QueryBudget>,
}

impl<'a> EmbeddingControl<'a> {
    pub fn new(cancel: &'a AtomicBool) -> Self {
        Self {
            cancel,
            budget: None,
        }
    }

    pub fn with_budget(cancel: &'a AtomicBool, budget: &'a QueryBudget) -> Self {
        Self {
            cancel,
            budget: Some(budget),
        }
    }

    pub fn cancellation(&self) -> &AtomicBool {
        self.cancel
    }

    pub fn check(&self) -> Result<(), EmbedError> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(EmbedError::Cancelled);
        }
        if let Some(budget) = self.budget {
            budget.check()?;
        }
        Ok(())
    }
}

const MAX_BATCH: usize = 16;
const MAX_TOKENS: usize = 512;
// Bounds text tokenization work before adding the fixed retrieval prompt.
const MAX_INPUT_BYTES: usize = 16_384;
const FULL_DIMENSIONS: usize = 2048;
pub(crate) const DIMENSIONS: usize = 1024;
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

struct SupportedModel {
    name: &'static str,
    revision: &'static str,
    files: &'static [(&'static str, &'static str)],
}

const SUPPORTED_MODEL: SupportedModel = SupportedModel {
    name: "voyageai/voyage-4-nano",
    revision: "5fb34e870f49f19e79c94d5e118f5ee730133279",
    files: MODEL_FILES,
};

impl SupportedModel {
    fn descriptor(&self, card_format: &str) -> serde_json::Value {
        serde_json::json!({
            "name": self.name,
            "revision": self.revision,
            "modelFiles": self.files.iter().map(|(path, sha256)| {
                serde_json::json!({"path": path, "sha256": sha256})
            }).collect::<Vec<_>>(),
            "dimensions": DIMENSIONS,
            "quantization": "int8-prefix-maxabs-f64-ties-even.v1",
            "maxTokens": MAX_TOKENS,
            "queryPrompt": QUERY_PROMPT,
            "documentPrompt": DOCUMENT_PROMPT,
            "pooling": "masked-mean-including-prompt-then-l2-f32.v1",
            "inputPolicy": "utf8-prefix-16384-python-strip-tokenizer-nfc-no-special-tokens-right-truncate-right-pad-151643.v1",
            "cardFormat": card_format,
        })
    }
}

pub(crate) fn supported_model_name() -> &'static str {
    SUPPORTED_MODEL.name
}

pub(crate) fn supported_model_revision() -> &'static str {
    SUPPORTED_MODEL.revision
}

pub(crate) fn supported_model_files() -> &'static [(&'static str, &'static str)] {
    SUPPORTED_MODEL.files
}

pub(crate) fn model_id_for_card_format(card_format: &str) -> String {
    crate::native_ids::digest(
        b"trellis.search-model.v1\0",
        &crate::native_ids::canonical(&SUPPORTED_MODEL.descriptor(card_format)),
    )
}

/// Only this module can construct verified model inputs. Loading consumes the same
/// owned bytes that were hashed, without reopening the source files.
pub(crate) struct VerifiedModel {
    config: Vec<u8>,
    tokenizer: Vec<u8>,
    weights: Vec<u8>,
}

impl VerifiedModel {
    pub fn from_path(path: &Path, control: &EmbeddingControl<'_>) -> Result<Self, EmbedError> {
        verify_files(path, SUPPORTED_MODEL.files, control)
    }
}

/// Only file identities survive a job. Neither weights nor a loaded runtime do.
#[derive(Default)]
pub(crate) struct ModelVerification {
    files: Option<Vec<FileIdentity>>,
    #[cfg(test)]
    verifications: usize,
}

#[derive(PartialEq)]
struct FileIdentity {
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
}

fn file_identities(
    path: &Path,
    files: &[(&str, &str)],
    control: &EmbeddingControl<'_>,
) -> Result<Vec<FileIdentity>, EmbedError> {
    files
        .iter()
        .map(|(name, _)| {
            control.check()?;
            let metadata = std::fs::metadata(path.join(name))
                .map_err(|error| EmbedError::ModelUnavailable(format!("{name}: {error}")))?;
            Ok(FileIdentity {
                length: metadata.len(),
                modified: metadata
                    .modified()
                    .map_err(|error| EmbedError::ModelUnavailable(error.to_string()))?,
                #[cfg(unix)]
                inode: {
                    use std::os::unix::fs::MetadataExt;
                    (
                        metadata.dev(),
                        metadata.ino(),
                        metadata.ctime(),
                        metadata.ctime_nsec(),
                    )
                },
            })
        })
        .collect()
}

impl ModelVerification {
    pub(crate) fn unchanged(
        &self,
        path: &Path,
        control: &EmbeddingControl<'_>,
    ) -> Result<bool, EmbedError> {
        self.unchanged_files(path, SUPPORTED_MODEL.files, control)
    }

    fn unchanged_files(
        &self,
        path: &Path,
        files: &[(&str, &str)],
        control: &EmbeddingControl<'_>,
    ) -> Result<bool, EmbedError> {
        let identities = file_identities(path, files, control)?;
        control.check()?;
        Ok(self.files.as_ref() == Some(&identities))
    }

    pub(crate) fn read(
        &mut self,
        path: &Path,
        control: &EmbeddingControl<'_>,
    ) -> Result<VerifiedModel, EmbedError> {
        self.read_files(path, SUPPORTED_MODEL.files, control)
    }

    fn read_files(
        &mut self,
        path: &Path,
        files: &[(&str, &str)],
        control: &EmbeddingControl<'_>,
    ) -> Result<VerifiedModel, EmbedError> {
        let before = file_identities(path, files, control)?;
        let verified = if self.files.as_ref() == Some(&before) {
            let read = |name| read_model_file(path, name, None, control);
            VerifiedModel {
                config: read("config.json")?,
                tokenizer: read("tokenizer.json")?,
                weights: read("model.safetensors")?,
            }
        } else {
            self.files = None;
            #[cfg(test)]
            {
                self.verifications += 1;
            }
            verify_files(path, files, control)?
        };
        if before != file_identities(path, files, control)? {
            self.files = None;
            return Err(EmbedError::ModelUnavailable(
                "model files changed while loading".into(),
            ));
        }
        control.check()?;
        self.files = Some(before);
        Ok(verified)
    }
}

fn verify_files(
    path: &Path,
    files: &[(&str, &str)],
    control: &EmbeddingControl<'_>,
) -> Result<VerifiedModel, EmbedError> {
    let mut verified = VerifiedModel {
        config: Vec::new(),
        tokenizer: Vec::new(),
        weights: Vec::new(),
    };
    for &(name, expected_hash) in files {
        let bytes = read_model_file(path, name, Some(expected_hash), control)?;
        match name {
            "config.json" => verified.config = bytes,
            "tokenizer.json" => verified.tokenizer = bytes,
            "model.safetensors" => verified.weights = bytes,
            _ => {}
        }
    }
    Ok(verified)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
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
    BudgetExceeded,
    Runtime(String),
}

impl fmt::Display for EmbedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ModelUnavailable(message) => write!(f, "model unavailable: {message}"),
            Self::InvalidInput(message) => write!(f, "invalid embedding input: {message}"),
            Self::Cancelled => f.write_str("embedding cancelled"),
            Self::BudgetExceeded => f.write_str("query embedding budget exceeded"),
            Self::Runtime(message) => write!(f, "embedding runtime: {message}"),
        }
    }
}

impl std::error::Error for EmbedError {}

impl From<candle_core::Error> for EmbedError {
    fn from(error: candle_core::Error) -> Self {
        runtime(error)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuantizedVector {
    pub values: Vec<i8>,
}

/// Maximum documents and padded token work in one prepared inference slice.
/// The token-work bound keeps long-document slices at the old 16 x 512 limit.
pub const DOCUMENT_BATCH_MAX: usize = 128;
pub const DOCUMENT_BATCH_TOKEN_WORK_MAX: usize = MAX_BATCH * MAX_TOKENS;

/// An unpadded, prompted document, or original text for an injected embedder.
#[derive(Clone, Debug)]
pub struct PreparedDocument {
    input: PreparedDocumentInput,
}

#[derive(Clone, Debug)]
enum PreparedDocumentInput {
    Tokens(Vec<u32>),
    Text(String),
}

impl PreparedDocument {
    #[cfg(test)]
    pub(crate) fn token_ids_for_tests(&self) -> &[u32] {
        match &self.input {
            PreparedDocumentInput::Tokens(ids) => ids,
            PreparedDocumentInput::Text(_) => panic!("expected tokenized document"),
        }
    }

    /// Actual prompted token count in production; conservative for test models.
    pub fn token_len(&self) -> usize {
        match &self.input {
            PreparedDocumentInput::Tokens(ids) => ids.len(),
            PreparedDocumentInput::Text(_) => MAX_TOKENS,
        }
    }
}

/// Owns only tokenizer state, never model tensors. A clone can prepare documents
/// on the read pipeline while the model owner serves queries or embeds a slice.
#[derive(Clone, Default)]
pub struct DocumentTokenizer {
    tokenizer: Option<Tokenizer>,
}

impl DocumentTokenizer {
    pub(crate) fn from_tokenizer(tokenizer: &Tokenizer) -> Self {
        let mut tokenizer = tokenizer.clone();
        tokenizer.with_padding(None);
        Self {
            tokenizer: Some(tokenizer),
        }
    }

    pub fn prepare(
        &self,
        texts: &[String],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<PreparedDocument>, EmbedError> {
        batch_boundary(|| {
            control.check()?;
            let mut documents = Vec::with_capacity(texts.len());
            for text in texts {
                control.check()?;
                let input = if let Some(tokenizer) = &self.tokenizer {
                    let encoding = tokenizer
                        .encode(prepare_input(text, InputKind::Document), false)
                        .map_err(|error| EmbedError::Runtime(format!("tokenization: {error}")))?;
                    if encoding.is_empty() || encoding.len() > MAX_TOKENS {
                        return Err(EmbedError::Runtime(
                            "invalid tokenized sequence length".into(),
                        ));
                    }
                    PreparedDocumentInput::Tokens(encoding.get_ids().to_vec())
                } else {
                    PreparedDocumentInput::Text(text.clone())
                };
                documents.push(PreparedDocument { input });
            }
            control.check()?;
            Ok(documents)
        })
    }
}

fn prepared_shape(documents: &[PreparedDocument]) -> Result<(usize, usize), EmbedError> {
    let tokens = documents
        .iter()
        .map(PreparedDocument::token_len)
        .max()
        .unwrap_or(0);
    if documents.len() > DOCUMENT_BATCH_MAX
        || documents.len() * tokens > DOCUMENT_BATCH_TOKEN_WORK_MAX
    {
        return Err(EmbedError::InvalidInput(format!(
            "prepared batch exceeds {DOCUMENT_BATCH_MAX} documents or {DOCUMENT_BATCH_TOKEN_WORK_MAX} padded tokens"
        )));
    }
    Ok((documents.len(), tokens))
}

fn pad_prepared(
    documents: &[PreparedDocument],
    tokens: usize,
) -> Result<(Vec<u32>, Vec<u32>), EmbedError> {
    let mut ids = Vec::with_capacity(documents.len() * tokens);
    let mut masks = Vec::with_capacity(documents.len() * tokens);
    for document in documents {
        let PreparedDocumentInput::Tokens(document_ids) = &document.input else {
            return Err(EmbedError::InvalidInput(
                "production embedding requires prepared token ids".into(),
            ));
        };
        ids.extend_from_slice(document_ids);
        ids.resize(ids.len() + tokens - document_ids.len(), 151_643);
        masks.extend(std::iter::repeat_n(1, document_ids.len()));
        masks.resize(masks.len() + tokens - document_ids.len(), 0);
    }
    Ok((ids, masks))
}

/// The production model and deterministic test models use the same job path.
pub trait SearchEmbedder: Send {
    fn embed(
        &self,
        texts: &[String],
        kind: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError>;

    fn document_tokenizer(&self) -> DocumentTokenizer {
        DocumentTokenizer::default()
    }

    fn embed_prepared(
        &self,
        documents: &[PreparedDocument],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        control.check()?;
        prepared_shape(documents)?;
        let texts = documents
            .iter()
            .map(|document| match &document.input {
                PreparedDocumentInput::Text(text) => Ok(text.clone()),
                PreparedDocumentInput::Tokens(_) => Err(EmbedError::InvalidInput(
                    "prepared tokens require their production embedder".into(),
                )),
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.embed(&texts, InputKind::Document, control)
    }
}

pub trait SearchEmbedderFactory: Send + Sync {
    fn load(&self, control: &EmbeddingControl<'_>) -> Result<Box<dyn SearchEmbedder>, EmbedError>;
}

impl SearchEmbedder for Embedder {
    fn embed(
        &self,
        texts: &[String],
        kind: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        Embedder::embed(self, texts, kind, control)
    }

    fn document_tokenizer(&self) -> DocumentTokenizer {
        Embedder::document_tokenizer(self)
    }

    fn embed_prepared(
        &self,
        documents: &[PreparedDocument],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        Embedder::embed_prepared(self, documents, control)
    }
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
        let cancel = AtomicBool::new(false);
        let control = EmbeddingControl::new(&cancel);
        let device = backend_device(backend)?;
        Self::from_verified_on_device(VerifiedModel::from_path(path, &control)?, device, &control)
    }

    pub(crate) fn from_verified(
        verified: VerifiedModel,
        backend: Backend,
        control: &EmbeddingControl<'_>,
    ) -> Result<Self, EmbedError> {
        control.check()?;
        let device = backend_device(backend)?;
        control.check()?;
        Self::from_verified_on_device(verified, device, control)
    }

    fn from_verified_on_device(
        verified: VerifiedModel,
        device: Device,
        control: &EmbeddingControl<'_>,
    ) -> Result<Self, EmbedError> {
        control.check()?;
        let VerifiedModel {
            config,
            tokenizer,
            weights,
        } = verified;
        let config: qwen3::Config = serde_json::from_slice(&config)
            .map_err(|error| EmbedError::ModelUnavailable(format!("config.json: {error}")))?;
        control.check()?;
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
        control.check()?;
        // The same owned bytes that passed SHA-256 verification are decoded here.
        // No unsafe mmap and no second open of a potentially changed weights file.
        let builder = VarBuilder::from_buffered_safetensors(weights, DType::F32, &device)
            .map_err(|error| EmbedError::ModelUnavailable(format!("model.safetensors: {error}")))?;
        control.check()?;
        let model = qwen3::Model::new(&config, builder, control).map_err(|error| match error {
            EmbedError::Cancelled | EmbedError::BudgetExceeded => error,
            error => EmbedError::ModelUnavailable(format!("model tensors: {error}")),
        })?;
        control.check()?;
        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Embeds at most 16 texts, preserving input order. Cancellation and budget
    /// checks are cooperative between stages and layers; results are never partial.
    pub fn embed(
        &self,
        texts: &[String],
        kind: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        let _phase = (kind == InputKind::Query)
            .then(|| crate::search_model::SearchPhaseTimer::start("query_embedding"));
        batch_boundary(|| {
            let vectors = self.encode_float(texts, kind, control)?;
            let result = vectors.iter().map(|vector| quantize(vector)).collect();
            control.check()?;
            result
        })
    }

    /// Full-dimensional normalized output only for numerical reference evaluation.
    #[cfg(feature = "test-support")]
    pub fn embed_reference(
        &self,
        texts: &[String],
        kind: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        batch_boundary(|| self.encode_float(texts, kind, control))
    }

    pub fn document_tokenizer(&self) -> DocumentTokenizer {
        DocumentTokenizer::from_tokenizer(&self.tokenizer)
    }

    /// Embeds one bounded slice of prepared documents, preserving slice order.
    pub fn embed_prepared(
        &self,
        documents: &[PreparedDocument],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<QuantizedVector>, EmbedError> {
        batch_boundary(|| {
            let vectors = self.encode_prepared_float(documents, control)?;
            let result = vectors.iter().map(|vector| quantize(vector)).collect();
            control.check()?;
            result
        })
    }

    /// Full-dimensional prepared output for numerical reference evaluation.
    #[cfg(feature = "test-support")]
    pub fn embed_prepared_reference(
        &self,
        documents: &[PreparedDocument],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        batch_boundary(|| self.encode_prepared_float(documents, control))
    }

    fn encode_prepared_float(
        &self,
        documents: &[PreparedDocument],
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        control.check()?;
        let (batch, tokens) = prepared_shape(documents)?;
        if batch == 0 {
            return Ok(Vec::new());
        }
        let (ids, masks) = pad_prepared(documents, tokens)?;
        self.encode_tokens(ids, masks, batch, tokens, control)
    }

    fn encode_float(
        &self,
        texts: &[String],
        kind: InputKind,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        control.check()?;
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
        control.check()?;
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
        self.encode_tokens(ids, masks, texts.len(), tokens, control)
    }

    fn encode_tokens(
        &self,
        ids: Vec<u32>,
        masks: Vec<u32>,
        batch: usize,
        tokens: usize,
        control: &EmbeddingControl<'_>,
    ) -> Result<Vec<Vec<f32>>, EmbedError> {
        let ids = Tensor::from_vec(ids, (batch, tokens), &self.device).map_err(runtime)?;
        let mask = Tensor::from_vec(masks, (batch, tokens), &self.device).map_err(runtime)?;
        control.check()?;
        let hidden = self.model.forward(&ids, &mask, control)?;
        control.check()?;
        if hidden.dims() != [batch, tokens, FULL_DIMENSIONS] {
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
        control.check()?;
        Ok(vectors)
    }
}

// Bound work between cooperative checks without changing the owned-byte verification path.
const MODEL_READ_CHUNK: usize = 1024 * 1024;

fn read_model_file(
    path: &Path,
    name: &str,
    expected_hash: Option<&str>,
    control: &EmbeddingControl<'_>,
) -> Result<Vec<u8>, EmbedError> {
    control.check()?;
    let unavailable = |error| EmbedError::ModelUnavailable(format!("{name}: {error}"));
    let mut file = std::fs::File::open(path.join(name)).map_err(unavailable)?;
    let length = usize::try_from(file.metadata().map_err(unavailable)?.len())
        .map_err(|error| EmbedError::ModelUnavailable(format!("{name}: {error}")))?;
    let mut bytes = Vec::with_capacity(length);
    let mut buffer = vec![0; MODEL_READ_CHUNK];
    let mut hasher = expected_hash.map(|_| Sha256::new());
    loop {
        control.check()?;
        let count = file.read(&mut buffer).map_err(unavailable)?;
        control.check()?;
        if count == 0 {
            break;
        }
        if let Some(hasher) = &mut hasher {
            hasher.update(&buffer[..count]);
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if let (Some(expected), Some(hasher)) = (expected_hash, hasher)
        && hex::encode(hasher.finalize()) != expected
    {
        return Err(EmbedError::ModelUnavailable(format!(
            "{name}: SHA-256 mismatch"
        )));
    }
    control.check()?;
    Ok(bytes)
}

pub(crate) fn backend_available(backend: Backend) -> Result<(), EmbedError> {
    backend_device(backend).map(|_| ())
}

fn backend_device(backend: Backend) -> Result<Device, EmbedError> {
    match backend {
        Backend::Cpu => Ok(Device::Cpu),
        Backend::Metal => metal_device(),
    }
}

#[cfg(all(target_os = "macos", feature = "metal"))]
fn metal_device() -> Result<Device, EmbedError> {
    Device::new_metal(0).map_err(|error| EmbedError::ModelUnavailable(format!("Metal: {error}")))
}

#[cfg(not(all(target_os = "macos", feature = "metal")))]
fn metal_device() -> Result<Device, EmbedError> {
    Err(EmbedError::ModelUnavailable(
        "Metal support is not enabled".into(),
    ))
}

fn runtime(error: candle_core::Error) -> EmbedError {
    EmbedError::Runtime(error.to_string())
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

    fn test_tokenizer() -> Tokenizer {
        let mut tokenizer = Tokenizer::from_bytes(
            serde_json::to_vec(&serde_json::json!({
                "version": "1.0",
                "truncation": null,
                "padding": null,
                "added_tokens": [],
                "normalizer": {"type": "NFC"},
                "pre_tokenizer": {"type": "Whitespace"},
                "post_processor": null,
                "decoder": null,
                "model": {
                    "type": "WordLevel",
                    "vocab": {"[UNK]": 0, "a": 1, "é": 2},
                    "unk_token": "[UNK]"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .unwrap();
        tokenizer.with_padding(Some(PaddingParams {
            pad_id: 151_643,
            ..Default::default()
        }));
        tokenizer
    }

    #[test]
    fn prepared_documents_match_prompted_unpadded_ids_and_original_batch_padding() {
        let tokenizer = test_tokenizer();
        let documents_tokenizer = DocumentTokenizer::from_tokenizer(&tokenizer);
        let texts = vec![
            " a e\u{301} \u{1c}".into(),
            "a ".repeat(MAX_TOKENS + 10),
            "界".repeat(MAX_INPUT_BYTES),
            String::new(),
        ];
        let cancel = AtomicBool::new(false);
        let control = EmbeddingControl::new(&cancel);
        let documents = documents_tokenizer.prepare(&texts, &control).unwrap();
        let original = tokenizer
            .encode_batch(
                texts
                    .iter()
                    .map(|text| prepare_input(text, InputKind::Document))
                    .collect::<Vec<_>>(),
                false,
            )
            .unwrap();
        for (document, original) in documents.iter().zip(&original) {
            let PreparedDocumentInput::Tokens(ids) = &document.input else {
                panic!("production preparation must retain ids");
            };
            let actual_len = original.get_attention_mask().iter().sum::<u32>() as usize;
            assert_eq!(document.token_len(), actual_len);
            assert_eq!(ids, &original.get_ids()[..actual_len]);
        }
        assert!(documents[0].token_len() < documents[1].token_len());
        assert_eq!(documents[1].token_len(), MAX_TOKENS);
        let (_, tokens) = prepared_shape(&documents).unwrap();
        let (ids, masks) = pad_prepared(&documents, tokens).unwrap();
        assert_eq!(
            ids,
            original
                .iter()
                .flat_map(|row| row.get_ids().iter().copied())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            masks,
            original
                .iter()
                .flat_map(|row| row.get_attention_mask().iter().copied())
                .collect::<Vec<_>>()
        );
        // Preparation disables padding only on the clone.
        assert!(tokenizer.get_padding().is_some());
        assert!(
            documents_tokenizer
                .tokenizer
                .as_ref()
                .unwrap()
                .get_padding()
                .is_none()
        );
        let mut sorted = documents.clone();
        sorted.sort_by_key(PreparedDocument::token_len);
        assert!(
            sorted
                .windows(2)
                .all(|pair| pair[0].token_len() <= pair[1].token_len())
        );
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            documents_tokenizer.prepare(&texts, &control),
            Err(EmbedError::Cancelled)
        ));
    }

    #[test]
    fn prepared_slices_bound_padded_work_and_document_count() {
        let tokens = |count| PreparedDocument {
            input: PreparedDocumentInput::Tokens(vec![1; count]),
        };
        for (length, count) in [(512, 16), (128, 64), (64, 128), (1, 128)] {
            let mut documents = vec![tokens(length); count];
            assert_eq!(prepared_shape(&documents).unwrap(), (count, length));
            documents.push(tokens(length));
            assert!(matches!(
                prepared_shape(&documents),
                Err(EmbedError::InvalidInput(_))
            ));
        }
        // Work uses the longest padded row, not the sum of unpadded lengths.
        let mut mixed = vec![tokens(1); 16];
        mixed.push(tokens(512));
        assert!(matches!(
            prepared_shape(&mixed),
            Err(EmbedError::InvalidInput(_))
        ));
        assert_eq!(prepared_shape(&[]).unwrap(), (0, 0));
    }

    #[test]
    fn injected_embedders_need_only_existing_embed_implementation() {
        struct Injected;
        impl SearchEmbedder for Injected {
            fn embed(
                &self,
                texts: &[String],
                kind: InputKind,
                control: &EmbeddingControl<'_>,
            ) -> Result<Vec<QuantizedVector>, EmbedError> {
                control.check()?;
                assert_eq!(kind, InputKind::Document);
                assert_eq!(texts, &["original text ".to_owned()]);
                Ok(vec![QuantizedVector { values: vec![42] }])
            }
        }
        let model: Box<dyn SearchEmbedder> = Box::new(Injected);
        let cancel = AtomicBool::new(false);
        let control = EmbeddingControl::new(&cancel);
        let documents = model
            .document_tokenizer()
            .prepare(&["original text ".into()], &control)
            .unwrap();
        assert_eq!(documents[0].token_len(), MAX_TOKENS);
        assert_eq!(
            model.embed_prepared(&documents, &control).unwrap()[0].values,
            vec![42]
        );
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            model.embed_prepared(&documents, &control),
            Err(EmbedError::Cancelled)
        ));
    }

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
    fn budget_uses_absolute_clock_and_cancellation_has_precedence() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let start = Instant::now();
        let tick = Arc::new(AtomicUsize::new(0));
        let clock_tick = tick.clone();
        let budget = QueryBudget::new(
            start + Duration::from_secs(3),
            Arc::new(move || {
                start + Duration::from_secs(clock_tick.load(Ordering::Relaxed) as u64)
            }),
        );
        let cancel = AtomicBool::new(false);
        let control = EmbeddingControl::with_budget(&cancel, &budget);
        assert!(control.check().is_ok());
        assert_eq!(budget.remaining(), Duration::from_secs(3));
        tick.store(3, Ordering::Relaxed);
        assert_eq!(budget.remaining(), Duration::ZERO);
        assert!(matches!(control.check(), Err(EmbedError::BudgetExceeded)));
        assert!(!cancel.load(Ordering::Acquire));
        cancel.store(true, Ordering::Release);
        assert!(matches!(control.check(), Err(EmbedError::Cancelled)));
    }

    #[test]
    fn budget_interrupts_file_chunks_without_publishing_identity() {
        use std::sync::atomic::AtomicUsize;
        use std::time::Duration;

        let directory = tempfile::tempdir().unwrap();
        let bytes = vec![42; MODEL_READ_CHUNK + 1];
        std::fs::write(directory.path().join("model.safetensors"), &bytes).unwrap();
        let hash = hex::encode(Sha256::digest(&bytes));
        let start = Instant::now();
        let calls = Arc::new(AtomicUsize::new(0));
        let clock_calls = calls.clone();
        let budget = QueryBudget::new(
            start + Duration::from_secs(1),
            Arc::new(move || {
                if clock_calls.fetch_add(1, Ordering::Relaxed) >= 4 {
                    start + Duration::from_secs(1)
                } else {
                    start
                }
            }),
        );
        let cancel = AtomicBool::new(false);
        let mut verification = ModelVerification::default();
        assert!(matches!(
            verification.read_files(
                directory.path(),
                &[("model.safetensors", hash.as_str())],
                &EmbeddingControl::with_budget(&cancel, &budget),
            ),
            Err(EmbedError::BudgetExceeded)
        ));
        assert!(verification.files.is_none());
        assert!(!cancel.load(Ordering::Acquire));
        let verified = verification
            .read_files(
                directory.path(),
                &[("model.safetensors", hash.as_str())],
                &EmbeddingControl::new(&cancel),
            )
            .unwrap();
        assert_eq!(verified.weights, bytes);
        assert!(verification.files.is_some());
    }

    #[test]
    fn cancellation_and_panics_are_typed() {
        assert!(matches!(
            EmbeddingControl::new(&AtomicBool::new(true)).check(),
            Err(EmbedError::Cancelled)
        ));
        let result: Result<(), EmbedError> = batch_boundary(|| panic!("runtime failure"));
        assert!(matches!(result, Err(EmbedError::Runtime(_))));
    }

    #[test]
    fn descriptor_is_closed_canonical_and_covers_every_verified_file() {
        let descriptor = SUPPORTED_MODEL.descriptor("test-card-format.v1");
        let object = descriptor.as_object().unwrap();
        assert_eq!(object.len(), 11);
        let files = descriptor["modelFiles"].as_array().unwrap();
        assert_eq!(files.len(), 9);
        let paths: Vec<_> = files.iter().map(|v| v["path"].as_str().unwrap()).collect();
        assert!(
            paths
                .windows(2)
                .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
        );
        for file in files {
            assert_eq!(file.as_object().unwrap().len(), 2);
            let hash = file["sha256"].as_str().unwrap();
            assert_eq!(hash.len(), 64);
            assert!(
                hash.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            );
        }
        let original = model_id_for_card_format("test-card-format.v1");
        assert_eq!(
            original,
            "165233b0aa2514c3b565cb92173ce56c9c2dc2a7067cf8edaf2df31df60b55d3"
        );
        for key in object.keys() {
            let mut changed = descriptor.clone();
            changed[key] = serde_json::Value::Null;
            assert_ne!(
                original,
                crate::native_ids::digest(
                    b"trellis.search-model.v1\0",
                    &crate::native_ids::canonical(&changed)
                ),
                "descriptor field {key} must bind the cache identity"
            );
        }
        for index in 0..files.len() {
            let mut changed = descriptor.clone();
            changed["modelFiles"][index]["sha256"] = serde_json::json!("0".repeat(64));
            assert_ne!(
                original,
                crate::native_ids::digest(
                    b"trellis.search-model.v1\0",
                    &crate::native_ids::canonical(&changed)
                )
            );
        }
        assert_eq!(original, model_id_for_card_format("test-card-format.v1"));
    }

    #[test]
    fn model_jobs_rehash_only_when_a_file_identity_changes() {
        let cancel = AtomicBool::new(false);
        let control = EmbeddingControl::new(&cancel);
        let directory = tempfile::tempdir().unwrap();
        let names = [
            "config.json",
            "tokenizer.json",
            "model.safetensors",
            "provenance.py",
        ];
        let hashes = names
            .iter()
            .map(|name| {
                std::fs::write(directory.path().join(name), name).unwrap();
                hex::encode(Sha256::digest(name.as_bytes()))
            })
            .collect::<Vec<_>>();
        let pins = names
            .iter()
            .zip(&hashes)
            .map(|(name, hash)| (*name, hash.as_str()))
            .collect::<Vec<_>>();
        let mut verification = ModelVerification::default();
        let first = verification
            .read_files(directory.path(), &pins, &control)
            .unwrap();
        assert_eq!(first.weights, b"model.safetensors");
        drop(first);
        let second = verification
            .read_files(directory.path(), &pins, &control)
            .unwrap();
        assert_eq!(second.weights, b"model.safetensors");
        assert_eq!(verification.verifications, 1);
        assert!(
            verification
                .unchanged_files(directory.path(), &pins, &control)
                .unwrap()
        );
        std::fs::write(directory.path().join("provenance.py"), b"changed-file").unwrap();
        assert!(
            !verification
                .unchanged_files(directory.path(), &pins, &control)
                .unwrap()
        );
        assert!(matches!(
            verification.read_files(directory.path(), &pins, &control),
            Err(EmbedError::ModelUnavailable(_))
        ));
        assert_eq!(verification.verifications, 2);
        assert!(verification.files.is_none());
    }

    #[test]
    fn verification_refuses_missing_or_modified_inputs_and_retains_owned_bytes() {
        let cancel = AtomicBool::new(false);
        let control = EmbeddingControl::new(&cancel);
        let temp = tempfile::tempdir().unwrap();
        let names = [
            "config.json",
            "tokenizer.json",
            "model.safetensors",
            "provenance.py",
        ];
        let hashes: Vec<_> = names
            .iter()
            .map(|name| {
                std::fs::write(temp.path().join(name), name.as_bytes()).unwrap();
                hex::encode(Sha256::digest(name.as_bytes()))
            })
            .collect();
        let pins: Vec<_> = names
            .iter()
            .zip(&hashes)
            .map(|(name, hash)| (*name, hash.as_str()))
            .collect();
        let verified = verify_files(temp.path(), &pins, &control).unwrap();
        std::fs::write(temp.path().join("config.json"), b"changed").unwrap();
        assert_eq!(verified.config, b"config.json");
        assert_eq!(verified.tokenizer, b"tokenizer.json");
        assert_eq!(verified.weights, b"model.safetensors");
        assert!(matches!(
            verify_files(temp.path(), &pins, &control),
            Err(EmbedError::ModelUnavailable(_))
        ));
        std::fs::write(temp.path().join("config.json"), b"config.json").unwrap();
        std::fs::write(temp.path().join("provenance.py"), b"changed").unwrap();
        assert!(matches!(
            verify_files(temp.path(), &pins, &control),
            Err(EmbedError::ModelUnavailable(_))
        ));
        std::fs::remove_file(temp.path().join("provenance.py")).unwrap();
        assert!(matches!(
            verify_files(temp.path(), &pins, &control),
            Err(EmbedError::ModelUnavailable(_))
        ));
    }
}
