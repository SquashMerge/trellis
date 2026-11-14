//! Re-admits retained raw rows through the bounded stage and the original SCIP decoder.
#[path = "retained_continuation.rs"]
mod continuation;
use super::{
    AdmittedBatch, BatchCounters, BatchLimits,
    admit::{Passes, ReaderSource, StageOwner, check_cancel},
    descriptor::{Descriptor, RowOrder, SourceManifestHasher, SourceRow},
    limits::IO_BUFFER_BYTES,
};
use crate::{
    model::CancelFlag,
    scip_admission::admission,
    scip_raw::{self, ScipCaptureError},
    store::{
        NativeReader,
        staging::{Stage, artifact_file},
    },
};
use anyhow::{Result, ensure};
pub(crate) use continuation::RetainedStageContinuation;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::Path,
};
