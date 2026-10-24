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
    legacy_path: Option<PathBuf>,
}

impl SessionOptionsStore {
    #[cfg(test)]
    pub(super) fn test_store(path: PathBuf, peer: &str) -> Self {
        Self {
            path,
            peer: peer.to_string(),
            legacy_path: None,
        }
    }

    pub fn for_peer(account: &str, peer: &str) -> Self {
        Self::in_directory(&crate::paths::default_app_state_dir(), account, peer)
    }

    fn in_directory(directory: &std::path::Path, account: &str, peer: &str) -> Self {
        // 普通账号沿用旧文件；其余账号按 UTF-8 字节编码到独立命名空间，
        // 避免路径替换和 Windows 大小写折叠导致串号。
        let path = if !account.is_empty()
            && account.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
        {
            directory.join(format!("weixin-session-options-{account}.json"))
        } else {
            let encoded: String = account.as_bytes().iter().map(|byte| format!("{byte:02x}")).collect();
            directory.join("weixin-session-options").join(format!("account-{encoded}.json"))
        };
        Self {
            path,
            peer: peer.to_string(),
            legacy_path: (!account.is_empty()
                && account.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                && account.bytes().any(|c| c.is_ascii_uppercase()))
                .then(|| directory.join(format!("weixin-session-options-{account}.json"))),
        }
    }

    fn load_all(&self) -> anyhow::Result<BTreeMap<String, SessionOptions>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("微信会话设置损坏")?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Some(legacy) = &self.legacy_path {
                    // 精确核对原文件名，不能让 Windows 用小写账号的旧文件匹配大写账号。
                    match std::fs::read_dir(legacy.parent().context("旧设置路径无父目录")?) {
                        Ok(entries) => {
                            for entry in entries {
                                let entry = entry?;
                                if Some(entry.file_name().as_os_str()) == legacy.file_name() {
                                    return Ok(serde_json::from_slice(&std::fs::read(entry.path())?)
                                        .context("微信会话设置损坏")?);
                                }
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                Ok(BTreeMap::new())
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn load(&self) -> anyhow::Result<SessionOptions> {
        Ok(self.load_all()?.remove(&self.peer).unwrap_or_default())
    }

    pub fn save(&self, options: SessionOptions) -> anyhow::Result<()> {
        let mut all = self.load_all()?;
        all.insert(self.peer.clone(), options);
        crate::settings::atomic_write(&self.path, &serde_json::to_vec_pretty(&all)?)
    }
}

pub fn effective_options(defaults: &SessionOptions, overrides: &SessionOptions) -> SessionOptions {
    SessionOptions {
        model: if overrides.model.is_empty() { defaults.model.clone() } else { overrides.model.clone() },
        reasoning_effort: if overrides.reasoning_effort.is_empty() { defaults.reasoning_effort.clone() } else { overrides.reasoning_effort.clone() },
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
                level.as_str()
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
    fn account_paths_do_not_collide_and_ordinary_legacy_files_still_load() {
        let temp = tempfile::tempdir().unwrap();
        let accounts = ["a/b", "a\\b", "a:b", "a\0b", "a_b", "A_b", "", "../a", "中文"];
        let mut paths = std::collections::BTreeSet::new();
        for (index, account) in accounts.iter().enumerate() {
            let store = SessionOptionsStore::in_directory(temp.path(), account, "peer");
            assert!(paths.insert(store.path.to_string_lossy().to_lowercase()));
            store.save(SessionOptions { model: index.to_string(), reasoning_effort: String::new() }).unwrap();
        }
        for (index, account) in accounts.iter().enumerate() {
            assert_eq!(SessionOptionsStore::in_directory(temp.path(), account, "peer").load().unwrap().model, index.to_string());
        }
        let legacy = temp.path().join("weixin-session-options-ordinary_123.json");
        std::fs::write(&legacy, br#"{"peer":{"model":"legacy","reasoningEffort":"low"}}"#).unwrap();
        let store = SessionOptionsStore::in_directory(temp.path(), "ordinary_123", "peer");
        assert_eq!(store.path, legacy);
        assert_eq!(store.load().unwrap().model, "legacy");
        let upper = temp.path().join("weixin-session-options-Upper_123.json");
        std::fs::write(&upper, br#"{"peer":{"model":"legacy-upper"}}"#).unwrap();
        let store = SessionOptionsStore::in_directory(temp.path(), "Upper_123", "peer");
        assert_eq!(store.load().unwrap().model, "legacy-upper");
        store.save(SessionOptions { model: "new".into(), reasoning_effort: String::new() }).unwrap();
        assert_eq!(store.load().unwrap().model, "new");
        assert!(std::fs::read_to_string(upper).unwrap().contains("legacy-upper"));
    }

    #[test]
    fn next_message_reloads_overrides_and_clearing_uses_connection_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let a = SessionOptionsStore::in_directory(temp.path(), "account", "a");
        let b = SessionOptionsStore::in_directory(temp.path(), "account", "b");
        let defaults = SessionOptions { model: "global".into(), reasoning_effort: "low".into() };
        let current = effective_options(&defaults, &a.load().unwrap());
        let overrides = SessionOptions { model: "override".into(), reasoning_effort: "high".into() };
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
        assert!(validate_options(&catalog, &SessionOptions { model: "global".into(), reasoning_effort: "high".into() }).is_err());
    }

    #[test]
    fn metadata_effort_shapes_are_validated() {
        for key in ["supportedReasoningEfforts", "supported_reasoning_levels", "supportedReasoningLevels"] {
            for levels in [json!(["low"]), json!([{"effort":"low"}]), json!([{"reasoningEffort":"low"}])] {
                let mut catalog = json!({"models":["m"],"modelMetadata":{"m":{}}});
                catalog["modelMetadata"]["m"][key] = levels;
                validate_options(&catalog, &SessionOptions { model: "m".into(), reasoning_effort: "low".into() }).unwrap();
                assert!(validate_options(&catalog, &SessionOptions { model: "m".into(), reasoning_effort: "high".into() }).is_err());
            }
        }
    }

    #[test]
    fn session_options_are_scoped_and_survive_reload() {
        let temp = tempfile::tempdir().unwrap();
        let a = SessionOptionsStore {
            path: temp.path().join("options.json"),
            peer: "a".into(),
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
