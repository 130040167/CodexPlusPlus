//! 全部 FileState 共用的保守配额；宁可返回 partial，也不丢弃未接收行或累计基线。
use super::{FileState, MAX_ACTIVITY, MAX_EVENTS};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const MAX_RETAINED_BYTES: usize = 512 * 1024 * 1024;
const MAX_ROUND_BYTES: u64 = 64 * 1024 * 1024;
// 解压窗口限制为 8 MiB，另给输入/输出缓冲及解码器管理结构留出余量。
pub(super) const CURSOR_BYTES: usize = 10 * 1024 * 1024;
const LINE_OVERHEAD: usize = 4096;

#[derive(Default, Clone, Copy)]
pub(super) struct Usage {
    events: usize,
    activity: usize,
    bytes: usize,
}

impl Usage {
    pub(super) fn of(state: &FileState) -> Self {
        Self {
            events: state.events.len(),
            activity: state.tools.len()
                + state.calls.len()
                + state.exec_skills.len()
                + state.exec_polls.len(),
            bytes: state.retained_charge,
        }
    }
}

pub(super) struct Budget {
    // 不含当前正在读取的 FileState；其用量在准入时实时检查。
    used: Usage,
    pub(super) read_left: u64,
    pub(super) source_left: u64,
    memory_limit: usize,
    event_limit: usize,
    activity_limit: usize,
}

impl Budget {
    pub(super) fn new<'a>(states: impl Iterator<Item = &'a FileState>) -> Self {
        let mut budget = Self {
            used: Usage::default(),
            read_left: MAX_ROUND_BYTES,
            source_left: MAX_ROUND_BYTES,
            memory_limit: MAX_RETAINED_BYTES,
            event_limit: MAX_EVENTS,
            activity_limit: MAX_ACTIVITY,
        };
        for state in states {
            budget.replace(Usage::default(), Usage::of(state));
        }
        budget
    }

    pub(super) fn replace(&mut self, before: Usage, after: Usage) {
        self.used.events = self.used.events.saturating_sub(before.events) + after.events;
        self.used.activity = self.used.activity.saturating_sub(before.activity) + after.activity;
        self.used.bytes = self.used.bytes.saturating_sub(before.bytes) + after.bytes;
    }

    fn memory_left(&self, state: &FileState) -> usize {
        self.memory_limit
            .saturating_sub(self.used.bytes.saturating_add(state.retained_charge))
    }

    pub(super) fn read_limit(&self, state: &FileState) -> usize {
        // Vec 扩容最多预留约两倍新增输入，不把正文计为永久保留量。
        (self.read_left as usize).min(self.memory_left(state) / 2)
    }

    pub(super) fn can_read(&self, state: &FileState) -> bool {
        self.read_limit(state) > 0
            && self.used.events + state.events.len() < self.event_limit
            && self.used.activity + Usage::of(state).activity + 4 <= self.activity_limit
    }

    pub(super) fn can_open(&self, state: &FileState) -> bool {
        self.can_read(state)
            && self.source_left > 0
            && self.memory_left(state) > CURSOR_BYTES + LINE_OVERHEAD
    }

    pub(super) fn charge_read(&mut self, bytes: usize) {
        self.read_left -= bytes as u64;
    }

    pub(super) fn accept_line(&self, state: &FileState) -> bool {
        let vector_growth = |len: usize, capacity: usize, element: usize| {
            if len == capacity {
                capacity.max(4).saturating_mul(element)
            } else {
                0
            }
        };
        let growth = vector_growth(
            state.events.len(),
            state.events.capacity(),
            std::mem::size_of::<super::UsageEvent>(),
        ) + vector_growth(
            state.tools.len(),
            state.tools.capacity(),
            std::mem::size_of::<super::ToolRecord>(),
        ) + state.calls.capacity()
            * (std::mem::size_of::<(String, super::Call)>() + 32)
            + state.exec_skills.capacity()
                * (std::mem::size_of::<(String, super::ExecSkill)>() + 32)
            + state.exec_polls.capacity() * (std::mem::size_of::<(String, String)>() + 32)
            + [
                &state.session,
                &state.turn,
                &state.provider,
                &state.model,
                &state.role,
            ]
            .iter()
            .map(|s| s.capacity())
            .sum::<usize>()
            + state
                .inherited_contexts
                .iter()
                .map(|context| {
                    [
                        &context.session,
                        &context.turn,
                        &context.provider,
                        &context.model,
                        &context.role,
                    ]
                    .iter()
                    .map(|s| s.capacity())
                    .sum::<usize>()
                })
                .sum::<usize>();
        if self.memory_left(state) < parse_reserve(&state.pending).saturating_add(growth)
            || self.used.events + state.events.len() >= self.event_limit
            || self.used.activity + Usage::of(state).activity + 4 > self.activity_limit
        {
            return false;
        }
        true
    }
}

// 正文由 accept 的借用 RawValue 路径丢弃，不为它申请 Value 解析空间。
fn parse_reserve(line: &[u8]) -> usize {
    #[derive(serde::Deserialize)]
    struct Envelope<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
        #[serde(borrow)]
        payload: &'a serde_json::value::RawValue,
    }
    #[derive(serde::Deserialize)]
    struct Item<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }
    if let Ok(envelope) = serde_json::from_slice::<Envelope<'_>>(line) {
        if envelope.kind == "response_item" {
            if let Ok(item) = serde_json::from_str::<Item<'_>>(envelope.payload.get()) {
                if !matches!(
                    item.kind,
                    "function_call"
                        | "custom_tool_call"
                        | "function_call_output"
                        | "custom_tool_call_output"
                ) {
                    return LINE_OVERHEAD;
                }
            }
        }
    }
    // 包括临时 Value、字段克隆和容器开销；只在本次 accept 前预留。
    line.len().saturating_mul(64).saturating_add(LINE_OVERHEAD)
}

fn value_heap(value: &serde_json::Value) -> usize {
    use serde_json::Value;
    match value {
        Value::String(text) => text.capacity(),
        Value::Array(values) => {
            values.capacity() * std::mem::size_of::<Value>()
                + values.iter().map(value_heap).sum::<usize>()
        }
        Value::Object(values) => values
            .iter()
            .map(|(key, value)| {
                // Map 的节点/桶及装载率留有余量，避免依赖 serde_json 的 Map 后端。
                128 + key.capacity() + std::mem::size_of::<Value>() + value_heap(value)
            })
            .sum(),
        _ => 0,
    }
}

fn call_heap(call: &super::Call) -> usize {
    call.timestamp.capacity() + call.session.capacity() + value_heap(&call.payload)
}

fn event_heap(event: &super::UsageEvent) -> usize {
    [
        &event.session,
        &event.turn,
        &event.model,
        &event.timestamp,
        &event.provider,
        &event.role,
    ]
    .iter()
    .map(|text| text.capacity())
    .sum::<usize>()
        + event
            .legacy_identity
            .as_ref()
            .map_or(0, |id| id.session.capacity() + id.role.capacity())
}

fn tool_heap(tool: &super::ToolRecord) -> usize {
    tool.id.capacity() + call_heap(&tool.call) + value_heap(&tool.output)
}

// 向量的已有记录只在每个文件/每轮入口遍历一次；每行仅计量新增尾部。
// 其余有界映射与元数据直接重估，正文清空后立即释放其预算。
pub(super) struct Meter {
    events: usize,
    tools: usize,
    event_heap: usize,
    tool_heap: usize,
}

impl Meter {
    pub(super) fn new(state: &FileState) -> Self {
        Self {
            events: state.events.len(),
            tools: state.tools.len(),
            event_heap: state.events.iter().map(event_heap).sum(),
            tool_heap: state.tools.iter().map(tool_heap).sum(),
        }
    }

    pub(super) fn refresh(&mut self, state: &mut FileState, cursor_bytes: usize) {
        if state.events.len() < self.events || state.tools.len() < self.tools {
            *self = Self::new(state);
        } else {
            self.event_heap += state.events[self.events..]
                .iter()
                .map(event_heap)
                .sum::<usize>();
            self.tool_heap += state.tools[self.tools..]
                .iter()
                .map(tool_heap)
                .sum::<usize>();
            self.events = state.events.len();
            self.tools = state.tools.len();
        }
        let strings = [
            &state.session,
            &state.model,
            &state.turn,
            &state.provider,
            &state.role,
        ];
        state.retained_charge = std::mem::size_of::<FileState>()
            + cursor_bytes
            + state.pending.capacity()
            + strings.iter().map(|text| text.capacity()).sum::<usize>()
            + state.events.capacity() * std::mem::size_of::<super::UsageEvent>()
            + self.event_heap
            + state.tools.capacity() * std::mem::size_of::<super::ToolRecord>()
            + self.tool_heap
            + state.calls.capacity() * (std::mem::size_of::<(String, super::Call)>() + 32)
            + state
                .calls
                .iter()
                .map(|(key, call)| key.capacity() + call_heap(call))
                .sum::<usize>()
            + state.exec_skills.capacity()
                * (std::mem::size_of::<(String, super::ExecSkill)>() + 32)
            + state
                .exec_skills
                .iter()
                .map(|(key, skill)| {
                    key.capacity() + call_heap(&skill.call) + skill.evidence.capacity()
                })
                .sum::<usize>()
            + state.exec_polls.capacity() * (std::mem::size_of::<(String, String)>() + 32)
            + state
                .exec_polls
                .iter()
                .map(|(key, value)| key.capacity() + value.capacity())
                .sum::<usize>()
            + state.inherited_contexts.capacity()
                * std::mem::size_of::<super::InheritedUsageContext>()
            + state
                .inherited_contexts
                .iter()
                .map(|context| {
                    [
                        &context.session,
                        &context.model,
                        &context.turn,
                        &context.provider,
                        &context.role,
                    ]
                    .iter()
                    .map(|text| text.capacity())
                    .sum::<usize>()
                        + context
                            .prefix
                            .as_ref()
                            .map_or(0, |(parent, _)| parent.capacity())
                })
                .sum::<usize>()
            + state
                .history_boundary
                .parent
                .as_ref()
                .map_or(0, String::capacity)
            + state
                .legacy_header
                .as_ref()
                .map_or(0, |(session, role)| session.capacity() + role.capacity())
            + state
                .scope_parent_candidates
                .iter()
                .map(|(session, parents)| {
                    128 + session.capacity()
                        + parents.capacity() * (std::mem::size_of::<Option<String>>() + 32)
                        + parents
                            .iter()
                            .flatten()
                            .map(String::capacity)
                            .sum::<usize>()
                })
                .sum::<usize>();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_history::structured::Index;
    use chrono::{Duration, Local};
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;

    fn fixture(home: &std::path::Path, suffix: &str, compressed: bool) -> PathBuf {
        let now = Local::now().to_rfc3339();
        let mut rows = vec![json!({"type":"session_meta","payload":{
            "id":"00000000-0000-4000-8000-000000000001"
        }})];
        for total in 1..=6 {
            rows.push(json!({"type":"event_msg","timestamp":now,"payload":{
                "type":"token_count","info":{"total_token_usage":{
                    "input_tokens":total,"output_tokens":0,"total_tokens":total
                },"last_token_usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}}
            }}));
        }
        let text = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
        let directory = home.join("sessions");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "rollout-{suffix}.jsonl{}",
            if compressed { ".zst" } else { "" }
        ));
        fs::write(
            &path,
            if compressed {
                zstd::stream::encode_all(text.as_bytes(), 1).unwrap()
            } else {
                text.into_bytes()
            },
        )
        .unwrap();
        fs::canonicalize(path).unwrap()
    }

    fn cutoff() -> chrono::NaiveDate {
        Local::now().date_naive() - Duration::days(1)
    }

    #[test]
    fn event_limit_is_shared_across_files_and_unread_rows_resume_once() {
        let temp = tempfile::tempdir().unwrap();
        let paths = [
            fixture(temp.path(), "a", false),
            fixture(temp.path(), "b", false),
        ];
        let mut states = [FileState::default(), FileState::default()];
        let mut budget = Budget::new(std::iter::empty());
        budget.event_limit = 8;
        assert!(
            states[0]
                .read_with_budget(&paths[0], cutoff(), &mut budget)
                .unwrap()
        );
        budget.replace(Usage::default(), Usage::of(&states[0]));
        assert!(
            !states[1]
                .read_with_budget(&paths[1], cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(
            states.iter().map(|state| state.events.len()).sum::<usize>(),
            8
        );
        assert_eq!(states[1].events.len(), 2);
        let mut budget = Budget::new(std::iter::once(&states[0]));
        assert!(
            states[1]
                .read_with_budget(&paths[1], cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(states[1].events.len(), 6);
        assert_eq!(
            states[1]
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            6
        );
        assert!(
            states[1]
                .read_with_budget(&paths[1], cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(states[1].events.len(), 6);
    }

    #[test]
    fn byte_budget_is_shared_across_files() {
        let temp = tempfile::tempdir().unwrap();
        let paths = [
            fixture(temp.path(), "first", false),
            fixture(temp.path(), "second", false),
        ];
        let mut states = [FileState::default(), FileState::default()];
        let limit = fs::metadata(&paths[0]).unwrap().len() + 50;
        let mut budget = Budget::new(std::iter::empty());
        budget.read_left = limit;
        assert!(
            states[0]
                .read_with_budget(&paths[0], cutoff(), &mut budget)
                .unwrap()
        );
        budget.replace(Usage::default(), Usage::of(&states[0]));
        assert!(
            !states[1]
                .read_with_budget(&paths[1], cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(states[0].bytes_read + states[1].bytes_read, limit);
        assert_eq!(states[1].offset, 50);
        let mut next = Budget::new(std::iter::once(&states[0]));
        assert!(
            states[1]
                .read_with_budget(&paths[1], cutoff(), &mut next)
                .unwrap()
        );
        assert_eq!(states[1].events.len(), 6);
        assert_eq!(
            states[1]
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            6
        );
    }

    #[test]
    fn plain_and_zstd_resume_across_byte_and_source_budgets_without_replay() {
        for compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = fixture(temp.path(), "bounded", compressed);
            let mut state = FileState::default();
            let mut complete = false;
            let mut consumed = 0;
            for _ in 0..200 {
                let mut budget = Budget::new(std::iter::empty());
                budget.read_left = 73;
                budget.source_left = 37;
                let before = state.bytes_read;
                complete = state
                    .read_with_budget(&path, cutoff(), &mut budget)
                    .unwrap();
                let bytes = state.bytes_read - before;
                assert!(bytes <= 73);
                assert!(budget.source_left <= 37);
                consumed += bytes;
                if complete {
                    break;
                }
            }
            assert!(complete, "bounded stream must eventually reach EOF");
            assert_eq!(consumed, state.offset);
            assert_eq!(state.events.len(), 6);
            assert_eq!(
                state
                    .events
                    .iter()
                    .map(|event| event.delta.total_tokens)
                    .sum::<u64>(),
                6
            );
            let before = state.offset;
            assert!(
                state
                    .read_with_budget(&path, cutoff(), &mut Budget::new(std::iter::empty()))
                    .unwrap()
            );
            assert_eq!(state.offset, before);
            assert_eq!(state.events.len(), 6);
        }
    }

    #[test]
    fn partial_reader_resets_when_path_is_replaced_with_larger_file() {
        use std::io::Write;

        let temp = tempfile::tempdir().unwrap();
        let path = fixture(temp.path(), "replaced", false);
        let original = fs::read_to_string(&path).unwrap();
        let mut state = FileState::default();
        let mut budget = Budget::new(std::iter::empty());
        // 保留已有事件、未完成行及旧句柄中的预读数据。
        budget.read_left = original
            .split_inclusive('\n')
            .take(4)
            .map(str::len)
            .sum::<usize>() as u64
            + 11;
        assert!(
            !state
                .read_with_budget(&path, cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(state.events.len(), 3);
        assert!(!state.pending.is_empty());
        assert!(state.reader.is_some());
        let old_identity = state.stamp.unwrap().2;
        assert!(old_identity.is_some());

        let replacement = fixture(temp.path(), "replacement", false);
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&replacement)
            .unwrap();
        let now = Local::now().to_rfc3339();
        for total in 7..=9 {
            writeln!(
                file,
                "{}",
                json!({"type":"event_msg","timestamp":now,"payload":{
                    "type":"token_count","info":{"total_token_usage":{
                        "input_tokens":total,"output_tokens":0,"total_tokens":total
                    },"last_token_usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}}
                }})
            )
            .unwrap();
        }
        drop(file);
        let replacement_len = fs::metadata(&replacement).unwrap().len();
        assert!(replacement_len > original.len() as u64);
        fs::rename(&path, path.with_extension("previous")).unwrap();
        fs::rename(&replacement, &path).unwrap();

        let mut budget = Budget::new(std::iter::empty());
        assert!(
            state
                .read_with_budget(&path, cutoff(), &mut budget)
                .unwrap()
        );
        assert_ne!(state.stamp.unwrap().2, old_identity);
        assert_eq!(state.offset, replacement_len);
        assert_eq!(state.events.len(), 9);
        assert_eq!(
            state
                .events
                .iter()
                .map(|event| event.total.total_tokens)
                .collect::<Vec<_>>(),
            (1..=9).collect::<Vec<_>>()
        );
        assert_eq!(
            state
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            9
        );
        assert!(state.pending.is_empty());
        assert!(state.reader.is_none());
        let before = state.bytes_read;
        assert!(
            state
                .read_with_budget(&path, cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(state.bytes_read, before);
        assert_eq!(state.events.len(), 9);
    }

    #[test]
    fn exhausted_memory_and_pending_activity_prevent_new_file_admission() {
        let temp = tempfile::tempdir().unwrap();
        let path = fixture(temp.path(), "memory", true);
        let mut held = FileState::default();
        let call = super::super::Call {
            timestamp: Local::now().to_rfc3339(),
            session: "00000000-0000-4000-8000-000000000001".into(),
            payload: json!({"arguments":"x".repeat(64 * 1024)}),
        };
        held.calls.insert("pending".into(), call.clone());
        held.exec_skills.insert(
            "background".into(),
            super::super::ExecSkill {
                call,
                evidence: "evidence".into(),
            },
        );
        held.exec_polls.insert("poll".into(), "background".into());
        Meter::new(&held).refresh(&mut held, 0);
        assert_eq!(Usage::of(&held).activity, 3);
        assert!(held.retained_charge >= 128 * 1024);
        let mut state = FileState::default();
        let mut budget = Budget::new(std::iter::once(&held));
        budget.memory_limit = held.retained_charge + CURSOR_BYTES - 1;
        assert!(
            !state
                .read_with_budget(&path, cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(state.offset, 0);
        assert!(state.stamp.is_none());
        assert!(state.reader.is_none());

        let mut budget = Budget::new(std::iter::once(&held));
        budget.activity_limit = 6;
        assert!(
            !state
                .read_with_budget(&path, cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(state.offset, 0);
        let mut budget = Budget::new(std::iter::empty());
        assert!(
            state
                .read_with_budget(&path, cutoff(), &mut budget)
                .unwrap()
        );
        assert_eq!(state.events.len(), 6);
    }

    #[test]
    fn large_ignored_bodies_cross_rounds_then_complete_with_token_baseline_intact() {
        use std::io::Write;
        for compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = fixture(temp.path(), "large", false);
            let prefix = fs::read(&path).unwrap();
            let mut file = File::create(&path).unwrap();
            file.write_all(&prefix).unwrap();
            let message = format!(
                "{}\n",
                json!({"type":"response_item","payload":{
                    "type":"message","content":"x".repeat(4 * 1024 * 1024)
                }})
            );
            // 超过真实每轮 64 MiB 限额；忽略正文不能永久耗尽 retained 配额。
            for _ in 0..18 {
                file.write_all(message.as_bytes()).unwrap();
            }
            let now = Local::now().to_rfc3339();
            writeln!(
                file,
                "{}",
                json!({"type":"event_msg","timestamp":now,"payload":{
                    "type":"token_count","info":{"total_token_usage":{
                        "input_tokens":7,"output_tokens":0,"total_tokens":7
                    },"last_token_usage":{"input_tokens":1,"output_tokens":0,"total_tokens":1}}
                }})
            )
            .unwrap();
            drop(file);
            let path = if compressed {
                let destination = path.with_extension("jsonl.zst");
                let mut input = File::open(&path).unwrap();
                let mut encoder =
                    zstd::stream::write::Encoder::new(File::create(&destination).unwrap(), 1)
                        .unwrap();
                io::copy(&mut input, &mut encoder).unwrap();
                encoder.finish().unwrap();
                // 普通文件保留但不参与发现，不删除测试文件。
                fs::rename(&path, path.with_extension("fixture")).unwrap();
                fs::canonicalize(destination).unwrap()
            } else {
                path
            };
            let home = fs::canonicalize(temp.path()).unwrap();
            let mut index = Index::default();
            let query =
                crate::usage_history::Query::parse(&json!({}), Local::now().date_naive()).unwrap();
            let first = index.update_with_request_log(&home, &home.join("missing"));
            assert!(first.bytes_read <= MAX_ROUND_BYTES);
            assert_eq!(first.response(&query, false)["complete"], false);
            assert_eq!(first.events.len(), 6);
            let second = index.update_with_request_log(&home, &home.join("missing"));
            assert!(second.bytes_read <= MAX_ROUND_BYTES);
            assert_eq!(second.response(&query, false)["complete"], true);
            assert_eq!(second.events.len(), 7);
            assert_eq!(
                second
                    .events
                    .iter()
                    .map(|event| event.delta.total_tokens)
                    .sum::<u64>(),
                7
            );
            assert!(index.files[&path].retained_charge < 16 * 1024 * 1024);
            let warm = index.update_with_request_log(&home, &home.join("missing"));
            assert_eq!(warm.bytes_read, 0);
            assert_eq!(warm.events.len(), 7);
            assert_eq!(warm.response(&query, false)["complete"], true);
        }
    }
}

struct Source {
    file: File,
    left: Arc<AtomicU64>,
}

impl Read for Source {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = (self.left.load(Ordering::Relaxed) as usize).min(buffer.len());
        if limit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "index read budget",
            ));
        }
        let bytes = self.file.read(&mut buffer[..limit])?;
        self.left.fetch_sub(bytes as u64, Ordering::Relaxed);
        Ok(bytes)
    }
}

pub(super) struct ReadCursor {
    pub(super) input: Box<dyn BufRead + Send>,
    left: Arc<AtomicU64>,
}

impl ReadCursor {
    pub(super) fn new(file: File, compressed: bool) -> io::Result<Self> {
        let left = Arc::new(AtomicU64::new(0));
        let source = Source {
            file,
            left: left.clone(),
        };
        let input: Box<dyn BufRead + Send> = if compressed {
            let mut decoder = zstd::stream::read::Decoder::new(source)?;
            decoder.window_log_max(23)?;
            Box::new(BufReader::with_capacity(128 * 1024, decoder))
        } else {
            Box::new(BufReader::with_capacity(128 * 1024, source))
        };
        Ok(Self { input, left })
    }

    pub(super) fn set_allowance(&self, bytes: u64) {
        self.left.store(bytes, Ordering::Relaxed);
    }

    pub(super) fn allowance(&self) -> u64 {
        self.left.load(Ordering::Relaxed)
    }
}
