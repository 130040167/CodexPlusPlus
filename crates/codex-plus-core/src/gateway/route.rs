//! 网关的路由：Agent 绑定 → 模型名 → 候选上游（供应商 × Key）列表。
//!
//! 绑定目标是单个供应商时，候选就是它的各个 Key；是路由组时复用聚合供应商的
//! 成员、策略与按模型路由规则，但每个路由组有自己的轮转选择器，不推进 Codex 的全局选择器。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};

use crate::gateway::key_pool::{self, KeyRef};
use crate::gateway::store::{AgentBinding, GatewaySettings, GatewayTargetKind, UnknownModelPolicy};
use crate::relay_rotation::{RelayRotationSelector, RotationContext, RotationEvent};
use crate::settings::{AggregateRelayProfile, BackendSettings, RelayMode, RelayProfile};
use crate::tools::ToolId;

/// 一个可尝试的上游：供应商（`api_key` 已换成该 Key）+ 发给它的模型名。
#[derive(Debug, Clone)]
pub struct GatewayCandidate {
    pub relay: RelayProfile,
    pub key: KeyRef,
    pub upstream_model: String,
}

#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub agent: ToolId,
    /// 客户端请求的模型名。
    pub source_model: String,
    /// 按绑定改写后的模型名（路由组成员可能再各自改写）。
    pub model: String,
    pub group_id: Option<String>,
    pub conversation: Option<String>,
    pub candidates: Vec<GatewayCandidate>,
    /// 候选为空且是因为 Key 都在冷却/熔断时，最近一个恢复的等待时间。
    pub cooling: Option<Duration>,
}

/// 供应商公布的模型（默认模型 + 模型列表），去掉窗口后缀。
pub fn relay_models(relay: &RelayProfile) -> Vec<String> {
    let mut models = Vec::new();
    let default = crate::relay_config::relay_profile_model(relay);
    if !default.trim().is_empty() {
        models.push(default.trim().to_string());
    }
    for (slug, _) in crate::tools::grok::parse_grok_model_list(&relay.model_list) {
        if !models.contains(&slug) {
            models.push(slug);
        }
    }
    models
}

fn relay_default_model(relay: &RelayProfile) -> String {
    relay_models(relay).into_iter().next().unwrap_or_default()
}

/// 绑定的主模型：绑定里写了就用它，否则用目标的默认模型。
pub fn binding_default_model(settings: &BackendSettings, binding: &AgentBinding) -> String {
    let (slug, _) = crate::model_suffix::parse_model_suffix(binding.model.trim());
    if !slug.trim().is_empty() {
        return slug.trim().to_string();
    }
    match binding.target.kind {
        GatewayTargetKind::Provider => find_relay(settings, &binding.target.id)
            .map(|relay| relay_default_model(&relay))
            .unwrap_or_default(),
        GatewayTargetKind::Group => find_group(settings, &binding.target.id)
            .and_then(|group| {
                let id = if group.default_relay_id.trim().is_empty() {
                    group.members.first()?.relay_id.clone()
                } else {
                    group.default_relay_id.clone()
                };
                find_relay(settings, &id)
            })
            .map(|relay| relay_default_model(&relay))
            .unwrap_or_default(),
    }
}

/// 把客户端请求的模型名按绑定改写。
pub fn effective_model(binding: &AgentBinding, default_model: &str, requested: &str) -> String {
    let requested = requested.trim();
    if requested.is_empty() {
        return default_model.to_string();
    }
    let (requested_slug, _) = crate::model_suffix::parse_model_suffix(requested);
    let declared = binding.declared_models();
    if declared.iter().any(|model| {
        model.eq_ignore_ascii_case(&requested_slug) || model.eq_ignore_ascii_case(requested)
    }) {
        return requested_slug;
    }
    match binding.unknown_model {
        UnknownModelPolicy::MapToDefault if !default_model.is_empty() => default_model.to_string(),
        _ => requested_slug,
    }
}

fn find_relay(settings: &BackendSettings, id: &str) -> Option<RelayProfile> {
    settings
        .relay_profiles
        .iter()
        .find(|relay| relay.id == id)
        .cloned()
}

fn find_group(settings: &BackendSettings, id: &str) -> Option<AggregateRelayProfile> {
    settings
        .aggregate_relay_profiles
        .iter()
        .find(|group| group.id == id)
        .cloned()
}

fn selectors() -> &'static Mutex<HashMap<String, RelayRotationSelector>> {
    static SELECTORS: OnceLock<Mutex<HashMap<String, RelayRotationSelector>>> = OnceLock::new();
    SELECTORS.get_or_init(Mutex::default)
}

/// 路由组里命中的路由规则若指定了目标模型，返回它。
fn routed_target_model(
    group: &AggregateRelayProfile,
    relay_id: &str,
    model: &str,
) -> Option<String> {
    let mut routes = group.routes.iter().collect::<Vec<_>>();
    routes.sort_by(|left, right| right.priority.cmp(&left.priority));
    routes
        .into_iter()
        .find(|route| {
            route.relay_id == relay_id
                && !route.pattern.trim().is_empty()
                && crate::relay_rotation::match_route_pattern(&route.pattern, model)
        })
        .map(|route| route.target_model.trim().to_string())
        .filter(|model| !model.is_empty())
}

/// 路由组成员不认识这个模型时，换成成员自己的默认模型（失败切换到另一家时自动换模型）。
fn member_model(relay: &RelayProfile, model: &str) -> String {
    let published = relay_models(relay);
    if published.is_empty()
        || published
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(model))
    {
        return model.to_string();
    }
    published[0].clone()
}

/// 按绑定解析出本次请求的候选上游。
pub fn resolve_plan(
    settings: &BackendSettings,
    gateway: &GatewaySettings,
    agent: &ToolId,
    requested_model: &str,
    conversation: Option<&str>,
) -> anyhow::Result<RoutePlan> {
    let binding = gateway
        .active_binding(agent)
        .with_context(|| format!("Codex++ 网关模式没有为 {} 开启", agent.as_str()))?;
    let default_model = binding_default_model(settings, binding);
    let model = effective_model(binding, &default_model, requested_model);
    let conversation = conversation
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let (relays, group_id): (Vec<(RelayProfile, String)>, Option<String>) =
        match binding.target.kind {
            GatewayTargetKind::Provider => {
                let relay = find_relay(settings, &binding.target.id)
                    .with_context(|| format!("网关绑定的供应商不存在：{}", binding.target.id))?;
                if relay.relay_mode == RelayMode::Aggregate {
                    bail!(
                        "网关绑定的供应商「{}」是聚合供应商，请改为绑定路由组",
                        relay.name
                    );
                }
                (vec![(relay, model.clone())], None)
            }
            GatewayTargetKind::Group => {
                let group = find_group(settings, &binding.target.id)
                    .with_context(|| format!("网关绑定的路由组不存在：{}", binding.target.id))?;
                let first = {
                    let mut guard = selectors()
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let rebuild = guard
                        .get(&group.id)
                        .is_none_or(|selector| selector.aggregate() != &group);
                    if rebuild {
                        guard.insert(
                            group.id.clone(),
                            RelayRotationSelector::for_aggregate(group.clone())?,
                        );
                    }
                    guard
                        .get_mut(&group.id)
                        .expect("selector initialized")
                        .select(
                            settings,
                            RotationContext {
                                conversation_id: conversation.clone(),
                                model: Some(model.clone()),
                            },
                        )?
                };
                let mut relays = vec![first.clone()];
                relays.extend(crate::relay_rotation::fallback_relays_after_in(
                    settings, &group, &first.id,
                )?);
                let relays = relays
                    .into_iter()
                    .map(|relay| {
                        let upstream = routed_target_model(&group, &relay.id, &model)
                            .unwrap_or_else(|| member_model(&relay, &model));
                        (relay, upstream)
                    })
                    .collect();
                (relays, Some(group.id))
            }
        };

    let now = Instant::now();
    let mut candidates = Vec::new();
    let mut cooling: Option<Duration> = None;
    for (relay, upstream_model) in relays {
        let (keys, soonest) = key_pool::order_keys(
            &relay,
            gateway.key_policy.mode,
            conversation.as_deref(),
            now,
        );
        if let Some(soonest) = soonest {
            cooling = Some(cooling.map_or(soonest, |current| current.min(soonest)));
        }
        for key in keys {
            candidates.push(GatewayCandidate {
                relay: key.relay,
                key: key.key,
                upstream_model: upstream_model.clone(),
            });
        }
    }
    Ok(RoutePlan {
        agent: agent.clone(),
        source_model: requested_model.trim().to_string(),
        model,
        group_id,
        conversation,
        candidates,
        cooling,
    })
}

/// 路由组成员成功/失败后推进该组的轮转状态（Failover 策略靠它换主成员）。
pub fn record_group_event(group_id: Option<&str>, event: RotationEvent) {
    let Some(group_id) = group_id else {
        return;
    };
    let mut guard = selectors()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(selector) = guard.get_mut(group_id) {
        selector.record_event(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::store::GatewayTarget;
    use crate::settings::{AggregateRelayMember, AggregateRelayRoute, AggregateRelayStrategy};

    fn relay(id: &str, models: &str) -> RelayProfile {
        RelayProfile {
            id: id.to_string(),
            name: id.to_string(),
            relay_mode: RelayMode::PureApi,
            base_url: format!("https://{id}.test/v1"),
            api_key: format!("sk-{id}"),
            model_list: models.to_string(),
            ..RelayProfile::default()
        }
    }

    fn gateway(kind: GatewayTargetKind, id: &str, model: &str) -> GatewaySettings {
        let mut gateway = GatewaySettings::default();
        gateway.bindings.insert(
            "claude".to_string(),
            AgentBinding {
                enabled: true,
                target: GatewayTarget {
                    kind,
                    id: id.to_string(),
                },
                model: model.to_string(),
                ..AgentBinding::default()
            },
        );
        gateway
    }

    #[test]
    fn unknown_models_map_to_the_binding_default() {
        let binding = AgentBinding {
            model: "deepseek-v4-pro".to_string(),
            extra_models: vec!["deepseek-v4-flash".to_string()],
            ..AgentBinding::default()
        };
        assert_eq!(
            effective_model(&binding, "deepseek-v4-pro", "claude-haiku-4-5"),
            "deepseek-v4-pro"
        );
        assert_eq!(
            effective_model(&binding, "deepseek-v4-pro", "deepseek-v4-flash"),
            "deepseek-v4-flash"
        );
        assert_eq!(
            effective_model(&binding, "deepseek-v4-pro", ""),
            "deepseek-v4-pro"
        );
        let passthrough = AgentBinding {
            unknown_model: UnknownModelPolicy::Passthrough,
            ..binding
        };
        assert_eq!(
            effective_model(&passthrough, "deepseek-v4-pro", "claude-haiku-4-5"),
            "claude-haiku-4-5"
        );
    }

    #[test]
    fn provider_binding_yields_single_candidate_with_default_model() {
        let settings = BackendSettings {
            relay_profiles: vec![relay("route-ds", "deepseek-v4-pro\ndeepseek-v4-flash")],
            ..BackendSettings::default()
        };
        let gateway = gateway(GatewayTargetKind::Provider, "route-ds", "");
        let plan = resolve_plan(
            &settings,
            &gateway,
            &ToolId::Claude,
            "claude-sonnet-5",
            None,
        )
        .unwrap();
        assert_eq!(plan.model, "deepseek-v4-pro");
        assert_eq!(plan.candidates.len(), 1);
        assert_eq!(plan.candidates[0].relay.id, "route-ds");
        assert_eq!(plan.candidates[0].upstream_model, "deepseek-v4-pro");
    }

    #[test]
    fn disabled_binding_is_rejected() {
        let settings = BackendSettings::default();
        let mut gateway = gateway(GatewayTargetKind::Provider, "missing", "");
        gateway.bindings.get_mut("claude").unwrap().enabled = false;
        assert!(resolve_plan(&settings, &gateway, &ToolId::Claude, "x", None).is_err());
    }

    #[test]
    fn group_binding_orders_members_and_maps_models_per_member() {
        let settings = BackendSettings {
            relay_profiles: vec![
                relay("route-g-a", "deepseek-v4-pro"),
                relay("route-g-b", "glm-5.3"),
            ],
            aggregate_relay_profiles: vec![AggregateRelayProfile {
                id: "route-group".to_string(),
                name: "group".to_string(),
                session_provider: Default::default(),
                model_aggregation_enabled: false,
                strategy: AggregateRelayStrategy::Failover,
                members: vec![
                    AggregateRelayMember {
                        relay_id: "route-g-a".to_string(),
                        weight: 1,
                    },
                    AggregateRelayMember {
                        relay_id: "route-g-b".to_string(),
                        weight: 1,
                    },
                ],
                routes: Vec::new(),
                default_relay_id: String::new(),
                sync_default_member_login: false,
            }],
            ..BackendSettings::default()
        };
        let gateway = gateway(GatewayTargetKind::Group, "route-group", "deepseek-v4-pro");
        let plan = resolve_plan(
            &settings,
            &gateway,
            &ToolId::Claude,
            "deepseek-v4-pro",
            None,
        )
        .unwrap();
        let order = plan
            .candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.relay.id.as_str(),
                    candidate.upstream_model.as_str(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            order,
            vec![("route-g-a", "deepseek-v4-pro"), ("route-g-b", "glm-5.3")]
        );
        assert_eq!(plan.group_id.as_deref(), Some("route-group"));
    }

    #[test]
    fn group_route_with_target_model_rewrites_upstream_model() {
        let settings = BackendSettings {
            relay_profiles: vec![
                relay("route-t-a", "deepseek-v4-pro"),
                relay("route-t-b", "glm-5.3"),
            ],
            aggregate_relay_profiles: vec![AggregateRelayProfile {
                id: "route-target".to_string(),
                name: "group".to_string(),
                session_provider: Default::default(),
                model_aggregation_enabled: false,
                strategy: AggregateRelayStrategy::Failover,
                members: vec![
                    AggregateRelayMember {
                        relay_id: "route-t-a".to_string(),
                        weight: 1,
                    },
                    AggregateRelayMember {
                        relay_id: "route-t-b".to_string(),
                        weight: 1,
                    },
                ],
                routes: vec![AggregateRelayRoute {
                    pattern: "fast".to_string(),
                    relay_id: "route-t-b".to_string(),
                    priority: 1,
                    target_model: "glm-5.3-air".to_string(),
                }],
                default_relay_id: String::new(),
                sync_default_member_login: false,
            }],
            ..BackendSettings::default()
        };
        let mut gateway = gateway(GatewayTargetKind::Group, "route-target", "deepseek-v4-pro");
        gateway
            .bindings
            .get_mut("claude")
            .unwrap()
            .extra_models
            .push("fast".to_string());
        let plan = resolve_plan(&settings, &gateway, &ToolId::Claude, "fast", None).unwrap();
        assert_eq!(plan.candidates[0].relay.id, "route-t-b");
        assert_eq!(plan.candidates[0].upstream_model, "glm-5.3-air");
    }
}
