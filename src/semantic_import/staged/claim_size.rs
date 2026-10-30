//! Size prepass for Trellis's already-admitted canonical claim grammar. No owned tokens.
use super::*;
use std::mem::size_of;

#[derive(Debug)]
pub(super) struct ClaimSize {
    pub retained: u64,
    pub scratch: u64,
}
struct Counter<'a> {
    bytes: &'a [u8],
    offset: usize,
    max_string: u64,
    max_number: u64,
}
impl Counter<'_> {
    fn literal(&mut self, expected: &[u8]) -> Result<()> {
        anyhow::ensure!(
            self.bytes.get(self.offset..self.offset + expected.len()) == Some(expected),
            "corrupt staged claim shape"
        );
        self.offset += expected.len();
        Ok(())
    }
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.offset).copied()
    }
    fn integer(&mut self) -> Result<u64> {
        let start = self.offset;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.offset += 1;
        }
        anyhow::ensure!(self.offset != start, "corrupt staged claim integer");
        let len = (self.offset - start) as u64;
        self.max_number = self.max_number.max(len);
        Ok(len)
    }
    fn hex(&mut self) -> Result<u32> {
        let mut value = 0;
        for _ in 0..4 {
            let byte = self.peek().context("truncated staged claim escape")?;
            self.offset += 1;
            value = value * 16
                + match byte {
                    b'0'..=b'9' => (byte - b'0') as u32,
                    b'a'..=b'f' => (byte - b'a' + 10) as u32,
                    b'A'..=b'F' => (byte - b'A' + 10) as u32,
                    _ => anyhow::bail!("corrupt staged claim escape"),
                };
        }
        Ok(value)
    }
    fn string(&mut self) -> Result<u64> {
        self.literal(b"\"")?;
        let mut length = 0u64;
        loop {
            let byte = self.peek().context("truncated staged claim string")?;
            self.offset += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let escape = self.peek().context("truncated staged claim escape")?;
                    self.offset += 1;
                    let width = match escape {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => 1,
                        b'u' => {
                            let first = self.hex()?;
                            let scalar = if (0xd800..=0xdbff).contains(&first) {
                                self.literal(b"\\u")?;
                                let second = self.hex()?;
                                anyhow::ensure!(
                                    (0xdc00..=0xdfff).contains(&second),
                                    "corrupt staged claim surrogate"
                                );
                                0x10000 + ((first - 0xd800) << 10) + second - 0xdc00
                            } else {
                                first
                            };
                            char::from_u32(scalar)
                                .context("corrupt staged claim scalar")?
                                .len_utf8() as u64
                        }
                        _ => anyhow::bail!("corrupt staged claim escape"),
                    };
                    length = sizes::sum(&[length, width])?;
                }
                0..=31 => anyhow::bail!("corrupt staged claim string"),
                _ => length = sizes::sum(&[length, 1])?,
            }
        }
        self.max_string = self.max_string.max(length);
        Ok(length)
    }
    fn boolean(&mut self) -> Result<u64> {
        if self.peek() == Some(b't') {
            self.literal(b"true")?;
        } else {
            self.literal(b"false")?;
        }
        Ok(0)
    }
    fn range(&mut self) -> Result<u64> {
        if self.peek() == Some(b'n') {
            self.literal(b"null")?;
            return Ok(0);
        }
        self.literal(b"{\"encoding\":")?;
        let (entries, keys, mut payload) = if self.bytes[self.offset..].starts_with(b"\"legacy\"") {
            self.literal(b"\"legacy\",\"values\":[")?;
            let mut numbers = self.integer()?;
            self.literal(b",")?;
            numbers = sizes::sum(&[numbers, self.integer()?])?;
            self.literal(b",")?;
            numbers = sizes::sum(&[numbers, self.integer()?])?;
            let mut count = 3;
            if self.peek() == Some(b',') {
                self.literal(b",")?;
                numbers = sizes::sum(&[numbers, self.integer()?])?;
                count += 1;
            }
            self.literal(b"]")?;
            (
                2,
                "encodingvalues".len(),
                sizes::sum(&[
                    6,
                    numbers,
                    (count.max(4) * size_of::<serde_json::Value>()) as u64,
                ])?,
            )
        } else if self.bytes[self.offset..].starts_with(b"\"singleLine\"") {
            self.literal(b"\"singleLine\",\"endCharacter\":")?;
            let end = self.integer()?;
            self.literal(b",\"line\":")?;
            let line = self.integer()?;
            self.literal(b",\"startCharacter\":")?;
            let start = self.integer()?;
            (
                4,
                "encodingendCharacterlinestartCharacter".len(),
                sizes::sum(&[10, end, line, start])?,
            )
        } else {
            self.literal(b"\"multiLine\",\"endCharacter\":")?;
            let end = self.integer()?;
            self.literal(b",\"endLine\":")?;
            let end_line = self.integer()?;
            self.literal(b",\"startCharacter\":")?;
            let start = self.integer()?;
            self.literal(b",\"startLine\":")?;
            let start_line = self.integer()?;
            (
                5,
                "encodingendCharacterendLinestartCharacterstartLine".len(),
                sizes::sum(&[9, end, end_line, start, start_line])?,
            )
        };
        self.literal(b"}")?;
        payload = sizes::sum(&[
            payload,
            keys as u64,
            super::heap::btree::<String, serde_json::Value>(entries),
        ])?;
        Ok(payload)
    }
    /// A Rust claim's `cargoTarget` object (#394): `(strings, object)`, the payload bytes of
    /// its four strings, which the claim's `CargoTarget` keeps, and the parsed `Value`
    /// object's own entries and keys, which reconstruction drops.
    fn cargo_target(&mut self) -> Result<(u64, u64)> {
        self.literal(b"{\"kind\":")?;
        let kind = self.string()?;
        self.literal(b",\"package\":")?;
        let package = self.string()?;
        self.literal(b",\"root\":")?;
        let root = self.string()?;
        self.literal(b",\"version\":")?;
        let version = if self.peek() == Some(b'n') {
            self.literal(b"null")?;
            0
        } else {
            self.string()?
        };
        self.literal(b"}")?;
        Ok((
            sizes::sum(&[kind, package, root, version])?,
            sizes::sum(&[
                "kindpackagerootversion".len() as u64,
                super::heap::btree::<String, serde_json::Value>(4),
            ])?,
        ))
    }
    fn raw(&mut self) -> Result<u64> {
        self.literal(b"{")?;
        let (entries, keys, payload) =
            if self.bytes[self.offset..].starts_with(b"\"enclosingRange\":") {
                self.literal(b"\"enclosingRange\":")?;
                let enclosing = self.range()?;
                self.literal(b",\"range\":")?;
                let range = self.range()?;
                self.literal(b",\"symbol\":")?;
                let symbol = self.string()?;
                self.literal(b",\"symbolRoles\":")?;
                let roles = self.integer()?;
                self.literal(b",\"syntaxKind\":")?;
                let syntax = self.integer()?;
                (
                    5,
                    "enclosingRangerangesymbolsymbolRolessyntaxKind".len(),
                    sizes::sum(&[enclosing, range, symbol, roles, syntax])?,
                )
            } else if self.bytes[self.offset..].starts_with(b"\"displayName\":") {
                self.literal(b"\"displayName\":")?;
                let display = self.string()?;
                self.literal(b",\"enclosingSymbol\":")?;
                let enclosing = self.string()?;
                self.literal(b",\"kind\":")?;
                let kind = self.integer()?;
                self.literal(b",\"symbol\":")?;
                let symbol = self.string()?;
                (
                    4,
                    "displayNameenclosingSymbolkindsymbol".len(),
                    sizes::sum(&[display, enclosing, kind, symbol])?,
                )
            } else {
                self.literal(b"\"isDefinition\":")?;
                self.boolean()?;
                self.literal(b",\"isImplementation\":")?;
                self.boolean()?;
                self.literal(b",\"isReference\":")?;
                self.boolean()?;
                self.literal(b",\"isTypeDefinition\":")?;
                self.boolean()?;
                self.literal(b",\"sourceSymbol\":")?;
                let source = self.string()?;
                self.literal(b",\"targetSymbol\":")?;
                let target = self.string()?;
                (
                6,
                "isDefinitionisImplementationisReferenceisTypeDefinitionsourceSymboltargetSymbol"
                    .len(),
                sizes::sum(&[source, target])?,
            )
            };
        self.literal(b"}")?;
        sizes::sum(&[
            payload,
            keys as u64,
            super::heap::btree::<String, serde_json::Value>(entries),
        ])
    }
}

pub(super) fn measure(canonical: &[u8], metadata_bytes: u64) -> Result<ClaimSize> {
    let mut counter = Counter {
        bytes: canonical,
        offset: 0,
        max_string: 0,
        max_number: 0,
    };
    // A Rust claim whose Cargo target is determined leads with `cargoTarget` (#394).
    let target_prefix = b"{\"cargoTarget\":";
    let (target, target_object) = if canonical.starts_with(target_prefix) {
        counter.literal(target_prefix)?;
        let target = counter.cargo_target()?;
        counter.literal(b",\"formatVersion\":")?;
        target
    } else {
        counter.literal(b"{\"formatVersion\":")?;
        (0, 0)
    };
    let format = counter.integer()?;
    counter.literal(b",\"positionEncoding\":")?;
    let encoding = counter.integer()?;
    counter.literal(b",\"raw\":")?;
    let raw = counter.raw()?;
    counter.literal(b",\"recordKind\":")?;
    let kind = counter.string()?;
    counter.literal(b",\"relativePath\":")?;
    let path = counter.string()?;
    counter.literal(b"}")?;
    anyhow::ensure!(
        counter.offset == canonical.len(),
        "corrupt staged claim trailing bytes"
    );
    // The claim keeps no copy of the canonical bytes; serde decodes the borrowed row.
    let retained = sizes::sum(&[
        size_of::<CapturedClaim>() as u64,
        kind,
        path,
        raw,
        target,
        metadata_bytes,
    ])?;
    // Current reconstruction parses an envelope Value then moves raw out. Its remaining
    // fields and object entries overlap the final claim; raw itself is not copied.
    let scratch = sizes::sum(&[
        // The parsed envelope has a sixth entry, `cargoTarget`, holding its own object and a
        // copy of the target strings, when the claim has a target.
        super::heap::btree::<String, serde_json::Value>(if target_object > 0 { 6 } else { 5 }),
        "formatVersionpositionEncodingrawrecordKindrelativePath".len() as u64,
        if target_object > 0 {
            sizes::sum(&["cargoTarget".len() as u64, target_object, target])?
        } else {
            0
        },
        format,
        encoding,
        kind,
        path,
        size_of::<serde_json::Value>() as u64,
        size_of::<CapturedClaim>() as u64,
        3 * size_of::<String>() as u64,
        sizes::sum(&[counter.max_string, counter.max_string])?.max(8),
        // Pinned serde_json's arbitrary-precision scanner starts with capacity 16.
        sizes::sum(&[counter.max_number, counter.max_number])?.max(16),
    ])?;
    Ok(ClaimSize { retained, scratch })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn payload(value: &serde_json::Value) -> u64 {
        match value {
            serde_json::Value::Null | serde_json::Value::Bool(_) => 0,
            serde_json::Value::Number(value) => value.to_string().len() as u64,
            serde_json::Value::String(value) => value.len() as u64,
            serde_json::Value::Array(values) => {
                values.capacity() as u64 * size_of::<serde_json::Value>() as u64
                    + values.iter().map(payload).sum::<u64>()
            }
            serde_json::Value::Object(values) => {
                super::super::heap::btree::<String, serde_json::Value>(values.len())
                    + values
                        .iter()
                        .map(|(key, value)| key.len() as u64 + payload(value))
                        .sum::<u64>()
            }
        }
    }
    #[test]
    fn prepass_counts_canonical_shapes_and_escaped_utf8_without_owned_tokens() {
        let ranges = [
            serde_json::json!({"encoding":"legacy","values":[0,2,4]}),
            serde_json::json!({"encoding":"legacy","values":[0,2,1,4]}),
            serde_json::json!({"encoding":"singleLine","endCharacter":4,"line":0,"startCharacter":2}),
            serde_json::json!({"encoding":"multiLine","endCharacter":4,"endLine":1,"startCharacter":2,"startLine":0}),
        ];
        let mut raws = vec![
            serde_json::json!({"displayName":"é\n\"\\🙂","enclosingSymbol":"","kind":17,"symbol":"pkg a."}),
            serde_json::json!({"isDefinition":true,"isImplementation":false,"isReference":false,"isTypeDefinition":true,"sourceSymbol":"pkg a.","targetSymbol":"pkg b."}),
        ];
        for range in ranges {
            for enclosing in [serde_json::Value::Null, range.clone()] {
                raws.push(serde_json::json!({"enclosingRange":enclosing,"range":range,"symbol":"pkg é.","symbolRoles":1,"syntaxKind":0}));
            }
        }
        for raw in raws {
            let envelope = serde_json::json!({"formatVersion":1,"positionEncoding":1,"raw":raw,"recordKind":"occurrence","relativePath":"src/é.js"});
            let canonical = crate::native_ids::canonical(&envelope);
            let size = measure(&canonical, 11).unwrap();
            let decoded: serde_json::Value = serde_json::from_slice(&canonical).unwrap();
            assert_eq!(
                size.retained,
                size_of::<CapturedClaim>() as u64
                    + 10
                    + "src/é.js".len() as u64
                    + payload(&decoded["raw"])
                    + 11
            );
            assert!(size.scratch > 0);
        }
        let mut counter = Counter {
            bytes: br#""\ud83d\ude42""#,
            offset: 0,
            max_string: 0,
            max_number: 0,
        };
        assert_eq!(counter.string().unwrap(), 4);
    }

    #[test]
    fn claim_budget_denial_precedes_decode_and_releases_for_retry() {
        let canonical = br#"{"formatVersion":1,"positionEncoding":1,"raw":{"displayName":"a","enclosingSymbol":"","kind":17,"symbol":"pkg a."},"recordKind":"symbolInformation","relativePath":"A.js"}"#;
        let conn = Connection::open_in_memory().unwrap();
        let mut statement = conn
            .prepare("SELECT 0,0,?1,'digest','proof','artifact','javascript'")
            .unwrap();
        let metadata =
            ("digest".len() + "proof".len() + "artifact".len() + "javascript".len()) as u64;
        let size = measure(canonical, metadata).unwrap();
        let limit = sizes::sum(&[size.retained, size.scratch]).unwrap();
        let denied = LiveBudget::new(limit - 1);
        // Same admitted shape and size, but decoding this scalar would fail UTF-8.
        // A typed budget failure therefore proves the owned decoder was not entered.
        let mut invalid = canonical.to_vec();
        invalid[canonical
            .windows(3)
            .position(|bytes| bytes == b"\"a\"")
            .unwrap()
            + 1] = 255;
        let error = {
            let mut rows = statement.query([&invalid]).unwrap();
            let row = rows.next().unwrap().unwrap();
            let mut reservation = denied.reserve(0).unwrap();
            decode_reserved_claim(row, &denied, &mut reservation).unwrap_err()
        };
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name == "unit expansion")
        );
        drop(denied.reserve(limit - 1).unwrap());
        let allowed = LiveBudget::new(limit);
        {
            let mut rows = statement.query([&invalid]).unwrap();
            let row = rows.next().unwrap().unwrap();
            let mut reservation = allowed.reserve(0).unwrap();
            let error = decode_reserved_claim(row, &allowed, &mut reservation).unwrap_err();
            assert!(error.downcast_ref::<ScipCaptureError>().is_none());
        }
        {
            let mut rows = statement.query([canonical.as_slice()]).unwrap();
            let row = rows.next().unwrap().unwrap();
            let mut reservation = allowed.reserve(0).unwrap();
            let claim = decode_reserved_claim(row, &allowed, &mut reservation).unwrap();
            assert_eq!(claim.envelope.record_kind, "symbolInformation");
            assert_eq!(claim.envelope.raw["kind"], 17);
        }
        drop(allowed.reserve(limit).unwrap());
    }
    /// Canonical claims of the shapes rust-analyzer produces, as staging stores them.
    fn rust_analyzer_claims() -> Vec<Vec<u8>> {
        let symbol = "rust-analyzer cargo trellis 0.1.0 store/impl#[Store]open_for_tests().";
        let single = serde_json::json!({"encoding":"singleLine","endCharacter":31,"line":4210,"startCharacter":18});
        let multi = serde_json::json!({"encoding":"multiLine","endCharacter":5,"endLine":4290,"startCharacter":4,"startLine":4206});
        let raws = [
            serde_json::json!({"enclosingRange":null,"range":single,"symbol":symbol,"symbolRoles":0,"syntaxKind":0}),
            serde_json::json!({"enclosingRange":multi,"range":single,"symbol":symbol,"symbolRoles":1,"syntaxKind":0}),
            serde_json::json!({"displayName":"open_for_tests","enclosingSymbol":"","kind":17,"symbol":symbol}),
            serde_json::json!({"isDefinition":false,"isImplementation":true,"isReference":false,"isTypeDefinition":false,"sourceSymbol":symbol,"targetSymbol":"rust-analyzer cargo trellis 0.1.0 store/Reader#"}),
        ];
        let kinds = [
            "occurrence",
            "occurrence",
            "symbolInformation",
            "relationship",
        ];
        // Each shape without a Cargo target, and with one (#394), with and without a version.
        let targets = [
            None,
            Some(
                serde_json::json!({"kind":"lib","package":"trellis","root":"src/lib.rs","version":"0.1.0"}),
            ),
            Some(
                serde_json::json!({"kind":"test","package":"trellis","root":"tests/store.rs","version":null}),
            ),
        ];
        raws.iter()
            .zip(kinds)
            .flat_map(|(raw, kind)| {
                targets.iter().map(move |target| {
                    let mut envelope = serde_json::json!({"formatVersion":1,"positionEncoding":1,"raw":raw,"recordKind":kind,"relativePath":"src/store.rs"});
                    if let Some(target) = target {
                        envelope["cargoTarget"] = target.clone();
                    }
                    crate::native_ids::canonical(&envelope)
                })
            })
            .collect()
    }

    /// The retained estimate is the decoded claim's heap storage exactly, plus its root (a
    /// slot of the document's claim Vec); retained plus scratch covers the decode's peak,
    /// including the row's metadata Strings the decode copies.
    #[test]
    fn retained_estimate_is_the_decoded_claims_storage() {
        let (digest, proof, artifact) = ("d".repeat(64), "p".repeat(71), "a".repeat(64));
        let metadata = (digest.len() + proof.len() + artifact.len() + "rust".len()) as u64;
        for canonical in rust_analyzer_claims() {
            let size = measure(&canonical, metadata).unwrap();
            let (claim, retained, peak) = crate::test_alloc::measure_peak(|| {
                claim_from_stage(
                    &canonical,
                    digest.clone(),
                    proof.clone(),
                    "rust".to_owned(),
                    artifact.clone(),
                )
                .unwrap()
            });
            let root = size_of::<CapturedClaim>() as u64;
            assert_eq!(
                size.retained,
                retained + root,
                "{}",
                claim.envelope.record_kind
            );
            assert!(peak <= size.retained + size.scratch, "{peak} vs {size:?}");
        }
    }

    #[test]
    fn malformed_counter_input_returns_errors() {
        for bytes in [b"".as_slice(), b"{}", b"{\"formatVersion\":"] {
            assert!(measure(bytes, 0).is_err());
        }
    }
}
