//! 删除标记与历史文件分开管理：只有不可从保留会话到达的已删除历史才能回收。
use super::history_graph::HistoryGraph;
use super::*;
use anyhow::{Context, bail};
use fs2::FileExt;
use std::collections::{BTreeMap, BTreeSet};

const TABLE: &str = "codex_plus_deleted_threads";

pub(super) struct HistoryRetention {
    _lock: File,
    home: PathBuf,
    databases: Vec<PathBuf>,
}

impl HistoryRetention {
    pub fn contains_history(&self, id: &str) -> anyhow::Result<bool> {
        Ok(self.graph()?.files.contains_key(id))
    }

    pub fn acquire(home: &Path, paths: &[PathBuf]) -> anyhow::Result<Self> {
        fs::create_dir_all(home.join("tmp"))?;
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(home.join("tmp/history-retention.lock"))?;
        lock.try_lock_exclusive()
            .context("会话删除或恢复正在进行，请稍后重试")?;
        let mut databases = paths.to_vec();
        for path in codex_plus_core::codex_sqlite::codex_session_db_paths_from_home(home) {
            if !databases.contains(&path) {
                databases.push(path);
            }
        }
        Ok(Self {
            _lock: lock,
            home: home.to_path_buf(),
            databases,
        })
    }

    fn deleted_ids(&self) -> anyhow::Result<BTreeSet<String>> {
        let mut ids = BTreeSet::new();
        for path in &self.databases {
            if !path.is_file() {
                continue;
            }
            let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            if has_table(&db, TABLE)? {
                let mut stmt = db.prepare(&format!("SELECT thread_id FROM {TABLE}"))?;
                for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
                    ids.insert(id?);
                }
            }
        }
        Ok(ids)
    }

    fn graph(&self) -> anyhow::Result<HistoryGraph> {
        let mut paths = Vec::new();
        for path in &self.databases {
            if !path.is_file() {
                continue;
            }
            let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            if has_columns(&db, "threads", &["id", "rollout_path"])? {
                let mut stmt = db.prepare("SELECT id, rollout_path FROM threads WHERE rollout_path IS NOT NULL AND rollout_path != ''")?;
                for row in stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })? {
                    let (id, path) = row?;
                    let original = PathBuf::from(&path);
                    if original.is_file() {
                        paths.push((id, original));
                    } else if let Some(alt) = wsl_path_alternative(&path).map(PathBuf::from) {
                        if alt.is_file() {
                            paths.push((id, alt));
                        }
                    }
                }
            }
        }
        HistoryGraph::scan(&self.home, &paths)
    }

    /// 先备份依赖闭包再提交删除意图，备份完成前不改变任何用户数据。
    pub fn prepare_delete(&self, id: &str, tables: &mut Map<String, Value>) -> anyhow::Result<()> {
        let graph = self.graph()?;
        let mut files = Vec::new();
        if graph.files.contains_key(id) {
            for path in graph.closure_files(id)? {
                if !graph.files[id].contains(&path) {
                    // 先确认祖先位于可恢复的原生目录，不能产生无法兑现的撤销备份。
                    self.validate_history_path(&path)?;
                }
                let bytes = fs::read(&path)?;
                files.push(rollout_file_backup_entry(
                    &path.to_string_lossy(),
                    &path.to_string_lossy(),
                    bytes,
                ));
            }
        }
        // 即使没有祖先也写此字段，区分支持依赖恢复的新备份与旧撤销契约。
        tables.insert("__history_files".into(), Value::Array(files));
        Ok(())
    }

    pub fn mark_deleted(db: &Connection, id: &str, token: &str) -> anyhow::Result<()> {
        db.execute_batch(&format!("CREATE TABLE IF NOT EXISTS {TABLE} (thread_id TEXT PRIMARY KEY, backup_token TEXT NOT NULL)"))?;
        db.execute(&format!("INSERT INTO {TABLE} (thread_id, backup_token) VALUES (?1,?2) ON CONFLICT(thread_id) DO UPDATE SET backup_token=excluded.backup_token"), [id, token])?;
        Ok(())
    }

    pub fn is_deleted(db: &Connection, id: &str) -> anyhow::Result<bool> {
        if !has_table(db, TABLE)? {
            return Ok(false);
        }
        Ok(db
            .query_row(
                &format!("SELECT 1 FROM {TABLE} WHERE thread_id=?1"),
                [id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn collect(&self) -> anyhow::Result<usize> {
        let deleted = self.deleted_ids()?;
        if deleted.is_empty() {
            return Ok(0);
        }
        if deleted.iter().any(|id| uuid::Uuid::parse_str(id).is_ok()) && native_writers_running()? {
            return Ok(0);
        }
        let graph = self.graph()?;
        // 原生 fork 不遵守本模块的锁。在线只软删除，避免检查后新建 fork 的竞态。
        if has_native_history(&graph) && native_writers_running()? {
            return Ok(0);
        }
        self.collect_quiescent(graph, &deleted)
    }

    fn collect_quiescent(
        &self,
        graph: HistoryGraph,
        deleted: &BTreeSet<String>,
    ) -> anyhow::Result<usize> {
        let reachable = graph.reachable_from(
            graph
                .files
                .keys()
                .filter(|id| !deleted.contains(*id))
                .cloned(),
        );
        // 任一保留会话的闭包不完整时，先停止回收，避免把损坏的索引当作无引用。
        for id in graph.files.keys().filter(|id| !deleted.contains(*id)) {
            graph.closure_files(id)?;
        }
        let mut removed = 0;
        for id in deleted.iter().filter(|id| !reachable.contains(*id)) {
            for path in graph.files.get(id).into_iter().flatten() {
                // 每个文件删除前再确认原生写入方未启动。
                if has_native_history(&graph) && native_writers_running()? {
                    return Ok(removed);
                }
                match fs::remove_file(path) {
                    Ok(()) => removed += 1,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).context("回收已删除的历史文件失败，稍后自动重试");
                    }
                }
            }
            for path in &self.databases {
                if !path.is_file() {
                    continue;
                }
                let mut db = Connection::open(path)?;
                if !has_table(&db, "threads")? || !Self::is_deleted(&db, id)? {
                    continue;
                }
                let tx = db.transaction()?;
                for table in ["thread_dynamic_tools", "thread_goals", "stage1_outputs"] {
                    delete_related_rows(&tx, table, "thread_id = ?1", &[id])?;
                }
                delete_related_rows(
                    &tx,
                    "thread_spawn_edges",
                    "parent_thread_id = ?1 OR child_thread_id = ?1",
                    &[id],
                )?;
                if has_columns(&tx, "agent_job_items", &["assigned_thread_id"])? {
                    tx.execute("UPDATE agent_job_items SET assigned_thread_id=NULL WHERE assigned_thread_id=?1", [id])?;
                }
                tx.execute("DELETE FROM threads WHERE id=?1", [id])?;
                tx.commit()?;
                // 标记继续保留：撤销叶子时祖先文件恢复后仍是软删除，不能变成列表中的活会话。
            }
        }
        Ok(removed)
    }

    /// 新备份允许保留中的同一文件存在，但绝不覆盖追加后的历史。
    pub fn prepare_undo(&self, backups: &mut [Value]) -> anyhow::Result<()> {
        for backup in backups {
            let db_path = backup_source_db(backup, &self.databases[0], &self.databases)?;
            let db = Connection::open(&db_path)?;
            let id = backup["session_id"].as_str().unwrap_or("").to_string();
            let token = backup["token"].as_str().unwrap_or("").to_string();
            let Some(tables) = backup["tables"].as_object_mut() else {
                continue;
            };
            validate_restore_tables(tables)?;
            if !tables.contains_key("__history_files") {
                continue;
            }
            let target_paths = tables
                .get("threads")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|row| row["rollout_path"].as_str())
                .map(|path| PathBuf::from(path.trim_start_matches("\\\\?\\")))
                .collect::<BTreeSet<_>>();
            let current_token = if has_table(&db, TABLE)? {
                db.query_row(
                    &format!("SELECT backup_token FROM {TABLE} WHERE thread_id=?1"),
                    [&id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
            } else {
                None
            };
            if current_token.as_deref() != Some(&token) {
                bail!("restore conflict: deletion state changed for {id}");
            }
            let mut archived = Vec::new();
            for (table, rows) in tables
                .iter_mut()
                .filter(|(name, _)| !name.starts_with("__"))
            {
                let Some(rows) = rows.as_array_mut() else {
                    continue;
                };
                let mut retained = Vec::new();
                for row in std::mem::take(rows) {
                    let Some(object) = row.as_object() else {
                        continue;
                    };
                    if restore_row_conflicts(&db, table, object)? {
                        if table == "threads" && row["id"].as_str() == Some(&id) {
                            let current =
                                select_dicts(&db, "SELECT * FROM threads WHERE id=?1", &[&id])?;
                            if current[0].get("archived").and_then(Value::as_i64) != Some(1) {
                                bail!("restore conflict: thread is no longer soft deleted");
                            }
                            archived.push(row.clone());
                        } else {
                            // 软删除不修改关联行；相同的行无需重复插入。
                            let cols = restore_conflict_key_columns(table, object);
                            let clauses = cols
                                .iter()
                                .enumerate()
                                .map(|(i, k)| format!("\"{k}\"=?{}", i + 1))
                                .collect::<Vec<_>>()
                                .join(" AND ");
                            let vals = cols
                                .iter()
                                .map(|k| OwnedSqlValue(json_to_sql_value(&object[*k])))
                                .collect::<Vec<_>>();
                            let refs = vals.iter().map(|v| v as &dyn ToSql).collect::<Vec<_>>();
                            let current = select_dicts(
                                &db,
                                &format!("SELECT * FROM \"{table}\" WHERE {clauses}"),
                                &refs,
                            )?;
                            if !current.contains(&row) {
                                bail!("restore conflict: {table} row changed");
                            }
                        }
                    } else {
                        retained.push(row);
                    }
                }
                *rows = retained;
            }
            tables.insert("__restore_archived".into(), Value::Array(archived));
            let mut files = BTreeMap::new();
            for key in ["__files", "__history_files"] {
                for file in tables
                    .get(key)
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let path = file["path"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("invalid history backup path"))?;
                    if key == "__history_files"
                        && !target_paths
                            .contains(&PathBuf::from(path.trim_start_matches("\\\\?\\")))
                    {
                        self.validate_history_path(Path::new(path))?;
                    }
                    files.insert(path.to_string(), file.clone());
                }
            }
            let mut missing = Vec::new();
            for (path, file) in files {
                let bytes = base64::Engine::decode(
                    &base64::engine::general_purpose::STANDARD,
                    file["content_b64"].as_str().unwrap_or(""),
                )?;
                match fs::read(&path) {
                    Ok(current) if current.starts_with(&bytes) => {}
                    Ok(_) => bail!("restore conflict: history file changed: {path}"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        missing.push(file)
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            tables.insert("__files".into(), Value::Array(missing));
        }
        Ok(())
    }

    fn validate_history_path(&self, path: &Path) -> anyhow::Result<()> {
        let home = fs::canonicalize(&self.home)?;
        // canonicalize 在 Windows 上增加合法的扩展路径前缀。
        let normalized = PathBuf::from(path.to_string_lossy().trim_start_matches("\\\\?\\"));
        let path = normalized.as_path();
        // 路径必须来自本 home 的两个原生历史目录；拒绝 .. 与逃逸链接。
        for root in ["sessions", "archived_sessions"] {
            let root = self.home.join(root);
            if path.starts_with(&root)
                && !path
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
            {
                let mut parent = path.parent();
                while let Some(candidate) = parent {
                    if candidate.exists() {
                        if fs::canonicalize(candidate)?.starts_with(&home) {
                            return Ok(());
                        }
                        break;
                    }
                    parent = candidate.parent();
                }
            }
        }
        bail!("unexpected history dependency path: {}", path.display())
    }

    pub fn finish_undo(&self, backups: &[Value]) -> anyhow::Result<()> {
        for backup in backups {
            if backup["tables"].get("__history_files").is_none() {
                continue;
            }
            let db_path = backup_source_db(backup, &self.databases[0], &self.databases)?;
            let db = Connection::open(db_path)?;
            db.execute(
                &format!("DELETE FROM {TABLE} WHERE thread_id=?1 AND backup_token=?2"),
                [
                    backup["session_id"].as_str().unwrap_or(""),
                    backup["token"].as_str().unwrap_or(""),
                ],
            )?;
            crate::provider_sync::clear_thread_tombstones(
                &self.home,
                &HashSet::from([backup["session_id"].as_str().unwrap_or("").to_string()]),
            );
        }
        Ok(())
    }
}

fn has_native_history(graph: &HistoryGraph) -> bool {
    graph
        .files
        .keys()
        .any(|id| uuid::Uuid::parse_str(id).is_ok())
}

fn native_writers_running() -> anyhow::Result<bool> {
    #[cfg(windows)]
    {
        let processes = codex_plus_core::windows_enumerate_processes();
        if processes.is_empty() {
            bail!("无法读取进程列表，暂缓历史回收");
        }
        Ok(processes.iter().any(|process| {
            process.exe_file.eq_ignore_ascii_case("codex.exe")
                || process.exe_file.eq_ignore_ascii_case("chatgpt.exe")
        }))
    }
    #[cfg(not(windows))]
    {
        let output = std::process::Command::new("ps")
            .args(["-axo", "comm="])
            .output()?;
        if !output.status.success() {
            bail!("无法确认 Codex 已停止，暂缓历史回收");
        }
        Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            let name = Path::new(line.trim())
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("");
            ["codex", "chatgpt", "codex dev"]
                .iter()
                .any(|wanted| name.eq_ignore_ascii_case(wanted))
        }))
    }
}

pub(crate) fn collect_deleted_history(home: &Path) -> anyhow::Result<usize> {
    let paths = codex_plus_core::codex_sqlite::codex_session_db_paths_from_home(home);
    HistoryRetention::acquire(home, &paths)?.collect()
}
