//! A source's recorded charge requests, held compactly between builds (#1313).
//!
//! A retained core keeps one plan per source so a later build can replay the source's
//! charges against fresh catalogs (#1301). Each request used to be a full [`ChargeSite`]
//! with its own heap strings. A plan now stores fixed-size records, every string once in one
//! text arena per source, and its static labels in a small table. Replay rebuilds each
//! request exactly as it was recorded, so charges, answers and the canonical form are
//! unchanged.
use super::rust::GlobUse;
use super::shared::{ChargeSite, SemanticCharge, SemanticChargeOutcome, StructuralCharge};
use super::{RawSite, ReadContext, incremental::normalize_document};
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;

/// One source's charge requests, in site order.
#[derive(Clone)]
pub(crate) struct SourceChargePlan {
    pub(crate) prepared_work: usize,
    sites: Box<[PlannedCharge]>,
    /// Destination path segments, as spans of `text`.
    parts: Box<[Span]>,
    labels: Box<[&'static str]>,
    text: Box<str>,
    /// What a site's Rust resolution read in its source's tree (#1315), by site index; only
    /// private-glob sites have one, so the table is sparse.
    globs: Box<[(u32, GlobUse)]>,
}

/// `len` bytes of the plan's text arena from `start`.
#[derive(Clone, Copy)]
struct Span {
    start: u32,
    len: u32,
}

#[derive(Clone, Copy)]
struct PlannedCharge {
    structural: PlannedStructural,
    semantic: Option<PlannedSemantic>,
}

#[derive(Clone, Copy)]
enum PlannedStructural {
    None,
    OwnerAmbiguous,
    Resolve(PlannedRaw),
}

/// A [`RawSite`]: its strings are arena spans, its labels indices into the label table.
#[derive(Clone, Copy)]
struct PlannedRaw {
    start: u32,
    end: u32,
    spelling: Span,
    kind: u16,
    namespace: Option<u8>,
    destination: PlannedDestination,
}

#[derive(Clone, Copy)]
enum PlannedDestination {
    /// `count` segments of the plan's `parts` from `first`.
    Path {
        first: u32,
        count: u32,
    },
    Unproved(u16),
}

#[derive(Clone, Copy)]
struct PlannedSemantic {
    outcome: PlannedOutcome,
    historical: bool,
}

#[derive(Clone, Copy)]
enum PlannedOutcome {
    Boundary,
    External,
    Ambiguous,
    Unresolved,
    Internal { document: u32, sid: Span },
}

/// Builds one [`SourceChargePlan`] as a source's sites are charged.
pub(super) struct ChargePlanBuilder {
    prepared_work: usize,
    sites: Vec<PlannedCharge>,
    parts: Vec<Span>,
    labels: Vec<&'static str>,
    text: String,
    globs: Vec<(u32, GlobUse)>,
    /// Each distinct string's span, so a repeated spelling, segment or target id is stored once.
    spans: HashMap<Box<str>, Span>,
}

impl ChargePlanBuilder {
    pub(super) fn new(prepared_work: usize) -> Self {
        Self {
            prepared_work,
            sites: Vec::new(),
            parts: Vec::new(),
            labels: Vec::new(),
            text: String::new(),
            globs: Vec::new(),
            spans: HashMap::new(),
        }
    }

    /// Records one request; an internal outcome's document is normalized to sorted order.
    pub(super) fn push(&mut self, read: ReadContext<'_, '_>, mut site: ChargeSite) -> Result<()> {
        if let Some(SemanticCharge {
            outcome: SemanticChargeOutcome::Internal { document, sid: _ },
            historical: _,
        }) = &mut site.semantic
        {
            *document = normalize_document(read, *document as usize)?;
        }
        self.record(site)
    }

    fn record(&mut self, site: ChargeSite) -> Result<()> {
        let ChargeSite {
            structural,
            semantic,
            glob,
        } = site;
        if let Some(glob) = glob {
            self.globs.push((u32::try_from(self.sites.len())?, glob));
        }
        let structural = match structural {
            StructuralCharge::None => PlannedStructural::None,
            StructuralCharge::OwnerAmbiguous => PlannedStructural::OwnerAmbiguous,
            StructuralCharge::Resolve(RawSite {
                start,
                end,
                kind,
                spelling,
                destination,
                namespace,
            }) => PlannedStructural::Resolve(PlannedRaw {
                start: u32::try_from(start)?,
                end: u32::try_from(end)?,
                spelling: self.span(&spelling)?,
                kind: self.label(kind)?,
                namespace,
                destination: match destination {
                    Ok(path) => {
                        let first = u32::try_from(self.parts.len())?;
                        for segment in &path {
                            let span = self.span(segment)?;
                            self.parts.push(span);
                        }
                        PlannedDestination::Path {
                            first,
                            count: u32::try_from(path.len())?,
                        }
                    }
                    Err(reason) => PlannedDestination::Unproved(self.label(reason)?),
                },
            }),
        };
        let semantic = match semantic {
            None => None,
            Some(SemanticCharge {
                outcome,
                historical,
            }) => Some(PlannedSemantic {
                outcome: match outcome {
                    SemanticChargeOutcome::Boundary => PlannedOutcome::Boundary,
                    SemanticChargeOutcome::External => PlannedOutcome::External,
                    SemanticChargeOutcome::Ambiguous => PlannedOutcome::Ambiguous,
                    SemanticChargeOutcome::Unresolved => PlannedOutcome::Unresolved,
                    SemanticChargeOutcome::Internal { document, sid } => PlannedOutcome::Internal {
                        document,
                        sid: self.span(&sid)?,
                    },
                },
                historical,
            }),
        };
        self.sites.push(PlannedCharge {
            structural,
            semantic,
        });
        Ok(())
    }

    fn span(&mut self, text: &str) -> Result<Span> {
        if let Some(span) = self.spans.get(text) {
            return Ok(*span);
        }
        let span = Span {
            start: u32::try_from(self.text.len())?,
            len: u32::try_from(text.len())?,
        };
        self.text.push_str(text);
        u32::try_from(self.text.len()).context("charge plan text overflow")?;
        self.spans.insert(text.into(), span);
        Ok(span)
    }

    fn label(&mut self, label: &'static str) -> Result<u16> {
        let index = match self.labels.iter().position(|known| *known == label) {
            Some(index) => index,
            None => {
                self.labels.push(label);
                self.labels.len() - 1
            }
        };
        Ok(u16::try_from(index)?)
    }

    pub(super) fn finish(self) -> SourceChargePlan {
        let Self {
            prepared_work,
            sites,
            parts,
            labels,
            text,
            globs,
            spans: _,
        } = self;
        SourceChargePlan {
            prepared_work,
            sites: sites.into_boxed_slice(),
            parts: parts.into_boxed_slice(),
            labels: labels.into_boxed_slice(),
            text: text.into_boxed_str(),
            globs: globs.into_boxed_slice(),
        }
    }
}

impl SourceChargePlan {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.sites.len()
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn clear_for_tests(&mut self) {
        self.sites = Box::new([]);
        self.globs = Box::new([]);
    }

    /// The recorded requests, rebuilt in site order.
    pub(crate) fn sites(&self) -> impl Iterator<Item = ChargeSite> + '_ {
        self.sites
            .iter()
            .enumerate()
            .map(|(index, site)| self.charge(index, site))
    }

    fn text(&self, span: Span) -> &str {
        &self.text[span.start as usize..(span.start + span.len) as usize]
    }

    fn charge(&self, index: usize, site: &PlannedCharge) -> ChargeSite {
        ChargeSite {
            glob: self
                .globs
                .binary_search_by_key(&index, |(at, _)| *at as usize)
                .ok()
                .map(|found| self.globs[found].1.clone()),
            structural: match site.structural {
                PlannedStructural::None => StructuralCharge::None,
                PlannedStructural::OwnerAmbiguous => StructuralCharge::OwnerAmbiguous,
                PlannedStructural::Resolve(raw) => StructuralCharge::Resolve(RawSite {
                    start: raw.start as usize,
                    end: raw.end as usize,
                    kind: self.labels[usize::from(raw.kind)],
                    spelling: self.text(raw.spelling).to_owned(),
                    destination: match raw.destination {
                        PlannedDestination::Path { first, count } => Ok(self.parts
                            [first as usize..(first + count) as usize]
                            .iter()
                            .map(|&span| self.text(span).to_owned())
                            .collect()),
                        PlannedDestination::Unproved(reason) => {
                            Err(self.labels[usize::from(reason)])
                        }
                    },
                    namespace: raw.namespace,
                }),
            },
            semantic: site.semantic.map(|semantic| SemanticCharge {
                outcome: match semantic.outcome {
                    PlannedOutcome::Boundary => SemanticChargeOutcome::Boundary,
                    PlannedOutcome::External => SemanticChargeOutcome::External,
                    PlannedOutcome::Ambiguous => SemanticChargeOutcome::Ambiguous,
                    PlannedOutcome::Unresolved => SemanticChargeOutcome::Unresolved,
                    PlannedOutcome::Internal { document, sid } => SemanticChargeOutcome::Internal {
                        document,
                        sid: self.text(sid).to_owned(),
                    },
                },
                historical: semantic.historical,
            }),
        }
    }
}

/// Serialized exactly as the requests it records, for canonical comparison.
impl Serialize for SourceChargePlan {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut plan = serializer.serialize_struct("SourceChargePlan", 2)?;
        plan.serialize_field("prepared_work", &self.prepared_work)?;
        plan.serialize_field("sites", &self.sites().collect::<Vec<_>>())?;
        plan.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(
        start: usize,
        kind: &'static str,
        spelling: &str,
        destination: Result<Vec<&str>, &'static str>,
    ) -> RawSite {
        RawSite {
            start,
            end: start + spelling.len(),
            kind,
            spelling: spelling.into(),
            destination: destination.map(|path| path.into_iter().map(String::from).collect()),
            namespace: start.is_multiple_of(2).then_some(1),
        }
    }

    fn internal(document: u32, sid: &str, historical: bool) -> Option<SemanticCharge> {
        Some(SemanticCharge {
            outcome: SemanticChargeOutcome::Internal {
                document,
                sid: sid.into(),
            },
            historical,
        })
    }

    /// Every shape a request can take, with repeated strings and labels.
    fn requests() -> Vec<ChargeSite> {
        let outcome = |outcome| {
            Some(SemanticCharge {
                outcome,
                historical: false,
            })
        };
        vec![
            ChargeSite {
                structural: StructuralCharge::Resolve(raw(
                    0,
                    "import",
                    "crate::a::f",
                    Ok(vec!["crate", "a", "f"]),
                )),
                semantic: None,
                glob: Some(GlobUse::for_tests(3)),
            },
            ChargeSite {
                structural: StructuralCharge::Resolve(raw(13, "reference", "a::f", Ok(vec![]))),
                semantic: internal(2, "s:a.rs#f", false),
                glob: None,
            },
            ChargeSite {
                structural: StructuralCharge::Resolve(raw(
                    31,
                    "import",
                    "crate::a::f",
                    Ok(vec!["crate", "a", "f"]),
                )),
                semantic: internal(2, "s:a.rs#f", true),
                glob: None,
            },
            ChargeSite {
                structural: StructuralCharge::Resolve(raw(
                    48,
                    "import",
                    "./x",
                    Err("javascript_import_unproved"),
                )),
                semantic: outcome(SemanticChargeOutcome::Boundary),
                glob: Some(GlobUse::for_tests(0)),
            },
            ChargeSite {
                structural: StructuralCharge::None,
                semantic: outcome(SemanticChargeOutcome::External),
                glob: None,
            },
            ChargeSite {
                structural: StructuralCharge::OwnerAmbiguous,
                semantic: outcome(SemanticChargeOutcome::Ambiguous),
                glob: None,
            },
            ChargeSite {
                structural: StructuralCharge::None,
                semantic: outcome(SemanticChargeOutcome::Unresolved),
                glob: None,
            },
            ChargeSite {
                structural: StructuralCharge::Resolve(raw(
                    60,
                    "import",
                    "",
                    Err("javascript_import_unproved"),
                )),
                semantic: None,
                glob: None,
            },
        ]
    }

    #[test]
    fn a_plan_replays_every_request_exactly_as_recorded() {
        let mut builder = ChargePlanBuilder::new(7);
        for site in requests() {
            builder.record(site).unwrap();
        }
        let plan = builder.finish();
        assert_eq!(plan.prepared_work, 7);
        assert_eq!(plan.len(), requests().len());
        assert_eq!(
            serde_json::to_value(plan.sites().collect::<Vec<_>>()).unwrap(),
            serde_json::to_value(requests()).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&plan).unwrap(),
            serde_json::json!({"prepared_work": 7, "sites": requests()})
        );
        assert!(ChargePlanBuilder::new(0).finish().sites().next().is_none());
    }

    #[test]
    fn a_plan_stores_each_distinct_string_and_label_once() {
        let mut builder = ChargePlanBuilder::new(0);
        for site in requests() {
            builder.record(site).unwrap();
        }
        let plan = builder.finish();
        let text = &plan.text;
        for repeated in ["crate::a::f", "s:a.rs#f"] {
            assert_eq!(text.matches(repeated).count(), 1, "{repeated}");
        }
        assert_eq!(
            plan.labels.as_ref(),
            ["import", "reference", "javascript_import_unproved"]
        );
    }
}
