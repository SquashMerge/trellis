//! The origin of a batch Trellis generated itself (#21), as descriptors and queued requests
//! record it.
use serde::{Deserialize, Serialize};

/// Which Trellis-owned flow wrote a generated batch. It names the private directory the
/// batch is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GenerationOrigin {
    #[serde(rename = "trellisRun")]
    TrellisRun,
    #[serde(rename = "ownerPickup")]
    OwnerPickup,
}

impl GenerationOrigin {
    /// The directory under the producer state directory that holds this origin's batches.
    pub fn subdir(self) -> &'static str {
        match self {
            Self::TrellisRun => "runs",
            Self::OwnerPickup => "pickups",
        }
    }
}
