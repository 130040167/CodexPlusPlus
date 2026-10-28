use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionOptions {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub reasoning_effort: String,
}

/// 与轮询游标分开保存，工具执行时落盘不会覆盖正在处理的消息边界。
#[derive(Debug, Clone)]
pub struct SessionOptionsStore {
    path: PathBuf,
    peer: String,
    owner: Option<String>,
    legacy_path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OwnedSessionOptions {
    account_id: String,
    options: BTreeMap<String, SessionOptions>,
}

impl SessionOptionsStore {
    #[cfg(test)]
    pub(super) fn test_store(path: PathBuf, peer: &str) -> Self {
        Self {
            path,
            peer: peer.to_string(),
            owner: None,
            legacy_path: None,
        }
    }

    pub fn for_peer(account: &str, peer: &str) -> Self {
        Self::in_directory(&crate::paths::default_app_state_dir(), account, peer)
    }

    fn in_directory(directory: &std::path::Path, account: &str, peer: &str) -> Self {
        let encoded: String = account
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let legacy_name = if account.is_empty() {
            Some("weixin-session-options.json".to_string())
        } else if account != "default"
            && account.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-".contains(&byte)
            })
        {
            Some(format!("weixin-session-options-{account}.json"))
        } else {
            None
        };
        Self {
            path: directory
                .join("weixin-session-options")
                .join(format!("account-{encoded}.json")),
            peer: peer.to_string(),
            owner: Some(account.to_string()),
            legacy_path: legacy_name.map(|name| directory.join(name)),
        }
    }

    fn read_all(
        &self,
        path: &std::path::Path,
        legacy: bool,
    ) -> anyhow::Result<Option<BTreeMap<String, SessionOptions>>> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if let Some(owner) = &self.owner
            && !legacy
        {
            let state: OwnedSessionOptions =
                serde_json::from_slice(&bytes).context("微信会话设置损坏或缺少账号归属")?;
            if &state.account_id != owner {
                bail!("微信会话设置账号归属不匹配，拒绝读写");
            }
            Ok(Some(state.options))
        } else {
            Ok(Some(
                serde_json::from_slice(&bytes).context("微信会话设置损坏")?,
            ))
        }
    }

    fn load_all(&self) -> anyhow::Result<BTreeMap<String, SessionOptions>> {
        if let Some(all) = self.read_all(&self.path, false)? {
            return Ok(all);
        }
        if let Some(account) = &self.owner {
            let directory = self
                .path
                .parent()
                .and_then(|path| path.parent())
                .context("微信会话设置路径无账号目录")?;
            let old_account = account.trim().replace(['/', '\\', ':', '\0'], "_");
            let mut legacy_names = vec![
                format!("weixin-session-options-{account}.json"),
                format!("weixin-session-options-{old_account}.json"),
            ];
            if old_account.is_empty() || old_account == "default" {
                legacy_names.push("weixin-session-options.json".to_string());
                legacy_names.push("weixin-session-options-default.json".to_string());
            }
            let expected = self.legacy_path.as_ref().and_then(|path| path.file_name());
            let mut exact = None;
            // 发现旧命名/大小写别名必须报错，不能把可能存在的覆盖静默当作空设置。
            match std::fs::read_dir(directory) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = entry?;
                        let name = entry.file_name();
                        if legacy_names.iter().any(|candidate| {
                            name.to_string_lossy().to_lowercase() == candidate.to_lowercase()
                        }) {
                            if Some(name.as_os_str()) != expected || exact.is_some() {
                                bail!("微信会话设置迁移归属错误：旧文件名存在歧义，拒绝认领或覆盖");
                            }
                            exact = Some(entry.path());
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(path) = exact {
                return self
                    .read_all(&path, true)?
                    .context("微信会话设置迁移期间旧文件已变化，拒绝使用默认设置");
            }
        }
        Ok(BTreeMap::new())
    }

    pub fn load(&self) -> anyhow::Result<SessionOptions> {
        Ok(self.load_all()?.remove(&self.peer).unwrap_or_default())
    }

    pub fn save(&self, options: SessionOptions) -> anyhow::Result<()> {
        let mut all = self.load_all()?;
        all.insert(self.peer.clone(), options);
        let bytes = if let Some(owner) = &self.owner {
            serde_json::to_vec_pretty(&OwnedSessionOptions {
                account_id: owner.clone(),
                options: all,
            })?
        } else {
            serde_json::to_vec_pretty(&all)?
        };
        crate::settings::atomic_write(&self.path, &bytes)
    }
}

pub fn effective_options(defaults: &SessionOptions, overrides: &SessionOptions) -> SessionOptions {
    SessionOptions {
        model: if overrides.model.is_empty() {
            defaults.model.clone()
        } else {
            overrides.model.clone()
        },
        reasoning_effort: if overrides.reasoning_effort.is_empty() {
            defaults.reasoning_effort.clone()
        } else {
            overrides.reasoning_effort.clone()
        },
    }
}

pub fn session_tool_spec() -> Value {
    json!({
        "type": "function", "name": "weixin_session_settings",
        "description": "查看当前微信会话的模型/推理强度和可选模型，或按用户明确要求修改当前会话。action=get 返回当前设置及目录，action=set 保存 model 和/或 reasoningEffort，空字符串恢复继承连接默认值，仅当前微信会话生效，不改全局配置。变更从下一条微信消息开始生效；当前正在生成的回合无法更换模型或档位，不得声称当前回合已切换。不得仅通过编辑全局 config.toml 修改设置。",
        "inputSchema": {
            "type": "object",
            "properties": {
                "action": {"type":"string","enum":["get","set"]},
                "model": {"type":"string"},
                "reasoningEffort": {"type":"string"}
            },
            "required":["action"], "additionalProperties":false
        }
    })
}

impl SessionOptions {
    pub fn with_defaults(&self, defaults: &Self) -> Self {
        effective_options(defaults, self)
    }

    pub fn resolve(&self, defaults: &Self, catalog: &Value) -> Self {
        let mut resolved = self.with_defaults(defaults);
        if resolved.model.is_empty() {
            resolved.model = catalog["model"]
                .as_str()
                .or_else(|| catalog["default_model"].as_str())
                .unwrap_or_default()
                .to_string();
        }
        if resolved.reasoning_effort.is_empty() {
            resolved.reasoning_effort =
                catalog["modelMetadata"][&resolved.model]["defaultReasoningEffort"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
        }
        resolved
    }
}

pub fn updated_options(
    catalog: &Value,
    defaults: &SessionOptions,
    previous: &SessionOptions,
    args: &serde_json::Map<String, Value>,
) -> anyhow::Result<SessionOptions> {
    if !args.contains_key("model") && !args.contains_key("reasoningEffort") {
        bail!("没有指定需要修改的设置");
    }
    let mut next = previous.clone();
    if let Some(value) = args.get("model") {
        next.model = value
            .as_str()
            .context("model 必须为字符串")?
            .trim()
            .to_string();
    }
    if let Some(value) = args.get("reasoningEffort") {
        next.reasoning_effort = value
            .as_str()
            .context("reasoningEffort 必须为字符串")?
            .trim()
            .to_string();
    }
    validate_options(catalog, &next.resolve(defaults, catalog))?;
    Ok(next)
}

pub fn validate_options(catalog: &Value, options: &SessionOptions) -> anyhow::Result<()> {
    if !options.model.is_empty() {
        let available = catalog["models"].as_array().context("当前模型目录不可用")?;
        if !available
            .iter()
            .any(|model| model.as_str() == Some(&options.model))
        {
            bail!("该模型不在当前模型列表中，请先查看可用模型");
        }
        let metadata = &catalog["modelMetadata"][&options.model];
        let levels = metadata
            .get("supportedReasoningEfforts")
            .or_else(|| metadata.get("supported_reasoning_levels"))
            .or_else(|| metadata.get("supportedReasoningLevels"))
            .and_then(Value::as_array);
        if let Some(levels) = levels
            && !options.reasoning_effort.is_empty()
            && !levels.iter().any(|level| {
                level
                    .as_str()
                    .or_else(|| level.get("effort").and_then(Value::as_str))
                    .or_else(|| level.get("reasoningEffort").and_then(Value::as_str))
                    == Some(&options.reasoning_effort)
            })
        {
            bail!("当前模型不支持该推理强度");
        }
        if levels.is_none()
            && ![
                "", "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
            ]
            .contains(&options.reasoning_effort.as_str())
        {
            bail!("当前模型目录未声明该推理强度");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_namespaces_do_not_collide_and_preserve_owner_and_peers() {
        let temp = tempfile::tempdir().unwrap();
        let accounts = [
            "a/b", "a\\b", "a:b", "a\0b", "a_b", "A_b", "", "default", "../a", "中文", "a_b ",
        ];
        let mut paths = std::collections::BTreeSet::new();
        for (index, account) in accounts.iter().enumerate() {
            let store = SessionOptionsStore::in_directory(temp.path(), account, "peer");
            assert!(paths.insert(store.path.to_string_lossy().to_lowercase()));
            assert_eq!(
                store.path.parent().unwrap(),
                temp.path().join("weixin-session-options")
            );
            store
                .save(SessionOptions {
                    model: index.to_string(),
                    reasoning_effort: String::new(),
                })
                .unwrap();
            SessionOptionsStore::in_directory(temp.path(), account, "other-peer")
                .save(SessionOptions {
                    model: "other".into(),
                    reasoning_effort: "low".into(),
                })
                .unwrap();
            let state: Value =
                serde_json::from_slice(&std::fs::read(&store.path).unwrap()).unwrap();
            assert_eq!(state["accountId"], *account);
        }
        for (index, account) in accounts.iter().enumerate() {
            assert_eq!(
                SessionOptionsStore::in_directory(temp.path(), account, "peer")
                    .load()
                    .unwrap()
                    .model,
                index.to_string()
            );
            assert_eq!(
                SessionOptionsStore::in_directory(temp.path(), account, "other-peer")
                    .load()
                    .unwrap()
                    .model,
                "other"
            );
        }
        assert_eq!(
            SessionOptionsStore::in_directory(temp.path(), "A_b", "peer")
                .path
                .file_name()
                .unwrap(),
            "account-415f62.json"
        );
        assert_eq!(
            SessionOptionsStore::in_directory(temp.path(), "中文", "peer")
                .path
                .file_name()
                .unwrap(),
            "account-e4b8ade69687.json"
        );
    }

    #[test]
    fn safe_legacy_settings_are_copied_on_save_without_rewriting_legacy() {
        for (account, name) in [
            ("", "weixin-session-options.json"),
            (
                "ordinary_123-safe",
                "weixin-session-options-ordinary_123-safe.json",
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let legacy = temp.path().join(name);
            let bytes = br#"{"peer":{"model":"legacy","reasoningEffort":"low"},"other":{"model":"preserved"}}"#;
            std::fs::write(&legacy, bytes).unwrap();
            let store = SessionOptionsStore::in_directory(temp.path(), account, "peer");
            assert_eq!(store.load().unwrap().model, "legacy");
            assert!(!store.path.exists(), "load must not write a migration");
            store
                .save(SessionOptions {
                    model: "new".into(),
                    reasoning_effort: "high".into(),
                })
                .unwrap();
            assert_eq!(std::fs::read(&legacy).unwrap(), bytes);
            assert_eq!(store.load().unwrap().model, "new");
            assert_eq!(
                SessionOptionsStore::in_directory(temp.path(), account, "other")
                    .load()
                    .unwrap()
                    .model,
                "preserved"
            );
            std::fs::write(&legacy, b"broken legacy after migration").unwrap();
            assert_eq!(
                store.load().unwrap().model,
                "new",
                "owned namespace takes priority"
            );
        }
    }

    #[test]
    fn ambiguous_legacy_settings_reject_load_and_save_without_silent_defaults() {
        for (account, name) in [
            ("default", "weixin-session-options.json"),
            ("default", "weixin-session-options-default.json"),
            ("A_b", "weixin-session-options-A_b.json"),
            ("lower", "weixin-session-options-LOWER.json"),
            ("a/b", "weixin-session-options-a_b.json"),
            ("a\\b", "weixin-session-options-a_b.json"),
            ("a:b", "weixin-session-options-a_b.json"),
            ("a\0b", "weixin-session-options-a_b.json"),
            (" lower ", "weixin-session-options-lower.json"),
            ("中文", "weixin-session-options-中文.json"),
            ("Ä", "weixin-session-options-ä.json"),
            ("", "weixin-session-options-default.json"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let legacy = temp.path().join(name);
            let bytes = br#"{"peer":{"model":"must-not-adopt"}}"#;
            std::fs::write(&legacy, bytes).unwrap();
            let store = SessionOptionsStore::in_directory(temp.path(), account, "peer");
            let error = store.load().unwrap_err();
            assert!(
                error.to_string().contains("迁移归属错误"),
                "{account}: {error:#}"
            );
            assert!(store.save(SessionOptions::default()).is_err());
            assert_eq!(std::fs::read(legacy).unwrap(), bytes);
            assert!(!store.path.exists());
        }
    }

    #[test]
    fn owned_settings_take_priority_over_ambiguous_legacy_and_other_accounts_stay_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let owner = SessionOptionsStore::in_directory(temp.path(), "A_b", "peer");
        owner
            .save(SessionOptions {
                model: "owned".into(),
                reasoning_effort: "high".into(),
            })
            .unwrap();
        let legacy = temp.path().join("weixin-session-options-A_b.json");
        let bytes = br#"{"peer":{"model":"ambiguous"}}"#;
        std::fs::write(&legacy, bytes).unwrap();
        assert_eq!(owner.load().unwrap().model, "owned");
        owner
            .save(SessionOptions {
                model: "updated".into(),
                reasoning_effort: "low".into(),
            })
            .unwrap();
        assert_eq!(owner.load().unwrap().model, "updated");
        let lower = SessionOptionsStore::in_directory(temp.path(), "a_b", "peer");
        assert!(lower.load().is_err());
        assert!(lower.save(SessionOptions::default()).is_err());
        let unrelated = SessionOptionsStore::in_directory(temp.path(), "unrelated", "peer");
        assert_eq!(unrelated.load().unwrap(), SessionOptions::default());
        unrelated
            .save(SessionOptions {
                model: "independent".into(),
                reasoning_effort: String::new(),
            })
            .unwrap();
        assert_eq!(unrelated.load().unwrap().model, "independent");
        assert_eq!(owner.load().unwrap().model, "updated");
        assert_eq!(std::fs::read(legacy).unwrap(), bytes);
        assert!(!lower.path.exists());
    }

    #[test]
    fn safe_empty_account_file_does_not_hide_an_ambiguous_default_alias() {
        let temp = tempfile::tempdir().unwrap();
        let bytes = br#"{"peer":{"model":"legacy"}}"#;
        for name in [
            "weixin-session-options.json",
            "weixin-session-options-default.json",
        ] {
            std::fs::write(temp.path().join(name), bytes).unwrap();
        }
        let store = SessionOptionsStore::in_directory(temp.path(), "", "peer");
        assert!(
            store
                .load()
                .unwrap_err()
                .to_string()
                .contains("迁移归属错误")
        );
        assert!(store.save(SessionOptions::default()).is_err());
        assert!(!store.path.exists());
        for name in [
            "weixin-session-options.json",
            "weixin-session-options-default.json",
        ] {
            assert_eq!(std::fs::read(temp.path().join(name)).unwrap(), bytes);
        }
    }

    #[test]
    fn wrong_or_missing_new_owner_rejects_load_and_save_without_legacy_fallback() {
        for invalid in [
            json!({"accountId":"other","options":{"peer":{"model":"foreign"}}}),
            json!({"accountId":null,"options":{"peer":{"model":"foreign"}}}),
            json!({"options":{"peer":{"model":"unowned"}}}),
            json!({"peer":{"model":"old-unowned-hex-file"}}),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let store = SessionOptionsStore::in_directory(temp.path(), "lower", "peer");
            std::fs::create_dir_all(store.path.parent().unwrap()).unwrap();
            let bytes = serde_json::to_vec(&invalid).unwrap();
            std::fs::write(&store.path, &bytes).unwrap();
            let legacy = temp.path().join("weixin-session-options-lower.json");
            let legacy_bytes = br#"{"peer":{"model":"legacy"}}"#;
            std::fs::write(&legacy, legacy_bytes).unwrap();
            assert!(store.load().is_err());
            assert!(store.save(SessionOptions::default()).is_err());
            assert_eq!(std::fs::read(&store.path).unwrap(), bytes);
            assert_eq!(std::fs::read(legacy).unwrap(), legacy_bytes);
        }
    }

    #[test]
    fn invalid_legacy_owner_is_not_silently_adopted_or_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = temp.path().join("weixin-session-options-lower.json");
        let bytes = br#"{"accountId":"other","options":{"peer":{"model":"foreign"}}}"#;
        std::fs::write(&legacy, bytes).unwrap();
        let store = SessionOptionsStore::in_directory(temp.path(), "lower", "peer");
        assert!(store.load().is_err());
        assert!(store.save(SessionOptions::default()).is_err());
        assert!(!store.path.exists());
        assert_eq!(std::fs::read(legacy).unwrap(), bytes);
    }

    #[test]
    fn explicit_test_store_keeps_unowned_map_compatibility() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("options.json");
        let bytes = br#"{"peer":{"model":"existing","reasoningEffort":"low"}}"#;
        std::fs::write(&path, bytes).unwrap();
        let store = SessionOptionsStore::test_store(path.clone(), "peer");
        assert_eq!(store.load().unwrap().model, "existing");
        store
            .save(SessionOptions {
                model: "updated".into(),
                reasoning_effort: String::new(),
            })
            .unwrap();
        let saved: BTreeMap<String, SessionOptions> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved["peer"].model, "updated");
    }

    #[test]
    fn next_message_reloads_overrides_and_clearing_uses_connection_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let a = SessionOptionsStore::in_directory(temp.path(), "account", "a");
        let b = SessionOptionsStore::in_directory(temp.path(), "account", "b");
        let defaults = SessionOptions {
            model: "global".into(),
            reasoning_effort: "low".into(),
        };
        let current = effective_options(&defaults, &a.load().unwrap());
        let overrides = SessionOptions {
            model: "override".into(),
            reasoning_effort: "high".into(),
        };
        a.save(overrides.clone()).unwrap();
        assert_eq!(current, defaults);
        assert_eq!(effective_options(&defaults, &a.load().unwrap()), overrides);
        assert_eq!(effective_options(&defaults, &b.load().unwrap()), defaults);
        a.save(SessionOptions::default()).unwrap();
        let next = effective_options(&defaults, &a.load().unwrap());
        assert_eq!(next, defaults);
        let catalog = json!({"models":["global","override"],"modelMetadata":{
            "global":{"supportedReasoningEfforts":[{"reasoningEffort":"low"}]},
            "override":{"supportedReasoningEfforts":[{"reasoningEffort":"high"}]}
        }});
        validate_options(&catalog, &next).unwrap();
        assert!(
            validate_options(
                &catalog,
                &SessionOptions {
                    model: "global".into(),
                    reasoning_effort: "high".into()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn metadata_effort_shapes_are_validated() {
        for key in [
            "supportedReasoningEfforts",
            "supported_reasoning_levels",
            "supportedReasoningLevels",
        ] {
            for levels in [
                json!(["low"]),
                json!([{"effort":"low"}]),
                json!([{"reasoningEffort":"low"}]),
            ] {
                let mut catalog = json!({"models":["m"],"modelMetadata":{"m":{}}});
                catalog["modelMetadata"]["m"][key] = levels;
                validate_options(
                    &catalog,
                    &SessionOptions {
                        model: "m".into(),
                        reasoning_effort: "low".into(),
                    },
                )
                .unwrap();
                assert!(
                    validate_options(
                        &catalog,
                        &SessionOptions {
                            model: "m".into(),
                            reasoning_effort: "high".into()
                        }
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn session_options_are_scoped_and_survive_reload() {
        let temp = tempfile::tempdir().unwrap();
        let a = SessionOptionsStore {
            path: temp.path().join("options.json"),
            peer: "a".into(),
            owner: None,
            legacy_path: None,
        };
        let b = SessionOptionsStore {
            peer: "b".into(),
            ..a.clone()
        };
        a.save(SessionOptions {
            model: "m".into(),
            reasoning_effort: "low".into(),
        })
        .unwrap();
        assert_eq!(b.load().unwrap(), SessionOptions::default());
        b.save(SessionOptions {
            model: "n".into(),
            reasoning_effort: "high".into(),
        })
        .unwrap();
        assert_eq!(a.load().unwrap().reasoning_effort, "low");
        assert_eq!(b.load().unwrap().model, "n");
    }

    #[test]
    fn invalid_models_and_unsupported_efforts_are_rejected() {
        let catalog = json!({"models":["m"],"modelMetadata":{"m":{"supportedReasoningEfforts":[{"reasoningEffort":"low"},{"reasoningEffort":"high"}]}}});
        let mut options = SessionOptions {
            model: "m".into(),
            reasoning_effort: "low".into(),
        };
        validate_options(&catalog, &options).unwrap();
        options.reasoning_effort = "xhigh".into();
        assert!(validate_options(&catalog, &options).is_err());
        options.model = "unknown".into();
        assert!(validate_options(&catalog, &options).is_err());
    }

    #[test]
    fn remote_change_and_reset_use_connection_defaults_not_previous_turn() {
        let catalog = json!({"models":["m","n"],"modelMetadata":{
            "m":{"supportedReasoningEfforts":[{"reasoningEffort":"high"}]},
            "n":{"supportedReasoningEfforts":[{"reasoningEffort":"low"}]}
        }});
        let defaults = SessionOptions {
            model: "m".into(),
            reasoning_effort: "high".into(),
        };
        let args = json!({"model":"n","reasoningEffort":"low"});
        let next = updated_options(
            &catalog,
            &defaults,
            &SessionOptions::default(),
            args.as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(next.model, "n");
        let reset = json!({"model":"","reasoningEffort":""});
        let reset =
            updated_options(&catalog, &defaults, &next, reset.as_object().unwrap()).unwrap();
        assert_eq!(reset.with_defaults(&defaults), defaults);
        let invalid = json!({"reasoningEffort":"high"});
        assert!(updated_options(&catalog, &defaults, &next, invalid.as_object().unwrap()).is_err());
        assert_eq!(next.reasoning_effort, "low");
    }

    #[test]
    fn inherited_effort_uses_model_default_when_global_effort_is_unset() {
        let defaults = SessionOptions {
            model: "m".into(),
            reasoning_effort: String::new(),
        };
        let catalog = json!({"model":"m","models":["m"],"modelMetadata":{
            "m":{"defaultReasoningEffort":"adaptive","supportedReasoningEfforts":[{"reasoningEffort":"adaptive"}]}
        }});
        let inherited = SessionOptions::default().resolve(&defaults, &catalog);
        assert_eq!(inherited.reasoning_effort, "adaptive");
        validate_options(&catalog, &inherited).unwrap();
    }
}
