# File → method → sequence

> Current language update: native Java and Python navigation/static sequences are now available.
> See [support and boundaries](java-python-support.md). Historical first-pass limits below remain dated.

The primary workflow is an indexed file tree. Expand a file to see its methods inline,
then choose a method to view a source-linked static sequence diagram alongside the tree.
No provider call is required. Existing question tools and the outgoing call hierarchy
remain available separately.

## Evidence and architecture

The renderer consumes a shared `SequenceView` behavior representation, not JavaScript
syntax nodes. In this first slice, a JavaScript adapter derives that representation from
**cached indexed source** and measured call-site identities. It does not read or execute
the live workspace. This is a compatibility bridge until behavior facts are persisted by
language adapters at index time; the current call graph alone is not enough to infer
execution structure.

Branches, loops, exceptional paths and nested evaluation must remain explicit. Callback
and nested-function declarations are not calls. Unresolved callees remain boundaries.
Unsupported control constructs must produce a warning/boundary rather than invented
behavior. This is a static possible-path view, not a runtime trace or concurrency proof.

## Filtering

The initial filters are conservative local heuristics, not model-based semantic judgments.
Show-all controls expose filtered methods and steps. Do not equate zero outgoing calls
with an inconsequential method: validation, computation and state changes can matter without
calling anything. Selection quality and broader language support remain separate gates.

## Scope

Language-neutral navigation, DTOs and rendering; JavaScript adapter initially. Bounded
snapshot-consistent file catalog and method/sequence routes, explicit stale-state handling,
source links, and contained diagram scrolling. No automatic indexing or inference.

[Implementation contract](sequence-slice-contract.md).


## Current filtering limits

Method lists retain all indexed functions/methods, including accessors and zero-call checks:
there is no semantic method-importance classifier yet. The show-all method control is ready
for retention hints, but the current backend does not hide methods without evidence.
Sequence diagrams show admitted calls first, with their branch/alternative/loop/try scope,
concise unknown behavior and early-exit context. Standalone writes, bindings, return-value text
and deferred nested definitions are available through Show all steps. The original DTO, measured
call order and source identities do not change. A zero-call view does not mean no behavior.
The same Show all steps setting reveals incidental calls (including explicitly labelled logging
heuristics, external calls and trivial accessors) and restores full admitted step text.
Frontend filtering cannot recover calls clipped by server limits. The visible 200-step bound,
seed 150/callee reserve 50, work, depth and participant bounds and truncation warnings stay unchanged.

Complex loop transfers, unsupported patterns and optional-chain operations may appear as
source-linked boundaries rather than expanded behavior. These limits are explicit; the
adapter must not invent a straight-line execution order for them.
