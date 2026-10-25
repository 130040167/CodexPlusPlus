//! Computer Use 正在操作哪个应用：CU 服务进程写，管理器读，用来弹悬浮预览。
//!
//! # 为什么要它
//!
//! Codex 的「画中画」（Remote Hosted PiP）由 CU 服务渲染、由 Codex 应用当宿主嵌进窗口，
//! 宿主只认 OpenAI 自家的 bundle id，第三方挂不进去（见 `window_capture` 顶部）。
//! Codex++ 改用自己的窗口取流画预览，但取流在管理器里，而知道「正在操作谁」的是
//! 各客户端拉起的 `--codex-plus-cu-mcp` 进程。两边用状态目录里的一个小文件通信：
//!
//! - CU 服务每次在返回结果里看到被操作的应用就写一次 `act`：离散工具档的结果带
//!   `App=… (bundleID …, pid …)`；JS 档只有 `Window: "…", App: 名字.`，记应用名，
//!   由管理器按名字找进程；
//! - 会话结束（客户端断开、JS 档回合结束）写 `end`；
//! - 管理器轮询这个文件，决定悬浮预览的显示与收起。
//!
//! 文件只放最近一次的状态，多个会话同时操作时以最后写入的为准。

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const ACTIVITY_FILE: &str = "cu/activity.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CuActivity {
    /// `act`：正在操作；`end`：这个会话结束了。
    pub event: String,
    /// 写入方（CU 服务进程）的 pid，用来区分会话。
    pub session: u32,
    /// 被操作应用的 pid；`end` 时为 0。
    #[serde(default)]
    pub pid: i32,
    #[serde(default)]
    pub bundle_id: String,
    /// 应用名（JS 档拿不到 pid 时用它定位）。
    #[serde(default)]
    pub app_name: String,
    /// 写入时间（毫秒）。
    pub at: u64,
}

pub fn activity_path() -> PathBuf {
    crate::paths::default_app_state_dir().join(ACTIVITY_FILE)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// 写一条活动。失败只忽略：预览是锦上添花，不能影响 CU 本身。
pub fn record(event: &str, pid: i32, bundle_id: &str, app_name: &str) {
    let activity = CuActivity {
        event: event.to_string(),
        session: std::process::id(),
        pid,
        bundle_id: bundle_id.to_string(),
        app_name: app_name.to_string(),
        at: now_millis(),
    };
    let path = activity_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_vec(&activity) {
        let _ = crate::settings::atomic_write(&path, &text);
    }
}

pub fn read() -> Option<CuActivity> {
    let text = std::fs::read(activity_path()).ok()?;
    serde_json::from_slice(&text).ok()
}

/// 从 CU 返回的原始帧里找最后一个 `App=… (bundleID X, pid N)`，取出 bundle id 与 pid。
///
/// 直接扫字节、不解析 JSON：帧里常带几百 KB 的截图。路径里的 `/` 可能被转义成 `\/`，
/// 所以只取 bundle id 和 pid 这两个不含转义的字段。
pub fn parse_app_line(frame: &[u8]) -> Option<(String, i32)> {
    const MARK: &[u8] = b"(bundleID ";
    let start = frame
        .windows(MARK.len())
        .rposition(|window| window == MARK)?
        + MARK.len();
    let rest = &frame[start..];
    let close = rest.iter().position(|&byte| byte == b')')?;
    let inner = std::str::from_utf8(&rest[..close]).ok()?;
    let (bundle, pid) = inner.split_once(", pid ")?;
    let pid = pid.trim().parse::<i32>().ok()?;
    let bundle = bundle.trim();
    (!bundle.is_empty() && pid > 0).then(|| (bundle.to_string(), pid))
}

/// JS 档的状态只有 `Window: "标题", App: 名字.`：取最后一个应用名。
///
/// 帧是 JSON 文本，换行在字节里是 `\n` 两个字符；名字可能被转义（`\uXXXX`），
/// 所以截出来后按 JSON 字符串再解一次。
pub fn parse_app_name(frame: &[u8]) -> Option<String> {
    const MARK: &[u8] = b"\", App: ";
    let start = frame
        .windows(MARK.len())
        .rposition(|window| window == MARK)?
        + MARK.len();
    let rest = &frame[start..];
    let end = rest.windows(3).position(|window| window == b".\\n")?;
    let raw = std::str::from_utf8(&rest[..end]).ok()?;
    let name: String = serde_json::from_str(&format!("\"{raw}\"")).ok()?;
    let name = name.trim();
    (!name.is_empty() && name.len() <= 120).then(|| name.to_string())
}

/// 当前前台应用的 pid。Codex 只给「后台被操作的应用」显示画中画，这里同样据此判断。
#[cfg(target_os = "macos")]
pub fn frontmost_pid() -> Option<i32> {
    let workspace = objc2_app_kit::NSWorkspace::sharedWorkspace();
    let app = workspace.frontmostApplication()?;
    Some(app.processIdentifier())
}

#[cfg(not(target_os = "macos"))]
pub fn frontmost_pid() -> Option<i32> {
    None
}

/// 把某个 pid 的应用切到前台（悬浮预览上的「切到该应用」）。
#[cfg(target_os = "macos")]
pub fn activate_pid(pid: i32) -> bool {
    use objc2_app_kit::{NSApplicationActivationOptions, NSRunningApplication};
    let Some(app) = NSRunningApplication::runningApplicationWithProcessIdentifier(pid) else {
        return false;
    };
    #[allow(deprecated)]
    app.activateWithOptions(NSApplicationActivationOptions::ActivateAllWindows)
}

#[cfg(not(target_os = "macos"))]
pub fn activate_pid(_pid: i32) -> bool {
    false
}

/// 把窗口浮在最上层并显示出来，**不激活**所属应用：CU 在后台操作时，
/// 用户正在用的窗口不能因为预览弹出而失焦。必须在主线程调用。
///
/// # Safety
/// `ns_window` 必须是有效的 `NSWindow` 指针（Tauri 的 `ns_window()`）。
#[cfg(target_os = "macos")]
pub unsafe fn show_floating_without_activation(ns_window: *mut std::ffi::c_void) {
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};
    if ns_window.is_null() {
        return;
    }
    // SAFETY: 调用方保证指针有效且在主线程。
    let window: &NSWindow = unsafe { &*(ns_window as *const NSWindow) };
    // NSFloatingWindowLevel = 3：在普通窗口之上，不压住系统级浮层。
    window.setLevel(3);
    window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary,
    );
    window.orderFrontRegardless();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_app_name_from_js_state() {
        // 帧里的名字被转义成 \uXXXX、换行是 `\n` 两个字符的情况。
        let backslash = char::from(92u8);
        let escaped: String = ["8ba1", "7b97", "5668"]
            .iter()
            .map(|code| format!("{backslash}u{code}"))
            .collect();
        let frame = format!(
            r#"{{"content":[{{"type":"text","text":"docs...\n\nWindow: \"{escaped}\", App: {escaped}.\n0 standard window"}}]}}"#
        );
        assert!(frame.is_ascii());
        assert_eq!(parse_app_name(frame.as_bytes()), Some("计算器".to_string()));
        // 名字是原样 UTF-8 的情况（node 的 JSON.stringify 不转义中文）。
        let utf8 = r#"{"text":"Window: \"计算器\", App: 计算器.\n0 x"}"#;
        assert_eq!(parse_app_name(utf8.as_bytes()), Some("计算器".to_string()));
        assert_eq!(parse_app_name(b"nothing"), None);
    }

    #[test]
    fn parses_last_app_line_from_raw_frame() {
        let frame = br#"{"result":{"content":[{"type":"text","text":"App=\/Applications\/Calculator.app\/ (bundleID com.apple.calculator, pid 98341)\nWindow: x"}]}}"#;
        assert_eq!(
            parse_app_line(frame),
            Some(("com.apple.calculator".to_string(), 98341))
        );
        // 多个应用时取最后一个（JS 档一次调用可能操作了多个应用）。
        let two = b"App=/a (bundleID one, pid 1) ... App=/b (bundleID two, pid 2)";
        assert_eq!(parse_app_line(two), Some(("two".to_string(), 2)));
        assert_eq!(parse_app_line(b"no app here"), None);
        assert_eq!(parse_app_line(b"(bundleID x, pid abc)"), None);
    }
}
