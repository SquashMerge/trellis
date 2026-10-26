//! Identity of this binary, not an index or a selected runtime backend.
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BuildInfo {
    pub version: &'static str,
    pub target: &'static str,
    pub features: Vec<&'static str>,
    pub commit: &'static str,
    pub toolchain: &'static str,
    pub embedding_capabilities: &'static [&'static str],
}

/// Compiled capabilities only. Device availability and automatic CPU fallback
/// belong to the embedding runtime, not build identity or search coverage.
pub fn current() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        target: env!("TRELLIS_BUILD_TARGET"),
        features: env!("TRELLIS_BUILD_FEATURES")
            .split(',')
            .filter(|feature| !feature.is_empty())
            .collect(),
        commit: env!("TRELLIS_BUILD_COMMIT"),
        toolchain: env!("TRELLIS_BUILD_TOOLCHAIN"),
        embedding_capabilities: if cfg!(feature = "metal") {
            &["cpu", "metal"]
        } else {
            &["cpu"]
        },
    }
}

/// Clap and daemon status are projections of the same compiled descriptor.
pub fn version() -> &'static str {
    static VERSION: OnceLock<String> = OnceLock::new();
    VERSION.get_or_init(|| {
        let build = current();
        format!(
            "{} (target={}; features=[{}]; commit={}; toolchain={}; embeddingCapabilities=[{}])",
            build.version,
            build.target,
            build.features.join(","),
            build.commit,
            build.toolchain,
            build.embedding_capabilities.join(","),
        )
    })
}
