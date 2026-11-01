//! What one document's stored evidence may cost (#564).
//!
//! A selected read refuses a document past these budgets, so a document that reaches them is
//! not published with its facts: extraction keeps its file module and a whole-file coverage
//! gap, and the rest of the checkout indexes. One repository file never fails an index or
//! leaves it needing reconciliation.
use crate::model::SourceFile;
use crate::native_evidence::{Declaration, Key};

/// The coverage diagnostic of a document whose evidence was left out. Reads find such a
/// document by it, so a change here is a change of stored meaning.
pub(crate) const DIAGNOSTIC: &str =
    "native evidence exceeds the per-file budget; only the file module was kept";

/// Selected rows of one table, for a source of `bytes`.
pub(crate) fn row_limit(bytes: i64) -> i64 {
    bytes.saturating_mul(16).clamp(1024, 1_000_000)
}
/// Bytes of one native or class row.
pub(crate) fn max_row(bytes: i64) -> i64 {
    bytes
        .saturating_mul(32)
        .saturating_add(16 * 1024)
        .min(256 * 1024 * 1024)
}
/// Bytes of all of a document's native rows, of its class rows and of its graph rows, each.
/// A class takes members from other files, so class rows are only known when a revision is
/// composed, long after a document's native rows are measured. They have a budget of their own
/// so that neither gate depends on the other.
pub(crate) fn total(bytes: i64) -> i64 {
    bytes
        .saturating_mul(64)
        .saturating_add(256 * 1024)
        .min(512 * 1024 * 1024)
}
/// Bytes of the class extraction stored for one document, whatever its source.
pub(crate) const CLASS_EXTRACTION: usize = 64 * 1024 * 1024;

/// `document:v1:` and a hex digest: the stored document-version ID each native row is charged.
const VERSION_ID_BYTES: usize = 76;
/// The stored form of a syntax, occurrence or owner ID.
const ID_BYTES: usize = 39;

/// The rows of one document's native facts, charged as a selected read charges them.
#[derive(Default)]
struct Charge {
    count: [usize; 15],
    max_row: usize,
    total: usize,
}
impl Charge {
    fn row(&mut self, table: usize, bytes: usize) {
        let bytes = VERSION_ID_BYTES + bytes;
        self.count[table] += 1;
        self.max_row = self.max_row.max(bytes);
        self.total = self.total.saturating_add(bytes);
    }
}

fn digits(value: usize) -> usize {
    value.checked_ilog10().unwrap_or(0) as usize + 1
}
fn text(value: &Option<String>) -> usize {
    value.as_ref().map_or(0, String::len)
}

fn declaration(charge: &mut Charge, d: &Declaration) {
    let name = d.name_range.as_ref();
    let signature = d.key.signature.as_ref();
    charge.row(
        0,
        2 * ID_BYTES
            + d.kind.len()
            + text(&d.name)
            + text(&d.lookup_key)
            + 2
            + signature.map_or(0, |s| digits(s.type_parameter_count))
            + digits(d.key.ordinal)
            + digits(d.range.start)
            + digits(d.range.end)
            + name.map_or(0, |r| digits(r.start) + digits(r.end)),
    );
    if let Some(signature) = signature {
        for (ordinal, parameter) in signature.parameter_types.iter().enumerate() {
            charge.row(1, ID_BYTES + digits(ordinal) + parameter.len());
        }
    }
    for (ordinal, key) in d.ancestors.iter().enumerate() {
        ancestor(charge, ordinal, key);
    }
    let header = &d.header;
    charge.row(
        2,
        ID_BYTES + header.kind.len() + text(&header.name) + text(&header.result_type),
    );
    for items in [&header.modifiers, &header.type_parameters, &header.bases] {
        for (ordinal, item) in items.iter().enumerate() {
            charge.row(
                3,
                ID_BYTES + "typeParameter".len() + digits(ordinal) + item.len(),
            );
        }
    }
    for (ordinal, parameter) in header.parameters.iter().enumerate() {
        charge.row(
            4,
            ID_BYTES + digits(ordinal) + text(&parameter.name) + text(&parameter.type_name) + 1,
        );
    }
}

fn ancestor(charge: &mut Charge, ordinal: usize, key: &Key) {
    let signature = key.signature.as_ref();
    charge.row(
        5,
        ID_BYTES
            + digits(ordinal)
            + key.kind.len()
            + text(&key.name)
            + digits(key.ordinal)
            + 2
            + signature.map_or(0, |s| digits(s.type_parameter_count)),
    );
    if let Some(signature) = signature {
        for (type_ordinal, parameter) in signature.parameter_types.iter().enumerate() {
            charge.row(
                6,
                ID_BYTES + digits(ordinal) + digits(type_ordinal) + parameter.len(),
            );
        }
    }
}

/// Where each kind of fact of one document starts in its artifact.
pub(crate) struct Facts<'a> {
    pub declarations: &'a [Declaration],
    pub calls: &'a [crate::native_evidence::Call],
    pub control_regions: &'a [crate::native_evidence::ControlRegion],
    pub references: &'a [crate::native_evidence::Reference],
    pub gaps: &'a [crate::native_evidence::MeasurementGap],
    pub macro_expansions: usize,
}

fn native_charge(facts: &Facts<'_>, source_set_id: usize, file: &SourceFile) -> Charge {
    let language = file.language.len();
    let mut charge = Charge::default();
    for d in facts.declarations {
        declaration(&mut charge, d);
    }
    for (ordinal, call) in facts.calls.iter().enumerate() {
        let lookup =
            crate::native_evidence::verified_callee_lookup(&file.language, call, &file.text);
        charge.row(
            7,
            2 * ID_BYTES
                + digits(ordinal)
                + digits(call.range.start)
                + digits(call.range.end)
                + call
                    .callee_range
                    .as_ref()
                    .map_or(0, |r| digits(r.start) + digits(r.end))
                + text(&call.spelling)
                + text(&lookup)
                + source_set_id
                + language,
        );
        for (region, _) in call.region_ids.iter().enumerate() {
            charge.row(8, 2 * ID_BYTES + digits(region));
        }
    }
    for region in facts.control_regions {
        charge.row(
            9,
            3 * ID_BYTES
                + digits(region.ordinal)
                + region.kind.len()
                + digits(region.range.start)
                + digits(region.range.end)
                + text(&region.arm),
        );
    }
    for (ordinal, reference) in facts.references.iter().enumerate() {
        charge.row(
            10,
            2 * ID_BYTES
                + digits(ordinal)
                + digits(reference.range.start)
                + digits(reference.range.end)
                + reference.spelling.len()
                + reference.lookup_key.len(),
        );
    }
    // A report has one row per family and one per family's gap: every family has them all.
    const FAMILY: usize = "declarationName".len();
    for _ in crate::native_evidence::MeasurementKind::ALL {
        charge.row(11, FAMILY + 2);
        for (ordinal, gap) in facts.gaps.iter().enumerate() {
            charge.row(
                12,
                FAMILY
                    + digits(ordinal)
                    + digits(gap.range.start)
                    + digits(gap.range.end)
                    + gap.diagnostic.len(),
            );
        }
    }
    for ordinal in 0..facts.macro_expansions {
        // Two offsets of at most a source's bytes: charged at the 16 MiB source ceiling.
        charge.row(13, digits(ordinal) + 16);
    }
    for _ in 0..4 {
        charge.row(14, "supported".len() + "definition".len() + 1);
    }
    charge
}

/// The bytes a selected read charges one document's native rows.
#[cfg(test)]
pub(crate) fn native_bytes(facts: &Facts<'_>, source_set_id: &str, file: &SourceFile) -> usize {
    native_charge(facts, source_set_id.len(), file).total
}

/// Whether one document's native rows, and the graph rows they project, exceed its budgets.
pub(crate) fn exceeded(file: &SourceFile, source_set_id: &str, facts: Facts<'_>) -> bool {
    let source = i64::try_from(file.text.len()).unwrap_or(i64::MAX);
    let charge = native_charge(&facts, source_set_id.len(), file);
    let limit = usize::try_from(row_limit(source)).unwrap_or(usize::MAX);
    if charge.count.iter().any(|&count| count > limit)
        || charge.max_row as i64 > max_row(source)
        || charge.total as i64 > total(source)
    {
        return true;
    }
    // A graph row is a few times its native row. Project only a document that could exceed.
    let rows = facts.declarations.len() + facts.calls.len() + facts.control_regions.len();
    if (charge.total + rows * 4 * file.path.len()).saturating_mul(8) <= total(source) as usize {
        return false;
    }
    let projected = crate::indexer::project_document(
        &file.path,
        file.text.as_bytes(),
        crate::indexer::NativeDocument {
            declarations: facts.declarations.iter().collect(),
            calls: facts.calls.iter().collect(),
            control_regions: facts.control_regions.iter().collect(),
        },
    );
    projected
        .is_ok_and(|(nodes, calls, regions)| graph_bytes(&nodes, &calls, &regions, source).is_err())
}

/// The former graph-row SQL charge: key text bytes plus the compact JSON payload.
pub(crate) fn graph_bytes(
    nodes: &[crate::model::Symbol],
    calls: &[crate::model::CallSite],
    regions: &[crate::model::ControlRegion],
    source_bytes: i64,
) -> anyhow::Result<u64> {
    fn encoded<T: serde::Serialize>(value: &T) -> anyhow::Result<usize> {
        Ok(serde_json::to_vec(value)?.len())
    }
    let mut largest = 0u64;
    let mut sum = 0u64;
    let mut add = |row: usize| {
        largest = largest.max(row as u64);
        sum = sum.saturating_add(row as u64);
    };
    for node in nodes {
        add(node.id.len() + node.name.len() + node.path.len() + encoded(node)?);
    }
    for call in calls {
        add(call.id.len() + call.caller.len() + call.path.len() + encoded(call)?);
    }
    for region in regions {
        add(region.id.len() + region.owner.len() + region.path.len() + encoded(region)?);
    }
    let bytes = source_bytes.max(0) as u64;
    anyhow::ensure!(
        largest
            <= bytes
                .saturating_mul(6)
                .saturating_add(16 * 1024)
                .min(256 * 1024 * 1024 + 16 * 1024)
            && sum <= total(source_bytes.max(0)) as u64,
        "incompatible_index: selected graph row byte budget exceeded"
    );
    Ok(sum)
}
