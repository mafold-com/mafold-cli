//! Optional metadata from this Codex thread's own rollout. Read only a bounded
//! tail, filter to this invocation's timestamp, and never export prompt text.
//! stdout remains authoritative for turn usage; a rollout is optional enrichment.
use mafold_transcript::RunStats;
use serde_json::Value;
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(super) struct ItemStats(HashMap<String, Option<bool>>);
impl ItemStats {
    pub fn observe(&mut self, phase: &str, item: &Value) {
        if !matches!(
            item["type"].as_str(),
            Some(
                "command_execution"
                    | "file_change"
                    | "mcp_tool_call"
                    | "collab_tool_call"
                    | "web_search"
            )
        ) {
            return;
        }
        let Some(id) = item["id"].as_str().filter(|s| !s.is_empty()) else {
            return;
        };
        let status = if phase == "item.completed" {
            match item["status"].as_str() {
                Some("failed" | "declined") => Some(true),
                Some("completed") => Some(item["exit_code"].as_i64().is_some_and(|n| n != 0)),
                _ => item["exit_code"].as_i64().map(|n| n != 0),
            }
        } else {
            None
        };
        self.0
            .entry(id.to_string())
            .and_modify(|old| {
                if status.is_some() {
                    *old = status;
                }
            })
            .or_insert(status);
    }
    pub fn snapshot(&self) -> RunStats {
        RunStats {
            tool_calls: Some(self.0.len() as u64),
            tool_errors: self
                .0
                .values()
                .all(Option::is_some)
                .then(|| self.0.values().filter(|s| **s == Some(true)).count() as u64),
            ..Default::default()
        }
    }
}

pub(super) fn metadata(home: &Path, thread: &str, started_ms: u64) -> RunStats {
    // Also prevents path/pattern injection from an untrusted thread.started.
    if uuid::Uuid::parse_str(thread).is_err() {
        return RunStats::default();
    }
    let Some(path) = find_rollout(&home.join("sessions"), thread, 4) else {
        return RunStats::default();
    };
    read_metadata(&path, started_ms).unwrap_or_default()
}

fn find_rollout(dir: &Path, thread: &str, depth: u8) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let kind = entry.file_type().ok()?;
        if kind.is_file()
            && entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.ends_with(&format!("-{thread}.jsonl")))
        {
            return Some(entry.path());
        }
        if kind.is_dir() {
            if let Some(found) = find_rollout(&entry.path(), thread, depth - 1) {
                return Some(found);
            }
        }
    }
    None
}

fn read_metadata(path: &Path, started_ms: u64) -> std::io::Result<RunStats> {
    let mut file = File::open(path)?;
    let mut first = String::new();
    // Metadata lives at the head even when this is a years-old resumed thread.
    BufReader::new((&file).take(1024 * 1024)).read_line(&mut first)?;
    let mut stats = RunStats::default();
    if let Ok(v) = serde_json::from_str::<Value>(&first) {
        if v["type"] == "session_meta" {
            stats.provider = v["payload"]["model_provider"].as_str().map(str::to_string);
        }
    }
    let start = file.metadata()?.len().saturating_sub(8 * 1024 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file.take(8 * 1024 * 1024));
    if start > 0 {
        // The bounded tail can begin in the middle of a UTF-8 character.
        let mut partial = Vec::new();
        reader.read_until(b'\n', &mut partial)?;
    }
    for line in reader.lines().map_while(Result::ok) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        fold_metadata(&mut stats, &v, started_ms);
    }
    Ok(stats)
}

fn fold_metadata(stats: &mut RunStats, v: &Value, started_ms: u64) {
    let at = v["timestamp"]
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.timestamp_millis());
    if !at.is_some_and(|at| at >= 0 && at as u64 >= started_ms) {
        return;
    }
    let p = &v["payload"];
    if v["type"] == "turn_context" {
        stats.model = p["model"].as_str().map(str::to_string);
        stats.effort = p["effort"].as_str().map(str::to_string);
    }
    if v["type"] == "event_msg" && p["type"] == "token_count" {
        let info = &p["info"];
        if let Some(n) = info["last_token_usage"]["input_tokens"].as_u64() {
            stats.context_used_tokens = Some(n);
            stats.context_limit_tokens = info["model_context_window"].as_u64();
            stats.context_basis = Some("last_request_input".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn a_bounded_tail_can_start_inside_a_utf8_character() {
        let head = "{\"type\":\"session_meta\",\"payload\":{\"model_provider\":\"test\"}}\n";
        let body = "中\n{\"type\":\"turn_context\",\"timestamp\":\"2026-09-05T00:00:01Z\",\"payload\":{\"model\":\"fixture\"}}\n";
        let mut bytes = format!("{head}{body}").into_bytes();
        // The last 8 MiB start at the second byte of 中.
        bytes.resize(head.len() + 1 + 8 * 1024 * 1024, b' ');
        let path =
            std::env::temp_dir().join(format!("mafold-rollout-{}.jsonl", uuid::Uuid::new_v4()));
        std::fs::write(&path, bytes).unwrap();
        let result = read_metadata(&path, 0);
        std::fs::remove_file(path).unwrap();
        let stats = result.unwrap();
        assert_eq!(stats.provider.as_deref(), Some("test"));
        assert_eq!(stats.model.as_deref(), Some("fixture"));
    }
    #[test]
    fn resumed_threads_ignore_older_measurements_and_never_use_cumulative_context() {
        let mut s = RunStats::default();
        let v = json!({"type":"event_msg","timestamp":"2026-09-05T00:00:01Z","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":900000},"last_token_usage":{"input_tokens":12000},"model_context_window":128000}}});
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-05T00:00:01Z")
            .unwrap()
            .timestamp_millis() as u64;
        fold_metadata(&mut s, &v, at + 1);
        assert_eq!(s.context_used_tokens, None);
        fold_metadata(&mut s, &v, at);
        assert_eq!(s.context_used_tokens, Some(12000));
        assert_eq!(s.context_limit_tokens, Some(128000));
        assert_eq!(s.input_tokens, None);
    }
    #[test]
    fn tool_items_deduplicate_and_file_changes_count_one_invocation() {
        let mut s = ItemStats::default();
        let v = json!({"id":"p1","type":"file_change","status":"completed","changes":[{},{}]});
        s.observe("item.started", &v);
        s.observe("item.completed", &v);
        s.observe("item.completed", &v);
        assert_eq!(s.snapshot().tool_calls, Some(1));
        assert_eq!(s.snapshot().tool_errors, Some(0));
        s.observe(
            "item.completed",
            &json!({"id":"cmd","type":"command_execution","exit_code":2}),
        );
        assert_eq!(s.snapshot().tool_errors, Some(1));
        s.observe(
            "item.started",
            &json!({"id":"unknown","type":"mcp_tool_call"}),
        );
        assert_eq!(s.snapshot().tool_errors, None);
    }
}
