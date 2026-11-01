//! Language-neutral static possible-path sequence contract.
//! Adapters consume cached source and measured calls. This is not a runtime trace.
use crate::model::{CallSite, IndexPin, Resolution, SourceFile, SourceRange, Symbol};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SequenceView {
    pub revision: IndexPin,
    pub seed: Symbol,
    pub participants: Vec<Participant>,
    pub steps: Vec<SequenceStep>,
    pub warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub semantic_warnings: Vec<crate::graph_query::Warning>,
    /// Which evidence built the participants and targets, and why syntax was used (#522).
    #[serde(default)]
    pub semantic: SequenceSemantic,
    pub hidden_steps: usize,
    /// Calls bound to standard-library or other external declarations (#522). They are
    /// hidden (and counted in `hidden_steps`) unless `showAll` is set.
    #[serde(default)]
    pub incidental_steps: usize,
    pub truncated: bool,
    /// The bounds that truncated this view (#536), when known: omitted when none did.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub truncated_by: Vec<SequenceLimit>,
}
/// A bound that truncated a sequence view (#536).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum SequenceLimit {
    /// [`MAX_STEPS`] shown steps: hidden steps do not count.
    VisibleSteps,
    /// [`MAX_WORK_STEPS`] steps built or expanded, hidden ones included.
    WorkSteps,
    /// [`MAX_STEP_DEPTH`] step levels.
    Depth,
    /// 20 participant columns (#558): many methods of one type share a column.
    Participants,
}
impl SequenceView {
    /// Marks the view truncated by `limit`.
    pub(crate) fn limit(&mut self, limit: SequenceLimit) {
        self.truncated = true;
        if let Err(at) = self.truncated_by.binary_search(&limit) {
            self.truncated_by.insert(at, limit);
        }
    }
}
/// The evidence tier of a sequence view (#522). `semantic` names the selected producer
/// whose fresh bindings built the participants; `syntax` carries the reason it fell back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SequenceSemantic {
    pub tier: EvidenceTier,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
impl Default for SequenceSemantic {
    fn default() -> Self {
        Self::syntax(None)
    }
}
impl SequenceSemantic {
    pub fn syntax(reason: Option<String>) -> Self {
        Self {
            tier: EvidenceTier::Syntax,
            producer_id: None,
            reason,
        }
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EvidenceTier {
    Syntax,
    Semantic,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Participant {
    pub id: String,
    pub label: String,
    pub kind: String,
    #[serde(default)]
    pub identification: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declaration: Option<Symbol>,
    /// The type or module whose column this participant is drawn in (#558). Set for the
    /// seed and every exact target of an enriched view; a syntax hint has none and keeps
    /// its own column. This participant stays the exact identity of the call's target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<SequenceColumn>,
}
/// One drawn column of a sequence (#558): a type or a module, holding one or more
/// participants. A display grouping: it proves nothing about receivers at runtime.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SequenceColumn {
    /// Identity within the view: equal for two participants of one type or module.
    pub id: String,
    pub label: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SequenceStep {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub path: String,
    pub range: SourceRange,
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<Resolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_evidence: Option<CallEvidence>,
    pub children: Vec<SequenceStep>,
    pub alternate: Vec<SequenceStep>,
    pub hidden: bool,
    /// The participant whose body this step belongs to, when it is not the seed's: steps of
    /// an expanded direct/constructor callee (#522). Absent for the seed's own steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lane: Option<String>,
    /// Control-flow steps folded into this step. On a call (#542): a Rust `?` early-return
    /// branch (its `alternate` keeps the early return) and the "only if the preceding path
    /// continues normally" guard that followed it, whose `children` now follow this step
    /// directly. On a branch that draws an exit (#558): that guard. The step is marked as
    /// one that may leave the path; nothing is dropped. Folded steps are not counted
    /// against any step bound but the work bound.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub folded: Vec<SequenceStep>,
    /// Why this step is incidental (#543): hidden unless `showAll`, and counted in
    /// `incidentalSteps`. Absent for a step that is not incidental.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incidental: Option<IncidentalReason>,
}
/// Why a step is incidental (#536, #543).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum IncidentalReason {
    /// A standalone call bound to an external declaration (the standard library included).
    External,
    /// A logging call or macro, or a step evaluating its arguments: a naming rule.
    Logging,
    /// A call whose fresh exact target's body only returns or assigns one field.
    Accessor,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CallEvidence {
    pub boundary_reason: crate::graph_query::BoundaryReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<crate::semantic_evidence::Resolution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<crate::semantic_evidence::Dispatch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<crate::semantic_evidence::Freshness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance_id: Option<String>,
    /// The package an `external` binding's SCIP symbol names (#450). A producer claim
    /// for in-context library lookup, never a resolved library declaration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_package: Option<ExternalPackage>,
    /// The captured basis of the selected binding's provenance (#522), as MCP reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic_basis: Option<crate::semantic_evidence::SemanticBasis>,
}
/// The `<manager> <name> <version>` package of a global SCIP symbol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExternalPackage {
    pub manager: String,
    pub name: String,
    pub version: Option<String>,
}
impl ExternalPackage {
    /// Reads `<scheme> <manager> <name> <version> <descriptors>`: a doubled space escapes
    /// a space and `.` is an empty field. Local or malformed symbols, and symbols without
    /// a manager or name, carry no package.
    pub fn from_scip(symbol: &str) -> Option<Self> {
        if symbol.starts_with("local ") {
            return None;
        }
        let mut fields: Vec<String> = Vec::with_capacity(4);
        let mut field = String::new();
        let mut chars = symbol.char_indices().peekable();
        while let Some((at, ch)) = chars.next() {
            if ch != ' ' {
                field.push(ch);
            } else if chars.peek().is_some_and(|(_, next)| *next == ' ') {
                chars.next();
                field.push(' ');
            } else {
                fields.push(std::mem::take(&mut field));
                if fields.len() == 4 {
                    // One or more well-formed descriptors must follow the package.
                    if !scip_descriptors(&symbol[at + 1..]) {
                        return None;
                    }
                    let present =
                        |value: &str| (!value.is_empty() && value != ".").then(|| value.to_owned());
                    if fields[0].is_empty() {
                        return None;
                    }
                    return Some(Self {
                        manager: present(&fields[1])?,
                        name: present(&fields[2])?,
                        version: present(&fields[3]),
                    });
                }
            }
        }
        None
    }
}
/// Whether `tail` is one or more SCIP descriptors and nothing else:
/// `name/` `name#` `name.` `name:` `name!` `name(disambiguator?).` `[name]` `(name)`,
/// where a name is `[_+$A-Za-z0-9-]+` or a backtick-quoted identifier with `` `` `` for a
/// backtick. Linear in `tail`; never panics.
fn scip_descriptors(tail: &str) -> bool {
    fn simple(bytes: &[u8], mut at: usize) -> Option<usize> {
        let start = at;
        while at < bytes.len()
            && (bytes[at].is_ascii_alphanumeric() || matches!(bytes[at], b'_' | b'+' | b'-' | b'$'))
        {
            at += 1;
        }
        (at > start).then_some(at)
    }
    fn name(bytes: &[u8], at: usize) -> Option<usize> {
        if bytes.get(at) != Some(&b'`') {
            return simple(bytes, at);
        }
        let mut at = at + 1;
        let mut empty = true;
        loop {
            match bytes.get(at)? {
                b'`' if bytes.get(at + 1) == Some(&b'`') => at += 2,
                b'`' => return (!empty).then_some(at + 1),
                _ => at += 1,
            }
            empty = false;
        }
    }
    let bytes = tail.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        at = match bytes[at] {
            b'[' => match name(bytes, at + 1) {
                Some(end) if bytes.get(end) == Some(&b']') => end + 1,
                _ => return false,
            },
            b'(' => match name(bytes, at + 1) {
                Some(end) if bytes.get(end) == Some(&b')') => end + 1,
                _ => return false,
            },
            _ => {
                let Some(end) = name(bytes, at) else {
                    return false;
                };
                match bytes.get(end) {
                    Some(b'/' | b'#' | b'.' | b':' | b'!') => end + 1,
                    Some(b'(') => {
                        let close = simple(bytes, end + 1).unwrap_or(end + 1);
                        if bytes.get(close..close + 2) != Some(b").") {
                            return false;
                        }
                        close + 2
                    }
                    _ => return false,
                }
            }
        };
    }
    at > 0
}
/// Builds the syntax view of `seed` within the work bound and depth, and then within the
/// visible step bound: the view a sequence without semantic enrichment shows.
pub fn build_sequence(
    revision: IndexPin,
    seed: &Symbol,
    file: &SourceFile,
    calls: &[CallSite],
    show_all: bool,
) -> Result<SequenceView> {
    let mut view = build_sequence_work(revision, seed, file, calls, show_all)?;
    cap_visible(&mut view, MAX_STEPS);
    Ok(view)
}

/// Builds the syntax view of `seed` within [`MAX_WORK_STEPS`] and [`MAX_STEP_DEPTH`], but
/// not yet within [`MAX_STEPS`]: semantic enrichment hides incidental calls first (#536),
/// then applies the visible bound with [`cap_visible`].
pub(crate) fn build_sequence_work(
    revision: IndexPin,
    seed: &Symbol,
    file: &SourceFile,
    calls: &[CallSite],
    show_all: bool,
) -> Result<SequenceView> {
    let mut view = match file.language.as_str() {
        "javascript" => crate::behavior_js::build(revision, seed, file, calls, show_all),
        "rust" => crate::behavior_rust::build(revision, seed, file, calls, show_all),
        "java" => crate::behavior_java::build(revision, seed, file, calls, show_all),
        "python" => crate::behavior_python::build(revision, seed, file, calls, show_all),
        _ => bail!("unsupported sequence language: {}", file.language),
    }?;
    cap_depth(&mut view);
    crate::behavior_incidental::mark_logging(&mut view, file, show_all);
    Ok(view)
}

/// The step levels a view may carry: top-level steps are at depth 0, the deepest at 23.
pub const MAX_STEP_DEPTH: usize = 24;
/// The drawn participant columns of one view, the seed's included (#558). Participants are
/// bounded by the steps that target them; a target of a column already drawn costs none.
pub const MAX_COLUMNS: usize = 20;
/// The visible step bound of one view, expanded callee bodies included. Hidden steps (an
/// incidental call, a hidden logging wrapper) do not count (#536).
pub const MAX_STEPS: usize = 200;
/// The hard work bound: the steps an adapter builds for one body, and the steps one view
/// builds and expands in all, hidden ones included (#536). It keeps hostile input bounded
/// however many of its steps are hidden.
pub const MAX_WORK_STEPS: usize = 2000;
/// The seed's own shown steps when it has a resolved call target (#542): the rest of
/// [`MAX_STEPS`] is reserved for the callee bodies its first calls expand.
pub const SEED_STEPS: usize = 150;
pub(crate) const BOUNDS_WARNING: &str = "Sequence bounds reached (2000 steps of work, 20 participants, depth 24); omitted behavior is unknown.";
pub(crate) const VISIBLE_WARNING: &str =
    "Sequence step limit reached (200 shown steps); later behavior is not shown.";
pub(crate) const SEED_WARNING: &str = "Sequence step limit reached (150 shown steps of the method, 50 reserved for callee bodies); later behavior is not shown.";

/// Marks an adapter's view truncated, typed as the work bound when `count` reached it.
pub(crate) fn adapter_limit(view: &mut SequenceView, count: usize) {
    view.truncated = true;
    if count >= MAX_WORK_STEPS {
        view.limit(SequenceLimit::WorkSteps);
    }
}

pub(crate) const TRY_LABEL: &str = "?: continue only on success; otherwise early return";
pub(crate) const CONTINUES_LABEL: &str = "only if the preceding path continues normally";

/// A measured call step, drawn as a call or, when its source-hint lanes were full, as a
/// boundary: it may carry a folded `?` (#542).
pub(crate) fn measured_call(step: &SequenceStep) -> bool {
    matches!(step.kind.as_str(), "call" | "boundary")
        && step.call_id.is_some()
        && step.children.is_empty()
        && step.alternate.is_empty()
}

/// Whether `steps` draw a way off the path: a `return` or `throw` step, or a call carrying
/// a folded `?`. An opaque boundary, a `break` and an unbounded loop may leave it too, but
/// they say nothing the reader can follow, so they do not count.
fn draws_exit(steps: &[SequenceStep]) -> bool {
    steps.iter().any(|step| {
        matches!(step.kind.as_str(), "return" | "throw")
            || !step.folded.is_empty() && measured_call(step)
            || draws_exit(&step.children)
            || draws_exit(&step.alternate)
    })
}

/// Folds a "continues normally" guard into the step it guards against, when that step is
/// drawn and says so, leaving the guarded suffix to follow it at the same level. Only plain
/// calls, which cannot leave the path themselves, may stand between them. The step is:
///
/// - a call carrying a folded `?` (#542): that `?` is the exit; or
/// - an `if` or `match` whose own frame draws an exit (#558).
///
/// Anything else keeps its guard, returned here. Lossless: the guard stays on the step's
/// `folded`, with its label and range, and the step is marked as one that may leave.
pub(crate) fn fold_guard(steps: &mut [SequenceStep], guard: SequenceStep) -> Option<SequenceStep> {
    let plain = |step: &&mut SequenceStep| measured_call(step) && step.folded.is_empty();
    let Some(step) = steps.iter_mut().rev().find(|step| !plain(step)) else {
        return Some(guard);
    };
    let may_return = step.folded.iter().any(|f| f.label == TRY_LABEL);
    let control = step.kind == "branch"
        && (step.label.starts_with("if ") || step.label.starts_with("match:"))
        && (draws_exit(&step.children) || draws_exit(&step.alternate));
    if measured_call(step) && may_return || control {
        step.folded.push(guard);
        None
    } else {
        Some(guard)
    }
}

/// The deepest nesting below `steps`, counting `steps` themselves as depth 0.
pub(crate) fn tree_depth(steps: &[SequenceStep]) -> usize {
    steps
        .iter()
        .filter(|step| !step.children.is_empty() || !step.alternate.is_empty())
        .map(|step| 1 + tree_depth(&step.children).max(tree_depth(&step.alternate)))
        .max()
        .unwrap_or(0)
}

/// Every step in `steps`, nested and folded (#542) ones included: the work they hold.
pub(crate) fn step_count(steps: &[SequenceStep]) -> usize {
    steps
        .iter()
        .map(|step| {
            1 + step_count(&step.children) + step_count(&step.alternate) + step_count(&step.folded)
        })
        .sum()
}

/// The steps in `steps` that are shown: a hidden step is not, nor is anything it nests.
pub(crate) fn visible_count(steps: &[SequenceStep]) -> usize {
    steps
        .iter()
        .filter(|step| !step.hidden)
        .map(|step| 1 + visible_count(&step.children) + visible_count(&step.alternate))
        .sum()
}

/// The incidental steps in `steps`, as `incidentalSteps` counts them (#543): a group of
/// incidental calls counts its calls, not itself.
pub(crate) fn incidental_count(steps: &[SequenceStep]) -> usize {
    steps
        .iter()
        .map(|step| {
            usize::from(step.incidental.is_some() && step.kind != "group")
                + incidental_count(&step.children)
                + incidental_count(&step.alternate)
        })
        .sum()
}

/// The hidden steps in `steps`, as `hiddenSteps` counts them: a hidden group of hidden
/// calls counts its calls, not itself.
pub(crate) fn hidden_count(steps: &[SequenceStep]) -> usize {
    steps
        .iter()
        .map(|step| {
            usize::from(step.hidden && step.kind != "group")
                + hidden_count(&step.children)
                + hidden_count(&step.alternate)
        })
        .sum()
}

/// Keeps the view within `limit` shown steps: [`MAX_STEPS`], or [`SEED_STEPS`] for a seed
/// that reserves room for callee bodies (#536, #542). Steps are kept in pre-order (a step,
/// its children, then its alternate) up to the bound's last shown step; everything after
/// it, hidden steps included, is omitted. Hidden steps before it are kept and do not count.
pub(crate) fn cap_visible(view: &mut SequenceView, limit: usize) {
    /// Keeps a prefix of `steps` within `room` shown steps; true when it cut.
    fn cap(steps: &mut Vec<SequenceStep>, room: &mut usize) -> bool {
        for at in 0..steps.len() {
            if *room == 0 {
                steps.truncate(at);
                return true;
            }
            let step = &mut steps[at];
            if step.hidden {
                continue;
            }
            *room -= 1;
            if cap(&mut step.children, room) {
                step.alternate.clear();
                steps.truncate(at + 1);
                return true;
            }
            if cap(&mut step.alternate, room) {
                steps.truncate(at + 1);
                return true;
            }
        }
        false
    }
    let mut room = limit;
    let cut = cap(&mut view.steps, &mut room);
    view.hidden_steps = hidden_count(&view.steps);
    view.incidental_steps = incidental_count(&view.steps);
    if !cut {
        return;
    }
    view.limit(SequenceLimit::VisibleSteps);
    let warning = if limit < MAX_STEPS {
        SEED_WARNING
    } else {
        VISIBLE_WARNING
    };
    if !view.warnings.iter().any(|w| w == warning) {
        view.warnings.push(warning.into());
    }
}

/// Keeps the step tree within [`MAX_STEP_DEPTH`]: a step at the bound that still nests
/// becomes one explicit boundary at its own source range, and the view is truncated.
/// Adapters bound their syntax recursion separately; this bounds the DTO they produce.
pub(crate) fn cap_depth(view: &mut SequenceView) {
    fn cap(steps: &mut [SequenceStep], depth: usize) -> bool {
        let mut capped = false;
        for step in steps {
            if step.children.is_empty() && step.alternate.is_empty() {
                continue;
            }
            if depth + 1 == MAX_STEP_DEPTH {
                step.kind = "boundary".into();
                step.label = "Depth limit: behavior omitted.".into();
                step.call_id = None;
                step.target = None;
                step.resolution = None;
                step.call_evidence = None;
                step.children.clear();
                step.alternate.clear();
                capped = true;
            } else {
                capped |= cap(&mut step.children, depth + 1);
                capped |= cap(&mut step.alternate, depth + 1);
            }
        }
        capped
    }
    if cap(&mut view.steps, 0) {
        view.limit(SequenceLimit::Depth);
        for warning in ["Depth limit: behavior omitted.", BOUNDS_WARNING] {
            if !view.warnings.iter().any(|w| w == warning) {
                view.warnings.push(warning.into());
            }
        }
    }
}

#[cfg(test)]
mod external_package_tests {
    use super::ExternalPackage;

    fn package(manager: &str, name: &str, version: Option<&str>) -> Option<ExternalPackage> {
        Some(ExternalPackage {
            manager: manager.into(),
            name: name.into(),
            version: version.map(Into::into),
        })
    }

    #[test]
    fn reads_the_package_field_of_global_scip_symbols() {
        assert_eq!(
            ExternalPackage::from_scip("rust-analyzer cargo serde_json 1.0.140 de/from_str()."),
            package("cargo", "serde_json", Some("1.0.140"))
        );
        assert_eq!(
            ExternalPackage::from_scip("scip-java maven com.google  guava . Lists#"),
            package("maven", "com.google guava", None)
        );
        // Every descriptor kind, escaped names and method disambiguators.
        for tail in [
            "candidate_only().",
            "std/io/Read#read(+1).",
            "Vec#[T]push().(value)",
            "`impl Foo for Bar`#meta:mac!",
            "`a``b`#",
        ] {
            assert_eq!(
                ExternalPackage::from_scip(&format!(
                    "rust-analyzer cargo local-library 0.1.0 {tail}"
                )),
                package("cargo", "local-library", Some("0.1.0")),
                "{tail}"
            );
        }
    }

    #[test]
    fn local_malformed_and_nameless_symbols_carry_no_package() {
        for symbol in [
            "local 7",
            "pkg outside",
            "rust-analyzer cargo serde 1.0",
            "rust-analyzer cargo serde 1.0 ",
            "rust-analyzer . serde 1.0 x().",
            "rust-analyzer cargo . 1.0 x().",
            " cargo serde 1.0 x().",
            // Descriptor tails must be complete SCIP descriptors (#508 review).
            "rust-analyzer cargo local-library 0.1.0 definitely-not-a-descriptor",
            "rust-analyzer cargo local-library 0.1.0 x().@",
            "rust-analyzer cargo local-library 0.1.0 x(",
            "rust-analyzer cargo local-library 0.1.0 [T",
            "rust-analyzer cargo local-library 0.1.0 ``#",
            "rust-analyzer cargo local-library 0.1.0 `open.",
            "rust-analyzer cargo local-library 0.1.0 a/ b#",
            "",
        ] {
            assert_eq!(ExternalPackage::from_scip(symbol), None, "{symbol:?}");
        }
    }
}
