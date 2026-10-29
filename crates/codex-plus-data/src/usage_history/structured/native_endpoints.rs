//! 原生直连没有经过协议代理时，使用实际 HTTP 完成日志补充归属。
use super::*;
use rusqlite::{Connection, OpenFlags, params};
use std::collections::BTreeSet;

const WINDOW_MS: i64 = 600_000;
const MAX_ROWS: usize = 20_000;
const MAX_SESSION_ROWS: usize = 2_000;

pub(super) fn profiles(
    path: &Path,
) -> anyhow::Result<Vec<codex_plus_core::settings::RelayProfile>> {
    #[derive(Deserialize)]
    struct Profiles {
        #[serde(rename = "relayProfiles", default)]
        profiles: Vec<codex_plus_core::settings::RelayProfile>,
    }
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 {
        anyhow::bail!("supplier settings exceed read budget");
    }
    Ok(serde_json::from_reader::<_, Profiles>(file.take(8 * 1024 * 1024 + 1))?.profiles)
}

pub(super) fn log_paths(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut paths: Vec<_> = fs::read_dir(root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|s| s.to_str())
                .and_then(|name| name.strip_prefix("logs_")?.strip_suffix(".sqlite"))
                .is_some_and(|v| !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()))
        })
        .collect();
    paths.sort();
    if paths.len() > 4 {
        anyhow::bail!("too many native log databases");
    }
    Ok(paths)
}

fn log_paths_for_home(home: &Path, explicit: Option<&Path>) -> anyhow::Result<Vec<PathBuf>> {
    // 显式目录尚未创建也不能回退，避免读取另一实例的日志。
    log_paths(
        explicit
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(home),
    )
}

pub(super) fn open_log(path: &Path) -> anyhow::Result<Connection> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    db.busy_timeout(Duration::from_millis(50))?;
    Ok(db)
}

pub(super) fn source_identity(path: &Path) -> anyhow::Result<String> {
    let file = File::open(path)?;
    let (volume, index) = codex_plus_core::codex_sqlite::opened_file_identity(&file)?;
    Ok(format!(
        "{}:{volume}:{index}",
        fs::canonicalize(path)?.to_string_lossy()
    ))
}

#[derive(Clone, Deserialize, serde::Serialize)]
pub(super) struct Attribution {
    key: RequestMatchKey,
    pub(super) at: i64,
    turn: String,
    provider: String,
    #[serde(default)]
    version: u8,
    #[serde(default)]
    binding_since: i64,
    #[serde(default)]
    rejected: bool,
}

impl Attribution {
    pub(super) fn valid(&self) -> bool {
        self.version == 2
            && self.binding_since > 0
            && self.at >= self.binding_since
            && !unknown_provider(&self.provider)
            && [
                &self.key.session,
                &self.key.model,
                &self.turn,
                &self.provider,
            ]
            .iter()
            .all(|s| !s.is_empty() && s.len() <= 256)
            && self.key.cached <= self.key.input
            && self
                .key
                .input
                .checked_add(self.key.output)
                .is_some_and(|n| n > 0)
    }
}

#[derive(Default)]
pub(super) struct Cache {
    pub(super) verified: HashMap<[u8; 32], Attribution>,
    negative: HashMap<[u8; 32], Instant>,
    binding_since: i64,
    cursor: usize,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq, Eq)]
pub(super) struct Request {
    pub(super) session: String,
    turn: String,
    model: String,
    pub(super) at: i64,
    provider: Option<String>,
    pub(super) binding_since: i64,
    identity: [u8; 32],
    #[serde(default)]
    source: String,
    #[serde(default)]
    row_id: i64,
}

impl Request {
    pub(super) fn valid(&self) -> bool {
        self.at > 0
            && self.binding_since > 0
            && [&self.session, &self.turn, &self.model]
                .iter()
                .all(|s| !s.is_empty() && s.len() <= 256)
            && self.provider.as_ref().is_none_or(|p| {
                !unknown_provider(p) && p.len() <= 256 && self.at >= self.binding_since
            })
    }
}

// 日志字段只解析独立键；正文、请求头和错误文本不作为身份来源。
fn field(text: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=");
    let mut found = None;
    for (start, _) in text.match_indices(&needle) {
        if start > 0 && !matches!(text.as_bytes()[start - 1], b' ' | b'{' | b'\n') {
            continue;
        }
        let value = &text[start + needle.len()..];
        let value = if value.starts_with('"') {
            let mut escaped = false;
            let end = value.char_indices().skip(1).find_map(|(i, ch)| {
                if escaped {
                    escaped = false;
                    return None;
                }
                if ch == '\\' {
                    escaped = true;
                    return None;
                }
                (ch == '"').then_some(i)
            })?;
            serde_json::from_str::<String>(&value[..=end]).ok()?
        } else {
            value
                .split(|c: char| c.is_whitespace() || matches!(c, '}' | ','))
                .next()?
                .to_owned()
        };
        if value.is_empty() || value.len() > 2048 {
            return None;
        }
        if found.as_ref().is_some_and(|previous| previous != &value) {
            return None;
        }
        found = Some(value);
    }
    found
}

fn public_endpoint(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    !authority.is_empty()
        && !authority.contains('@')
        && !value.contains(['?', '#', '\\'])
        && !value.chars().any(char::is_control)
        && !["localhost", "127.0.0.1", "[::1]"]
            .iter()
            .any(|host| authority == *host || authority.starts_with(&format!("{host}:")))
}

pub(super) fn endpoints(
    profiles: &[codex_plus_core::settings::RelayProfile],
) -> BTreeMap<String, Option<String>> {
    let mut result = BTreeMap::new();
    for profile in profiles {
        if profile.relay_mode == codex_plus_core::settings::RelayMode::Aggregate
            || unknown_provider(&profile.id)
        {
            continue;
        }
        let base = codex_plus_core::relay_config::relay_profile_base_url(profile);
        let endpoint = codex_plus_core::protocol_proxy::responses_url(&base);
        if !public_endpoint(&endpoint) {
            continue;
        }
        result
            .entry(endpoint)
            .and_modify(|previous: &mut Option<String>| {
                if previous.as_deref() != Some(&profile.id) {
                    *previous = None;
                }
            })
            .or_insert_with(|| Some(profile.id.clone()));
    }
    result
}

pub(super) fn request(
    body: &str,
    session: &str,
    at: i64,
    endpoints: &BTreeMap<String, Option<String>>,
) -> Option<Request> {
    let (spans, message) = body.rsplit_once("}: Request completed ")?;
    // 只读取完成消息的固定字段；后续响应头中同名字段不能覆盖这些值。
    let message = message.split(" headers=").next()?;
    if field(message, "method")?.as_str() != "POST" {
        return None;
    }
    let status = field(message, "status")?.parse::<u16>().ok()?;
    if !(200..300).contains(&status) {
        return None;
    }
    let url = field(message, "url")?;
    if !url.ends_with("/responses") {
        return None;
    }
    let thread = field(spans, "thread.id").or_else(|| field(spans, "thread_id"))?;
    if thread != session {
        return None;
    }
    let turn = field(spans, "turn.id")?;
    let model = field(spans, "model")?;
    Some(Request {
        session: session.to_owned(),
        turn,
        model: request_base_model(&model).to_ascii_lowercase(),
        at,
        provider: endpoints.get(&url).cloned().flatten(),
        binding_since: 0,
        identity: Sha256::digest(body.as_bytes()).into(),
        source: String::new(),
        row_id: 0,
    })
}

pub(super) fn identify(mut row: Request, source: &str, id: i64) -> Request {
    row.source = source.to_owned();
    row.row_id = id;
    row
}

pub(super) fn bind(mut row: Request, since: i64) -> Request {
    row.binding_since = since;
    if row.at < since {
        row.provider = None;
    }
    row
}

fn event_key(
    event: &UsageEvent,
    aliases: &RequestSessionAliases,
) -> Option<(RequestMatchKey, i64)> {
    Some((
        RequestMatchKey {
            session: aliases.resolve(&event.session)?,
            model: request_base_model(&event.model).to_ascii_lowercase(),
            input: event.delta.input_tokens,
            output: event.delta.output_tokens,
            cached: event.delta.cached_input_tokens,
        },
        DateTime::parse_from_rfc3339(&event.timestamp)
            .ok()?
            .timestamp_millis(),
    ))
}

impl Cache {
    pub(super) fn attribute(
        &mut self,
        home: &Path,
        events: &mut [UsageEvent],
        endpoints: &BTreeMap<String, Option<String>>,
        binding_since: i64,
        session_aliases: &BTreeMap<String, String>,
        captured: &provider_cache::ProviderCache,
    ) -> Value {
        let aliases = RequestSessionAliases::new(session_aliases);
        let keys: Vec<_> = events
            .iter()
            .map(|event| event_key(event, &aliases))
            .collect();
        let now = Local::now().timestamp_millis();
        let cutoff = provider_cache::ProviderCache::since(
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        );
        self.verified.retain(|_, proof| proof.at >= cutoff);
        if self.binding_since != binding_since {
            self.negative.clear();
            self.binding_since = binding_since;
        }
        self.negative
            .retain(|_, at| at.elapsed() < Duration::from_secs(300));
        let mut targets: BTreeMap<String, (i64, i64)> = BTreeMap::new();
        let mut live_targets = HashSet::new();
        for (event, key) in events.iter().zip(&keys) {
            let Some((key, at)) = key else {
                continue;
            };
            if !unknown_provider(&event.provider) || event.turn.is_empty() {
                continue;
            }
            if self.negative.contains_key(&event.id) {
                continue;
            }
            let saved = self.verified.get(&event.id);
            let retained = saved.is_some_and(|s| {
                !s.rejected && &s.key == key && s.at == *at && s.turn == event.turn
            }) && now.saturating_sub(*at) > WINDOW_MS;
            if !retained {
                live_targets.insert(key.session.clone());
            }
            targets
                .entry(key.session.clone())
                .and_modify(|(since, until)| {
                    *since = (*since).min(*at);
                    *until = (*until).max(*at);
                })
                .or_insert((*at, *at));
        }
        let mut records = Vec::new();
        let mut reads = 0usize;
        let mut limited = false;
        let mut unavailable = false;
        let mut coverage = BTreeMap::new();
        let mut checked = HashSet::new();
        let mut captured_reads = 0usize;
        let ordered: Vec<_> = targets.iter().collect();
        let selected: Vec<_> = (0..ordered.len().min(256))
            .map(|offset| ordered[(self.cursor + offset) % ordered.len()])
            .collect();
        // 反向展开已解析的别名；查询原始日志 ID，匹配时统一使用规范 ID。
        let mut query_sessions: BTreeMap<String, BTreeSet<String>> = selected
            .iter()
            .map(|(session, _)| ((*session).clone(), BTreeSet::from([(*session).clone()])))
            .collect();
        for (raw, canonical) in &aliases.canonical {
            if let Some(sessions) = canonical.as_ref().and_then(|id| query_sessions.get_mut(id)) {
                sessions.insert(raw.clone());
            }
        }
        for raw in session_aliases
            .keys()
            .chain(session_aliases.values())
            .chain(events.iter().map(|event| &event.session))
        {
            if let Some(sessions) = aliases
                .resolve(raw)
                .and_then(|id| query_sessions.get_mut(&id))
            {
                sessions.insert(raw.clone());
            }
        }
        // 已采集的请求可早于当前配置的观测 epoch；每条证据保留自己的原始映射。
        for (session, (since, until)) in &selected {
            let mut floor = i64::MIN;
            let mut complete = true;
            let mut available = false;
            for raw in &query_sessions[*session] {
                match captured.captured_native(raw, since.saturating_sub(WINDOW_MS), *until) {
                    Ok(batch) if records.len() + batch.rows.len() <= MAX_ROWS => {
                        available = true;
                        floor = floor.max(batch.floor);
                        complete &= batch.complete;
                        captured_reads += batch.rows.len();
                        records.extend(batch.rows);
                    }
                    Ok(_) => {
                        available = true;
                        limited = true;
                        floor = i64::MAX;
                        complete = false;
                    }
                    Err(_) => complete = false,
                }
            }
            if available {
                coverage.insert((*session).clone(), (floor, complete));
            }
        }
        if !targets.is_empty() {
            let explicit = std::env::var_os("CODEX_SQLITE_HOME").map(PathBuf::from);
            let paths = match log_paths_for_home(home, explicit.as_deref()) {
                Ok(paths) => paths,
                Err(_) => {
                    unavailable = true;
                    Vec::new()
                }
            };
            let mut live_complete = !paths.is_empty();
            for path in paths {
                let result = (|| -> anyhow::Result<()> {
                    let source = source_identity(&path)?;
                    let db = open_log(&path)?;
                    let snapshot = db.unchecked_transaction()?;
                    let mut statement = db.prepare(
                        "SELECT ts,ts_nanos,substr(feedback_log_body,1,16385),thread_id,id
                         FROM logs WHERE thread_id=?1 AND ts>=?2 AND ts<=?3
                         AND target='codex_http_client::client' ORDER BY ts,id LIMIT ?4",
                    )?;
                    for (session, (since, until)) in &selected {
                        if !live_targets.contains(*session) {
                            continue;
                        }
                        if reads >= MAX_ROWS {
                            limited = true;
                            coverage.insert((*session).clone(), (i64::MAX, false));
                            continue;
                        }
                        let mut session_reads = 0;
                        for raw in &query_sessions[*session] {
                            let rows = statement.query_map(
                                params![
                                    raw,
                                    since.saturating_sub(WINDOW_MS) / 1000,
                                    until / 1000 + 1,
                                    (MAX_SESSION_ROWS + 1) as i64
                                ],
                                |row| {
                                    Ok((
                                        row.get::<_, i64>(0)?,
                                        row.get::<_, i64>(1)?,
                                        row.get::<_, String>(2)?,
                                        row.get::<_, String>(3)?,
                                        row.get::<_, i64>(4)?,
                                    ))
                                },
                            )?;
                            for row in rows {
                                let (seconds, nanos, body, raw_session, id) = row?;
                                reads += 1;
                                session_reads += 1;
                                if session_reads > MAX_SESSION_ROWS
                                    || reads > MAX_ROWS
                                    || body.len() > 16384
                                {
                                    limited = true;
                                    coverage.insert((*session).clone(), (i64::MAX, false));
                                    break;
                                }
                                let Some(at) = seconds
                                    .checked_mul(1000)
                                    .and_then(|s| s.checked_add(nanos / 1_000_000))
                                else {
                                    continue;
                                };
                                if let Some(record) = request(&body, &raw_session, at, endpoints) {
                                    records.push(identify(
                                        bind(record, binding_since),
                                        &source,
                                        id,
                                    ));
                                }
                            }
                            if session_reads > MAX_SESSION_ROWS || reads > MAX_ROWS {
                                break;
                            }
                        }
                        checked.insert((*session).clone());
                    }
                    drop(statement);
                    snapshot.commit()?;
                    anyhow::ensure!(
                        source_identity(&path)? == source,
                        "native log replaced during read"
                    );
                    Ok(())
                })();
                unavailable |= result.is_err();
                live_complete &= result.is_ok();
            }
            if live_complete {
                for (session, _) in &selected {
                    if live_targets.contains(*session) {
                        coverage
                            .entry((*session).clone())
                            .or_insert((i64::MIN, true));
                    }
                }
            }
            limited |= targets.len() > 256;
            self.cursor = self.cursor.wrapping_add(targets.len().min(256));
        }
        // 部分读取不能建立唯一性；旧的独立证据仍可保留。
        if unavailable {
            // 成功读到的反证必须保留；其他来源失败仅禁止建立新的唯一性证明。
            for (_, complete) in coverage.values_mut() {
                *complete = false;
            }
            checked.clear();
        }
        // 同一请求可能同时来自完成缓存和尚未轮转的日志，保留采集时的映射。
        let mut unique = BTreeMap::new();
        for mut row in records {
            let Some(session) = aliases.resolve(&row.session) else {
                continue;
            };
            row.session = session;
            unique
                .entry((row.source.clone(), row.row_id, row.at, row.identity))
                .or_insert(row);
        }
        let records: Vec<_> = unique.into_values().collect();
        let stats = self.apply(events, &keys, &records, binding_since, &coverage);
        for (event, key) in events.iter().zip(&keys) {
            if key.as_ref().is_some_and(|(key, at)| {
                checked.contains(&key.session)
                    && coverage.get(&key.session).is_some_and(|(floor, complete)| {
                        *complete && at.saturating_sub(WINDOW_MS) >= *floor
                    })
            }) && unknown_provider(&event.provider)
                && now.saturating_sub(key.as_ref().unwrap().1) > WINDOW_MS
                && self.negative.len() < MAX_EVENTS
            {
                self.negative.insert(event.id, Instant::now());
            }
        }
        json!({"source":"native-http-endpoint","recoveredEvents":stats.0,"recoveredTokens":stats.1,
            "logRowsRead":reads,"capturedRequestsRead":captured_reads,"limited":limited,"unavailable":unavailable,
            "cachedEvents":self.verified.len()})
    }

    fn apply(
        &mut self,
        events: &mut [UsageEvent],
        keys: &[Option<(RequestMatchKey, i64)>],
        records: &[Request],
        binding_since: i64,
        coverage: &BTreeMap<String, (i64, bool)>,
    ) -> (usize, u64) {
        type Group = (String, String, String);
        let mut requests: BTreeMap<Group, Vec<&Request>> = BTreeMap::new();
        for row in records {
            requests
                .entry((row.session.clone(), row.turn.clone(), row.model.clone()))
                .or_default()
                .push(row);
        }
        for rows in requests.values_mut() {
            rows.sort_by_key(|row| row.at);
        }
        let mut times: BTreeMap<(String, String), Vec<i64>> = BTreeMap::new();
        for (event, key) in events.iter().zip(keys) {
            if let Some((key, at)) = key {
                times
                    .entry((key.session.clone(), event.turn.clone()))
                    .or_default()
                    .push(*at);
            }
        }
        for rows in times.values_mut() {
            rows.sort_unstable();
        }
        let (mut recovered, mut tokens) = (0, 0u64);
        for (event, key) in events.iter_mut().zip(keys) {
            if !unknown_provider(&event.provider) || event.turn.is_empty() {
                continue;
            }
            let Some((key, at)) = key else {
                continue;
            };
            let group = (key.session.clone(), event.turn.clone(), key.model.clone());
            let boundaries = &times[&(key.session.clone(), event.turn.clone())];
            let position = boundaries.partition_point(|time| time < at);
            if boundaries.partition_point(|time| time <= at) - position != 1 {
                continue;
            }
            let floor = position
                .checked_sub(1)
                .map(|i| boundaries[i])
                .unwrap_or(i64::MIN)
                .max(at.saturating_sub(WINDOW_MS));
            let empty = Vec::new();
            let candidates = requests.get(&group).unwrap_or(&empty);
            let start = candidates.partition_point(|row| row.at <= floor);
            let end = candidates.partition_point(|row| row.at <= *at);
            let rows = &candidates[start..end];
            let candidate = rows
                .first()
                .and_then(|first| first.provider.as_ref())
                .filter(|provider| {
                    rows.iter().all(|row| {
                        row.provider.as_ref() == Some(*provider)
                            && row.at
                                >= if row.binding_since > 0 {
                                    row.binding_since
                                } else {
                                    binding_since
                                }
                    })
                });
            if let Some(saved) = self.verified.get_mut(&event.id) {
                if saved.key != *key
                    || saved.at != *at
                    || saved.turn != event.turn
                    || !rows.is_empty() && candidate != Some(&saved.provider)
                {
                    saved.rejected = true;
                }
            }
            let complete = coverage
                .get(&key.session)
                .is_some_and(|(since, complete)| *complete && floor >= *since);
            if let Some(provider) =
                candidate.filter(|_| complete && self.verified.len() < MAX_EVENTS)
            {
                let since = rows
                    .iter()
                    .map(|row| {
                        if row.binding_since > 0 {
                            row.binding_since
                        } else {
                            binding_since
                        }
                    })
                    .max()
                    .unwrap_or(binding_since);
                self.verified
                    .entry(event.id)
                    .or_insert_with(|| Attribution {
                        key: key.clone(),
                        at: *at,
                        turn: event.turn.clone(),
                        provider: provider.clone(),
                        version: 2,
                        binding_since: since,
                        rejected: false,
                    });
            }
            if let Some(saved) = self.verified.get(&event.id).filter(|s| !s.rejected) {
                event.provider.clone_from(&saved.provider);
                recovered += 1;
                tokens = tokens.saturating_add(event.delta.total_tokens);
            }
        }
        (recovered, tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage_event(id: u8, at: i64, turn: &str, model: &str, provider: &str) -> UsageEvent {
        UsageEvent {
            id: [id; 32],
            session: "session".into(),
            turn: turn.into(),
            model: model.into(),
            timestamp: DateTime::from_timestamp_millis(at).unwrap().to_rfc3339(),
            date: Local::now().date_naive(),
            total: Tokens::default(),
            delta: Tokens {
                input_tokens: 100,
                cached_input_tokens: 50,
                output_tokens: 10,
                total_tokens: 110,
                ..Default::default()
            },
            has_file_baseline: true,
            provider: provider.into(),
            role: "main".into(),
            inherited: false,
            legacy_identity: None,
        }
    }

    fn matching_request(at: i64, provider: Option<&str>) -> Request {
        Request {
            session: "session".into(),
            turn: "turn".into(),
            model: "model".into(),
            at,
            provider: provider.map(str::to_owned),
            binding_since: 0,
            identity: [0; 32],
            source: "source".into(),
            row_id: at,
        }
    }

    fn apply(cache: &mut Cache, events: &mut [UsageEvent], requests: &[Request]) -> (usize, u64) {
        let aliases = RequestSessionAliases::new(&BTreeMap::new());
        let keys: Vec<_> = events.iter().map(|e| event_key(e, &aliases)).collect();
        cache.apply(
            events,
            &keys,
            requests,
            1,
            &BTreeMap::from([("session".into(), (i64::MIN, true))]),
        )
    }

    fn attribute_alias_requests(
        requests: &[(&str, &str, bool)],
        sessions: &[&str],
        aliases: &BTreeMap<String, String>,
    ) -> (Value, Vec<UsageEvent>) {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis() - 1000;
        let logs = Connection::open(temp.path().join("logs_2.sqlite")).unwrap();
        logs.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,feedback_log_body TEXT,thread_id TEXT,target TEXT);").unwrap();
        let mut captured = provider_cache::ProviderCache::default();
        captured
            .open(
                temp.path(),
                Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
            )
            .unwrap();
        let mut endpoints = BTreeMap::new();
        let mut rows = Vec::new();
        for (index, (session, provider, capture)) in requests.iter().enumerate() {
            let id = index as i64 + 1;
            let url = format!("https://{provider}.example/v1/responses");
            endpoints.insert(url.clone(), Some((*provider).to_owned()));
            let body = format!(
                "turn{{thread.id={session} turn.id=turn model=model}}: Request completed method=POST url={url} status=200 headers={{}}"
            );
            if *capture {
                rows.push((
                    Sha256::digest(id.to_le_bytes()).into(),
                    identify(
                        bind(
                            request(&body, session, at - 1000, &endpoints).unwrap(),
                            at - 2000,
                        ),
                        "captured-source",
                        id,
                    ),
                ));
            } else {
                logs.execute(
                    "INSERT INTO logs VALUES(?1,?2,0,?3,?4,'codex_http_client::client')",
                    params![id, (at - 1000) / 1000, body, session],
                )
                .unwrap();
            }
        }
        drop(logs);
        if !rows.is_empty() {
            captured
                .capture_native(
                    "captured-source",
                    requests.len() as i64,
                    &rows,
                    true,
                    at - WINDOW_MS - 1,
                    "anchor",
                )
                .unwrap();
        }
        let mut events: Vec<_> = sessions
            .iter()
            .enumerate()
            .map(|(index, session)| {
                let mut event = usage_event(index as u8 + 1, at, "turn", "model", "");
                event.session = (*session).to_owned();
                event
            })
            .collect();
        let before: Vec<_> = events
            .iter()
            .map(|event| (event.total, event.delta))
            .collect();
        let result = Cache::default().attribute(
            temp.path(),
            &mut events,
            &endpoints,
            at - 2000,
            aliases,
            &captured,
        );
        assert_eq!(
            events
                .iter()
                .map(|event| (event.total, event.delta))
                .collect::<Vec<_>>(),
            before
        );
        (result, events)
    }

    #[test]
    fn native_alias_queries_match_both_directions_and_chains_live_and_captured() {
        let aliases = BTreeMap::from([
            ("session-b".into(), "session-c".into()),
            ("session-c".into(), "session-a".into()),
        ]);
        for captured in [false, true] {
            for (raw, event) in [
                ("session-b", "session-a"),
                ("session-a", "session-b"),
                ("session-c", "session-a"),
            ] {
                let (result, events) =
                    attribute_alias_requests(&[(raw, "supplier-a", captured)], &[event], &aliases);
                assert_eq!(result["recoveredEvents"], 1, "{raw} -> {event}, {captured}");
                assert_eq!(events[0].provider, "supplier-a");
                assert_eq!(events[0].session, event);
                assert_eq!(result["capturedRequestsRead"], usize::from(captured));
                assert_eq!(result["logRowsRead"], usize::from(!captured));
            }
        }
    }

    #[test]
    fn native_alias_queries_preserve_original_uuid_spelling() {
        let canonical = "11111111-1111-4111-8111-111111111111";
        let raw = "local:AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA";
        let aliases = BTreeMap::from([(raw.into(), canonical.into())]);
        for captured in [false, true] {
            let (result, events) =
                attribute_alias_requests(&[(raw, "supplier-a", captured)], &[canonical], &aliases);
            assert_eq!(result["recoveredEvents"], 1);
            assert_eq!(events[0].provider, "supplier-a");
        }
    }

    #[test]
    fn native_alias_conflicts_reject_across_live_and_captured_without_mixing_sessions() {
        let aliases = BTreeMap::from([
            ("session-b".into(), "session-a".into()),
            ("session-c".into(), "session-a".into()),
        ]);
        for first_captured in [false, true] {
            for second_captured in [false, true] {
                for first in ["session-a", "session-b"] {
                    let (result, events) = attribute_alias_requests(
                        &[
                            (first, "supplier-a", first_captured),
                            ("session-c", "supplier-b", second_captured),
                            ("independent", "supplier-c", first_captured),
                        ],
                        &["session-a", "independent"],
                        &aliases,
                    );
                    assert_eq!(result["recoveredEvents"], 1);
                    assert!(unknown_provider(&events[0].provider));
                    assert_eq!(events[1].provider, "supplier-c");
                }
            }
        }
    }

    #[test]
    fn native_alias_duplicate_events_and_invalid_components_remain_rejected() {
        for captured in [false, true] {
            let aliases = BTreeMap::from([("session-b".into(), "session-a".into())]);
            let (result, events) = attribute_alias_requests(
                &[("session-b", "supplier-a", captured)],
                &["session-a", "session-b"],
                &aliases,
            );
            assert_eq!(result["recoveredEvents"], 0);
            assert!(events.iter().all(|event| unknown_provider(&event.provider)));
            let cycle = BTreeMap::from([
                ("session-b".into(), "session-a".into()),
                ("session-a".into(), "session-b".into()),
            ]);
            let (result, events) = attribute_alias_requests(
                &[("session-b", "supplier-a", captured)],
                &["session-a"],
                &cycle,
            );
            assert_eq!(result["recoveredEvents"], 0);
            assert!(unknown_provider(&events[0].provider));
        }
    }

    #[test]
    fn native_alias_live_queries_share_the_canonical_session_read_budget() {
        let aliases = BTreeMap::from([
            ("session-b".into(), "session-a".into()),
            ("session-c".into(), "session-a".into()),
        ]);
        let mut requests = vec![("session-b", "supplier-a", false); 1001];
        requests.extend(vec![("session-c", "supplier-a", false); 1000]);
        requests.push(("z-independent", "supplier-b", false));
        let (result, events) =
            attribute_alias_requests(&requests, &["session-a", "z-independent"], &aliases);
        assert_eq!(result["limited"], true);
        assert_eq!(result["recoveredEvents"], 1);
        assert!(unknown_provider(&events[0].provider));
        assert_eq!(events[1].provider, "supplier-b");
    }

    #[test]
    fn native_explicit_sqlite_path_never_falls_back_when_missing_or_not_a_directory() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let log = home.join("logs_2.sqlite");
        drop(Connection::open(&log).unwrap());
        let missing = temp.path().join("not-created");
        assert!(log_paths_for_home(&home, Some(&missing)).is_err());
        assert!(log_paths_for_home(&home, Some(&log)).is_err());
        assert_eq!(log_paths_for_home(&home, None).unwrap(), vec![log.clone()]);
        assert_eq!(
            log_paths_for_home(&home, Some(Path::new(""))).unwrap(),
            vec![log]
        );
        assert!(!missing.exists());
        let explicit = temp.path().join("explicit");
        fs::create_dir(&explicit).unwrap();
        assert!(
            log_paths_for_home(&home, Some(&explicit))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn native_fields_are_exact_and_conflicting_values_are_rejected() {
        assert_eq!(
            field("turn.id=\"turn-1\" turn.id=turn-1", "turn.id").as_deref(),
            Some("turn-1")
        );
        assert_eq!(
            field("notmodel=wrong model=right", "model").as_deref(),
            Some("right")
        );
        assert!(field("model=one model=two", "model").is_none());
        assert!(field("model=\"unterminated", "model").is_none());
    }

    #[test]
    fn only_actual_successful_response_urls_are_accepted() {
        let urls = BTreeMap::from([(
            "https://relay.example/v1/responses".into(),
            Some("supplier-a".into()),
        )]);
        let body = "session_loop{thread_id=session}: turn{thread.id=session turn.id=turn model=model}: Request completed method=POST url=https://relay.example/v1/responses status=200 headers={model=untrusted}";
        let row = request(body, "session", 10, &urls).unwrap();
        assert_eq!(row.provider.as_deref(), Some("supplier-a"));
        assert!(
            request(
                &body.replace("status=200", "status=502"),
                "session",
                10,
                &urls
            )
            .is_none()
        );
        assert!(request(body, "other-session", 10, &urls).is_none());
        assert!(
            request(
                &body.replace("Request completed", "Request failed"),
                "session",
                10,
                &urls
            )
            .is_none()
        );
        let unknown = body.replace("relay.example/", "relay.example.attacker/");
        assert!(
            request(&unknown, "session", 10, &urls)
                .unwrap()
                .provider
                .is_none()
        );
        assert!(!public_endpoint(
            "https://user:secret@relay.example/v1/responses"
        ));
        assert!(!public_endpoint("https://localhost:123/v1/responses"));
        assert!(!public_endpoint(
            "https://relay.example/v1/responses?key=secret"
        ));
    }

    #[test]
    fn native_evidence_respects_turn_model_boundaries_and_known_suppliers() {
        let mut cache = Cache::default();
        let mut events = vec![
            usage_event(1, 10000, "turn", "model", "custom"),
            usage_event(2, 12000, "turn", "other-model", "custom"),
            usage_event(3, 14000, "turn", "model", "custom"),
            usage_event(4, 20000, "other-turn", "model", "custom"),
            usage_event(5, 30000, "turn", "model", "explicit"),
        ];
        let before: Vec<_> = events.iter().map(|e| (e.total, e.delta)).collect();
        assert_eq!(
            apply(
                &mut cache,
                &mut events,
                &[matching_request(9000, Some("supplier-a"))]
            ),
            (1, 110)
        );
        assert_eq!(events[0].provider, "supplier-a");
        assert_eq!(events[1].provider, "custom");
        assert_eq!(
            events[2].provider, "custom",
            "a prior model's request cannot leak past another usage event"
        );
        assert_eq!(events[3].provider, "custom");
        assert_eq!(events[4].provider, "explicit");
        assert_eq!(
            events
                .iter()
                .map(|e| (e.total, e.delta))
                .collect::<Vec<_>>(),
            before
        );
    }

    #[test]
    fn ambiguous_or_unmapped_requests_and_duplicate_completions_are_not_classified() {
        for requests in [
            vec![
                matching_request(9000, Some("a")),
                matching_request(9500, Some("b")),
            ],
            vec![
                matching_request(9000, Some("a")),
                matching_request(9500, None),
            ],
        ] {
            let mut events = vec![usage_event(1, 10000, "turn", "model", "")];
            assert_eq!(apply(&mut Cache::default(), &mut events, &requests).0, 0);
        }
        let mut events = vec![
            usage_event(1, 10000, "turn", "model", ""),
            usage_event(2, 10000, "turn", "model", ""),
        ];
        assert_eq!(
            apply(
                &mut Cache::default(),
                &mut events,
                &[matching_request(9000, Some("a"))]
            )
            .0,
            0
        );
    }

    #[test]
    fn endpoint_mapping_does_not_apply_to_usage_before_observation() {
        let mut events = vec![usage_event(1, 10000, "turn", "model", "")];
        let aliases = RequestSessionAliases::new(&BTreeMap::new());
        let keys: Vec<_> = events.iter().map(|e| event_key(e, &aliases)).collect();
        let mut cache = Cache::default();
        let requests = [matching_request(9000, Some("supplier-a"))];
        let coverage = BTreeMap::from([("session".into(), (i64::MIN, true))]);
        assert_eq!(
            cache
                .apply(&mut events, &keys, &requests, 11000, &coverage)
                .0,
            0
        );
        assert_eq!(
            cache
                .apply(&mut events, &keys, &requests, 8000, &coverage)
                .0,
            1
        );
    }

    #[test]
    fn endpoint_observation_survives_restart_and_changes_with_mapping() {
        let temp = tempfile::tempdir().unwrap();
        let cutoff = Local::now().date_naive();
        let endpoints = BTreeMap::from([(
            "https://relay.example/v1/responses".into(),
            Some("a".into()),
        )]);
        let mut db = provider_cache::ProviderCache::default();
        db.open(temp.path(), cutoff).unwrap();
        let since = db.endpoint_binding_since(&endpoints).unwrap();
        assert!(since > 0);
        drop(db);
        let mut restored = provider_cache::ProviderCache::default();
        restored.open(temp.path(), cutoff).unwrap();
        assert_eq!(restored.endpoint_binding_since(&endpoints).unwrap(), since);
        let changed = BTreeMap::from([(
            "https://relay.example/v1/responses".into(),
            Some("b".into()),
        )]);
        assert!(restored.endpoint_binding_since(&changed).unwrap() >= since);
    }

    #[test]
    fn retained_proofs_survive_log_rotation_and_revoke_changed_signatures_or_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let mut cache = Cache::default();
        let mut events = vec![usage_event(1, 10000, "turn", "model", "")];
        assert_eq!(
            apply(
                &mut cache,
                &mut events,
                &[matching_request(9000, Some("a"))]
            )
            .0,
            1
        );
        let mut db = provider_cache::ProviderCache::default();
        db.open(
            temp.path(),
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        )
        .unwrap();
        assert_eq!(db.save_native(&cache.verified).unwrap(), 1);
        assert_eq!(db.save_native(&cache.verified).unwrap(), 0);
        // 测试时间使用固定过去值，加载覆盖该证据的保留区间。
        let mut restored = Cache {
            verified: db
                .load_native(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap())
                .unwrap(),
            ..Default::default()
        };
        events[0].provider.clear();
        assert_eq!(apply(&mut restored, &mut events, &[]).0, 1);
        events[0].provider.clear();
        events[0].delta.input_tokens += 1;
        assert_eq!(apply(&mut restored, &mut events, &[]).0, 0);
        events[0].delta.input_tokens -= 1;
        assert_eq!(
            apply(
                &mut restored,
                &mut events,
                &[matching_request(9000, Some("b"))]
            )
            .0,
            0
        );
        assert_eq!(db.save_native(&restored.verified).unwrap(), 1);
    }

    #[test]
    fn actual_native_sqlite_is_read_only_and_supplies_only_matching_turns() {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis() - 1000;
        let path = temp.path().join("logs_2.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,feedback_log_body TEXT,thread_id TEXT,target TEXT);").unwrap();
        let body = "session_loop{thread_id=session}: turn{thread.id=session turn.id=turn model=model}: Request completed method=POST url=https://relay.example/v1/responses status=200 headers={authorization=secret}";
        db.execute(
            "INSERT INTO logs VALUES(1,?1,0,?2,'session','codex_http_client::client')",
            params![(at - 1000) / 1000, body],
        )
        .unwrap();
        db.execute(
            "WITH RECURSIVE seq(n) AS (SELECT 2 UNION ALL SELECT n+1 FROM seq WHERE n<2100)
             INSERT INTO logs SELECT n,?1,0,?2,'session','codex_http_client::client' FROM seq",
            params![at / 1000 + 1000, body],
        )
        .unwrap();
        drop(db);
        let before = fs::read(&path).unwrap();
        let mut events = vec![usage_event(1, at, "turn", "model", "")];
        let endpoints = BTreeMap::from([(
            "https://relay.example/v1/responses".into(),
            Some("supplier-a".into()),
        )]);
        let captured = provider_cache::ProviderCache::default();
        let stale = Cache::default().attribute(
            temp.path(),
            &mut events,
            &endpoints,
            at - 500,
            &BTreeMap::new(),
            &captured,
        );
        assert_eq!(
            stale["recoveredEvents"], 0,
            "a completion after observation cannot promote an earlier HTTP request"
        );
        let result = Cache::default().attribute(
            temp.path(),
            &mut events,
            &endpoints,
            at - 10000,
            &BTreeMap::new(),
            &captured,
        );
        assert_eq!(result["recoveredEvents"], 1);
        assert_eq!(
            result["logRowsRead"], 1,
            "future rows must not consume the read budget"
        );
        assert_eq!(events[0].provider, "supplier-a");
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn one_truncated_session_does_not_block_other_sessions() {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis() - 1000;
        let db = Connection::open(temp.path().join("logs_2.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,feedback_log_body TEXT,thread_id TEXT,target TEXT);").unwrap();
        let body = "session_loop{thread_id=session}: turn{thread.id=session turn.id=turn model=model}: Request completed method=POST url=https://relay.example/v1/responses status=200 headers={}";
        db.execute(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<=2000)
             INSERT INTO logs SELECT n,?1,0,?2,'session','codex_http_client::client' FROM seq",
            params![(at - 1000) / 1000, body],
        )
        .unwrap();
        db.execute(
            "INSERT INTO logs VALUES(3000,?1,0,?2,'z-session','codex_http_client::client')",
            params![(at - 1000) / 1000, body.replace("session", "z-session")],
        )
        .unwrap();
        drop(db);
        let mut events = vec![
            usage_event(1, at, "turn", "model", ""),
            usage_event(2, at, "turn", "model", ""),
        ];
        events[1].session = "z-session".into();
        let endpoints = BTreeMap::from([(
            "https://relay.example/v1/responses".into(),
            Some("a".into()),
        )]);
        let result = Cache::default().attribute(
            temp.path(),
            &mut events,
            &endpoints,
            at - 10000,
            &BTreeMap::new(),
            &provider_cache::ProviderCache::default(),
        );
        assert_eq!(result["limited"], true);
        assert_eq!(result["recoveredEvents"], 1);
        assert_eq!(events[0].provider, "");
        assert_eq!(events[1].provider, "a");
    }

    #[test]
    fn captured_completion_survives_rotation_and_later_mapping_epoch() {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis();
        let mut db = provider_cache::ProviderCache::default();
        db.open(
            temp.path(),
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        )
        .unwrap();
        let request = bind(matching_request(at - 1000, Some("observed-a")), at - 2000);
        db.capture_native(
            "source",
            1,
            &[([8; 32], request)],
            true,
            at - WINDOW_MS - 1,
            "anchor",
        )
        .unwrap();
        let mut events = vec![usage_event(1, at, "turn", "model", "")];
        let before = events[0].delta;
        let result = Cache::default().attribute(
            temp.path(),
            &mut events,
            &BTreeMap::new(),
            at + 1000,
            &BTreeMap::new(),
            &db,
        );
        assert_eq!(result["capturedRequestsRead"], 1);
        assert_eq!(events[0].provider, "observed-a");
        assert_eq!(events[0].delta, before);
        assert_eq!(result["logRowsRead"], 0);
    }

    #[test]
    fn captured_conflicting_requests_at_same_millisecond_remain_ambiguous() {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis();
        let mut db = provider_cache::ProviderCache::default();
        db.open(
            temp.path(),
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        )
        .unwrap();
        let mut a = bind(matching_request(at - 1000, Some("a")), at - 2000);
        let mut b = bind(matching_request(at - 1000, Some("b")), at - 2000);
        a.row_id = 1;
        b.row_id = 2;
        db.capture_native(
            "source",
            2,
            &[([1; 32], a), ([2; 32], b)],
            true,
            at - WINDOW_MS - 1,
            "anchor",
        )
        .unwrap();
        let mut events = vec![usage_event(1, at, "turn", "model", "")];
        let result = Cache::default().attribute(
            temp.path(),
            &mut events,
            &BTreeMap::new(),
            at - 2000,
            &BTreeMap::new(),
            &db,
        );
        assert_eq!(result["capturedRequestsRead"], 2);
        assert_eq!(result["recoveredEvents"], 0);
        assert!(unknown_provider(&events[0].provider));
    }

    #[test]
    fn incomplete_capture_cannot_create_a_proof_but_explicit_conflicts_revoke_old_proofs() {
        let aliases = RequestSessionAliases::new(&BTreeMap::new());
        let mut events = vec![usage_event(1, 10000, "turn", "model", "")];
        let keys: Vec<_> = events
            .iter()
            .map(|event| event_key(event, &aliases))
            .collect();
        let mut cache = Cache::default();
        let incomplete = BTreeMap::from([("session".into(), (0, false))]);
        let a = matching_request(9000, Some("a"));
        assert_eq!(
            cache
                .apply(&mut events, &keys, &[a.clone()], 1, &incomplete)
                .0,
            0
        );
        assert_eq!(apply(&mut cache, &mut events, &[a.clone()]).0, 1);
        events[0].provider.clear();
        assert_eq!(cache.apply(&mut events, &keys, &[], 1, &incomplete).0, 1);
        events[0].provider.clear();
        assert_eq!(
            cache
                .apply(
                    &mut events,
                    &keys,
                    &[a, matching_request(9500, Some("b"))],
                    1,
                    &incomplete
                )
                .0,
            0
        );
    }

    #[test]
    fn old_session_windows_do_not_suppress_later_complete_windows() {
        let aliases = RequestSessionAliases::new(&BTreeMap::new());
        let mut events = vec![
            usage_event(1, 10000, "turn", "model", ""),
            usage_event(2, 1_000_000, "turn", "model", ""),
        ];
        let keys: Vec<_> = events
            .iter()
            .map(|event| event_key(event, &aliases))
            .collect();
        let coverage = BTreeMap::from([("session".into(), (20000, true))]);
        assert_eq!(
            Cache::default()
                .apply(
                    &mut events,
                    &keys,
                    &[
                        matching_request(9000, Some("a")),
                        matching_request(999000, Some("b"))
                    ],
                    1,
                    &coverage
                )
                .0,
            1
        );
        assert_eq!(events[0].provider, "");
        assert_eq!(events[1].provider, "b");
    }

    #[test]
    fn native_query_limit_preserves_newest_complete_groups() {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis();
        let mut db = provider_cache::ProviderCache::default();
        db.open(
            temp.path(),
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        )
        .unwrap();
        let rows: Vec<_> = (1..=2001i64)
            .map(|id| {
                let key: [u8; 32] = Sha256::digest(id.to_le_bytes()).into();
                (
                    key,
                    identify(
                        bind(matching_request(at - 10000 + id, Some("a")), at - 20000),
                        "source",
                        id,
                    ),
                )
            })
            .collect();
        db.capture_native("source", 2001, &rows, true, at - 20000, "anchor")
            .unwrap();
        let saved = db.captured_native("session", at - 20000, at).unwrap();
        assert!(saved.complete);
        assert_eq!(saved.rows.len(), 2000);
        assert_eq!(saved.floor, at - 9999);
        assert!(saved.rows.iter().all(|row| row.at > saved.floor));
    }

    #[test]
    fn retained_old_proof_rechecks_late_captured_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis() - WINDOW_MS - 10000;
        let mut db = provider_cache::ProviderCache::default();
        db.open(
            temp.path(),
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        )
        .unwrap();
        let a = identify(
            bind(matching_request(at - 1000, Some("a")), at - 2000),
            "source",
            1,
        );
        db.capture_native(
            "source",
            1,
            &[([1; 32], a)],
            true,
            at - WINDOW_MS - 1,
            "anchor",
        )
        .unwrap();
        let mut cache = Cache::default();
        let mut events = vec![usage_event(1, at, "turn", "model", "")];
        let aliases = BTreeMap::from([("session-alias".into(), "session".into())]);
        assert_eq!(
            cache.attribute(
                temp.path(),
                &mut events,
                &BTreeMap::new(),
                at - 2000,
                &aliases,
                &db
            )["recoveredEvents"],
            1
        );
        let mut b = identify(
            bind(matching_request(at - 1000, Some("b")), at - 2000),
            "source",
            2,
        );
        b.session = "session-alias".into();
        db.capture_native(
            "source",
            2,
            &[([2; 32], b)],
            true,
            at - WINDOW_MS - 1,
            "anchor",
        )
        .unwrap();
        events[0].provider.clear();
        assert_eq!(
            cache.attribute(
                temp.path(),
                &mut events,
                &BTreeMap::new(),
                at - 2000,
                &aliases,
                &db
            )["recoveredEvents"],
            0
        );
        assert!(cache.verified[&events[0].id].rejected);
    }

    #[test]
    fn unavailable_source_does_not_erase_successfully_read_counterevidence() {
        let temp = tempfile::tempdir().unwrap();
        let at = Local::now().timestamp_millis();
        let mut db = provider_cache::ProviderCache::default();
        db.open(
            temp.path(),
            Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
        )
        .unwrap();
        let a = identify(
            bind(matching_request(at - 1000, Some("a")), at - 2000),
            "captured-source",
            1,
        );
        db.capture_native(
            "captured-source",
            1,
            &[([1; 32], a)],
            true,
            at - WINDOW_MS - 1,
            "anchor",
        )
        .unwrap();
        let logs = Connection::open(temp.path().join("logs_1.sqlite")).unwrap();
        logs.execute_batch("CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,feedback_log_body TEXT,thread_id TEXT,target TEXT);").unwrap();
        let body = "turn{thread.id=session turn.id=turn model=model}: Request completed method=POST url=https://b.example/v1/responses status=200 headers={}";
        logs.execute(
            "INSERT INTO logs VALUES(1,?1,0,?2,'session','codex_http_client::client')",
            params![(at - 1000) / 1000, body],
        )
        .unwrap();
        drop(logs);
        drop(Connection::open(temp.path().join("logs_2.sqlite")).unwrap());
        let mut events = vec![usage_event(1, at, "turn", "model", "")];
        let endpoints =
            BTreeMap::from([("https://b.example/v1/responses".into(), Some("b".into()))]);
        let result = Cache::default().attribute(
            temp.path(),
            &mut events,
            &endpoints,
            at - 2000,
            &BTreeMap::new(),
            &db,
        );
        assert_eq!(result["unavailable"], true);
        assert_eq!(result["recoveredEvents"], 0);
    }
}
