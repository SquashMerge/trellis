use super::*;
use crate::store::mcp_read::McpSource;
#[test]
fn source_complete_frame_or_range_error_without_truncation() {
    let pin: crate::model::IndexPin = serde_json::from_value(json!({
        "indexGeneration":"123e4567-e89b-42d3-a456-426614174000",
        "indexRevision":1}))
    .unwrap();
    let item = |text: String| McpSource {
        path: "a.js".into(),
        content_hash: "a".repeat(64),
        start_line: 1,
        end_line: 1,
        byte_start: 0,
        byte_end: text.len(),
        text,
        pin,
    };
    for modern in [true, false] {
        let envelope = source(
            &json!(1),
            item("hello\r\n".into()),
            std::path::Path::new("/root"),
            false,
            modern,
        )
        .unwrap();
        assert_eq!(envelope["data"]["text"], "hello\r\n");
        assert_eq!(envelope["data"]["byteRange"], json!({"start":0,"end":7}));
        assert_eq!(envelope["partial"], false);
        assert_eq!(envelope["truncated"], false);
        assert_eq!(envelope["warnings"][0]["code"], "syntaxOnly");
        let frame = json!({"jsonrpc":"2.0","id":1,
            "result":super::super::tools::result("trellis_read_source", envelope,modern)});
        let mut bytes = Vec::new();
        super::super::wire::write_response(&mut bytes, &frame).unwrap();
        assert!(bytes.len() <= super::super::wire::RESPONSE_BYTES);
        assert!(
            source(
                &json!(1),
                item("\0".repeat(16384)),
                std::path::Path::new("/root"),
                false,
                modern
            )
            .is_none()
        );
    }
}
