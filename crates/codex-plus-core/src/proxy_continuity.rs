//! 已打开的 Codex 客户端缓存 base_url；管理器换端口后仍需接住这些入口。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const HISTORY_FILE: &str = "proxy-endpoints.json";

#[derive(Default, Serialize, Deserialize)]
struct History {
    version: u32,
    homes: Vec<HomeEndpoints>,
}

#[derive(Serialize, Deserialize)]
struct HomeEndpoints {
    codex_home: PathBuf,
    ports: BTreeSet<u16>,
}

pub(crate) struct ProxyEndpoints {
    path: PathBuf,
    home: PathBuf,
    legacy_default: bool,
}

impl ProxyEndpoints {
    pub(crate) fn current() -> Self {
        let home = crate::codex_home::default_codex_home_dir();
        let user_home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex"));
        // 旧版默认端口迁移只属于生产主实例，不能让隔离实例接走主实例流量。
        let legacy_default = crate::paths::state_dir_override().is_none()
            && std::env::var_os("CODEX_PLUS_MULTI_INSTANCE").as_deref()
                != Some(std::ffi::OsStr::new("1"))
            && user_home.is_some_and(|expected| same_home(&home, &expected));
        Self::new(crate::paths::default_app_state_dir(), home, legacy_default)
    }

    pub(crate) fn new(state_dir: PathBuf, home: PathBuf, legacy_default: bool) -> Self {
        Self {
            path: state_dir.join(HISTORY_FILE),
            home,
            legacy_default,
        }
    }

    fn history(&self) -> anyhow::Result<History> {
        let contents = match std::fs::read(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(History {
                    version: 1,
                    ..History::default()
                });
            }
            Err(error) => return Err(error.into()),
        };
        let history: History = serde_json::from_slice(&contents)?;
        anyhow::ensure!(
            history.version == 1,
            "unsupported proxy endpoint history version"
        );
        Ok(history)
    }

    pub(crate) fn ports(&self) -> anyhow::Result<BTreeSet<u16>> {
        let history = self.history()?;
        let mut ports = history
            .homes
            .iter()
            .filter(|entry| same_home(&entry.codex_home, &self.home))
            .flat_map(|entry| entry.ports.iter().copied())
            .filter(|port| *port > 0)
            .collect::<BTreeSet<_>>();
        if self.legacy_default {
            ports.insert(crate::protocol_proxy::DEFAULT_PROTOCOL_PROXY_PORT);
        }
        Ok(ports)
    }

    pub(crate) fn restore_required(&self) -> bool {
        match self.ports() {
            Ok(ports) => !ports.is_empty(),
            Err(error) => {
                log_failure("history_read", &error);
                self.legacy_default
            }
        }
    }

    pub(crate) fn remember_live_proxy(&self, settings: &crate::settings::BackendSettings) {
        if !settings.relay_profiles_enabled || !settings.active_relay_uses_protocol_proxy() {
            return;
        }
        let Ok(contents) = std::fs::read_to_string(self.home.join("config.toml")) else {
            return;
        };
        let Ok(config) = contents.parse::<toml::Value>() else {
            return;
        };
        let provider = config
            .get("model_provider")
            .and_then(toml::Value::as_str)
            .unwrap_or("openai");
        let base = if provider == "openai" {
            config.get("openai_base_url")
        } else {
            config
                .get("model_providers")
                .and_then(|providers| providers.get(provider))
                .and_then(|provider| provider.get("base_url"))
        }
        .and_then(toml::Value::as_str);
        let Some(base) = base else {
            return;
        };
        // 当 live 指向供应商本身的本地代理（例如 CC Switch）时，不能认领其端口。
        let profile = settings.active_relay_profile();
        let upstream = if profile.upstream_base_url.trim().is_empty() {
            crate::relay_config::relay_profile_base_url(&profile)
        } else {
            profile.upstream_base_url.clone()
        };
        if base.trim_end_matches('/') == upstream.trim_end_matches('/') {
            return;
        }
        let Ok(url) = reqwest::Url::parse(base) else {
            return;
        };
        if url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.path().trim_end_matches('/') == "/v1"
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && let Some(port) = url.port()
            && let Err(error) = self.remember(port)
        {
            log_failure("live_endpoint_migration", &error);
        }
    }

    pub(crate) fn remember(&self, port: u16) -> anyhow::Result<()> {
        anyhow::ensure!(port > 0, "cannot remember an unbound proxy port");
        let mut history = self.history()?;
        if let Some(entry) = history
            .homes
            .iter_mut()
            .find(|entry| same_home(&entry.codex_home, &self.home))
        {
            entry.ports.insert(port);
        } else {
            history.homes.push(HomeEndpoints {
                codex_home: self.home.clone(),
                ports: BTreeSet::from([port]),
            });
        }
        let parent = self.path.parent().context("proxy history has no parent")?;
        std::fs::create_dir_all(parent)?;
        // 由状态目录的单实例锁串行化写入；持久记录不会随 helper 关闭而删除。
        crate::settings::atomic_write(&self.path, &serde_json::to_vec_pretty(&history)?)?;
        Ok(())
    }
}

fn same_home(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

pub(crate) fn log_failure(operation: &str, error: &anyhow::Error) {
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "helper.proxy_continuity_failed",
        serde_json::json!({ "operation": operation, "message": error.to_string() }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_upgrade_migrates_live_proxy_before_rewrite_without_claiming_a_supplier_port() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let endpoints = ProxyEndpoints::new(temp.path().join("state"), home.clone(), false);
        let mut settings: crate::settings::BackendSettings =
            serde_json::from_value(serde_json::json!({
                "relayProfilesEnabled": true,
                "relayProfiles": [{
                    "id": "local", "name": "Local", "relayMode": "pureApi", "protocol": "chatCompletions",
                    "upstreamBaseUrl": "http://127.0.0.1:15721/v1"
                }],
                "activeRelayId": "local"
            }))
            .unwrap();
        for (provider, key) in [
            ("custom", "[model_providers.custom]\nbase_url"),
            ("openai", "openai_base_url"),
        ] {
            std::fs::write(
                home.join("config.toml"),
                format!("model_provider = \"{provider}\"\n{key} = \"http://127.0.0.1:15721/v1\"\n"),
            )
            .unwrap();
            endpoints.remember_live_proxy(&settings);
            assert!(endpoints.ports().unwrap().is_empty());
        }
        std::fs::write(home.join("config.toml"),
            "model_provider = \"custom\"\n[model_providers.custom]\nbase_url = \"http://127.0.0.1:41004/v1\"\n").unwrap();
        endpoints.remember_live_proxy(&settings);
        assert_eq!(endpoints.ports().unwrap(), BTreeSet::from([41004]));
        settings.relay_profiles_enabled = false;
        std::fs::write(
            home.join("config.toml"),
            "openai_base_url = \"http://127.0.0.1:41005/v1\"\n",
        )
        .unwrap();
        endpoints.remember_live_proxy(&settings);
        assert_eq!(endpoints.ports().unwrap(), BTreeSet::from([41004]));
    }

    #[test]
    fn endpoint_history_survives_restarts_and_is_scoped_to_state_and_home() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let home = temp.path().join("home");
        let first = ProxyEndpoints::new(state.clone(), home.clone(), false);
        first.remember(41001).unwrap();
        let second = ProxyEndpoints::new(state.clone(), home.clone(), false);
        second.remember(41002).unwrap();
        second.remember(41001).unwrap();
        assert_eq!(second.ports().unwrap(), BTreeSet::from([41001, 41002]));
        let other_home = ProxyEndpoints::new(state.clone(), temp.path().join("other"), false);
        assert!(!other_home.restore_required());
        other_home.remember(41003).unwrap();
        assert_eq!(first.ports().unwrap(), BTreeSet::from([41001, 41002]));
        let other_state = ProxyEndpoints::new(temp.path().join("isolated"), home, false);
        assert!(other_state.ports().unwrap().is_empty());
        let legacy = ProxyEndpoints::new(state, temp.path().join("legacy"), true);
        assert_eq!(legacy.ports().unwrap(), BTreeSet::from([57321]));
    }

    #[test]
    fn corrupt_or_newer_history_is_reported_and_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let endpoints = ProxyEndpoints::new(temp.path().into(), temp.path().join("home"), false);
        for contents in [b"invalid".as_slice(), br#"{"version":2,"homes":[]}"#] {
            std::fs::write(&endpoints.path, contents).unwrap();
            assert!(endpoints.ports().is_err());
            assert!(endpoints.remember(41001).is_err());
            assert_eq!(std::fs::read(&endpoints.path).unwrap(), contents);
        }
    }
}
