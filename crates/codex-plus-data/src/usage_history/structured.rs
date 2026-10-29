//! 共享后台索引：正文留在后台，只向拓展提供用量和已完成工具回执。
use super::{Aggregate, Query, Record, Tokens, file_identity};
use chrono::{DateTime, Duration as Days, Local, NaiveDate};
use serde::Deserialize;
use serde_json::{Value, json, value::RawValue};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

mod index_budget;
mod live_receipts;
mod native_endpoints;
mod provider_cache;
mod tool_receipts;

const RETENTION_DAYS: i64 = 62;
const MAX_ACTIVITY: usize = 12_000;
const MAX_EVENTS: usize = 100_000;
const MAX_FILES: usize = 4096;
// 单行保护与文件大小无关；所有文件均可增量读取。巨大正文不传入渲染进程。
const MAX_LINE: usize = 32 * 1024 * 1024;
const REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const MAX_REQUEST_LOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_REQUEST_LOG_LINE: usize = 16 * 1024;
const MAX_REQUEST_EVIDENCE: usize = 8192;
const MAX_ROWS_CAPTURED_PROXY: usize = 20_000;
const REQUEST_MATCH_WINDOW_MS: i64 = 30_000;
const DELAYED_REQUEST_MATCH_WINDOW_MS: i64 = 120_000;
const MAX_PROVIDER_EVIDENCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_PROVIDER_EVIDENCE: usize = 100_000;

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ProviderEvidence {
    event_id: String,
    session: String,
    timestamp: String,
    model: String,
    role: String,
    input: u64,
    output: u64,
    cached: u64,
    provider: String,
    provider_name: String,
    source: String,
}

impl ProviderEvidence {
    fn id(&self) -> Option<[u8; 32]> {
        if self.event_id.len() != 64 {
            return None;
        }
        let mut id = [0; 32];
        for (byte, pair) in id.iter_mut().zip(self.event_id.as_bytes().chunks_exact(2)) {
            *byte =
                (pair[0] as char).to_digit(16)? as u8 * 16 + (pair[1] as char).to_digit(16)? as u8;
        }
        Some(id)
    }

    fn valid(&self) -> bool {
        self.id().is_some()
            && [
                &self.session,
                &self.timestamp,
                &self.model,
                &self.role,
                &self.provider,
            ]
            .iter()
            .all(|s| !s.trim().is_empty() && s.len() <= 256)
            && self.provider_name.len() <= 256
            && self.provider == self.provider.trim()
            && !unknown_provider(&self.provider)
            && matches!(
                self.source.as_str(),
                "upstream-exact-bidirectional" | "user-provider-classification"
            )
            && DateTime::parse_from_rfc3339(&self.timestamp).is_ok()
            && self.cached <= self.input
            && self
                .input
                .checked_add(self.output)
                .is_some_and(|total| total > 0)
    }

    fn matches(&self, event: &UsageEvent) -> bool {
        self.session == event.session
            && DateTime::parse_from_rfc3339(&self.timestamp)
                .ok()
                .zip(DateTime::parse_from_rfc3339(&event.timestamp).ok())
                .is_some_and(|(a, b)| a == b)
            && self.model == event.model
            && self.role == event.role
            && self.input == event.delta.input_tokens
            && self.output == event.delta.output_tokens
            && self.cached == event.delta.cached_input_tokens
    }
}

#[derive(Default)]
struct ProviderEvidenceCache {
    path: PathBuf,
    stamp: Option<(u64, Option<SystemTime>, Option<(u64, u64)>)>,
    records: HashMap<[u8; 32], ProviderEvidence>,
    // 改写过的身份在本索引生命周期内不再可信；历史同样有上限。
    seen: HashMap<[u8; 32], Option<ProviderEvidence>>,
    invalid: usize,
    reads: u64,
    bytes_read: u64,
}

fn bounded_provider_records<'de, D>(deserializer: D) -> Result<Vec<Box<RawValue>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Records;
    impl<'de> serde::de::Visitor<'de> for Records {
        type Value = Vec<Box<RawValue>>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("bounded provider evidence records")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut records = Vec::new();
            while let Some(record) = seq.next_element::<Box<RawValue>>()? {
                if records.len() == MAX_PROVIDER_EVIDENCE {
                    return Err(serde::de::Error::custom("provider evidence limit"));
                }
                records.push(record);
            }
            Ok(records)
        }
    }
    deserializer.deserialize_seq(Records)
}

impl ProviderEvidenceCache {
    fn refresh(&mut self, path: &Path) {
        if self.path != path {
            *self = Self {
                path: path.to_owned(),
                ..Self::default()
            };
        }
        self.bytes_read = 0;
        let metadata = fs::symlink_metadata(path).ok().filter(|m| m.is_file());
        let stamp = metadata
            .as_ref()
            .map(|m| (m.len(), m.modified().ok(), file_identity(m)));
        if self.stamp == stamp {
            return;
        }
        self.stamp = stamp;
        self.records.clear();
        self.invalid = 0;
        let Some(metadata) = metadata else {
            return;
        };
        if metadata.len() > MAX_PROVIDER_EVIDENCE_BYTES {
            self.invalid = 1;
            return;
        }
        let result = (|| -> io::Result<Vec<u8>> {
            let file = File::open(path)?;
            let opened = file.metadata()?;
            if (opened.len(), opened.modified().ok(), file_identity(&opened)) != stamp.unwrap() {
                return Err(io::Error::other("provider evidence changed"));
            }
            let mut bytes = Vec::new();
            file.take(MAX_PROVIDER_EVIDENCE_BYTES + 1)
                .read_to_end(&mut bytes)?;
            Ok(bytes)
        })();
        self.reads += 1;
        let Ok(bytes) = result else {
            self.invalid = 1;
            self.stamp = None;
            return;
        };
        self.bytes_read = bytes.len() as u64;
        let after = fs::symlink_metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok(), file_identity(&m)));
        if after != stamp || bytes.len() as u64 != metadata.len() {
            self.invalid = 1;
            self.stamp = None;
            return;
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Document {
            version: u32,
            #[serde(deserialize_with = "bounded_provider_records")]
            records: Vec<Box<RawValue>>,
        }
        let document = serde_json::from_slice::<Document>(&bytes);
        let Ok(document) = document else {
            self.invalid = 1;
            return;
        };
        if document.version != 1 {
            self.invalid = 1;
            return;
        }
        for raw in document.records {
            let parsed = serde_json::from_str::<ProviderEvidence>(raw.get());
            let Ok(record) = parsed else {
                // 类型损坏但身份明确时也必须撤销旧证据。
                if let Ok(value) = serde_json::from_str::<Value>(raw.get()) {
                    if let Some(id) = value.get("eventId").and_then(Value::as_str) {
                        let probe = ProviderEvidence {
                            event_id: id.into(),
                            session: String::new(),
                            timestamp: String::new(),
                            model: String::new(),
                            role: String::new(),
                            input: 0,
                            output: 0,
                            cached: 0,
                            provider: String::new(),
                            provider_name: String::new(),
                            source: String::new(),
                        };
                        if let Some(id) = probe.id() {
                            if self.seen.contains_key(&id)
                                || self.seen.len() < MAX_PROVIDER_EVIDENCE
                            {
                                self.seen.insert(id, None);
                            }
                            self.records.remove(&id);
                        }
                    }
                }
                self.invalid += 1;
                continue;
            };
            let Some(id) = record.id() else {
                self.invalid += 1;
                continue;
            };
            let accepted = record.valid()
                && match self.seen.get(&id) {
                    Some(Some(previous)) => previous == &record,
                    Some(None) => false,
                    None => self.seen.len() < MAX_PROVIDER_EVIDENCE,
                };
            if !accepted {
                if self.seen.contains_key(&id) || self.seen.len() < MAX_PROVIDER_EVIDENCE {
                    self.seen.insert(id, None);
                }
                self.records.remove(&id);
                self.invalid += 1;
                continue;
            }
            self.seen.entry(id).or_insert_with(|| Some(record.clone()));
            self.records.insert(id, record);
        }
    }

    fn attribute(&self, events: &mut [UsageEvent], names: &mut BTreeMap<String, String>) -> Value {
        let (mut applied, mut tokens, mut conflicts, mut mismatches) =
            (0usize, 0u64, 0usize, 0usize);
        let mut counts = HashMap::new();
        for event in events.iter() {
            *counts.entry(event.id).or_insert(0usize) += 1;
        }
        let mut legacy_counts = HashMap::new();
        for event in events.iter() {
            if let Some(legacy) = &event.legacy_identity {
                *legacy_counts.entry(legacy.id).or_insert(0usize) += 1;
            }
        }
        let mut migrated = 0usize;
        let mut classified = 0usize;
        let mut classified_tokens = 0u64;
        for event in events {
            let direct = self.records.get(&event.id);
            let legacy = event.legacy_identity.as_ref().and_then(|identity| {
                // 只有明确复制的父头可建立旧身份，且旧新两端均只能命中一条。
                if counts.contains_key(&identity.id) || legacy_counts[&identity.id] != 1 {
                    return None;
                }
                self.records.get(&identity.id).filter(|record| {
                    let mut previous = event.clone();
                    previous.id = identity.id;
                    previous.session.clone_from(&identity.session);
                    previous.role.clone_from(&identity.role);
                    record.matches(&previous)
                })
            });
            if direct
                .zip(legacy)
                .is_some_and(|(a, b)| a.provider != b.provider)
            {
                conflicts += 1;
                continue;
            }
            let Some(record) = direct.or(legacy) else {
                continue;
            };
            if (direct.is_some() && !record.matches(event)) || counts[&event.id] != 1 {
                mismatches += 1;
                continue;
            }
            if !unknown_provider(&event.provider) {
                conflicts += usize::from(event.provider != record.provider);
                continue;
            }
            event.provider = record.provider.clone();
            if !record.provider_name.trim().is_empty() {
                let name = names.entry(record.provider.clone()).or_default();
                if name.trim().is_empty() {
                    *name = record.provider_name.clone();
                }
            }
            applied += 1;
            if record.source == "user-provider-classification" {
                classified += 1;
                classified_tokens = classified_tokens.saturating_add(event.delta.total_tokens);
            }
            migrated += usize::from(direct.is_none());
            tokens = tokens.saturating_add(event.delta.total_tokens);
        }
        json!({"source":"upstream-exact-bidirectional","scope":"index",
            "appliedEvents":applied,"appliedTokens":tokens,"events":self.records.len(),
            "invalid":self.invalid,"conflictedEvents":conflicts,"signatureMismatches":mismatches,"migratedEvents":migrated,
            "userClassifiedEvents":classified,"userClassifiedTokens":classified_tokens,
            "verifiedEvents":applied.saturating_sub(classified),"verifiedTokens":tokens.saturating_sub(classified_tokens),
            "fileReads":self.reads,"fileBytesRead":self.bytes_read})
    }
}

#[derive(Clone, Default, serde::Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
struct RequestEvidence {
    id: String,
    thread_id: String,
    model: String,
    upstream_model: String,
    relay_id: String,
    relay_name: String,
    success: bool,
    state: String,
    status_code: u16,
    timestamp_ms: i64,
    started_at_ms: i64,
    attempts: Option<u32>,
    auto_retries: Option<u32>,
    usage: Option<RequestEvidenceUsage>,
}

#[derive(Clone, Copy, Default, serde::Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, rename_all = "camelCase")]
struct RequestEvidenceUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_tokens: Option<u64>,
}

#[derive(Clone, Hash, serde::Serialize, Deserialize, PartialEq, Eq)]
struct RequestMatchKey {
    session: String,
    model: String,
    input: u64,
    output: u64,
    cached: u64,
}

struct RequestSessionAliases {
    canonical: HashMap<String, Option<String>>,
}

impl RequestSessionAliases {
    fn identity(session: &str) -> String {
        super::normalize_session_id(session).unwrap_or_else(|| session.to_owned())
    }

    fn new(aliases: &BTreeMap<String, String>) -> Self {
        let mut edges: HashMap<String, String> = HashMap::new();
        let mut neighbors: HashMap<String, Vec<String>> = HashMap::new();
        let mut conflicts = HashSet::new();
        for (alias, target) in aliases {
            let alias = Self::identity(alias);
            let target = Self::identity(target);
            if alias.is_empty() || target.is_empty() {
                conflicts.insert(alias.clone());
                conflicts.insert(target.clone());
            }
            if edges
                .insert(alias.clone(), target.clone())
                .is_some_and(|old| old != target)
            {
                conflicts.insert(alias.clone());
            }
            neighbors
                .entry(alias.clone())
                .or_default()
                .push(target.clone());
            neighbors.entry(target).or_default().push(alias);
        }
        let mut canonical = HashMap::new();
        for start in neighbors.keys() {
            if canonical.contains_key(start) {
                continue;
            }
            let mut pending = vec![start.clone()];
            let mut component = HashSet::new();
            let mut terminal = None;
            let mut invalid = false;
            while let Some(session) = pending.pop() {
                if !component.insert(session.clone()) {
                    continue;
                }
                invalid |= conflicts.contains(&session);
                // 自指是正常文件名与会话头一致的声明；多节点循环没有终点。
                if edges.get(&session).is_none_or(|target| target == &session) {
                    invalid |= terminal.replace(session.clone()).is_some();
                }
                pending.extend(neighbors[&session].iter().cloned());
            }
            let resolved = terminal.filter(|_| !invalid);
            for session in component {
                canonical.insert(session, resolved.clone());
            }
        }
        Self { canonical }
    }

    fn resolve(&self, session: &str) -> Option<String> {
        let session = Self::identity(session);
        if session.is_empty() {
            return None;
        }
        self.canonical
            .get(&session)
            .cloned()
            .unwrap_or(Some(session))
    }
}

fn request_base_model(model: &str) -> &str {
    if codex_plus_core::aggregate_catalog::is_aggregate_model(model) {
        model.split_once('/').map_or(model, |(_, model)| model)
    } else {
        model
    }
}

fn unknown_provider(provider: &str) -> bool {
    provider.is_empty()
        || ["custom", "codex_plus_managed", "unknown"]
            .iter()
            .any(|generic| provider.eq_ignore_ascii_case(generic))
}

impl RequestEvidence {
    fn keys(&self) -> Vec<RequestMatchKey> {
        let strings = [
            &self.id,
            &self.thread_id,
            &self.model,
            &self.upstream_model,
            &self.relay_id,
            &self.relay_name,
        ];
        if strings.iter().any(|s| s.is_empty() || s.len() > 256)
            || unknown_provider(&self.relay_id)
            || !self.success
            || self.state != "success"
            || !(200..300).contains(&self.status_code)
            || self
                .attempts
                .zip(self.auto_retries)
                .is_none_or(|(attempts, retries)| attempts == 0 || retries >= attempts)
            || self.started_at_ms <= 0
            || self.timestamp_ms < self.started_at_ms
        {
            return Vec::new();
        }
        let Some(RequestEvidenceUsage {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cached_tokens: Some(cached),
        }) = self.usage
        else {
            return Vec::new();
        };
        if cached > input || input.saturating_add(output) == 0 {
            return Vec::new();
        }
        let mut models = vec![
            request_base_model(&self.model),
            request_base_model(&self.upstream_model),
        ];
        models.dedup();
        models
            .into_iter()
            .filter(|model| !model.is_empty())
            .map(|model| RequestMatchKey {
                session: self.thread_id.clone(),
                model: model.to_owned(),
                input,
                output,
                cached,
            })
            .collect()
    }
}

#[derive(Clone, serde::Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct VerifiedAttribution {
    key: RequestMatchKey,
    at: i64,
    request: RequestEvidence,
    rejected: bool,
}

#[derive(Default)]
struct RequestEvidenceCache {
    path: PathBuf,
    stamp: Option<(u64, Option<SystemTime>, Option<(u64, u64)>)>,
    records: Vec<RequestEvidence>,
    captured_start: Option<usize>,
    captured_cursor: usize,
    captured_complete: Option<HashSet<[u8; 32]>>,
    captured_rejected: HashSet<String>,
    captured_id_check_failed: bool,
    verified: HashMap<[u8; 32], VerifiedAttribution>,
    reads: u64,
    bytes_read: u64,
}

impl RequestEvidenceCache {
    #[cfg(test)]
    pub(super) fn retained_verified(&self) -> Vec<([u8; 32], VerifiedAttribution)> {
        let mut retained: Vec<_> = self
            .verified
            .iter()
            .map(|(id, saved)| (*id, saved.clone()))
            .collect();
        retained.sort_unstable_by_key(|(id, _)| *id);
        retained
    }

    pub(super) fn restore_verified(
        &mut self,
        retained: impl IntoIterator<Item = ([u8; 32], VerifiedAttribution)>,
    ) {
        self.verified.clear();
        for (id, saved) in retained {
            if let Some(previous) = self.verified.get_mut(&id) {
                if *previous != saved {
                    previous.rejected = true;
                }
            } else if self.verified.len() < MAX_EVENTS {
                self.verified.insert(id, saved);
            }
        }
    }

    fn supplement_missing_session_models(&self, sessions: &mut BTreeMap<String, Value>, now: i64) {
        let mut models: BTreeMap<&str, Option<&str>> = BTreeMap::new();
        let mut valid = BTreeMap::new();
        let mut ids = HashSet::new();
        let mut blocked = HashSet::new();
        for record in &self.records {
            if !ids.insert(record.id.as_str()) || record.keys().is_empty() {
                blocked.insert(record.id.as_str());
            }
        }
        for record in &self.records {
            if super::normalize_session_id(&record.thread_id).is_none()
                || sessions
                    .get(&record.thread_id)
                    .and_then(|info| info["model"].as_str())
                    .is_some_and(|model| {
                        !model.is_empty() && !model.eq_ignore_ascii_case("unknown")
                    })
            {
                continue;
            }
            let model = request_base_model(&record.model);
            let entry = models.entry(&record.thread_id).or_insert(Some(model));
            if model.is_empty() || model.eq_ignore_ascii_case("unknown") || *entry != Some(model) {
                *entry = None;
            }
            if !blocked.contains(record.id.as_str())
                && record.timestamp_ms <= now
                && now.saturating_sub(record.timestamp_ms) <= 10 * 60 * 1000
            {
                let latest = valid.entry(record.thread_id.as_str()).or_insert(0);
                *latest = (*latest).max(record.timestamp_ms);
            }
        }
        let mut recent: Vec<_> = valid
            .into_iter()
            .filter(|(session, _)| models.get(session).copied().flatten().is_some())
            .collect();
        recent.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        for (session, _) in recent.into_iter().take(16) {
            let Some(model) = models.get(session).copied().flatten() else {
                continue;
            };
            // 仅补缺失模型，保留已落盘的角色/供应商；不产生用量或供应商推断。
            let info = sessions
                .entry(session.to_owned())
                .or_insert_with(|| json!({}));
            info["model"] = json!(model);
            info["modelSource"] = json!("request-log-single-model");
        }
    }

    fn refresh(&mut self, path: &Path) {
        if let Some(start) = self.captured_start.take() {
            self.records.truncate(start);
        }
        if self.path != path {
            // 首次绑定日志路径时保留刚恢复的事务快照；切换日志来源仍清空旧缓存。
            let verified = if self.path.as_os_str().is_empty() {
                std::mem::take(&mut self.verified)
            } else {
                HashMap::new()
            };
            *self = Self {
                path: path.to_owned(),
                verified,
                ..Self::default()
            };
        }
        self.bytes_read = 0;
        let metadata = fs::symlink_metadata(path).ok().filter(|m| m.is_file());
        let stamp = metadata
            .as_ref()
            .map(|m| (m.len(), m.modified().ok(), file_identity(m)));
        if self.stamp == stamp {
            return;
        }
        self.stamp = stamp;
        self.records.clear();
        let Some(metadata) = metadata.filter(|m| m.len() <= MAX_REQUEST_LOG_BYTES) else {
            return;
        };
        let Ok(file) = File::open(path) else {
            self.stamp = None;
            return;
        };
        if file
            .metadata()
            .ok()
            .is_none_or(|m| file_identity(&m) != file_identity(&metadata))
        {
            self.stamp = None;
            return;
        }
        self.reads += 1;
        // 总量和单行均有界；serde 跳过未声明字段，不分配 error/正文字符串。
        let mut reader = BufReader::new(file.take(metadata.len()));
        let mut line = Vec::new();
        let mut oversized = false;
        let result = (|| -> io::Result<()> {
            loop {
                let buffer = reader.fill_buf()?;
                if buffer.is_empty() {
                    break;
                }
                let newline = buffer.iter().position(|b| *b == b'\n');
                let length = newline.map_or(buffer.len(), |i| i + 1);
                if !oversized {
                    if line.len() + length > MAX_REQUEST_LOG_LINE {
                        line.clear();
                        oversized = true;
                    } else {
                        line.extend_from_slice(&buffer[..length]);
                    }
                }
                reader.consume(length);
                self.bytes_read += length as u64;
                if newline.is_some() {
                    if !oversized {
                        #[derive(Deserialize)]
                        struct Identity {
                            id: String,
                        }
                        let record = serde_json::from_slice::<RequestEvidence>(&line)
                            .ok()
                            .or_else(|| {
                                // 字段类型损坏但身份仍明确时，撤销该 ID 的旧补证，不能当作轮转缺失。
                                serde_json::from_slice::<Identity>(&line)
                                    .ok()
                                    .map(|identity| RequestEvidence {
                                        id: identity.id,
                                        ..Default::default()
                                    })
                            });
                        if let Some(record) = record {
                            if self.records.len() == MAX_REQUEST_EVIDENCE {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "request evidence limit",
                                ));
                            }
                            self.records.push(record);
                        }
                    }
                    line.clear();
                    oversized = false;
                }
            }
            Ok(())
        })();
        // 未换行的尾记录不接受；读取期间被替换或追加时，下轮重新投影。
        let after = fs::symlink_metadata(path)
            .ok()
            .map(|m| (m.len(), m.modified().ok(), file_identity(&m)));
        if let Err(error) = result {
            self.records.clear();
            if error.kind() != io::ErrorKind::InvalidData {
                self.stamp = None;
            }
        }
        if after != stamp {
            self.records.clear();
            self.stamp = None;
        }
    }

    fn supplement_captured(
        &mut self,
        events: &[UsageEvent],
        aliases: &BTreeMap<String, String>,
        cache: &provider_cache::ProviderCache,
    ) -> (usize, bool) {
        if let Some(start) = self.captured_start.take() {
            self.records.truncate(start);
        }
        self.captured_complete = Some(HashSet::new());
        let aliases = RequestSessionAliases::new(aliases);
        let mut source_aliases: HashMap<String, Vec<String>> = HashMap::new();
        for alias in aliases.canonical.keys() {
            if let Some(target) = aliases.resolve(alias) {
                if &target != alias {
                    source_aliases
                        .entry(target)
                        .or_default()
                        .push(alias.clone());
                }
            }
        }
        let mut targets: BTreeMap<String, Vec<(i64, [u8; 32])>> = BTreeMap::new();
        for event in events.iter().filter(|e| unknown_provider(&e.provider)) {
            let Some(session) = aliases.resolve(&event.session) else {
                continue;
            };
            let Ok(at) = DateTime::parse_from_rfc3339(&event.timestamp) else {
                continue;
            };
            let at = at.timestamp_millis();
            targets.entry(session).or_default().push((at, event.id));
        }
        if targets.is_empty() {
            return (0, false);
        }
        // refresh 每轮丢弃旧补充；文件 stamp 不承担完成缓存的更新通知。
        self.captured_start = Some(self.records.len());
        let mut observed: HashMap<_, _> = self
            .records
            .iter()
            .map(|r| (r.id.clone(), r.clone()))
            .collect();
        let mut recovered = 0;
        let mut limited = targets.len() > 256;
        let targets: Vec<_> = targets.into_iter().collect();
        let offset = self.captured_cursor % targets.len();
        let count = targets.len().min(256);
        self.captured_cursor = offset + count;
        for (session, events) in targets.iter().cycle().skip(offset).take(count) {
            let since = events
                .iter()
                .map(|(at, _)| *at)
                .min()
                .unwrap()
                .saturating_sub(DELAYED_REQUEST_MATCH_WINDOW_MS);
            let until = events
                .iter()
                .map(|(at, _)| *at)
                .max()
                .unwrap()
                .saturating_add(DELAYED_REQUEST_MATCH_WINDOW_MS);
            let mut sources = source_aliases.remove(session).unwrap_or_default();
            sources.push(session.clone());
            sources.sort();
            if sources.len() > 16 {
                limited = true;
                continue;
            }
            let mut rows = Vec::new();
            let mut complete = true;
            let mut floor = 0;
            for source in sources {
                match cache.captured_proxy(&source, since, until) {
                    Ok(batch) => {
                        complete &= batch.complete;
                        floor = floor.max(batch.floor);
                        rows.extend(batch.rows);
                    }
                    Err(_) => {
                        complete = false;
                        break;
                    }
                }
            }
            // 一个别名的截断边界同样约束整组；不让更早的部分窗口推断唯一性。
            rows.retain(|row| row.timestamp_ms >= floor);
            let additional = rows
                .iter()
                .filter(|row| observed.get(&row.id) != Some(row))
                .count();
            if self.records.len().saturating_add(additional) > MAX_ROWS_CAPTURED_PROXY {
                limited = true;
                continue;
            }
            for row in rows {
                if observed.get(&row.id) == Some(&row) {
                    continue;
                }
                observed.insert(row.id.clone(), row.clone());
                self.records.push(row);
                recovered += 1;
            }
            for (at, id) in events {
                if complete && at.saturating_sub(DELAYED_REQUEST_MATCH_WINDOW_MS) >= floor {
                    self.captured_complete.as_mut().unwrap().insert(*id);
                } else {
                    limited = true;
                }
            }
        }
        (recovered, limited)
    }

    fn attribute_with_cache(
        &mut self,
        events: &mut [UsageEvent],
        names: &mut BTreeMap<String, String>,
        aliases: &BTreeMap<String, String>,
        cache: &provider_cache::ProviderCache,
    ) -> Value {
        let checked = cache.rejected_proxy_ids(
            self.records
                .iter()
                .chain(self.verified.values().map(|saved| &saved.request)),
        );
        self.captured_id_check_failed = checked.is_err();
        self.captured_rejected = checked.unwrap_or_default();
        self.attribute(events, names, aliases)
    }

    fn attribute(
        &mut self,
        events: &mut [UsageEvent],
        names: &mut BTreeMap<String, String>,
        aliases: &BTreeMap<String, String>,
    ) -> Value {
        let aliases = RequestSessionAliases::new(aliases);
        let cutoff = provider_cache::ProviderCache::since(
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        );
        // 暂时不可读的原文件恢复后仍需补证；不会用这些证据生成任何消费记录。
        self.verified.retain(|_, saved| saved.at >= cutoff);
        let mut requests = BTreeMap::new();
        let mut blocked = self.captured_rejected.clone();
        for record in &self.records {
            if record.id.is_empty() || record.id.len() > 256 {
                continue;
            }
            if record.keys().is_empty()
                || requests.insert(record.id.clone(), record.clone()).is_some()
            {
                blocked.insert(record.id.clone());
            }
        }
        let current: HashSet<_> = requests.keys().cloned().collect();
        for saved in self.verified.values() {
            if let Some(record) = requests.get(&saved.request.id) {
                if record != &saved.request {
                    blocked.insert(saved.request.id.clone());
                }
            } else {
                requests.insert(saved.request.id.clone(), saved.request.clone());
            }
        }
        requests.retain(|id, _| !blocked.contains(id));
        let requests: Vec<_> = requests.into_values().collect();
        let request_keys: Vec<Vec<_>> = requests
            .iter()
            .map(|request| {
                request
                    .keys()
                    .into_iter()
                    .filter_map(|mut key| {
                        key.session = aliases.resolve(&key.session)?;
                        Some(key)
                    })
                    .collect()
            })
            .collect();
        let mut request_index: HashMap<RequestMatchKey, Vec<(i64, usize)>> = HashMap::new();
        for (i, request) in requests.iter().enumerate() {
            for key in &request_keys[i] {
                request_index
                    .entry(key.clone())
                    .or_default()
                    .push((request.timestamp_ms, i));
            }
        }
        for entries in request_index.values_mut() {
            entries.sort_unstable();
        }
        let event_keys: Vec<_> = events
            .iter()
            .map(|event| {
                let at = DateTime::parse_from_rfc3339(&event.timestamp)
                    .ok()?
                    .timestamp_millis();
                if event.session.is_empty() || event.model.is_empty() {
                    return None;
                }
                Some((
                    RequestMatchKey {
                        session: aliases.resolve(&event.session)?,
                        model: request_base_model(&event.model).to_owned(),
                        input: event.delta.input_tokens,
                        output: event.delta.output_tokens,
                        cached: event.delta.cached_input_tokens,
                    },
                    at,
                ))
            })
            .collect();
        // 已知供应商也参与反向计数；按键/时间二分，避免事件数乘日志数。
        let mut event_index: HashMap<&RequestMatchKey, Vec<i64>> = HashMap::new();
        for (key, at) in event_keys.iter().flatten() {
            event_index.entry(key).or_default().push(*at);
        }
        for times in event_index.values_mut() {
            times.sort_unstable();
        }
        let reverse_counts = |window: i64| {
            requests
                .iter()
                .zip(&request_keys)
                .map(|(request, keys)| {
                    keys.iter()
                        .filter_map(|key| event_index.get(key))
                        .map(|times| {
                            times.partition_point(|at| {
                                *at <= request.timestamp_ms.saturating_add(window)
                            }) - times.partition_point(|at| {
                                *at < request.timestamp_ms.saturating_sub(window)
                            })
                        })
                        .sum()
                })
                .collect::<Vec<usize>>()
        };
        let reverse = reverse_counts(REQUEST_MATCH_WINDOW_MS);
        let delayed_reverse = reverse_counts(DELAYED_REQUEST_MATCH_WINDOW_MS);
        let (mut recovered, mut tokens, mut retained) = (0usize, 0u64, 0usize);
        let (mut delayed_events, mut delayed_tokens) = (0usize, 0u64);
        for (event, evidence) in events.iter_mut().zip(&event_keys) {
            if self.captured_id_check_failed {
                continue;
            }
            let candidate = evidence.as_ref().and_then(|(key, at)| {
                let entries = request_index.get(key)?;
                let start = entries.partition_point(|(time, _)| {
                    *time < at.saturating_sub(REQUEST_MATCH_WINDOW_MS)
                });
                let end = entries.partition_point(|(time, _)| {
                    *time <= at.saturating_add(REQUEST_MATCH_WINDOW_MS)
                });
                if end > start {
                    return (end - start == 1)
                        .then(|| entries[start].1)
                        .filter(|i| reverse[*i] == 1)
                        .map(|i| (i, false));
                }
                // 延迟补认只接受请求结束之后的记录；扩大双向查重范围，不能抢用其他事件的请求。
                let start = entries.partition_point(|(time, _)| {
                    *time < at.saturating_sub(DELAYED_REQUEST_MATCH_WINDOW_MS)
                });
                let end = entries.partition_point(|(time, _)| {
                    *time <= at.saturating_add(DELAYED_REQUEST_MATCH_WINDOW_MS)
                });
                if end - start != 1 {
                    return None;
                }
                let i = entries[start].1;
                let request = &requests[i];
                (request.timestamp_ms < *at
                    && request.attempts == Some(1)
                    && request.auto_retries == Some(0)
                    && delayed_reverse[i] == 1)
                    .then_some((i, true))
            });
            if let Some(saved) = self.verified.get_mut(&event.id) {
                let mut saved_key = saved.key.clone();
                let saved_session = aliases.resolve(&saved_key.session);
                if let Some(session) = &saved_session {
                    saved_key.session.clone_from(session);
                }
                if saved_session.is_none()
                    || evidence
                        .as_ref()
                        .is_none_or(|(key, at)| *key != saved_key || *at != saved.at)
                    || candidate.is_none_or(|(i, _)| requests[i] != saved.request)
                {
                    saved.rejected = true;
                }
            }
            if !unknown_provider(&event.provider) {
                continue;
            }
            let Some((i, delayed)) = candidate else {
                continue;
            };
            if self
                .verified
                .get(&event.id)
                .is_some_and(|saved| saved.rejected)
            {
                continue;
            }
            if !self.verified.contains_key(&event.id)
                && self
                    .captured_complete
                    .as_ref()
                    .is_some_and(|complete| !complete.contains(&event.id))
            {
                continue;
            }
            let request = &requests[i];
            if !self.verified.contains_key(&event.id) && self.verified.len() < MAX_EVENTS {
                let (key, at) = evidence.as_ref().unwrap();
                self.verified.insert(
                    event.id,
                    VerifiedAttribution {
                        key: key.clone(),
                        at: *at,
                        request: request.clone(),
                        rejected: false,
                    },
                );
            }
            event.provider = request.relay_id.clone();
            names
                .entry(request.relay_id.clone())
                .or_insert_with(|| request.relay_name.clone());
            recovered += 1;
            tokens = tokens.saturating_add(event.delta.total_tokens);
            if delayed {
                delayed_events += 1;
                delayed_tokens = delayed_tokens.saturating_add(event.delta.total_tokens);
            }
            retained += usize::from(!current.contains(&request.id));
        }
        json!({"source":"request-log-exact","scope":"index","recoveredEvents":recovered,"recoveredTokens":tokens,
            "delayedEvents":delayed_events,"delayedTokens":delayed_tokens,
            "strictWindowMs":REQUEST_MATCH_WINDOW_MS,"delayedWindowMs":DELAYED_REQUEST_MATCH_WINDOW_MS,
            "retainedEvents":retained,"conflictedEvents":self.verified.values().filter(|saved| saved.rejected).count(),
            "logReads":self.reads,"logBytesRead":self.bytes_read})
    }
}

#[derive(Clone, PartialEq, Eq)]
struct LegacyIdentity {
    id: [u8; 32],
    session: String,
    role: String,
}

#[derive(Clone, PartialEq, Eq)]
struct UsageEvent {
    id: [u8; 32],
    session: String,
    turn: String,
    model: String,
    timestamp: String,
    date: NaiveDate,
    total: Tokens,
    delta: Tokens,
    has_file_baseline: bool,
    provider: String,
    role: String,
    inherited: bool,
    legacy_identity: Option<LegacyIdentity>,
}

#[derive(Clone)]
struct Call {
    timestamp: String,
    session: String,
    payload: Value,
}

#[derive(Clone)]
struct ToolRecord {
    id: String,
    call: Call,
    output: Value,
    date: NaiveDate,
}

struct ExecSkill {
    call: Call,
    evidence: String,
}

#[derive(Default)]
struct InheritedUsageContext {
    session: String,
    model: String,
    turn: String,
    provider: String,
    role: String,
    end: u64,
    prefix: Option<(String, u64)>,
    ambiguous: bool,
}

#[derive(Default)]
struct FileState {
    stamp: Option<(u64, Option<SystemTime>, Option<(u64, u64)>)>,
    offset: u64,
    reader: Option<index_budget::ReadCursor>,
    retained_charge: usize,
    pending: Vec<u8>,
    skipping: bool,
    session: String,
    model: String,
    turn: String,
    previous: Option<Tokens>,
    events: Vec<UsageEvent>,
    calls: HashMap<String, Call>,
    exec_skills: HashMap<String, ExecSkill>,
    exec_polls: HashMap<String, String>,
    tools: Vec<ToolRecord>,
    issues: usize,
    bytes_read: u64,
    provider: String,
    role: String,
    history_boundary: super::UsageHistoryBoundary,
    inherited_contexts: Vec<InheritedUsageContext>,
    legacy_header: Option<(String, String)>,
    scope_parent_candidates: BTreeMap<String, HashSet<Option<String>>>,
}

#[derive(Default)]
struct Index {
    files: BTreeMap<PathBuf, FileState>,
    provider_stamp: Option<(u64, Option<SystemTime>)>,
    provider_names: BTreeMap<String, String>,
    provider_endpoints: BTreeMap<String, Option<String>>,
    provider_endpoint_since: i64,
    request_evidence: RequestEvidenceCache,
    native_evidence: native_endpoints::Cache,
    provider_evidence: ProviderEvidenceCache,
    provider_cache: provider_cache::ProviderCache,
}

#[derive(Default)]
struct Snapshot {
    live_requests: Vec<RequestEvidence>,
    events: Vec<UsageEvent>,
    tools: Vec<ToolRecord>,
    sessions: HashSet<String>,
    aliases: BTreeMap<String, String>,
    cursors: BTreeMap<String, (String, Tokens)>,
    files: usize,
    compressed: usize,
    indexed: usize,
    errors: usize,
    issues: usize,
    bytes_read: u64,
    elapsed_ms: u128,
    ready: bool,
    activity_truncated: bool,
    updated_at: i64,
    provider_names: BTreeMap<String, String>,
    session_info: BTreeMap<String, Value>,
    scope_parents: BTreeMap<String, String>,
    provider_recovery: Value,
    provider_evidence: Value,
}

struct Cache {
    index: Option<Index>,
    snapshot: Arc<Snapshot>,
    active: bool,
    refreshed: Option<Instant>,
}

pub(super) fn query(home: &Path, options: &Query, request: &Value) -> Value {
    let scope_session = match request.get("scopeSessionId") {
        None => None,
        Some(value) => {
            let Some(id) = value.as_str().and_then(super::normalize_session_id) else {
                return json!({"status":"unavailable","complete":false,"message":"会话范围 ID 无效"});
            };
            Some(id)
        }
    };
    if !request["startAt"].is_null() || !request["endAt"].is_null() {
        if request["startAt"]
            .as_i64()
            .zip(request["endAt"].as_i64())
            .is_none_or(|(start, end)| start >= end)
        {
            return json!({"status":"unavailable","complete":false,"message":"用量时间范围无效"});
        }
    }
    static HOMES: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<Cache>>>>> = OnceLock::new();
    let Ok(mut homes) = HOMES.get_or_init(Default::default).lock() else {
        return json!({"status":"unavailable","complete":false});
    };
    if !homes.contains_key(home) && homes.len() >= 2 {
        // 只回收空闲索引；运行中的扫描不能因另一个 home 查询而被反复创建。
        let idle = homes
            .iter()
            .find(|(_, cache)| cache.lock().is_ok_and(|c| !c.active))
            .map(|(path, _)| path.clone());
        if let Some(idle) = idle {
            homes.remove(&idle);
        } else {
            return json!({"status":"unavailable","complete":false});
        }
    }
    let cache = homes
        .entry(home.to_owned())
        .or_insert_with(|| {
            Arc::new(Mutex::new(Cache {
                index: Some(Index::default()),
                snapshot: Arc::new(Snapshot::default()),
                active: false,
                refreshed: None,
            }))
        })
        .clone();
    drop(homes);
    let Ok(mut state) = cache.lock() else {
        return json!({"status":"unavailable","complete":false});
    };
    if !state.active
        && state
            .refreshed
            .is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL)
    {
        let mut index = state.index.take().unwrap_or_default();
        state.active = true;
        let shared = cache.clone();
        let home = home.to_owned();
        if let Err(error) = std::thread::Builder::new()
            .name("rollout-history".into())
            .spawn(move || {
                let snapshot =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| index.update(&home)));
                if let Ok(mut state) = shared.lock() {
                    if let Ok(snapshot) = snapshot {
                        state.snapshot = Arc::new(snapshot);
                    }
                    state.index = Some(index);
                    state.active = false;
                    state.refreshed = Some(Instant::now());
                }
            })
        {
            state.active = false;
            state.refreshed = Some(Instant::now());
            return json!({"status":"unavailable","complete":false,"message":error.to_string()});
        }
    }
    let snapshot = state.snapshot.clone();
    let processing = state.active;
    drop(state);
    let mut response = snapshot.response_page(
        options,
        processing,
        request["activityPage"].as_u64().unwrap_or(1).clamp(1, 24) as usize,
        request["activityRevision"].as_i64(),
        request["startAt"].as_i64().zip(request["endAt"].as_i64()),
    );
    if let Some(session) = scope_session {
        response["scopedUsage"] = snapshot.scoped_usage(
            &session,
            options,
            request["startAt"].as_i64().zip(request["endAt"].as_i64()),
        );
    }
    response["liveAttributions"] = live_receipts::match_receipts(
        &snapshot.live_requests,
        &snapshot.aliases,
        &request["liveReceipts"],
    );
    response
}

fn log_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.starts_with("rollout-")
                && (name.ends_with(".jsonl") || name.ends_with(".jsonl.zst"))
        })
}

fn discover(home: &Path, files: &mut Vec<PathBuf>, errors: &mut usize) {
    let mut pending = vec![home.join("sessions"), home.join("archived_sessions")];
    while let Some(directory) = pending.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                *errors += 1;
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else {
                *errors += 1;
                continue;
            };
            let Ok(kind) = entry.file_type() else {
                *errors += 1;
                continue;
            };
            if kind.is_dir() {
                // 不跟随符号链接；年份/月/日目录是预期的最深层级。
                if entry
                    .path()
                    .strip_prefix(home)
                    .is_ok_and(|p| p.components().count() <= 4)
                {
                    pending.push(entry.path());
                }
            } else if kind.is_file() && log_file(&entry.path()) {
                if files.len() >= MAX_FILES {
                    *errors += 1;
                    continue;
                }
                match fs::canonicalize(entry.path()) {
                    Ok(path) if path.starts_with(home) => files.push(path),
                    _ => *errors += 1,
                }
            }
        }
    }
    files.sort();
}

impl Index {
    fn update(&mut self, home: &Path) -> Snapshot {
        self.update_with_request_log(home, &codex_plus_core::request_log::request_log_path())
    }

    fn update_with_request_log(&mut self, home: &Path, request_log: &Path) -> Snapshot {
        let started = Instant::now();
        let cutoff = Local::now().date_naive() - Days::days(RETENTION_DAYS - 1);
        let mut snapshot = Snapshot::default();
        let mut cache_available = true;
        match self.provider_cache.open(home, cutoff) {
            Ok(true) => {
                // refresh 会在路径变化时清空请求缓存，必须先建立路径再恢复落盘证据。
                self.request_evidence.refresh(request_log);
                match self.provider_cache.load_verified(cutoff) {
                    Ok(records) => self.request_evidence.restore_verified(records),
                    Err(_) => cache_available = false,
                }
                match self.provider_cache.load_native(cutoff) {
                    Ok(records) => self.native_evidence.verified = records,
                    Err(_) => cache_available = false,
                }
            }
            Ok(false) => {}
            Err(_) => cache_available = false,
        }
        snapshot.aliases = self.provider_cache.aliases.clone();
        let mut current_aliases = BTreeMap::new();
        let settings = codex_plus_core::paths::default_settings_path();
        if let Ok(metadata) = fs::metadata(&settings) {
            let stamp = (metadata.len(), metadata.modified().ok());
            if self.provider_stamp != Some(stamp) {
                self.provider_endpoints.clear();
                if let Ok(profiles) = native_endpoints::profiles(&settings) {
                    self.provider_endpoints = native_endpoints::endpoints(&profiles);
                    self.provider_endpoint_since = self
                        .provider_cache
                        .endpoint_binding_since(&self.provider_endpoints)
                        .unwrap_or_else(|_| Local::now().timestamp_millis());
                    self.provider_names = profiles
                        .into_iter()
                        .map(|name| (name.id, name.name))
                        .collect();
                    self.provider_stamp = Some(stamp);
                }
            }
        } else {
            self.provider_endpoints.clear();
            self.provider_stamp = None;
        }
        snapshot.provider_names = self.provider_names.clone();
        let mut paths = Vec::new();
        discover(home, &mut paths, &mut snapshot.errors);
        let existing: HashSet<_> = paths.iter().cloned().collect();
        self.files.retain(|path, _| existing.contains(path));
        let mut budget = index_budget::Budget::new(self.files.values());
        let mut seen = HashMap::new();
        let mut tools = BTreeMap::new();
        for path in paths {
            snapshot.files += 1;
            snapshot.compressed += usize::from(path.extension().is_some_and(|ext| ext == "zst"));
            let state = self.files.entry(path.clone()).or_default();
            state.bytes_read = 0;
            let previous_usage = index_budget::Usage::of(state);
            budget.replace(previous_usage, index_budget::Usage::default());
            match state.read_with_budget(&path, cutoff, &mut budget) {
                Err(_) => {
                    snapshot.errors += 1;
                    // 下次从有效文件头重试，不能让失败的解压流留下错误累计基线。
                    *state = FileState::default();
                }
                Ok(true) => snapshot.indexed += 1,
                Ok(false) => snapshot.errors += 1,
            }
            budget.replace(
                index_budget::Usage::default(),
                index_budget::Usage::of(state),
            );
            snapshot.issues += state.issues;
            snapshot.bytes_read += state.bytes_read;
            if !state.session.is_empty() {
                snapshot.sessions.insert(state.session.clone());
                let info = snapshot
                    .session_info
                    .entry(state.session.clone())
                    .or_insert_with(|| {
                        json!({
                            "model":"","provider":"","role":state.role
                        })
                    });
                if !state.model.is_empty() || info["model"].as_str().unwrap_or("").is_empty() {
                    *info = json!({
                    "model":state.model,"provider":usage_provider(&state.model, &state.provider, &self.provider_names),
                    "role":state.role
                    });
                }
                if let Some(name) =
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .and_then(|name| {
                            name.strip_suffix(".zst")
                                .unwrap_or(name)
                                .strip_suffix(".jsonl")
                        })
                {
                    if let Some(id) = name
                        .get(name.len().saturating_sub(36)..)
                        .and_then(super::normalize_session_id)
                    {
                        snapshot
                            .aliases
                            .entry(id.clone())
                            .and_modify(|previous| {
                                if *previous != state.session {
                                    *previous = id.clone();
                                }
                            })
                            .or_insert_with(|| state.session.clone());
                        current_aliases.insert(id.clone(), snapshot.aliases[&id].clone());
                    }
                }
            }
            for event in &state.events {
                if event.date < cutoff {
                    continue;
                }
                if let Some(&position) = seen.get(&event.id) {
                    let previous: &mut UsageEvent = &mut snapshot.events[position];
                    if previous.inherited && !event.inherited {
                        *previous = event.clone();
                    }
                    continue;
                }
                if snapshot.events.len() == MAX_EVENTS {
                    snapshot.issues += 1;
                    break;
                }
                let cursor = snapshot
                    .cursors
                    .entry(event.session.clone())
                    .or_insert_with(|| (event.timestamp.clone(), event.total));
                if event.timestamp >= cursor.0 {
                    *cursor = (event.timestamp.clone(), event.total);
                }
                seen.insert(event.id, snapshot.events.len());
                snapshot.events.push(event.clone());
            }
            for tool in &state.tools {
                if tool.date >= cutoff {
                    // 同一调用在复制/迁移日志里只保留一个完成回执。
                    tools.insert((tool.call.session.clone(), tool.id.clone()), tool.clone());
                }
            }
        }
        // 真正有独立头部的会话不是另一个会话的别名。
        for state in self
            .files
            .values()
            .filter(|state| !state.session.is_empty())
        {
            snapshot
                .aliases
                .insert(state.session.clone(), state.session.clone());
            current_aliases.insert(state.session.clone(), state.session.clone());
        }
        snapshot.collect_scope_parents(self.files.values());
        snapshot.tools = tools.into_values().collect();
        // 文件分段共享会话累计基线；同模型不同会话（包括子智能体）仍独立计量。
        snapshot.events.sort_by(|a, b| {
            a.session
                .cmp(&b.session)
                .then(a.timestamp.cmp(&b.timestamp))
        });
        let mut baselines: HashMap<String, Tokens> = HashMap::new();
        let mut known_models: HashMap<String, String> = snapshot
            .session_info
            .iter()
            .filter_map(|(id, info)| {
                info["model"]
                    .as_str()
                    .filter(|model| !model.is_empty())
                    .map(|model| (id.clone(), model.to_owned()))
            })
            .collect();
        for event in snapshot.events.iter().rev() {
            if !event.model.is_empty() {
                known_models.insert(event.session.clone(), event.model.clone());
            }
        }
        for event in &mut snapshot.events {
            if event.model.is_empty() {
                event.model = known_models
                    .get(&event.session)
                    .cloned()
                    .unwrap_or_default();
            } else {
                known_models.insert(event.session.clone(), event.model.clone());
            }
            event.provider =
                usage_provider(&event.model, &event.provider, &self.provider_names).to_owned();
            event.delta = match baselines.insert(event.session.clone(), event.total) {
                // 缓存/推理子计数缺失或修正不代表新增消费，不能重放 last_usage。
                Some(previous)
                    if event.total.input_tokens == previous.input_tokens
                        && event.total.output_tokens == previous.output_tokens =>
                {
                    Tokens::default()
                }
                Some(previous) if event.total.is_monotonic_after(previous) => {
                    let merged = event.total.delta(previous);
                    // 同会话副本可能使用不同累计基线；合并不能放大文件内已经确认的增量。
                    if !event.has_file_baseline || merged.total_tokens <= event.delta.total_tokens {
                        merged
                    } else {
                        event.delta
                    }
                }
                // 保留文件内已有的基线计算，避免保留窗口截断后把首条当整次请求。
                _ => event.delta,
            };
        }
        snapshot.events.retain(|event| event.delta.total_tokens > 0);
        self.request_evidence.refresh(request_log);
        let captured = self.request_evidence.supplement_captured(
            &snapshot.events,
            &snapshot.aliases,
            &self.provider_cache,
        );
        snapshot.live_requests = self.request_evidence.records.clone();
        self.request_evidence.supplement_missing_session_models(
            &mut snapshot.session_info,
            Local::now().timestamp_millis(),
        );
        snapshot.provider_recovery = self.request_evidence.attribute_with_cache(
            &mut snapshot.events,
            &mut snapshot.provider_names,
            &snapshot.aliases,
            &self.provider_cache,
        );
        snapshot.provider_recovery["capturedProxyRequestsRead"] = json!(captured.0);
        snapshot.provider_recovery["capturedProxyLimited"] = json!(captured.1);
        if let Some(parent) = request_log.parent() {
            self.provider_evidence
                .refresh(&parent.join("usage-provider-evidence.json"));
            snapshot.provider_evidence = self
                .provider_evidence
                .attribute(&mut snapshot.events, &mut snapshot.provider_names);
        }
        snapshot.provider_recovery["nativeEndpoint"] = self.native_evidence.attribute(
            home,
            &mut snapshot.events,
            &self.provider_endpoints,
            self.provider_endpoint_since,
            &snapshot.aliases,
            &self.provider_cache,
        );
        // 精确事件证据供尚未合并的实时回执使用；不把最后供应商推广到整段会话。
        let now = Local::now().timestamp_millis();
        let mut recent: Vec<_> = snapshot
            .events
            .iter()
            .filter(|event| {
                !unknown_provider(&event.provider)
                    && DateTime::parse_from_rfc3339(&event.timestamp).is_ok_and(|at| {
                        (0..=600_000).contains(&now.saturating_sub(at.timestamp_millis()))
                    })
            })
            .collect();
        recent.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        let mut per_session = HashMap::new();
        for event in recent.into_iter().take(128) {
            let count = per_session.entry(&event.session).or_insert(0usize);
            if *count >= 16 {
                continue;
            }
            *count += 1;
            let info = snapshot
                .session_info
                .entry(event.session.clone())
                .or_insert_with(|| json!({}));
            if !info["verifiedUsage"].is_array() {
                info["verifiedUsage"] = json!([]);
            }
            info["verifiedUsage"].as_array_mut().unwrap().push(json!({
                "timestamp":event.timestamp,"model":event.model,"provider":event.provider,
                "role":event.role,"total":event.total,"delta":event.delta
            }));
        }
        let writes = self
            .provider_cache
            .save(&self.request_evidence.verified, &current_aliases);
        cache_available &= writes.is_ok();
        let native_writes = self
            .provider_cache
            .save_native(&self.native_evidence.verified);
        cache_available &= native_writes.is_ok();
        snapshot.provider_recovery["nativeCacheWrittenRecords"] = json!(native_writes.unwrap_or(0));
        snapshot.provider_recovery["cacheAvailable"] = json!(cache_available);
        snapshot.provider_recovery["cacheWrittenRecords"] = json!(writes.unwrap_or(0));
        snapshot.tools.sort_by(|a, b| {
            a.call
                .timestamp
                .cmp(&b.call.timestamp)
                .then(a.id.cmp(&b.id))
        });
        if snapshot.tools.len() > MAX_ACTIVITY {
            snapshot.activity_truncated = true;
            snapshot.tools.drain(..snapshot.tools.len() - MAX_ACTIVITY);
        }
        snapshot.ready = true;
        snapshot.updated_at = Local::now().timestamp_millis();
        snapshot.elapsed_ms = started.elapsed().as_millis();
        snapshot
    }
}

impl FileState {
    fn inherited_context(&mut self, ordinal: u64) -> &mut InheritedUsageContext {
        if self.inherited_contexts.is_empty() {
            self.inherited_contexts.push(InheritedUsageContext {
                session: self.history_boundary.parent.clone().unwrap(),
                end: self.history_boundary.start.unwrap(),
                ..InheritedUsageContext::default()
            });
        }
        while self.inherited_contexts.len() > 1
            && ordinal >= self.inherited_contexts.last().unwrap().end
        {
            self.inherited_contexts.pop();
        }
        loop {
            let context = self.inherited_contexts.last().unwrap();
            let Some((parent, end)) = context.prefix.as_ref().filter(|(_, end)| ordinal < *end)
            else {
                break;
            };
            if context.ambiguous
                || self.inherited_contexts.len() >= 32
                || *end > context.end
                || parent == &self.session
                || self
                    .inherited_contexts
                    .iter()
                    .any(|context| &context.session == parent)
            {
                self.inherited_contexts.last_mut().unwrap().ambiguous = true;
                break;
            }
            self.inherited_contexts.push(InheritedUsageContext {
                session: parent.clone(),
                end: *end,
                ..InheritedUsageContext::default()
            });
        }
        self.inherited_contexts.last_mut().unwrap()
    }

    #[cfg(test)]
    fn read(&mut self, path: &Path, cutoff: NaiveDate) -> io::Result<()> {
        let mut budget = index_budget::Budget::new(std::iter::empty());
        self.read_with_budget(path, cutoff, &mut budget).map(|_| ())
    }

    fn read_with_budget(
        &mut self,
        path: &Path,
        cutoff: NaiveDate,
        budget: &mut index_budget::Budget,
    ) -> io::Result<bool> {
        let before = fs::symlink_metadata(path)?;
        if !before.is_file() || fs::canonicalize(path)? != path {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "rollout path changed",
            ));
        }
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        if file_identity(&metadata) != file_identity(&before) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "rollout identity changed",
            ));
        }
        let stamp = (
            metadata.len(),
            metadata.modified().ok(),
            Some(codex_plus_core::codex_sqlite::opened_file_identity(&file)?),
        );
        let compressed = path.extension().is_some_and(|ext| ext == "zst");
        self.events.retain(|event| event.date >= cutoff);
        self.tools.retain(|tool| tool.date >= cutoff);
        self.exec_skills.retain(|_, pending| {
            DateTime::parse_from_rfc3339(&pending.call.timestamp)
                .is_ok_and(|time| time.with_timezone(&Local).date_naive() >= cutoff)
        });
        self.exec_polls
            .retain(|_, session| self.exec_skills.contains_key(session));
        let mut meter = index_budget::Meter::new(self);
        let cursor_bytes = if self.reader.is_some() {
            index_budget::CURSOR_BYTES
        } else {
            0
        };
        meter.refresh(self, cursor_bytes);
        if self.stamp == Some(stamp) && self.reader.is_none() {
            return Ok(self.pending.is_empty() && !self.skipping);
        }
        if self.stamp.is_some_and(|old| {
            old != stamp
                && (compressed
                    || old.0 > stamp.0
                    || old.2 != stamp.2
                    || old.0 == stamp.0 && old.1 != stamp.1)
        }) {
            *self = Self::default();
            meter = index_budget::Meter::new(self);
            meter.refresh(self, 0);
        }
        if self.reader.is_none() {
            if !budget.can_open(self) {
                return Ok(false);
            }
            if !compressed {
                file.seek(SeekFrom::Start(self.offset))?;
            }
            self.reader = Some(index_budget::ReadCursor::new(file, compressed)?);
            self.retained_charge += index_budget::CURSOR_BYTES;
        }
        self.stamp = Some(stamp);
        let mut cursor = self.reader.take().unwrap();
        cursor.set_allowance(budget.source_left);
        let result = (|| -> io::Result<bool> {
            let input = &mut cursor.input;
            loop {
                if self.pending.last() == Some(&b'\n') {
                    if !budget.accept_line(self) {
                        return Ok(false);
                    }
                    let line = std::mem::take(&mut self.pending);
                    self.accept(&line, cutoff);
                    // 不长期保留巨大单行的容量。
                    if line.capacity() < 256 * 1024 {
                        self.pending = line;
                        self.pending.clear();
                    }
                    self.skipping = false;
                    meter.refresh(self, index_budget::CURSOR_BYTES);
                }
                if !budget.can_read(self) {
                    return Ok(false);
                }
                let bytes = input.fill_buf()?;
                if bytes.is_empty() {
                    if compressed && !self.pending.is_empty() {
                        if !budget.accept_line(self) {
                            return Ok(false);
                        }
                        let line = std::mem::take(&mut self.pending);
                        self.accept(&line, cutoff);
                        meter.refresh(self, index_budget::CURSOR_BYTES);
                    }
                    return Ok(self.pending.is_empty() && !self.skipping);
                }
                let length = bytes
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(bytes.len(), |pos| pos + 1)
                    .min(budget.read_limit(self));
                if length == 0 {
                    return Ok(false);
                }
                let part = &bytes[..length];
                let complete = part.last() == Some(&b'\n');
                let pending_capacity = self.pending.capacity();
                if !self.skipping {
                    if self.pending.len() + part.len() <= MAX_LINE {
                        self.pending.extend_from_slice(part);
                    } else {
                        self.pending.clear();
                        self.skipping = true;
                        self.issues += 1;
                    }
                }
                self.offset += length as u64;
                self.bytes_read += length as u64;
                budget.charge_read(length);
                self.retained_charge =
                    self.retained_charge.saturating_sub(pending_capacity) + self.pending.capacity();
                if complete && self.skipping {
                    self.skipping = false;
                }
                input.consume(length);
            }
        })();
        budget.source_left = cursor.allowance();
        match result {
            // WouldBlock 是输入预算耗尽，不是损坏流；保留解压器内部状态供下轮续读。
            Err(ref error) if error.kind() == io::ErrorKind::WouldBlock => {
                self.reader = Some(cursor);
                Ok(false)
            }
            Ok(false) => {
                self.reader = Some(cursor);
                Ok(false)
            }
            other => {
                meter.refresh(self, 0);
                other
            }
        }
    }

    fn accept(&mut self, line: &[u8], cutoff: NaiveDate) {
        #[derive(Deserialize)]
        struct Envelope<'a> {
            #[serde(rename = "type")]
            kind: &'a str,
            timestamp: Option<&'a str>,
            ordinal: Option<i64>,
            #[serde(borrow)]
            payload: &'a RawValue,
        }
        let Ok(envelope) = serde_json::from_slice::<Envelope>(line) else {
            if !line.iter().all(u8::is_ascii_whitespace) {
                self.issues += 1;
            }
            return;
        };
        if !matches!(
            envelope.kind,
            "session_meta" | "turn_context" | "event_msg" | "response_item"
        ) {
            return;
        }
        // 忽略消息/推理正文，而不是把多 MB 的文本分配成 Value。
        if envelope.kind == "response_item" {
            #[derive(Deserialize)]
            struct Item<'a> {
                #[serde(rename = "type")]
                kind: &'a str,
                call_id: Option<&'a str>,
            }
            let Ok(item) = serde_json::from_str::<Item>(envelope.payload.get()) else {
                return;
            };
            if !matches!(
                item.kind,
                "function_call"
                    | "custom_tool_call"
                    | "function_call_output"
                    | "custom_tool_call_output"
            ) {
                return;
            }
            if item.kind.ends_with("_output")
                && item.call_id.is_none_or(|id| {
                    !self.calls.contains_key(id) && !self.exec_polls.contains_key(id)
                })
            {
                return;
            }
        } else if envelope.kind == "event_msg" {
            #[derive(Deserialize)]
            struct Event<'a> {
                #[serde(rename = "type")]
                kind: &'a str,
            }
            let Ok(event) = serde_json::from_str::<Event>(envelope.payload.get()) else {
                return;
            };
            if event.kind != "token_count"
                && !event.kind.starts_with("thread_settings")
                && !matches!(event.kind, "task_started" | "turn_started")
            {
                return;
            }
        }
        let Ok(payload) = serde_json::from_str::<Value>(envelope.payload.get()) else {
            self.issues += 1;
            return;
        };
        let ordinal = envelope.ordinal.and_then(|n| u64::try_from(n).ok());
        let inherited = self.history_boundary.contains(ordinal);
        if envelope.kind == "session_meta" {
            let id = payload["id"].as_str().and_then(super::normalize_session_id);
            let role = if payload["source"].get("subagent").is_some()
                || payload["source"]
                    .as_str()
                    .is_some_and(|s| s.to_ascii_lowercase().contains("subagent"))
            {
                "subagent"
            } else {
                "main"
            };
            if inherited {
                if let Some(id) = &id {
                    self.legacy_header = Some((id.clone(), role.into()));
                }
                let context = self.inherited_context(ordinal.unwrap());
                if id.as_ref() != Some(&context.session) {
                    context.ambiguous = true;
                    return;
                }
                context.provider = payload["model_provider"].as_str().unwrap_or("").to_owned();
                context.role = role.into();
                let boundary = super::UsageHistoryBoundary::from_meta(&payload, &context.session);
                // 复制头的原始 ordinal 为 0；其局部边界平移到当前文件的 ordinal。
                context.prefix = boundary
                    .parent
                    .zip(boundary.start)
                    .and_then(|(parent, start)| {
                        ordinal.unwrap().checked_add(start).map(|end| (parent, end))
                    });
                if context
                    .prefix
                    .as_ref()
                    .is_some_and(|(_, end)| *end <= ordinal.unwrap() || *end > context.end)
                    || ["forked_from_id", "parent_thread_id"]
                        .iter()
                        .any(|key| payload[*key].as_str().is_some_and(|id| !id.is_empty()))
                        && context.prefix.is_none()
                {
                    context.ambiguous = true;
                }
                return;
            }
            if let Some(id) = id {
                self.session = id;
                let role = if payload["source"].get("subagent").is_some()
                    || payload["source"]
                        .as_str()
                        .is_some_and(|source| source.to_ascii_lowercase().contains("subagent"))
                {
                    "subagent"
                } else {
                    "main"
                };
                let parent = (role == "subagent")
                    .then(|| {
                        payload["source"]["subagent"]["thread_spawn"]["parent_thread_id"]
                            .as_str()
                            .or_else(|| payload["parent_thread_id"].as_str())
                    })
                    .flatten()
                    .and_then(super::normalize_session_id);
                self.scope_parent_candidates
                    .entry(self.session.clone())
                    .or_default()
                    .insert(parent);
                self.role = role.into();
            }
            self.history_boundary = super::UsageHistoryBoundary::from_meta(&payload, &self.session);
            self.inherited_contexts.clear();
            self.legacy_header = None;
            self.provider = payload["model_provider"].as_str().unwrap_or("").to_owned();
            return;
        }
        if self.session.is_empty() {
            return;
        }
        let Some(time) = envelope
            .timestamp
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        else {
            return;
        };
        let timestamp = time.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let date = time.with_timezone(&Local).date_naive();
        let event_type = payload["type"].as_str().unwrap_or("");
        if envelope.kind == "turn_context" || event_type.starts_with("thread_settings") {
            let (provider, model, turn) = if inherited {
                let context = self.inherited_context(ordinal.unwrap());
                (&mut context.provider, &mut context.model, &mut context.turn)
            } else {
                (&mut self.provider, &mut self.model, &mut self.turn)
            };
            if let Some(value) = payload["model_provider"]
                .as_str()
                .or_else(|| payload["modelProvider"].as_str())
                .or_else(|| payload["thread_settings"]["model_provider"].as_str())
                .or_else(|| payload["thread_settings"]["modelProvider"].as_str())
                .filter(|provider| !provider.is_empty() && provider.len() <= 200)
            {
                *provider = value.to_owned();
            }
            if let Some(value) = payload["model"]
                .as_str()
                .or_else(|| payload["thread_settings"]["model"].as_str())
            {
                if value.len() <= 200 {
                    *model = value.to_owned();
                }
            }
            if let Some(value) = payload["turn_id"].as_str() {
                *turn = value.to_owned();
            }
        }
        if matches!(event_type, "task_started" | "turn_started") {
            if let Some(turn) = payload["turn_id"].as_str() {
                if inherited {
                    self.inherited_context(ordinal.unwrap()).turn = turn.to_owned();
                } else {
                    self.turn = turn.to_owned();
                }
            }
        }
        if envelope.kind == "response_item" && date >= cutoff {
            if !inherited {
                self.accept_tool(payload, timestamp, date);
            }
            return;
        }
        if event_type != "token_count" {
            return;
        }
        let info = &payload["info"];
        let Some(total) = Tokens::parse(&info["total_token_usage"]) else {
            return;
        };
        let last = Tokens::parse(&info["last_token_usage"]);
        let has_file_baseline = self.previous.is_some();
        let delta = total.usage_delta(self.previous, last);
        self.previous = Some(total);
        if date < cutoff {
            return;
        }
        if self.events.len() >= MAX_EVENTS {
            self.issues += 1;
            return;
        }
        let (owner, model, turn, provider, role) = if inherited {
            let context = self.inherited_context(ordinal.unwrap());
            if context.ambiguous {
                self.issues += 1;
                return;
            }
            (
                context.session.clone(),
                context.model.clone(),
                context.turn.clone(),
                context.provider.clone(),
                context.role.clone(),
            )
        } else {
            (
                self.session.clone(),
                self.model.clone(),
                self.turn.clone(),
                self.provider.clone(),
                self.role.clone(),
            )
        };
        let mut digest = Sha256::new();
        digest.update(owner.as_bytes());
        digest.update(timestamp.as_bytes());
        digest.update(serde_json::to_vec(&json!([total, last])).unwrap_or_default());
        let legacy_identity =
            self.legacy_header
                .as_ref()
                .filter(|_| !inherited)
                .map(|(session, role)| {
                    let mut old = Sha256::new();
                    old.update(session.as_bytes());
                    old.update(timestamp.as_bytes());
                    old.update(serde_json::to_vec(&json!([total, last])).unwrap_or_default());
                    LegacyIdentity {
                        id: old.finalize().into(),
                        session: session.clone(),
                        role: role.clone(),
                    }
                });
        self.events.push(UsageEvent {
            id: digest.finalize().into(),
            session: owner,
            turn,
            model,
            timestamp,
            date,
            total,
            delta,
            has_file_baseline,
            provider,
            role,
            inherited,
            legacy_identity,
        });
    }

    fn accept_tool(&mut self, mut payload: Value, timestamp: String, date: NaiveDate) {
        let Some(id) = payload["call_id"]
            .as_str()
            .or_else(|| payload["id"].as_str())
            .filter(|id| id.len() <= 300)
            .map(str::to_owned)
        else {
            return;
        };
        let kind = payload["type"].as_str().unwrap_or("").to_owned();
        if matches!(kind.as_str(), "function_call" | "custom_tool_call") {
            let name = payload["name"].as_str().unwrap_or("");
            let namespace = payload["namespace"].as_str().unwrap_or("");
            let arguments = payload["arguments"]
                .as_str()
                .or_else(|| payload["input"].as_str())
                .unwrap_or("");
            if name.rsplit('.').next() == Some("write_stdin") {
                if let Ok(args) = serde_json::from_str::<Value>(arguments) {
                    let session = args["session_id"].as_u64().map(|id| id.to_string());
                    if let Some(session) = session.filter(|id| self.exec_skills.contains_key(id)) {
                        if self.exec_polls.len() >= 512 {
                            self.exec_polls.clear();
                        }
                        self.exec_polls.insert(id, session);
                    }
                }
                return;
            }
            if !name.starts_with("mcp__")
                && !namespace.starts_with("mcp__")
                && !arguments.to_lowercase().contains("skill.md")
            {
                return;
            }
            if arguments.len() > 64_000 {
                self.issues += 1;
                return;
            }
            if !arguments.to_lowercase().contains("skill.md") {
                payload["arguments"] = json!("{}");
                payload.as_object_mut().unwrap().remove("input");
            }
            if self.calls.len() >= 512 {
                if let Some(oldest) = self
                    .calls
                    .iter()
                    .min_by(|a, b| a.1.timestamp.cmp(&b.1.timestamp))
                    .map(|(id, _)| id.clone())
                {
                    self.calls.remove(&oldest);
                }
            }
            self.calls.insert(
                id,
                Call {
                    timestamp,
                    session: self.session.clone(),
                    payload,
                },
            );
        } else if matches!(
            kind.as_str(),
            "function_call_output" | "custom_tool_call_output"
        ) {
            let session = self.exec_polls.remove(&id);
            let asynchronous = session
                .as_ref()
                .and_then(|key| self.exec_skills.remove(key));
            let (call, evidence) = if let Some(pending) = asynchronous {
                (Some(pending.call), pending.evidence)
            } else {
                (self.calls.remove(&id), String::new())
            };
            let Some(call) = call else {
                return;
            };
            let shell_skill = call.payload["arguments"]
                .as_str()
                .or_else(|| call.payload["input"].as_str())
                .is_some_and(|args| args.to_lowercase().contains("skill.md"))
                && call.payload["name"].as_str().is_some_and(|name| {
                    name.contains("exec") || name.contains("shell") || name.contains("terminal")
                });
            let receipt = exec_receipt(&payload["output"]);
            let failed = payload["isError"] == true
                || payload["error"].is_object()
                || payload["error"].is_string()
                || payload["output"]["success"] == false
                || payload["output"]["isError"] == true
                || payload["output"]["error"].is_object()
                || payload["output"]["error"].is_string()
                || matches!(
                    payload["output"]["status"].as_str(),
                    Some("failed" | "error" | "cancelled" | "canceled" | "interrupted")
                )
                || ["exit_code", "exitCode"].iter().any(|key| {
                    payload["output"][key]
                        .as_i64()
                        .is_some_and(|code| code != 0)
                })
                || payload["output"].as_str().is_some_and(|text| {
                    text.trim_start().starts_with("Error")
                        || text.trim_start().starts_with("Tool call error:")
                });
            let exited = receipt.as_ref().is_some_and(|(running, _)| !running)
                || payload["output"]["exit_code"].is_number()
                || payload["output"]["exitCode"].is_number()
                || payload["output"]["success"] == true;
            if shell_skill
                && !failed
                && (receipt.as_ref().is_some_and(|r| r.0) || session.is_some() && !exited)
            {
                let session = session.or_else(|| receipt.map(|(_, id)| id)).unwrap();
                let evidence = compact_evidence(&evidence, &payload["output"]);
                if self.exec_skills.len() >= 256 {
                    if let Some(oldest) = self
                        .exec_skills
                        .iter()
                        .min_by(|a, b| a.1.call.timestamp.cmp(&b.1.call.timestamp))
                        .map(|(id, _)| id.clone())
                    {
                        self.exec_skills.remove(&oldest);
                    }
                }
                self.exec_skills
                    .insert(session, ExecSkill { call, evidence });
                return;
            }
            let skill_read = call.payload["arguments"]
                .as_str()
                .or_else(|| call.payload["input"].as_str())
                .is_some_and(|args| args.to_lowercase().contains("skill.md"));
            payload["output"] = tool_receipts::compact(&payload["output"], 0, skill_read);
            let id = call.payload["call_id"]
                .as_str()
                .or_else(|| call.payload["id"].as_str())
                .unwrap_or(&id)
                .to_owned();
            // 不返回完整工具输入/输出正文，保持调用身份与失败判定所需字段。
            let mut output = json!({"type":kind,"call_id":id,"timestamp":timestamp,"output":payload["output"],
                "isError":payload["isError"],"error":payload["error"].is_object() || payload["error"].is_string()});
            if !evidence.is_empty() {
                output["skillReadEvidence"] = json!(evidence);
            }
            let date = DateTime::parse_from_rfc3339(&call.timestamp)
                .map(|time| time.with_timezone(&Local).date_naive())
                .unwrap_or(date);
            if self.tools.len() >= MAX_ACTIVITY {
                self.tools.remove(0);
                self.issues += 1;
            }
            self.tools.push(ToolRecord {
                id,
                call,
                output,
                date,
            });
        }
    }
}

// 只识别执行器回执头，正文里的示例不能建立后台会话关联。
fn exec_receipt(value: &Value) -> Option<(bool, String)> {
    let text = value.as_str()?.trim();
    for line in text.lines() {
        let line = line.trim_end();
        if [
            "Chunk ID:",
            "Wall time:",
            "Original token count:",
            "Final output:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        {
            continue;
        }
        if let Some(id) = line.strip_prefix("Process running with session ID ") {
            let id = id.split_whitespace().next()?;
            return (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| (true, id.to_owned()));
        }
        if line
            .strip_prefix("Process exited with code ")
            .and_then(|code| code.split_whitespace().next())
            .and_then(|code| code.parse::<i32>().ok())
            .is_some()
        {
            return Some((false, String::new()));
        }
        return None;
    }
    None
}

fn compact_evidence(previous: &str, output: &Value) -> String {
    let output = compact_output(output, 0);
    let text = format!("{previous}\n{}", output.as_str().unwrap_or(""));
    if text.chars().count() <= 6000 {
        return text;
    }
    let prefix: String = text.chars().take(4000).collect();
    let suffix: String = text
        .chars()
        .rev()
        .take(2000)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("{prefix}\n{suffix}")
}

fn compact_output(value: &Value, depth: usize) -> Value {
    tool_receipts::compact(value, depth, true)
}

fn usage_provider<'a>(
    model: &'a str,
    recorded: &'a str,
    providers: &'a BTreeMap<String, String>,
) -> &'a str {
    if codex_plus_core::aggregate_catalog::is_aggregate_model(model) {
        // 复用目录的转义规则；上游型号可以含斜线，不能从最后一段反推成员。
        return providers
            .keys()
            .find(|id| {
                model.starts_with(&codex_plus_core::aggregate_catalog::aggregate_model_id(
                    id, "",
                ))
            })
            .map(String::as_str)
            .unwrap_or_else(|| {
                // 删除配置后日志仍保有绑定身份；保留原 ID，不改派到当前成员。
                model
                    .split_once('/')
                    .and_then(|(prefix, _)| prefix.strip_prefix("cpx-"))
                    .unwrap_or("")
            });
    }
    if let Some((prefix, _)) = model.split_once('/') {
        if providers.contains_key(prefix) {
            return prefix;
        }
    }
    // 外部代理可沿用原生标签；未配置的标签不能阻止实际请求证据归属。
    if recorded.eq_ignore_ascii_case("openai") && !providers.contains_key(recorded) {
        return "";
    }
    recorded
}

impl Snapshot {
    fn collect_scope_parents<'a>(&mut self, files: impl Iterator<Item = &'a FileState>) {
        let mut candidates: BTreeMap<String, HashSet<Option<String>>> = BTreeMap::new();
        for file in files {
            if let Some(parents) = file.scope_parent_candidates.get(&file.session) {
                let entries = candidates.entry(file.session.clone()).or_default();
                for parent in parents {
                    entries.insert(
                        parent
                            .as_ref()
                            .map(|id| self.aliases.get(id).unwrap_or(id).clone()),
                    );
                }
            }
        }
        let parents: BTreeMap<String, String> = candidates
            .into_iter()
            .filter_map(|(child, parents)| {
                if parents.len() != 1 {
                    return None;
                }
                parents
                    .into_iter()
                    .next()
                    .flatten()
                    .map(|parent| (child, parent))
            })
            .collect();
        // 只关闭不可信归属边；原有用量事件、继承历史去重与全局状态均不受影响。
        self.scope_parents = parents
            .iter()
            .filter(|(child, _)| {
                let mut seen = HashSet::new();
                let mut node = child.as_str();
                while let Some(parent) = parents.get(node) {
                    if !seen.insert(node) {
                        return false;
                    }
                    node = parent;
                }
                true
            })
            .map(|(child, parent)| (child.clone(), parent.clone()))
            .collect();
        for (child, parent) in &self.scope_parents {
            if let Some(info) = self.session_info.get_mut(child) {
                info["parentSessionId"] = json!(parent);
            }
        }
    }

    fn scoped_usage(&self, session: &str, query: &Query, bounds: Option<(i64, i64)>) -> Value {
        let session = self
            .aliases
            .get(session)
            .map(String::as_str)
            .unwrap_or(session);
        let mut sessions = HashSet::new();
        if self.sessions.contains(session) {
            sessions.insert(session.to_owned());
            let mut children: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
            for (child, parent) in &self.scope_parents {
                children.entry(parent).or_default().push(child);
            }
            let mut pending = vec![session];
            while let Some(parent) = pending.pop() {
                for &child in children.get(parent).into_iter().flatten() {
                    if sessions.insert(child.to_owned()) {
                        pending.push(child);
                    }
                }
            }
        }
        let mut ids: Vec<_> = sessions
            .iter()
            .filter(|id| id.as_str() != session)
            .cloned()
            .collect();
        ids.sort();
        if sessions.contains(session) {
            ids.insert(0, session.to_owned());
        }
        json!({"sessionId":session,"sessionIds":ids,"days":self.usage_days(query, bounds, Some(&sessions))})
    }

    #[cfg(test)]
    fn response(&self, query: &Query, processing: bool) -> Value {
        self.response_page(query, processing, 1, None, None)
    }

    fn usage_days(
        &self,
        query: &Query,
        bounds: Option<(i64, i64)>,
        sessions: Option<&HashSet<String>>,
    ) -> Vec<Value> {
        let mut days: BTreeMap<NaiveDate, Aggregate> = BTreeMap::new();
        let mut models: BTreeMap<(NaiveDate, String), Aggregate> = BTreeMap::new();
        let mut breakdowns: BTreeMap<(NaiveDate, String, String, String), Aggregate> =
            BTreeMap::new();
        for event in &self.events {
            if sessions.is_some_and(|ids| !ids.contains(&event.session)) {
                continue;
            }
            let at = DateTime::parse_from_rfc3339(&event.timestamp)
                .map(|time| time.timestamp_millis())
                .unwrap_or(0);
            if bounds.is_some_and(|(start, end)| start >= end || at < start || at >= end) {
                continue;
            }
            let record = Record {
                id: String::new(),
                session_id: event.session.clone(),
                turn_id: event.turn.clone(),
                model: Some(event.model.clone()).filter(|m| !m.is_empty()),
                started_at: Some(event.timestamp.clone()),
                updated_at: Some(event.timestamp.clone()),
                date: Some(event.date),
                status: "completed",
                usage: Some(event.delta),
            };
            if !query.accepts(&record) {
                continue;
            }
            let price = query.price_for(&event.model);
            days.entry(event.date).or_default().add(&record, price);
            models
                .entry((event.date, event.model.clone()))
                .or_default()
                .add(&record, price);
            breakdowns
                .entry((
                    event.date,
                    event.model.clone(),
                    event.provider.clone(),
                    event.role.clone(),
                ))
                .or_default()
                .add(&record, price);
        }
        days.into_iter()
            .map(|(date, aggregate)| {
                let mut row = aggregate.value();
                row["date"] = json!(date.to_string());
                row["models"] = json!(
                    models
                        .range((date, String::new())..=(date, char::MAX.to_string()))
                        .map(|((_, model), aggregate)| {
                            let mut row = aggregate.value();
                            row["model"] = json!(model);
                            row
                        })
                        .collect::<Vec<_>>()
                );
                row["breakdowns"] = json!(
                    breakdowns
                        .iter()
                        .filter(|((day, _, _, _), _)| day == &date)
                        .map(|((_, model, provider, role), aggregate)| {
                            let mut row = aggregate.value();
                            row["model"] = json!(model);
                            row["provider"] = json!(provider);
                            row["providerName"] = json!(self.provider_names.get(provider));
                            row["role"] = json!(role);
                            row
                        })
                        .collect::<Vec<_>>()
                );
                row
            })
            .collect()
    }

    fn response_page(
        &self,
        query: &Query,
        processing: bool,
        activity_page: usize,
        revision: Option<i64>,
        bounds: Option<(i64, i64)>,
    ) -> Value {
        let day_rows = self.usage_days(query, bounds, None);
        let selected_tools: Vec<_> = self
            .tools
            .iter()
            .filter(|tool| {
                query
                    .session_id
                    .as_ref()
                    .is_none_or(|id| id == &tool.call.session)
                    && query.from.is_none_or(|from| tool.date >= from)
                    && query.to.is_none_or(|to| tool.date <= to)
                    && bounds.is_none_or(|(start, end)| {
                        DateTime::parse_from_rfc3339(&tool.call.timestamp).is_ok_and(|time| {
                            time.timestamp_millis() >= start && time.timestamp_millis() < end
                        })
                    })
            })
            .collect();
        let unchanged = revision == Some(self.updated_at);
        let page_size = 512;
        let tools: Vec<_> = selected_tools
            .iter()
            .skip((activity_page - 1) * page_size)
            .take(if unchanged { 0 } else { page_size })
            .map(|tool| {
                json!({"conversationKey":tool.call.session,"timestamp":tool.call.timestamp,
            "call":tool.call.payload,"output":tool.output})
            })
            .collect();
        let complete = self.ready && self.errors == 0 && self.issues == 0;
        let mut response = json!({"status":if complete {"ok"} else {"partial"},"complete":complete,
            "updatedAt":self.updated_at,"source":"structured-local-rollout","days":day_rows,
            "accountingVersion":2,"rangeApplied":bounds.is_some(),
            "sessionInfo":self.session_info,"providerNames":self.provider_names,
            "providerRecovery":self.provider_recovery,
            "providerEvidence":self.provider_evidence,
            "sessionIds":self.sessions.iter().collect::<Vec<_>>(),"sessionAliases":self.aliases,
            "activityRecords":tools,"activityTruncated":self.activity_truncated,
            "activityPagination":{"page":activity_page,"pages":selected_tools.len().div_ceil(page_size).max(1),
                "total":selected_tools.len(),"unchanged":unchanged},
            "scan":{"discoveredFiles":self.files,"indexedFiles":self.indexed,
                "ready":self.ready,
                "compressedFiles":self.compressed,"pendingFiles":self.files.saturating_sub(self.indexed),
                "errors":self.errors,"malformedRecords":self.issues,"processing":processing,
                "bytesRead":self.bytes_read,"elapsedMs":self.elapsed_ms,
                "sessions":self.sessions.len(),"retentionDays":RETENTION_DAYS},
            "activityComplete":complete && !self.activity_truncated});
        if query.include_session_cursors {
            response["sessionCursors"] = json!(
                self.cursors
                    .iter()
                    .filter(|(id, _)| query.session_id.as_ref().is_none_or(|filter| filter == *id))
                    .map(|(id, (timestamp, total))| json!({
                        "sessionId": id,
                        "timestamp": timestamp,
                        "total": total
                    }))
                    .collect::<Vec<_>>()
            );
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    mod scope_tests {
        include!("structured/scope_tests.rs");
    }

    mod capture_tests {
        include!("structured/capture_tests.rs");
    }

    const SESSION: &str = "00000000-0000-4000-8000-000000000001";
    const OTHER: &str = "00000000-0000-4000-8000-000000000002";
    fn time(tick: i64) -> String {
        static BASE: OnceLock<DateTime<chrono::Utc>> = OnceLock::new();
        (*BASE.get_or_init(|| chrono::Utc::now() - Days::hours(1)) + Days::milliseconds(tick))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    }
    fn row(kind: &str, timestamp: &str, payload: Value) -> Value {
        json!({"type":kind,"timestamp":timestamp,"payload":payload})
    }
    fn token(timestamp: &str, total: u64, last: u64) -> Value {
        let usage = |input| json!({"input_tokens":input,"cached_input_tokens":0,"output_tokens":0,"total_tokens":input});
        row(
            "event_msg",
            timestamp,
            json!({"type":"token_count","info":{
            "total_token_usage":usage(total),"last_token_usage":usage(last)}}),
        )
    }
    fn header() -> Value {
        row("session_meta", &time(0), json!({"id":SESSION}))
    }
    fn model() -> Value {
        row(
            "event_msg",
            &time(1),
            json!({"type":"thread_settings_updated","thread_settings":{"model":"provider/gpt-6.1-sol"}}),
        )
    }
    fn write(home: &Path, suffix: &str, rows: &[Value], compressed: bool) -> PathBuf {
        let directory = home.join("sessions/2026/10/19");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join(format!(
            "rollout-2026-10-19T01-00-00-{suffix}.jsonl{}",
            if compressed { ".zst" } else { "" }
        ));
        let text = rows
            .iter()
            .map(|row| format!("{row}\n"))
            .collect::<String>();
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
    fn options() -> Query {
        Query::parse(
            &json!({"includeSessionCursors":true}),
            Local::now().date_naive(),
        )
        .unwrap()
    }

    #[test]
    fn file_usage_delta_matches_legacy_and_summary_on_component_reset_and_corrections() {
        let temp = tempfile::tempdir().unwrap();
        let mut rows = vec![header()];
        rows.extend(super::super::tests::usage_regression_rows(&time(10)));
        write(temp.path(), SESSION, &rows, false);
        let home = fs::canonicalize(temp.path()).unwrap();
        let mut index = Index::default();
        let snapshot = index.update_with_request_log(&home, &home.join("missing"));
        let state = index.files.values().next().unwrap();
        let totals = |events: &[UsageEvent]| {
            let mut usage = Tokens::default();
            for event in events.iter().filter(|event| event.delta.total_tokens > 0) {
                usage.add(event.delta);
            }
            usage
        };
        let expected = Tokens::parse(&json!({
            "input_tokens":93,"cached_input_tokens":24,"output_tokens":32,
            "reasoning_output_tokens":26
        }))
        .unwrap();
        assert_eq!(totals(&state.events), expected);
        assert_eq!(totals(&snapshot.events), expected);
        assert_eq!(
            state
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .collect::<Vec<_>>(),
            vec![100, 10, 0, 0, 10, 0, 5]
        );
        assert_eq!(
            snapshot.response(&options(), false)["days"][0]["usage"]["totalTokens"],
            125
        );
    }

    #[test]
    fn file_usage_delta_preserves_last_usage_when_reasoning_regresses_with_growing_total() {
        let temp = tempfile::tempdir().unwrap();
        let cutoff = Local::now().date_naive() - Days::days(1);
        let mut state = FileState::default();
        state.accept(&serde_json::to_vec(&header()).unwrap(), cutoff);
        let usage = |input, cached, output, reasoning| {
            json!({
                "input_tokens":input,"cached_input_tokens":cached,
                "output_tokens":output,"reasoning_output_tokens":reasoning
            })
        };
        let before = usage(80, 20, 20, 10);
        let next = usage(90, 25, 30, 5);
        let last = usage(15, 3, 15, 2);
        let mut rows = vec![header()];
        for (at, total, last) in [(10, before.clone(), before), (20, next, last)] {
            let row = row(
                "event_msg",
                &time(at),
                json!({
                    "type":"token_count","info":{"total_token_usage":total,"last_token_usage":last}
                }),
            );
            state.accept(&serde_json::to_vec(&row).unwrap(), cutoff);
            rows.push(row);
        }
        assert_eq!(state.events[1].delta.total_tokens, 30);
        assert_eq!(state.events[1].delta.reasoning_output_tokens, Some(2));
        assert!(
            !state.events[1]
                .total
                .is_monotonic_after(state.events[0].total)
        );
        write(temp.path(), SESSION, &rows, false);
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        assert_eq!(
            snapshot.events[1].delta.total_tokens, 30,
            "cross-file merging must preserve the same reset decision"
        );
    }

    #[test]
    fn session_cursors_preserve_timestamp_when_cumulative_usage_resets() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[
                header(),
                token(&time(10), 100, 100),
                token(&time(20), 10, 5),
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        let response = snapshot.response(&options(), false);
        let cursor = &response["sessionCursors"][0];
        assert_eq!(cursor["sessionId"], SESSION);
        assert_eq!(cursor["total"]["totalTokens"], 10);
        assert_eq!(cursor["timestamp"], time(20));
        assert!(DateTime::parse_from_rfc3339(cursor["timestamp"].as_str().unwrap()).is_ok());
        let query = Query::parse(
            &json!({"sessionId":OTHER,"includeSessionCursors":true}),
            Local::now().date_naive(),
        )
        .unwrap();
        assert_eq!(
            snapshot.response(&query, false)["sessionCursors"],
            json!([])
        );
    }

    #[test]
    fn structured_session_cursors_are_opt_in() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[header(), token(&time(10), 100, 100)],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        let without = Query::parse(&json!({}), Local::now().date_naive()).unwrap();
        assert!(
            snapshot
                .response(&without, false)
                .get("sessionCursors")
                .is_none()
        );
        let with = Query::parse(
            &json!({"includeSessionCursors":true}),
            Local::now().date_naive(),
        )
        .unwrap();
        assert_eq!(
            snapshot.response(&with, false)["sessionCursors"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn legacy_child_evidence_migration_is_exact_unique_and_does_not_change_usage() {
        let temp = tempfile::tempdir().unwrap();
        let mut inherited_header = header();
        inherited_header["ordinal"] = json!(1);
        let mut suffix = token(&time(30), 15, 5);
        suffix["ordinal"] = json!(6);
        write(
            temp.path(),
            OTHER,
            &[
                row(
                    "session_meta",
                    &time(20),
                    json!({"id":OTHER,"parent_thread_id":SESSION,
                "subagent_history_start_ordinal":5,"source":{"subagent":{}}}),
                ),
                inherited_header,
                model(),
                suffix,
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        let event = snapshot.events[0].clone();
        let legacy = event.legacy_identity.as_ref().unwrap();
        let mut old = event.clone();
        old.id = legacy.id;
        old.session.clone_from(&legacy.session);
        old.role.clone_from(&legacy.role);
        let record = provider_record(&old);
        let path = home.join("legacy-proof.json");
        write_provider_records(&path, vec![record.clone()]);
        let mut evidence = ProviderEvidenceCache::default();
        evidence.refresh(&path);
        let mut events = vec![event.clone()];
        let result = evidence.attribute(&mut events, &mut BTreeMap::new());
        assert_eq!(result["migratedEvents"], 1);
        assert_eq!(events[0].provider, "supplier-a");
        assert_eq!(events[0].delta, event.delta);
        assert_eq!(events[0].session, OTHER);
        for changed in 0..5 {
            let mut candidate = event.clone();
            match changed {
                0 => candidate.model = "different".into(),
                1 => candidate.timestamp = time(31),
                2 => candidate.delta = Tokens::new(19, 0, 0),
                3 => candidate.legacy_identity = None,
                _ => candidate.legacy_identity.as_mut().unwrap().role = "different".into(),
            }
            assert_eq!(
                evidence.attribute(&mut [candidate], &mut BTreeMap::new())["appliedEvents"],
                0
            );
        }
        // 不能把父记录的补证分配给多个子会话，也不能同时用于仍存在的父事件。
        for competitor in [event.clone(), old] {
            let mut events = vec![event.clone(), competitor];
            assert_eq!(
                evidence.attribute(&mut events, &mut BTreeMap::new())["migratedEvents"],
                0
            );
        }
        let mut direct = provider_record(&event);
        direct["provider"] = json!("conflicting");
        write_provider_records(&path, vec![record, direct]);
        let mut evidence = ProviderEvidenceCache::default();
        evidence.refresh(&path);
        let result = evidence.attribute(&mut [event], &mut BTreeMap::new());
        assert_eq!(result["conflictedEvents"], 1);
        assert_eq!(result["appliedEvents"], 0);
    }

    #[test]
    fn provider_cache_recovers_after_transient_rollout_absence_and_expires_while_open() {
        let temp = tempfile::tempdir().unwrap();
        let usage = json!({"input_tokens":10,"output_tokens":2,"cached_input_tokens":3});
        write(
            temp.path(),
            SESSION,
            &[
                header(),
                row(
                    "turn_context",
                    &time(1),
                    json!({"model":"vendor/model","model_provider":"custom"}),
                ),
                row(
                    "event_msg",
                    &time(10),
                    json!({"type":"token_count","info":{
                "total_token_usage":usage,"last_token_usage":usage}}),
                ),
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let log = home.join("request-log.jsonl");
        fs::write(
            &log,
            format!("{}\n", request_evidence_json("temporary-absence")),
        )
        .unwrap();
        let mut index = Index::default();
        assert_eq!(
            index.update_with_request_log(&home, &log).events[0].provider,
            "supplier-a"
        );
        fs::write(&log, b"").unwrap();
        fs::rename(home.join("sessions"), home.join("temporarily-away")).unwrap();
        assert!(index.update_with_request_log(&home, &log).events.is_empty());
        fs::rename(home.join("temporarily-away"), home.join("sessions")).unwrap();
        let snapshot = index.update_with_request_log(&home, &log);
        assert_eq!(snapshot.events[0].provider, "supplier-a");
        assert_eq!(snapshot.provider_recovery["cacheWrittenRecords"], 0);
        let cutoff = Local::now().date_naive() + Days::days(100);
        assert!(!index.provider_cache.open(&home, cutoff).unwrap());
        assert!(index.provider_cache.aliases.is_empty());
        let db = rusqlite::Connection::open(home.join("usage-provider-cache.sqlite")).unwrap();
        assert_eq!(
            db.query_row("SELECT count(*) FROM verified_requests", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn provider_cache_upgrades_v1_without_losing_aliases_or_verified_requests() {
        let temp = tempfile::tempdir().unwrap();
        let db =
            rusqlite::Connection::open(temp.path().join("usage-provider-cache.sqlite")).unwrap();
        db.execute_batch(
            "CREATE TABLE verified_requests(id BLOB PRIMARY KEY NOT NULL, at INTEGER NOT NULL,
            data TEXT NOT NULL) WITHOUT ROWID;
            CREATE TABLE aliases(id TEXT PRIMARY KEY NOT NULL,target TEXT NOT NULL) WITHOUT ROWID;
            INSERT INTO aliases(id,target) VALUES('old-alias','old-session');
            PRAGMA user_version=1;",
        )
        .unwrap();
        let event = request_event(SESSION, "", 10);
        let mut cache = RequestEvidenceCache::default();
        cache
            .records
            .push(evidence(request_evidence_json("upgrade")));
        cache.attribute(&mut [event], &mut BTreeMap::new(), &BTreeMap::new());
        let mut store = provider_cache::ProviderCache::default();
        let cutoff = Local::now().date_naive() - Days::days(RETENTION_DAYS - 1);
        assert!(store.open(temp.path(), cutoff).unwrap());
        assert_eq!(
            store.aliases.get("old-alias").map(String::as_str),
            Some("old-session")
        );
        store.save(&cache.verified, &BTreeMap::new()).unwrap();
        assert_eq!(store.load_verified(cutoff).unwrap().len(), 1);
        assert_eq!(
            db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert!(!store.open(temp.path(), cutoff).unwrap());
    }

    #[test]
    fn nested_inherited_headers_preserve_outer_child_identity() {
        let temp = tempfile::tempdir().unwrap();
        let mut parent = row(
            "session_meta",
            &time(15),
            json!({"id":SESSION,
            "parent_thread_id":"01a15000-0000-7000-8000-000000000083","subagent_history_start_ordinal":4}),
        );
        parent["ordinal"] = json!(1);
        let mut ancestor = header();
        ancestor["payload"]["id"] = json!("01a15000-0000-7000-8000-000000000083");
        ancestor["ordinal"] = json!(2);
        let mut own = token(&time(30), 20, 5);
        own["ordinal"] = json!(9);
        write(
            temp.path(),
            OTHER,
            &[
                row(
                    "session_meta",
                    &time(20),
                    json!({"id":OTHER,"parent_thread_id":SESSION,
                "subagent_history_start_ordinal":8,"source":{"subagent":{}}}),
                ),
                parent,
                ancestor,
                model(),
                own,
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        assert_eq!(snapshot.events[0].session, OTHER);
        assert_eq!(snapshot.events[0].role, "subagent");
    }

    #[test]
    fn fork_three_levels_keep_ancestor_identity_after_original_files_disappear() {
        let grandparent = "01a15000-0000-7000-8000-000000000083";
        let numbered = |mut value: Value, ordinal| {
            value["ordinal"] = json!(ordinal);
            value
        };
        let grandparent_header = row("session_meta", &time(0), json!({"id":grandparent}));
        let parent_header = row(
            "session_meta",
            &time(15),
            json!({
                "id":SESSION,"forked_from_id":grandparent,"subagent_history_start_ordinal":4
            }),
        );
        let context = |at, model, provider| {
            row(
                "turn_context",
                &time(at),
                json!({
                    "model":model,"model_provider":provider
                }),
            )
        };
        for compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let grandparent_path = write(
                temp.path(),
                grandparent,
                &[
                    numbered(grandparent_header.clone(), 0),
                    numbered(context(1, "grandparent-model", "supplier-g"), 1),
                    numbered(token(&time(10), 100, 100), 2),
                ],
                compressed,
            );
            let parent_path = write(
                temp.path(),
                SESSION,
                &[
                    numbered(parent_header.clone(), 0),
                    numbered(grandparent_header.clone(), 1),
                    numbered(context(1, "grandparent-model", "supplier-g"), 2),
                    numbered(token(&time(10), 100, 100), 3),
                    numbered(context(16, "parent-model", "supplier-p"), 4),
                    numbered(token(&time(20), 120, 20), 5),
                ],
                !compressed,
            );
            let child_rows = [
                numbered(
                    row(
                        "session_meta",
                        &time(25),
                        json!({
                            "id":OTHER,"forked_from_id":SESSION,"subagent_history_start_ordinal":8
                        }),
                    ),
                    0,
                ),
                numbered(parent_header.clone(), 1),
                numbered(grandparent_header.clone(), 2),
                numbered(context(1, "grandparent-model", "supplier-g"), 3),
                numbered(token(&time(10), 100, 100), 4),
                numbered(context(16, "parent-model", "supplier-p"), 5),
                numbered(token(&time(20), 120, 20), 6),
                numbered(context(26, "child-model", "supplier-c"), 8),
                numbered(token(&time(30), 130, 10), 9),
            ];
            let child_path = write(temp.path(), OTHER, &child_rows, compressed);
            let home = fs::canonicalize(temp.path()).unwrap();
            let log = home.join("missing");
            let mut index = Index::default();
            for removed in 0..3 {
                if removed == 1 {
                    fs::rename(
                        &grandparent_path,
                        grandparent_path.with_extension("retired"),
                    )
                    .unwrap();
                } else if removed == 2 {
                    fs::rename(&parent_path, parent_path.with_extension("retired")).unwrap();
                }
                let snapshot = index.update_with_request_log(&home, &log);
                assert_eq!(snapshot.events.len(), 3);
                assert_eq!(snapshot.issues, 0);
                for (session, total, model, provider) in [
                    (grandparent, 100, "grandparent-model", "supplier-g"),
                    (SESSION, 20, "parent-model", "supplier-p"),
                    (OTHER, 10, "child-model", "supplier-c"),
                ] {
                    let event = snapshot
                        .events
                        .iter()
                        .find(|event| event.session == session)
                        .unwrap();
                    assert_eq!(
                        event.delta.total_tokens, total,
                        "removed={removed} session={session}"
                    );
                    assert_eq!(event.model, model);
                    assert_eq!(event.provider, provider);
                }
                assert_eq!(
                    snapshot.response(&options(), false)["days"][0]["usage"]["totalTokens"],
                    130
                );
                let cold = Index::default().update_with_request_log(&home, &log);
                assert_eq!(
                    cold.events
                        .iter()
                        .map(|event| event.delta.total_tokens)
                        .sum::<u64>(),
                    130
                );
            }
            // 仅剩子文件时追加重复通知，不能再次计量祖先或子消费。
            let mut state = FileState::default();
            let cutoff = Local::now().date_naive() - Days::days(1);
            state.read(&child_path, cutoff).unwrap();
            let mut repeated = child_rows.last().unwrap().clone();
            repeated["ordinal"] = json!(10);
            repeated["timestamp"] = json!(time(31));
            state.accept(&serde_json::to_vec(&repeated).unwrap(), cutoff);
            assert_eq!(state.events.last().unwrap().delta.total_tokens, 0);
        }
    }

    #[test]
    fn fork_inherited_context_does_not_replace_child_meta_provider_or_own_context() {
        let numbered = |mut value: Value, ordinal| {
            value["ordinal"] = json!(ordinal);
            value
        };
        for own_context in [false, true] {
            let mut state = FileState::default();
            let cutoff = Local::now().date_naive() - Days::days(1);
            let mut rows = vec![numbered(
                row(
                    "session_meta",
                    &time(20),
                    json!({
                        "id":OTHER,"forked_from_id":SESSION,"subagent_history_start_ordinal":5,
                        "model_provider":"supplier-b","source":{"subagent":{}}
                    }),
                ),
                0,
            )];
            if own_context {
                rows.push(row(
                    "turn_context",
                    &time(21),
                    json!({
                        "model":"child-model","turn_id":"child-turn","model_provider":"supplier-b"
                    }),
                ));
            }
            rows.extend([
                numbered(row("session_meta", &time(0), json!({
                    "id":SESSION,"model_provider":"supplier-a"
                })), 1),
                numbered(row("turn_context", &time(1), json!({
                    "model":"parent-model","turn_id":"parent-turn","model_provider":"supplier-a"
                })), 2),
                numbered(token(&time(10), 100, 100), 3),
                numbered(token(&time(30), 120, 20), 5),
            ]);
            for row in rows {
                state.accept(&serde_json::to_vec(&row).unwrap(), cutoff);
            }
            let parent = &state.events[0];
            assert_eq!(parent.session, SESSION);
            assert_eq!(parent.provider, "supplier-a");
            assert_eq!(parent.model, "parent-model");
            assert_eq!(parent.turn, "parent-turn");
            assert_eq!(parent.role, "main");
            let child = &state.events[1];
            assert_eq!(child.session, OTHER);
            assert_eq!(child.delta.total_tokens, 20);
            assert_eq!(child.provider, "supplier-b");
            assert_eq!(child.role, "subagent");
            assert_eq!(child.model, if own_context { "child-model" } else { "" });
            assert_eq!(child.turn, if own_context { "child-turn" } else { "" });
            assert_eq!(state.provider, "supplier-b");
        }
    }

    #[test]
    fn fork_unassignable_nested_prefix_is_partial_but_keeps_child_delta() {
        let ancestor = "01a15000-0000-7000-8000-000000000083";
        for metadata in [
            json!({"id":SESSION,"forked_from_id":ancestor}),
            json!({"id":SESSION,"forked_from_id":ancestor,"subagent_history_start_ordinal":99}),
            json!({"id":ancestor}),
            json!({"id":"invalid"}),
            json!({"id":SESSION,"forked_from_id":OTHER,"subagent_history_start_ordinal":3}),
        ] {
            let mut state = FileState::default();
            let cutoff = Local::now().date_naive() - Days::days(1);
            for value in [
                row(
                    "session_meta",
                    &time(20),
                    json!({
                        "id":OTHER,"forked_from_id":SESSION,"subagent_history_start_ordinal":5
                    }),
                ),
                json!({"type":"session_meta","ordinal":1,"payload":metadata}),
                json!({"type":"event_msg","ordinal":2,"timestamp":time(10),"payload":token(&time(10),100,100)["payload"]}),
                json!({"type":"event_msg","ordinal":5,"timestamp":time(30),"payload":token(&time(30),120,20)["payload"]}),
            ] {
                state.accept(&serde_json::to_vec(&value).unwrap(), cutoff);
            }
            assert_eq!(state.issues, 1);
            assert_eq!(state.events.len(), 1);
            assert_eq!(state.events[0].session, OTHER);
            assert_eq!(state.events[0].delta.total_tokens, 20);
        }
    }

    #[test]
    fn provider_cache_cold_restart_retains_alias_match_and_revalidates_source() {
        let temp = tempfile::tempdir().unwrap();
        let usage = json!({"input_tokens":10,"output_tokens":2,"cached_input_tokens":3});
        write(
            temp.path(),
            OTHER,
            &[
                header(),
                row(
                    "turn_context",
                    &time(1),
                    json!({"model":"vendor/model","model_provider":"custom"}),
                ),
                row(
                    "event_msg",
                    &time(10),
                    json!({"type":"token_count","info":{
                "total_token_usage":usage,"last_token_usage":usage}}),
                ),
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let log = home.join("request-log.jsonl");
        let mut request = request_evidence_json("persistent-alias-match");
        request["threadId"] = json!(OTHER);
        fs::write(&log, format!("{request}\n")).unwrap();
        let mut index = Index::default();
        let snapshot = index.update_with_request_log(&home, &log);
        assert_eq!(snapshot.events[0].provider, "supplier-a");
        assert_eq!(snapshot.provider_recovery["cacheAvailable"], true);
        assert_eq!(
            index.update_with_request_log(&home, &log).provider_recovery["cacheWrittenRecords"],
            0
        );
        fs::write(&log, b"").unwrap();
        drop(index);
        let mut index = Index::default();
        let snapshot = index.update_with_request_log(&home, &log);
        assert_eq!(snapshot.events[0].provider, "supplier-a");
        assert_eq!(snapshot.provider_recovery["retainedEvents"], 1);
        request["success"] = json!(false);
        fs::write(&log, format!("{request}\n")).unwrap();
        assert!(unknown_provider(
            &index.update_with_request_log(&home, &log).events[0].provider
        ));
        drop(index);
        fs::write(&log, b"").unwrap();
        assert!(unknown_provider(
            &Index::default().update_with_request_log(&home, &log).events[0].provider
        ));
        // 缓存只有归属证据，不能作为已删除会话的用量来源。
        fs::rename(home.join("sessions"), home.join("removed-sessions")).unwrap();
        assert!(
            Index::default()
                .update_with_request_log(&home, &log)
                .events
                .is_empty()
        );
        let db = rusqlite::Connection::open(home.join("usage-provider-cache.sqlite")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='token_events'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
    }

    #[test]
    fn provider_cache_unavailable_does_not_block_usage_or_modify_invalid_file() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[header(), model(), token(&time(10), 10, 10)],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let cache = home.join("usage-provider-cache.sqlite");
        fs::write(&cache, b"invalid cache").unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        assert_eq!(snapshot.events[0].delta.total_tokens, 10);
        assert_eq!(snapshot.provider_recovery["cacheAvailable"], false);
        assert_eq!(snapshot.errors, 0);
        assert_eq!(fs::read(cache).unwrap(), b"invalid cache");
    }

    #[test]
    fn explicit_fork_prefix_is_deduplicated_and_independent_requests_remain_separate() {
        let temp = tempfile::tempdir().unwrap();
        let mut parent = header();
        parent["payload"]["source"] = json!({"subagent":{}});
        write(
            temp.path(),
            SESSION,
            &[parent, model(), token(&time(10), 10, 10)],
            false,
        );
        let mut prefix = token(&time(10), 10, 10);
        prefix["ordinal"] = json!(2);
        let mut suffix = token(&time(30), 15, 5);
        suffix["ordinal"] = json!(5);
        write(
            temp.path(),
            OTHER,
            &[
                row(
                    "session_meta",
                    &time(20),
                    json!({"id":OTHER,"forked_from_id":SESSION,
                "subagent_history_start_ordinal":4}),
                ),
                model(),
                prefix,
                suffix,
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|e| e.delta.total_tokens)
                .sum::<u64>(),
            15
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .find(|e| e.session == SESSION)
                .unwrap()
                .role,
            "subagent"
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|e| e.session == OTHER)
                .count(),
            1
        );
    }

    #[test]
    fn inherited_parent_metadata_does_not_overwrite_child_identity_after_explicit_boundary() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[header(), model(), token(&time(10), 10, 10)],
            false,
        );
        let mut inherited_header = header();
        inherited_header["ordinal"] = json!(1);
        let mut prefix = token(&time(10), 10, 10);
        prefix["ordinal"] = json!(2);
        let mut suffix = token(&time(30), 15, 5);
        suffix["ordinal"] = json!(6);
        write(
            temp.path(),
            OTHER,
            &[
                row(
                    "session_meta",
                    &time(20),
                    json!({"id":OTHER,"parent_thread_id":SESSION,
                "forked_from_id":SESSION,"subagent_history_start_ordinal":5,"source":{"subagent":{}}}),
                ),
                inherited_header,
                model(),
                prefix,
                suffix,
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = Index::default().update_with_request_log(&home, &home.join("missing"));
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|e| e.delta.total_tokens)
                .sum::<u64>(),
            15
        );
        let child = snapshot.events.iter().find(|e| e.session == OTHER).unwrap();
        assert_eq!(child.delta.total_tokens, 5);
        assert_eq!(child.role, "subagent");
        assert_eq!(snapshot.aliases[OTHER], OTHER);
    }

    fn request_event(session: &str, provider: &str, tick: i64) -> UsageEvent {
        let timestamp = time(tick);
        let delta = Tokens::new(10, 3, 2);
        UsageEvent {
            id: Sha256::digest(format!("{session}|{timestamp}").as_bytes()).into(),
            session: session.into(),
            turn: String::new(),
            model: "vendor/model".into(),
            timestamp,
            date: Local::now().date_naive(),
            total: delta,
            delta,
            has_file_baseline: true,
            provider: provider.into(),
            role: "main".into(),
            inherited: false,
            legacy_identity: None,
        }
    }

    fn provider_record(event: &UsageEvent) -> Value {
        json!({"eventId": event.id.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "session":event.session,"timestamp":event.timestamp,"model":event.model,"role":event.role,
            "input":event.delta.input_tokens,"output":event.delta.output_tokens,
            "cached":event.delta.cached_input_tokens,"provider":"supplier-a",
            "providerName":"Evidence name","source":"upstream-exact-bidirectional"})
    }

    fn write_provider_records(path: &Path, records: Vec<Value>) {
        fs::write(
            path,
            serde_json::to_vec(&json!({"version":1,"records":records})).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn provider_evidence_exact_match_names_and_tokens() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage-provider-evidence.json");
        let original = request_event(SESSION, "custom", 10);
        let mut record = provider_record(&original);
        record["timestamp"] = json!(
            DateTime::parse_from_rfc3339(&original.timestamp)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        );
        // 样本中的附加审计字段不参与签名，也不进入缓存或响应。
        record["upstreamAt"] = json!(time(10));
        record["upstreamSample"] = json!(1);
        record["cutoff"] = json!(time(11));
        write_provider_records(&path, vec![record]);
        let mut cache = ProviderEvidenceCache::default();
        cache.refresh(&path);
        for provider in ["", "custom", "UNKNOWN", "codex_plus_managed"] {
            let mut event = original.clone();
            event.provider = provider.into();
            let mut events = vec![event];
            let mut names = BTreeMap::from([("supplier-a".into(), "Current name".into())]);
            let stats = cache.attribute(&mut events, &mut names);
            assert_eq!(events[0].provider, "supplier-a");
            assert_eq!(events[0].delta, original.delta);
            assert_eq!(events[0].total, original.total);
            assert_eq!(stats["appliedEvents"], 1);
            assert_eq!(stats["appliedTokens"], original.delta.total_tokens);
            assert_eq!(names["supplier-a"], "Current name");
        }
        let mut events = vec![original.clone()];
        let mut names = BTreeMap::from([("supplier-a".into(), String::new())]);
        cache.attribute(&mut events, &mut names);
        assert_eq!(names["supplier-a"], "Evidence name");
        cache.refresh(&path);
        assert_eq!(cache.reads, 1);
        assert_eq!(cache.bytes_read, 0);
    }

    #[test]
    fn provider_evidence_requires_every_signature_field_and_unique_event() {
        let temp = tempfile::tempdir().unwrap();
        let original = request_event(SESSION, "custom", 10);
        for field in [
            "eventId",
            "session",
            "timestamp",
            "model",
            "role",
            "input",
            "output",
            "cached",
        ] {
            let path = temp.path().join(field);
            let mut record = provider_record(&original);
            record[field] = match field {
                "eventId" => json!("00".repeat(32)),
                "timestamp" => json!(time(11)),
                "input" => json!(11),
                "output" => json!(3),
                "cached" => json!(2),
                _ => json!("different"),
            };
            write_provider_records(&path, vec![record]);
            let mut cache = ProviderEvidenceCache::default();
            cache.refresh(&path);
            let mut events = vec![original.clone()];
            let stats = cache.attribute(&mut events, &mut BTreeMap::new());
            assert_eq!(events[0].provider, "custom", "{field}");
            assert_eq!(stats["appliedEvents"], 0, "{field}");
        }
        let path = temp.path().join("duplicate-event");
        write_provider_records(&path, vec![provider_record(&original)]);
        let mut cache = ProviderEvidenceCache::default();
        cache.refresh(&path);
        let mut events = vec![original.clone(), original];
        let stats = cache.attribute(&mut events, &mut BTreeMap::new());
        assert_eq!(stats["signatureMismatches"], 2);
        assert!(events.iter().all(|event| event.provider == "custom"));
    }

    #[test]
    fn provider_evidence_conflicting_ids_and_malformed_records_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("evidence");
        let original = request_event(SESSION, "custom", 10);
        for field in ["provider", "model", "input", "source"] {
            let record = provider_record(&original);
            let mut changed = record.clone();
            changed[field] = if field == "input" {
                json!("malformed")
            } else {
                json!("different")
            };
            for records in [vec![record.clone(), changed.clone()], vec![changed, record]] {
                write_provider_records(&path, records);
                let mut cache = ProviderEvidenceCache::default();
                cache.refresh(&path);
                let mut events = vec![original.clone()];
                let stats = cache.attribute(&mut events, &mut BTreeMap::new());
                assert_eq!(stats["appliedEvents"], 0, "{field}");
                assert_eq!(events[0].provider, "custom");
                assert!(cache.invalid > 0);
            }
        }
        let record = provider_record(&original);
        write_provider_records(&path, vec![record.clone(), record]);
        let mut cache = ProviderEvidenceCache::default();
        cache.refresh(&path);
        assert_eq!(cache.records.len(), 1);
    }

    #[test]
    fn provider_evidence_known_and_request_log_take_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("evidence");
        let original = request_event(SESSION, "custom", 10);
        write_provider_records(&path, vec![provider_record(&original)]);
        let mut cache = ProviderEvidenceCache::default();
        cache.refresh(&path);
        let mut request = request_evidence_json("request-1");
        request["relayId"] = json!("supplier-b");
        let mut requests = RequestEvidenceCache {
            records: vec![evidence(request)],
            ..Default::default()
        };
        let mut events = vec![original];
        let mut names = BTreeMap::new();
        requests.attribute(&mut events, &mut names, &BTreeMap::new());
        assert_eq!(events[0].provider, "supplier-b");
        let stats = cache.attribute(&mut events, &mut names);
        assert_eq!(events[0].provider, "supplier-b");
        assert_eq!(stats["conflictedEvents"], 1);
        assert!(!names.contains_key("supplier-a"));
        events[0].provider = "supplier-a".into();
        assert_eq!(cache.attribute(&mut events, &mut names)["appliedEvents"], 0);
    }

    #[test]
    fn provider_evidence_rewrite_delete_and_restore() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("evidence");
        let original = request_event(SESSION, "custom", 10);
        let record = provider_record(&original);
        write_provider_records(&path, vec![record.clone()]);
        let mut cache = ProviderEvidenceCache::default();
        cache.refresh(&path);
        let apply = |cache: &ProviderEvidenceCache| {
            let mut events = vec![original.clone()];
            cache.attribute(&mut events, &mut BTreeMap::new());
            events[0].provider.clone()
        };
        assert_eq!(apply(&cache), "supplier-a");
        fs::remove_file(&path).unwrap();
        cache.refresh(&path);
        assert_eq!(apply(&cache), "custom");
        write_provider_records(&path, vec![record.clone()]);
        cache.refresh(&path);
        assert_eq!(apply(&cache), "supplier-a");
        let mut changed = record.clone();
        changed["provider"] = json!("supplier-changed");
        write_provider_records(&path, vec![changed]);
        cache.refresh(&path);
        assert_eq!(apply(&cache), "custom");
        write_provider_records(&path, vec![record]);
        cache.refresh(&path);
        assert_eq!(apply(&cache), "custom");
        assert_eq!(cache.reads, 4);
    }

    #[test]
    fn provider_evidence_bounds_and_version() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("evidence");
        let original = request_event(SESSION, "custom", 10);
        let mut cache = ProviderEvidenceCache::default();
        let file = File::create(&path).unwrap();
        file.set_len(MAX_PROVIDER_EVIDENCE_BYTES + 1).unwrap();
        drop(file);
        cache.refresh(&path);
        assert_eq!(cache.reads, 0);
        assert_eq!(cache.invalid, 1);
        write_provider_records(&path, vec![json!(null); MAX_PROVIDER_EVIDENCE + 1]);
        cache.refresh(&path);
        assert!(cache.records.is_empty());
        assert_eq!(cache.invalid, 1);
        write_provider_records(
            &path,
            vec![provider_record(&original); MAX_PROVIDER_EVIDENCE],
        );
        cache.refresh(&path);
        assert_eq!(cache.records.len(), 1);
        fs::write(&path, br#"{"version":2,"records":[]}"#).unwrap();
        cache.refresh(&path);
        assert!(cache.records.is_empty());
        assert_eq!(cache.invalid, 1);
        for field in ["eventId", "source", "provider", "cached", "input"] {
            let mut record = provider_record(&original);
            record[field] = match field {
                "cached" => json!(11),
                "input" => json!(u64::MAX),
                _ => json!("unknown"),
            };
            write_provider_records(&path, vec![record]);
            let mut cache = ProviderEvidenceCache::default();
            cache.refresh(&path);
            assert!(cache.records.is_empty(), "{field}");
            assert_eq!(cache.invalid, 1);
        }
    }

    #[test]
    fn provider_evidence_index_rebuild_revokes_deleted_file_without_usage_changes() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("request-log.jsonl");
        let path = temp.path().join("usage-provider-evidence.json");
        write(
            temp.path(),
            SESSION,
            &[
                row(
                    "session_meta",
                    &time(0),
                    json!({"id":SESSION,"model_provider":"custom"}),
                ),
                row("turn_context", &time(1), json!({"model":"gpt"})),
                token(&time(10), 100, 100),
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let mut index = Index::default();
        let baseline = index.update_with_request_log(&home, &log);
        assert_eq!(baseline.events.len(), 1);
        write_provider_records(&path, vec![provider_record(&baseline.events[0])]);
        let imported = index.update_with_request_log(&home, &log);
        assert_eq!(imported.events[0].provider, "supplier-a");
        assert_eq!(imported.events[0].delta, baseline.events[0].delta);
        assert_eq!(imported.events[0].total, baseline.events[0].total);
        assert_eq!(
            imported.response(&options(), false)["providerEvidence"]["appliedEvents"],
            1
        );
        assert_eq!(
            index.files.values().next().unwrap().events[0].provider,
            "custom"
        );
        let warm = index.update_with_request_log(&home, &log);
        assert_eq!(warm.provider_evidence["fileReads"], 1);
        assert_eq!(warm.provider_evidence["fileBytesRead"], 0);
        fs::remove_file(&path).unwrap();
        let removed = index.update_with_request_log(&home, &log);
        assert_eq!(removed.events[0].provider, "custom");
        assert_eq!(removed.events[0].delta, baseline.events[0].delta);
        assert_eq!(removed.provider_evidence["appliedEvents"], 0);
        assert!(!removed.provider_names.contains_key("supplier-a"));
        assert_eq!(removed.response(&options(), false)["complete"], true);
    }

    fn request_evidence_json(id: &str) -> Value {
        json!({"id":id,"threadId":SESSION,"model":"vendor/model","upstreamModel":"vendor/model",
            "relayId":"supplier-a","relayName":"Logged name","success":true,"state":"success",
            "statusCode":200,"timestampMs":DateTime::parse_from_rfc3339(&time(10)).unwrap().timestamp_millis(),
            "startedAtMs":DateTime::parse_from_rfc3339(&time(9)).unwrap().timestamp_millis(),
            "attempts":1,"autoRetries":0,"usage":{"inputTokens":10,"outputTokens":2,"cachedTokens":3}})
    }

    fn evidence(value: Value) -> RequestEvidence {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn request_alias_matches_both_directions_and_trusted_chains() {
        let intermediate = "request-alias-intermediate";
        let aliases = BTreeMap::from([
            (OTHER.to_owned(), intermediate.to_owned()),
            (intermediate.to_owned(), SESSION.to_owned()),
            (SESSION.to_owned(), SESSION.to_owned()),
        ]);
        for (thread, session) in [(OTHER, SESSION), (SESSION, OTHER), (intermediate, OTHER)] {
            for tick in [10, 30_026] {
                let mut request = evidence(request_evidence_json("request-alias"));
                request.thread_id = thread.into();
                let mut cache = RequestEvidenceCache {
                    records: vec![request],
                    ..Default::default()
                };
                let mut events = vec![request_event(session, "custom", tick)];
                let before = events[0].delta;
                let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &aliases);
                assert_eq!(summary["recoveredEvents"], 1, "{thread} -> {session}");
                assert_eq!(summary["delayedEvents"], usize::from(tick > 30_010));
                assert_eq!(events[0].provider, "supplier-a");
                assert_eq!(events[0].delta, before);
                assert_eq!(events[0].session, session);
            }
        }
    }

    #[test]
    fn request_alias_normalizes_uuid_spelling_without_merging_independent_suffixes() {
        let aliases = BTreeMap::from([(OTHER.to_uppercase(), format!("local:{SESSION}"))]);
        let mut request = evidence(request_evidence_json("request-alias"));
        request.thread_id = format!("local:{}", OTHER.to_uppercase());
        let mut cache = RequestEvidenceCache {
            records: vec![request],
            ..Default::default()
        };
        let mut events = vec![request_event(SESSION, "custom", 10)];
        assert_eq!(
            cache.attribute(&mut events, &mut BTreeMap::new(), &aliases)["recoveredEvents"],
            1
        );

        for (thread, session) in [
            (SESSION.to_owned(), format!("{SESSION}-branch")),
            (format!("{SESSION}-branch-a"), format!("{SESSION}-branch-b")),
        ] {
            let mut request = evidence(request_evidence_json("independent-suffix"));
            request.thread_id = thread;
            let mut cache = RequestEvidenceCache {
                records: vec![request],
                ..Default::default()
            };
            let mut events = vec![request_event(&session, "custom", 10)];
            let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &aliases);
            assert_eq!(summary["recoveredEvents"], 0);
            assert_eq!(events[0].provider, "custom");
        }
    }

    #[test]
    fn request_alias_rejects_ambiguity_in_both_directions_and_delayed_windows() {
        let aliases = BTreeMap::from([(OTHER.to_owned(), SESSION.to_owned())]);
        for tick in [10, 30_026] {
            let first = evidence(request_evidence_json("request-alias-1"));
            let mut second = evidence(request_evidence_json("request-alias-2"));
            second.thread_id = OTHER.into();
            let mut cache = RequestEvidenceCache {
                records: vec![first.clone(), second],
                ..Default::default()
            };
            let mut events = vec![request_event(SESSION, "custom", tick)];
            assert_eq!(
                cache.attribute(&mut events, &mut BTreeMap::new(), &aliases)["recoveredEvents"],
                0
            );
            cache.records = vec![first];
            for provider in ["custom", "known-provider"] {
                let mut events = vec![
                    request_event(SESSION, "custom", tick),
                    request_event(OTHER, provider, 11),
                ];
                let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &aliases);
                let strict_only = tick == 30_026 && provider == "custom";
                assert_eq!(summary["recoveredEvents"], usize::from(strict_only));
                assert_eq!(events[0].provider, "custom");
                assert_eq!(
                    events[1].provider,
                    if strict_only { "supplier-a" } else { provider }
                );
            }
        }
    }

    #[test]
    fn request_alias_rejects_cycles_and_normalized_conflicts_across_the_component() {
        for aliases in [
            BTreeMap::from([
                (SESSION.to_owned(), OTHER.to_owned()),
                (OTHER.to_owned(), SESSION.to_owned()),
                ("cycle-entry".into(), SESSION.to_owned()),
            ]),
            BTreeMap::from([
                (SESSION.to_owned(), OTHER.to_owned()),
                (format!("local:{SESSION}"), "conflicting-terminal".into()),
            ]),
        ] {
            for session in [SESSION, OTHER] {
                let mut request = evidence(request_evidence_json("request-alias"));
                request.thread_id = session.into();
                let mut cache = RequestEvidenceCache {
                    records: vec![request],
                    ..Default::default()
                };
                let mut events = vec![request_event(session, "custom", 10)];
                let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &aliases);
                assert_eq!(summary["recoveredEvents"], 0, "{session}");
                assert!(cache.verified.is_empty());
            }
        }
        let aliases = BTreeMap::from([
            ("unrelated-a".into(), "unrelated-b".into()),
            ("unrelated-b".into(), "unrelated-a".into()),
        ]);
        let mut cache = RequestEvidenceCache {
            records: vec![evidence(request_evidence_json("unrelated-cycle"))],
            ..Default::default()
        };
        assert_eq!(
            cache.attribute(
                &mut [request_event(SESSION, "custom", 10)],
                &mut BTreeMap::new(),
                &aliases
            )["recoveredEvents"],
            1
        );
    }

    #[test]
    fn request_alias_late_conflict_revokes_and_remains_rejected_after_alias_removal() {
        for conflict in 0..3 {
            let original = request_event(SESSION, "custom", 30_026);
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-alias"))],
                ..Default::default()
            };
            assert_eq!(
                cache.attribute(
                    &mut [original.clone()],
                    &mut BTreeMap::new(),
                    &BTreeMap::new()
                )["delayedEvents"],
                1
            );
            let aliases = BTreeMap::from([(OTHER.to_owned(), SESSION.to_owned())]);
            let mut events = vec![original.clone()];
            match conflict {
                0 => events.push(request_event(OTHER, "known-provider", 10)),
                1 => {
                    let mut request = evidence(request_evidence_json("late-alias-request"));
                    request.thread_id = OTHER.into();
                    cache.records.push(request);
                }
                _ => {}
            }
            let aliases = if conflict == 2 {
                BTreeMap::from([
                    (SESSION.to_owned(), OTHER.to_owned()),
                    (OTHER.to_owned(), SESSION.to_owned()),
                ])
            } else {
                aliases
            };
            let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &aliases);
            assert_eq!(summary["conflictedEvents"], 1);
            assert_eq!(events[0].provider, "custom");
            cache.records.truncate(1);
            assert_eq!(
                cache.attribute(&mut [original], &mut BTreeMap::new(), &BTreeMap::new())["recoveredEvents"],
                0
            );
        }
    }

    #[test]
    fn request_alias_new_trusted_mapping_preserves_previously_verified_signature() {
        let original = request_event(OTHER, "custom", 10);
        let mut request = evidence(request_evidence_json("request-alias"));
        request.thread_id = OTHER.into();
        let mut cache = RequestEvidenceCache {
            records: vec![request],
            ..Default::default()
        };
        cache.attribute(
            &mut [original.clone()],
            &mut BTreeMap::new(),
            &BTreeMap::new(),
        );
        let aliases = BTreeMap::from([(OTHER.to_owned(), SESSION.to_owned())]);
        let summary = cache.attribute(&mut [original], &mut BTreeMap::new(), &aliases);
        assert_eq!(summary["recoveredEvents"], 1);
        assert_eq!(summary["conflictedEvents"], 0);
    }

    #[test]
    fn request_persistence_roundtrip_keeps_rotated_sources_and_revokes_late_conflicts() {
        for conflict in 0..4 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("request-log.jsonl");
            let aliases = BTreeMap::from([(OTHER.to_owned(), SESSION.to_owned())]);
            let mut request = request_evidence_json("persisted-request");
            request["threadId"] = json!(OTHER);
            fs::write(&path, format!("{request}\n")).unwrap();
            let original = request_event(SESSION, "custom", 30_026);
            let mut cache = RequestEvidenceCache::default();
            cache.refresh(&path);
            assert_eq!(
                cache.attribute(&mut [original.clone()], &mut BTreeMap::new(), &aliases)["delayedEvents"],
                1
            );
            let serialized = serde_json::to_vec(&cache.retained_verified()).unwrap();
            let value: Value = serde_json::from_slice(&serialized).unwrap();
            assert_eq!(value[0][1]["rejected"], false);
            assert_eq!(value[0][1]["request"]["threadId"], OTHER);
            assert_eq!(value[0][1]["request"]["usage"]["cachedTokens"], 3);
            fs::rename(&path, temp.path().join("rotated.jsonl")).unwrap();
            let mut restarted = RequestEvidenceCache::default();
            restarted.restore_verified(
                serde_json::from_slice::<Vec<([u8; 32], VerifiedAttribution)>>(&serialized)
                    .unwrap(),
            );
            restarted.refresh(&path);
            let mut events = vec![original.clone()];
            let summary = restarted.attribute(&mut events, &mut BTreeMap::new(), &aliases);
            assert_eq!(summary["retainedEvents"], 1);
            assert_eq!(summary["delayedEvents"], 1);
            assert_eq!(events[0].provider, "supplier-a");
            assert_eq!(events[0].delta, original.delta);

            let mut events = vec![original.clone()];
            match conflict {
                0 => events.push(request_event(OTHER, "known-provider", 10)),
                1 => {
                    let mut competing = request.clone();
                    competing["id"] = json!("late-request");
                    fs::write(&path, format!("{competing}\n")).unwrap();
                }
                2 => {
                    request["relayId"] = json!("rewritten-provider");
                    fs::write(&path, format!("{request}\n")).unwrap();
                }
                _ => {
                    request["success"] = json!(false);
                    fs::write(&path, format!("{request}\n")).unwrap();
                }
            }
            restarted.refresh(&path);
            let summary = restarted.attribute(&mut events, &mut BTreeMap::new(), &aliases);
            assert_eq!(summary["conflictedEvents"], 1, "conflict {conflict}");
            assert_eq!(events[0].provider, "custom");
            let serialized = serde_json::to_vec(&restarted.retained_verified()).unwrap();
            let value: Value = serde_json::from_slice(&serialized).unwrap();
            assert_eq!(value[0][1]["rejected"], true);
            assert_eq!(value[0][1]["request"]["relayId"], "supplier-a");
            let mut restarted = RequestEvidenceCache::default();
            restarted.restore_verified(
                serde_json::from_slice::<Vec<([u8; 32], VerifiedAttribution)>>(&serialized)
                    .unwrap(),
            );
            let summary = restarted.attribute(&mut [original], &mut BTreeMap::new(), &aliases);
            assert_eq!(summary["recoveredEvents"], 0);
            assert_eq!(summary["conflictedEvents"], 1);
        }
    }

    #[test]
    fn request_persistence_restore_rechecks_event_signature_and_retention() {
        let original = request_event(SESSION, "custom", 10);
        let mut cache = RequestEvidenceCache {
            records: vec![evidence(request_evidence_json("persisted-request"))],
            ..Default::default()
        };
        cache.attribute(
            &mut [original.clone()],
            &mut BTreeMap::new(),
            &BTreeMap::new(),
        );
        let retained = cache.retained_verified();
        for changed in 0..4 {
            let mut restarted = RequestEvidenceCache::default();
            restarted.restore_verified(retained.clone());
            let mut altered = original.clone();
            match changed {
                0 => altered.session = OTHER.into(),
                1 => altered.model = "different-model".into(),
                2 => altered.timestamp = time(11),
                _ => altered.delta = Tokens::new(11, 3, 2),
            }
            let summary =
                restarted.attribute(&mut [altered], &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(summary["recoveredEvents"], 0);
            assert_eq!(summary["conflictedEvents"], 1);
            restarted.attribute(&mut [], &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(restarted.retained_verified().len(), 1);
        }
        let mut duplicate = retained[0].clone();
        duplicate.1.rejected = true;
        let mut restarted = RequestEvidenceCache::default();
        restarted.restore_verified(retained.into_iter().chain([duplicate]));
        assert_eq!(
            restarted.attribute(&mut [original], &mut BTreeMap::new(), &BTreeMap::new())["recoveredEvents"],
            0
        );
    }

    #[test]
    fn request_evidence_exact_match_preserves_usage_known_provider_and_current_names() {
        let mut cache = RequestEvidenceCache {
            records: vec![evidence(request_evidence_json("request-1"))],
            ..Default::default()
        };
        for provider in ["", "custom", "CoDeX_PlUs_MaNaGeD", "UNKNOWN"] {
            let mut events = vec![request_event(SESSION, provider, 10)];
            let before = events[0].delta;
            let mut names = BTreeMap::from([("supplier-a".into(), "Current name".into())]);
            let summary = cache.attribute(&mut events, &mut names, &BTreeMap::new());
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].delta, before);
            assert_eq!(events[0].provider, "supplier-a");
            assert_eq!(names["supplier-a"], "Current name");
            assert_eq!(summary["recoveredEvents"], 1);
            assert_eq!(summary["recoveredTokens"], 12);
            assert_eq!(summary["scope"], "index");
        }
        let mut events = vec![request_event(SESSION, "recorded-provider", 10)];
        let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
        assert_eq!(events[0].provider, "recorded-provider");
        assert_eq!(summary["recoveredEvents"], 0);
        let mut names = BTreeMap::new();
        cache.attribute(
            &mut [request_event(SESSION, "custom", 10)],
            &mut names,
            &BTreeMap::new(),
        );
        assert_eq!(names["supplier-a"], "Logged name");
    }

    #[test]
    fn request_only_session_model_requires_consistent_success_and_preserves_recorded_context() {
        let mut cache = RequestEvidenceCache {
            records: vec![evidence(request_evidence_json("request-1"))],
            ..Default::default()
        };
        let mut sessions = BTreeMap::new();
        let now = cache.records[0].timestamp_ms;
        cache.supplement_missing_session_models(&mut sessions, now);
        assert_eq!(sessions[SESSION]["model"], "vendor/model");
        assert_eq!(sessions[SESSION]["modelSource"], "request-log-single-model");
        assert!(sessions[SESSION].get("provider").is_none());
        assert!(sessions[SESSION].get("role").is_none());
        sessions.insert(
            SESSION.into(),
            json!({"model":"recorded/model","role":"subagent"}),
        );
        cache.supplement_missing_session_models(&mut sessions, now);
        assert_eq!(sessions[SESSION]["model"], "recorded/model");
        let mut changed = request_evidence_json("request-2");
        changed["model"] = json!("different/model");
        cache.records.push(evidence(changed));
        sessions.clear();
        cache.supplement_missing_session_models(&mut sessions, now);
        assert!(
            sessions.is_empty(),
            "model switches cannot be replaced by the latest request"
        );
        let mut failed = request_evidence_json("failed");
        failed["success"] = json!(false);
        cache.records = vec![evidence(failed)];
        cache.supplement_missing_session_models(&mut sessions, now);
        assert!(
            sessions.is_empty(),
            "failed requests alone cannot supply model context"
        );
    }

    #[test]
    fn request_only_model_fills_empty_metadata_and_rejects_duplicate_or_stale_evidence() {
        let request = evidence(request_evidence_json("request-1"));
        let now = request.timestamp_ms;
        let mut cache = RequestEvidenceCache {
            records: vec![request.clone()],
            ..Default::default()
        };
        for model in ["", "Unknown"] {
            let mut sessions = BTreeMap::from([(
                SESSION.into(),
                json!({"model":model,"provider":"recorded-provider","role":"subagent"}),
            )]);
            cache.supplement_missing_session_models(&mut sessions, now);
            assert_eq!(sessions[SESSION]["model"], "vendor/model");
            assert_eq!(sessions[SESSION]["provider"], "recorded-provider");
            assert_eq!(sessions[SESSION]["role"], "subagent");
        }
        for offset in [-1, 600_001] {
            let mut sessions = BTreeMap::new();
            cache.supplement_missing_session_models(&mut sessions, now + offset);
            assert!(
                sessions.is_empty(),
                "future or expired records cannot supply context"
            );
        }
        for duplicate in [
            request.clone(),
            RequestEvidence {
                id: request.id.clone(),
                ..Default::default()
            },
        ] {
            cache.records = vec![request.clone(), duplicate];
            let mut sessions = BTreeMap::new();
            cache.supplement_missing_session_models(&mut sessions, now);
            assert!(
                sessions.is_empty(),
                "duplicated or invalidated request identity is not trusted"
            );
        }
    }

    #[test]
    fn request_only_model_limit_applies_to_missing_valid_contexts() {
        let mut cache = RequestEvidenceCache::default();
        let mut sessions = BTreeMap::new();
        let now = evidence(request_evidence_json("base")).timestamp_ms;
        for index in 0..20 {
            let session = format!("01a15000-0000-7000-8000-{index:012}");
            let mut request = request_evidence_json(&format!("known-{index}"));
            request["threadId"] = json!(session);
            cache.records.push(evidence(request));
            sessions.insert(session, json!({"model":"recorded/model"}));
        }
        let mut missing = evidence(request_evidence_json("missing"));
        missing.timestamp_ms -= 1;
        cache.records.push(missing);
        cache.supplement_missing_session_models(&mut sessions, now);
        assert_eq!(
            sessions[SESSION]["model"], "vendor/model",
            "already indexed sessions cannot exhaust the missing-context budget"
        );
    }

    #[test]
    fn request_evidence_rejects_both_ambiguity_directions_including_known_events() {
        let first = evidence(request_evidence_json("request-1"));
        let mut cache = RequestEvidenceCache {
            records: vec![first.clone(), evidence(request_evidence_json("request-2"))],
            ..Default::default()
        };
        let mut events = vec![request_event(SESSION, "custom", 10)];
        assert_eq!(
            cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new())["recoveredEvents"],
            0
        );
        cache.records = vec![first];
        for provider in ["custom", "known-provider"] {
            let mut events = vec![
                request_event(SESSION, "custom", 10),
                request_event(SESSION, provider, 11),
            ];
            let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(summary["recoveredEvents"], 0);
            assert_eq!(events[0].provider, "custom");
            assert_eq!(events[1].provider, provider);
        }
    }

    #[test]
    fn request_evidence_requires_complete_success_and_consistent_final_retry() {
        let mut retried = request_evidence_json("request-1");
        retried["attempts"] = json!(3);
        retried["autoRetries"] = json!(1);
        retried["relayId"] = json!("final-supplier");
        let mut cache = RequestEvidenceCache {
            records: vec![evidence(retried.clone())],
            ..Default::default()
        };
        let mut events = vec![request_event(SESSION, "custom", 10)];
        cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
        assert_eq!(events[0].provider, "final-supplier");
        for field in [
            "threadId",
            "model",
            "upstreamModel",
            "relayId",
            "relayName",
            "success",
            "state",
            "statusCode",
            "timestampMs",
            "startedAtMs",
            "attempts",
            "autoRetries",
            "usage",
        ] {
            let mut row = retried.clone();
            row.as_object_mut().unwrap().remove(field);
            assert!(evidence(row).keys().is_empty(), "{field}");
        }
        for (field, value) in [
            ("success", json!(false)),
            ("state", json!("running")),
            ("state", json!("failed")),
            ("statusCode", json!(500)),
            ("attempts", json!(0)),
            ("autoRetries", json!(3)),
            ("startedAtMs", json!(0)),
            ("timestampMs", json!(1)),
        ] {
            let mut row = retried.clone();
            row[field] = value;
            assert!(evidence(row).keys().is_empty(), "{field}");
        }
        for field in ["inputTokens", "outputTokens", "cachedTokens"] {
            let mut row = retried.clone();
            row["usage"].as_object_mut().unwrap().remove(field);
            assert!(evidence(row).keys().is_empty(), "{field}");
        }
    }

    #[test]
    fn request_evidence_requires_exact_thread_model_counts_and_time_window() {
        for changed in 0..7 {
            let mut event = request_event(SESSION, "custom", 10);
            match changed {
                0 => event.session = OTHER.into(),
                1 => event.model = "model".into(),
                2 => event.delta = Tokens::new(11, 3, 2),
                3 => event.delta = Tokens::new(10, 3, 3),
                4 => event.delta = Tokens::new(10, 4, 2),
                5 => event.timestamp = time(120_011),
                _ => event.timestamp = time(-29_991),
            }
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-1"))],
                ..Default::default()
            };
            assert_eq!(
                cache.attribute(&mut [event], &mut BTreeMap::new(), &BTreeMap::new())["recoveredEvents"],
                0
            );
        }
        for tick in [-29_990, 30_010] {
            let mut event = request_event(SESSION, "custom", tick);
            event.model = "cpx-supplier-a/vendor/model".into();
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-1"))],
                ..Default::default()
            };
            assert_eq!(
                cache.attribute(&mut [event], &mut BTreeMap::new(), &BTreeMap::new())["recoveredEvents"],
                1
            );
        }
    }

    #[test]
    fn request_evidence_delayed_boundaries_and_retry_exclusion() {
        for (gap, accepted) in [
            (30_000, true),
            (30_016, true),
            (120_000, true),
            (120_001, false),
            (-30_001, false),
            (-120_000, false),
        ] {
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-1"))],
                ..Default::default()
            };
            let mut events = vec![request_event(SESSION, "custom", 10 + gap)];
            let before = events[0].delta;
            let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(
                summary["recoveredEvents"],
                usize::from(accepted),
                "gap {gap}"
            );
            assert_eq!(
                summary["delayedEvents"],
                usize::from(accepted && gap > 30_000)
            );
            assert_eq!(events[0].delta, before);
            assert_eq!(
                events[0].provider,
                if accepted { "supplier-a" } else { "custom" }
            );
        }
        for (attempts, retries) in [(2, 0), (2, 1), (3, 1)] {
            let mut row = request_evidence_json("retried");
            row["attempts"] = json!(attempts);
            row["autoRetries"] = json!(retries);
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(row)],
                ..Default::default()
            };
            let summary = cache.attribute(
                &mut [request_event(SESSION, "custom", 30_026)],
                &mut BTreeMap::new(),
                &BTreeMap::new(),
            );
            assert_eq!(summary["recoveredEvents"], 0);
        }
    }

    #[test]
    fn request_evidence_delayed_requires_wide_reciprocal_uniqueness() {
        let request = request_evidence_json("request-1");
        for conflict in 0..4 {
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request.clone())],
                ..Default::default()
            };
            let mut events = vec![request_event(SESSION, "custom", 30_026)];
            match conflict {
                0 | 1 => events.push(request_event(
                    SESSION,
                    if conflict == 0 {
                        "custom"
                    } else {
                        "known-provider"
                    },
                    10,
                )),
                _ => {
                    let mut other = request_evidence_json("request-2");
                    other["timestampMs"] = json!(cache.records[0].timestamp_ms - 40_000);
                    other["startedAtMs"] = json!(cache.records[0].started_at_ms - 40_000);
                    if conflict == 3 {
                        other["attempts"] = json!(2);
                    }
                    cache.records.push(evidence(other));
                }
            }
            cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(events[0].provider, "custom", "conflict {conflict}");
        }
    }

    #[test]
    fn request_evidence_delayed_revokes_on_late_conflict_and_preserves_known_provider() {
        let original = request_event(SESSION, "custom", 30_026);
        let mut cache = RequestEvidenceCache {
            records: vec![evidence(request_evidence_json("request-1"))],
            ..Default::default()
        };
        let summary = cache.attribute(
            &mut [original.clone()],
            &mut BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(summary["delayedEvents"], 1);
        assert_eq!(summary["delayedTokens"], 12);
        let mut events = vec![
            original.clone(),
            request_event(SESSION, "recorded-provider", 10),
        ];
        cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
        assert_eq!(events[0].provider, "custom");
        assert_eq!(events[1].provider, "recorded-provider");
        let summary = cache.attribute(&mut [original], &mut BTreeMap::new(), &BTreeMap::new());
        assert_eq!(
            summary["recoveredEvents"], 0,
            "revoked identities remain untrusted"
        );
        assert_eq!(summary["conflictedEvents"], 1);
    }

    #[test]
    fn request_evidence_delayed_rotation_and_late_request_conflicts() {
        for conflict in 0..3 {
            let original = request_event(SESSION, "custom", 30_026);
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-1"))],
                ..Default::default()
            };
            cache.attribute(
                &mut [original.clone()],
                &mut BTreeMap::new(),
                &BTreeMap::new(),
            );
            let saved = cache.records[0].clone();
            cache.records.clear();
            let summary = cache.attribute(
                &mut [original.clone()],
                &mut BTreeMap::new(),
                &BTreeMap::new(),
            );
            assert_eq!(summary["retainedEvents"], 1);
            assert_eq!(summary["delayedEvents"], 1);
            cache.records = vec![saved.clone()];
            match conflict {
                0 => cache
                    .records
                    .push(evidence(request_evidence_json("request-2"))),
                1 => cache.records[0].relay_id = "rewritten-provider".into(),
                _ => cache.records[0].success = false,
            }
            let mut events = vec![original.clone()];
            let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(events[0].provider, "custom");
            assert_eq!(events[0].delta.total_tokens, 12);
            assert_eq!(summary["conflictedEvents"], 1);
            cache.records = vec![saved];
            assert_eq!(
                cache.attribute(&mut [original], &mut BTreeMap::new(), &BTreeMap::new())["recoveredEvents"],
                0
            );
        }
    }

    #[test]
    fn request_evidence_strict_match_cannot_be_reused_by_delayed_event_in_either_order() {
        for reversed in [false, true] {
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-1"))],
                ..Default::default()
            };
            let mut events = vec![
                request_event(SESSION, "custom", 10),
                request_event(SESSION, "custom", 40_010),
            ];
            if reversed {
                events.reverse();
            }
            let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(summary["recoveredEvents"], 1);
            assert_eq!(summary["delayedEvents"], 0);
            for event in events {
                assert_eq!(
                    event.provider,
                    if event.timestamp == time(10) {
                        "supplier-a"
                    } else {
                        "custom"
                    }
                );
            }
        }
    }

    #[test]
    fn request_evidence_cached_identity_rechecks_full_event_signature() {
        for changed in 0..6 {
            let event = request_event(SESSION, "custom", 10);
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-1"))],
                ..Default::default()
            };
            cache.attribute(&mut [event.clone()], &mut BTreeMap::new(), &BTreeMap::new());
            cache.records.clear();
            let mut altered = event.clone();
            match changed {
                0 => altered.delta = Tokens::new(11, 3, 2),
                1 => altered.delta = Tokens::new(10, 3, 3),
                2 => altered.delta = Tokens::new(10, 4, 2),
                3 => altered.session = OTHER.into(),
                4 => altered.model = "other-model".into(),
                _ => altered.timestamp = time(11),
            }
            assert_eq!(altered.id, event.id);
            let summary = cache.attribute(&mut [altered], &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(summary["recoveredEvents"], 0);
            assert_eq!(summary["conflictedEvents"], 1);
        }
    }

    #[test]
    fn request_evidence_cached_conflict_is_revoked_in_both_directions_and_on_rewrite() {
        for conflict in 0..4 {
            let original = request_event(SESSION, "custom", 10);
            let mut cache = RequestEvidenceCache {
                records: vec![evidence(request_evidence_json("request-1"))],
                ..Default::default()
            };
            cache.attribute(
                &mut [original.clone()],
                &mut BTreeMap::new(),
                &BTreeMap::new(),
            );
            let mut events = vec![original.clone()];
            match conflict {
                0 => cache
                    .records
                    .push(evidence(request_evidence_json("request-2"))),
                1 => events.push(request_event(SESSION, "known-provider", 11)),
                2 => cache.records[0].relay_id = "rewritten-provider".into(),
                _ => cache.records[0].success = false,
            }
            let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
            assert_eq!(summary["recoveredEvents"], 0);
            assert_eq!(summary["conflictedEvents"], 1);
            assert_eq!(events[0].provider, "custom");
            cache.records.clear();
            assert_eq!(
                cache.attribute(&mut [original], &mut BTreeMap::new(), &BTreeMap::new())["recoveredEvents"],
                0
            );
        }
    }

    #[test]
    fn request_evidence_projection_is_bounded_rejects_bad_lines_and_skips_unused_fields() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("request-log.jsonl");
        let mut valid = request_evidence_json("request-1");
        valid["error"] = json!({"ignored":["not", "a", "string"]});
        fs::write(
            &log,
            format!(
                "{}\n{{bad}}\n{}\n{}",
                json!({"id":"too-long","error":"x".repeat(MAX_REQUEST_LOG_LINE)}),
                valid,
                request_evidence_json("unterminated")
            ),
        )
        .unwrap();
        let mut cache = RequestEvidenceCache::default();
        cache.refresh(&log);
        assert_eq!(cache.records.len(), 1);
        assert_eq!(cache.records[0].id, "request-1");
        assert!(!cache.records[0].keys().is_empty());
        assert_eq!(cache.reads, 1);
        cache.refresh(&log);
        assert_eq!(cache.reads, 1);
        assert_eq!(cache.bytes_read, 0);
        cache.attribute(
            &mut [request_event(SESSION, "custom", 10)],
            &mut BTreeMap::new(),
            &BTreeMap::new(),
        );
        let mut invalid_type = valid.clone();
        invalid_type["state"] = Value::Null;
        fs::write(&log, format!("{invalid_type}\n")).unwrap();
        cache.refresh(&log);
        assert_eq!(cache.records.len(), 1);
        assert!(cache.records[0].keys().is_empty());
        let summary = cache.attribute(
            &mut [request_event(SESSION, "custom", 10)],
            &mut BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert_eq!(summary["recoveredEvents"], 0);
        assert_eq!(summary["conflictedEvents"], 1);
        fs::write(
            &log,
            format!(
                "{}\n",
                format!("{valid}\n").repeat(MAX_REQUEST_EVIDENCE + 1)
            ),
        )
        .unwrap();
        cache.refresh(&log);
        assert!(cache.records.is_empty());
        let reads = cache.reads;
        cache.refresh(&log);
        assert_eq!(cache.reads, reads);
        assert_eq!(cache.bytes_read, 0);
        let oversized = temp.path().join("oversized.jsonl");
        File::create(&oversized)
            .unwrap()
            .set_len(MAX_REQUEST_LOG_BYTES + 1)
            .unwrap();
        cache.refresh(&oversized);
        assert!(cache.records.is_empty());
        assert_eq!(cache.reads, 0);
    }

    #[test]
    fn request_evidence_index_recomputes_delta_preserves_rotation_and_never_adds_usage() {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("request-log.jsonl");
        let mut request = request_evidence_json("request-1");
        request["model"] = json!("gpt");
        request["upstreamModel"] = json!("gpt");
        request["usage"] = json!({"inputTokens":250,"outputTokens":0,"cachedTokens":0});
        request["timestampMs"] = json!(
            DateTime::parse_from_rfc3339(&time(20))
                .unwrap()
                .timestamp_millis()
        );
        fs::write(&log, format!("{request}\n")).unwrap();
        write(
            temp.path(),
            SESSION,
            &[
                row(
                    "session_meta",
                    &time(0),
                    json!({"id":SESSION,"model_provider":"custom"}),
                ),
                row("turn_context", &time(1), json!({"model":"gpt"})),
                token(&time(10), 100, 100),
                token(&time(20), 350, 250),
                token(&time(21), 350, 250),
            ],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let mut index = Index::default();
        let first = index.update_with_request_log(&home, &log);
        assert_eq!(first.events.len(), 2);
        assert_eq!(
            first
                .events
                .iter()
                .map(|e| e.delta.total_tokens)
                .sum::<u64>(),
            350
        );
        assert_eq!(first.provider_recovery["recoveredEvents"], 1);
        assert_eq!(first.events[0].provider, "custom");
        assert_eq!(first.events[1].provider, "supplier-a");
        assert_eq!(
            index.files.values().next().unwrap().events[1].provider,
            "custom"
        );
        let warm = index.update_with_request_log(&home, &log);
        assert_eq!(warm.bytes_read, 0);
        assert_eq!(warm.provider_recovery["logBytesRead"], 0);
        assert_eq!(warm.provider_recovery["logReads"], 1);
        fs::rename(&log, temp.path().join("rotated.jsonl")).unwrap();
        let rotated = index.update_with_request_log(&home, &log);
        assert_eq!(rotated.provider_recovery["retainedEvents"], 1);
        assert_eq!(
            rotated
                .events
                .iter()
                .map(|e| e.delta.total_tokens)
                .sum::<u64>(),
            350
        );
        // 同一路径重写为失败记录，必须撤销内存中的历史补证，但不影响主要索引完整性。
        request["success"] = json!(false);
        fs::write(&log, format!("{request}\n")).unwrap();
        let rewritten = index.update_with_request_log(&home, &log);
        assert_eq!(rewritten.provider_recovery["recoveredEvents"], 0);
        assert_eq!(rewritten.provider_recovery["conflictedEvents"], 1);
        assert_eq!(rewritten.events[1].provider, "custom");
        assert_eq!(rewritten.response(&options(), false)["complete"], true);
        fs::write(&log, b"{bad}\n").unwrap();
        let invalid = index.update_with_request_log(&home, &log);
        assert_eq!(invalid.response(&options(), false)["complete"], true);
        assert_eq!(
            invalid
                .events
                .iter()
                .map(|e| e.delta.total_tokens)
                .sum::<u64>(),
            350
        );
    }

    #[test]
    fn request_evidence_verified_cache_does_not_stop_at_projection_capacity() {
        let mut cache = RequestEvidenceCache::default();
        let mut events = Vec::new();
        for i in 0..MAX_REQUEST_EVIDENCE {
            let session = format!("test-session-{i}");
            let event = request_event(&session, "custom", 10);
            let mut request = evidence(request_evidence_json(&format!("request-{i}")));
            request.thread_id = session;
            cache.verified.insert(
                event.id,
                VerifiedAttribution {
                    key: request.keys().remove(0),
                    at: request.timestamp_ms,
                    request,
                    rejected: false,
                },
            );
            events.push(event);
        }
        cache
            .records
            .push(evidence(request_evidence_json("new-request")));
        events.push(request_event(SESSION, "custom", 10));
        let summary = cache.attribute(&mut events, &mut BTreeMap::new(), &BTreeMap::new());
        assert_eq!(summary["recoveredEvents"], MAX_REQUEST_EVIDENCE + 1);
        assert_eq!(events.last().unwrap().provider, "supplier-a");
        assert_eq!(cache.verified.len(), MAX_REQUEST_EVIDENCE + 1);
        cache.attribute(&mut [], &mut BTreeMap::new(), &BTreeMap::new());
        assert_eq!(cache.verified.len(), MAX_REQUEST_EVIDENCE + 1);
    }

    #[test]
    fn provider_attribution_uses_catalog_member_identity_and_recorded_direct_provider() {
        let names = BTreeMap::from([
            ("relay".to_owned(), "Relay".to_owned()),
            ("relay/team %".to_owned(), "Escaped relay".to_owned()),
        ]);
        for id in names.keys() {
            let model = codex_plus_core::aggregate_catalog::aggregate_model_id(id, "vendor/model");
            assert_eq!(usage_provider(&model, "aggregate", &names), id);
        }
        assert_eq!(usage_provider("vendor/model", "direct", &names), "direct");
        assert_eq!(usage_provider("vendor/model", "", &names), "");
        assert_eq!(
            usage_provider("cpx-removed/vendor/model", "aggregate", &names),
            "removed"
        );
        assert_eq!(
            usage_provider("relay/vendor/model", "custom", &names),
            "relay"
        );
    }

    #[test]
    fn provider_context_changes_are_attributed_to_their_own_usage() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[
                header(),
                row(
                    "turn_context",
                    &time(1),
                    json!({"model":"gpt-6.1-sol","model_provider":"supplier-a"}),
                ),
                token(&time(10), 100, 100),
                row(
                    "event_msg",
                    &time(11),
                    json!({"type":"thread_settings_updated",
                "thread_settings":{"model":"gpt-6.1-sol","modelProvider":"supplier-b"}}),
                ),
                token(&time(20), 150, 50),
            ],
            false,
        );
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.events[0].provider, "supplier-a");
        assert_eq!(snapshot.events[1].provider, "supplier-b");
    }

    #[test]
    fn split_baselines_model_attribution_roles_and_minute_bounds() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[header(), model(), token(&time(10), 100, 100)],
            false,
        );
        // 重开时旧快照带新时间，且模型上下文稍后才写入。
        write(
            temp.path(),
            &format!("{SESSION}_{OTHER}"),
            &[
                header(),
                token(&time(20), 100, 100),
                model(),
                token(&time(30), 160, 60),
            ],
            true,
        );
        let child = row(
            "session_meta",
            &time(0),
            json!({"id":OTHER,"model_provider":"other",
            "source":{"subagent":{"thread_spawn":{"parent_thread_id":SESSION}}}}),
        );
        write(
            temp.path(),
            OTHER,
            &[child, model(), token(&time(15), 40, 40)],
            false,
        );
        let mut index = Index::default();
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = index.update(&home);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            200
        );
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| event.model == "provider/gpt-6.1-sol")
        );
        let rows = snapshot.response(&options(), false);
        let parts = rows["days"][0]["breakdowns"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts
                .iter()
                .find(|part| part["role"] == "subagent")
                .unwrap()["usage"]["totalTokens"],
            40
        );
        let start = DateTime::parse_from_rfc3339(&time(30))
            .unwrap()
            .timestamp_millis();
        let bounded = snapshot.response_page(&options(), false, 1, None, Some((start, start + 1)));
        assert_eq!(bounded["days"][0]["usage"]["totalTokens"], 60);
        assert_eq!(bounded["rangeApplied"], true);
        assert_eq!(index.update(&home).bytes_read, 0);
    }

    #[test]
    fn equal_usage_in_different_sessions_is_not_a_duplicate() {
        let temp = tempfile::tempdir().unwrap();
        let mut other = header();
        other["payload"]["id"] = json!(OTHER);
        write(
            temp.path(),
            SESSION,
            &[header(), token(&time(10), 100, 100)],
            false,
        );
        write(
            temp.path(),
            OTHER,
            &[other, token(&time(10), 100, 100)],
            false,
        );
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            200
        );
    }

    #[test]
    fn interleaved_copied_baselines_never_amplify_file_local_usage() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[
                header(),
                model(),
                token(&time(10), 100_000, 100),
                token(&time(20), 100_100, 100),
                token(&time(30), 100_200, 100),
            ],
            false,
        );
        write(
            temp.path(),
            &format!("{SESSION}_{OTHER}"),
            &[
                header(),
                model(),
                token(&time(15), 100, 100),
                token(&time(25), 200, 100),
                token(&time(35), 300, 100),
            ],
            true,
        );
        let mut index = Index::default();
        let home = fs::canonicalize(temp.path()).unwrap();
        let snapshot = index.update(&home);
        assert_eq!(snapshot.events.len(), 6);
        assert!(
            snapshot
                .events
                .iter()
                .all(|event| event.delta.total_tokens == 100)
        );
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            600
        );
        assert_eq!(index.update(&home).bytes_read, 0);
    }

    #[test]
    fn continuation_without_file_baseline_preserves_missing_cumulative_gap() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[header(), model(), token(&time(10), 100, 100)],
            false,
        );
        write(
            temp.path(),
            &format!("{SESSION}_{OTHER}"),
            &[header(), model(), token(&time(20), 300, 50)],
            true,
        );
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.events[1].delta.total_tokens, 200);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            300
        );
    }

    #[test]
    fn continuation_with_regressed_components_uses_local_last_usage() {
        let temp = tempfile::tempdir().unwrap();
        let mut first = token(&time(10), 100, 100);
        first["payload"]["info"]["total_token_usage"]["input_tokens"] = json!(80);
        first["payload"]["info"]["total_token_usage"]["output_tokens"] = json!(20);
        write(temp.path(), SESSION, &[header(), model(), first], false);
        write(
            temp.path(),
            &format!("{SESSION}_{OTHER}"),
            &[header(), model(), token(&time(20), 300, 50)],
            true,
        );
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.events[1].delta.total_tokens, 50);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            150
        );
    }

    #[test]
    fn copied_unchanged_consumption_with_missing_cached_field_does_not_replay_last_usage() {
        for compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut first = token(&time(10), 100, 100);
            first["payload"]["info"]["total_token_usage"]["cached_input_tokens"] = json!(50);
            first["payload"]["info"]["last_token_usage"]["cached_input_tokens"] = json!(50);
            write(
                temp.path(),
                SESSION,
                &[header(), model(), first],
                !compressed,
            );
            let mut copied = token(&time(20), 100, 100);
            copied["payload"]["info"]["total_token_usage"]
                .as_object_mut()
                .unwrap()
                .remove("cached_input_tokens");
            write(
                temp.path(),
                &format!("{SESSION}_{OTHER}"),
                &[header(), model(), copied, token(&time(30), 120, 20)],
                compressed,
            );
            let mut index = Index::default();
            let home = fs::canonicalize(temp.path()).unwrap();
            let snapshot = index.update(&home);
            assert_eq!(snapshot.events.len(), 2);
            assert_eq!(
                snapshot
                    .events
                    .iter()
                    .map(|event| event.delta.total_tokens)
                    .sum::<u64>(),
                120
            );
            assert_eq!(snapshot.events[0].delta.cached_input_tokens, 50);
            assert_eq!(index.update(&home).bytes_read, 0);
        }
    }

    #[test]
    fn context_after_final_usage_resolves_model_without_changing_consumption() {
        let temp = tempfile::tempdir().unwrap();
        write(
            temp.path(),
            SESSION,
            &[header(), token(&time(10), 100, 100), model()],
            false,
        );
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.events[0].model, "provider/gpt-6.1-sol");
        assert_eq!(snapshot.events[0].delta.total_tokens, 100);
        let end = DateTime::parse_from_rfc3339(&time(10))
            .unwrap()
            .timestamp_millis();
        assert!(
            snapshot.response_page(&options(), false, 1, None, Some((end - 1, end)))["days"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            query(temp.path(), &options(), &json!({"startAt":1,"endAt":1}))["status"],
            "unavailable"
        );
    }

    #[test]
    fn merges_migrated_continuations_and_deduplicates_copied_events() {
        let temp = tempfile::tempdir().unwrap();
        let first = token(&time(10), 100, 100);
        write(
            temp.path(),
            SESSION,
            &[header(), model(), first.clone()],
            false,
        );
        write(
            temp.path(),
            &format!("{SESSION}_{OTHER}"),
            &[header(), model(), first, token(&time(20), 150, 50)],
            true,
        );
        let mut index = Index::default();
        let snapshot = index.update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(
            snapshot
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            150
        );
        assert_eq!(
            snapshot.aliases.get(OTHER).map(String::as_str),
            Some(SESSION)
        );
        assert!(
            snapshot.response(&options(), false)["complete"]
                .as_bool()
                .unwrap()
        );
        let warm = index.update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(
            warm.bytes_read, 0,
            "unchanged compressed and plain logs must not be reread"
        );
    }

    #[test]
    fn appends_and_replaces_without_duplicate_usage_and_waits_for_complete_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(
            temp.path(),
            SESSION,
            &[header(), model(), token(&time(10), 100, 100)],
            false,
        );
        let home = fs::canonicalize(temp.path()).unwrap();
        let mut index = Index::default();
        index.update(&home);
        let next = token(&time(20), 150, 50).to_string();
        let split = next.len() / 2;
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&next.as_bytes()[..split])
            .unwrap();
        assert_eq!(index.update(&home).events.len(), 1);
        writeln!(
            File::options().append(true).open(&path).unwrap(),
            "{}",
            &next[split..]
        )
        .unwrap();
        let appended = index.update(&home);
        assert_eq!(
            appended
                .events
                .iter()
                .map(|event| event.delta.total_tokens)
                .sum::<u64>(),
            150
        );
        write(
            temp.path(),
            SESSION,
            &[header(), model(), token(&time(30), 30, 30)],
            false,
        );
        assert_eq!(index.update(&home).events[0].delta.total_tokens, 30);
    }

    #[test]
    fn reads_large_plain_and_compressed_files_without_returning_message_bodies() {
        let temp = tempfile::tempdir().unwrap();
        let huge = row(
            "response_item",
            &time(2),
            json!({"type":"message","content":"x".repeat(17 * 1024 * 1024)}),
        );
        let rows = [header(), model(), huge, token(&time(10), 100, 100)];
        write(temp.path(), SESSION, &rows, false);
        write(temp.path(), &format!("{SESSION}_{OTHER}"), &rows, true);
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert!(snapshot.bytes_read > 34 * 1024 * 1024);
        let response = snapshot.response(&options(), false);
        assert_eq!(response["complete"], true);
        assert_eq!(response["days"][0]["usage"]["totalTokens"], 100);
        assert!(
            response.to_string().len() < 5000,
            "full message text must stay in background"
        );
    }

    #[test]
    fn completed_tool_pairs_keep_identity_failure_signals_and_exclude_pending_calls() {
        let temp = tempfile::tempdir().unwrap();
        let started_at = time(10);
        let completed_at = time(20);
        let call = |id, name, namespace, arguments| {
            row(
                "response_item",
                &started_at,
                json!({"type":"function_call","call_id":id,"name":name,"namespace":namespace,"arguments":arguments}),
            )
        };
        let output = |id, result| {
            row(
                "response_item",
                &completed_at,
                json!({"type":"function_call_output","call_id":id,"output":result}),
            )
        };
        write(
            temp.path(),
            SESSION,
            &[
                header(),
                model(),
                call(
                    "skill",
                    "exec_command",
                    "",
                    r#"{"cmd":"Get-Content C:/skills/playwright/SKILL.md"}"#,
                ),
                output(
                    "skill",
                    json!(format!(
                        "Process exited with code 0\n{}",
                        "Error example\n".repeat(100_000)
                    )),
                ),
                call("failed", "generate", "mcp__images", "{}"),
                output(
                    "failed",
                    json!([{"type":"input_text","text":"Error executing tool: unavailable"}]),
                ),
                call("pending", "generate", "mcp__images", "{}"),
                call("ordinary", "exec_command", "", r#"{"cmd":"Get-Date"}"#),
                output("ordinary", json!("Process exited with code 0\n")),
            ],
            false,
        );
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.tools.len(), 2);
        let response = snapshot.response(&options(), false);
        assert!(response.to_string().len() < 10_000);
        assert!(
            response["activityRecords"]
                .as_array()
                .unwrap()
                .iter()
                .all(|record| record["timestamp"] == started_at
                    && record["output"]["timestamp"] == completed_at)
        );
        assert!(
            response["activityRecords"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["output"]["call_id"] == "failed"
                    && record["output"]["output"][0]["text"] == "Error: tool execution failed")
        );
    }

    #[test]
    fn async_skill_receipts_keep_original_identity_and_wait_for_completion() {
        let cutoff = Local::now().date_naive() - Days::days(1);
        let mut state = FileState::default();
        state.accept(&serde_json::to_vec(&header()).unwrap(), cutoff);
        let mut accept = |id: &str, kind: &str, name: &str, args: &str, output: Value, at: i64| {
            let event = row(
                "response_item",
                &time(at),
                json!({"type":kind,"call_id":id,"name":name,"arguments":args,"output":output}),
            );
            state.accept(&serde_json::to_vec(&event).unwrap(), cutoff);
        };
        accept(
            "original",
            "function_call",
            "functions.exec_command",
            r#"{"cmd":"Get-Content C:/skills/pdf/SKILL.md; Start-Sleep -Seconds 20"}"#,
            Value::Null,
            10,
        );
        accept(
            "original",
            "function_call_output",
            "",
            "",
            json!(
                "Chunk ID: a\nWall time: 10 seconds\nProcess running with session ID 123\nOutput:\n---\nname: pdf\n---\n# PDF"
            ),
            11,
        );
        accept(
            "poll",
            "function_call",
            "functions.write_stdin",
            r#"{"session_id":123}"#,
            Value::Null,
            12,
        );
        accept(
            "poll",
            "function_call_output",
            "",
            "",
            json!("Process running with session ID 123\nOutput:\n"),
            13,
        );
        accept(
            "unknown",
            "function_call",
            "functions.write_stdin",
            r#"{"session_id":123}"#,
            Value::Null,
            14,
        );
        accept(
            "unknown",
            "function_call_output",
            "",
            "",
            json!({"status":"running"}),
            15,
        );
        assert!(state.tools.is_empty());
        assert_eq!(state.exec_skills.len(), 1);
        // 轮询跨增量读取保留；原始回执 ID 和起止时间不被轮询替换。
        for (id, kind, name, args, output, at) in [
            (
                "final",
                "function_call",
                "functions.write_stdin",
                r#"{"session_id":123}"#,
                Value::Null,
                16,
            ),
            (
                "final",
                "function_call_output",
                "",
                "",
                json!("Process exited with code 1\nOutput:\nrg: missing"),
                20,
            ),
            (
                "repeated",
                "function_call",
                "functions.write_stdin",
                r#"{"session_id":123}"#,
                Value::Null,
                21,
            ),
            (
                "repeated",
                "function_call_output",
                "",
                "",
                json!("Process exited with code 0\nOutput:\n"),
                22,
            ),
        ] {
            state.accept(
                &serde_json::to_vec(&row(
                    "response_item",
                    &time(at),
                    json!({"type":kind,"call_id":id,"name":name,"arguments":args,"output":output}),
                ))
                .unwrap(),
                cutoff,
            );
        }
        assert_eq!(state.tools.len(), 1);
        let tool = &state.tools[0];
        assert_eq!(tool.id, "original");
        assert_eq!(tool.call.timestamp, time(10));
        assert_eq!(tool.output["timestamp"], time(20));
        assert_eq!(tool.output["call_id"], "original");
        assert!(
            tool.output["skillReadEvidence"]
                .as_str()
                .unwrap()
                .contains("name: pdf")
        );
        assert!(
            tool.output["output"]
                .as_str()
                .unwrap()
                .starts_with("Process exited with code 1")
        );
        assert!(state.exec_skills.is_empty());
        assert!(state.exec_polls.is_empty());
        assert!(exec_receipt(&json!("Output:\nProcess running with session ID 123")).is_none());
        assert!(compact_evidence("", &json!("x".repeat(100_000))).len() <= 6001);
    }

    #[test]
    fn async_skill_backfill_handles_appends_compression_and_session_isolation() {
        use std::io::Write;
        for compressed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let original = row(
                "response_item",
                &time(10),
                json!({
                    "type":"function_call","call_id":"original","name":"exec_command",
                    "arguments":r#"{"cmd":"Get-Content C:/skills/pdf/SKILL.md"}"#
                }),
            );
            let running = row(
                "response_item",
                &time(11),
                json!({
                    "type":"function_call_output","call_id":"original",
                    "output":"Process running with session ID 123\nOutput:\n"
                }),
            );
            let mut rows = vec![header(), original.clone(), running.clone()];
            let path = write(temp.path(), SESSION, &rows, compressed);
            let home = fs::canonicalize(temp.path()).unwrap();
            let mut index = Index::default();
            assert!(index.update(&home).tools.is_empty());
            let poll = row(
                "response_item",
                &time(20),
                json!({
                    "type":"function_call","call_id":"poll","name":"write_stdin",
                    "arguments":r#"{"session_id":123}"#
                }),
            );
            let output = row(
                "response_item",
                &time(30),
                json!({
                    "type":"function_call_output","call_id":"poll","output":{"exit_code":0}
                }),
            );
            rows.extend([poll.clone(), output.clone()]);
            if compressed {
                write(temp.path(), SESSION, &rows, true);
            } else {
                let mut file = File::options().append(true).open(&path).unwrap();
                writeln!(file, "{poll}\n{output}").unwrap();
            }
            let mut other_header = header();
            other_header["payload"]["id"] = json!(OTHER);
            // 同进程号只在自己的会话文件内匹配。
            write(
                temp.path(),
                OTHER,
                &[other_header, poll, output],
                compressed,
            );
            let snapshot = index.update(&home);
            assert_eq!(snapshot.tools.len(), 1);
            assert_eq!(snapshot.tools[0].id, "original");
            assert_eq!(snapshot.tools[0].call.session, SESSION);
            assert_eq!(snapshot.tools[0].output["timestamp"], time(30));
            assert_eq!(index.update(&home).bytes_read, 0);
        }
    }

    #[test]
    fn completed_tools_use_start_date_across_midnight() {
        use chrono::TimeZone;
        let date = Local::now().date_naive() - Days::days(1);
        let start = Local
            .from_local_datetime(&date.and_hms_opt(23, 59, 59).unwrap())
            .earliest()
            .unwrap();
        let end = start + Days::seconds(3);
        let mut state = FileState::default();
        state.session = SESSION.into();
        state.accept_tool(
            json!({"type":"function_call","call_id":"midnight",
            "name":"mcp__example__inspect","arguments":"{}"}),
            start.to_rfc3339(),
            date,
        );
        state.accept_tool(
            json!({"type":"function_call_output","call_id":"midnight","output":"ok"}),
            end.to_rfc3339(),
            end.date_naive(),
        );
        assert_eq!(state.tools[0].date, date);
        assert_eq!(state.tools[0].call.timestamp, start.to_rfc3339());
    }

    #[test]
    fn malformed_and_corrupt_logs_report_partial_and_retry_from_header() {
        let temp = tempfile::tempdir().unwrap();
        let path = write(temp.path(), SESSION, &[header(), model()], true);
        fs::write(&path, b"not zstandard").unwrap();
        let mut index = Index::default();
        let home = fs::canonicalize(temp.path()).unwrap();
        assert_eq!(
            index.update(&home).response(&options(), false)["complete"],
            false
        );
        write(
            temp.path(),
            SESSION,
            &[header(), model(), token(&time(10), 50, 50)],
            true,
        );
        let recovered = index.update(&home);
        assert_eq!(recovered.response(&options(), false)["complete"], true);
        assert_eq!(recovered.events[0].delta.total_tokens, 50);
    }

    #[test]
    fn activity_pages_are_bounded_and_matching_revision_skips_unchanged_receipts() {
        let mut snapshot = Snapshot::default();
        snapshot.ready = true;
        snapshot.updated_at = 1234;
        for index in 0..600 {
            snapshot.tools.push(ToolRecord {
                id: format!("call-{index}"),
                call: Call {
                    timestamp: time(index),
                    session: SESSION.into(),
                    payload: json!({"type":"function_call","call_id":format!("call-{index}"),
                        "name":"mcp__test__read","arguments":"{}"}),
                },
                output: json!({"type":"function_call_output","call_id":format!("call-{index}"),
                    "output":"ok"}),
                date: Local::now().date_naive(),
            });
        }
        let first = snapshot.response_page(&options(), false, 1, None, None);
        let second = snapshot.response_page(&options(), false, 2, None, None);
        assert_eq!(first["activityPagination"]["pages"], 2);
        assert_eq!(first["activityRecords"].as_array().unwrap().len(), 512);
        assert_eq!(second["activityRecords"].as_array().unwrap().len(), 88);
        assert_eq!(second["activityRecords"][0]["call"]["call_id"], "call-512");
        let unchanged = snapshot.response_page(&options(), false, 1, Some(1234), None);
        assert_eq!(unchanged["activityPagination"]["unchanged"], true);
        assert!(unchanged["activityRecords"].as_array().unwrap().is_empty());
    }

    #[test]
    fn identical_call_ids_in_different_sessions_are_distinct() {
        let temp = tempfile::tempdir().unwrap();
        for session in [SESSION, OTHER] {
            write(
                temp.path(),
                session,
                &[
                    row("session_meta", &time(0), json!({"id":session})),
                    row(
                        "response_item",
                        &time(10),
                        json!({"type":"function_call",
                        "name":"mcp__test__read","call_id":"call-1","arguments":"{}"}),
                    ),
                    row(
                        "response_item",
                        &time(20),
                        json!({"type":"function_call_output",
                        "call_id":"call-1","output":"ok"}),
                    ),
                ],
                false,
            );
        }
        let snapshot = Index::default().update(&fs::canonicalize(temp.path()).unwrap());
        assert_eq!(snapshot.tools.len(), 2);
        assert_ne!(
            snapshot.tools[0].call.session,
            snapshot.tools[1].call.session
        );
    }
}
