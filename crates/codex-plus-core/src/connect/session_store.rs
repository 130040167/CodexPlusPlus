use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

const STATE_FILE: &str = "weixin-connect-state.json";
const MAX_PROCESSED_IDS: usize = 512;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    #[serde(default)]
    pub get_updates_buf: String,
    #[serde(default)]
    pub thread_ids: BTreeMap<String, String>,
    /// 历史附件会话映射；保留供无原始会话映射的联系人续接。
    #[serde(default)]
    pub media_thread_ids: BTreeMap<String, String>,
    /// 注册了附件和会话设置工具的新版线程；旧映射保留，便于回看。
    #[serde(default)]
    pub controlled_thread_ids: BTreeMap<String, String>,
    #[serde(default)]
    pub context_tokens: BTreeMap<String, String>,
    #[serde(default)]
    processed_message_keys: VecDeque<String>,
}

impl ConnectState {
    pub fn is_processed(&self, message_key: &str) -> bool {
        !message_key.is_empty()
            && self
                .processed_message_keys
                .iter()
                .any(|key| key == message_key)
    }

    pub fn mark_processed(&mut self, message_key: impl Into<String>) {
        let message_key = message_key.into();
        if message_key.is_empty() || self.is_processed(&message_key) {
            return;
        }
        self.processed_message_keys.push_back(message_key);
        while self.processed_message_keys.len() > MAX_PROCESSED_IDS {
            self.processed_message_keys.pop_front();
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConnectSessionStore {
    path: PathBuf,
    owner: Option<String>,
    legacy_path: Option<PathBuf>,
}

impl ConnectSessionStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            owner: None,
            legacy_path: None,
        }
    }

    pub fn default_for_account(account_id: &str) -> Self {
        Self::in_directory(&crate::paths::default_app_state_dir(), account_id)
    }

    fn in_directory(directory: &Path, account_id: &str) -> Self {
        let encoded: String = account_id
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let legacy_name = if account_id.is_empty() {
            Some(STATE_FILE.to_string())
        } else if account_id != "default"
            && account_id.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
            })
        {
            Some(format!("weixin-connect-state-{account_id}.json"))
        } else {
            None
        };
        Self {
            path: directory
                .join("weixin-connect-state")
                .join(format!("account-{encoded}.json")),
            owner: Some(account_id.to_string()),
            legacy_path: legacy_name.map(|name| directory.join(name)),
        }
    }

    pub fn load(&self) -> anyhow::Result<ConnectState> {
        if let Some(state) = self.read_state(&self.path, false)? {
            return Ok(state);
        }
        if let Some(owner) = &self.owner {
            // 仅用于发现旧文件，不能从旧 sanitize 的碰撞结果猜测账号归属。
            let sanitized: String = owner
                .trim()
                .chars()
                .map(|character| match character {
                    '/' | '\\' | ':' | '\0' => '_',
                    other => other,
                })
                .collect();
            let legacy_name = if sanitized.is_empty() || sanitized == "default" {
                STATE_FILE.to_string()
            } else {
                format!("weixin-connect-state-{sanitized}.json")
            };
            let directory = self
                .path
                .parent()
                .and_then(Path::parent)
                .context("账号状态路径无旧状态目录")?;
            let mut ambiguous = false;
            match std::fs::read_dir(directory) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry?;
                        let name = entry.file_name();
                        let Some(name) = name.to_str() else {
                            continue;
                        };
                        if name.to_lowercase() != legacy_name.to_lowercase() {
                            continue;
                        }
                        // 安全账号仍须精确匹配实际文件名，不能让 Windows 折叠大小写。
                        if self.legacy_path.as_ref().is_some_and(|legacy| {
                            legacy.file_name() == Some(entry.file_name().as_os_str())
                        }) {
                            if let Some(state) = self.read_state(&entry.path(), true)? {
                                return Ok(state);
                            }
                        } else {
                            ambiguous = true;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("无法读取旧微信连接状态目录"),
            }
            if ambiguous {
                bail!(
                    "旧微信连接状态迁移归属不明确，原会话文件已保留；请确认账号归属后迁移，拒绝创建新会话"
                );
            }
        }
        self.check_owner(ConnectState::default(), true)
    }

    fn read_state(&self, path: &Path, allow_unowned: bool) -> anyhow::Result<Option<ConnectState>> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let state = serde_json::from_slice(&bytes)
                    .with_context(|| format!("无法解析微信连接状态 {}", path.display()))?;
                self.check_owner(state, allow_unowned).map(Some)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("无法读取微信连接状态 {}", path.display()))
            }
        }
    }

    fn check_owner(
        &self,
        mut state: ConnectState,
        allow_unowned: bool,
    ) -> anyhow::Result<ConnectState> {
        if let Some(owner) = &self.owner {
            match &state.account_id {
                Some(account) if account != owner => bail!("微信连接状态账号归属不匹配，拒绝读写"),
                None if !allow_unowned => bail!("微信连接状态缺少账号归属，拒绝认领"),
                _ => {}
            }
            state.account_id = Some(owner.clone());
        }
        Ok(state)
    }

    pub fn save(&self, state: &ConnectState) -> anyhow::Result<()> {
        let state = self.check_owner(state.clone(), true)?;
        // 即使调用方没有先 load，也不能覆盖归属错误或缺少归属的新格式文件。
        if self.owner.is_some() {
            self.read_state(&self.path, false)?;
        }
        let bytes = serde_json::to_vec_pretty(&state)?;
        crate::settings::atomic_write(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn populated_state() -> ConnectState {
        let mut state = ConnectState {
            get_updates_buf: "cursor".into(),
            ..ConnectState::default()
        };
        state.thread_ids.insert("peer".into(), "original".into());
        state.media_thread_ids.insert("peer".into(), "media".into());
        state
            .controlled_thread_ids
            .insert("peer".into(), "controlled".into());
        state
            .context_tokens
            .insert("peer".into(), "fixture-context".into());
        state.mark_processed("fixture-message");
        state
    }

    #[test]
    fn account_namespaces_do_not_collide_and_preserve_complete_state() {
        let temp = tempfile::tempdir().unwrap();
        let accounts = [
            "A/B", "A_B", "a_b", "ABC", "abc", "", "default", "账号", "abc ",
        ];
        let mut paths = std::collections::BTreeSet::new();
        for (index, account) in accounts.iter().enumerate() {
            let store = ConnectSessionStore::in_directory(temp.path(), account);
            assert!(paths.insert(store.path.to_string_lossy().to_ascii_lowercase()));
            assert_eq!(
                store.path.parent().unwrap(),
                temp.path().join("weixin-connect-state")
            );
            let mut state = populated_state();
            state.get_updates_buf = index.to_string();
            store.save(&state).unwrap();
        }
        for (index, account) in accounts.iter().enumerate() {
            let store = ConnectSessionStore::in_directory(temp.path(), account);
            let mut expected = populated_state();
            expected.account_id = Some(account.to_string());
            expected.get_updates_buf = index.to_string();
            assert_eq!(store.load().unwrap(), expected);
            let json: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&store.path).unwrap()).unwrap();
            assert_eq!(json["accountId"], *account);
        }
        assert_eq!(
            ConnectSessionStore::in_directory(temp.path(), "A/B")
                .path
                .file_name()
                .unwrap(),
            "account-412f42.json"
        );
        assert_eq!(
            ConnectSessionStore::in_directory(temp.path(), "账号")
                .path
                .file_name()
                .unwrap(),
            "account-e8b4a6e58fb7.json"
        );
    }

    #[test]
    fn eligible_legacy_state_is_adopted_without_rewriting_legacy() {
        for (account, name) in [
            ("", STATE_FILE),
            ("lower_123-safe", "weixin-connect-state-lower_123-safe.json"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let legacy = temp.path().join(name);
            let bytes = serde_json::to_vec(&populated_state()).unwrap();
            std::fs::write(&legacy, &bytes).unwrap();
            let store = ConnectSessionStore::in_directory(temp.path(), account);
            let loaded = store.load().unwrap();
            let mut expected = populated_state();
            expected.account_id = Some(account.to_string());
            assert_eq!(loaded, expected);
            assert!(!store.path.exists(), "load 不能写入或迁移文件");
            store.save(&loaded).unwrap();
            assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
            assert_eq!(store.load().unwrap(), expected);
            std::fs::write(&legacy, b"invalid legacy after migration").unwrap();
            assert_eq!(store.load().unwrap(), expected, "新状态优先，不再读 legacy");
        }
    }

    #[test]
    fn ambiguous_and_case_mismatched_legacy_files_block_new_sessions() {
        for (account, legacy_name) in [
            ("default", STATE_FILE),
            (" default ", STATE_FILE),
            ("", "WEIXIN-CONNECT-STATE.JSON"),
            ("A/B", "weixin-connect-state-A_B.json"),
            ("A\\B", "weixin-connect-state-a_b.json"),
            ("a:b", "weixin-connect-state-a_b.json"),
            ("a\0b", "weixin-connect-state-a_b.json"),
            ("A_B", "weixin-connect-state-A_B.json"),
            ("Mixed", "weixin-connect-state-Mixed.json"),
            ("lower", "weixin-connect-state-LOWER.json"),
            (" lower ", "weixin-connect-state-lower.json"),
            ("a/b", "weixin-connect-state-a_b.json"),
            ("账号", "weixin-connect-state-账号.json"),
            ("É", "weixin-connect-state-é.json"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let legacy = temp.path().join(legacy_name);
            let bytes = serde_json::to_vec(&populated_state()).unwrap();
            std::fs::write(&legacy, &bytes).unwrap();
            let store = ConnectSessionStore::in_directory(temp.path(), account);
            let error = store.load().unwrap_err().to_string();
            assert!(error.contains("迁移归属不明确"), "{account}: {error}");
            assert!(error.contains("拒绝创建新会话"), "{account}: {error}");
            assert!(!store.path.exists(), "不得创建替代状态文件");
            assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
        }
    }

    #[test]
    fn accounts_without_matching_legacy_state_can_start_normally() {
        for account in [
            "", "default", "lower", "A/B", "A_B", "Mixed", "账号", " lower ",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = ConnectSessionStore::in_directory(temp.path(), account);
            let expected = ConnectState {
                account_id: Some(account.to_string()),
                ..ConnectState::default()
            };
            assert_eq!(store.load().unwrap(), expected);
            std::fs::write(
                temp.path().join("weixin-connect-state-unrelated.json"),
                b"unrelated legacy must not be read",
            )
            .unwrap();
            assert_eq!(store.load().unwrap(), expected);
            assert!(!store.path.exists());
            store.save(&expected).unwrap();
            assert_eq!(store.load().unwrap(), expected);
        }
    }

    #[test]
    fn owned_namespace_takes_priority_over_ambiguous_legacy_state() {
        for (account, name) in [
            ("default", STATE_FILE),
            ("A/B", "weixin-connect-state-a_b.json"),
            ("lower", "weixin-connect-state-LOWER.json"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = ConnectSessionStore::in_directory(temp.path(), account);
            let legacy = temp.path().join(name);
            let bytes = b"ambiguous legacy is not parsed or changed";
            std::fs::write(&legacy, bytes).unwrap();
            assert!(store.load().is_err());
            let expected = ConnectState {
                account_id: Some(account.to_string()),
                ..populated_state()
            };
            store.save(&expected).unwrap();
            assert_eq!(store.load().unwrap(), expected);
            assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
        }
    }

    #[test]
    fn wrong_or_missing_new_owner_is_rejected_without_fallback_or_overwrite() {
        for owner in [None, Some("other")] {
            let temp = tempfile::tempdir().unwrap();
            let store = ConnectSessionStore::in_directory(temp.path(), "lower");
            std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
            let mut state = populated_state();
            state.account_id = owner.map(str::to_string);
            let bytes = serde_json::to_vec(&state).unwrap();
            std::fs::write(&store.path, &bytes).unwrap();
            let legacy = temp.path().join("weixin-connect-state-lower.json");
            let legacy_bytes = serde_json::to_vec(&populated_state()).unwrap();
            std::fs::write(&legacy, &legacy_bytes).unwrap();
            assert!(store.load().is_err());
            assert!(store.save(&ConnectState::default()).is_err());
            assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
            assert_eq!(std::fs::read(&legacy).unwrap(), legacy_bytes);
            assert_eq!(
                ConnectSessionStore::new(store.path.clone()).load().unwrap(),
                state
            );
        }
        let temp = tempfile::tempdir().unwrap();
        let store = ConnectSessionStore::in_directory(temp.path(), "lower");
        let mut foreign = populated_state();
        foreign.account_id = Some("other".into());
        assert!(store.save(&foreign).is_err());
        assert!(!store.path.exists());
    }

    #[test]
    fn legacy_owner_must_match_even_for_an_eligible_account() {
        let temp = tempfile::tempdir().unwrap();
        let store = ConnectSessionStore::in_directory(temp.path(), "lower");
        let mut legacy = populated_state();
        legacy.account_id = Some("other".into());
        let path = temp.path().join("weixin-connect-state-lower.json");
        let bytes = serde_json::to_vec(&legacy).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert!(store.load().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(!store.path.exists());
    }

    #[test]
    fn state_store_round_trips_and_limits_dedup_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = ConnectSessionStore::new(temp.path().join("state.json"));
        let mut state = ConnectState {
            get_updates_buf: "cursor".to_string(),
            ..ConnectState::default()
        };
        state
            .thread_ids
            .insert("peer".to_string(), "thread".to_string());
        state
            .media_thread_ids
            .insert("peer".to_string(), "media-thread".to_string());
        for id in 1..=600 {
            state.mark_processed(format!("message-{id}"));
        }

        store.save(&state).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded.get_updates_buf, "cursor");
        assert_eq!(
            loaded.thread_ids.get("peer").map(String::as_str),
            Some("thread")
        );
        assert_eq!(
            loaded.media_thread_ids.get("peer").map(String::as_str),
            Some("media-thread")
        );
        assert!(!loaded.is_processed("message-1"));
        assert!(loaded.is_processed("message-600"));
        assert_eq!(loaded, state);
        assert!(loaded.account_id.is_none());
    }

    #[test]
    fn old_state_keeps_legacy_threads_without_claiming_attachment_tool_support() {
        let state: ConnectState =
            serde_json::from_str(r#"{"threadIds":{"peer":"legacy"}}"#).unwrap();
        assert_eq!(
            state.thread_ids.get("peer").map(String::as_str),
            Some("legacy")
        );
        assert!(state.media_thread_ids.is_empty());
    }
}
