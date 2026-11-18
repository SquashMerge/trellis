//! Call shapes and call dispatch. `TargetCallable` and the settled and rejoined dispatch
//! stay in `semantic_import.rs`.
use super::{TargetCallable, join_store::OccurrenceRow};
use crate::{native_evidence as native, semantic_evidence as semantic};

pub(super) struct MeasuredOccurrence {
    pub(super) row: OccurrenceRow,
    pub(super) compatible_syntax_id: Option<String>,
    /// How the exactly bound definition's native declaration can be called (#470).
    pub(super) callable: TargetCallable,
}

/// The measured syntactic shape of a native call (#470), read from its parser-measured
/// invocation and callee ranges and the authenticated source bytes between their starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CallShape {
    /// The callee token starts the call: a JavaScript or Rust identifier callee (`foo()`,
    /// Rust `foo::<T>()`).
    Plain,
    /// A Rust path callee (`m::foo()`, `crate::m::foo()`): only path segments precede it.
    Path,
    /// A JavaScript `new` expression whose constructor is an identifier or dotted name.
    New,
    /// The callee token follows a `.`: `x.m()`, `self.m()`, `a?.b()`, Java `this.m()`, Python
    /// `module.f()`.
    Member,
    /// A compound callee, a call without a measured callee token, or anything not proved above.
    Other,
}

pub(super) fn call_shape(language: &str, call: &native::Call, source: &[u8]) -> CallShape {
    let Some(callee) = &call.callee_range else {
        return CallShape::Other;
    };
    let Some(prefix) = source.get(call.range.start..callee.start) else {
        return CallShape::Other;
    };
    if prefix.is_empty() {
        return CallShape::Plain;
    }
    match language {
        // `new`, whitespace, then only a dotted name: the native callee of a `new_expression`
        // is its constructor identifier or the last property of its dotted constructor, and
        // no call or member expression on another value fits in that prefix.
        "javascript" | "typescript" => match prefix.strip_prefix(b"new") {
            Some(rest)
                if rest.first().is_some_and(u8::is_ascii_whitespace)
                    && rest.iter().all(|b| {
                        b.is_ascii_alphanumeric()
                            || b.is_ascii_whitespace()
                            || matches!(b, b'_' | b'$' | b'.')
                    }) =>
            {
                CallShape::New
            }
            _ if member_prefix(prefix) => CallShape::Member,
            _ => CallShape::Other,
        },
        // A path callee's native token is its `scoped_identifier` name, so the prefix is path
        // segments ending in `::`. A method call's prefix holds the receiver and its `.`.
        "rust"
            if prefix.trim_ascii_end().ends_with(b"::")
                && !prefix.iter().any(|b| {
                    matches!(
                        b,
                        b'.' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b'/' | b'"' | b'\''
                    )
                }) =>
        {
            CallShape::Path
        }
        "rust" | "java" | "python" if member_prefix(prefix) => CallShape::Member,
        _ => CallShape::Other,
    }
}

/// A call whose callee token directly follows a `.`. The receiver before it can be any
/// expression; only the separator is read.
fn member_prefix(prefix: &[u8]) -> bool {
    prefix.trim_ascii_end().ends_with(b".")
}

/// A call binding's dispatch (owner rulings 2026-10-30, #470, and 2026-10-31, #557), from the
/// measured native call shape and the exact bound target's native declaration only: a plain
/// JavaScript call, or a plain or path Rust call, to a free function is `direct`, and so is a
/// Rust member or path call to an inherent method and a Java plain or member call to a
/// `static`, `private` or `final` method; a JavaScript `new` of a class or its constructor is
/// `constructor`; a Rust trait method, a JavaScript member call to a method and any other Java
/// method call is `dynamic`; everything else stays `unknown`. For Python (owner ruling
/// 2026-11-13, #977), a plain or module-attribute call to a free function is `direct`, calling
/// a class is `constructor` and a member call to a method is `dynamic`.
pub(super) fn call_dispatch(
    language: &str,
    shape: CallShape,
    callable: TargetCallable,
) -> semantic::Dispatch {
    use CallShape::{Member, New, Path, Plain};
    use TargetCallable::{Constructor, DynamicMethod, FixedMethod, FreeFunction};
    match (language, shape, callable) {
        ("javascript" | "typescript", Plain, FreeFunction)
        | ("rust", Plain | Path, FreeFunction)
        | ("rust", Member | Path, FixedMethod)
        | ("java", Plain | Member, FixedMethod)
        | ("python", Plain | Member, FreeFunction) => semantic::Dispatch::Direct,
        ("javascript" | "typescript", New, Constructor)
        | ("python", Plain | Member, Constructor) => semantic::Dispatch::Constructor,
        ("javascript" | "typescript", Member, DynamicMethod)
        | ("rust", Member | Path, DynamicMethod)
        | ("java", Plain | Member, DynamicMethod)
        | ("python", Member, DynamicMethod) => semantic::Dispatch::Dynamic,
        _ => semantic::Dispatch::Unknown,
    }
}

/// Whether the settled row still resolves, with a verified target, to `joined_target`.
pub(super) fn resolves_to(
    joined_target: Option<&semantic::Target>,
    settled: &semantic::CallBinding,
) -> bool {
    matches!(
        (joined_target, &settled.declared_target),
        (
            Some(semantic::Target::Internal {
                syntax_id: a,
                document: x,
                ..
            }),
            Some(semantic::Target::Internal {
                syntax_id: b,
                document: y,
                ..
            }),
        ) if settled.resolution == semantic::Resolution::Resolved
            && settled.stale_target == Some(false)
            && a == b
            && x == y
    )
}
