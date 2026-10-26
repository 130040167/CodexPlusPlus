//! 网关模式的开启 / 关闭，以及网关自己的供应商池、路由组的增删改。
//!
//! - Claude Code：在 `claude-providers.json` 里放一个托管供应商（[`GATEWAY_PROVIDER_ID`]），
//!   用现有的「关键字段」切换逻辑写进 `~/.claude/settings.json`，用户的 hooks 等键不动；
//!   关闭时切回开启前的供应商。
//! - Grok：合成一个指向网关的 Responses 供应商写进 `~/.grok/config.toml`；
//!   关闭时重新应用 Grok 页里当前选中的供应商。
//! - 供应商池与路由组只存在 `gateway.json`；从直连模式导入是复制，之后互不影响。

use std::path::PathBuf;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::gateway::route::{self, binding_default_model};
use crate::gateway::store::{
    AgentBinding, GROUP_MODEL_PREFIX, GatewayGroup, GatewayProvider, GatewaySettings, GatewayStore,
    GroupMember, GroupMode, ProviderPrice, new_client_token,
};
use crate::settings::{BackendSettings, RelayApiKey, RelayMode, RelayProfile, RelayProtocol};
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

/// 开启前检查：池里有这个 Agent 能用的供应商，默认模型能路由。
fn ensure_routable(gateway: &GatewaySettings, binding: &AgentBinding) -> anyhow::Result<()> {
    if route::binding_pool_relays(gateway, binding).is_empty() {
        bail!("供应商池是空的或都已关闭：先在「供应商」里添加供应商");
    }
    let model = binding_default_model(gateway, binding);
    if model.is_empty() {
        bail!("没有可用的默认模型：请给池里的供应商配置模型");
    }
    if !route::model_is_routable(gateway, binding, &model) {
        bail!("供应商池里没有供应商提供模型 {model}");
    }
    Ok(())
}

/// 写给 Claude Code 的托管供应商。
pub fn claude_gateway_provider(
    gateway: &GatewaySettings,
    binding: &AgentBinding,
    port: u16,
) -> ClaudeProvider {
    let default_model = binding_default_model(gateway, binding);
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
    gateway: &GatewaySettings,
    binding: &AgentBinding,
    port: u16,
) -> RelayProfile {
    let default_model = binding_default_model(gateway, binding);
    let window = binding.context_window;
    let mut models = vec![default_model.clone()];
    // 池里所有能用的模型和路由组都进 Grok 的模型列表。
    for model in route::pool_models(gateway, binding)
        .into_iter()
        .chain(binding.declared_models())
    {
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
    agent: &ToolId,
    binding: &AgentBinding,
) -> anyhow::Result<Option<String>> {
    let gateway = ctx.gateway.load()?;
    match agent {
        ToolId::Claude => {
            claude::save_provider(
                &ctx.claude,
                claude_gateway_provider(&gateway, binding, ctx.port),
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
            let profile = grok_gateway_profile(&gateway, binding, ctx.port);
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
    agent: &ToolId,
    mut binding: AgentBinding,
) -> anyhow::Result<GatewaySettings> {
    let mut gateway = ctx.gateway.load()?;
    ensure_routable(&gateway, &binding)?;
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
    if let Some(snapshot) = write_agent_config(ctx, agent, &binding)?
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
pub fn reapply_if_port_changed(ctx: &ApplyContext) -> anyhow::Result<usize> {
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
        write_agent_config(ctx, &agent, &binding)?;
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

/// 供应商池或路由组变化后重写开着的 Agent 配置：Grok 的模型列表、Claude 的默认模型随池更新。
/// 不碰恢复现场；某个 Agent 写失败时继续写其它的，最后返回第一个错误。
pub fn refresh_enabled(ctx: &ApplyContext) -> anyhow::Result<usize> {
    let gateway = ctx.gateway.load()?;
    let mut count = 0;
    let mut first_error = None;
    for (agent, binding) in &gateway.bindings {
        if !binding.enabled {
            continue;
        }
        match write_agent_config(ctx, &ToolId::parse(agent), binding) {
            Ok(_) => count += 1,
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(count),
    }
}

/// 启动时把开着的绑定重新写进 Agent 配置（退出时恢复过直连，或端口变了）。
pub fn reapply_enabled(ctx: &ApplyContext) -> anyhow::Result<usize> {
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
        if let Some(snapshot) = write_agent_config(ctx, &agent, &binding)?
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

/// 把一个 Claude 供应商复制成网关供应商（Anthropic 协议）。只读 Claude 的数据，不改它。
pub fn copy_claude_provider(provider: &ClaudeProvider) -> anyhow::Result<RelayProfile> {
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
    let mut profile = build_relay(&ProviderInput {
        name: provider.name.clone(),
        base_url,
        protocol: RelayProtocol::AnthropicMessages,
        keys: vec![api_key],
        model: models.first().cloned().unwrap_or_default(),
        model_list: models.join("\n"),
        price: None,
    })?;
    if bearer {
        profile.anthropic_auth_header =
            crate::anthropic_bridge::ANTHROPIC_AUTH_HEADER_BEARER.to_string();
    }
    Ok(profile)
}

/// 把 Grok 页的一个供应商复制成网关供应商。只读 Grok 的数据，不改它。
pub fn copy_grok_profile(
    settings: &BackendSettings,
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
    build_relay(&ProviderInput {
        name: source.name.clone(),
        base_url,
        protocol: source.protocol,
        keys: vec![source.api_key.clone()],
        model: models.first().cloned().unwrap_or_default(),
        model_list: models.join("\n"),
        price: None,
    })
}

/// 可以从 Codex 复制过来的供应商：去掉聚合供应商和官方登录账号。
///
/// 官方登录的 refresh token 每用一次就换新的，刷新结果只写回 Codex 自己那一条；
/// 复制一份会让两边互相把对方的登录刷失效，所以网关里的订阅账号要单独登录。
pub fn codex_importable(settings: &BackendSettings) -> Vec<&RelayProfile> {
    settings
        .relay_profiles
        .iter()
        .filter(|relay| {
            relay.relay_mode != RelayMode::Aggregate
                && !crate::official_login::member_uses_official_login_transport(relay)
        })
        .collect()
}

/// 复制一个 Codex 供应商（地址、协议、多 Key、请求头等原样带上）。
pub fn copy_codex_provider(
    settings: &BackendSettings,
    relay_id: &str,
) -> anyhow::Result<RelayProfile> {
    let relay = settings
        .relay_profiles
        .iter()
        .find(|relay| relay.id == relay_id)
        .with_context(|| format!("Codex 供应商不存在：{relay_id}"))?;
    if relay.relay_mode == RelayMode::Aggregate {
        bail!(
            "「{}」是聚合供应商，网关的路由组请在「路由」里新建",
            relay.name
        );
    }
    if crate::official_login::member_uses_official_login_transport(relay) {
        bail!(
            "「{}」是官方登录账号，不能复制：两边共用登录会互相把对方刷失效，请在网关里单独登录",
            relay.name
        );
    }
    if relay.base_url.contains("/gw/") {
        bail!("「{}」指向网关本身，不能导入", relay.name);
    }
    Ok(relay.clone())
}

/// 新建 / 编辑网关供应商的输入。`keys` 每个一行；编辑时为空表示沿用原来的 Key。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInput {
    pub name: String,
    pub base_url: String,
    pub protocol: RelayProtocol,
    #[serde(default)]
    pub keys: Vec<String>,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub model_list: String,
    /// 每百万 token 的价格；不填就不估算费用。
    #[serde(default)]
    pub price: Option<ProviderPrice>,
}

fn clean_keys(keys: &[String]) -> Vec<String> {
    let mut cleaned: Vec<String> = Vec::new();
    for key in keys {
        let key = key.trim();
        if !key.is_empty() && !cleaned.iter().any(|known| known == key) {
            cleaned.push(key.to_string());
        }
    }
    cleaned
}

/// 按输入组装上游配置：多个 Key 写成命名 Key，参与多 Key 轮换。
fn build_relay(input: &ProviderInput) -> anyhow::Result<RelayProfile> {
    if input.base_url.contains("/gw/") {
        bail!("不能把网关自己加进供应商池");
    }
    let keys = clean_keys(&input.keys);
    let Some(first) = keys.first() else {
        bail!("API Key 为空");
    };
    let mut relay = crate::provider_import::pool_relay_profile(
        &input.name,
        &input.base_url,
        first,
        input.protocol,
        &input.model,
        &input.model_list,
        &[],
    )?;
    relay.relay_mode = RelayMode::PureApi;
    if keys.len() > 1 {
        relay.api_keys = keys
            .iter()
            .enumerate()
            .map(|(index, key)| RelayApiKey {
                id: format!("key-{}", index + 1),
                name: format!("Key {}", index + 1),
                api_key: key.clone(),
            })
            .collect();
        relay.active_api_key_id = "key-1".to_string();
    }
    crate::relay_config::normalize_relay_profile_for_storage(&mut relay)?;
    Ok(relay)
}

/// 把一份上游配置加进网关池，id 换成网关里不重复的短名。返回新供应商的 id。
pub fn add_provider(
    store: &GatewayStore,
    mut relay: RelayProfile,
    source: &str,
) -> anyhow::Result<(GatewaySettings, String)> {
    let mut gateway = store.load()?;
    relay.id = gateway.unique_slug(&relay.name, false);
    let id = relay.id.clone();
    gateway.providers.push(GatewayProvider {
        enabled: true,
        source: source.to_string(),
        price: None,
        relay,
    });
    store.save(&gateway)?;
    Ok((gateway, id))
}

/// 用预设或手填的地址、Key 新建网关供应商。
pub fn create_provider(
    store: &GatewayStore,
    input: &ProviderInput,
    source: &str,
) -> anyhow::Result<(GatewaySettings, String)> {
    let (mut gateway, id) = add_provider(store, build_relay(input)?, source)?;
    if let Some(provider) = gateway
        .providers
        .iter_mut()
        .find(|provider| provider.relay.id == id)
    {
        provider.price = clean_price(input.price.clone());
        store.save(&gateway)?;
    }
    Ok((gateway, id))
}

/// 全是 0 的价格等于没填。
fn clean_price(price: Option<ProviderPrice>) -> Option<ProviderPrice> {
    price.filter(|price| price.input > 0.0 || price.cached_input > 0.0 || price.output > 0.0)
}

/// 编辑网关供应商：id 不变（Agent、路由组里的引用都还有效）；`keys` 为空时沿用原来的 Key。
pub fn update_provider(
    store: &GatewayStore,
    id: &str,
    input: &ProviderInput,
) -> anyhow::Result<GatewaySettings> {
    let mut gateway = store.load()?;
    let Some(provider) = gateway
        .providers
        .iter_mut()
        .find(|provider| provider.relay.id == id)
    else {
        bail!("供应商不存在：{id}");
    };
    let mut input = input.clone();
    let keep_keys = clean_keys(&input.keys).is_empty();
    if keep_keys {
        input.keys = if provider.relay.api_keys.len() > 1 {
            provider
                .relay
                .api_keys
                .iter()
                .map(|key| key.api_key.clone())
                .collect()
        } else {
            vec![provider.relay.api_key.clone()]
        };
    }
    let mut relay = build_relay(&input)?;
    relay.id = id.to_string();
    // 原来的请求头、User-Agent、Anthropic 鉴权方式这些高级设置保留。
    relay.custom_headers = provider.relay.custom_headers.clone();
    relay.user_agent = provider.relay.user_agent.clone();
    relay.anthropic_auth_header = provider.relay.anthropic_auth_header.clone();
    provider.relay = relay;
    provider.price = clean_price(input.price.clone());
    store.save(&gateway)?;
    Ok(gateway)
}

/// 删除网关供应商：同时清掉路由组里指向它的成员和 Agent「挑选」里的记录。直连模式的数据不受影响。
pub fn delete_provider(store: &GatewayStore, id: &str) -> anyhow::Result<GatewaySettings> {
    let mut gateway = store.load()?;
    let before = gateway.providers.len();
    gateway.providers.retain(|provider| provider.relay.id != id);
    if gateway.providers.len() == before {
        bail!("供应商不存在：{id}");
    }
    let prefix = format!("{id}/");
    for group in &mut gateway.groups {
        group
            .members
            .retain(|member| !member.model.starts_with(&prefix));
    }
    for binding in gateway.bindings.values_mut() {
        binding.pool_exclude.retain(|excluded| excluded != id);
    }
    crate::gateway::key_pool::reset(id);
    store.save(&gateway)?;
    Ok(gateway)
}

/// 打开或关掉池里的某个供应商：关掉后不参与路由，也不出现在 Agent 的模型列表里。
pub fn set_provider_enabled(
    store: &GatewayStore,
    id: &str,
    enabled: bool,
) -> anyhow::Result<GatewaySettings> {
    let mut gateway = store.load()?;
    let Some(provider) = gateway
        .providers
        .iter_mut()
        .find(|provider| provider.relay.id == id)
    else {
        bail!("供应商不存在：{id}");
    };
    provider.enabled = enabled;
    store.save(&gateway)?;
    Ok(gateway)
}

/// 调整池里供应商的顺序（同一个模型有多家时按这个顺序尝试）。没列出的保持原顺序排在后面。
pub fn reorder_providers(store: &GatewayStore, ids: &[String]) -> anyhow::Result<GatewaySettings> {
    let mut gateway = store.load()?;
    let mut ordered: Vec<GatewayProvider> = Vec::new();
    for id in ids {
        if let Some(position) = gateway
            .providers
            .iter()
            .position(|provider| &provider.relay.id == id)
        {
            ordered.push(gateway.providers.remove(position));
        }
    }
    ordered.append(&mut gateway.providers);
    gateway.providers = ordered;
    store.save(&gateway)?;
    Ok(gateway)
}

/// 新建 / 编辑路由组的输入。`id` 为空表示新建。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupInput {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub mode: GroupMode,
    #[serde(default)]
    pub members: Vec<GroupMember>,
}

/// 保存路由组。成员去空、去重；不能直接包含自己。返回组 id。
pub fn save_group(
    store: &GatewayStore,
    input: &GroupInput,
) -> anyhow::Result<(GatewaySettings, String)> {
    let mut gateway = store.load()?;
    let name = input.name.trim();
    if name.is_empty() {
        bail!("请填写路由组名称");
    }
    let id = if input.id.trim().is_empty() {
        gateway.unique_slug(name, true)
    } else {
        input.id.trim().to_string()
    };
    let own = format!("{GROUP_MODEL_PREFIX}{id}");
    let mut members: Vec<GroupMember> = Vec::new();
    for member in &input.members {
        let model = member.model.trim();
        if model.is_empty() || model.eq_ignore_ascii_case(&own) {
            continue;
        }
        if !members.iter().any(|known| known.model == model) {
            members.push(GroupMember {
                model: model.to_string(),
                weight: member.weight.max(1),
            });
        }
    }
    if members.is_empty() {
        bail!("路由组至少要有一个成员");
    }
    let group = GatewayGroup {
        id: id.clone(),
        name: name.to_string(),
        mode: input.mode,
        members,
    };
    match gateway.groups.iter_mut().find(|known| known.id == id) {
        Some(existing) => *existing = group,
        None => gateway.groups.push(group),
    }
    store.save(&gateway)?;
    Ok((gateway, id))
}

/// 删除路由组：其它组里引用它的成员一并去掉。
pub fn delete_group(store: &GatewayStore, id: &str) -> anyhow::Result<GatewaySettings> {
    let mut gateway = store.load()?;
    let before = gateway.groups.len();
    gateway.groups.retain(|group| group.id != id);
    if gateway.groups.len() == before {
        bail!("路由组不存在：{id}");
    }
    let reference = format!("{GROUP_MODEL_PREFIX}{id}");
    for group in &mut gateway.groups {
        group.members.retain(|member| member.model != reference);
    }
    store.save(&gateway)?;
    Ok(gateway)
}

/// 绑定解析后的默认模型，UI 展示用。
pub fn resolved_default_model(gateway: &GatewaySettings, binding: &AgentBinding) -> String {
    route::binding_default_model(gateway, binding)
}
