//! 网关模式命令：Agent 绑定的开关、供应商池导入、Key 健康状态。
//!
//! 具体行为在 `codex_plus_core::gateway`；这里负责读写当前设置、同步常驻 helper 和提示文案。

use serde::Serialize;

use codex_plus_core::gateway::activity::{self, ActivitySnapshot};
use codex_plus_core::gateway::apply::{self, ApplyContext};
use codex_plus_core::gateway::key_pool::{self, PoolSnapshot};
use codex_plus_core::gateway::store::{
    AgentBinding, GatewaySettings, GatewayStore, KeyRotationMode, new_client_token,
};
use codex_plus_core::settings::{BackendSettings, RelayProtocol, SettingsStore};
use codex_plus_core::tools::ToolId;
use codex_plus_core::tools::claude::{self, ClaudeContext, GATEWAY_PROVIDER_ID};

use crate::commands::{CommandResult, failed, ok, sync_resident_helper};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayPoolEntry {
    pub id: String,
    pub name: String,
    /// 只用于界面判断预设是否已添加（地址不含 Key）。
    pub base_url: String,
    pub protocol: RelayProtocol,
    pub official: bool,
    pub models: Vec<String>,
    pub key_count: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayGroupEntry {
    pub id: String,
    pub name: String,
    pub strategy: codex_plus_core::settings::AggregateRelayStrategy,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayAgentView {
    pub agent: String,
    pub name: String,
    /// 本机装的 CLI 版本（后台探测，探测完成前为空）。
    pub version: Option<String>,
    /// 开启网关时改写的配置文件，界面上展示给用户。
    pub config_path: String,
    pub enabled: bool,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySourceEntry {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayPayload {
    pub gateway: GatewaySettings,
    pub port: u16,
    pub helper_running: bool,
    pub pool: Vec<GatewayPoolEntry>,
    /// 还没加入池的 Codex 供应商，可在「添加供应商」里挑选加入。
    pub codex_providers: Vec<GatewayPoolEntry>,
    pub groups: Vec<GatewayGroupEntry>,
    pub agents: Vec<GatewayAgentView>,
    pub health: PoolSnapshot,
    /// 路由动态：各 Agent 最近一次请求怎么走的、计数、正在回答的上游。
    pub activity: ActivitySnapshot,
    /// 可导入供应商池的 Claude 供应商（不含网关托管的那个）。
    pub claude_providers: Vec<GatewaySourceEntry>,
    /// 可导入供应商池的 Grok 供应商。
    pub grok_profiles: Vec<GatewaySourceEntry>,
}

const GATEWAY_AGENTS: &[ToolId] = &[ToolId::Claude, ToolId::Grok];

fn port() -> u16 {
    codex_plus_core::resident_helper::resident_helper_port()
        .unwrap_or_else(codex_plus_core::protocol_proxy::protocol_proxy_port)
}

fn context() -> ApplyContext {
    ApplyContext::default_paths(port())
}

fn parse_agent(agent: &str) -> anyhow::Result<ToolId> {
    let agent = ToolId::parse(agent);
    if !GATEWAY_AGENTS.contains(&agent) {
        anyhow::bail!("{} 不支持网关模式", agent.as_str());
    }
    Ok(agent)
}

fn pool_entry(relay: &codex_plus_core::settings::RelayProfile) -> GatewayPoolEntry {
    GatewayPoolEntry {
        id: relay.id.clone(),
        name: relay.name.clone(),
        base_url: relay.base_url.trim().to_string(),
        protocol: relay.protocol,
        official: codex_plus_core::official_login::member_uses_official_login_transport(relay),
        models: codex_plus_core::gateway::route::relay_models(relay),
        key_count: key_pool::expand_keys(relay).len(),
    }
}

fn build_payload(settings: &BackendSettings, gateway: GatewaySettings) -> GatewayPayload {
    let port = port();
    let pool_relays = apply::gateway_pool(settings, &gateway);
    let codex_providers = apply::pool_relays(settings)
        .into_iter()
        .filter(|relay| !pool_relays.iter().any(|item| item.id == relay.id))
        .map(pool_entry)
        .collect();
    let pool = pool_relays.into_iter().map(pool_entry).collect();
    let groups = settings
        .aggregate_relay_profiles
        .iter()
        .map(|group| GatewayGroupEntry {
            id: group.id.clone(),
            name: group.name.clone(),
            strategy: group.strategy,
            members: group
                .members
                .iter()
                .map(|member| member.relay_id.clone())
                .collect(),
        })
        .collect();
    let agents = GATEWAY_AGENTS
        .iter()
        .map(|agent| {
            let binding = gateway.binding(agent);
            GatewayAgentView {
                agent: agent.as_str().to_string(),
                name: codex_plus_core::tools::tool_display_name(agent).to_string(),
                version: agent_version(agent),
                config_path: agent_config_path(agent).to_string(),
                enabled: binding.is_some_and(|binding| binding.enabled),
                base_url: apply::gateway_base_url(port, agent),
                default_model: binding
                    .map(|binding| apply::resolved_default_model(settings, &gateway, binding))
                    .unwrap_or_default(),
                models: codex_plus_core::gateway::server::exposed_models(settings, &gateway, agent),
            }
        })
        .collect();
    let claude_providers = claude::load(&ClaudeContext::default_paths())
        .map(|payload| {
            payload
                .providers
                .into_iter()
                .filter(|provider| provider.id != GATEWAY_PROVIDER_ID)
                .map(|provider| GatewaySourceEntry {
                    id: provider.id,
                    name: provider.name,
                })
                .collect()
        })
        .unwrap_or_default();
    let grok_profiles = settings
        .tools
        .get(&ToolId::Grok)
        .map(|config| {
            config
                .relay_profiles
                .iter()
                .map(|profile| GatewaySourceEntry {
                    id: profile.id.clone(),
                    name: profile.name.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    GatewayPayload {
        gateway,
        port,
        helper_running: codex_plus_core::resident_helper::resident_helper_port().is_some(),
        pool,
        codex_providers,
        groups,
        agents,
        health: key_pool::snapshot(std::time::Instant::now()),
        activity: activity::snapshot(),
        claude_providers,
        grok_profiles,
    }
}

fn current_payload() -> Option<GatewayPayload> {
    let settings = SettingsStore::default().load().ok()?;
    let gateway = GatewayStore::default().load().ok()?;
    Some(build_payload(&settings, gateway))
}

fn respond(
    success: &str,
    failure: &str,
    result: anyhow::Result<GatewaySettings>,
) -> CommandResult<Option<GatewayPayload>> {
    match result {
        Ok(_) => ok(success, current_payload()),
        Err(error) => failed(&format!("{failure}：{error:#}"), current_payload()),
    }
}

#[tauri::command]
pub fn load_gateway() -> CommandResult<Option<GatewayPayload>> {
    match (
        SettingsStore::default().load(),
        GatewayStore::default().load(),
    ) {
        (Ok(settings), Ok(gateway)) => {
            ok("网关状态已加载。", Some(build_payload(&settings, gateway)))
        }
        (Err(error), _) | (_, Err(error)) => failed(&format!("读取网关状态失败：{error:#}"), None),
    }
}

/// 开启或更新一个 Agent 的网关绑定：改写它的配置指向网关，并确保常驻 helper 在跑。
#[tauri::command]
pub fn save_gateway_binding(
    agent: String,
    binding: AgentBinding,
) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let agent = parse_agent(&agent)?;
        let settings = SettingsStore::default().load()?;
        let saved = apply::enable_binding(&context(), &settings, &agent, binding)?;
        if sync_resident_helper().is_none() {
            anyhow::bail!("网关绑定已写入，但本地网关没能启动，请查看诊断日志");
        }
        Ok(saved)
    })();
    let name = codex_plus_core::tools::tool_display_name(&ToolId::parse(&agent));
    respond(
        &format!("{name} 已改为经 Codex++ 网关，新开的会话生效。"),
        "开启网关模式失败",
        result,
    )
}

/// 关闭一个 Agent 的网关绑定，恢复成直连。
#[tauri::command]
pub fn disable_gateway_binding(agent: String) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let agent = parse_agent(&agent)?;
        let settings = SettingsStore::default().load()?;
        apply::disable_binding(&context(), &settings, &agent)
    })();
    let name = codex_plus_core::tools::tool_display_name(&ToolId::parse(&agent));
    respond(
        &format!("{name} 已恢复直连，新开的会话生效。"),
        "关闭网关模式失败",
        result,
    )
}

/// 给 Agent 换一个新的本地 token（旧 token 立即失效）；开着的绑定同步重写配置。
#[tauri::command]
pub fn rotate_gateway_token(agent: String) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let agent = parse_agent(&agent)?;
        let store = GatewayStore::default();
        let mut gateway = store.load()?;
        let Some(binding) = gateway.bindings.get_mut(agent.as_str()) else {
            anyhow::bail!("还没有配置网关绑定");
        };
        binding.token = new_client_token(&agent);
        let binding = binding.clone();
        store.save(&gateway)?;
        if binding.enabled {
            let settings = SettingsStore::default().load()?;
            return apply::enable_binding(&context(), &settings, &agent, binding);
        }
        Ok(gateway)
    })();
    respond("已更换网关 token。", "更换网关 token 失败", result)
}

/// 网关全局选项：Key 轮换方式、退出时恢复直连。
#[tauri::command]
pub fn save_gateway_options(
    key_rotation: String,
    restore_on_exit: bool,
) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let store = GatewayStore::default();
        let mut gateway = store.load()?;
        gateway.key_policy.mode = match key_rotation.as_str() {
            "roundRobin" => KeyRotationMode::RoundRobin,
            _ => KeyRotationMode::Sticky,
        };
        gateway.restore_on_exit = restore_on_exit;
        store.save(&gateway)?;
        Ok(gateway)
    })();
    respond("网关选项已保存。", "保存网关选项失败", result)
}

/// 把一个 Claude 供应商导入供应商池（变成 Anthropic 协议的普通供应商，Codex 页也能看到）。
#[tauri::command]
pub fn import_claude_provider_to_gateway_pool(id: String) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let provider = claude::load(&ClaudeContext::default_paths())?
            .providers
            .into_iter()
            .find(|provider| provider.id == id)
            .ok_or_else(|| anyhow::anyhow!("Claude 供应商不存在：{id}"))?;
        let store = SettingsStore::default();
        let mut settings = store.load()?;
        let profile = apply::import_claude_provider_to_pool(&mut settings, &provider)?;
        store.save(&settings)?;
        apply::add_to_pool(&GatewayStore::default(), &settings, &[profile.id])
    })();
    let result = refresh_agents(result);
    respond("已导入供应商池。", "导入供应商池失败", result)
}

/// 把 Grok 页的一个供应商导入供应商池。
#[tauri::command]
pub fn import_grok_profile_to_gateway_pool(id: String) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let store = SettingsStore::default();
        let mut settings = store.load()?;
        let profile = apply::import_grok_profile_to_pool(&mut settings, &id)?;
        store.save(&settings)?;
        apply::add_to_pool(&GatewayStore::default(), &settings, &[profile.id])
    })();
    let result = refresh_agents(result);
    respond("已导入供应商池。", "导入供应商池失败", result)
}

/// 把已有的 Codex 供应商加入网关池。
#[tauri::command]
pub fn add_gateway_pool_providers(ids: Vec<String>) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let settings = SettingsStore::default().load()?;
        apply::add_to_pool(&GatewayStore::default(), &settings, &ids)
    })();
    let result = refresh_agents(result);
    respond("已加入供应商池。", "加入供应商池失败", result)
}

/// 把供应商移出网关池；Codex 供应商列表里的这一项保留。
#[tauri::command]
pub fn remove_gateway_pool_provider(id: String) -> CommandResult<Option<GatewayPayload>> {
    respond(
        "已移出供应商池，Codex 供应商列表里的这一项仍然保留。",
        "移出供应商池失败",
        refresh_agents(apply::remove_from_pool(&GatewayStore::default(), &id)),
    )
}

/// 打开或关掉池里的某个供应商。
#[tauri::command]
pub fn set_gateway_pool_provider_enabled(
    id: String,
    enabled: bool,
) -> CommandResult<Option<GatewayPayload>> {
    respond(
        if enabled {
            "已打开，Agent 的模型列表里会出现它的模型。"
        } else {
            "已关闭，它不再参与路由。"
        },
        "切换供应商失败",
        refresh_agents(apply::set_pool_enabled(
            &GatewayStore::default(),
            &id,
            enabled,
        )),
    )
}

/// 调整供应商池顺序。
#[tauri::command]
pub fn reorder_gateway_pool(ids: Vec<String>) -> CommandResult<Option<GatewayPayload>> {
    respond(
        "已调整顺序。",
        "调整顺序失败",
        refresh_agents(apply::reorder_pool(&GatewayStore::default(), &ids)),
    )
}

/// 用内置预设或手填的地址、Key 新建供应商：写进 Codex 供应商列表并加入网关池。
#[tauri::command]
pub fn create_gateway_pool_provider(
    name: String,
    base_url: String,
    api_key: String,
    protocol: RelayProtocol,
    model: String,
    model_list: String,
) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let store = SettingsStore::default();
        let mut settings = store.load()?;
        let profile = apply::create_pool_provider(
            &mut settings,
            apply::NewPoolProvider {
                name: &name,
                base_url: &base_url,
                api_key: &api_key,
                protocol,
                model: &model,
                model_list: &model_list,
            },
        )?;
        store.save(&settings)?;
        apply::add_to_pool(&GatewayStore::default(), &settings, &[profile.id])
    })();
    let result = refresh_agents(result);
    respond(
        &format!("已添加「{}」，可以在智能体里选它的模型了。", name.trim()),
        "添加供应商失败",
        result,
    )
}

/// 在访达 / 资源管理器里显示网关改动的 Agent 配置文件。
#[tauri::command]
pub fn reveal_gateway_agent_config(agent: String) -> CommandResult<Option<GatewayPayload>> {
    let result = (|| {
        let agent = parse_agent(&agent)?;
        let relative = agent_config_path(&agent).trim_start_matches("~/");
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("找不到用户目录"))?;
        let path = home.join(relative);
        if !path.exists() {
            anyhow::bail!("{} 还不存在", path.display());
        }
        #[cfg(target_os = "macos")]
        let status = std::process::Command::new("open").arg("-R").arg(&path).status()?;
        #[cfg(target_os = "windows")]
        let status = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .status()?;
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let status = std::process::Command::new("xdg-open")
            .arg(path.parent().unwrap_or(&path))
            .status()?;
        let _ = status;
        GatewayStore::default().load()
    })();
    respond("已在文件管理器中显示。", "打开失败", result)
}

/// 供应商池变了：开着的供应商池模式 Agent 重写配置，让它的模型列表跟上。重写失败只记日志。
fn refresh_agents(result: anyhow::Result<GatewaySettings>) -> anyhow::Result<GatewaySettings> {
    if result.is_ok()
        && let Ok(settings) = SettingsStore::default().load()
        && let Err(error) = apply::refresh_enabled(&context(), &settings)
    {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "gateway.refresh_agents_failed",
            serde_json::json!({ "message": format!("{error:#}") }),
        );
    }
    result
}

fn agent_config_path(agent: &ToolId) -> &'static str {
    match agent {
        ToolId::Claude => "~/.claude/settings.json",
        ToolId::Grok => "~/.grok/config.toml",
        _ => "",
    }
}

/// Agent CLI 版本：第一次读取时在后台线程探测并缓存，探测完成前返回 None。
fn agent_version(agent: &ToolId) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, Once, OnceLock};
    static VERSIONS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    static PROBE: Once = Once::new();
    let versions = VERSIONS.get_or_init(Mutex::default);
    PROBE.call_once(|| {
        std::thread::spawn(|| {
            for (agent, binary) in [("claude", "claude"), ("grok", "grok")] {
                if let Some(version) = probe_cli_version(binary) {
                    let mut guard = VERSIONS
                        .get_or_init(Mutex::default)
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    guard.insert(agent.to_string(), version);
                }
            }
        });
    });
    versions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(agent.as_str())
        .cloned()
}

/// 在 PATH 和常见安装目录里找 CLI，跑 `--version`（最多等 4 秒），取第一个形如 1.2.3 的片段。
fn probe_cli_version(binary: &str) -> Option<String> {
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let mut dirs_to_try: Vec<std::path::PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for extra in [
        ".local/bin",
        ".claude/local",
        ".grok/bin",
        ".npm-global/bin",
        ".bun/bin",
        ".volta/bin",
    ] {
        dirs_to_try.push(home.join(extra));
    }
    dirs_to_try.push("/opt/homebrew/bin".into());
    dirs_to_try.push("/usr/local/bin".into());
    let path = dirs_to_try
        .into_iter()
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())?;
    let mut child = std::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(4);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    parse_version(&String::from_utf8_lossy(&output.stdout))
}

fn parse_version(text: &str) -> Option<String> {
    text.split(|ch: char| ch.is_whitespace() || ch == '(' || ch == ')' || ch == ',')
        .map(|token| token.trim_start_matches('v'))
        .find(|token| {
            token.split('.').count() >= 2
                && token.split('.').all(|part| {
                    !part.is_empty()
                        && part
                            .chars()
                            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
                })
                && token.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        })
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::parse_version;

    #[test]
    fn cli_version_is_the_first_dotted_number() {
        assert_eq!(
            parse_version("2.1.310 (Claude Code)\n").as_deref(),
            Some("2.1.310")
        );
        assert_eq!(
            parse_version("grok v0.4.2-beta").as_deref(),
            Some("0.4.2-beta")
        );
        assert_eq!(parse_version("no version here"), None);
    }
}

/// 清掉某个供应商的 Key 冷却与熔断状态。
#[tauri::command]
pub fn reset_gateway_key_health(relay_id: String) -> CommandResult<Option<GatewayPayload>> {
    key_pool::reset(&relay_id);
    ok("已重置该供应商的 Key 状态。", current_payload())
}

/// 管理器启动时：有 Agent 开着网关就拉起常驻 helper，并在端口变化或退出时恢复过直连的情况下重写配置。
pub fn start_from_saved_state() {
    let Ok(gateway) = GatewayStore::default().load() else {
        return;
    };
    if !gateway.any_enabled() {
        return;
    }
    let Some(port) = sync_resident_helper() else {
        return;
    };
    let Ok(settings) = SettingsStore::default().load() else {
        return;
    };
    let ctx = ApplyContext::default_paths(port);
    let result = if gateway.restore_on_exit {
        apply::reapply_enabled(&ctx, &settings)
    } else {
        apply::reapply_if_port_changed(&ctx, &settings)
    };
    if let Err(error) = result {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "gateway.startup_reapply_failed",
            serde_json::json!({ "message": format!("{error:#}") }),
        );
    }
}

/// 管理器退出时：用户要求的话把走网关的 Agent 恢复成直连。
pub fn restore_for_exit() {
    let Ok(settings) = SettingsStore::default().load() else {
        return;
    };
    if let Err(error) = apply::restore_all_for_exit(&context(), &settings) {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "gateway.exit_restore_failed",
            serde_json::json!({ "message": format!("{error:#}") }),
        );
    }
}
