//! 供应商卡片上的倍率 / 余额：识别中转站类型（sub2api、new-api），用供应商自己的 Key
//! 查倍率和余额。识别结果只用于展示，失败不影响任何转发。

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings::RelayProfile;

const BILLING_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 识别结果。`platform` 为空表示不是已知的中转站（或这个供应商不识别）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayBillingProbe {
    /// `sub2api` / `newapi`
    pub platform: Option<String>,
    pub rate: Option<RelayRateInfo>,
    pub usage: Option<RelayUsageInfo>,
    /// 识别不出或请求失败的原因，仅供排查，前端不弹窗。
    pub detail: String,
}

/// 倍率。sub2api 来自 `/v1/sub2api/billing`（已含高峰倍率）；
/// new-api 来自这个 Key 最近一条消费记录里的分组倍率。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayRateInfo {
    pub endpoint: String,
    pub multiplier: f64,
    pub applied_peak_multiplier: Option<f64>,
    /// new-api：消费记录里的分组名。
    pub group: Option<String>,
}

/// 余额摘要，按计费方式分：
/// - `keyQuota`：Key 自己设了总额度或限速
/// - `subscription`：订阅分组（sub2api），`remaining` 是日/周/月限额里最紧的那个
/// - `wallet`：账户余额
/// - `unlimited`：Key 不限额度，且站点不让查账户余额（new-api）
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayUsageInfo {
    pub endpoint: String,
    pub kind: String,
    pub is_valid: bool,
    pub plan_name: Option<String>,
    pub remaining: Option<f64>,
    pub quota_limit: Option<f64>,
    pub quota_used: Option<f64>,
    pub unit: String,
    pub rate_limits: Vec<RelayRateLimitWindow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayRateLimitWindow {
    pub window: String,
    pub limit: f64,
    pub used: f64,
    pub remaining: f64,
}

/// 一次识别要用的上游地址、Key 和带代理的 client。
pub(crate) struct BillingTarget {
    pub client: reqwest::Client,
    pub base_url: String,
    pub api_key: String,
}

/// 先试 sub2api，不是再试 new-api。官方登录、聚合、免鉴权供应商不发请求。
pub async fn probe_relay_billing(profile: &RelayProfile) -> RelayBillingProbe {
    let unknown = |detail: String| RelayBillingProbe {
        platform: None,
        rate: None,
        usage: None,
        detail,
    };
    if crate::official_login::is_official_login_profile(profile)
        || profile.relay_mode == crate::settings::RelayMode::Aggregate
        || profile.uses_no_auth()
    {
        return unknown("官方登录、聚合或免鉴权供应商不识别中转站类型".to_string());
    }
    let target = match billing_target(profile) {
        Ok(target) => target,
        Err(error) => return unknown(error.to_string()),
    };
    let sub2api = crate::sub2api::probe(&target).await;
    if sub2api.platform.is_some() {
        return sub2api;
    }
    let newapi = crate::newapi::probe(&target).await;
    if newapi.platform.is_some() {
        return newapi;
    }
    unknown(format!("sub2api：{}；new-api：{}", sub2api.detail, newapi.detail))
}

fn billing_target(profile: &RelayProfile) -> anyhow::Result<BillingTarget> {
    let base_url = if profile.upstream_base_url.trim().is_empty() {
        profile.base_url.trim()
    } else {
        profile.upstream_base_url.trim()
    };
    if base_url.is_empty() {
        anyhow::bail!("Base URL 不能为空");
    }
    let api_key = crate::relay_config::relay_profile_api_key(profile);
    if api_key.trim().is_empty() {
        anyhow::bail!("API Key 不能为空");
    }
    Ok(BillingTarget {
        client: crate::http_client::proxied_client_for_relay(&profile.user_agent, profile)?,
        base_url: base_url.trim_end_matches('/').to_string(),
        api_key: api_key.trim().to_string(),
    })
}

/// GET 一个 JSON 接口；`api_key` 为空时不带鉴权（如 new-api 的公开 `/api/status`）。
pub(crate) async fn get_json(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: Option<&str>,
) -> anyhow::Result<String> {
    let mut request = client
        .get(endpoint)
        // 卡片后台识别用，上游卡住时不能让刷新一直转。
        .timeout(BILLING_REQUEST_TIMEOUT);
    if let Some(api_key) = api_key {
        request = request.bearer_auth(api_key).header("x-api-key", api_key);
    }
    let response = request
        .send()
        .await
        .with_context(|| format!("请求 {endpoint} 失败"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "HTTP {}：{}",
            status.as_u16(),
            error_message(&body).unwrap_or_else(|| "上游拒绝请求".to_string())
        );
    }
    Ok(body)
}

fn error_message(body: &str) -> Option<String> {
    let payload: Value = serde_json::from_str(body).ok()?;
    for path in [&["error", "message"][..], &["message"][..], &["error"][..]] {
        let mut current = &payload;
        for key in path {
            current = current.get(*key)?;
        }
        if let Some(message) = current
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Some(message.to_string());
        }
    }
    None
}

/// 把 `.../v1` 这类带版本号的地址退回站点根，new-api 的管理接口挂在根下。
pub(crate) fn site_root(base_url: &str) -> String {
    let cleaned = base_url.trim().trim_end_matches('/');
    if crate::protocol_proxy::has_version_suffix(cleaned) {
        if let Some((root, _)) = cleaned.rsplit_once('/') {
            return root.to_string();
        }
    }
    cleaned.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn relay_profile(base_url: String) -> RelayProfile {
        RelayProfile {
            base_url,
            api_key: "sk-test".to_string(),
            relay_mode: crate::settings::RelayMode::PureApi,
            ..RelayProfile::default()
        }
    }

    #[test]
    fn site_root_strips_version_suffix() {
        assert_eq!(site_root("https://example.com/v1/"), "https://example.com");
        assert_eq!(site_root("https://example.com"), "https://example.com");
        assert_eq!(site_root("https://example.com/openai/v1"), "https://example.com/openai");
    }

    #[tokio::test]
    async fn probe_recognizes_old_sub2api_without_billing_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/usage"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"mode":"unrestricted","isValid":true,"planName":"钱包余额","remaining":8,"unit":"USD","balance":8}"#,
            ))
            .mount(&server)
            .await;

        let probe = probe_relay_billing(&relay_profile(server.uri())).await;

        assert_eq!(probe.platform.as_deref(), Some("sub2api"));
        assert!(probe.rate.is_none());
        assert_eq!(probe.usage.unwrap().remaining, Some(8.0));
    }

    #[tokio::test]
    async fn probe_falls_back_to_newapi() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/usage/token/"))
            .and(header("authorization", "Bearer sk-test"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"code":true,"message":"ok","data":{"object":"token_usage","name":"k","total_granted":1000000,
                    "total_used":250000,"total_available":750000,"unlimited_quota":false,"expires_at":0}}"#,
            ))
            .mount(&server)
            .await;

        let probe = probe_relay_billing(&relay_profile(format!("{}/v1", server.uri()))).await;

        assert_eq!(probe.platform.as_deref(), Some("newapi"));
        let usage = probe.usage.unwrap();
        assert_eq!(usage.kind, "keyQuota");
        assert_eq!(usage.remaining, Some(1.5));
        assert_eq!(usage.quota_limit, Some(2.0));
    }

    #[tokio::test]
    async fn probe_reports_unknown_relays() {
        let server = MockServer::start().await;

        let probe = probe_relay_billing(&relay_profile(server.uri())).await;

        assert!(probe.platform.is_none());
        assert!(probe.rate.is_none() && probe.usage.is_none());
    }

    #[tokio::test]
    async fn probe_skips_profiles_without_upstream_credentials() {
        let aggregate = RelayProfile {
            relay_mode: crate::settings::RelayMode::Aggregate,
            ..relay_profile("http://127.0.0.1:9".to_string())
        };
        let missing_key = RelayProfile {
            api_key: String::new(),
            ..relay_profile("http://127.0.0.1:9".to_string())
        };

        assert!(probe_relay_billing(&aggregate).await.platform.is_none());
        assert!(probe_relay_billing(&missing_key).await.platform.is_none());
    }
}
