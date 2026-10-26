//! 网关模式的开启 / 关闭：把 Agent 的配置指向本地网关，关闭时恢复成直连。
//!
//! - Claude Code：在 `claude-providers.json` 里放一个托管供应商（[`GATEWAY_PROVIDER_ID`]），
//!   用现有的「关键字段」切换逻辑写进 `~/.claude/settings.json`，用户的 hooks 等键不动；
//!   关闭时切回开启前的供应商。
//! - Grok：合成一个指向网关的 Responses 供应商写进 `~/.grok/config.toml`；
//!   关闭时重新应用 Grok 页里当前选中的供应商。

use std::path::PathBuf;

use anyhow::{Context, bail};
use serde_json::{Map, Value, json};

use crate::gateway::route::{self, binding_default_model};
use crate::gateway::store::{
    AgentBinding, GatewaySettings, GatewayStore, GatewayTargetKind, new_client_token,
};
use crate::settings::{BackendSettings, RelayMode, RelayProfile, RelayProtocol};
use crate::tools::ToolId;
use crate::tools::claude::{self, ClaudeContext, ClaudeProvider, GATEWAY_PROVIDER_ID};

/// 开启/关闭时用到的全部路径，命令层用 [`ApplyContext::default_paths`]，测试指向临时目录。
#[derive(Debug, Clone)]
pub struct ApplyContext {
    pub gateway: GatewayStore,
    pub claude: ClaudeContext,
    pub grok_home: PathBuf,
    pub backup_root: PathBuf,
    pub port: u16,
}

impl ApplyContext {
    pub fn default_paths(port: u16) -> Self {
        Self {
            gateway: GatewayStore::default(),
            claude: ClaudeContext::default_paths(),
            grok_home: crate::grok_config::default_grok_home_dir(),
            backup_root: crate::paths::default_app_state_dir().join("backups"),
            port,
        }
    }
}

/// Agent 访问网关的根地址（不含 `/v1`）。
pub fn gateway_base_url(port: u16, agent: &ToolId) -> String {
    format!("http://127.0.0.1:{port}/gw/{}", agent.as_str())
}

fn ensure_target_exists(settings: &BackendSettings, binding: &AgentBinding) -> anyhow::Result<()> {
    match binding.target.kind {
        GatewayTargetKind::Provider => {
            let relay = settings
                .relay_profiles
                .iter()
                .find(|relay| relay.id == binding.target.id)
                .with_context(|| format!("供应商不存在：{}", binding.target.id))?;
            if relay.relay_mode == RelayMode::Aggregate {
                bail!("「{}」是聚合供应商，请选择路由组", relay.name);
            }
        }
        GatewayTargetKind::Group => {
            let group = settings
                .aggregate_relay_profiles
                .iter()
                .find(|group| group.id == binding.target.id)
                .with_context(|| format!("路由组不存在：{}", binding.target.id))?;
            if group.members.is_empty() {
                bail!("路由组「{}」没有成员", group.name);
            }
        }
    }
    Ok(())
}

/// 写给 Claude Code 的托管供应商。
pub fn claude_gateway_provider(
    settings: &BackendSettings,
    binding: &AgentBinding,
    port: u16,
) -> ClaudeProvider {
    let default_model = binding_default_model(settings, binding);
    let role = |value: &str| {
        let (slug, _) = crate::model_suffix::parse_model_suffix(value.trim());
        if slug.trim().is_empty() {
            default_model.clone()
        } else {
            slug.trim().to_string()
        }
    };
    let mut env = Map::new();
    env.insert(
        "ANTHROPIC_BASE_URL".to_string(),
        json!(gateway_base_url(port, &ToolId::Claude)),
    );
    env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), json!(binding.token));
    env.insert("ANTHROPIC_MODEL".to_string(), json!(default_model));
    let opus = role(&binding.claude_roles.opus);
    env.insert("ANTHROPIC_DEFAULT_OPUS_MODEL".to_string(), json!(opus));
    env.insert("ANTHROPIC_DEFAULT_FABLE_MODEL".to_string(), json!(opus));
    env.insert(
        "ANTHROPIC_DEFAULT_SONNET_MODEL".to_string(),
        json!(role(&binding.claude_roles.sonnet)),
    );
    env.insert(
        "ANTHROPIC_DEFAULT_HAIKU_MODEL".to_string(),
        json!(role(&binding.claude_roles.haiku)),
    );
    if !binding.claude_roles.subagent.trim().is_empty() {
        env.insert(
            "CLAUDE_CODE_SUBAGENT_MODEL".to_string(),
            json!(role(&binding.claude_roles.subagent)),
        );
    }
    let window = binding.context_window.or_else(|| {
        let (_, window) = crate::model_suffix::parse_model_suffix(binding.model.trim());
        window
    });
    if let Some(window) = window {
        env.insert(
            "CLAUDE_CODE_MAX_CONTEXT_TOKENS".to_string(),
            json!(window.to_string()),
        );
    }
    if binding.claude_enable_effort {
        env.insert("CLAUDE_CODE_ALWAYS_ENABLE_EFFORT".to_string(), json!("1"));
    }
    ClaudeProvider {
        id: GATEWAY_PROVIDER_ID.to_string(),
        name: "Codex++ 网关".to_string(),
        website_url: String::new(),
        notes: "由 Codex++ 网关模式管理，请在网关模式里修改或关闭。".to_string(),
        settings_config: json!({ "env": Value::Object(env) }),
        created_at: 0,
        ccs_source_id: String::new(),
    }
}

/// 写给 Grok 的合成供应商：Responses 协议，地址是网关，Key 是网关 token。
pub fn grok_gateway_profile(
    settings: &BackendSettings,
    binding: &AgentBinding,
    port: u16,
) -> RelayProfile {
    let default_model = binding_default_model(settings, binding);
    let window = binding.context_window;
    let mut models = vec![default_model.clone()];
    for model in binding.declared_models() {
        if !models.contains(&model) {
            models.push(model);
        }
    }
    let model_list = models
        .iter()
        .filter(|model| !model.is_empty())
        .map(|model| match window {
            Some(window) => format!("{model}[{window}]"),
            None => model.clone(),
        })
        .collect::<Vec<_>>()
        .join("\n");
    let base = format!("{}/v1", gateway_base_url(port, &ToolId::Grok));
    RelayProfile {
        id: "codex-plus-gateway".to_string(),
        name: "Codex++ 网关".to_string(),
        protocol: RelayProtocol::Responses,
        relay_mode: RelayMode::PureApi,
        base_url: base.clone(),
        upstream_base_url: base,
        api_key: binding.token.clone(),
        model_list,
        ..RelayProfile::default()
    }
}

/// 写 Agent 配置。Claude 首次切换时若把 live 配置存成了「切换前的配置」，返回它的 id。
fn write_agent_config(
    ctx: &ApplyContext,
    settings: &BackendSettings,
    agent: &ToolId,
    binding: &AgentBinding,
) -> anyhow::Result<Option<String>> {
    match agent {
        ToolId::Claude => {
            claude::save_provider(
                &ctx.claude,
                claude_gateway_provider(settings, binding, ctx.port),
            )?;
            let before = claude::load(&ctx.claude)?
                .providers
                .into_iter()
                .map(|provider| provider.id)
                .collect::<Vec<_>>();
            let payload = claude::switch_provider(&ctx.claude, GATEWAY_PROVIDER_ID)?;
            let snapshot = payload
                .providers
                .iter()
                .find(|provider| !before.contains(&provider.id))
                .map(|provider| provider.id.clone());
            return Ok(snapshot);
        }
        ToolId::Grok => {
            let profile = grok_gateway_profile(settings, binding, ctx.port);
            crate::tools::grok::apply_profile_to_grok_at(
                &ctx.grok_home,
                &profile,
                &ctx.backup_root,
            )?;
        }
        other => bail!("{} 不支持网关模式", other.as_str()),
    }
    Ok(None)
}

/// 开启（或更新）一个 Agent 的网关绑定，并把它的配置指向网关。
///
/// 已经开着时只更新绑定与配置，开启前记下的「恢复现场」保持不变。
pub fn enable_binding(
    ctx: &ApplyContext,
    settings: &BackendSettings,
    agent: &ToolId,
    mut binding: AgentBinding,
) -> anyhow::Result<GatewaySettings> {
    ensure_target_exists(settings, &binding)?;
    if binding_default_model(settings, &binding).is_empty() {
        bail!("没有可用的主模型：请填写主模型，或给目标供应商配置默认模型");
    }
    let mut gateway = ctx.gateway.load()?;
    let previous = gateway.binding(agent).cloned();
    binding.token = previous
        .as_ref()
        .map(|previous| previous.token.clone())
        .filter(|token| !token.trim().is_empty())
        .unwrap_or_else(|| new_client_token(agent));
    binding.restore = match previous.filter(|previous| previous.enabled) {
        Some(previous) => previous.restore,
        None => {
            let mut restore = crate::gateway::store::GatewayRestoreState::default();
            if *agent == ToolId::Claude {
                let current = claude::current_provider_id(&ctx.claude)?;
                if current != GATEWAY_PROVIDER_ID {
                    restore.claude_previous_provider_id = current;
                }
            }
            restore
        }
    };
    binding.enabled = true;
    binding.restore.applied_port = Some(ctx.port);
    if let Some(snapshot) = write_agent_config(ctx, settings, agent, &binding)?
        && binding.restore.claude_previous_provider_id.is_empty()
    {
        binding.restore.claude_previous_provider_id = snapshot;
    }
    gateway.bindings.insert(agent.as_str().to_string(), binding);
    ctx.gateway.save(&gateway)?;
    Ok(gateway)
}

/// 关闭一个 Agent 的网关绑定，把它的配置恢复成直连。绑定本身（目标、模型）保留，方便再次开启。
pub fn disable_binding(
    ctx: &ApplyContext,
    settings: &BackendSettings,
    agent: &ToolId,
) -> anyhow::Result<GatewaySettings> {
    let mut gateway = ctx.gateway.load()?;
    let Some(binding) = gateway.bindings.get(agent.as_str()).cloned() else {
        return Ok(gateway);
    };
    match agent {
        ToolId::Claude => restore_claude(ctx, &binding)?,
        ToolId::Grok => restore_grok(ctx, settings)?,
        _ => {}
    }
    if let Some(binding) = gateway.bindings.get_mut(agent.as_str()) {
        binding.enabled = false;
        binding.restore = Default::default();
    }
    ctx.gateway.save(&gateway)?;
    Ok(gateway)
}

fn restore_claude(ctx: &ApplyContext, binding: &AgentBinding) -> anyhow::Result<()> {
    let payload = claude::load(&ctx.claude)?;
    if payload.current_id == GATEWAY_PROVIDER_ID {
        let previous = binding.restore.claude_previous_provider_id.trim();
        let can_switch_back = !previous.is_empty()
            && payload
                .providers
                .iter()
                .any(|provider| provider.id == previous);
        if can_switch_back {
            claude::switch_provider(&ctx.claude, previous)?;
        } else {
            claude::clear_current(&ctx.claude)?;
        }
    }
    if payload
        .providers
        .iter()
        .any(|provider| provider.id == GATEWAY_PROVIDER_ID)
    {
        claude::delete_provider(&ctx.claude, GATEWAY_PROVIDER_ID)?;
    }
    Ok(())
}

fn restore_grok(ctx: &ApplyContext, settings: &BackendSettings) -> anyhow::Result<()> {
    let Some(config) = settings.tools.get(&ToolId::Grok) else {
        return Ok(());
    };
    let Some(profile) = crate::tools::grok::active_grok_profile(config) else {
        return Ok(());
    };
    if crate::tools::grok::parse_grok_model_list(&profile.model_list).is_empty() {
        return Ok(());
    }
    crate::tools::grok::apply_profile_to_grok_at(&ctx.grok_home, &profile, &ctx.backup_root)?;
    Ok(())
}

/// 用户在 Claude 供应商列表里直接切到别的供应商：网关绑定随之失效，不再尝试恢复。
pub fn note_claude_switched(store: &GatewayStore, target_id: &str) -> anyhow::Result<()> {
    if target_id == GATEWAY_PROVIDER_ID {
        return Ok(());
    }
    let mut gateway = store.load()?;
    if let Some(binding) = gateway.bindings.get_mut(ToolId::Claude.as_str())
        && binding.enabled
    {
        binding.enabled = false;
        binding.restore = Default::default();
        store.save(&gateway)?;
    }
    Ok(())
}

/// 用户在 Grok 页直接应用了别的供应商：网关绑定随之失效。
pub fn note_grok_switched(store: &GatewayStore) -> anyhow::Result<()> {
    let mut gateway = store.load()?;
    if let Some(binding) = gateway.bindings.get_mut(ToolId::Grok.as_str())
        && binding.enabled
    {
        binding.enabled = false;
        binding.restore = Default::default();
        store.save(&gateway)?;
    }
    Ok(())
}

/// 网关端口变了（例如 Windows 端口被占后换了新端口）：重写所有开着的 Agent 配置。
pub fn reapply_if_port_changed(
    ctx: &ApplyContext,
    settings: &BackendSettings,
) -> anyhow::Result<usize> {
    let mut gateway = ctx.gateway.load()?;
    let mut changed = 0;
    let agents = gateway
        .bindings
        .iter()
        .filter(|(_, binding)| binding.enabled && binding.restore.applied_port != Some(ctx.port))
        .map(|(agent, _)| ToolId::parse(agent))
        .collect::<Vec<_>>();
    for agent in agents {
        let Some(binding) = gateway.bindings.get_mut(agent.as_str()) else {
            continue;
        };
        binding.restore.applied_port = Some(ctx.port);
        let binding = binding.clone();
        write_agent_config(ctx, settings, &agent, &binding)?;
        changed += 1;
    }
    if changed > 0 {
        ctx.gateway.save(&gateway)?;
    }
    Ok(changed)
}

/// 退出 Codex++ 时恢复所有走网关的 Agent（用户打开了「退出时恢复直连」）。
/// 绑定保留为开启状态，下次启动时由 [`reapply_enabled`] 重新指回网关。
pub fn restore_all_for_exit(ctx: &ApplyContext, settings: &BackendSettings) -> anyhow::Result<()> {
    let gateway = ctx.gateway.load()?;
    if !gateway.restore_on_exit {
        return Ok(());
    }
    for (agent, binding) in &gateway.bindings {
        if !binding.enabled {
            continue;
        }
        match ToolId::parse(agent) {
            ToolId::Claude => restore_claude(ctx, binding)?,
            ToolId::Grok => restore_grok(ctx, settings)?,
            _ => {}
        }
    }
    Ok(())
}

/// 启动时把开着的绑定重新写进 Agent 配置（退出时恢复过直连，或端口变了）。
pub fn reapply_enabled(ctx: &ApplyContext, settings: &BackendSettings) -> anyhow::Result<usize> {
    let gateway = ctx.gateway.load()?;
    let mut count = 0;
    for (agent, binding) in &gateway.bindings {
        if !binding.enabled {
            continue;
        }
        let agent = ToolId::parse(agent);
        let mut binding = binding.clone();
        binding.restore.applied_port = Some(ctx.port);
        // Claude 恢复过直连时，重新开启前的「上一个供应商」要重新记录。
        if agent == ToolId::Claude {
            let current = claude::current_provider_id(&ctx.claude)?;
            if current != GATEWAY_PROVIDER_ID && !current.is_empty() {
                binding.restore.claude_previous_provider_id = current;
            }
        }
        if let Some(snapshot) = write_agent_config(ctx, settings, &agent, &binding)?
            && binding.restore.claude_previous_provider_id.is_empty()
        {
            binding.restore.claude_previous_provider_id = snapshot;
        }
        let mut latest = ctx.gateway.load()?;
        latest.bindings.insert(agent.as_str().to_string(), binding);
        ctx.gateway.save(&latest)?;
        count += 1;
    }
    Ok(count)
}

/// 把一个 Claude 供应商导入供应商池（Anthropic 协议）。返回新供应商。
pub fn import_claude_provider_to_pool(
    settings: &mut BackendSettings,
    provider: &ClaudeProvider,
) -> anyhow::Result<RelayProfile> {
    let env = provider
        .settings_config
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let text = |key: &str| {
        env.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default()
            .to_string()
    };
    let base_url = text("ANTHROPIC_BASE_URL");
    if base_url.is_empty() {
        bail!(
            "Claude 供应商「{}」没有 ANTHROPIC_BASE_URL，无法导入",
            provider.name
        );
    }
    if base_url.contains("/gw/claude") {
        bail!("「{}」是网关托管的供应商，不能导入", provider.name);
    }
    let (api_key, bearer) = match (text("ANTHROPIC_AUTH_TOKEN"), text("ANTHROPIC_API_KEY")) {
        (token, _) if !token.is_empty() => (token, true),
        (_, key) if !key.is_empty() => (key, false),
        _ => bail!("Claude 供应商「{}」没有 Key，无法导入", provider.name),
    };
    let mut models = Vec::new();
    for key in [
        "ANTHROPIC_MODEL",
        "ANTHROPIC_DEFAULT_OPUS_MODEL",
        "ANTHROPIC_DEFAULT_SONNET_MODEL",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL",
        "ANTHROPIC_DEFAULT_FABLE_MODEL",
    ] {
        let model = text(key);
        if !model.is_empty() && !models.contains(&model) {
            models.push(model);
        }
    }
    let existing = settings
        .relay_profiles
        .iter()
        .map(|relay| relay.id.clone())
        .collect::<Vec<_>>();
    let mut profile = crate::provider_import::pool_relay_profile(
        &provider.name,
        &base_url,
        &api_key,
        RelayProtocol::AnthropicMessages,
        models.first().map(String::as_str).unwrap_or_default(),
        &models.join("\n"),
        &existing,
    )?;
    if bearer {
        profile.anthropic_auth_header =
            crate::anthropic_bridge::ANTHROPIC_AUTH_HEADER_BEARER.to_string();
    }
    crate::relay_config::normalize_relay_profile_for_storage(&mut profile)?;
    settings.relay_profiles.push(profile.clone());
    Ok(profile)
}

/// 把 Grok 页的一个供应商复制进供应商池。
pub fn import_grok_profile_to_pool(
    settings: &mut BackendSettings,
    profile_id: &str,
) -> anyhow::Result<RelayProfile> {
    let source = settings
        .tools
        .get(&ToolId::Grok)
        .and_then(|config| {
            config
                .relay_profiles
                .iter()
                .find(|profile| profile.id == profile_id)
                .cloned()
        })
        .with_context(|| format!("Grok 供应商不存在：{profile_id}"))?;
    let base_url = crate::tools::grok::profile_base_url(&source);
    if base_url.contains("/gw/grok") {
        bail!("「{}」指向网关本身，不能导入", source.name);
    }
    let models = crate::tools::grok::parse_grok_model_list(&source.model_list)
        .into_iter()
        .map(|(slug, _)| slug)
        .collect::<Vec<_>>();
    let existing = settings
        .relay_profiles
        .iter()
        .map(|relay| relay.id.clone())
        .collect::<Vec<_>>();
    let mut profile = crate::provider_import::pool_relay_profile(
        &source.name,
        &base_url,
        &source.api_key,
        source.protocol,
        models.first().map(String::as_str).unwrap_or_default(),
        &source.model_list,
        &existing,
    )?;
    crate::relay_config::normalize_relay_profile_for_storage(&mut profile)?;
    settings.relay_profiles.push(profile.clone());
    Ok(profile)
}

/// 供应商池：可以被网关绑定的供应商（排除聚合供应商）。
pub fn pool_relays(settings: &BackendSettings) -> Vec<&RelayProfile> {
    settings
        .relay_profiles
        .iter()
        .filter(|relay| relay.relay_mode != RelayMode::Aggregate)
        .collect()
}

/// 绑定解析后的主模型，UI 展示用。
pub fn resolved_default_model(settings: &BackendSettings, binding: &AgentBinding) -> String {
    route::binding_default_model(settings, binding)
}
