//! The daemon's operations and the policy of each: its route, read kind, budget and reply.
use crate::{CLI_ARCHITECTURE_WAIT, CLI_READ_WAIT};
use std::time::Duration;

/// One daemon-socket operation: the request's operation string, parsed once.
/// Everything the connection handler decides by operation is in the two
/// tables below, [`Operation::requires_preface`] and [`Operation::route`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Operation {
    DaemonStop,
    Mcp,
    Api,
    GcReport,
    GcRun,
    Serve,
    Status,
    Symbols,
    Query,
    Index,
    SemanticImport,
    Metrics,
    RulesCheck,
    References,
    ProducerPlan,
    ProducerStatus,
    ProducerPickup,
    ProducerRun,
    ProducerCancel,
    SettledForTests,
    /// Any other string. It is refused as `unknown daemon operation` only
    /// after the request's workspace has been checked, like a known read.
    Unknown,
}

/// Which handler answers an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Hands the connection to the daemon's stop sequence, which answers it.
    Stop,
    /// Attaches an MCP session and serves it on this connection.
    Mcp,
    /// One allowlisted browser route, with no checkout attached.
    Api,
    /// The index garbage-collection report, with no checkout attached.
    GcReport,
    /// One request against the workspace it names.
    Read(ReadKind, ReadPolicy),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadKind {
    /// Registers a browser session and holds the connection open.
    Serve,
    /// Activates the checkout and runs one selected read or write.
    Checkout,
    /// Refused once the workspace has been checked.
    Unknown,
}

/// The per-operation policy of a [`Route::Read`] request. `Serve` and
/// `Unknown` end before `marker`, `awaits_first_head` and `replayable` are
/// consulted, and before `read_scope` applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ReadPolicy {
    /// One deadline for the whole CLI read (#142): activation contention, the
    /// wait for startup reconciliation and every SQLite busy wait share it, so
    /// they cannot stack. When it passes the read fails typed.
    pub(super) budget: ReadBudget,
    /// Activation and the selected read run under that deadline as the
    /// store's read deadline.
    pub(super) read_scope: bool,
    pub(super) marker: Marker,
    /// The read commands that activate their checkout and wait for its first
    /// head (#842). The read then starts a fresh budget.
    pub(super) awaits_first_head: bool,
    /// A read-only call repeats on typed contention and on `index_not_ready`
    /// until its deadline. An accepted write is never replayed.
    pub(super) replayable: bool,
    pub(super) reply: ReplyTransfer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReadBudget {
    Fixed(Duration),
    /// From the request's scope, so it stays code: `metrics_read_budget`.
    MetricsWork,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Marker {
    /// Publishes the workspace marker that activation needs. A status read
    /// activates its checkout like every other read (#842).
    Publish,
    /// Observes an existing marker and creates none.
    ExistingReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReplyTransfer {
    /// Written whole, or in chunks when it exceeds one frame.
    Chunked,
    /// A success past the deadline becomes `deadline_exceeded`, a deadline
    /// error more than 500 ms late is not sent, and the transfer has its own
    /// window.
    DeadlineBounded,
}

impl Operation {
    pub(super) fn parse(operation: &str) -> Self {
        match operation {
            "daemon_stop" => Self::DaemonStop,
            "mcp" => Self::Mcp,
            "api" => Self::Api,
            "gc_report" => Self::GcReport,
            "gc_run" => Self::GcRun,
            "serve" => Self::Serve,
            "status" => Self::Status,
            "symbols" => Self::Symbols,
            "query" => Self::Query,
            "index" => Self::Index,
            "semantic_import" => Self::SemanticImport,
            "metrics" => Self::Metrics,
            "rules_check" => Self::RulesCheck,
            "references" => Self::References,
            "producer_plan" => Self::ProducerPlan,
            "producer_status" => Self::ProducerStatus,
            "producer_pickup" => Self::ProducerPickup,
            "producer_run" => Self::ProducerRun,
            "producer_cancel" => Self::ProducerCancel,
            "settled_for_tests" => Self::SettledForTests,
            _ => Self::Unknown,
        }
    }

    /// Whether the request must arrive behind the metrics admission preface.
    /// Every other operation must arrive without it.
    pub(super) fn requires_preface(self) -> bool {
        matches!(self, Self::Metrics)
    }

    pub(super) fn route(self) -> Route {
        use {Marker::*, ReadBudget::*, ReadKind::*, ReplyTransfer::*};
        match self {
            Self::DaemonStop => Route::Stop,
            Self::Mcp => Route::Mcp,
            Self::Api => Route::Api,
            Self::GcReport => Route::GcReport,
            Self::Serve => Route::Read(
                Serve,
                ReadPolicy {
                    budget: Fixed(CLI_READ_WAIT),
                    read_scope: false,
                    marker: Publish,
                    awaits_first_head: false,
                    replayable: true,
                    reply: Chunked,
                },
            ),
            Self::Unknown => Route::Read(
                Unknown,
                ReadPolicy {
                    budget: Fixed(CLI_READ_WAIT),
                    read_scope: false,
                    marker: Publish,
                    awaits_first_head: false,
                    replayable: true,
                    reply: Chunked,
                },
            ),
            Self::Status | Self::Symbols | Self::Query => Route::Read(
                Checkout,
                ReadPolicy {
                    budget: Fixed(CLI_READ_WAIT),
                    read_scope: true,
                    marker: Publish,
                    awaits_first_head: true,
                    replayable: true,
                    reply: Chunked,
                },
            ),
            Self::Metrics => Route::Read(
                Checkout,
                ReadPolicy {
                    budget: MetricsWork,
                    read_scope: true,
                    marker: Publish,
                    awaits_first_head: true,
                    replayable: true,
                    reply: DeadlineBounded,
                },
            ),
            // A rules check or references read can wait for the revision's
            // architecture core (#556).
            Self::RulesCheck | Self::References => Route::Read(
                Checkout,
                ReadPolicy {
                    budget: Fixed(CLI_ARCHITECTURE_WAIT),
                    read_scope: true,
                    marker: Publish,
                    awaits_first_head: true,
                    replayable: true,
                    reply: Chunked,
                },
            ),
            // A plan reads the head's native basis, so it waits for a head as the reads do,
            // including an index recreated after an upgrade (#1324).
            Self::ProducerPlan => Route::Read(
                Checkout,
                ReadPolicy {
                    budget: Fixed(CLI_READ_WAIT),
                    read_scope: true,
                    marker: ExistingReadOnly,
                    awaits_first_head: true,
                    replayable: true,
                    reply: Chunked,
                },
            ),
            Self::ProducerStatus => Route::Read(
                Checkout,
                ReadPolicy {
                    budget: Fixed(CLI_READ_WAIT),
                    read_scope: true,
                    marker: ExistingReadOnly,
                    awaits_first_head: false,
                    replayable: true,
                    reply: Chunked,
                },
            ),
            Self::Index
            | Self::GcRun
            | Self::SemanticImport
            | Self::ProducerPickup
            | Self::ProducerRun
            | Self::ProducerCancel => Route::Read(
                Checkout,
                ReadPolicy {
                    budget: Fixed(CLI_READ_WAIT),
                    read_scope: false,
                    marker: Publish,
                    awaits_first_head: false,
                    replayable: false,
                    reply: Chunked,
                },
            ),
            // Only activation contention shares this; the wait has its own bound.
            Self::SettledForTests => Route::Read(
                Checkout,
                ReadPolicy {
                    budget: Fixed(trellis::testing::HANG_BACKSTOP),
                    read_scope: false,
                    marker: ExistingReadOnly,
                    awaits_first_head: false,
                    replayable: true,
                    reply: Chunked,
                },
            ),
        }
    }
}

#[cfg(test)]
mod operation_policy_tests {
    use super::*;

    const KNOWN: [&str; 20] = [
        "daemon_stop",
        "mcp",
        "api",
        "gc_report",
        "gc_run",
        "serve",
        "status",
        "symbols",
        "query",
        "index",
        "semantic_import",
        "metrics",
        "rules_check",
        "references",
        "producer_plan",
        "producer_status",
        "producer_pickup",
        "producer_run",
        "producer_cancel",
        "settled_for_tests",
    ];

    /// The policy as the string comparisons spelled it before the table
    /// (#1072), kept here as the independent statement of each rule.
    fn expected_route(operation: &str) -> Route {
        match operation {
            "daemon_stop" => return Route::Stop,
            "mcp" => return Route::Mcp,
            "api" => return Route::Api,
            "gc_report" => return Route::GcReport,
            _ => {}
        }
        let kind = if operation == "serve" {
            ReadKind::Serve
        } else if matches!(
            operation,
            "status"
                | "symbols"
                | "query"
                | "index"
                | "gc_run"
                | "semantic_import"
                | "metrics"
                | "rules_check"
                | "references"
                | "producer_plan"
                | "producer_status"
                | "producer_pickup"
                | "producer_run"
                | "producer_cancel"
                | "settled_for_tests"
        ) {
            ReadKind::Checkout
        } else {
            ReadKind::Unknown
        };
        let budget = if operation == "metrics" {
            ReadBudget::MetricsWork
        } else if matches!(operation, "rules_check" | "references") {
            ReadBudget::Fixed(Duration::from_secs(30))
        } else if operation == "settled_for_tests" {
            ReadBudget::Fixed(trellis::testing::HANG_BACKSTOP)
        } else {
            ReadBudget::Fixed(Duration::from_secs(5))
        };
        let read_scope = matches!(
            operation,
            "status"
                | "symbols"
                | "query"
                | "metrics"
                | "rules_check"
                | "references"
                | "producer_plan"
                | "producer_status"
        );
        let marker = if matches!(
            operation,
            "producer_plan" | "producer_status" | "settled_for_tests"
        ) {
            Marker::ExistingReadOnly
        } else {
            Marker::Publish
        };
        // `producer_plan` since #1324.
        let awaits_first_head = matches!(
            operation,
            "status"
                | "symbols"
                | "query"
                | "metrics"
                | "rules_check"
                | "references"
                | "producer_plan"
        );
        let replayable = !matches!(
            operation,
            "index"
                | "gc_run"
                | "semantic_import"
                | "producer_pickup"
                | "producer_run"
                | "producer_cancel"
        );
        let reply = if operation == "metrics" {
            ReplyTransfer::DeadlineBounded
        } else {
            ReplyTransfer::Chunked
        };
        Route::Read(
            kind,
            ReadPolicy {
                budget,
                read_scope,
                marker,
                awaits_first_head,
                replayable,
                reply,
            },
        )
    }

    #[test]
    fn every_operation_has_the_policy_its_string_had() {
        for operation in KNOWN
            .into_iter()
            .chain(["", "bogus", "Status", "status ", "metrics2"])
        {
            let parsed = Operation::parse(operation);
            assert_eq!(parsed.route(), expected_route(operation), "{operation:?}");
            assert_eq!(
                parsed.requires_preface(),
                operation == "metrics",
                "{operation:?}"
            );
        }
    }

    #[test]
    fn each_known_string_is_its_own_operation() {
        let parsed: Vec<Operation> = KNOWN.into_iter().map(Operation::parse).collect();
        for (index, operation) in parsed.iter().enumerate() {
            assert_ne!(*operation, Operation::Unknown, "{}", KNOWN[index]);
            assert!(!parsed[..index].contains(operation), "{}", KNOWN[index]);
        }
        for other in ["", "bogus", "Status", "status ", "metrics2"] {
            assert_eq!(Operation::parse(other), Operation::Unknown, "{other:?}");
        }
    }
}
