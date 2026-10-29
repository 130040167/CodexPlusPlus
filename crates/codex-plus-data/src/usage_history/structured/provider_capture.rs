//! 供应商证据随管理器/worker 采集，不依赖网页、rollout 或用量面板。
use super::*;
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::mpsc;

const INTERVAL: Duration = Duration::from_secs(2);
const ID_WINDOW: i64 = 16_384;
const ROW_LIMIT: usize = 512;
const LOCK_DIAGNOSTIC_INTERVAL: Duration = Duration::from_secs(60);

pub struct ProviderCollector {
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ProviderCollector {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn start_provider_collector(
    home: PathBuf,
    sqlite_home: PathBuf,
    settings_path: PathBuf,
) -> io::Result<ProviderCollector> {
    start_provider_collector_with_log(
        home,
        sqlite_home,
        settings_path,
        codex_plus_core::request_log::request_log_path(),
    )
}

fn start_provider_collector_with_log(
    home: PathBuf,
    sqlite_home: PathBuf,
    settings_path: PathBuf,
    request_log: PathBuf,
) -> io::Result<ProviderCollector> {
    let (stop, receiver) = mpsc::channel();
    let thread = std::thread::Builder::new().name("supplier-evidence".into()).spawn(move || {
        let mut collector = Collector::default();
        let mut owner = None;
        let mut failed = false;
        let mut last_lock_diagnostic = None;
        loop {
            if owner.is_none() && home.is_dir() {
                match acquire(&home) {
                    Ok(lock) => owner = Some(lock),
                    Err(error) => {
                        if should_log_lock_error(&error, &mut last_lock_diagnostic, Instant::now()) {
                            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                                "usage.provider_capture.lock_failed",
                                json!({"kind":format!("{:?}", error.kind()),"message":error.to_string()}),
                            );
                        }
                    }
                }
            }
            let mut pending = false;
            if owner.is_some() {
                match collector.tick(&home, &sqlite_home, &settings_path, &request_log) {
                    Ok(backlog) => { pending = backlog; failed = false; },
                    Err(_) => {
                        // 短暂锁库或半写配置下次重试；不影响请求转发或页面。
                        collector.settings_stamp = None;
                        if !failed {
                            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                                "usage.provider_capture.retry",
                                json!({"reason":"local evidence temporarily unavailable"}),
                            );
                        }
                        failed = true;
                    }
                }
            }
            let delay = if pending { Duration::from_millis(250) } else { INTERVAL };
            if receiver.recv_timeout(delay) != Err(mpsc::RecvTimeoutError::Timeout) {
                // 只做最后一个有界批次，避免退出后还留有后台写入。
                if owner.is_some() { let _ = collector.tick(&home, &sqlite_home, &settings_path, &request_log); }
                return;
            }
        }
    })?;
    Ok(ProviderCollector {
        stop,
        thread: Some(thread),
    })
}

fn acquire(home: &Path) -> io::Result<File> {
    use fs2::FileExt;
    fs::create_dir_all(home.join("tmp"))?;
    let path = home.join("tmp/usage-provider-collector.lock");
    if fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
        return Err(io::Error::other(
            "supplier collector lock is not a regular file",
        ));
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock_exclusive().map_err(|error| {
        if error.raw_os_error().is_some()
            && error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
        {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "supplier collector is already owned",
            )
        } else {
            error
        }
    })?;
    Ok(file)
}

fn should_log_lock_error(error: &io::Error, last: &mut Option<Instant>, now: Instant) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return false;
    }
    if last.is_some_and(|last| now.duration_since(last) < LOCK_DIAGNOSTIC_INTERVAL) {
        return false;
    }
    *last = Some(now);
    true
}

#[derive(Default)]
struct Collector {
    cache: provider_cache::ProviderCache,
    settings_stamp: Option<(u64, Option<SystemTime>, Option<(u64, u64)>)>,
    endpoints: BTreeMap<String, Option<String>>,
    since: i64,
    requests: RequestEvidenceCache,
    sources: BTreeMap<PathBuf, Source>,
}

#[derive(Default)]
struct Source {
    identity: String,
    position: i64,
    anchor: String,
    floor: i64,
    stamp: Vec<Option<(u64, Option<SystemTime>, Option<(u64, u64)>)>>,
    pending: bool,
}

fn stamp(path: &Path) -> Option<(u64, Option<SystemTime>, Option<(u64, u64)>)> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.is_file())
        .map(|m| (m.len(), m.modified().ok(), file_identity(&m)))
}

fn cursor_anchor(db: &Connection, position: i64) -> anyhow::Result<String> {
    if position == 0 {
        return Ok("start".into());
    }
    let row: Option<(i64, i64, Option<String>)> = db
        .query_row(
            "SELECT ts,ts_nanos,target FROM logs WHERE id=?1",
            [position],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some(row) => serde_json::to_string(&row)?,
        None => String::new(),
    })
}

fn verify_source(path: &Path, opened: &File, identity: &str) -> anyhow::Result<()> {
    use codex_plus_core::codex_sqlite::opened_file_identity;
    anyhow::ensure!(
        opened_file_identity(opened)? == opened_file_identity(&File::open(path)?)?
            && native_endpoints::source_identity(path)? == identity,
        "native log source changed during capture",
    );
    Ok(())
}

fn timestamp_ms(seconds: i64, nanos: i64) -> Option<i64> {
    if !(0..1_000_000_000).contains(&nanos) {
        return None;
    }
    seconds.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

fn gap_floor(at: Option<i64>, observed_at: i64) -> i64 {
    at.filter(|at| *at > 0)
        .map(|at| at - 1)
        .unwrap_or(observed_at)
}

impl Collector {
    fn tick(
        &mut self,
        home: &Path,
        sqlite_home: &Path,
        settings: &Path,
        request_log: &Path,
    ) -> anyhow::Result<bool> {
        let cutoff = Local::now().date_naive() - Days::days(RETENTION_DAYS - 1);
        self.cache.open(home, cutoff)?;
        // 代理完成日志已实时落盘；在轮转前留存其最小匹配元数据。
        let before = self.requests.reads;
        self.requests.refresh(request_log);
        if self.requests.reads != before && self.requests.stamp.is_some() {
            if let Err(error) = self.cache.capture_proxy(&self.requests.records) {
                self.requests.stamp = None;
                return Err(error);
            }
        }
        let current = stamp(settings);
        if current != self.settings_stamp || self.since == 0 {
            // 配置读取失败不能继续沿用旧映射。
            self.endpoints.clear();
            self.settings_stamp = None;
            self.since = 0;
            if current.is_some_and(|(size, _, _)| size <= 8 * 1024 * 1024) {
                let profiles = native_endpoints::profiles(settings)?;
                if stamp(settings) != current {
                    anyhow::bail!("supplier settings changed during read");
                }
                self.endpoints = native_endpoints::endpoints(&profiles);
                self.since = self.cache.endpoint_binding_since(&self.endpoints)?;
                self.settings_stamp = current;
            }
        }
        if self.since == 0 {
            return Ok(false);
        }
        let paths = native_endpoints::log_paths(sqlite_home)?;
        let observed_at = Local::now().timestamp_millis();
        let disappeared: Vec<_> = self
            .sources
            .keys()
            .filter(|path| !paths.contains(path))
            .cloned()
            .collect();
        for path in disappeared {
            let source = &self.sources[&path];
            if !source.identity.is_empty() {
                self.cache.capture_native(
                    &source.identity,
                    source.position,
                    &[],
                    true,
                    if source.pending {
                        source.floor.max(observed_at)
                    } else {
                        source.floor
                    },
                    &source.anchor,
                )?;
            }
            self.sources.remove(&path);
        }
        let identities: anyhow::Result<HashSet<_>> = paths
            .iter()
            .map(|path| native_endpoints::source_identity(path))
            .collect();
        // 启动时也封存进程中断前遗留的孤儿 cursor；读取身份失败不能当作源已消失。
        let mut pending = if let Ok(identities) = identities {
            self.cache.retire_native_sources(&identities, observed_at)?
        } else {
            false
        };
        // 一个损坏的数据库不能阻止其他版本的日志继续采集。
        let mut failed = false;
        for path in paths {
            match self.read_source(&path) {
                Ok(backlog) => pending |= backlog,
                Err(_) => {
                    if let Some(source) = self.sources.get_mut(&path) {
                        source.stamp.clear();
                        source.pending = true;
                    }
                    failed = true;
                }
            }
        }
        if failed {
            anyhow::bail!("native supplier evidence temporarily unavailable");
        }
        Ok(pending)
    }

    fn read_source(&mut self, path: &Path) -> anyhow::Result<bool> {
        let observed_at = Local::now().timestamp_millis();
        let current: Vec<_> = codex_plus_core::codex_sqlite::codex_sqlite_sidecar_paths(path)
            .iter()
            .map(|p| stamp(p))
            .collect();
        if current[0].is_none() {
            return Ok(false);
        }
        // 保持文件句柄存活，避免路径替换后旧文件 ID 被立即复用。
        let opened = File::open(path)?;
        let source_id = native_endpoints::source_identity(path)?;
        let source = self.sources.entry(path.to_owned()).or_default();
        let db = native_endpoints::open_log(path)?;
        // high、旧锚点和本批数据必须来自同一个 SQLite 读快照。
        let snapshot = db.unchecked_transaction()?;
        let high: i64 =
            snapshot.query_row("SELECT coalesce(max(id),0) FROM logs", [], |row| row.get(0))?;
        verify_source(path, &opened, &source_id)?;
        let mut discontinuity = false;
        if source.identity != source_id {
            // 完整来源轮转不构成已知缺口；未读尾部丢失才抬高覆盖边界。
            discontinuity = !source.identity.is_empty() && source.pending;
            if !source.identity.is_empty() {
                self.cache.capture_native(
                    &source.identity,
                    source.position,
                    &[],
                    true,
                    if source.pending {
                        source.floor.max(observed_at)
                    } else {
                        source.floor
                    },
                    &source.anchor,
                )?;
            }
            source.identity = source_id;
            let (position, anchor, floor) = self
                .cache
                .capture_cursor(&source.identity)?
                .unwrap_or((0, "start".into(), 0));
            source.position = position;
            source.anchor = anchor;
            source.floor = floor;
            source.stamp.clear();
        }
        let anchor = cursor_anchor(&snapshot, source.position)?;
        if source.position < 0
            || high < source.position
            || source.floor < 0
            || anchor.is_empty()
            || anchor != source.anchor
        {
            // 原文件 DELETE/重建后 high 可能更大；冲突必须重读旧 cursor 前的全部 ID。
            source.position = 0;
            source.anchor = "start".into();
            source.floor = source.floor.max(0);
            source.stamp.clear();
            discontinuity = true;
        }
        if source.stamp == current && !source.pending && source.position == high {
            return Ok(false);
        }
        let mut position = source.position;
        let mut anchor = source.anchor.clone();
        // 连续的 1..high（含空前缀）没有未扫描 ID；不能把配置 epoch 当覆盖边界。
        // 查询侧会再叠加缓存保留期限，旧映射请求仍以 provider=None 参与歧义判断。
        let mut floor = source.floor;
        let mut requests = Vec::new();
        if high > position {
            let first: i64 = snapshot.query_row(
                "SELECT id FROM logs WHERE id>?1 ORDER BY id LIMIT 1",
                [position],
                |row| row.get(0),
            )?;
            // 数字 ID 大缺口直接跳到首个真实行；每轮仍至多覆盖 16384 个主键。
            let upper = high.min(first.saturating_add(ID_WINDOW - 1));
            let mut checkpoint = upper;
            let mut last_target = position;
            let mut statement = snapshot.prepare(
                "SELECT id,ts,ts_nanos,
                substr(feedback_log_body,1,16385),thread_id
                FROM logs WHERE id>?1 AND id<=?2 AND target='codex_http_client::client'
                ORDER BY id LIMIT ?3",
            )?;
            let rows =
                statement.query_map(params![position, upper, (ROW_LIMIT + 1) as i64], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?;
            for (offset, row) in rows.enumerate() {
                let (id, seconds, nanos, body, session) = row?;
                if offset == ROW_LIMIT {
                    checkpoint = last_target;
                    break;
                }
                last_target = id;
                let (Some(body), Some(session)) = (body, session) else {
                    continue;
                };
                let Some(at) = timestamp_ms(seconds, nanos) else {
                    continue;
                };
                let Some(row) = native_endpoints::request(&body, &session, at, &self.endpoints)
                else {
                    continue;
                };
                let key = Sha256::digest(serde_json::to_vec(&(&source.identity, id, at))?).into();
                requests.push((
                    key,
                    native_endpoints::identify(
                        native_endpoints::bind(row, self.since),
                        &source.identity,
                        id,
                    ),
                ));
            }
            drop(statement);
            // 只扫本预算内的元数据，检测内部缺口并把锚点落在真实的 checkpoint 行。
            let mut statement = snapshot.prepare(
                "SELECT id,ts,ts_nanos,target FROM logs WHERE id>?1 AND id<=?2 ORDER BY id",
            )?;
            let rows = statement.query_map(params![position, checkpoint], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            for row in rows {
                let (id, seconds, nanos, target) = row?;
                if discontinuity || id > position.saturating_add(1) {
                    floor = floor.max(gap_floor(timestamp_ms(seconds, nanos), observed_at));
                }
                discontinuity = false;
                position = id;
                anchor = serde_json::to_string(&(seconds, nanos, target))?;
            }
        }
        // 重建后仍为空时还没有后区首条日志，先用观测时间保守界定真实缺口。
        if discontinuity {
            floor = floor.max(observed_at);
        }
        verify_source(path, &opened, &source.identity)?;
        let pending = position < high;
        self.cache.capture_native(
            &source.identity,
            position,
            &requests,
            !pending,
            floor,
            &anchor,
        )?;
        source.position = position;
        source.anchor = anchor;
        source.floor = floor;
        source.stamp = current;
        source.pending = pending;
        Ok(source.pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    struct Fixture {
        _root: tempfile::TempDir,
        home: PathBuf,
        sqlite: PathBuf,
        settings: PathBuf,
        log: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("home");
            let sqlite = root.path().join("sqlite");
            fs::create_dir_all(&home).unwrap();
            fs::create_dir_all(&sqlite).unwrap();
            let settings = root.path().join("settings.json");
            let log = root.path().join("request-log.jsonl");
            let fixture = Self {
                _root: root,
                home,
                sqlite,
                settings,
                log,
            };
            fixture.profiles("a");
            fixture
                .db()
                .execute_batch(
                    "CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,
                feedback_log_body TEXT,thread_id TEXT,target TEXT);",
                )
                .unwrap();
            fixture
        }

        fn profiles(&self, provider: &str) {
            fs::write(
                &self.settings,
                serde_json::to_vec(&json!({"relayProfiles":[{
                    "id":provider,"name":provider,"baseUrl":"https://relay.example/v1"
                }]}))
                .unwrap(),
            )
            .unwrap();
        }

        fn db(&self) -> Connection {
            Connection::open(self.sqlite.join("logs_2.sqlite")).unwrap()
        }

        fn insert(&self, id: i64, at: i64, status: u16, host: &str) {
            let body = format!(
                "session_loop{{thread_id=session}}: turn{{thread.id=session turn.id=turn model=model}}: \
                Request completed method=POST url=https://{host}/v1/responses status={status} headers={{authorization=never-persist-this}}"
            );
            self.db()
                .execute(
                    "INSERT INTO logs VALUES(?1,?2,?3,?4,'session','codex_http_client::client')",
                    params![id, at / 1000, at % 1000 * 1_000_000, body],
                )
                .unwrap();
        }

        fn insert_many(&self, count: i64, at: i64, target: &str) {
            let body = "session_loop{thread_id=session}: turn{thread.id=session turn.id=turn model=model}: \
                Request completed method=POST url=https://relay.example/v1/responses status=200 headers={}";
            self.db()
                .execute(
                    "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<?1)
                 INSERT INTO logs SELECT n,?2,?3,
                     CASE WHEN ?4='codex_http_client::client' THEN ?5 END,'session',?4 FROM seq",
                    params![count, at / 1000, at % 1000 * 1_000_000, target, body],
                )
                .unwrap();
        }

        fn tick(&self, collector: &mut Collector) -> bool {
            collector
                .tick(&self.home, &self.sqlite, &self.settings, &self.log)
                .unwrap()
        }

        fn rows(&self, _collector: &Collector) -> Vec<native_endpoints::Request> {
            // 验证落盘证据本身；查询侧还会按 floor/complete 限制可用于归属的范围。
            let db = Connection::open(self.home.join("usage-provider-cache.sqlite")).unwrap();
            let mut statement = db.prepare(
                "SELECT data FROM completed_native_requests WHERE session='session' ORDER BY at",
            ).unwrap();
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .map(|row| serde_json::from_str(&row.unwrap()).unwrap())
                .collect()
        }

        fn cursor(&self, source: &str) -> (i64, String, i64, bool) {
            Connection::open(self.home.join("usage-provider-cache.sqlite")).unwrap().query_row(
                "SELECT position,anchor,floor,complete FROM native_capture_cursors WHERE source=?1",
                [source], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            ).unwrap()
        }

        fn first_attribution(&self, at: i64) -> (UsageEvent, Value) {
            let usage = Tokens {
                input_tokens: 100,
                cached_input_tokens: 50,
                output_tokens: 10,
                total_tokens: 110,
                ..Default::default()
            };
            let mut events = vec![UsageEvent {
                id: [1; 32],
                session: "session".into(),
                turn: "turn".into(),
                model: "model".into(),
                timestamp: DateTime::from_timestamp_millis(at).unwrap().to_rfc3339(),
                date: Local::now().date_naive(),
                total: usage,
                delta: usage,
                has_file_baseline: true,
                provider: String::new(),
                role: "main".into(),
                inherited: false,
                legacy_identity: None,
            }];
            // 冷读落盘完成证据，首次匹配前没有任何已核验归属可供恢复。
            let mut cache = provider_cache::ProviderCache::default();
            cache
                .open(
                    &self.home,
                    Local::now().date_naive() - Days::days(RETENTION_DAYS - 1),
                )
                .unwrap();
            assert!(
                cache
                    .load_native(Local::now().date_naive() - Days::days(RETENTION_DAYS - 1))
                    .unwrap()
                    .is_empty()
            );
            let result = native_endpoints::Cache::default().attribute(
                &self.sqlite,
                &mut events,
                &BTreeMap::new(),
                0,
                &BTreeMap::new(),
                &cache,
            );
            (events.remove(0), result)
        }
    }

    #[test]
    fn complete_capture_rotates_before_first_attribution_including_after_restart() {
        for restart in [false, true] {
            for replace in [false, true] {
                let fixture = Fixture::new();
                let path = fixture.sqlite.join("logs_2.sqlite");
                let mut collector = Collector::default();
                fixture.tick(&mut collector);
                let at = collector.since;
                fixture.insert(1, at, 200, "relay.example");
                assert!(!fixture.tick(&mut collector));
                let old = native_endpoints::source_identity(&path).unwrap();
                let before = fixture.cursor(&old);
                assert!(before.3);
                assert!(!fixture.home.join("sessions").exists());
                fs::rename(&path, fixture.sqlite.join("retired.sqlite")).unwrap();
                if replace {
                    fixture
                        .db()
                        .execute_batch(
                            "CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,
                            feedback_log_body TEXT,thread_id TEXT,target TEXT);",
                        )
                        .unwrap();
                }
                if restart {
                    drop(collector);
                    collector = Collector::default();
                }
                assert!(!fixture.tick(&mut collector));
                assert_eq!(fixture.cursor(&old).2, before.2);
                let (event, result) = fixture.first_attribution(at + 1);
                assert_eq!(event.provider, "a");
                assert_eq!(result["recoveredEvents"], 1);
                assert_eq!(result["capturedRequestsRead"], 1);
                assert_eq!(result["logRowsRead"], 0);
                assert_eq!(event.delta.total_tokens, 110);
            }
        }
    }

    #[test]
    fn complete_capture_rotation_preserves_existing_gap_rejection() {
        let fixture = Fixture::new();
        let path = fixture.sqlite.join("logs_2.sqlite");
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let at = collector.since;
        // 首条 ID 为 2，未观测到的前缀仍覆盖本次匹配窗口。
        fixture.insert(2, at, 200, "relay.example");
        assert!(!fixture.tick(&mut collector));
        let source = native_endpoints::source_identity(&path).unwrap();
        let before = fixture.cursor(&source);
        assert!(before.3);
        assert_eq!(before.2, at - 1);
        fs::rename(&path, fixture.sqlite.join("retired.sqlite")).unwrap();
        assert!(!fixture.tick(&mut collector));
        assert_eq!(fixture.cursor(&source).2, before.2);
        let (event, result) = fixture.first_attribution(at + 1);
        assert!(event.provider.is_empty());
        assert_eq!(result["capturedRequestsRead"], 1);
        assert_eq!(result["recoveredEvents"], 0);
    }

    #[test]
    fn in_place_rebuild_with_higher_high_rereads_the_prefix_including_after_restart() {
        for restart in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.sqlite.join("logs_2.sqlite");
            let mut collector = Collector::default();
            fixture.tick(&mut collector);
            let at = collector.since + 1000;
            fixture.insert(1, at, 200, "relay.example");
            fixture.insert(2, at + 1, 200, "relay.example");
            fixture.tick(&mut collector);
            let identity = native_endpoints::source_identity(&path).unwrap();
            let before = fixture.cursor(&identity);
            assert_eq!(before.0, 2);
            let opened_identity =
                codex_plus_core::codex_sqlite::opened_file_identity(&File::open(&path).unwrap())
                    .unwrap();
            fixture
                .db()
                .execute_batch(
                    "DROP TABLE logs;
                 CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,
                     feedback_log_body TEXT,thread_id TEXT,target TEXT);",
                )
                .unwrap();
            for id in 1..=3 {
                fixture.insert(id, at + 100 + id, 200, "relay.example");
            }
            assert_eq!(native_endpoints::source_identity(&path).unwrap(), identity);
            assert_eq!(
                codex_plus_core::codex_sqlite::opened_file_identity(&File::open(&path).unwrap(),)
                    .unwrap(),
                opened_identity
            );
            if restart {
                drop(collector);
                collector = Collector::default();
            }
            assert!(!fixture.tick(&mut collector));
            let rows = fixture.rows(&collector);
            for id in 1..=3 {
                let row = rows
                    .iter()
                    .find(|row| row.at == at + 100 + id)
                    .expect("rebuilt rows before the old cursor must be captured");
                let row = serde_json::to_value(row).unwrap();
                assert_eq!(row["source"], identity);
                assert_eq!(row["row_id"], id);
            }
            let after = fixture.cursor(&identity);
            assert_eq!(after.0, 3);
            assert_ne!(after.1, before.1);
            assert_eq!(after.2, before.2.max(at + 100));
            assert!(after.3);
        }
    }

    #[test]
    fn changed_source_is_rejected_and_replacement_does_not_reuse_the_old_cursor() {
        let fixture = Fixture::new();
        let path = fixture.sqlite.join("logs_2.sqlite");
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let at = collector.since + 1000;
        fixture.insert(1, at, 200, "relay.example");
        fixture.insert(2, at + 1, 200, "relay.example");
        fixture.tick(&mut collector);
        let opened = File::open(&path).unwrap();
        let previous = native_endpoints::source_identity(&path).unwrap();
        let replacement = fixture.sqlite.join("replacement.sqlite");
        fs::copy(&path, &replacement).unwrap();
        fs::rename(&path, fixture.sqlite.join("previous.sqlite")).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(verify_source(&path, &opened, &previous).is_err());
        let current = native_endpoints::source_identity(&path).unwrap();
        assert_ne!(current, previous);
        fixture
            .db()
            .execute("UPDATE logs SET ts_nanos=ts_nanos+1", [])
            .unwrap();
        assert!(!fixture.tick(&mut collector));
        assert_eq!(fixture.cursor(&current).0, 2);
        let rows = fixture.rows(&collector);
        assert!(
            rows.iter()
                .any(|row| serde_json::to_value(row).unwrap()["source"] == current)
        );
    }

    #[test]
    fn sparse_ids_raise_the_floor_and_anchor_actual_rows_without_empty_batches() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let path = fixture.sqlite.join("logs_2.sqlite");
        let source = native_endpoints::source_identity(&path).unwrap();
        let before = fixture.cursor(&source);
        fixture
            .db()
            .execute(
                "INSERT INTO logs VALUES(1,1,0,NULL,NULL,'unrelated_target')",
                [],
            )
            .unwrap();
        fixture.insert(1_000_000_000, collector.since + 1000, 200, "relay.example");
        assert!(fixture.tick(&mut collector));
        assert_eq!(fixture.cursor(&source).0, 1);
        assert!(!fixture.tick(&mut collector));
        let after = fixture.cursor(&source);
        assert_eq!(after.0, 1_000_000_000);
        assert_eq!(after.1, cursor_anchor(&fixture.db(), after.0).unwrap());
        assert_eq!(after.2, before.2.max(collector.since + 999));
        assert!(after.3);
        assert_eq!(fixture.rows(&collector).len(), 1);
    }

    #[test]
    fn anchor_is_checked_even_when_metadata_stamp_matches_and_target_changed() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let at = collector.since + 1000;
        fixture.insert(1, at, 200, "relay.example");
        fixture.insert(2, at + 1, 200, "relay.example");
        fixture.tick(&mut collector);
        fixture
            .db()
            .execute_batch(
                "UPDATE logs SET ts_nanos=ts_nanos+2000000 WHERE id=1;
             UPDATE logs SET target='replacement_target' WHERE id=2;",
            )
            .unwrap();
        let path = fixture.sqlite.join("logs_2.sqlite");
        // 模拟文件系统时间戳粒度过粗或被保留，不能用 stamp 跳过锚点校验。
        collector.sources.get_mut(&path).unwrap().stamp =
            codex_plus_core::codex_sqlite::codex_sqlite_sidecar_paths(&path)
                .iter()
                .map(|path| stamp(path))
                .collect();
        assert!(!fixture.tick(&mut collector));
        assert!(fixture.rows(&collector).iter().any(|row| row.at == at + 2));
        let source = native_endpoints::source_identity(&path).unwrap();
        assert!(fixture.cursor(&source).1.contains("replacement_target"));
    }

    #[test]
    fn complete_prefix_preserves_old_unbound_requests_and_has_no_startup_matching_blind_window() {
        let fixture = Fixture::new();
        let observed = Local::now().timestamp_millis();
        let old = observed - 1000;
        let recent = observed + 1000;
        fixture.insert(1, old, 200, "relay.example");
        fixture.insert(2, recent, 200, "relay.example");
        let mut collector = Collector::default();
        assert!(!fixture.tick(&mut collector));
        let source =
            native_endpoints::source_identity(&fixture.sqlite.join("logs_2.sqlite")).unwrap();
        assert_eq!(fixture.cursor(&source).2, 0);
        let window_floor = recent - 600_000;
        let captured = collector
            .cache
            .captured_native("session", window_floor, recent)
            .unwrap();
        assert!(captured.complete && captured.floor <= window_floor);
        assert_eq!(captured.rows.len(), 2);
        let old_row = captured.rows.iter().find(|row| row.at == old).unwrap();
        let recent_row = captured.rows.iter().find(|row| row.at == recent).unwrap();
        assert!(serde_json::to_value(old_row).unwrap()["provider"].is_null());
        assert_eq!(serde_json::to_value(recent_row).unwrap()["provider"], "a");
    }

    #[test]
    fn empty_initial_prefix_does_not_pin_the_floor_to_startup_time() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let recent = collector.since + 1;
        fixture.insert(1, recent, 200, "relay.example");
        assert!(!fixture.tick(&mut collector));
        let captured = collector
            .cache
            .captured_native("session", recent - 600_000, recent)
            .unwrap();
        assert!(captured.complete && captured.floor <= recent - 600_000);
        assert_eq!(captured.rows.len(), 1);
    }

    #[test]
    fn missing_prefix_uses_the_first_retained_time_instead_of_a_new_startup_blind_window() {
        let fixture = Fixture::new();
        let observed = Local::now().timestamp_millis();
        let first_retained = observed - 700_000;
        let recent = observed + 1000;
        fixture.insert(100, first_retained, 200, "relay.example");
        fixture.insert(101, recent, 200, "relay.example");
        let mut collector = Collector::default();
        assert!(!fixture.tick(&mut collector));
        let source =
            native_endpoints::source_identity(&fixture.sqlite.join("logs_2.sqlite")).unwrap();
        assert_eq!(fixture.cursor(&source).2, first_retained - 1);
        let captured = collector
            .cache
            .captured_native("session", recent - 600_000, recent)
            .unwrap();
        assert!(captured.complete && captured.floor <= recent - 600_000);
        assert_eq!(captured.rows.len(), 1);
        // 更早的未知映射请求也留在证据库，而非因配置 epoch 被丢弃。
        assert_eq!(fixture.rows(&collector).len(), 2);
    }

    #[test]
    fn vanished_or_replaced_pending_sources_are_sealed_including_after_restart() {
        for restart in [false, true] {
            for replace in [false, true] {
                let fixture = Fixture::new();
                let path = fixture.sqlite.join("logs_2.sqlite");
                let mut collector = Collector::default();
                fixture.tick(&mut collector);
                let at = collector.since + 1000;
                fixture.insert_many(600, at, "codex_http_client::client");
                assert!(fixture.tick(&mut collector));
                let old = native_endpoints::source_identity(&path).unwrap();
                assert!(!fixture.cursor(&old).3);
                assert_eq!(fixture.rows(&collector).len(), ROW_LIMIT);
                fs::rename(&path, fixture.sqlite.join("retired.sqlite")).unwrap();
                if replace {
                    fixture
                        .db()
                        .execute_batch(
                            "CREATE TABLE logs(id INTEGER PRIMARY KEY,ts INTEGER,ts_nanos INTEGER,
                            feedback_log_body TEXT,thread_id TEXT,target TEXT);",
                        )
                        .unwrap();
                }
                if restart {
                    drop(collector);
                    collector = Collector::default();
                }
                let observed = Local::now().timestamp_millis();
                assert!(!fixture.tick(&mut collector));
                let sealed = fixture.cursor(&old);
                assert!(sealed.3 && sealed.2 >= observed);
                assert_eq!(fixture.rows(&collector).len(), ROW_LIMIT);
                let captured = collector
                    .cache
                    .captured_native("session", at - 1, at)
                    .unwrap();
                assert!(
                    captured.complete,
                    "an orphan pending cursor must not starve new proofs"
                );
                let (event, result) = fixture.first_attribution(at + 1);
                assert!(event.provider.is_empty());
                assert_eq!(result["recoveredEvents"], 0);
                if replace {
                    // 真实缺口之后的完整窗口仍可用于新归属，不会永久被旧 complete=false 饿死。
                    let recent = sealed.2.max(at) + 600_001;
                    fixture.insert(1, recent, 200, "relay.example");
                    assert!(!fixture.tick(&mut collector));
                    let captured = collector
                        .cache
                        .captured_native("session", recent - 600_000, recent)
                        .unwrap();
                    assert!(captured.complete && captured.floor <= recent - 600_000);
                    assert_eq!(captured.rows.len(), 1);
                }
            }
        }
    }

    #[test]
    fn non_target_backlog_advances_by_the_full_id_budget() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        fixture.insert_many(ID_WINDOW * 4, collector.since + 1000, "unrelated_target");
        let source =
            native_endpoints::source_identity(&fixture.sqlite.join("logs_2.sqlite")).unwrap();
        let started = Instant::now();
        for batch in 1..=4 {
            assert_eq!(fixture.tick(&mut collector), batch < 4);
            let cursor = fixture.cursor(&source);
            assert_eq!(cursor.0, ID_WINDOW * batch);
            assert_eq!(cursor.3, batch == 4);
        }
        eprintln!(
            "synthetic 65536 non-target rows / 4 batches: {:?}",
            started.elapsed()
        );
        assert!(fixture.rows(&collector).is_empty());
    }

    #[test]
    fn orphan_retirement_is_bounded_and_does_not_seal_an_active_pending_source() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let active =
            native_endpoints::source_identity(&fixture.sqlite.join("logs_2.sqlite")).unwrap();
        for index in 0..65 {
            collector
                .cache
                .capture_native(&format!("orphan-{index:02}"), 0, &[], false, 0, "start")
                .unwrap();
        }
        collector
            .cache
            .capture_native(&active, 0, &[], false, 0, "start")
            .unwrap();
        let identities = HashSet::from([active.clone()]);
        let observed = Local::now().timestamp_millis();
        assert!(
            collector
                .cache
                .retire_native_sources(&identities, observed)
                .unwrap()
        );
        let db = Connection::open(fixture.home.join("usage-provider-cache.sqlite")).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM native_capture_cursors WHERE complete=0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        assert!(!fixture.cursor(&active).3);
        assert!(
            !collector
                .cache
                .retire_native_sources(&identities, observed)
                .unwrap()
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM native_capture_cursors WHERE complete=1 AND floor=?1",
                [observed],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            65
        );
        assert!(!fixture.cursor(&active).3);
    }

    #[test]
    fn captures_before_any_rollout_or_history_query_and_survives_rotation_restart() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let at = Local::now().timestamp_millis().max(collector.since);
        fixture.insert(1, at, 200, "relay.example");
        fixture.tick(&mut collector);
        let rows = fixture.rows(&collector);
        assert_eq!(rows.len(), 1);
        assert!(!fixture.home.join("sessions").exists());
        let json = serde_json::to_string(&rows).unwrap();
        assert!(!json.contains("never-persist-this"));
        assert!(!json.contains("https://"));
        assert!(!json.contains("tokens"));
        fixture.db().execute("DELETE FROM logs", []).unwrap();
        drop(collector);
        let mut restarted = Collector::default();
        fixture.tick(&mut restarted);
        assert_eq!(fixture.rows(&restarted), rows);
        fixture.insert(1, at + 100, 200, "relay.example");
        fixture.tick(&mut restarted);
        assert_eq!(fixture.rows(&restarted).len(), 2);
        let cache = Connection::open(fixture.home.join("usage-provider-cache.sqlite")).unwrap();
        assert_eq!(
            cache
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name='token_events'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn mapping_changes_preserve_observed_requests_and_do_not_relabel_old_traffic() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let first = collector.since;
        fixture.insert(1, first, 200, "relay.example");
        fixture.tick(&mut collector);
        fixture.profiles("b");
        fixture.tick(&mut collector);
        let second = collector.since;
        fixture.insert(2, second.max(first + 1), 200, "relay.example");
        fixture.insert(3, first - 1000, 200, "relay.example");
        fixture.insert(4, second + 2, 502, "relay.example");
        fixture.insert(5, second + 3, 200, "unmapped.example");
        fixture.tick(&mut collector);
        let rows = fixture.rows(&collector);
        assert_eq!(rows.len(), 4);
        let providers: Vec<_> = rows
            .iter()
            .map(|r| serde_json::to_value(r).unwrap()["provider"].clone())
            .collect();
        assert_eq!(
            providers,
            vec![Value::Null, json!("a"), json!("b"), Value::Null]
        );
    }

    #[test]
    fn bounded_batches_resume_exactly_without_duplicate_storage_or_idle_writes() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        let source =
            native_endpoints::source_identity(&fixture.sqlite.join("logs_2.sqlite")).unwrap();
        let initial = fixture.cursor(&source);
        assert_eq!(initial.2, 0);
        assert!(initial.3);
        let at = collector.since;
        let db = fixture.db();
        let body = "session_loop{thread_id=session}: turn{thread.id=session turn.id=turn model=model}: \
            Request completed method=POST url=https://relay.example/v1/responses status=200 headers={}";
        db.execute(
            "WITH RECURSIVE seq(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM seq WHERE n<600)
            INSERT INTO logs SELECT n,?1,?2,?3,'session','codex_http_client::client' FROM seq",
            params![at / 1000, at % 1000 * 1_000_000, body],
        )
        .unwrap();
        assert!(fixture.tick(&mut collector));
        assert_eq!(fixture.rows(&collector).len(), ROW_LIMIT);
        let partial = fixture.cursor(&source);
        assert_eq!(partial.0, ROW_LIMIT as i64);
        assert!(!partial.3);
        assert!(!partial.1.is_empty());
        assert!(!fixture.tick(&mut collector));
        assert_eq!(fixture.rows(&collector).len(), 600);
        let complete = fixture.cursor(&source);
        assert_eq!(complete.0, 600);
        assert!(complete.3);
        assert_eq!(complete.2, initial.2);
        let db = Connection::open(fixture.home.join("usage-provider-cache.sqlite")).unwrap();
        let before: i64 = db
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .unwrap();
        fixture.tick(&mut collector);
        let after: i64 = db
            .query_row("PRAGMA data_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            before, after,
            "unchanged native/proxy sources must not write during idle polling"
        );
    }

    #[test]
    fn collector_lock_is_exclusive_and_drop_releases_it_promptly() {
        let fixture = Fixture::new();
        let owner = acquire(&fixture.home).unwrap();
        assert_eq!(
            acquire(&fixture.home).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(owner);
        let guard = start_provider_collector_with_log(
            fixture.home.clone(),
            fixture.sqlite.clone(),
            fixture.settings.clone(),
            fixture.log.clone(),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fixture.home.join("usage-provider-cache.sqlite").exists() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
        let started = Instant::now();
        drop(guard);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(acquire(&fixture.home).is_ok());
    }

    #[test]
    fn lock_contention_is_quiet_but_invalid_paths_and_permission_errors_are_rate_limited() {
        let fixture = Fixture::new();
        fs::create_dir_all(fixture.home.join("tmp/usage-provider-collector.lock")).unwrap();
        let invalid = acquire(&fixture.home).unwrap_err();
        assert_ne!(invalid.kind(), io::ErrorKind::WouldBlock);
        let now = Instant::now();
        let mut last = None;
        assert!(!should_log_lock_error(
            &io::Error::from(io::ErrorKind::WouldBlock),
            &mut last,
            now,
        ));
        assert!(last.is_none());
        assert!(should_log_lock_error(&invalid, &mut last, now));
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(!should_log_lock_error(
            &denied,
            &mut last,
            now + Duration::from_secs(59)
        ));
        assert!(should_log_lock_error(
            &denied,
            &mut last,
            now + LOCK_DIAGNOSTIC_INTERVAL
        ));
    }

    #[test]
    fn native_log_is_read_only_and_bad_settings_never_reuse_old_mapping() {
        let fixture = Fixture::new();
        let mut collector = Collector::default();
        fixture.tick(&mut collector);
        fixture.insert(1, collector.since, 200, "relay.example");
        let before = fs::read(fixture.sqlite.join("logs_2.sqlite")).unwrap();
        fixture.tick(&mut collector);
        assert_eq!(
            fs::read(fixture.sqlite.join("logs_2.sqlite")).unwrap(),
            before
        );
        fs::write(&fixture.settings, b"{").unwrap();
        fixture.insert(2, collector.since + 10, 200, "relay.example");
        assert!(
            collector
                .tick(
                    &fixture.home,
                    &fixture.sqlite,
                    &fixture.settings,
                    &fixture.log
                )
                .is_err()
        );
        assert!(collector.endpoints.is_empty());
        fixture.profiles("b");
        fixture.tick(&mut collector);
        assert_eq!(fixture.rows(&collector).len(), 2);
    }
}
