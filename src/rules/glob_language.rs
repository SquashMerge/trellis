//! Exact inclusion for the admitted glob grammar, intersected with valid file paths.
//!
//! globset's wildcards consume bytes, not Unicode scalars. The product therefore
//! includes a UTF-8 recognizer as well as the repository path-component grammar.
use anyhow::{Result, ensure};
use std::collections::{HashSet, VecDeque};

#[cfg(test)]
use crate::references::work;
use crate::references::work::Work;
// The existing file_tree::valid_path bound; inclusion includes this bound too.
const PATH_BYTES: usize = 8192;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Relation {
    Equivalent,
    StrictSubset,
    StrictSuperset,
    Incomparable,
}

pub(super) fn relation(
    a: &str,
    b: &str,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Relation> {
    let mut budget = Budget { work, check };
    let a = Nfa::compile(a, &mut budget)?;
    let b = Nfa::compile(b, &mut budget)?;
    compare(&a, &b, PATH_BYTES, &mut budget)
}

struct Budget<'a, F> {
    work: &'a mut Work,
    check: &'a mut F,
}
impl<F: FnMut() -> Result<()>> Budget<'_, F> {
    fn charge(&mut self, amount: usize) -> Result<()> {
        (self.check)()?;
        ensure!(
            self.work.charge(amount),
            "bounded_work: glob language comparison budget"
        );
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum ByteMatch {
    Literal(u8),
    NonSlash,
    Any,
}
impl ByteMatch {
    fn accepts(self, byte: u8) -> bool {
        match self {
            Self::Literal(b) => b == byte,
            Self::NonSlash => byte != b'/',
            Self::Any => true,
        }
    }
}
#[derive(Default)]
struct State {
    edges: Vec<(ByteMatch, usize)>,
    epsilon: Vec<usize>,
}
struct Nfa {
    states: Vec<State>,
    accept: usize,
}
impl Nfa {
    fn state(&mut self, budget: &mut Budget<'_, impl FnMut() -> Result<()>>) -> Result<usize> {
        budget.charge(1)?;
        let index = self.states.len();
        self.states.push(State::default());
        Ok(index)
    }
    fn edge(
        &mut self,
        from: usize,
        matcher: ByteMatch,
        to: usize,
        budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
    ) -> Result<()> {
        budget.charge(1)?;
        self.states[from].edges.push((matcher, to));
        Ok(())
    }
    fn epsilon(
        &mut self,
        from: usize,
        to: usize,
        budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
    ) -> Result<()> {
        budget.charge(1)?;
        self.states[from].epsilon.push(to);
        Ok(())
    }
    fn byte(
        &mut self,
        current: usize,
        matcher: ByteMatch,
        budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
    ) -> Result<usize> {
        let next = self.state(budget)?;
        self.edge(current, matcher, next, budget)?;
        Ok(next)
    }
    fn star(
        &mut self,
        current: usize,
        matcher: ByteMatch,
        budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
    ) -> Result<usize> {
        let next = self.state(budget)?;
        self.edge(current, matcher, current, budget)?;
        self.epsilon(current, next, budget)?;
        Ok(next)
    }
    // globset RecursivePrefix: (?:/?|.*/). The optional leading slash is
    // impossible in the valid-path intersection, so empty | .*/ suffices.
    fn prefix(
        &mut self,
        current: usize,
        budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
    ) -> Result<usize> {
        let looping = self.state(budget)?;
        let next = self.state(budget)?;
        self.epsilon(current, looping, budget)?;
        self.epsilon(current, next, budget)?;
        self.edge(looping, ByteMatch::Any, looping, budget)?;
        self.edge(looping, ByteMatch::Literal(b'/'), next, budget)?;
        Ok(next)
    }
    fn compile(pattern: &str, budget: &mut Budget<'_, impl FnMut() -> Result<()>>) -> Result<Self> {
        // Callers pass only patterns already validated by rules::pattern.
        budget.charge(pattern.len())?;
        let mut components = Vec::new();
        for component in pattern.split('/') {
            budget.charge(1)?;
            // globset collapses adjacent recursive components, including at
            // the beginning/end (e.g. **/** and a/**/**/b).
            if component != "**" || components.last() != Some(&"**") {
                budget.charge(1)?;
                components.push(component);
            }
        }
        let mut nfa = Self {
            states: Vec::new(),
            accept: 0,
        };
        let mut current = nfa.state(budget)?;
        if components.as_slice() == ["**"] {
            current = nfa.star(current, ByteMatch::Any, budget)?;
        } else {
            for (index, component) in components.iter().enumerate() {
                budget.charge(1)?;
                if *component == "**" {
                    if index == 0 {
                        current = nfa.prefix(current, budget)?;
                    } else {
                        current = nfa.byte(current, ByteMatch::Literal(b'/'), budget)?;
                        if index + 1 == components.len() {
                            // RecursiveSuffix is /.*, not an optional slash.
                            current = nfa.star(current, ByteMatch::Any, budget)?;
                        } else {
                            // RecursiveZeroOrMore is (?:/|/.*/).
                            current = nfa.prefix(current, budget)?;
                        }
                    }
                } else {
                    if index > 0 && components[index - 1] != "**" {
                        current = nfa.byte(current, ByteMatch::Literal(b'/'), budget)?;
                    }
                    for byte in component.bytes() {
                        budget.charge(1)?;
                        current = match byte {
                            b'*' => nfa.star(current, ByteMatch::NonSlash, budget)?,
                            b'?' => nfa.byte(current, ByteMatch::NonSlash, budget)?,
                            b => nfa.byte(current, ByteMatch::Literal(b), budget)?,
                        };
                    }
                }
            }
        }
        nfa.accept = current;
        Ok(nfa)
    }
    fn closure(
        &self,
        subset: &mut Vec<usize>,
        budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
    ) -> Result<()> {
        let mut cursor = 0;
        while cursor < subset.len() {
            budget.charge(1)?;
            let from = subset[cursor];
            for &to in &self.states[from].epsilon {
                // All epsilon edges point forward; sorted insertion cannot
                // introduce an unvisited state behind the cursor.
                debug_assert!(to > from);
                insert(subset, to, budget)?;
            }
            cursor += 1;
        }
        Ok(())
    }
    fn start(&self, budget: &mut Budget<'_, impl FnMut() -> Result<()>>) -> Result<Vec<usize>> {
        budget.charge(1)?;
        let mut subset = vec![0];
        self.closure(&mut subset, budget)?;
        Ok(subset)
    }
    fn advance(
        &self,
        subset: &[usize],
        byte: u8,
        budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
    ) -> Result<Vec<usize>> {
        budget.charge(1)?;
        let mut next = Vec::new();
        for &from in subset {
            budget.charge(1)?;
            for &(matcher, to) in &self.states[from].edges {
                budget.charge(1)?;
                if matcher.accepts(byte) {
                    insert(&mut next, to, budget)?;
                }
            }
        }
        self.closure(&mut next, budget)?;
        Ok(next)
    }
}
fn insert(
    subset: &mut Vec<usize>,
    value: usize,
    budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
) -> Result<()> {
    // Covers lookup and the worst-case movement/allocation before insertion.
    budget.charge(subset.len() + 1)?;
    if let Err(index) = subset.binary_search(&value) {
        subset.insert(index, value);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
enum Utf8 {
    #[default]
    Ready,
    One,
    Two,
    Three,
    E0,
    Ed,
    F0,
    F4,
}
impl Utf8 {
    fn advance(self, byte: u8) -> Option<Self> {
        match (self, byte) {
            (Self::Ready, 0x00..=0x7f) => Some(Self::Ready),
            (Self::Ready, 0xc2..=0xdf) => Some(Self::One),
            (Self::Ready, 0xe0) => Some(Self::E0),
            (Self::Ready, 0xe1..=0xec | 0xee..=0xef) => Some(Self::Two),
            (Self::Ready, 0xed) => Some(Self::Ed),
            (Self::Ready, 0xf0) => Some(Self::F0),
            (Self::Ready, 0xf1..=0xf3) => Some(Self::Three),
            (Self::Ready, 0xf4) => Some(Self::F4),
            (Self::One, 0x80..=0xbf) => Some(Self::Ready),
            (Self::Two, 0x80..=0xbf) => Some(Self::One),
            (Self::Three, 0x80..=0xbf) => Some(Self::Two),
            (Self::E0, 0xa0..=0xbf) | (Self::Ed, 0x80..=0x9f) => Some(Self::One),
            (Self::F0, 0x90..=0xbf) | (Self::F4, 0x80..=0x8f) => Some(Self::Two),
            _ => None,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
enum Component {
    #[default]
    Empty,
    Dot,
    DotDot,
    Other,
}
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
struct Domain {
    utf8: Utf8,
    component: Component,
}
impl Domain {
    fn advance(self, byte: u8) -> Option<Self> {
        if matches!(byte, 0 | b'\\' | b':') {
            return None;
        }
        let utf8 = self.utf8.advance(byte)?;
        let component = match (self.component, byte) {
            (Component::Other, b'/') => Component::Empty,
            (_, b'/') => return None,
            (Component::Empty, b'.') => Component::Dot,
            (Component::Dot, b'.') => Component::DotDot,
            _ => Component::Other,
        };
        Some(Self { utf8, component })
    }
    fn accepts(self) -> bool {
        self.utf8 == Utf8::Ready && self.component == Component::Other
    }
}
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Product {
    a: Vec<usize>,
    b: Vec<usize>,
    domain: Domain,
}
impl Product {
    fn size(&self) -> usize {
        self.a.len() + self.b.len() + 1
    }
}

fn alphabet(
    a: &Nfa,
    b: &Nfa,
    budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
) -> Result<Vec<u8>> {
    // Boundaries of byte classes for Domain and Utf8. Literal NFA edges split
    // these further. Within a class every automaton has identical transitions.
    let mut cuts = [false; 256];
    for byte in [
        0, 1, 46, 47, 48, 58, 59, 92, 93, 128, 144, 160, 192, 194, 224, 225, 237, 238, 240, 241,
        244, 245,
    ] {
        budget.charge(1)?;
        cuts[byte] = true;
    }
    for nfa in [a, b] {
        for state in &nfa.states {
            budget.charge(1)?;
            for &(matcher, _) in &state.edges {
                budget.charge(1)?;
                if let ByteMatch::Literal(byte) = matcher {
                    cuts[usize::from(byte)] = true;
                    if byte < 255 {
                        cuts[usize::from(byte) + 1] = true;
                    }
                }
            }
        }
    }
    let mut alphabet = Vec::new();
    for (byte, cut) in cuts.into_iter().enumerate() {
        budget.charge(1)?;
        if cut {
            budget.charge(1)?;
            alphabet.push(byte as u8);
        }
    }
    Ok(alphabet)
}

fn compare(
    a: &Nfa,
    b: &Nfa,
    max_bytes: usize,
    budget: &mut Budget<'_, impl FnMut() -> Result<()>>,
) -> Result<Relation> {
    let alphabet = alphabet(a, b, budget)?;
    let initial = Product {
        a: a.start(budget)?,
        b: b.start(budget)?,
        domain: Domain::default(),
    };
    budget.charge(initial.size() * 2 + 1)?;
    let mut seen = HashSet::from([initial.clone()]);
    let mut queue = VecDeque::from([(initial, 0)]);
    let (mut only_a, mut only_b) = (false, false);
    while let Some((state, length)) = queue.pop_front() {
        budget.charge(state.size())?;
        if state.domain.accepts() {
            let in_a = state.a.binary_search(&a.accept).is_ok();
            let in_b = state.b.binary_search(&b.accept).is_ok();
            only_a |= in_a && !in_b;
            only_b |= in_b && !in_a;
            if only_a && only_b {
                return Ok(Relation::Incomparable);
            }
        }
        if length == max_bytes {
            continue;
        }
        for &byte in &alphabet {
            budget.charge(1)?;
            let Some(domain) = state.domain.advance(byte) else {
                continue;
            };
            let next = Product {
                a: a.advance(&state.a, byte, budget)?,
                b: b.advance(&state.b, byte, budget)?,
                domain,
            };
            if next.a.is_empty() && next.b.is_empty() {
                continue;
            }
            budget.charge(next.size())?;
            if !seen.contains(&next) {
                // Account for hashing, copying and both container insertions
                // before growth. The request owns and discards the whole graph.
                budget.charge(next.size() * 2 + 1)?;
                seen.insert(next.clone());
                queue.push_back((next, length + 1));
            }
        }
    }
    // Unit-byte BFS visits a product state at its shortest distance. Any later
    // visit has the same possible suffixes and less of the 8192-byte allowance,
    // so omitting it cannot hide a witness inside the path-length bound.
    Ok(match (only_a, only_b) {
        (false, false) => Relation::Equivalent,
        (false, true) => Relation::StrictSubset,
        (true, false) => Relation::StrictSuperset,
        (true, true) => unreachable!("returned when both witnesses were found"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use globset::{GlobBuilder, GlobSetBuilder};

    fn compile(pattern: &str) -> Nfa {
        Nfa::compile(
            pattern,
            &mut Budget {
                work: &mut Work::new(work::rules_limit(0, 0)),
                check: &mut || Ok(()),
            },
        )
        .unwrap()
    }
    fn accepts(nfa: &Nfa, path: &str) -> bool {
        let mut work = Work::new(work::rules_limit(0, 0));
        let mut check = || Ok(());
        let mut budget = Budget {
            work: &mut work,
            check: &mut check,
        };
        let mut state = nfa.start(&mut budget).unwrap();
        for byte in path.bytes() {
            state = nfa.advance(&state, byte, &mut budget).unwrap();
        }
        state.binary_search(&nfa.accept).is_ok()
    }
    fn domain_accepts(path: &[u8]) -> bool {
        let mut state = Domain::default();
        for &byte in path {
            let Some(next) = state.advance(byte) else {
                return false;
            };
            state = next;
        }
        state.accepts()
    }
    fn related(a: &str, b: &str) -> Relation {
        relation(
            a,
            b,
            &mut Work::new(work::rules_limit(0, 0)),
            &mut || Ok(()),
        )
        .unwrap()
    }
    fn bounded_relation(a: &str, b: &str, limit: usize) -> Relation {
        compare(
            &compile(a),
            &compile(b),
            limit,
            &mut Budget {
                work: &mut Work::new(work::rules_limit(0, 0)),
                check: &mut || Ok(()),
            },
        )
        .unwrap()
    }

    #[test]
    fn approved_containment_and_overlap() {
        for (a, b) in [
            ("src/store.rs", "src/*.rs"),
            ("src/store/search/**", "src/store/**"),
            ("src/?.rs", "src/*.rs"),
            ("src/*??*", "src/*"),
            ("src/*/*.rs", "src/**/*.rs"),
            ("src/store/**", "**"),
            ("**/?.rs", "**/*.rs"),
            ("src/é?.rs", "src/é*.rs"),
        ] {
            assert_eq!(related(a, b), Relation::StrictSubset, "{a} < {b}");
            assert_eq!(related(b, a), Relation::StrictSuperset, "{b} > {a}");
        }
        for (a, b) in [
            ("src/*?*", "src/?*"),
            ("**/**", "**"),
            ("a/**/**/b", "a/**/b"),
            ("a/**/**", "a/**"),
            ("**/**/a", "**/a"),
            ("src/*", "src/?*"), // Empty components are not valid paths.
        ] {
            assert_eq!(related(a, b), Relation::Equivalent, "{a} = {b}");
        }
        assert!(accepts(&compile("src/*??*"), "src/ab"));
        assert!(accepts(&compile("src/*"), "src/ab"));
        assert_eq!(
            related("src/a*/b.rs", "src/ab/*.rs"),
            Relation::Incomparable
        );
        assert_eq!(related("a/**", "a"), Relation::Incomparable);
    }

    #[test]
    fn byte_wildcards_and_valid_path_intersection() {
        assert_eq!(related("é", "?"), Relation::Incomparable);
        assert_eq!(related("é", "??"), Relation::StrictSubset);
        assert_eq!(related("*", "?*"), Relation::Equivalent);
        for path in [
            "", ".", "..", "/a", "a/", "a//b", "a/./b", "a/../b", "a:b", "a\\b", "a\0b",
        ] {
            assert!(!domain_accepts(path.as_bytes()), "{path:?}");
        }
        for path in ["a", ".a", "...", "a/.b", "é/中/😀", "a\nb"] {
            assert!(domain_accepts(path.as_bytes()), "{path:?}");
        }
        for bytes in [
            &b"\x80"[..],
            &b"\xc0\xaf"[..],
            &b"\xed\xa0\x80"[..],
            &b"\xf4\x90\x80\x80"[..],
            &b"\xf0\x80\x80\x80"[..],
        ] {
            assert!(!domain_accepts(bytes), "{bytes:?}");
        }
    }

    #[test]
    fn globset_differential_fixtures() {
        let patterns = [
            "**",
            "*",
            "?",
            "??",
            "é",
            "é?",
            "*?*",
            "**/**",
            "**/**/**",
            "a/**",
            "a/**/**",
            "**/a",
            "**/**/a",
            "a/**/b",
            "a/**/**/b",
            "**/a/**",
            "a/*/b",
            "a/?",
            "a/*??*",
            "a/é*",
            "a/**/é?",
            "*.rs",
            "**/*.rs",
            "src/*",
            "src/*??*",
            "src/*/*.rs",
            "src/**/*.rs",
            "src/store/**",
        ];
        let mut paths: Vec<String> = [
            "a",
            "b",
            "é",
            "中",
            "😀",
            "src/ab",
            "src/ab.rs",
            "a/b",
            "a/a/b",
            "a/x/y/b",
            "a/é",
            "a/éx",
            "a/é/x/b",
            "a/\nb",
            "a/\n/b",
            "a.rs",
            "x/a.rs",
            "src/a.rs",
            "src/x/a.rs",
            "src/store.rs",
            "src/store/a.rs",
            "src/store/x/a.rs",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        // Exhaustive normal-size byte-sensitive words, filtered to the domain.
        let alphabet = ["a", ".", "/", "é", "\n"];
        let mut frontier = vec![String::new()];
        for _ in 0..4 {
            let mut next = Vec::new();
            for prefix in frontier {
                for suffix in alphabet {
                    let path = format!("{prefix}{suffix}");
                    if !path.is_empty() && crate::file_tree::valid_path(&path) {
                        paths.push(path.clone());
                    }
                    next.push(path);
                }
            }
            frontier = next;
        }
        for pattern in patterns {
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .backslash_escape(false)
                .build()
                .unwrap();
            let mut builder = GlobSetBuilder::new();
            builder.add(glob);
            let matcher = builder.build().unwrap();
            let nfa = compile(pattern);
            for path in &paths {
                assert_eq!(
                    accepts(&nfa, path),
                    matcher.is_match(path),
                    "{pattern:?} / {path:?}"
                );
            }
        }
    }

    #[test]
    fn shortest_witness_respects_byte_length_bound() {
        assert_eq!(bounded_relation("??", "???", 1), Relation::Equivalent);
        assert_eq!(bounded_relation("??", "???", 2), Relation::StrictSuperset);
        assert_eq!(bounded_relation("??", "???", 3), Relation::Incomparable);
        assert_eq!(bounded_relation("é", "??", 1), Relation::Equivalent);
        assert_eq!(bounded_relation("é", "??", 2), Relation::StrictSubset);
        // Witness pruning uses the same byte-distance rule with a small bound.
        assert_eq!(bounded_relation("aaaa", "bbbb", 3), Relation::Equivalent);
    }

    #[test]
    fn compilation_and_product_growth_are_bounded_and_cancelled() {
        let mut work = Work::new(0);
        let error = relation("*", "?", &mut work, &mut || Ok(())).unwrap_err();
        assert!(error.to_string().contains("bounded_work"));
        let error = relation(
            "*",
            "?",
            &mut Work::new(work::rules_limit(0, 0)),
            &mut || anyhow::bail!("cancelled"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
        let a = compile("a/**");
        let b = compile("**/a");
        let mut exhausted = Work::new(0);
        let error = compare(
            &a,
            &b,
            PATH_BYTES,
            &mut Budget {
                work: &mut exhausted,
                check: &mut || Ok(()),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("bounded_work"));
        let error = compare(
            &a,
            &b,
            PATH_BYTES,
            &mut Budget {
                work: &mut Work::new(work::rules_limit(0, 0)),
                check: &mut || anyhow::bail!("cancelled"),
            },
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
    }
}
