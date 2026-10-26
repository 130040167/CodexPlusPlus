//! 网关模式开启 / 关闭对 Agent 配置文件的改写与恢复。

use codex_plus_core::gateway::apply::{
    ApplyContext, disable_binding, enable_binding, import_claude_provider_to_pool,
    note_claude_switched, reapply_if_port_changed,
};
use codex_plus_core::gateway::store::{
    AgentBinding, ClaudeRoleModels, GatewayStore, GatewayTarget, GatewayTargetKind,
};
use codex_plus_core::settings::{BackendSettings, RelayMode, RelayProfile, RelayProtocol};
use codex_plus_core::tools::ToolId;
use codex_plus_core::tools::claude::{self, ClaudeContext, ClaudeProvider, GATEWAY_PROVIDER_ID};
use serde_json::{Value, json};

struct Fixture {
    _temp: tempfile::TempDir,
    ctx: ApplyContext,
}

fn fixture(port: u16) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let ctx = ApplyContext {
        gateway: GatewayStore::at(state.join("gateway.json")),
        claude: ClaudeContext {
            store_path: state.join("claude-providers.json"),
            claude_home: temp.path().join("claude"),
            backup_root: state.join("backups"),
        },
        grok_home: temp.path().join("grok"),
        backup_root: state.join("backups"),
        port,
    };
    std::fs::create_dir_all(&ctx.claude.claude_home).unwrap();
    std::fs::create_dir_all(&ctx.grok_home).unwrap();
    Fixture { _temp: temp, ctx }
}

fn settings() -> BackendSettings {
    BackendSettings {
        relay_profiles: vec![RelayProfile {
            id: "ds".into(),
            name: "DeepSeek".into(),
            relay_mode: RelayMode::PureApi,
            protocol: RelayProtocol::ChatCompletions,
            base_url: "https://api.deepseek.test/v1".into(),
            api_key: "sk-ds".into(),
            model_list: "deepseek-v4-pro\ndeepseek-v4-flash".into(),
            ..RelayProfile::default()
        }],
        ..BackendSettings::default()
    }
}

fn binding() -> AgentBinding {
    AgentBinding {
        target: GatewayTarget {
            kind: GatewayTargetKind::Provider,
            id: "ds".into(),
        },
        model: "deepseek-v4-pro".into(),
        claude_roles: ClaudeRoleModels {
            haiku: "deepseek-v4-flash".into(),
            ..ClaudeRoleModels::default()
        },
        context_window: Some(1_000_000),
        ..AgentBinding::default()
    }
}

fn read_settings_json(ctx: &ApplyContext) -> Value {
    serde_json::from_str(
        &std::fs::read_to_string(ctx.claude.claude_home.join("settings.json")).unwrap(),
    )
    .unwrap()
}

fn seed_claude_provider(ctx: &ApplyContext) {
    claude::save_provider(
        &ctx.claude,
        ClaudeProvider {
            id: "kimi".into(),
            name: "Kimi".into(),
            website_url: String::new(),
            notes: String::new(),
            settings_config: json!({ "env": {
                "ANTHROPIC_BASE_URL": "https://api.kimi.test/anthropic",
                "ANTHROPIC_AUTH_TOKEN": "sk-kimi",
                "ANTHROPIC_MODEL": "kimi-k2.5"
            }}),
            created_at: 0,
            ccs_source_id: String::new(),
        },
    )
    .unwrap();
    claude::switch_provider(&ctx.claude, "kimi").unwrap();
}

#[test]
fn claude_enable_points_at_gateway_and_disable_restores_previous_provider() {
    let fixture = fixture(57399);
    let ctx = &fixture.ctx;
    // 用户自己的键：网关开关都不能动它。
    std::fs::write(
        ctx.claude.claude_home.join("settings.json"),
        r#"{ "permissions": { "allow": ["Bash(ls)"] }, "env": { "MY_FLAG": "1" } }"#,
    )
    .unwrap();
    seed_claude_provider(ctx);

    let gateway = enable_binding(ctx, &settings(), &ToolId::Claude, binding()).unwrap();
    let saved = gateway.binding(&ToolId::Claude).unwrap();
    assert!(saved.enabled);
    assert!(saved.token.starts_with("cpx-gw-claude-"));
    assert_eq!(saved.restore.claude_previous_provider_id, "kimi");
    assert_eq!(saved.restore.applied_port, Some(57399));

    let live = read_settings_json(ctx);
    let env = &live["env"];
    assert_eq!(
        env["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:57399/gw/claude"
    );
    assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], saved.token.as_str());
    assert_eq!(env["ANTHROPIC_MODEL"], "deepseek-v4-pro");
    assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "deepseek-v4-pro");
    assert_eq!(env["ANTHROPIC_DEFAULT_HAIKU_MODEL"], "deepseek-v4-flash");
    assert_eq!(env["CLAUDE_CODE_MAX_CONTEXT_TOKENS"], "1000000");
    assert_eq!(env["MY_FLAG"], "1");
    assert_eq!(live["permissions"]["allow"][0], "Bash(ls)");
    assert_eq!(
        claude::current_provider_id(&ctx.claude).unwrap(),
        GATEWAY_PROVIDER_ID
    );

    // 再次保存（改模型）不换 token、不丢恢复现场。
    let mut updated = binding();
    updated.model = "deepseek-v4-flash".into();
    let gateway = enable_binding(ctx, &settings(), &ToolId::Claude, updated).unwrap();
    let again = gateway.binding(&ToolId::Claude).unwrap();
    assert_eq!(again.token, saved.token);
    assert_eq!(again.restore.claude_previous_provider_id, "kimi");
    assert_eq!(
        read_settings_json(ctx)["env"]["ANTHROPIC_MODEL"],
        "deepseek-v4-flash"
    );

    let gateway = disable_binding(ctx, &settings(), &ToolId::Claude).unwrap();
    assert!(!gateway.binding(&ToolId::Claude).unwrap().enabled);
    let live = read_settings_json(ctx);
    assert_eq!(
        live["env"]["ANTHROPIC_BASE_URL"],
        "https://api.kimi.test/anthropic"
    );
    assert_eq!(live["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-kimi");
    assert!(live["env"].get("ANTHROPIC_DEFAULT_HAIKU_MODEL").is_none());
    assert!(live["env"].get("CLAUDE_CODE_MAX_CONTEXT_TOKENS").is_none());
    assert_eq!(live["env"]["MY_FLAG"], "1");
    assert_eq!(live["permissions"]["allow"][0], "Bash(ls)");
    let payload = claude::load(&ctx.claude).unwrap();
    assert_eq!(payload.current_id, "kimi");
    assert!(
        payload
            .providers
            .iter()
            .all(|provider| provider.id != GATEWAY_PROVIDER_ID)
    );
}

#[test]
fn claude_without_previous_provider_snapshots_live_config_and_restores_it() {
    let fixture = fixture(57398);
    let ctx = &fixture.ctx;
    std::fs::write(
        ctx.claude.claude_home.join("settings.json"),
        r#"{ "env": { "ANTHROPIC_BASE_URL": "https://hand.test", "ANTHROPIC_AUTH_TOKEN": "sk-hand" } }"#,
    )
    .unwrap();
    let gateway = enable_binding(ctx, &settings(), &ToolId::Claude, binding()).unwrap();
    assert!(
        !gateway
            .binding(&ToolId::Claude)
            .unwrap()
            .restore
            .claude_previous_provider_id
            .is_empty()
    );
    disable_binding(ctx, &settings(), &ToolId::Claude).unwrap();
    let live = read_settings_json(ctx);
    assert_eq!(live["env"]["ANTHROPIC_BASE_URL"], "https://hand.test");
    assert_eq!(live["env"]["ANTHROPIC_AUTH_TOKEN"], "sk-hand");
}

#[test]
fn claude_with_nothing_configured_is_cleared_on_disable() {
    let fixture = fixture(57397);
    let ctx = &fixture.ctx;
    enable_binding(ctx, &settings(), &ToolId::Claude, binding()).unwrap();
    disable_binding(ctx, &settings(), &ToolId::Claude).unwrap();
    let live = read_settings_json(ctx);
    let env = live
        .get("env")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    assert!(
        env.keys().all(|key| !key.starts_with("ANTHROPIC_")),
        "{live}"
    );
    assert_eq!(claude::current_provider_id(&ctx.claude).unwrap(), "");
}

#[test]
fn switching_claude_provider_directly_turns_binding_off() {
    let fixture = fixture(57396);
    let ctx = &fixture.ctx;
    seed_claude_provider(ctx);
    enable_binding(ctx, &settings(), &ToolId::Claude, binding()).unwrap();
    claude::switch_provider(&ctx.claude, "kimi").unwrap();
    note_claude_switched(&ctx.gateway, "kimi").unwrap();
    assert!(
        !ctx.gateway
            .load()
            .unwrap()
            .binding(&ToolId::Claude)
            .unwrap()
            .enabled
    );
}

#[test]
fn port_change_rewrites_enabled_agents() {
    let fixture = fixture(57395);
    enable_binding(&fixture.ctx, &settings(), &ToolId::Claude, binding()).unwrap();
    let mut moved = fixture.ctx.clone();
    moved.port = 58001;
    assert_eq!(reapply_if_port_changed(&moved, &settings()).unwrap(), 1);
    assert_eq!(
        read_settings_json(&moved)["env"]["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:58001/gw/claude"
    );
    assert_eq!(reapply_if_port_changed(&moved, &settings()).unwrap(), 0);
}

#[test]
fn grok_enable_writes_gateway_endpoint_and_disable_restores_profile() {
    let fixture = fixture(57394);
    let ctx = &fixture.ctx;
    let mut settings = settings();
    let grok_profile = RelayProfile {
        id: "grok-direct".into(),
        name: "直连".into(),
        relay_mode: RelayMode::PureApi,
        protocol: RelayProtocol::ChatCompletions,
        upstream_base_url: "https://api.x.test/v1".into(),
        api_key: "sk-x".into(),
        model_list: "grok-5".into(),
        ..RelayProfile::default()
    };
    let mut shard = settings.tool_config(&ToolId::Grok);
    shard.relay_profiles = vec![grok_profile];
    shard.active_relay_id = "grok-direct".into();
    settings.set_tool_config(&ToolId::Grok, shard);

    let gateway = enable_binding(ctx, &settings, &ToolId::Grok, binding()).unwrap();
    let token = gateway.binding(&ToolId::Grok).unwrap().token.clone();
    let config = std::fs::read_to_string(ctx.grok_home.join("config.toml")).unwrap();
    assert!(
        config.contains("http://127.0.0.1:57394/gw/grok/v1"),
        "{config}"
    );
    assert!(config.contains("api_backend = \"responses\""), "{config}");
    assert!(config.contains(&token), "{config}");
    assert!(config.contains("deepseek-v4-pro"), "{config}");

    disable_binding(ctx, &settings, &ToolId::Grok).unwrap();
    let config = std::fs::read_to_string(ctx.grok_home.join("config.toml")).unwrap();
    assert!(config.contains("https://api.x.test/v1"), "{config}");
    assert!(!config.contains("/gw/grok"), "{config}");
}

#[test]
fn enabling_requires_an_existing_target_and_model() {
    let fixture = fixture(57393);
    let mut missing = binding();
    missing.target.id = "nope".into();
    assert!(enable_binding(&fixture.ctx, &settings(), &ToolId::Claude, missing).is_err());
    let mut no_model = binding();
    no_model.model.clear();
    let mut bare = settings();
    bare.relay_profiles[0].model_list.clear();
    assert!(enable_binding(&fixture.ctx, &bare, &ToolId::Claude, no_model).is_err());
    assert!(fixture.ctx.gateway.load().unwrap().bindings.is_empty());
}

#[test]
fn claude_provider_imports_into_pool_as_anthropic_relay() {
    let mut settings = settings();
    let provider = ClaudeProvider {
        id: "kimi".into(),
        name: "Kimi".into(),
        website_url: String::new(),
        notes: String::new(),
        settings_config: json!({ "env": {
            "ANTHROPIC_BASE_URL": "https://api.kimi.test/anthropic",
            "ANTHROPIC_AUTH_TOKEN": "sk-kimi",
            "ANTHROPIC_MODEL": "kimi-k2.5",
            "ANTHROPIC_DEFAULT_HAIKU_MODEL": "kimi-k2.5-mini"
        }}),
        created_at: 0,
        ccs_source_id: String::new(),
    };
    let profile = import_claude_provider_to_pool(&mut settings, &provider).unwrap();
    assert_eq!(profile.protocol, RelayProtocol::AnthropicMessages);
    assert_eq!(profile.base_url, "https://api.kimi.test/anthropic");
    assert_eq!(profile.api_key, "sk-kimi");
    assert_eq!(profile.anthropic_auth_header, "authorization");
    assert_eq!(profile.model_list, "kimi-k2.5\nkimi-k2.5-mini");
    assert_eq!(
        codex_plus_core::relay_config::relay_profile_model(&profile),
        "kimi-k2.5"
    );
    assert_eq!(settings.relay_profiles.len(), 2);
}
