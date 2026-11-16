//! Per-language partial-coverage reasons for the import report.
use crate::semantic_evidence as semantic;

/// JSON bytes of the partial documents one language of an import report lists with their
/// reasons (#520). Further partial documents are only counted, so the four languages stay well
/// inside the report's 1 MiB bound.
pub(crate) const REPORT_PARTIAL_DOCUMENT_BYTES: usize = 64 * 1024;

/// One language's coverage reasons in an import report (#520): partial documents with each
/// reason, unsupported occurrences by native gap kind, and the partial documents in coverage
/// order up to [`REPORT_PARTIAL_DOCUMENT_BYTES`]. The in-memory and staged joins feed it the
/// same rows in the same order.
#[derive(Default)]
pub(crate) struct LanguageReasons {
    documents: u64,
    native_gaps: u64,
    native_gaps_only: u64,
    capped: u64,
    omitted: u64,
    captured_diagnostics: u64,
    occurrences: [u64; 4],
    listed: Vec<serde_json::Value>,
    listed_bytes: usize,
    /// Set by the first document that did not fit, so later ones are never listed after it.
    full: bool,
}

impl LanguageReasons {
    pub(crate) fn add(&mut self, row: &semantic::Coverage) {
        let Some(reasons) = &row.reasons else {
            return;
        };
        let gaps = reasons.native_gaps() > 0;
        self.documents += 1;
        self.native_gaps += u64::from(gaps);
        self.native_gaps_only +=
            u64::from(gaps && !reasons.capped && !reasons.omitted && !reasons.captured_diagnostics);
        self.capped += u64::from(reasons.capped);
        self.omitted += u64::from(reasons.omitted);
        self.captured_diagnostics += u64::from(reasons.captured_diagnostics);
        for (total, count) in self.occurrences.iter_mut().zip(reasons.gaps()) {
            *total = total.saturating_add(count);
        }
        if self.full {
            return;
        }
        let entry = serde_json::json!({
            "path": row.document_path, "diagnostic": row.diagnostic, "reasons": reasons,
        });
        // The serialized array is `[` + entries joined by `,` + `]`: each entry plus its
        // separator or opening bracket, plus the closing bracket, must fit the bound.
        let bytes = entry.to_string().len() + 1;
        if self.listed_bytes + bytes < REPORT_PARTIAL_DOCUMENT_BYTES {
            self.listed_bytes += bytes;
            self.listed.push(entry);
        } else {
            self.full = true;
        }
    }

    /// The language's `partialReasons` report object.
    pub(crate) fn json(self) -> serde_json::Value {
        let occurrences: serde_json::Map<_, _> = semantic::GAP_KINDS
            .iter()
            .zip(self.occurrences)
            .map(|(kind, count)| ((*kind).to_owned(), count.into()))
            .collect();
        let unlisted = self.documents - self.listed.len() as u64;
        serde_json::json!({
            "documents": self.documents, "nativeGaps": self.native_gaps,
            "nativeGapsOnly": self.native_gaps_only, "capped": self.capped,
            "omitted": self.omitted, "capturedDiagnostics": self.captured_diagnostics,
            "unsupportedOccurrences": occurrences,
            "listed": self.listed, "unlisted": unlisted,
        })
    }
}

#[cfg(test)]
mod language_reason_tests {
    use super::*;

    fn partial(path: String) -> semantic::Coverage {
        semantic::Coverage {
            producer_id: "p".into(),
            language: semantic::Language::Rust,
            source_set_id: "s".into(),
            document_path: path,
            revision_id: "r".into(),
            requested: true,
            selected: true,
            state: semantic::CoverageState::Partial,
            supported_roles: vec![],
            observed_roles: vec![],
            diagnostic: Some("unsupported_native_gap".into()),
            reasons: Some(semantic::CoverageReasons::new(
                [2, 1, 0, 0],
                false,
                false,
                false,
            )),
        }
    }

    /// The listed documents stop at the byte bound; every partial document is still counted.
    #[test]
    fn report_lists_partial_documents_up_to_its_byte_bound() {
        let mut reasons = LanguageReasons::default();
        for i in 0..2000 {
            reasons.add(&partial(format!("src/module_{i:04}.rs")));
        }
        let json = reasons.json();
        let listed = json["listed"].as_array().unwrap();
        assert!(listed.len() < 2000 && !listed.is_empty());
        assert!(json["listed"].to_string().len() <= REPORT_PARTIAL_DOCUMENT_BYTES);
        assert_eq!(json["unlisted"], 2000 - listed.len() as u64);
        assert_eq!(listed[0]["path"], "src/module_0000.rs");
        assert_eq!(json["documents"], 2000);
        assert_eq!(json["nativeGapsOnly"], 2000);
        assert_eq!(json["unsupportedOccurrences"]["macroInvocation"], 4000);
        assert_eq!(json["unsupportedOccurrences"]["attributeItem"], 2000);
    }

    /// The bound counts the array's brackets and commas: 316 entries whose separators and
    /// opening bracket total exactly the bound would serialize one byte over it with the
    /// closing bracket, so the last one is left unlisted.
    #[test]
    fn report_listing_bound_counts_array_framing() {
        let mut reasons = LanguageReasons::default();
        for i in 0..316 {
            let mut path = format!("src/module_{i:04}.rs");
            if i == 315 {
                path = format!("src/module_{i:04}{}.rs", "x".repeat(124));
            }
            reasons.add(&partial(path));
        }
        let json = reasons.json();
        assert!(json["listed"].to_string().len() <= REPORT_PARTIAL_DOCUMENT_BYTES);
        assert_eq!(json["listed"].as_array().unwrap().len(), 315);
        assert_eq!(json["unlisted"], 1);
    }
}
