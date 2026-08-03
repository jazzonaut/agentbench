//! The transcript wire format, reduced to the fields the daemon measures.
//!
//! Transcripts are large — several hundred megabytes across a few hundred files — and almost all of
//! that volume is payload the daemon has no use for: file contents, diffs, thinking blocks, command
//! output. Every field here is therefore either measured or used to link two rows together, and the
//! bulky ones are skipped without being materialised.

use serde::{
    Deserialize, Deserializer,
    de::{IgnoredAny, SeqAccess, Visitor},
};
use std::fmt;

/// One line of a transcript.
///
/// Unknown fields are ignored rather than rejected: the format gains fields between Claude Code
/// releases, and a daemon that stopped importing on an unrecognised key would break on upgrade.
#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Row {
    #[serde(rename = "type")]
    pub row_type: String,
    pub uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub timestamp: Option<String>,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    /// Working directory of the session, used as the project identity.
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    /// Claude Code's own version, the only place it is recorded.
    pub version: Option<String>,
    pub effort: Option<String>,
    pub is_meta: bool,
    pub is_api_error_message: bool,
    pub is_sidechain: bool,
    /// The assistant row whose tool call this row answers.
    #[serde(rename = "sourceToolAssistantUUID")]
    pub source_tool_assistant_uuid: Option<String>,
    /// Set when a tool call was refused rather than run.
    pub tool_denial_kind: Option<String>,
    /// Present on a row carrying a tool's result.
    ///
    /// Deliberately not deserialised. The payload is an entire file, diff or command output — the
    /// bulk of a transcript — and only its presence distinguishes a tool result from a prompt. It is
    /// also variously an object, a string or an array, so a typed shape would reject valid rows.
    pub tool_use_result: Option<IgnoredAny>,
    pub message: Option<Message>,
}

/// What a row means to the importer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A model response, possibly one of several for a single API request.
    Assistant,
    /// Something the person typed, which starts a measurable response interval.
    Prompt,
    /// The result of a tool call, which ends a measurable tool interval.
    ToolResult,
    /// Everything else: attachments, mode changes, hook output, snapshots. Not measured, but its
    /// identity still matters, because an attachment sits between a prompt and its answer.
    Other,
}

impl Row {
    /// Classify the row.
    pub fn kind(&self) -> Kind {
        match self.row_type.as_str() {
            "assistant" => Kind::Assistant,
            "user" if self.tool_use_result.is_some() => Kind::ToolResult,
            // A meta row is injected context rather than something a person typed, so it must not
            // start a response interval.
            "user" if !self.is_meta => Kind::Prompt,
            _ => Kind::Other,
        }
    }

    /// Milliseconds since the Unix epoch, if the row carries a parseable timestamp.
    pub fn ts_ms(&self) -> Option<i64> {
        self.timestamp.as_deref().and_then(parse_ms)
    }

    /// Tool calls requested by this row.
    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str)> {
        self.blocks()
            .filter(|block| block.kind == "tool_use")
            .filter_map(|block| Some((block.id.as_deref()?, block.name.as_deref()?)))
    }

    /// The tool result carried by this row, as `(tool_use_id, failed)`.
    pub fn tool_result(&self) -> Option<(Option<&str>, bool)> {
        self.blocks()
            .find(|block| block.kind == "tool_result")
            .map(|block| (block.tool_use_id.as_deref(), block.is_error == Some(true)))
    }

    fn blocks(&self) -> impl Iterator<Item = &Block> {
        self.message.iter().flat_map(|message| &message.content)
    }
}

/// The API message a row wraps.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Message {
    pub model: Option<String>,
    pub usage: Option<Usage>,
    #[serde(deserialize_with = "content_blocks")]
    pub content: Vec<Block>,
}

/// Token accounting for one API request.
///
/// Repeated identically on every assistant row belonging to that request, and cumulative rather than
/// incremental, which is why the importer collapses rows by request instead of summing them.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub service_tier: Option<String>,
}

/// One content block. Text, thinking and result payloads are skipped by the deserialiser.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Block {
    #[serde(rename = "type")]
    pub kind: String,
    pub id: Option<String>,
    pub name: Option<String>,
    pub tool_use_id: Option<String>,
    /// Absent, null, or a boolean, depending on the tool.
    pub is_error: Option<bool>,
}

/// Milliseconds since the Unix epoch for an RFC 3339 timestamp.
pub fn parse_ms(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|when| when.timestamp_millis())
}

/// Accept both content shapes without buffering the row.
///
/// A user message's content is sometimes a bare string. Serde's untagged enums would handle that by
/// materialising every content block into an intermediate value first, which on a transcript means
/// rebuilding megabytes of file contents just to discard them.
fn content_blocks<'de, D>(deserializer: D) -> Result<Vec<Block>, D::Error>
where
    D: Deserializer<'de>,
{
    struct BlocksVisitor;

    impl<'de> Visitor<'de> for BlocksVisitor {
        type Value = Vec<Block>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a content string or a list of content blocks")
        }

        fn visit_str<E>(self, _value: &str) -> Result<Self::Value, E> {
            // Prose carries nothing measurable.
            Ok(Vec::new())
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())
        }

        fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let mut blocks = Vec::new();
            while let Some(block) = seq.next_element::<Block>()? {
                blocks.push(block);
            }
            Ok(blocks)
        }
    }

    deserializer.deserialize_any(BlocksVisitor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(json: &str) -> Row {
        serde_json::from_str(json).expect("row should parse")
    }

    #[test]
    fn timestamps_become_epoch_milliseconds() {
        assert_eq!(parse_ms("1970-01-01T00:00:01.500Z"), Some(1500));
        assert_eq!(
            parse_ms("2026-06-27T00:08:43.945Z"),
            Some(1_782_518_923_945)
        );
        // An offset is respected rather than ignored, or an hour of history lands in the wrong hour.
        assert_eq!(
            parse_ms("2026-06-27T02:08:43.945+02:00"),
            Some(1_782_518_923_945)
        );
        assert_eq!(parse_ms("not a timestamp"), None);
        assert_eq!(parse_ms(""), None);
    }

    #[test]
    fn an_assistant_row_yields_its_usage_and_tool_call() {
        let row = row(
            r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","requestId":"req_1",
                "timestamp":"2026-06-27T00:08:43.945Z","sessionId":"s1","cwd":"D:\\Work",
                "gitBranch":"main","version":"2.1.187","effort":"high",
                "message":{"model":"claude-opus-5","usage":{"input_tokens":7,"output_tokens":11,
                  "cache_read_input_tokens":13,"cache_creation_input_tokens":17,
                  "service_tier":"standard"},
                 "content":[{"type":"thinking","thinking":"ignored"},
                            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"a":1}}]}}"#,
        );
        assert_eq!(row.kind(), Kind::Assistant);
        assert_eq!(row.session_id.as_deref(), Some("s1"));
        assert_eq!(row.git_branch.as_deref(), Some("main"));
        let usage = row.message.as_ref().unwrap().usage.as_ref().unwrap();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.cache_creation_input_tokens, 17);
        assert_eq!(usage.service_tier.as_deref(), Some("standard"));
        assert_eq!(
            row.tool_uses().collect::<Vec<_>>(),
            vec![("toolu_1", "Read")]
        );
    }

    #[test]
    fn a_tool_result_row_is_distinguished_by_its_payload_not_its_type() {
        let row = row(
            r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
                "timestamp":"2026-06-27T00:08:46.362Z",
                "toolUseResult":{"stdout":"lots of output","structuredPatch":[1,2,3]},
                "message":{"role":"user","content":[
                  {"type":"tool_result","tool_use_id":"toolu_1","content":"…","is_error":true}]}}"#,
        );
        assert_eq!(row.kind(), Kind::ToolResult);
        assert_eq!(row.tool_result(), Some((Some("toolu_1"), true)));
    }

    /// The payload takes every JSON shape there is, and a rejected row is a lost measurement.
    #[test]
    fn a_tool_result_payload_is_accepted_whatever_shape_it_takes() {
        for payload in ["{\"a\":1}", "\"plain string\"", "[1,2,3]", "null", "42"] {
            let text = format!(r#"{{"type":"user","uuid":"r","toolUseResult":{payload}}}"#);
            let row: Row = serde_json::from_str(&text).expect(payload);
            // A null payload is indistinguishable from an absent one, and a prompt is the safe
            // reading: it contributes no tool latency.
            let expected = if payload == "null" {
                Kind::Prompt
            } else {
                Kind::ToolResult
            };
            assert_eq!(row.kind(), expected, "{payload}");
        }
    }

    #[test]
    fn prompt_rows_are_told_apart_from_injected_context() {
        let typed = row(r#"{"type":"user","uuid":"u1","promptSource":"typed",
                "message":{"role":"user","content":"do the thing"}}"#);
        assert_eq!(typed.kind(), Kind::Prompt);

        let meta = row(r#"{"type":"user","uuid":"u2","isMeta":true,"message":{"content":"ctx"}}"#);
        assert_eq!(meta.kind(), Kind::Other);
    }

    #[test]
    fn a_string_content_carries_no_blocks_and_does_not_fail() {
        let row = row(r#"{"type":"user","uuid":"u1","message":{"content":"just words"}}"#);
        assert!(row.message.unwrap().content.is_empty());
    }

    #[test]
    fn unknown_row_types_and_unknown_fields_are_tolerated() {
        let row = row(r#"{"type":"future-thing","brandNewField":{"nested":[1]},"uuid":"x"}"#);
        assert_eq!(row.kind(), Kind::Other);
        assert_eq!(row.uuid.as_deref(), Some("x"));
    }

    #[test]
    fn parallel_tool_calls_in_one_row_are_all_reported() {
        let row = row(r#"{"type":"assistant","uuid":"a1","message":{"content":[
                 {"type":"tool_use","id":"t1","name":"Read"},
                 {"type":"tool_use","id":"t2","name":"Grep"}]}}"#);
        assert_eq!(
            row.tool_uses().collect::<Vec<_>>(),
            vec![("t1", "Read"), ("t2", "Grep")]
        );
    }
}
