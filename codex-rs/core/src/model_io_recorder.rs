use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::warn;

#[derive(Clone, Debug)]
struct ToolCallOrigin {
    request_id: u64,
    seq: u64,
    tool_name: String,
    observed_at_unix_ms: u128,
}

#[derive(Clone, Debug)]
struct RequestPhaseEntry {
    phase: &'static str,
    payload: Value,
    observed_at_unix_ms: u128,
    seq: Option<u64>,
}

#[derive(Debug)]
struct ModelIoWriteEntry {
    markdown: String,
    ai_jsonl: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ModelIoRecorder {
    tx: mpsc::Sender<ModelIoWriteEntry>,
    path: Arc<PathBuf>,
    ai_path: Arc<PathBuf>,
    thread_id: Arc<String>,
    next_request_id: Arc<AtomicU64>,
    next_event_id: Arc<AtomicU64>,
    tool_call_origins: Arc<Mutex<std::collections::HashMap<String, ToolCallOrigin>>>,
    request_phase_entries: Arc<Mutex<std::collections::HashMap<u64, Vec<RequestPhaseEntry>>>>,
}

impl ModelIoRecorder {
    pub(crate) fn new(dir: PathBuf, conversation_id: &ThreadId) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;

        let thread_id = sanitize_component(&conversation_id.to_string());
        let file_name = format!("model-io-{thread_id}.md");
        let path = dir.join(file_name);
        let ai_file_name = format!("model-io-{thread_id}.ai.jsonl");
        let ai_dir = dir.join("ai-flow");
        std::fs::create_dir_all(&ai_dir)?;
        let ai_path = ai_dir.join(ai_file_name);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let mut ai_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ai_path)?;

        let started_at_unix_ms = now_unix_ms();

        // Make it obvious that debug is enabled even if we exit before receiving a `Completed`
        // event (or the async runtime shuts down before buffered writes flush).
        if file.metadata().map(|meta| meta.len()).unwrap_or(0) == 0 {
            use std::io::Write;
            let header = format!(
                "# Model I/O Debug Trace (Strict Timeline)\n\n- thread_id: `{conversation_id}`\n- started_at_unix_ms: {started_at_unix_ms}\n- format: strict_timeline_v1\n\n## Timeline\n\n"
            );
            let _ = file.write_all(header.as_bytes());
            let _ = file.flush();
        }
        if ai_file.metadata().map(|meta| meta.len()).unwrap_or(0) == 0 {
            use std::io::Write;
            let session_start = serde_json::json!({
                "schema": "model_io_ai_flow_v1",
                "record_type": "session_start",
                "thread_id": conversation_id,
                "started_at_unix_ms": started_at_unix_ms,
            });
            if let Ok(json_line) = serde_json::to_string(&session_start) {
                let _ = writeln!(ai_file, "{json_line}");
                let _ = ai_file.flush();
            }
        }

        let file = tokio::fs::File::from_std(file);
        let ai_file = tokio::fs::File::from_std(ai_file);
        // Large buffer because strict timeline can log many delta events.
        let (tx, mut rx) = mpsc::channel::<ModelIoWriteEntry>(32_768);
        let path_for_task = path.clone();
        let ai_path_for_task = ai_path.clone();

        tokio::spawn(async move {
            let mut file = file;
            let mut ai_file = ai_file;
            while let Some(entry) = rx.recv().await {
                if let Err(err) = file.write_all(entry.markdown.as_bytes()).await {
                    warn!(%err, path = %path_for_task.display(), "failed to write model io debug entry");
                    break;
                }
                if let Some(ai_jsonl) = entry.ai_jsonl
                    && let Err(err) = ai_file.write_all(ai_jsonl.as_bytes()).await
                {
                    warn!(%err, path = %ai_path_for_task.display(), "failed to write model io ai-flow entry");
                }
                // Flush every entry so Windows Explorer shows growth immediately and so we keep
                // useful data even if the runtime shuts down abruptly.
                if let Err(err) = file.flush().await {
                    warn!(%err, path = %path_for_task.display(), "failed to flush model io debug entry");
                    break;
                }
                if let Err(err) = ai_file.flush().await {
                    warn!(%err, path = %ai_path_for_task.display(), "failed to flush model io ai-flow entry");
                }
            }
            let _ = file.flush().await;
            let _ = ai_file.flush().await;
        });

        Ok(Self {
            tx,
            path: Arc::new(path),
            ai_path: Arc::new(ai_path),
            thread_id: Arc::new(conversation_id.to_string()),
            next_request_id: Arc::new(AtomicU64::new(1)),
            next_event_id: Arc::new(AtomicU64::new(1)),
            tool_call_origins: Arc::new(Mutex::new(std::collections::HashMap::new())),
            request_phase_entries: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    pub(crate) fn next_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) async fn record_request_start(&self, request_id: u64, request: &Value) {
        let system_prompt_payload = serde_json::json!({
            "system_prompt": request.get("instructions").cloned().unwrap_or(Value::Null),
        });
        self.push_request_phase_entry(request_id, None, "system_prompt", system_prompt_payload)
            .await;

        let tools_payload = serde_json::json!({
            "tools": request.get("tools").cloned().unwrap_or(Value::Null),
            "tool_choice": request.get("tool_choice").cloned().unwrap_or(Value::Null),
            "parallel_tool_calls": request.get("parallel_tool_calls").cloned().unwrap_or(Value::Null),
        });
        self.push_request_phase_entry(request_id, None, "tools", tools_payload)
            .await;

        self.record_timeline_json(request_id, Some(0), "request_start", request.clone())
            .await;
    }

    pub(crate) async fn record_request_end(&self, request_id: u64, seq: u64, outcome: &Value) {
        if let Some(request_payload) = self.take_request_phase_payload(request_id).await {
            self.record_timeline_json(request_id, Some(seq), "request", request_payload)
                .await;
        }
        self.record_timeline_json(request_id, Some(seq), "response", outcome.clone())
            .await;
        self.record_timeline_json(request_id, Some(seq), "request_end", outcome.clone())
            .await;
    }

    pub(crate) async fn record_request_input_item(
        &self,
        request_id: u64,
        seq: u64,
        index: usize,
        item: &ResponseItem,
    ) {
        let mut payload = serde_json::to_value(item).unwrap_or(Value::Null);
        if let Some(call_id) = tool_output_call_id(item)
            && let Some(obj) = payload.as_object_mut()
        {
            if let Some(origin) = self.lookup_tool_call_origin(call_id).await {
                obj.insert(
                    "correlates_to".to_string(),
                    serde_json::json!({
                        "call_id": call_id,
                        "tool_name": origin.tool_name,
                        "request_id": origin.request_id,
                        "seq": origin.seq,
                        "observed_at_unix_ms": origin.observed_at_unix_ms,
                    }),
                );
            } else {
                obj.insert(
                    "correlates_to".to_string(),
                    serde_json::json!({
                        "call_id": call_id,
                    }),
                );
            }
        }

        let wrapped = serde_json::json!({
            "index": index,
            "item": payload,
        });

        self.push_request_phase_entry(request_id, Some(seq), request_phase(item), wrapped.clone())
            .await;

        self.record_timeline_json(request_id, Some(seq), "request_input_item", wrapped)
            .await;
    }

    pub(crate) async fn record_response_event(&self, request_id: u64, seq: u64, event: Value) {
        self.record_timeline_json(request_id, Some(seq), "response_event", event)
            .await;
    }

    pub(crate) async fn record_transport_stage(
        &self,
        request_id: u64,
        seq: u64,
        transport: &'static str,
        stage: &'static str,
        details: Value,
    ) {
        self.record_timeline_json(
            request_id,
            Some(seq),
            "transport_stage",
            serde_json::json!({
                "transport": transport,
                "stage": stage,
                "details": details,
            }),
        )
        .await;
    }

    pub(crate) async fn register_tool_call_origin(
        &self,
        call_id: String,
        request_id: u64,
        seq: u64,
        tool_name: String,
    ) {
        let observed_at_unix_ms = now_unix_ms();
        let mut guard = self.tool_call_origins.lock().await;
        guard.insert(
            call_id,
            ToolCallOrigin {
                request_id,
                seq,
                tool_name,
                observed_at_unix_ms,
            },
        );
    }

    async fn lookup_tool_call_origin(&self, call_id: &str) -> Option<ToolCallOrigin> {
        let guard = self.tool_call_origins.lock().await;
        guard.get(call_id).cloned()
    }

    async fn push_request_phase_entry(
        &self,
        request_id: u64,
        seq: Option<u64>,
        phase: &'static str,
        payload: Value,
    ) {
        let mut guard = self.request_phase_entries.lock().await;
        guard
            .entry(request_id)
            .or_default()
            .push(RequestPhaseEntry {
                phase,
                payload,
                observed_at_unix_ms: now_unix_ms(),
                seq,
            });
    }

    async fn take_request_phase_payload(&self, request_id: u64) -> Option<Value> {
        let mut guard = self.request_phase_entries.lock().await;
        let mut entries = guard.remove(&request_id)?;
        entries.sort_by(|left, right| {
            (
                left.observed_at_unix_ms,
                left.seq.unwrap_or_default(),
                request_phase_rank(left.phase),
            )
                .cmp(&(
                    right.observed_at_unix_ms,
                    right.seq.unwrap_or_default(),
                    request_phase_rank(right.phase),
                ))
        });
        let phases = entries
            .into_iter()
            .map(|entry| {
                serde_json::json!({
                    "phase": entry.phase,
                    "seq": entry.seq,
                    "observed_at_unix_ms": entry.observed_at_unix_ms,
                    "payload": entry.payload,
                })
            })
            .collect::<Vec<_>>();
        Some(serde_json::json!({ "phases": phases }))
    }

    async fn record_timeline_json(
        &self,
        request_id: u64,
        seq: Option<u64>,
        kind: &str,
        payload: Value,
    ) {
        let event_id = self.next_event_id.fetch_add(1, Ordering::Relaxed);
        let ts_unix_ms = now_unix_ms();
        let Ok(payload_json) = serde_json::to_string_pretty(&payload) else {
            return;
        };
        let seq_label = seq
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".to_string());
        let error_keywords = matched_error_keywords(&payload);
        let call_id = first_string_field(&payload, "call_id");
        let tool_name = first_string_field(&payload, "tool_name")
            .or_else(|| first_string_field(&payload, "name"));
        let role = first_string_field(&payload, "role");
        let markdown = format!(
            "- event_id={event_id} t_unix_ms={ts_unix_ms} request_id={request_id} seq={seq_label} kind={kind}\n```json\n{payload_json}\n```\n\n"
        );
        let ai_event = serde_json::json!({
            "schema": AI_FLOW_SCHEMA,
            "record_type": "event",
            "thread_id": self.thread_id.as_ref(),
            "event_id": event_id,
            "t_unix_ms": ts_unix_ms,
            "request_id": request_id,
            "seq": seq,
            "kind": kind,
            "workflow_stage": workflow_stage(kind),
            "turn_boundary": turn_boundary(kind),
            "retrieval": {
                "has_error": !error_keywords.is_empty(),
                "error_keywords": error_keywords,
                "call_id": call_id,
                "tool_name": tool_name,
                "role": role,
            },
            "payload": payload,
        });
        let ai_jsonl = serde_json::to_string(&ai_event)
            .ok()
            .map(|json_line| format!("{json_line}\n"));

        if self
            .tx
            .send(ModelIoWriteEntry { markdown, ai_jsonl })
            .await
            .is_err()
        {
            warn!(
                path = %self.path.display(),
                ai_path = %self.ai_path.display(),
                "model io debug channel closed"
            );
        }
    }
}

const AI_FLOW_SCHEMA: &str = "model_io_ai_flow_v1";
const AI_ERROR_TERMS: [&str; 8] = [
    "error",
    "failed",
    "panic",
    "timeout",
    "exception",
    "denied",
    "refused",
    "rate limit",
];

fn now_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|dur| dur.as_millis())
        .unwrap_or_default()
}

fn workflow_stage(kind: &str) -> &'static str {
    match kind {
        "request_start" | "request" => "request",
        "request_input_item" => "input_item",
        "transport_stage" => "transport",
        "response_event" | "response" => "response",
        "request_end" => "turn_end",
        _ => "timeline",
    }
}

fn turn_boundary(kind: &str) -> Option<&'static str> {
    match kind {
        "request_start" => Some("start"),
        "request_end" => Some("end"),
        _ => None,
    }
}

fn matched_error_keywords(value: &Value) -> Vec<String> {
    let Ok(payload_json) = serde_json::to_string(value) else {
        return Vec::new();
    };
    let payload_json = payload_json.to_ascii_lowercase();
    AI_ERROR_TERMS
        .iter()
        .filter(|term| payload_json.contains(**term))
        .map(|term| (*term).to_string())
        .collect()
}

fn first_string_field(value: &Value, field_name: &str) -> Option<String> {
    match value {
        Value::Object(map) => {
            if let Some(raw_value) = map.get(field_name).and_then(Value::as_str) {
                return Some(raw_value.to_string());
            }
            map.values()
                .find_map(|entry| first_string_field(entry, field_name))
        }
        Value::Array(entries) => entries
            .iter()
            .find_map(|entry| first_string_field(entry, field_name)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => None,
    }
}

fn tool_output_call_id(item: &ResponseItem) -> Option<&str> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
        ResponseItem::CustomToolCallOutput { call_id, .. } => Some(call_id.as_str()),
        _ => None,
    }
}

fn request_phase(item: &ResponseItem) -> &'static str {
    match item {
        ResponseItem::Message { role, .. } if role == "user" || role == "assistant" => {
            "user_assistant"
        }
        ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::WebSearchCall { .. } => "function_call",
        ResponseItem::FunctionCallOutput { .. } | ResponseItem::CustomToolCallOutput { .. } => {
            "tool_result"
        }
        ResponseItem::Message { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::GhostSnapshot { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::Other => "user_assistant",
    }
}

fn request_phase_rank(phase: &str) -> u8 {
    match phase {
        "system_prompt" => 0,
        "tools" => 1,
        "user_assistant" => 2,
        "function_call" => 3,
        "tool_result" => 4,
        _ => 5,
    }
}

fn sanitize_component(raw: &str) -> String {
    raw.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => c,
            _ => '_',
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tokio::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn writes_strict_timeline_entries() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conversation_id = ThreadId::new();
        let recorder = ModelIoRecorder::new(dir.path().to_path_buf(), &conversation_id)?;
        let path = recorder.path.as_path().to_path_buf();

        recorder
            .record_request_start(3, &serde_json::json!({"model": "gpt-5.1"}))
            .await;
        drop(recorder);

        let contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&path).await
                    && contents.contains("request_id=3")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        assert!(contents.contains("# Model I/O Debug Trace (Strict Timeline)"));
        assert!(contents.contains("format: strict_timeline_v1"));
        assert!(contents.contains("kind=request_start"));
        assert!(contents.contains("\"model\": \"gpt-5.1\""));
        Ok(())
    }

    #[tokio::test]
    async fn writes_header_on_creation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conversation_id = ThreadId::new();
        let recorder = ModelIoRecorder::new(dir.path().to_path_buf(), &conversation_id)?;
        let path = recorder.path.as_path().to_path_buf();
        drop(recorder);

        let contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&path).await
                    && contents.contains("# Model I/O Debug Trace")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        assert!(contents.contains(&conversation_id.to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn writes_ai_flow_session_and_turn_boundaries() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conversation_id = ThreadId::new();
        let recorder = ModelIoRecorder::new(dir.path().to_path_buf(), &conversation_id)?;
        let ai_path = recorder.ai_path.as_path().to_path_buf();

        recorder
            .record_request_start(13, &serde_json::json!({"model": "gpt-5.1"}))
            .await;
        recorder
            .record_request_end(13, 1, &serde_json::json!({"type": "completed"}))
            .await;
        drop(recorder);

        let contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&ai_path).await
                    && contents.contains("\"kind\":\"request_end\"")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let records: Vec<Value> = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        let first_record = records.first().expect("session start record");
        assert_eq!(
            first_record
                .get("record_type")
                .and_then(Value::as_str)
                .expect("record_type"),
            "session_start"
        );
        assert_eq!(
            first_record
                .get("schema")
                .and_then(Value::as_str)
                .expect("schema"),
            AI_FLOW_SCHEMA
        );
        let request_start = records
            .iter()
            .find(|record| record.get("kind").and_then(Value::as_str) == Some("request_start"))
            .expect("request_start");
        assert_eq!(
            request_start
                .get("turn_boundary")
                .and_then(Value::as_str)
                .expect("request start boundary"),
            "start"
        );
        let request_end = records
            .iter()
            .find(|record| record.get("kind").and_then(Value::as_str) == Some("request_end"))
            .expect("request_end");
        assert_eq!(
            request_end
                .get("turn_boundary")
                .and_then(Value::as_str)
                .expect("request end boundary"),
            "end"
        );
        Ok(())
    }

    #[tokio::test]
    async fn writes_transport_stage_entries() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conversation_id = ThreadId::new();
        let recorder = ModelIoRecorder::new(dir.path().to_path_buf(), &conversation_id)?;
        let ai_path = recorder.ai_path.as_path().to_path_buf();

        recorder
            .record_transport_stage(
                21,
                7,
                "responses_http",
                "dispatch_start",
                serde_json::json!({"path": "api"}),
            )
            .await;
        drop(recorder);

        let contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&ai_path).await
                    && contents.contains("\"kind\":\"transport_stage\"")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        let record = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<Value>)
            .flatten()
            .find(|value| value.get("kind").and_then(Value::as_str) == Some("transport_stage"))
            .expect("transport stage record");

        assert_eq!(
            record
                .get("workflow_stage")
                .and_then(Value::as_str)
                .expect("workflow stage"),
            "transport"
        );
        assert_eq!(
            record
                .get("payload")
                .and_then(Value::as_object)
                .and_then(|payload| payload.get("stage"))
                .and_then(Value::as_str)
                .expect("stage"),
            "dispatch_start"
        );
        Ok(())
    }

    #[tokio::test]
    async fn keeps_strict_event_order_with_monotonic_event_ids() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conversation_id = ThreadId::new();
        let recorder = ModelIoRecorder::new(dir.path().to_path_buf(), &conversation_id)?;
        let path = recorder.path.as_path().to_path_buf();

        recorder
            .record_response_event(11, 1, serde_json::json!({"i": 1}))
            .await;
        recorder
            .record_response_event(11, 2, serde_json::json!({"i": 2}))
            .await;
        recorder
            .record_response_event(11, 3, serde_json::json!({"i": 3}))
            .await;
        drop(recorder);

        let contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&path).await
                    && contents.contains("event_id=3")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        let pos_1 = contents.find("event_id=1").expect("event 1");
        let pos_2 = contents.find("event_id=2").expect("event 2");
        let pos_3 = contents.find("event_id=3").expect("event 3");
        assert!(pos_1 < pos_2);
        assert!(pos_2 < pos_3);
        Ok(())
    }

    #[tokio::test]
    async fn request_input_tool_result_includes_call_correlation() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conversation_id = ThreadId::new();
        let recorder = ModelIoRecorder::new(dir.path().to_path_buf(), &conversation_id)?;
        let path = recorder.path.as_path().to_path_buf();
        let ai_path = recorder.ai_path.as_path().to_path_buf();

        recorder
            .register_tool_call_origin("call-42".to_string(), 5, 9, "shell".to_string())
            .await;
        recorder
            .record_request_input_item(
                8,
                1,
                0,
                &ResponseItem::FunctionCallOutput {
                    call_id: "call-42".to_string(),
                    output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                        "ok".to_string(),
                    ),
                },
            )
            .await;
        drop(recorder);

        let contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&path).await
                    && contents.contains("\"correlates_to\"")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        assert!(contents.contains("\"call_id\": \"call-42\""));
        assert!(contents.contains("\"tool_name\": \"shell\""));
        assert!(contents.contains("\"request_id\": 5"));
        assert!(contents.contains("\"seq\": 9"));

        let ai_contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&ai_path).await
                    && contents.contains("\"kind\":\"request_input_item\"")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        let ai_records: Vec<Value> = ai_contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect::<Result<Vec<_>, _>>()?;
        let request_input = ai_records
            .iter()
            .find(|record| record.get("kind").and_then(Value::as_str) == Some("request_input_item"))
            .expect("request_input_item");
        assert_eq!(
            request_input
                .pointer("/retrieval/call_id")
                .and_then(Value::as_str)
                .expect("call_id"),
            "call-42"
        );
        assert_eq!(
            request_input
                .pointer("/retrieval/tool_name")
                .and_then(Value::as_str)
                .expect("tool_name"),
            "shell"
        );
        Ok(())
    }

    #[tokio::test]
    async fn emits_request_and_response_summaries_in_phase_order() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conversation_id = ThreadId::new();
        let recorder = ModelIoRecorder::new(dir.path().to_path_buf(), &conversation_id)?;
        let path = recorder.path.as_path().to_path_buf();

        recorder
            .record_request_start(
                99,
                &serde_json::json!({
                    "instructions": "be concise",
                    "tools": [{"name": "shell"}],
                    "tool_choice": "auto",
                    "parallel_tool_calls": false,
                }),
            )
            .await;
        recorder
            .record_request_input_item(
                99,
                1,
                0,
                &ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![],
                    end_turn: None,
                    phase: None,
                },
            )
            .await;
        recorder
            .record_request_input_item(
                99,
                2,
                1,
                &ResponseItem::Message {
                    id: None,
                    role: "assistant".to_string(),
                    content: vec![],
                    end_turn: None,
                    phase: None,
                },
            )
            .await;
        recorder
            .record_request_input_item(
                99,
                3,
                2,
                &ResponseItem::FunctionCall {
                    id: None,
                    name: "shell".to_string(),
                    arguments: "{\"command\":\"pwd\"}".to_string(),
                    call_id: "call-99".to_string(),
                },
            )
            .await;
        recorder
            .register_tool_call_origin("call-99".to_string(), 99, 3, "shell".to_string())
            .await;
        recorder
            .record_request_input_item(
                99,
                4,
                3,
                &ResponseItem::FunctionCallOutput {
                    call_id: "call-99".to_string(),
                    output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                        "ok".to_string(),
                    ),
                },
            )
            .await;
        recorder
            .record_request_end(99, 5, &serde_json::json!({ "type": "completed" }))
            .await;
        drop(recorder);

        let contents = timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(contents) = tokio::fs::read_to_string(&path).await
                    && contents.contains("kind=response")
                {
                    break contents;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        let request_marker = "kind=request\n```json\n";
        let request_start = contents.find(request_marker).expect("request marker");
        let request_json_start = request_start + request_marker.len();
        let request_rest = &contents[request_json_start..];
        let request_json_end = request_rest.find("\n```").expect("request json end");
        let request_payload: Value = serde_json::from_str(&request_rest[..request_json_end])?;
        let phase_names: Vec<&str> = request_payload
            .get("phases")
            .and_then(Value::as_array)
            .expect("request phases")
            .iter()
            .map(|phase| {
                phase
                    .get("phase")
                    .and_then(Value::as_str)
                    .expect("phase name")
            })
            .collect();
        assert_eq!(
            phase_names,
            vec![
                "system_prompt",
                "tools",
                "user_assistant",
                "user_assistant",
                "function_call",
                "tool_result",
            ]
        );

        let response_marker = "kind=response\n```json\n";
        let response_start = contents.find(response_marker).expect("response marker");
        let response_json_start = response_start + response_marker.len();
        let response_rest = &contents[response_json_start..];
        let response_json_end = response_rest.find("\n```").expect("response json end");
        let response_payload: Value = serde_json::from_str(&response_rest[..response_json_end])?;
        assert_eq!(response_payload, serde_json::json!({ "type": "completed" }));
        assert!(request_start < response_start);
        Ok(())
    }
}
