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
    /// Whether the view's client calls were read against HTTP interface evidence (#853).
    /// Omitted when the view has no call to read, or the checkout has fewer than two apps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interfaces: Option<SequenceInterfaces>,
}
/// How a Sequence view's client calls were read against HTTP interface evidence (#853).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SequenceInterfaces {
    pub state: InterfacesState,
    /// Calls that cross to another app's handler, as a labelled boundary hop.
    pub hops: usize,
    /// Client calls whose path matched no route: they stay boundaries.
    pub unresolved: usize,
}
/// Whether interface evidence could be read for a view's revision (#853). Anything but
/// `ready` says nothing about the view's calls: a missing hop is never read as no request.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InterfacesState {
    /// Read against the revision's interface evidence.
    Ready,
    /// The revision's architecture evidence is not built yet, or not kept for this revision.
    NotMeasured,
    /// The evidence failed or the rules file is invalid.
    Unavailable,
}
/// A client call's HTTP interface (#853): structural evidence that its method and path match
/// a route, never proof that the request is sent or reaches that handler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct InterfaceHop {
    /// Always `http`.
    pub kind: String,
    /// The route's method for a match, else the call's when readable.
    pub method: Option<String>,
    /// The route's template for a match, else the call's when readable.
    pub path_template: Option<String>,
    /// `exact`, `template`, `wrapperInferred` or `unresolved` (#788).
    pub confidence: String,
    pub client_app: Option<String>,
    /// The handler's app. Absent when unresolved.
    pub server_app: Option<String>,
    /// The handler's anchor kind (`declaration`, `closure` or `dispatchArm`). Absent when
    /// unresolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handler_kind: Option<String>,
    /// The participant drawn for the handler, in a column named `app:<server app>`. Absent when
    /// unresolved, or when the handler's declaration cannot be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participant: Option<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub documents: Vec<crate::model::DocumentSemanticStatus>,
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
            documents: vec![],
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
    /// The HTTP interface this client call reaches (#853), when it is a read client call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interface: Option<InterfaceHop>,
    /// #888: on a callback boundary, the callback's body as built but not shown: it holds
    /// no interaction, so the boundary stays as it was. The read that finds an HTTP interface
    /// hop in it shows it then ([`splice_callbacks`]). Never serialized: a candidate only.
    #[serde(skip)]
    pub callback: Option<Box<CallbackCandidate>>,
}
/// A callback's body that Sequence built but does not show yet (#888).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackCandidate {
    /// The block that replaces the boundary when it is shown: kind [`CALLBACK_KIND`].
    pub block: SequenceStep,
    /// The measured call the callback is passed to.
    pub outer_call: String,
    /// The participants the block's own resolved calls target, in columns already drawn.
    pub participants: Vec<Participant>,
    /// The call it is passed to, as its source spells it: what a refusal names.
    pub callee: String,
    /// The bound that refused building it whole (Prime review on #891): the block then keeps
    /// only its measured calls, so a read that finds a drawn hop among them reports the bound
    /// with [`callback_warning`] instead of dropping the hop silently. Never shown.
    pub refused: Option<SequenceLimit>,
}
impl CallbackCandidate {
    /// A candidate refused by `limit`: only the measured calls of `body`, nested candidates'
    /// included, are kept, flat.
    pub(crate) fn refused(
        boundary: &SequenceStep,
        callee: &str,
        outer_call: String,
        body: &[SequenceStep],
        limit: SequenceLimit,
    ) -> Self {
        let mut calls = vec![];
        any_step(body, &mut |step| {
            if step.call_id.is_some() {
                calls.push(SequenceStep {
                    children: vec![],
                    alternate: vec![],
                    folded: vec![],
                    callback: None,
                    ..step.clone()
                });
            }
            false
        });
        Self {
            block: callback_block(boundary, callee, calls),
            outer_call,
            participants: vec![],
            callee: callee.to_owned(),
            refused: Some(limit),
        }
    }
}
/// The warning of a callback a bound refused (#888): `bound` is `step`, `work` or `depth`.
pub(crate) fn callback_warning(limit: SequenceLimit, callee: &str) -> String {
    let bound = match limit {
        SequenceLimit::VisibleSteps => "step",
        SequenceLimit::WorkSteps => "work",
        SequenceLimit::Depth => "depth",
        SequenceLimit::Participants => "participant",
    };
    format!("Sequence {bound} limit reached; the callback passed to {callee} is not shown.")
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<crate::model::SemanticProjection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub historical_freshness: Option<crate::semantic_evidence::Freshness>,
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
    /// A `dispatch` boundary's exact declaration (#557): the trait, interface or prototype
    /// method the binding names, not a runtime body. It takes no lane and never expands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_target: Option<DispatchTarget>,
}
/// The declaration a dynamic call names.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DispatchTarget {
    pub id: String,
    pub name: Option<String>,
    pub kind: String,
    pub path: String,
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
        "javascript" | "typescript" => {
            crate::behavior_js::build(revision, seed, file, calls, show_all)
        }
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

/// #888: a callback block, the body of a callable passed to a measured call. It is shown, under
/// that call, only when it holds an interaction ([`interacts`]) or an HTTP interface hop;
/// any other callback stays the boundary it always was. When and how often it runs is unknown.
pub const CALLBACK_KIND: &str = "callback";

/// The block that shows `body`, the steps of the callback at `boundary`, passed to `callee`.
/// It keeps the boundary's identity, range and lane: the callback runs in its definer's column.
pub(crate) fn callback_block(
    boundary: &SequenceStep,
    callee: &str,
    body: Vec<SequenceStep>,
) -> SequenceStep {
    let callee: String = {
        let mut name: String = callee.chars().take(80).collect();
        if callee.chars().count() > 80 {
            name.push('…');
        }
        name
    };
    SequenceStep {
        kind: CALLBACK_KIND.into(),
        label: format!("callback passed to {callee} · when and how often it runs is unknown"),
        call_id: None,
        target: None,
        resolution: None,
        call_evidence: None,
        children: body,
        alternate: vec![],
        hidden: false,
        folded: vec![],
        incidental: None,
        interface: None,
        callback: None,
        ..boundary.clone()
    }
}

/// A document's syntax, parsed once per view for [`passed_callable`] (#888).
pub(crate) type CallbackTree = tree_sitter::Tree;

/// The syntax tree of `file` in its language, for [`passed_callable`]; `None` for a language
/// with no callback rule.
pub(crate) fn callback_tree(file: &SourceFile) -> Option<CallbackTree> {
    let language: tree_sitter::Language = match file.language.as_str() {
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => crate::indexer_ts::grammar(&file.path),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        _ => return None,
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(&file.text, None)
}

/// #888 (Prime review on #891): whether `start..end` in `tree` is a callable expression that
/// is itself an argument of a call (a closure, lambda, arrow or function expression, alone in
/// parentheses or as a Python keyword argument at most). A declaration nested inside an
/// argument (a Rust `fn` in a block argument) is not passed, so it is not a callback.
pub(crate) fn passed_callable(tree: &CallbackTree, start: usize, end: usize) -> bool {
    const CALLABLE: [&str; 7] = [
        "arrow_function",
        "function_expression",
        "function",
        "generator_function",
        "closure_expression",
        "lambda",
        "lambda_expression",
    ];
    let Some(mut node) = tree.root_node().descendant_for_byte_range(start, end) else {
        return false;
    };
    while !CALLABLE.contains(&node.kind()) {
        match node.parent() {
            Some(parent) if parent.start_byte() == start && parent.end_byte() == end => {
                node = parent;
            }
            _ => return false,
        }
    }
    if node.start_byte() != start || node.end_byte() != end {
        return false;
    }
    let mut parent = node.parent();
    while let Some(wrapper) =
        parent.filter(|p| matches!(p.kind(), "parenthesized_expression" | "keyword_argument"))
    {
        parent = wrapper.parent();
    }
    parent.is_some_and(|p| matches!(p.kind(), "arguments" | "argument_list"))
}

/// The column a participant is drawn in (#558): its type or module, or its own.
fn column_of(participant: &Participant) -> &str {
    participant
        .column
        .as_ref()
        .map_or(participant.id.as_str(), |column| column.id.as_str())
}

/// Whether `steps` show an interaction (#636): a shown, non-incidental call whose exact target
/// is drawn in another column than the step's own. What the Interactions view draws as an
/// arrow, and what makes a callback worth showing (#888).
pub(crate) fn interacts(steps: &[SequenceStep], participants: &[Participant], seed: &str) -> bool {
    let column = |id: &str| participants.iter().find(|p| p.id == id).map(column_of);
    steps.iter().filter(|step| !step.hidden).any(|step| {
        let crosses = step.kind == "call"
            && step.incidental.is_none()
            && step
                .target
                .as_deref()
                .and_then(column)
                .is_some_and(|target| Some(target) != column(step.lane.as_deref().unwrap_or(seed)));
        crosses
            || interacts(&step.children, participants, seed)
            || interacts(&step.alternate, participants, seed)
    })
}

/// Shows the callback `block` in place of the boundary at `at` (#888). The block goes under
/// the call it is passed to when that call is a later shown sibling, after any body the call
/// expands; otherwise it takes the boundary's place. True when `at` no longer holds a step
/// of its own: the boundary was removed.
pub(crate) fn place_callback(
    steps: &mut Vec<SequenceStep>,
    at: usize,
    block: SequenceStep,
    outer: &str,
) -> bool {
    let host = (at + 1..steps.len())
        .find(|&j| steps[j].call_id.as_deref() == Some(outer))
        .filter(|&j| !steps[j].hidden);
    match host {
        Some(j) => {
            steps.remove(at);
            steps[j - 1].children.push(block);
            true
        }
        None => {
            steps[at] = block;
            false
        }
    }
}

/// Every step of `steps` and what they nest, candidates' blocks included: what a read must
/// look at to decide on a candidate (#888).
pub(crate) fn any_step(
    steps: &[SequenceStep],
    found: &mut dyn FnMut(&SequenceStep) -> bool,
) -> bool {
    steps.iter().any(|step| {
        found(step)
            || any_step(&step.children, found)
            || any_step(&step.alternate, found)
            || step
                .callback
                .as_ref()
                .is_some_and(|candidate| any_step(std::slice::from_ref(&candidate.block), found))
    })
}

/// Shows each callback candidate whose block holds a step `accept` takes (#888), within the
/// view's bounds: its participants' columns must fit (#558), and its steps the work bound and,
/// when `visible_bound` holds, the shown-step bound. A candidate that does not fit stays the
/// boundary it was, and the view is truncated by that bound. Nested candidates are decided
/// first. Every other candidate is dropped. True when a block was shown.
pub(crate) fn splice_callbacks(
    view: &mut SequenceView,
    accept: &dyn Fn(&SequenceStep) -> bool,
    visible_bound: bool,
) -> bool {
    struct Splice<'v> {
        participants: &'v mut Vec<Participant>,
        warnings: &'v mut Vec<String>,
        limits: Vec<SequenceLimit>,
        shown: usize,
        work: usize,
        visible_bound: bool,
        spliced: bool,
    }
    impl Splice<'_> {
        fn refuse(&mut self, limit: SequenceLimit, warning: &str) {
            self.limits.push(limit);
            if !self.warnings.iter().any(|w| w == warning) {
                self.warnings.push(warning.into());
            }
        }
        /// Decides `candidate`'s nested candidates, then whether it is shown.
        fn decide(
            &mut self,
            mut candidate: CallbackCandidate,
            accept: &dyn Fn(&SequenceStep) -> bool,
        ) -> Option<SequenceStep> {
            let mut nested = Splice {
                participants: &mut candidate.participants,
                warnings: &mut *self.warnings,
                limits: vec![],
                shown: 0,
                work: 0,
                visible_bound: false,
                spliced: false,
            };
            nested.walk(&mut candidate.block.children, accept);
            let limits = std::mem::take(&mut nested.limits);
            self.limits.extend(limits);
            if !any_step(std::slice::from_ref(&candidate.block), &mut |step| {
                accept(step)
            }) {
                return None;
            }
            if let Some(limit) = candidate.refused {
                self.refuse(limit, &callback_warning(limit, &candidate.callee));
                return None;
            }
            // The block replaces its boundary (Prime review on #891): it costs its body.
            let shown = visible_count(&candidate.block.children);
            let work = step_count(&candidate.block.children);
            if self.visible_bound && self.shown + shown > MAX_STEPS {
                let warning = callback_warning(SequenceLimit::VisibleSteps, &candidate.callee);
                self.refuse(SequenceLimit::VisibleSteps, &warning);
                return None;
            }
            if self.work + work > MAX_WORK_STEPS {
                let warning = callback_warning(SequenceLimit::WorkSteps, &candidate.callee);
                self.refuse(SequenceLimit::WorkSteps, &warning);
                return None;
            }
            let mut added = self.participants.clone();
            for participant in candidate.participants {
                if added.iter().any(|p| p.id == participant.id) {
                    continue;
                }
                let column = participant.column.clone().unwrap_or(SequenceColumn {
                    id: participant.id.clone(),
                    label: participant.label.clone(),
                });
                if !crate::behavior_columns::fits(&added, &column) {
                    self.refuse(
                        SequenceLimit::Participants,
                        "Sequence participant limit reached; additional targets are not shown.",
                    );
                    return None;
                }
                added.push(participant);
            }
            *self.participants = added;
            self.shown += shown;
            self.work += work;
            self.spliced = true;
            Some(candidate.block)
        }
        fn walk(&mut self, steps: &mut Vec<SequenceStep>, accept: &dyn Fn(&SequenceStep) -> bool) {
            let mut at = 0;
            while at < steps.len() {
                if let Some(candidate) = steps[at].callback.take() {
                    let outer = candidate.outer_call.clone();
                    if let Some(block) = self.decide(*candidate, accept)
                        && place_callback(steps, at, block, &outer)
                    {
                        continue;
                    }
                } else {
                    self.walk(&mut steps[at].children, accept);
                    self.walk(&mut steps[at].alternate, accept);
                }
                at += 1;
            }
        }
    }
    let mut splice = Splice {
        shown: visible_count(&view.steps),
        work: step_count(&view.steps),
        participants: &mut view.participants,
        warnings: &mut view.warnings,
        limits: vec![],
        visible_bound,
        spliced: false,
    };
    splice.walk(&mut view.steps, accept);
    let (limits, spliced) = (splice.limits, splice.spliced);
    for limit in limits {
        view.limit(limit);
    }
    spliced
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

#[cfg(test)]
mod callback_tests {
    use super::*;

    fn step(id: &str, kind: &str) -> SequenceStep {
        SequenceStep {
            id: id.into(),
            kind: kind.into(),
            label: id.into(),
            path: "a.js".into(),
            range: SourceRange::default(),
            call_id: None,
            target: None,
            resolution: None,
            call_evidence: None,
            children: vec![],
            alternate: vec![],
            hidden: false,
            lane: None,
            folded: vec![],
            incidental: None,
            interface: None,
            callback: None,
        }
    }

    fn participant(id: &str, column: &str) -> Participant {
        Participant {
            id: id.into(),
            label: id.into(),
            kind: "function".into(),
            identification: String::new(),
            declaration: None,
            column: Some(SequenceColumn {
                id: column.into(),
                label: column.into(),
            }),
        }
    }

    #[test]
    fn only_a_shown_non_incidental_call_into_another_column_interacts() {
        let participants = [
            participant("seed", "module:a"),
            participant("same", "module:a"),
            participant("other", "module:b"),
        ];
        let call = |target: &str| SequenceStep {
            target: Some(target.into()),
            call_id: Some(format!("call:{target}")),
            ..step(target, "call")
        };
        assert!(!interacts(&[call("same")], &participants, "seed"));
        assert!(interacts(&[call("other")], &participants, "seed"));
        let hidden = SequenceStep {
            hidden: true,
            ..call("other")
        };
        assert!(!interacts(&[hidden], &participants, "seed"));
        let incidental = SequenceStep {
            incidental: Some(IncidentalReason::Accessor),
            ..call("other")
        };
        assert!(!interacts(&[incidental], &participants, "seed"));
        // Nested in a branch, it still interacts; on the other column's own lane, it does not.
        let mut branch = step("if", "branch");
        branch.children.push(call("other"));
        assert!(interacts(&[branch], &participants, "seed"));
        let own = SequenceStep {
            lane: Some("other".into()),
            ..call("other")
        };
        assert!(!interacts(&[own], &participants, "seed"));
    }

    #[test]
    fn a_block_goes_under_its_later_shown_call_or_takes_the_boundary_place() {
        let outer = SequenceStep {
            call_id: Some("outer".into()),
            ..step("call", "call")
        };
        let block = || callback_block(&step("boundary", "boundary"), "wrap", vec![]);
        let mut steps = vec![step("boundary", "boundary"), outer.clone()];
        assert!(place_callback(&mut steps, 0, block(), "outer"));
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].children[0].kind, CALLBACK_KIND);
        // A hidden call (an incidental `setTimeout`) would hide the block: it stays in place.
        let hidden = SequenceStep {
            hidden: true,
            ..outer.clone()
        };
        let mut steps = vec![step("boundary", "boundary"), hidden];
        assert!(!place_callback(&mut steps, 0, block(), "outer"));
        assert_eq!(steps[0].kind, CALLBACK_KIND);
        assert!(steps[1].children.is_empty());
        // No such later sibling: in place.
        let mut steps = vec![outer, step("boundary", "boundary")];
        assert!(!place_callback(&mut steps, 1, block(), "outer"));
        assert_eq!(steps[1].kind, CALLBACK_KIND);
    }
}
