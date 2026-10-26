//! 网关模式的持久状态：`<状态目录>/gateway.json`。
//!
//! 不放进 `settings.json`：那份文件由前端整份保存，网关绑定写在里面会被覆盖。
//! 供应商池与路由组仍以 `settings.json` 的 `relayProfiles` / `aggregateRelayProfiles`
//! 为准，这里只存「哪个 Agent 走网关、指向哪个供应商或路由组、用哪些模型」。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::tools::ToolId;

const STORE_FILE: &str = "gateway.json";
const STORE_VERSION: u32 = 1;
/// 客户端 token 前缀；日志里看到它就知道是网关签发的本地凭据，不是上游 Key。
pub const CLIENT_TOKEN_PREFIX: &str = "cpx-gw-";

/// 绑定目标：单个供应商，或一个路由组（复用聚合供应商的成员与策略）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayTarget {
    pub kind: GatewayTargetKind,
    pub id: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GatewayTargetKind {
    #[default]
    Provider,
    Group,
}

/// 请求里的模型不在绑定的模型集合里时怎么办。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UnknownModelPolicy {
    /// 改写成绑定的主模型。Claude Code 的后台任务会固定请求 claude-haiku-* 之类的
    /// 模型名，第三方上游没有，原样转发只会报错。
    #[default]
    MapToDefault,
    /// 原样转发。
    Passthrough,
}

/// Claude Code 的分档模型（opus / sonnet / haiku / 子代理）。空串表示沿用主模型。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeRoleModels {
    #[serde(default)]
    pub opus: String,
    #[serde(default)]
    pub sonnet: String,
    #[serde(default)]
    pub haiku: String,
    #[serde(default)]
    pub subagent: String,
}

/// 开启网关前的现场，关闭时据此恢复。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRestoreState {
    /// 开启前 Claude Code 正在用的供应商 id（`claude-providers.json` 里）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub claude_previous_provider_id: String,
    /// 写进各 Agent 配置时用的网关端口；端口变化后要重写。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_port: Option<u16>,
}

/// 一个 Agent 的网关绑定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentBinding {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub target: GatewayTarget,
    /// 主模型：写进 Agent 配置的默认模型，也是未知模型的改写目标。
    /// 为空时用目标供应商自己的默认模型。
    #[serde(default)]
    pub model: String,
    /// 额外暴露给 Agent 的模型（每行一个，可带 `[1M]` 窗口后缀）。
    #[serde(default)]
    pub extra_models: Vec<String>,
    #[serde(default)]
    pub claude_roles: ClaudeRoleModels,
    #[serde(default)]
    pub unknown_model: UnknownModelPolicy,
    /// 写给 Agent 的上下文窗口（Claude Code 的 `CLAUDE_CODE_MAX_CONTEXT_TOKENS`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 非 claude-* 模型名时 Claude Code 默认不发 effort，打开后强制启用。
    #[serde(default)]
    pub claude_enable_effort: bool,
    /// 网关签发给该 Agent 的本地 token，用来鉴权本地请求。
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub restore: GatewayRestoreState,
}

impl Default for AgentBinding {
    fn default() -> Self {
        Self {
            enabled: false,
            target: GatewayTarget::default(),
            model: String::new(),
            extra_models: Vec::new(),
            claude_roles: ClaudeRoleModels::default(),
            unknown_model: UnknownModelPolicy::default(),
            context_window: None,
            claude_enable_effort: false,
            token: String::new(),
            restore: GatewayRestoreState::default(),
        }
    }
}

impl AgentBinding {
    /// 绑定里声明过的模型名（主模型、分档模型、额外模型），去掉窗口后缀、去重保序。
    pub fn declared_models(&self) -> Vec<String> {
        let mut models = Vec::new();
        let roles = [
            &self.claude_roles.opus,
            &self.claude_roles.sonnet,
            &self.claude_roles.haiku,
            &self.claude_roles.subagent,
        ];
        let candidates = std::iter::once(&self.model)
            .chain(roles)
            .chain(self.extra_models.iter());
        for raw in candidates {
            let (slug, _) = crate::model_suffix::parse_model_suffix(raw.trim());
            let slug = slug.trim().to_string();
            if !slug.is_empty() && !models.contains(&slug) {
                models.push(slug);
            }
        }
        models
    }
}

/// Key 轮换策略（多 Key 供应商）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum KeyRotationMode {
    /// 会话粘性：同一会话固定用一个 Key，出错才换。保护上游 prompt cache。
    #[default]
    Sticky,
    /// 每个请求轮换下一个 Key。
    RoundRobin,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyPolicy {
    #[serde(default)]
    pub mode: KeyRotationMode,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySettings {
    #[serde(default)]
    pub version: u32,
    /// 键是 Agent 标识（`claude` / `grok`）。
    #[serde(default)]
    pub bindings: BTreeMap<String, AgentBinding>,
    #[serde(default)]
    pub key_policy: KeyPolicy,
    /// 退出 Codex++ 时把走网关的 Agent 恢复成直连，避免网关不在时连不上。
    #[serde(default)]
    pub restore_on_exit: bool,
    /// 供应商池：加入网关的供应商 id（引用 Codex 供应商列表，不复制数据）。
    /// 旧版状态文件没有这个字段，视为空池；已绑定的供应商总会算在池里。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pool: Vec<String>,
}

impl GatewaySettings {
    /// 把供应商加入池（已在池里则不变）。
    pub fn add_to_pool(&mut self, relay_id: &str) {
        let relay_id = relay_id.trim();
        if !relay_id.is_empty() && !self.pool.iter().any(|id| id == relay_id) {
            self.pool.push(relay_id.to_string());
        }
    }

    /// 绑定直接指向的供应商 id（不含路由组成员）。
    pub fn bound_provider_ids(&self) -> Vec<String> {
        self.bindings
            .values()
            .filter(|binding| binding.target.kind == GatewayTargetKind::Provider)
            .map(|binding| binding.target.id.clone())
            .filter(|id| !id.is_empty())
            .collect()
    }

    pub fn binding(&self, agent: &ToolId) -> Option<&AgentBinding> {
        self.bindings.get(agent.as_str())
    }

    pub fn active_binding(&self, agent: &ToolId) -> Option<&AgentBinding> {
        self.binding(agent).filter(|binding| binding.enabled)
    }

    pub fn any_enabled(&self) -> bool {
        self.bindings.values().any(|binding| binding.enabled)
    }
}

/// 网关状态文件的位置。命令层用 [`GatewayStore::default_path`]，测试指向临时目录。
#[derive(Debug, Clone)]
pub struct GatewayStore {
    pub path: PathBuf,
}

impl Default for GatewayStore {
    fn default() -> Self {
        Self {
            path: Self::default_path(),
        }
    }
}

impl GatewayStore {
    pub fn default_path() -> PathBuf {
        crate::paths::default_app_state_dir().join(STORE_FILE)
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// 读状态；文件不存在或为空视作全部关闭。
    pub fn load(&self) -> anyhow::Result<GatewaySettings> {
        read_store(&self.path)
    }

    pub fn save(&self, settings: &GatewaySettings) -> anyhow::Result<()> {
        write_store(&self.path, settings)
    }
}

fn read_store(path: &Path) -> anyhow::Result<GatewaySettings> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GatewaySettings::default());
        }
        Err(error) => return Err(error).with_context(|| format!("无法读取 {}", path.display())),
    };
    if text.trim().is_empty() {
        return Ok(GatewaySettings::default());
    }
    serde_json::from_str(&text).with_context(|| format!("无法解析 {}", path.display()))
}

fn write_store(path: &Path, settings: &GatewaySettings) -> anyhow::Result<()> {
    let mut settings = settings.clone();
    settings.version = STORE_VERSION;
    let existed = path.is_file();
    let mut text = serde_json::to_string_pretty(&settings)?;
    text.push('\n');
    crate::settings::atomic_write(path, text.as_bytes())
        .with_context(|| format!("无法写入 {}", path.display()))?;
    // 里面有本地 token：新建的文件只给本人读写。
    if !existed {
        set_private(path)?;
    }
    Ok(())
}

#[cfg(unix)]
fn set_private(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("无法设置权限 {}", path.display()))
}

#[cfg(not(unix))]
fn set_private(_path: &Path) -> anyhow::Result<()> {
    Ok(())
}

/// 为 Agent 签发一个新的本地 token。
pub fn new_client_token(agent: &ToolId) -> String {
    format!(
        "{CLIENT_TOKEN_PREFIX}{}-{}",
        agent.as_str(),
        uuid::Uuid::new_v4().simple()
    )
}

/// 常量时间比较，避免按响应时间猜 token。
pub fn token_matches(expected: &str, provided: &str) -> bool {
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();
    if expected.is_empty() || expected.len() != provided.len() {
        return false;
    }
    expected
        .iter()
        .zip(provided)
        .fold(0_u8, |diff, (left, right)| diff | (left ^ right))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_reads_as_all_disabled() {
        let temp = tempfile::tempdir().unwrap();
        let store = GatewayStore::at(temp.path().join("gateway.json"));
        let settings = store.load().unwrap();
        assert!(!settings.any_enabled());
        assert!(settings.active_binding(&ToolId::Claude).is_none());
    }

    #[test]
    fn round_trip_keeps_bindings_and_writes_version() {
        let temp = tempfile::tempdir().unwrap();
        let store = GatewayStore::at(temp.path().join("gateway.json"));
        let mut settings = GatewaySettings::default();
        settings.bindings.insert(
            "claude".to_string(),
            AgentBinding {
                enabled: true,
                target: GatewayTarget {
                    kind: GatewayTargetKind::Group,
                    id: "pool".to_string(),
                },
                model: "deepseek-v4-pro".to_string(),
                token: new_client_token(&ToolId::Claude),
                ..AgentBinding::default()
            },
        );
        store.save(&settings).unwrap();
        let text = std::fs::read_to_string(&store.path).unwrap();
        assert!(text.contains("\"version\": 1"), "{text}");
        assert!(text.contains("\"kind\": \"group\""), "{text}");
        let loaded = store.load().unwrap();
        assert_eq!(loaded.bindings, settings.bindings);
        assert!(loaded.any_enabled());
    }

    #[test]
    fn declared_models_strip_window_suffix_and_dedupe() {
        let binding = AgentBinding {
            model: "deepseek-v4-pro[1M]".to_string(),
            extra_models: vec!["deepseek-v4-pro".to_string(), "glm-5.3".to_string()],
            claude_roles: ClaudeRoleModels {
                haiku: "deepseek-v4-flash".to_string(),
                ..ClaudeRoleModels::default()
            },
            ..AgentBinding::default()
        };
        assert_eq!(
            binding.declared_models(),
            vec!["deepseek-v4-pro", "deepseek-v4-flash", "glm-5.3"]
        );
    }

    #[test]
    fn token_comparison_requires_exact_match() {
        let token = new_client_token(&ToolId::Claude);
        assert!(token.starts_with("cpx-gw-claude-"));
        assert!(token_matches(&token, &token.clone()));
        assert!(!token_matches(&token, "cpx-gw-claude-x"));
        assert!(!token_matches("", ""));
    }
}
