//! 上游的响应速度：每个（供应商, 模型）最近几次成功请求从发出到收到响应头的耗时，做指数平均。
//!
//! 只存在内存，重启清零；模型组「速度优先」按它排先后，界面在模型调度表里显示。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;

/// 新样本的权重：越大越跟得上最近的变化。
const ALPHA: f64 = 0.3;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpeedView {
    pub relay_id: String,
    pub model: String,
    /// 平均首包耗时（毫秒）。
    pub ms: u64,
    pub samples: u32,
}

fn table() -> &'static Mutex<HashMap<(String, String), (f64, u32)>> {
    static TABLE: OnceLock<Mutex<HashMap<(String, String), (f64, u32)>>> = OnceLock::new();
    TABLE.get_or_init(Mutex::default)
}

fn key(relay_id: &str, model: &str) -> (String, String) {
    (relay_id.to_string(), model.to_ascii_lowercase())
}

/// 记一次成功请求的首包耗时。
pub fn record(relay_id: &str, model: &str, elapsed: Duration) {
    let ms = elapsed.as_secs_f64() * 1000.0;
    let mut guard = table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let entry = guard.entry(key(relay_id, model)).or_insert((ms, 0));
    entry.0 = if entry.1 == 0 {
        ms
    } else {
        entry.0 * (1.0 - ALPHA) + ms * ALPHA
    };
    entry.1 = entry.1.saturating_add(1);
}

/// 平均首包耗时；还没测过返回 None。
pub fn latency(relay_id: &str, model: &str) -> Option<Duration> {
    let guard = table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .get(&key(relay_id, model))
        .map(|(ms, _)| Duration::from_secs_f64(ms / 1000.0))
}

pub fn snapshot() -> Vec<SpeedView> {
    let guard = table()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut views: Vec<SpeedView> = guard
        .iter()
        .map(|((relay_id, model), (ms, samples))| SpeedView {
            relay_id: relay_id.clone(),
            model: model.clone(),
            ms: ms.round() as u64,
            samples: *samples,
        })
        .collect();
    views
        .sort_by(|left, right| (&left.relay_id, &left.model).cmp(&(&right.relay_id, &right.model)));
    views
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn averages_recent_samples_and_ignores_model_case() {
        record("speed-test", "Model-A", Duration::from_millis(1000));
        assert_eq!(
            latency("speed-test", "model-a"),
            Some(Duration::from_millis(1000))
        );
        record("speed-test", "model-a", Duration::from_millis(2000));
        let ms = latency("speed-test", "model-a").unwrap().as_millis();
        assert_eq!(ms, 1300, "0.7 × 1000 + 0.3 × 2000");
        assert_eq!(latency("speed-test", "other"), None);
        let view = snapshot()
            .into_iter()
            .find(|view| view.relay_id == "speed-test")
            .unwrap();
        assert_eq!((view.ms, view.samples), (1300, 2));
    }
}
