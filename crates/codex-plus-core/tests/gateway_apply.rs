//! 网关模式开启 / 关闭对 Agent 配置文件的改写与恢复，以及网关自己的供应商池、路由组。

use codex_plus_core::gateway::apply::{
    ApplyContext, GroupInput, ProviderInput, codex_importable, copy_claude_provider,
    copy_codex_provider, create_provider, delete_group, delete_provider, disable_binding,
    enable_binding, note_claude_switched, note_codex_switched, reapply_if_port_changed,
    refresh_enabled, reorder_providers, save_group, set_provider_enabled, update_provider,
};
use codex_plus_core::gateway::store::{
    AgentBinding, ClaudeRoleModels, GatewayStore, GroupMember, GroupMode,
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
        codex_home: temp.path().join("codex"),
        settings: codex_plus_core::settings::SettingsStore::new(state.join("settings.json")),
        backup_root: state.join("backups"),
        port,
    };
    std::fs::create_dir_all(&ctx.claude.claude_home).unwrap();
    std::fs::create_dir_all(&ctx.grok_home).unwrap();
    // 网关池里放一个供应商；直连模式的 settings 与它无关。
    create_provider(&ctx.gateway, &deepseek_input(), "preset:deepseek").unwrap();
    Fixture { _temp: temp, ctx }
}

fn deepseek_input() -> ProviderInput {
    ProviderInput {
        name: "DeepSeek".into(),
        base_url: "https://api.deepseek.test/v1".into(),
        protocol: RelayProtocol::ChatCompletions,
        keys: vec!["sk-ds".into()],
        model: "deepseek-v4-pro".into(),
        model_list: "deepseek-v4-pro\ndeepseek-v4-flash".into(),
        price: None,
    }
}

/// 直连模式的设置：只用于关闭网关时恢复 Grok 原来的供应商。
fn settings() -> BackendSettings {
    BackendSettings::default()
}

fn binding() -> AgentBinding {
    AgentBinding {
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

    let gateway = enable_binding(ctx, &ToolId::Claude, binding()).unwrap();
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
    let gateway = enable_binding(ctx, &ToolId::Claude, updated).unwrap();
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
    let gateway = enable_binding(ctx, &ToolId::Claude, binding()).unwrap();
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
    enable_binding(ctx, &ToolId::Claude, binding()).unwrap();
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
    enable_binding(ctx, &ToolId::Claude, binding()).unwrap();
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
    enable_binding(&fixture.ctx, &ToolId::Claude, binding()).unwrap();
    let mut moved = fixture.ctx.clone();
    moved.port = 58001;
    assert_eq!(reapply_if_port_changed(&moved).unwrap(), 1);
    assert_eq!(
        read_settings_json(&moved)["env"]["ANTHROPIC_BASE_URL"],
        "http://127.0.0.1:58001/gw/claude"
    );
    assert_eq!(reapply_if_port_changed(&moved).unwrap(), 0);
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

    let gateway = enable_binding(ctx, &ToolId::Grok, binding()).unwrap();
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

/// Codex：走直连模式自己的切换流程，托管供应商进出列表，断开时切回原来的供应商。
#[test]
fn codex_enable_switches_to_managed_gateway_profile_and_disable_switches_back() {
    let fixture = fixture(57395);
    let ctx = &fixture.ctx;
    std::fs::create_dir_all(&ctx.codex_home).unwrap();
    let mut direct = codex_plus_core::provider_import::pool_relay_profile(
        "直连",
        "https://api.direct.test/v1",
        "sk-direct",
        RelayProtocol::Responses,
        "gpt-direct",
        "gpt-direct",
        &[],
    )
    .unwrap();
    direct.id = "direct".into();
    direct.relay_mode = RelayMode::PureApi;
    codex_plus_core::relay_config::normalize_relay_profile_for_storage(&mut direct).unwrap();
    let mut seeded = BackendSettings::default();
    seeded.relay_profiles_enabled = true;
    seeded.relay_profiles = vec![direct];
    seeded.active_relay_id = "direct".into();
    codex_plus_core::relay_switch::switch_relay_profile_in_home(
        &ctx.settings,
        &ctx.codex_home,
        seeded,
        "",
    )
    .unwrap();
    let config = || std::fs::read_to_string(ctx.codex_home.join("config.toml")).unwrap();
    assert!(config().contains("api.direct.test"), "{}", config());

    let gateway = enable_binding(ctx, &ToolId::Codex, binding()).unwrap();
    let saved = gateway.binding(&ToolId::Codex).unwrap();
    assert_eq!(saved.restore.codex_previous_relay_id, "direct");
    let settings = ctx.settings.load().unwrap();
    assert_eq!(settings.active_relay_id, "codex-plus-gateway");
    assert!(
        config().contains("http://127.0.0.1:57395/gw/codex/v1"),
        "{}",
        config()
    );
    assert!(
        settings
            .relay_profiles
            .iter()
            .any(|profile| profile.id == "direct"),
        "原来的供应商还在"
    );

    // 池变了再写一次：仍在网关上，回填的是托管供应商自己。
    refresh_enabled(ctx).unwrap();
    assert_eq!(
        ctx.settings.load().unwrap().active_relay_id,
        "codex-plus-gateway"
    );

    disable_binding(ctx, &settings, &ToolId::Codex).unwrap();
    let settings = ctx.settings.load().unwrap();
    assert_eq!(settings.active_relay_id, "direct");
    assert!(
        !settings
            .relay_profiles
            .iter()
            .any(|profile| profile.id == "codex-plus-gateway"),
        "托管供应商已删除"
    );
    assert!(config().contains("api.direct.test"), "{}", config());
    assert!(!config().contains("/gw/codex"), "{}", config());

    // 在 Codex 供应商页直接切走：绑定随之断开。
    enable_binding(ctx, &ToolId::Codex, binding()).unwrap();
    note_codex_switched(&ctx.gateway, "direct").unwrap();
    assert!(
        !ctx.gateway
            .load()
            .unwrap()
            .binding(&ToolId::Codex)
            .unwrap()
            .enabled
    );
}

#[test]
fn enabling_requires_a_pool_and_a_routable_model() {
    let fixture = fixture(57393);
    let ctx = &fixture.ctx;
    let mut unknown = binding();
    unknown.model = "not-in-pool".into();
    assert!(enable_binding(ctx, &ToolId::Claude, unknown).is_err());
    // 「挑选」把唯一的供应商排除掉 → 没有可用供应商。
    let mut excluded = binding();
    excluded.pool_exclude = vec!["deepseek".into()];
    assert!(enable_binding(ctx, &ToolId::Claude, excluded).is_err());
    assert!(ctx.gateway.load().unwrap().bindings.is_empty());
    // `<供应商 id>/<模型>` 能路由，即使模型不在列表里。
    let mut qualified = binding();
    qualified.model = "deepseek/deepseek-v4-preview".into();
    assert!(enable_binding(ctx, &ToolId::Claude, qualified).is_ok());
}

#[test]
fn providers_live_only_in_the_gateway_store() {
    let fixture = fixture(57392);
    let store = &fixture.ctx.gateway;
    // 新建：多个 Key 写成命名 Key；重名换成新短名。
    let mut input = deepseek_input();
    input.keys = vec!["sk-a".into(), " sk-b ".into(), "sk-a".into()];
    let (gateway, id) = create_provider(store, &input, "preset:deepseek").unwrap();
    assert_eq!(id, "deepseek-2");
    let relay = &gateway.provider("deepseek-2").unwrap().relay;
    assert_eq!(relay.api_keys.len(), 2);
    assert_eq!(relay.api_key, "sk-a");
    assert_eq!(
        gateway.provider("deepseek-2").unwrap().source,
        "preset:deepseek"
    );

    // 读回来地址、模型、Key 都还在。
    let loaded = store.load().unwrap();
    let relay = &loaded.provider("deepseek").unwrap().relay;
    assert_eq!(relay.base_url, "https://api.deepseek.test/v1");
    assert_eq!(relay.api_key, "sk-ds");
    assert_eq!(
        codex_plus_core::gateway::route::relay_models(relay),
        vec!["deepseek-v4-pro", "deepseek-v4-flash"]
    );

    // 编辑：Key 留空沿用原来的，id 不变。
    let mut edit = deepseek_input();
    edit.name = "DeepSeek 主力".into();
    edit.keys.clear();
    edit.model_list = "deepseek-v4-pro".into();
    let gateway = update_provider(store, "deepseek-2", &edit).unwrap();
    let relay = &gateway.provider("deepseek-2").unwrap().relay;
    assert_eq!(relay.name, "DeepSeek 主力");
    assert_eq!(relay.api_keys.len(), 2, "Key 留空时保留原来的两个 Key");

    // 排序、开关、删除。
    let gateway = reorder_providers(store, &["deepseek-2".into()]).unwrap();
    assert_eq!(gateway.providers[0].relay.id, "deepseek-2");
    let gateway = set_provider_enabled(store, "deepseek", false).unwrap();
    assert!(!gateway.provider("deepseek").unwrap().enabled);
    let (_, group) = save_group(
        store,
        &GroupInput {
            id: String::new(),
            name: "Daily coding".into(),
            mode: GroupMode::InOrder,
            members: vec![
                GroupMember {
                    model: "deepseek-2/deepseek-v4-pro".into(),
                    weight: 1,
                },
                GroupMember {
                    model: "deepseek/deepseek-v4-flash".into(),
                    weight: 1,
                },
            ],
        },
    )
    .unwrap();
    assert_eq!(group, "daily-coding");
    let gateway = delete_provider(store, "deepseek-2").unwrap();
    assert!(gateway.provider("deepseek-2").is_none());
    assert_eq!(
        gateway.group("daily-coding").unwrap().members.len(),
        1,
        "删掉供应商时路由组里指向它的成员一起去掉"
    );
    assert!(delete_provider(store, "deepseek-2").is_err());
}

#[test]
fn groups_are_saved_validated_and_unlinked_on_delete() {
    let fixture = fixture(57391);
    let store = &fixture.ctx.gateway;
    let member = |model: &str| GroupMember {
        model: model.into(),
        weight: 0,
    };
    assert!(
        save_group(
            store,
            &GroupInput {
                name: "空".into(),
                ..GroupInput::default()
            }
        )
        .is_err(),
        "至少一个成员"
    );
    let (_, fast) = save_group(
        store,
        &GroupInput {
            name: "Fast".into(),
            mode: GroupMode::Rotate,
            members: vec![
                member("deepseek-v4-flash"),
                member("group/fast"),
                member("deepseek-v4-flash"),
            ],
            ..GroupInput::default()
        },
    )
    .unwrap();
    let (gateway, coder) = save_group(
        store,
        &GroupInput {
            name: "Coder".into(),
            members: vec![member("deepseek/deepseek-v4-pro"), member("group/fast")],
            ..GroupInput::default()
        },
    )
    .unwrap();
    let saved = gateway.group(&fast).unwrap();
    assert_eq!(saved.members.len(), 1, "去掉自引用和重复成员");
    assert_eq!(saved.members[0].weight, 1, "权重至少为 1");
    assert_eq!(saved.mode, GroupMode::Rotate);
    let gateway = delete_group(store, &fast).unwrap();
    assert_eq!(
        gateway.group(&coder).unwrap().members.len(),
        1,
        "删掉的组从别的组里移除"
    );
}

#[test]
fn imports_copy_data_and_leave_the_direct_mode_untouched() {
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
    let relay = copy_claude_provider(&provider).unwrap();
    assert_eq!(relay.protocol, RelayProtocol::AnthropicMessages);
    assert_eq!(relay.base_url, "https://api.kimi.test/anthropic");
    assert_eq!(relay.api_key, "sk-kimi");
    assert_eq!(relay.anthropic_auth_header, "authorization");
    assert_eq!(relay.model_list, "kimi-k2.5\nkimi-k2.5-mini");

    // Codex 供应商：普通 API 供应商原样复制；官方登录账号和聚合供应商不能复制。
    let mut settings = BackendSettings::default();
    settings.relay_profiles = vec![
        RelayProfile {
            id: "api".into(),
            name: "API".into(),
            relay_mode: RelayMode::PureApi,
            base_url: "https://api.test/v1".into(),
            api_key: "sk-api".into(),
            ..RelayProfile::default()
        },
        RelayProfile {
            id: "agg".into(),
            name: "聚合".into(),
            relay_mode: RelayMode::Aggregate,
            ..RelayProfile::default()
        },
    ];
    let before = settings.clone();
    let copied = copy_codex_provider(&settings, "api").unwrap();
    assert_eq!(copied.api_key, "sk-api");
    assert!(copy_codex_provider(&settings, "agg").is_err());
    assert_eq!(
        codex_importable(&settings)
            .iter()
            .map(|relay| relay.id.as_str())
            .collect::<Vec<_>>(),
        vec!["api"]
    );
    assert_eq!(settings, before, "复制不改直连模式的数据");
}

#[test]
fn pool_changes_follow_into_grok_model_list() {
    let fixture = fixture(57390);
    let ctx = &fixture.ctx;
    enable_binding(ctx, &ToolId::Grok, AgentBinding::default()).unwrap();
    let config = || std::fs::read_to_string(ctx.grok_home.join("config.toml")).unwrap();
    assert!(config().contains("deepseek-v4-pro"), "{}", config());
    assert!(config().contains("deepseek-v4-flash"), "{}", config());
    assert!(!config().contains("kimi-k3"), "{}", config());

    let mut kimi = deepseek_input();
    kimi.name = "Kimi".into();
    kimi.model = "kimi-k3".into();
    kimi.model_list = "kimi-k3".into();
    create_provider(&ctx.gateway, &kimi, "").unwrap();
    assert_eq!(refresh_enabled(ctx).unwrap(), 1);
    assert!(config().contains("kimi-k3"), "{}", config());
    save_group(
        &ctx.gateway,
        &GroupInput {
            name: "Fast".into(),
            members: vec![GroupMember {
                model: "kimi-k3".into(),
                weight: 1,
            }],
            ..GroupInput::default()
        },
    )
    .unwrap();
    refresh_enabled(ctx).unwrap();
    assert!(config().contains("group/fast"), "{}", config());
    set_provider_enabled(&ctx.gateway, "kimi", false).unwrap();
    refresh_enabled(ctx).unwrap();
    assert!(!config().contains("kimi-k3"), "{}", config());
}
