//! Turning a stream of transcript rows into measurements.
//!
//! Transcripts record no durations. Every number here is an interval between two rows that have to be
//! matched to each other first, which is what makes this a state machine rather than a mapping.
//!
//! The state is deliberately per-pass and thrown away afterwards. An importer that carried state
//! between passes would behave differently on a long-running daemon than on a fresh one, and
//! restarting it would silently change what got measured. Instead the deriver reports, via
//! [`Deriver::resume_offset`], the earliest row it is still waiting on, and the next pass starts
//! there — so the state is always rebuilt from the file itself, reading only the rows that matter.

use crate::watch::{
    collect::sessions::row::{Kind, Row},
    store::{Record, ToolCall, ToolVersion, Turn},
};
use std::collections::{HashMap, HashSet, VecDeque};

/// Name recorded for Claude Code in `tool_versions`.
pub const CLAUDE_CODE: &str = "claude-code";

/// Unanswered tool calls remembered at once.
///
/// A call is normally answered by the very next row. This bound exists for the pathological
/// transcript — an agent that fired calls it never awaited — not for the ordinary one.
const MAX_PENDING_TOOLS: usize = 512;

/// Requests remembered per pass, so the same turn is not derived twice.
///
/// Bounded for the same reason as [`MAX_PENDING_TOOLS`], and generously: a pass re-reads at most a
/// megabyte of tail, so an ordinary transcript never approaches this. Overflow clears the whole set rather
/// than evicting the oldest, which is affordable because the set is an optimisation and not the guarantee —
/// `(machine_id, request_id)` is unique in the database and the insert ignores conflicts, so the worst a
/// forgotten request can cost is one redundant insert.
const MAX_SEEN_REQUESTS: usize = 4_096;

/// Rows accepted between a prompt and the answer that measures it.
///
/// A prompt is usually followed by attachments before the first assistant row, so the chain has to
/// tolerate intermediate rows; it does not have to tolerate many.
const MAX_PROMPT_CHAIN: usize = 32;

/// Longest interval accepted as a measurement.
///
/// Beyond this the two rows are not plausibly cause and effect: a tool left pending while the machine
/// slept, or a timestamp written by a clock that has since been corrected.
const MAX_INTERVAL_MS: i64 = 6 * 60 * 60 * 1000;

/// A tool call awaiting its result.
#[derive(Debug, Clone)]
struct PendingTool {
    ts: i64,
    name: String,
    /// Byte offset of the row that asked, so a later pass can find it again.
    offset: i64,
}

/// A prompt awaiting the first assistant message that answers it.
#[derive(Debug, Clone)]
struct PendingPrompt {
    ts: i64,
    /// The prompt row, plus any rows descended from it that are not themselves answers.
    chain: HashSet<String>,
    offset: i64,
}

/// Derives measurements from one transcript's rows, in file order.
#[derive(Debug, Default)]
pub struct Deriver {
    /// Keyed by tool-use id, which is what a result row cites.
    tools: HashMap<String, PendingTool>,
    /// Keyed by the assistant row's own identity, for a result row that cites no tool-use id.
    tools_by_row: HashMap<String, PendingTool>,
    /// Insertion order, so the bound evicts the oldest rather than an arbitrary entry.
    order: VecDeque<String>,
    prompt: Option<PendingPrompt>,
    /// Requests already turned into a turn during this pass, capped at [`MAX_SEEN_REQUESTS`].
    seen_requests: HashSet<String>,
    version: Option<String>,
}

impl Deriver {
    /// Feed one row, appending whatever it completes.
    ///
    /// `offset` is where the row begins in the file, and is remembered for anything the row leaves
    /// unresolved.
    pub fn push(&mut self, offset: i64, row: &Row, out: &mut Vec<Record>) {
        self.note_version(row, out);
        match row.kind() {
            Kind::Assistant => self.assistant(offset, row, out),
            Kind::Prompt => self.begin_prompt(offset, row),
            Kind::ToolResult => self.tool_result(row, out),
            Kind::Other => self.extend_prompt_chain(row),
        }
    }

    /// The earliest row still awaiting its other half.
    ///
    /// This is what makes an incremental import exact rather than approximate. Stopping at the last
    /// byte read would lose every measurement whose two rows straddle the boundary; stopping some
    /// fixed distance earlier would re-read whole files to catch them. Stopping here re-reads the few
    /// rows that are genuinely still open, and nothing else.
    pub fn resume_offset(&self) -> Option<i64> {
        self.tools
            .values()
            .map(|pending| pending.offset)
            .chain(self.prompt.as_ref().map(|prompt| prompt.offset))
            .min()
    }

    /// Record Claude Code's version the first time this pass sees it, and whenever it changes.
    ///
    /// A transcript is the only artefact carrying it, so it is captured while the bytes are already
    /// being read rather than by a second pass over every file later.
    fn note_version(&mut self, row: &Row, out: &mut Vec<Record>) {
        let Some(version) = row.version.as_deref() else {
            return;
        };
        if self.version.as_deref() == Some(version) {
            return;
        }
        self.version = Some(version.to_string());
        if let Some(ts) = row.ts_ms() {
            out.push(
                ToolVersion {
                    ts,
                    tool: CLAUDE_CODE.to_string(),
                    version: version.to_string(),
                }
                .into(),
            );
        }
    }

    /// An assistant row: possibly a new turn, possibly the start of tool calls.
    fn assistant(&mut self, offset: i64, row: &Row, out: &mut Vec<Record>) {
        let Some(ts) = row.ts_ms() else { return };
        for (id, name) in row.tool_uses() {
            let pending = PendingTool {
                ts,
                name: name.to_string(),
                offset,
            };
            if let Some(uuid) = row.uuid.as_deref() {
                self.tools_by_row.insert(uuid.to_string(), pending.clone());
            }
            self.remember_tool(id.to_string(), pending);
        }
        if let Some(turn) = self.turn(row, ts) {
            out.push(turn.into());
        }
    }

    /// Build the turn for a request, or nothing if this row does not open one.
    ///
    /// Only the first row of a request produces a turn. Every row of that request repeats the same
    /// *cumulative* usage, so summing them would multiply a session's token counts by three or four.
    fn turn(&mut self, row: &Row, ts: i64) -> Option<Turn> {
        let request_id = row.request_id.as_deref()?;
        // An error response was never a request that did work, and a synthetic message never left
        // the machine.
        if row.is_api_error_message {
            return None;
        }
        let message = row.message.as_ref()?;
        if message
            .model
            .as_deref()
            .is_none_or(|model| model == "<synthetic>")
        {
            return None;
        }
        if self.seen_requests.len() >= MAX_SEEN_REQUESTS {
            self.seen_requests.clear();
        }
        if !self.seen_requests.insert(request_id.to_string()) {
            return None;
        }
        let usage = message.usage.as_ref();
        Some(Turn {
            uuid: row.uuid.clone().unwrap_or_else(|| request_id.to_string()),
            request_id: request_id.to_string(),
            session_id: row.session_id.clone().unwrap_or_default(),
            ts,
            project: row.cwd.clone(),
            branch: row.git_branch.clone(),
            model: message.model.clone(),
            effort: row.effort.clone(),
            service_tier: usage.and_then(|usage| usage.service_tier.clone()),
            first_response_ms: self.claim_prompt(row, ts),
            input_tokens: usage.map_or(0, |usage| usage.input_tokens),
            output_tokens: usage.map_or(0, |usage| usage.output_tokens),
            cache_read: usage.map_or(0, |usage| usage.cache_read_input_tokens),
            cache_create: usage.map_or(0, |usage| usage.cache_creation_input_tokens),
        })
    }

    /// The interval from the pending prompt to this answer, consuming the prompt.
    ///
    /// This is *not* a time to first token. The row is written once the whole first assistant message
    /// exists, which for a thinking model includes the entire thinking block, and a prompt typed while
    /// the agent was still working waits in a queue before the request is even sent.
    fn claim_prompt(&mut self, row: &Row, ts: i64) -> Option<i64> {
        let parent = row.parent_uuid.as_deref()?;
        let prompt = self.prompt.as_ref()?;
        if !prompt.chain.contains(parent) {
            return None;
        }
        let elapsed = ts - prompt.ts;
        self.prompt = None;
        (0..=MAX_INTERVAL_MS).contains(&elapsed).then_some(elapsed)
    }

    fn begin_prompt(&mut self, offset: i64, row: &Row) {
        let (Some(uuid), Some(ts)) = (row.uuid.as_deref(), row.ts_ms()) else {
            return;
        };
        self.prompt = Some(PendingPrompt {
            ts,
            chain: HashSet::from([uuid.to_string()]),
            offset,
        });
    }

    /// Follow a prompt through the rows that separate it from its answer.
    ///
    /// Attachments are the common case and they are the majority: most first responses descend from an
    /// attachment row rather than from the prompt itself, so a chain that stopped at the prompt would
    /// measure almost nothing.
    fn extend_prompt_chain(&mut self, row: &Row) {
        let Some(prompt) = self.prompt.as_mut() else {
            return;
        };
        let (Some(uuid), Some(parent)) = (row.uuid.as_deref(), row.parent_uuid.as_deref()) else {
            return;
        };
        if prompt.chain.contains(parent) && prompt.chain.len() < MAX_PROMPT_CHAIN {
            prompt.chain.insert(uuid.to_string());
        }
    }

    /// A result row closes the tool call it cites.
    fn tool_result(&mut self, row: &Row, out: &mut Vec<Record>) {
        let (Some(uuid), Some(ts)) = (row.uuid.as_deref(), row.ts_ms()) else {
            return;
        };
        let (tool_use_id, failed) = row.tool_result().unwrap_or((None, false));
        let Some(pending) = self.take_tool(tool_use_id, row.source_tool_assistant_uuid.as_deref())
        else {
            return;
        };
        let duration_ms = ts - pending.ts;
        if !(0..=MAX_INTERVAL_MS).contains(&duration_ms) {
            return;
        }
        out.push(
            ToolCall {
                uuid: uuid.to_string(),
                ts: pending.ts,
                project: row.cwd.clone(),
                tool: pending.name,
                duration_ms,
                // A refused call spent its time waiting for a person, and a failed one returned
                // early. Either would corrupt a latency series, so the flag is what charts filter on.
                ok: !failed && row.tool_denial_kind.is_none(),
            }
            .into(),
        );
    }

    fn remember_tool(&mut self, id: String, pending: PendingTool) {
        if self.tools.insert(id.clone(), pending).is_none() {
            self.order.push_back(id);
        }
        while self.order.len() > MAX_PENDING_TOOLS {
            if let Some(oldest) = self.order.pop_front() {
                self.tools.remove(&oldest);
            }
        }
        // The fallback index is bounded by the same budget; it is only consulted for result rows that
        // cite no tool-use id at all.
        if self.tools_by_row.len() > MAX_PENDING_TOOLS {
            self.tools_by_row.clear();
        }
    }

    /// Resolve a result row to the call it answers.
    ///
    /// The tool-use id is the precise link, and the only one that stays correct when a single
    /// assistant row requests several tools at once. The assistant row's identity is the fallback.
    fn take_tool(
        &mut self,
        tool_use_id: Option<&str>,
        assistant: Option<&str>,
    ) -> Option<PendingTool> {
        if let Some(id) = tool_use_id
            && let Some(pending) = self.tools.remove(id)
        {
            return Some(pending);
        }
        self.tools_by_row.remove(assistant?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watch::store::Record;

    /// Feed raw JSON lines and collect what they derive.
    fn derive(lines: &[&str]) -> Vec<Record> {
        let (records, _) = derive_with_offsets(lines);
        records
    }

    /// As [`derive`], also reporting where a later pass would have to resume.
    fn derive_with_offsets(lines: &[&str]) -> (Vec<Record>, Option<i64>) {
        let mut deriver = Deriver::default();
        let mut out = Vec::new();
        let mut offset = 0;
        for line in lines {
            let row: Row = serde_json::from_str(line).expect(line);
            deriver.push(offset, &row, &mut out);
            offset += line.len() as i64 + 1;
        }
        (out, deriver.resume_offset())
    }

    fn turns(records: &[Record]) -> Vec<Turn> {
        records
            .iter()
            .filter_map(|record| match record {
                Record::Turn(turn) => Some(turn.clone()),
                _ => None,
            })
            .collect()
    }

    fn calls(records: &[Record]) -> Vec<ToolCall> {
        records
            .iter()
            .filter_map(|record| match record {
                Record::ToolCall(call) => Some(call.clone()),
                _ => None,
            })
            .collect()
    }

    fn assistant(uuid: &str, parent: &str, request: &str, ts: &str, extra: &str) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","parentUuid":"{parent}",
                 "requestId":"{request}","timestamp":"{ts}","sessionId":"s1","cwd":"D:\\Work",
                 "gitBranch":"main","version":"2.1.187","effort":"high",
                 "message":{{"model":"claude-opus-5","usage":{{"input_tokens":2,"output_tokens":378,
                   "cache_read_input_tokens":26850,"cache_creation_input_tokens":7,
                   "service_tier":"standard"}},"content":[{extra}]}}}}"#
        )
    }

    /// The trap that multiplies every token count in the database if it is missed.
    #[test]
    fn one_request_that_emits_several_rows_becomes_one_turn() {
        let first = assistant("a1", "p1", "req_1", "2026-06-27T00:00:10.000Z", "");
        let second = assistant("a2", "a1", "req_1", "2026-06-27T00:00:11.000Z", "");
        let third = assistant("a3", "a2", "req_1", "2026-06-27T00:00:12.000Z", "");
        let records = derive(&[&first, &second, &third]);
        let turns = turns(&records);
        assert_eq!(turns.len(), 1, "one request must yield one turn");
        assert_eq!(turns[0].uuid, "a1", "the first row identifies the turn");
        assert_eq!(
            turns[0].output_tokens, 378,
            "usage is cumulative, not additive"
        );
        assert_eq!(turns[0].cache_read, 26_850);
        assert_eq!(turns[0].model.as_deref(), Some("claude-opus-5"));
        assert_eq!(turns[0].service_tier.as_deref(), Some("standard"));
        assert_eq!(turns[0].branch.as_deref(), Some("main"));
    }

    /// The dedupe set is a cache, not the guarantee, so it is allowed to be bounded.
    ///
    /// Every other per-pass map here is capped; this one was not, which made a single enormous transcript
    /// the one input that could grow the deriver without limit. Forgetting a request costs at most a
    /// redundant insert, since `(machine_id, request_id)` is unique in the database.
    #[test]
    fn the_request_dedupe_set_is_bounded() {
        let mut deriver = Deriver::default();
        let mut out = Vec::new();
        for index in 0..=MAX_SEEN_REQUESTS {
            let line = assistant(
                &format!("a{index}"),
                "p1",
                &format!("req_{index}"),
                "2026-06-27T00:00:10.000Z",
                "",
            );
            let row: Row = serde_json::from_str(&line).expect(&line);
            deriver.push(0, &row, &mut out);
        }
        assert_eq!(
            turns(&out).len(),
            MAX_SEEN_REQUESTS + 1,
            "every distinct request is still a turn"
        );
        assert!(
            deriver.seen_requests.len() <= MAX_SEEN_REQUESTS,
            "the set grew to {}",
            deriver.seen_requests.len()
        );
    }

    #[test]
    fn a_tool_call_is_timed_from_its_request_to_its_result() {
        let request = assistant(
            "a1",
            "p1",
            "req_1",
            "2026-06-27T00:00:10.000Z",
            r#"{"type":"tool_use","id":"toolu_1","name":"Read"}"#,
        );
        let result = r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:10.011Z","cwd":"D:\\Work","toolUseResult":{"file":"x"},
            "message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1"}]}}"#;
        let calls = calls(&derive(&[&request, result]));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "Read");
        assert_eq!(calls[0].duration_ms, 11);
        assert_eq!(
            calls[0].uuid, "r1",
            "the result row gives the call its identity"
        );
        assert!(calls[0].ok);
        assert_eq!(calls[0].project.as_deref(), Some("D:\\Work"));
    }

    #[test]
    fn parallel_calls_in_one_row_are_matched_by_tool_use_id_not_by_row() {
        let request = assistant(
            "a1",
            "p1",
            "req_1",
            "2026-06-27T00:00:10.000Z",
            r#"{"type":"tool_use","id":"t1","name":"Read"},
               {"type":"tool_use","id":"t2","name":"Grep"}"#,
        );
        let first = r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:10.020Z","toolUseResult":{},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t2"}]}}"#;
        let second = r#"{"type":"user","uuid":"r2","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:10.050Z","toolUseResult":{},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
        let calls = calls(&derive(&[&request, first, second]));
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool, "Grep");
        assert_eq!(calls[0].duration_ms, 20);
        assert_eq!(calls[1].tool, "Read");
        assert_eq!(calls[1].duration_ms, 50);
    }

    #[test]
    fn a_result_citing_no_tool_use_id_still_resolves_through_its_assistant_row() {
        let request = assistant(
            "a1",
            "p1",
            "req_1",
            "2026-06-27T00:00:10.000Z",
            r#"{"type":"tool_use","id":"t1","name":"Glob"}"#,
        );
        let result = r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:10.400Z","toolUseResult":"plain text result"}"#;
        let calls = calls(&derive(&[&request, result]));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "Glob");
        assert_eq!(calls[0].duration_ms, 400);
    }

    #[test]
    fn failed_and_refused_calls_are_recorded_but_marked_not_ok() {
        let request = assistant(
            "a1",
            "p1",
            "req_1",
            "2026-06-27T00:00:10.000Z",
            r#"{"type":"tool_use","id":"t1","name":"Bash"},
               {"type":"tool_use","id":"t2","name":"Edit"}"#,
        );
        let refused = r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:01:10.000Z","toolDenialKind":"permission-rule",
            "toolUseResult":{},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true}]}}"#;
        let failed = r#"{"type":"user","uuid":"r2","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:10.002Z","toolUseResult":{},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true}]}}"#;
        let calls = calls(&derive(&[&request, refused, failed]));
        assert_eq!(calls.len(), 2, "both are kept: they are still events");
        assert!(calls.iter().all(|call| !call.ok));
    }

    /// Most first responses descend from an attachment row, not from the prompt itself.
    #[test]
    fn the_first_response_is_measured_through_an_attachment_row() {
        let prompt = r#"{"type":"user","uuid":"p1","promptSource":"typed",
            "timestamp":"2026-06-27T00:00:00.000Z","message":{"content":"do the thing"}}"#;
        let attachment = r#"{"type":"attachment","uuid":"at1","parentUuid":"p1",
            "timestamp":"2026-06-27T00:00:00.100Z","attachment":{"kind":"whatever"}}"#;
        let answer = assistant("a1", "at1", "req_1", "2026-06-27T00:00:14.500Z", "");
        let turns = turns(&derive(&[prompt, attachment, &answer]));
        assert_eq!(turns[0].first_response_ms, Some(14_500));
    }

    #[test]
    fn a_continuation_driven_by_a_tool_result_has_no_first_response() {
        let prompt = r#"{"type":"user","uuid":"p1","timestamp":"2026-06-27T00:00:00.000Z",
            "message":{"content":"go"}}"#;
        let first = assistant("a1", "p1", "req_1", "2026-06-27T00:00:05.000Z", "");
        let result = r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:06.000Z","toolUseResult":{}}"#;
        let second = assistant("a2", "r1", "req_2", "2026-06-27T00:00:09.000Z", "");
        let turns = turns(&derive(&[prompt, &first, result, &second]));
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].first_response_ms, Some(5_000));
        assert_eq!(
            turns[1].first_response_ms, None,
            "a continuation has no prompt to measure from"
        );
    }

    #[test]
    fn injected_context_does_not_start_a_response_interval() {
        let meta = r#"{"type":"user","uuid":"m1","isMeta":true,
            "timestamp":"2026-06-27T00:00:00.000Z","message":{"content":"context"}}"#;
        let answer = assistant("a1", "m1", "req_1", "2026-06-27T00:00:04.000Z", "");
        let turns = turns(&derive(&[meta, &answer]));
        assert_eq!(turns[0].first_response_ms, None);
    }

    #[test]
    fn error_and_synthetic_responses_are_not_turns() {
        let api_error = r#"{"type":"assistant","uuid":"a1","requestId":"req_1",
            "isApiErrorMessage":true,"timestamp":"2026-06-27T00:00:10.000Z",
            "message":{"model":"claude-opus-5","usage":{"input_tokens":1}}}"#;
        let synthetic = r#"{"type":"assistant","uuid":"a2","requestId":"req_2",
            "timestamp":"2026-06-27T00:00:11.000Z","message":{"model":"<synthetic>"}}"#;
        let no_request = r#"{"type":"assistant","uuid":"a3",
            "timestamp":"2026-06-27T00:00:12.000Z","message":{"model":"claude-opus-5"}}"#;
        assert!(turns(&derive(&[api_error, synthetic, no_request])).is_empty());
    }

    #[test]
    fn implausible_intervals_are_dropped_rather_than_charted() {
        let request = assistant(
            "a1",
            "p1",
            "req_1",
            "2026-06-27T00:00:10.000Z",
            r#"{"type":"tool_use","id":"t1","name":"Read"}"#,
        );
        // Answered a day later, and answered before it was asked.
        let late = r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-28T00:00:10.000Z","toolUseResult":{},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
        assert!(calls(&derive(&[&request, late])).is_empty());

        let backwards = r#"{"type":"user","uuid":"r2","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:09.000Z","toolUseResult":{},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;
        assert!(calls(&derive(&[&request, backwards])).is_empty());
    }

    #[test]
    fn the_claude_code_version_is_recorded_once_and_again_on_change() {
        let first = assistant("a1", "p1", "req_1", "2026-06-27T00:00:10.000Z", "");
        let same = assistant("a2", "a1", "req_2", "2026-06-27T00:00:11.000Z", "");
        let upgraded = same.replace("2.1.187", "2.2.0");
        let versions: Vec<ToolVersion> = derive(&[&first, &same, &upgraded])
            .into_iter()
            .filter_map(|record| match record {
                Record::ToolVersion(version) => Some(version),
                _ => None,
            })
            .collect();
        assert_eq!(versions.len(), 2, "{versions:?}");
        assert_eq!(versions[0].version, "2.1.187");
        assert_eq!(versions[0].tool, CLAUDE_CODE);
        assert_eq!(versions[1].version, "2.2.0");
    }

    /// Where the next pass has to start, and therefore how much gets re-read.
    #[test]
    fn the_resume_offset_is_the_oldest_row_still_awaiting_its_other_half() {
        let request = assistant(
            "a1",
            "p1",
            "req_1",
            "2026-06-27T00:00:10.000Z",
            r#"{"type":"tool_use","id":"t1","name":"Read"}"#,
        );
        let result = r#"{"type":"user","uuid":"r1","sourceToolAssistantUUID":"a1",
            "timestamp":"2026-06-27T00:00:10.011Z","toolUseResult":{},
            "message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}"#;

        // A call awaiting its result holds the offset at the row that asked.
        let (_, pending) = derive_with_offsets(&[&request]);
        assert_eq!(pending, Some(0));

        // Once answered, nothing is outstanding and the next pass can start after the last row.
        let (_, settled) = derive_with_offsets(&[&request, result]);
        assert_eq!(settled, None);

        // An unanswered prompt is outstanding too: its answer is still to come.
        let prompt = r#"{"type":"user","uuid":"p9","timestamp":"2026-06-27T00:00:00.000Z",
            "message":{"content":"go"}}"#;
        let (_, waiting) = derive_with_offsets(&[&request, result, prompt]);
        assert_eq!(
            waiting,
            Some(request.len() as i64 + result.len() as i64 + 2)
        );
    }

    #[test]
    fn unanswered_tool_calls_cannot_grow_without_bound() {
        let mut deriver = Deriver::default();
        let mut out = Vec::new();
        for index in 0..(MAX_PENDING_TOOLS * 2) {
            let line = assistant(
                &format!("a{index}"),
                "p1",
                &format!("req_{index}"),
                "2026-06-27T00:00:10.000Z",
                &format!(r#"{{"type":"tool_use","id":"t{index}","name":"Read"}}"#),
            );
            let row: Row = serde_json::from_str(&line).expect("row");
            deriver.push(index as i64, &row, &mut out);
        }
        assert!(deriver.tools.len() <= MAX_PENDING_TOOLS);
        assert!(deriver.tools_by_row.len() <= MAX_PENDING_TOOLS + 1);
    }

    #[test]
    fn a_blank_or_unparseable_row_is_simply_not_a_measurement() {
        assert!(derive(&[r#"{"type":"assistant","uuid":"a1"}"#]).is_empty());
        assert!(derive(&[r#"{"type":"user","uuid":"u1","toolUseResult":{}}"#]).is_empty());
    }
}
