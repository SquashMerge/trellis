//! The stored search-card records and the declaration kinds that take a card.
//!
//! Plain data and pure predicates only: capture, fact preparation and card rendering stay in
//! `search_capture` and `search_card`, which re-export these names.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct Ancestor {
    pub kind: String,
    pub name: Option<String>,
    pub self_type: Option<String>,
    pub trait_name: Option<String>,
}

/// Prepared once for storage and card production, without rereading source.
#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
pub struct PreparedFacts {
    pub doc: String,
    pub calls: Vec<String>,
    pub types: Vec<String>,
    pub strings: Vec<String>,
    pub error_codes: Vec<String>,
    pub members: Vec<String>,
    pub head: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct SearchCard {
    pub text: String,
    pub hash: String,
}

pub fn is_container(kind: &str) -> bool {
    matches!(kind, "type" | "implementation" | "namespace" | "module")
}

pub fn is_eligible(kind: &str, name: Option<&str>) -> bool {
    (is_container(kind) || matches!(kind, "function" | "method"))
        && name.is_some_and(|name| !name.is_empty())
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct CapturedCard {
    pub path: String,
    pub syntax_id: String,
    pub card: SearchCard,
    pub ancestors: Vec<Ancestor>,
    pub facts: PreparedFacts,
}
