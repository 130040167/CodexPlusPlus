//! ZCode 增强：带调试端口启动 + 页面语音输入注入。
//!
//! 背景（实测结论，改动前先读）：
//! - ZCode 是 Electron 应用，正常启动不带 `--remote-debugging-port`，渲染层因此接不上
//!   Codex++。带参启动后 CDP 可用，页面是 `file://` 且没有 CSP。
//! - ZCode 有单实例锁：已经在跑时再执行带参启动，新进程会把参数丢掉直接退出。
//!   所以「接管」必须先把旧实例优雅退掉（SIGTERM，ZCode 自己会走 before-quit），
//!   再重新带参启动；SIGTERM 在官方包与 ad-hoc 副本上都实测可用，不需要
//!   Automation 权限，也绝不 SIGKILL。
//! - ZCode 渲染层没有麦克风 entitlement（TCC 直接拒绝），所以录音由 Codex++ 管理端
//!   负责（见 `mic.rs` / `voice_server.rs`），页面脚本只负责入口和把识别文本写进输入框。
//! - 打包版 `open -n -a <副本>` 会被 AMFI/LaunchServices 拒绝（adhoc signed not
//!   valid），所以统一直接 spawn `Contents/MacOS/ZCode`；`open <app>` 只做「唤起
//!   前台窗口」的 best-effort 补充，失败忽略。
//! - Electron 启动后会把进程标题改成 `ZCode`，macOS 的 `ps` 对这类进程只显示标题，
//!   连 spawn 时传的 `--remote-debugging-port=` 参数都看不到。所以管理端自己拉起的
//!   实例要把 (pid, debugPort, appPath) 记到 state 目录的 `zcode-session.json`，
//!   detect 时按记录回填端口；记录与当前实例对不上就忽略并删除。
//!
//! 开发/测试用环境变量 `CODEX_PLUS_ZCODE_APP_PATH`：覆盖要启动/探测的 ZCode 应用
//! 路径，用于在独立副本上做端到端验证，不影响正式用户。设置后运行态检测只认这个
//! 路径下的主进程（避免在官方 ZCode 也开着时误判/误退用户正在用的实例）。

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use codex_plus_core::bridge::{
    add_script_to_new_documents, evaluate_script, evaluate_script_return_by_value,
    send_cdp_command,
};
use codex_plus_core::cdp::{self, CdpTarget};
use codex_plus_core::diagnostic_log::append_diagnostic_log;
use codex_plus_core::settings::SettingsStore;

/// 覆盖 ZCode 应用路径的环境变量。仅用于开发/测试（独立副本），正式用户不设置。
const APP_PATH_ENV: &str = "CODEX_PLUS_ZCODE_APP_PATH";
const APP_BUNDLE_NAME: &str = "ZCode.app";
const APP_EXECUTABLE_RELATIVE: &str = "Contents/MacOS/ZCode";

/// 等 ZCode 页面 target 出现的预算。冷启动实测 2~5 秒，留足 30 秒。
const DEBUG_PORT_WAIT: Duration = Duration::from_secs(30);
/// 调试端口轮询间隔。
const DEBUG_PORT_POLL: Duration = Duration::from_millis(250);
/// 请求 ZCode 退出后的等待预算。实测 1 秒内退出，给到 10 秒兜底。
const EXIT_GRACE: Duration = Duration::from_secs(10);
/// 退出轮询间隔。
const EXIT_POLL: Duration = Duration::from_millis(100);
/// 退出后等单实例锁释放的短暂缓冲。
const EXIT_SETTLE: Duration = Duration::from_millis(300);
/// 注入后健康检查重试间隔/次数（页面可能还在加载）。
const INJECT_RETRY_INTERVAL: Duration = Duration::from_millis(200);
const INJECT_RETRY_ATTEMPTS: usize = 5;
/// 补注入守护线程 tick。只看「已运行 + 有调试端口 + 未注入」，很轻。
const SUPERVISOR_TICK: Duration = Duration::from_secs(3);

/// 关闭语音输入时尽量让页面脚本自己清理（脚本幂等，正常安装过就一定有这个函数）。
const DISPOSE_SCRIPT: &str = r#"(() => {
  try { window.__zcodePlusVoiceDispose?.(); } catch (error) {}
  try { window.__zcodePlusVoiceInstalled = false; } catch (error) {}
})()"#;

/// 给前端的只读状态。字段名/顺序与前端 `zcode_enhancement_status` 约定一致。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZcodeStatus {
    /// 是否找到可增强的 ZCode 应用（目前只有 macOS 支持）。
    pub supported: bool,
    /// 探测到的 ZCode.app 路径（环境变量覆盖时就是覆盖值，即使它不存在）。
    pub app_path: String,
    /// ZCode 版本（Info.plist 的 CFBundleShortVersionString），读不到为空串。
    pub source_version: String,
    pub running: bool,
    pub pid: Option<u32>,
    /// 主进程带 `--remote-debugging-port=` 启动时才有值。
    pub debug_port: Option<u16>,
    /// 页面里已安装语音脚本且配置（endpoint/token）与当前语音服务一致。
    pub injected: bool,
    /// 设置里的「ZCode 语音输入」开关。
    pub voice_input: bool,
    /// 本地语音服务是否已在运行。
    pub voice_server_ready: bool,
    /// 正在运行但没带调试端口：需要重启才能接管。
    pub needs_restart: bool,
    /// 全局听写是否开启（语音识别服务本身没开时，页面入口可用但识别会失败）。
    pub dictation_enabled: bool,
}

impl Default for ZcodeStatus {
    fn default() -> Self {
        Self {
            supported: false,
            app_path: String::new(),
            source_version: String::new(),
            running: false,
            pid: None,
            debug_port: None,
            injected: false,
            voice_input: true,
            voice_server_ready: false,
            needs_restart: false,
            dictation_enabled: false,
        }
    }
}

// ---------------------------------------------------------------- 进程探测

/// `ps` 里解析出来的一条 ZCode 主进程记录。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ZcodeProcess {
    pid: u32,
    /// 完整的可执行文件路径，如 `/Applications/ZCode.app/Contents/MacOS/ZCode`。
    ///
    /// 只有「带参启动」时 `ps` 的 argv[0] 才是绝对路径。用户从 Dock/Finder 正常
    /// 启动时 macOS 只把 argv[0] 设成 `ZCode`，此时路径未知（`None`）——这正是
    /// 「正在运行但没带调试端口」需要重启的典型场景。
    executable: Option<PathBuf>,
    /// 应用包路径，如 `/Applications/ZCode.app`；同上，argv0 只有名字时未知。
    app_bundle: Option<PathBuf>,
    debug_port: Option<u16>,
}

fn log_event(event: &str, detail: serde_json::Value) {
    let _ = append_diagnostic_log(event, detail);
}

/// 从 `ps -axo pid=,command=` 的输出里解析所有 ZCode 主进程。
///
/// 判据：
/// - 可执行路径必须是 `*/ZCode.app/Contents/MacOS/ZCode`（允许 `/tmp` 副本）；
/// - 排除 `--type=`：那是 Chromium 里 renderer/GPU/utility 的子进程，不是主进程；
/// - 排除 helper（`ZCode Helper.app`）等：它们的路径不匹配上面的主可执行文件。
fn parse_main_processes(ps_output: &str) -> Vec<ZcodeProcess> {
    parse_main_processes_with(ps_output, resolve_process_executable)
}

/// 与 [`parse_main_processes`] 相同的解析，但把「补全 argv0 路径」做成可注入函数，
/// 让单测不依赖本机真实 PID。
fn parse_main_processes_with(
    ps_output: &str,
    resolve_executable: impl Fn(u32) -> Option<PathBuf>,
) -> Vec<ZcodeProcess> {
    let mut processes = Vec::new();
    for line in ps_output.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        let command = line.trim_start_matches(|c: char| c.is_ascii_digit() || c.is_whitespace());
        if command.contains("--type=") {
            continue;
        }
        let argv0 = command.split_whitespace().next().unwrap_or_default();
        let (executable, app_bundle) = match main_executable_in(command) {
            Some(executable) => {
                let app_bundle = executable
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::parent)
                    .map(Path::to_path_buf);
                (Some(executable), app_bundle)
            }
            // LaunchServices 启动的官方实例：`ps` 里只有 `ZCode`，没有绝对路径。
            // 必须要求整个 argv0 就是 `ZCode`（不带 `/`）：helper 的路径里含空格，
            // split_whitespace 后的第一个 token 恰好也叫 ZCode，不能误判成主进程。
            None if argv0 == "ZCode" => match resolve_executable(pid) {
                Some(executable)
                    if executable
                        .to_string_lossy()
                        .ends_with(&format!("/{APP_BUNDLE_NAME}/{APP_EXECUTABLE_RELATIVE}")) =>
                {
                    let app_bundle = executable
                        .parent()
                        .and_then(Path::parent)
                        .and_then(Path::parent)
                        .map(Path::to_path_buf);
                    (Some(executable), app_bundle)
                }
                // 能解析出可执行文件但不是 ZCode：只是同名进程，不能当成 ZCode 处理
                // （否则「退出旧实例」会误杀无关进程）。
                Some(_) => continue,
                // 解析不出来：保留「路径未知但主进程在跑」的保守状态，退出前会再核对。
                None => (None, None),
            },
            None => continue,
        };
        processes.push(ZcodeProcess {
            pid,
            executable,
            app_bundle,
            debug_port: parse_debug_port(command),
        });
    }
    processes
}

/// 在命令行里找 `*/ZCode.app/Contents/MacOS/ZCode`，返回完整可执行文件路径。
///
/// 主进程的 argv[0] 一定是命令行开头的绝对路径（LaunchServices / 我们的 spawn
/// 都这么启动），所以从行首截到 needle 末尾即可；这样副本路径里带空格也能解析。
fn main_executable_in(command: &str) -> Option<PathBuf> {
    let needle = format!("/{APP_BUNDLE_NAME}/{APP_EXECUTABLE_RELATIVE}");
    let index = command.find(&needle)?;
    let candidate = command.get(..index + needle.len())?;
    // argv[0] 必须是绝对路径；出现「空格 + /」说明 needle 命中的是后面的参数
    // （例如 `/usr/bin/open /Applications/ZCode.app/...`），不是可执行文件本身。
    if !candidate.starts_with('/') || candidate.contains(" /") {
        return None;
    }
    Some(PathBuf::from(candidate))
}

/// 解析 `--remote-debugging-port=N`，兼容 `--remote-debugging-port N` 写法。
fn parse_debug_port(command: &str) -> Option<u16> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut index = 0;
    while index < tokens.len() {
        let token = tokens[index];
        if let Some(value) = token.strip_prefix("--remote-debugging-port=") {
            return value.parse::<u16>().ok().filter(|port| *port > 0);
        }
        if token == "--remote-debugging-port" {
            return tokens
                .get(index + 1)
                .and_then(|value| value.parse::<u16>().ok())
                .filter(|port| *port > 0);
        }
        index += 1;
    }
    None
}

/// 从主进程候选里挑出「当前要操作的那个实例」。
///
/// 设置 `CODEX_PLUS_ZCODE_APP_PATH`（副本/测试）时只认该路径下的进程：官方 ZCode
/// 可能同时在跑，绝不能把它当成目标（后续退出/注入都会误伤用户正在用的实例）。
/// 没设置时优先匹配解析出的官方路径，匹配不到才退回「任意一个 ZCode 主进程」——
/// 用户从 Finder 拖出来的副本也仍能接管。
fn select_main_process<'a>(
    processes: &'a [ZcodeProcess],
    expected_app: Option<&Path>,
    allow_any: bool,
) -> Option<&'a ZcodeProcess> {
    if let Some(expected) = expected_app {
        let expected = normalize_path_text(expected);
        if let Some(process) = processes.iter().find(|process| {
            process
                .app_bundle
                .as_ref()
                .is_some_and(|bundle| normalize_path_text(bundle) == expected)
        }) {
            return Some(process);
        }
    }
    if !allow_any {
        return None;
    }
    // 路径未知的实例（LaunchServices 启动）很可能是用户点开的官方 App；
    // 没有环境变量覆盖时也允许接管它，但优先选路径已知的实例。
    processes
        .iter()
        .find(|process| process.app_bundle.is_some())
        .or_else(|| processes.first())
}

fn normalize_path_text(path: &Path) -> String {
    std::fs::canonicalize(path)
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string_lossy().trim_end_matches('/').to_string())
}

#[cfg(target_os = "macos")]
fn read_process_table() -> Option<String> {
    let output = Command::new("/bin/ps")
        .args(["-axo", "pid=,command="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(not(target_os = "macos"))]
fn read_process_table() -> Option<String> {
    None
}

/// 把「`ps` 里 argv0 只有 `ZCode`」的进程补全成可执行文件绝对路径。
///
/// macOS 上从 Dock/Finder 启动的 App，`ps` 的 argv0 只有进程名，拿不到 bundle
/// 路径。`proc_pidpath` 是 libSystem 的公开 API（libproc.h），不需要 root 或
/// Automation 权限，也不额外起子进程；拿不到时返回 None，调用方保留「路径未知」
/// 的保守处理。
#[cfg(target_os = "macos")]
fn resolve_process_executable(pid: u32) -> Option<PathBuf> {
    const PROC_PIDPATHINFO_MAXSIZE: u32 = 4096;
    let Ok(pid) = i32::try_from(pid) else {
        return None;
    };
    let mut buffer = vec![0_u8; PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: buffer 长度与传入的 buffersize 一致，proc_pidpath 只会写入最多该长度。
    let length = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<std::os::raw::c_void>(),
            PROC_PIDPATHINFO_MAXSIZE,
        )
    };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    let path = String::from_utf8_lossy(&buffer).to_string();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    /// `int proc_pidpath(int pid, void *buffer, uint32_t buffersize);`
    fn proc_pidpath(pid: i32, buffer: *mut std::os::raw::c_void, buffersize: u32) -> i32;
}

#[cfg(not(target_os = "macos"))]
fn resolve_process_executable(_pid: u32) -> Option<PathBuf> {
    None
}

/// PID 是否还活着。只用 `ps -p`，不向进程发任何信号。
fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    {
        let Ok(output) = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "pid="])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        output.status.success() && !String::from_utf8_lossy(&output.stdout).trim().is_empty()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        false
    }
}

// ---------------------------------------------------------------- 应用路径

/// 应用路径覆盖值（环境变量）。空串/纯空白按未设置处理。
fn app_path_override() -> Option<String> {
    std::env::var(APP_PATH_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_app_candidates(home_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("/Applications").join(APP_BUNDLE_NAME)];
    if let Some(home) = home_dir {
        candidates.push(home.join("Applications").join(APP_BUNDLE_NAME));
    }
    candidates
}

/// 选择要操作的应用路径：环境变量优先；否则取第一个存在的默认候选。
fn choose_app_path(override_value: Option<&str>, candidates: &[PathBuf]) -> Option<PathBuf> {
    if let Some(value) = override_value.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(value));
    }
    candidates
        .iter()
        .find(|candidate| candidate.is_dir())
        .cloned()
}

fn resolve_app_path() -> Option<PathBuf> {
    let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
    let candidates = default_app_candidates(home.as_deref());
    choose_app_path(app_path_override().as_deref(), &candidates)
}

fn app_executable(app: &Path) -> PathBuf {
    app.join(APP_EXECUTABLE_RELATIVE)
}

/// ZCode version：Info.plist 的 CFBundleShortVersionString。读不到返回空串。
fn read_app_version(app: &Path) -> String {
    let plist_path = app.join("Contents").join("Info.plist");
    let Ok(bytes) = std::fs::read(&plist_path) else {
        return String::new();
    };
    if bytes.starts_with(b"bplist") {
        #[cfg(target_os = "macos")]
        {
            return plutil_version(&plist_path).unwrap_or_default();
        }
        #[cfg(not(target_os = "macos"))]
        {
            return String::new();
        }
    }
    let Ok(text) = String::from_utf8(bytes) else {
        return String::new();
    };
    parse_plist_version(&text).unwrap_or_default()
}

/// 从 XML plist 文本里取 CFBundleShortVersionString。纯函数，便于单测。
fn parse_plist_version(text: &str) -> Option<String> {
    const KEY: &str = "<key>CFBundleShortVersionString</key>";
    let key_index = text.find(KEY)?;
    let rest = text.get(key_index + KEY.len()..)?;
    let start = rest.find("<string>")? + "<string>".len();
    let end = rest.get(start..)?.find("</string>")? + start;
    let value = rest.get(start..end)?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[cfg(target_os = "macos")]
fn plutil_version(plist: &Path) -> Option<String> {
    let output = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
        .arg(plist)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

// ---------------------------------------------------------------- 本管理端拉起实例的会话记录

/// 管理端自己拉起 ZCode 时记下的会话信息。
///
/// 为什么需要它：Electron 启动后会把进程标题改成 `ZCode`，macOS 的 `ps` 对这类
/// 进程只显示标题，**连我们 spawn 时传的 `--remote-debugging-port=` 参数都看不到**。
/// 于是「已拉起、带端口」的实例靠 ps 永远解析不出端口，只能由管理端自己记一份。
/// 记录只是提示：pid 不存活、appPath 对不上、或读不出来就忽略并删掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ZcodeSessionRecord {
    pid: u32,
    debug_port: u16,
    app_path: String,
}

fn session_record_path() -> PathBuf {
    codex_plus_core::paths::default_app_state_dir().join("zcode-session.json")
}

fn read_session_record() -> Option<ZcodeSessionRecord> {
    let path = session_record_path();
    let bytes = std::fs::read(&path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(record) => Some(record),
        Err(_) => {
            // 损坏的记录没有保留价值，删掉免得以后一直被读到。
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

/// 写入会话记录；失败只写诊断日志（记录只是提示信息，不能影响启动主流程）。
fn write_session_record(record: &ZcodeSessionRecord) {
    let path = session_record_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let bytes = match serde_json::to_vec(record) {
        Ok(bytes) => bytes,
        Err(error) => {
            log_event(
                "zcode.session_record_serialize_failed",
                json!({ "message": error.to_string() }),
            );
            return;
        }
    };
    if let Err(error) = codex_plus_core::settings::atomic_write(&path, &bytes) {
        log_event(
            "zcode.session_record_write_failed",
            json!({ "message": error.to_string() }),
        );
    }
}

fn clear_session_record() {
    let path = session_record_path();
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            log_event(
                "zcode.session_record_remove_failed",
                json!({ "message": error.to_string() }),
            );
        }
    }
}

/// 记录是否仍然指向「当前这个运行实例」。
///
/// 条件：pid 与检测到的实例一致、进程还活着、记录的 appPath 与当前期望的 ZCode.app
/// 是同一路径（canonicalize 后比较，兼容副本/软链）。
fn session_record_matches(
    record: &ZcodeSessionRecord,
    running_pid: Option<u32>,
    expected_app: Option<&Path>,
    process_alive: impl Fn(u32) -> bool,
) -> bool {
    running_pid == Some(record.pid)
        && process_alive(record.pid)
        && expected_app.is_some_and(|app| {
            normalize_path_text(app) == normalize_path_text(Path::new(&record.app_path))
        })
}

/// 记录被采纳时回填 debugPort（ps 里已有端口就不覆盖）。
fn fill_debug_port_from_session_record(
    status: &mut ZcodeStatus,
    record: Option<&ZcodeSessionRecord>,
    expected_app: Option<&Path>,
    process_alive: impl Fn(u32) -> bool,
) -> bool {
    let Some(record) = record else {
        return false;
    };
    if !session_record_matches(record, status.pid, expected_app, process_alive) {
        return false;
    }
    if status.debug_port.is_none() {
        status.debug_port = Some(record.debug_port);
    }
    true
}

/// 记录是否已经可以安全删除。
///
/// 不能「没采纳就删」：管理端拉起 ZCode 后进程标题很快变成 ZCode，而守护线程的
/// `ps` 快照可能恰好拍在 spawn 前后，出现「进程记录刚写、本轮快照还没看到」的
/// 加载窗口期。那种情况下记录是对的，只是这轮用不上；误删会让之后所有 detect 都
/// 丢掉端口（正是这次要修的 bug）。所以只在两种确定过期的情况下删除：
/// - 记录的 appPath 与当前期望的 ZCode.app 不一致（换了副本/路径）；
/// - 记录里的 PID 已经不存在（实例已退出，PID 可能被复用）。
fn should_discard_session_record(
    record: &ZcodeSessionRecord,
    expected_app: Option<&Path>,
    pid_alive: impl Fn(u32) -> bool,
) -> bool {
    if let Some(app) = expected_app
        && normalize_path_text(app) != normalize_path_text(Path::new(&record.app_path))
    {
        return true;
    }
    !pid_alive(record.pid)
}

/// needs_restart 的统一计算：运行中且（ps/记录都）不知道调试端口才需要重启。
fn finalize_status(status: &mut ZcodeStatus) {
    status.needs_restart = status.running && status.debug_port.is_none();
}

// ---------------------------------------------------------------- 状态

fn detect_with_processes(app_path: Option<&Path>, process_table: Option<&str>) -> ZcodeStatus {
    let settings = SettingsStore::default().load().ok();
    let supported = cfg!(target_os = "macos") && app_path.is_some_and(|app| app.is_dir());
    let processes = process_table
        .map(parse_main_processes)
        .unwrap_or_default();
    // 环境变量覆盖（副本/测试）时必须按路径精确匹配；默认路径下允许退回任意实例。
    let allow_any = app_path_override().is_none();
    let process = supported
        .then(|| select_main_process(&processes, app_path, allow_any))
        .flatten();
    ZcodeStatus {
        supported,
        app_path: app_path
            .map(|app| app.to_string_lossy().to_string())
            .unwrap_or_default(),
        source_version: app_path.map(read_app_version).unwrap_or_default(),
        running: process.is_some(),
        pid: process.map(|process| process.pid),
        debug_port: process.and_then(|process| process.debug_port),
        injected: false,
        voice_input: settings
            .as_ref()
            .map(|settings| settings.zcode_app_voice_input)
            .unwrap_or(true),
        voice_server_ready: crate::voice_server::current().is_some(),
        needs_restart: false,
        dictation_enabled: settings
            .as_ref()
            .map(|settings| settings.dictation.enabled)
            .unwrap_or(false),
    }
}

/// 同步探测：应用是否存在、是否运行、是否带调试端口。不连 CDP。
pub fn detect() -> ZcodeStatus {
    let app_path = resolve_app_path();
    let process_table = read_process_table();
    let mut status = detect_with_processes(app_path.as_deref(), process_table.as_deref());
    // Electron 会把进程标题改成 ZCode，ps 里看不到调试端口；用本管理端拉起的
    // 会话记录回填。记录与当前实例对不上（pid 换代 / 路径不同）就删掉。
    let record = read_session_record();
    let adopted = fill_debug_port_from_session_record(
        &mut status,
        record.as_ref(),
        app_path.as_deref(),
        process_alive,
    );
    if let Some(record) = record.as_ref()
        && !adopted
        && should_discard_session_record(record, app_path.as_deref(), process_alive)
    {
        clear_session_record();
    }
    // 「正在跑但哪都拿不到调试端口」= 需要重启才能接管。
    finalize_status(&mut status);
    status
}

async fn detect_off_thread() -> ZcodeStatus {
    match tauri::async_runtime::spawn_blocking(detect).await {
        Ok(status) => status,
        Err(_) => detect(),
    }
}

/// 探测 + CDP 健康检查。CDP 失败不报错：状态里 injected=false 即可。
pub async fn probe() -> ZcodeStatus {
    let mut status = detect_off_thread().await;
    if status.running
        && let Some(port) = status.debug_port
    {
        status.injected = read_injection_health(port).await.unwrap_or(false);
    }
    status
}

// ---------------------------------------------------------------- 纯函数：命令行/脚本

fn launch_args(port: u16) -> Vec<String> {
    vec![
        format!("--remote-debugging-port={port}"),
        format!("--remote-allow-origins=http://127.0.0.1:{port}"),
    ]
}

fn pick_free_loopback_port() -> anyhow::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("无法分配本地调试端口")?;
    let port = listener
        .local_addr()
        .context("无法读取本地调试端口")?
        .port();
    drop(listener);
    ensure!(port > 0, "无法分配本地调试端口");
    Ok(port)
}

/// 页面脚本 = 先写配置（endpoint/token），再接幂等安装脚本。
///
/// 配置必须用 JSON 转义（而不是 Rust 的 `{:?}`）：URL/令牌里一旦出现非 ASCII 或
/// 反斜杠，Rust 调试格式会生成 `\u{...}` 这类 JS 不接受的转义。
fn build_page_script(endpoint: &str, token: &str, page_source: &str) -> String {
    let endpoint = serde_json::to_string(endpoint).unwrap_or_else(|_| "\"\"".to_string());
    let token = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "window.__zcodePlusVoiceConfig = {{ endpoint: {endpoint}, token: {token} }};\n{page_source}\n"
    )
}

fn page_script(endpoint: &str, token: &str) -> String {
    build_page_script(
        endpoint,
        token,
        include_str!("../../../../assets/zcode-injection/page-dictation.js"),
    )
}

/// 注入健康判据：脚本装好 + 页面里的配置与当前语音服务完全一致。
///
/// 「配置一致」是必须的：管理器重启后 voice server 会换端口/令牌，而 ZCode 还开着
/// 旧页面，只判 `__zcodePlusVoiceInstalled` 会把过期配置当成健康。
fn injection_health_script(endpoint: &str, token: &str) -> String {
    let endpoint = serde_json::to_string(endpoint).unwrap_or_else(|_| "\"\"".to_string());
    let token = serde_json::to_string(token).unwrap_or_else(|_| "\"\"".to_string());
    format!(
        "window.__zcodePlusVoiceInstalled === true && !!window.__zcodePlusVoiceConfig && \
         window.__zcodePlusVoiceConfig.endpoint === {endpoint} && \
         window.__zcodePlusVoiceConfig.token === {token}"
    )
}

fn is_zcode_page_target(target: &CdpTarget) -> bool {
    if target.target_type != "page"
        || !target
            .web_socket_debugger_url
            .as_deref()
            .is_some_and(|url| !url.is_empty())
    {
        return false;
    }
    let haystack = format!("{} {}", target.title, target.url).to_lowercase();
    haystack.contains("zcode") || haystack.contains("zcode.app")
}

fn select_zcode_page_target(targets: &[CdpTarget]) -> Option<&CdpTarget> {
    targets.iter().find(|target| is_zcode_page_target(target))
}

/// 从 `Runtime.evaluate` 的 CDP 响应里取布尔返回值（`returnByValue: true`）。
fn evaluate_bool(response: &Value) -> Option<bool> {
    response
        .pointer("/result/result/value")
        .and_then(Value::as_bool)
}

fn script_identifier(response: &Value) -> Option<String> {
    response
        .pointer("/result/identifier")
        .and_then(Value::as_str)
        .filter(|identifier| !identifier.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------- 异步调度

/// 在异步流程里等待：睡眠放到阻塞线程池，不占住 async 工作线程。
async fn async_delay(duration: Duration) {
    let _ = tauri::async_runtime::spawn_blocking(move || std::thread::sleep(duration)).await;
}

async fn ensure_voice_server() -> anyhow::Result<crate::voice_server::VoiceServerInfo> {
    match tauri::async_runtime::spawn_blocking(crate::voice_server::ensure).await {
        Ok(result) => result,
        Err(error) => bail!("本地语音服务启动失败：{error}"),
    }
}

// ---------------------------------------------------------------- CDP 注入

#[derive(Debug, Clone, PartialEq, Eq)]
struct InjectionRecord {
    port: u16,
    target_id: String,
    identifier: Option<String>,
    endpoint: String,
    token: String,
}

static INJECTION_RECORD: OnceLock<Mutex<Option<InjectionRecord>>> = OnceLock::new();

fn injection_record() -> &'static Mutex<Option<InjectionRecord>> {
    INJECTION_RECORD.get_or_init(|| Mutex::new(None))
}

fn current_injection_record() -> Option<InjectionRecord> {
    injection_record()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

fn take_injection_record() -> Option<InjectionRecord> {
    injection_record()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
}

fn store_injection_record(record: InjectionRecord) {
    *injection_record()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Some(record);
}

/// 端口 + target + 配置三者任一变化都要重注册 `Page.addScriptToEvaluateOnNewDocument`：
/// 否则页面下一次 reload 会安装旧端口的配置。
fn needs_script_registration(port: u16, target_id: &str, endpoint: &str, token: &str) -> bool {
    match current_injection_record() {
        Some(record) => {
            record.port != port
                || record.target_id != target_id
                || record.endpoint != endpoint
                || record.token != token
        }
        None => true,
    }
}

async fn find_zcode_target(port: u16) -> anyhow::Result<CdpTarget> {
    let targets = cdp::list_targets(port)
        .await
        .with_context(|| format!("无法读取 ZCode 的调试目标（端口 {port}）"))?;
    select_zcode_page_target(&targets)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("没有找到 ZCode 页面（调试端口 {port}）"))
}

async fn read_injection_health(port: u16) -> anyhow::Result<bool> {
    // 健康判据必须带当前语音服务的 endpoint/token。服务还没起（例如管理器刚重启）
    // 时页面里就算有旧脚本也一定不可用，按未注入处理；守护线程会在几秒内用新配置
    // 重新注入。
    let Some(info) = crate::voice_server::current() else {
        return Ok(false);
    };
    let target = find_zcode_target(port).await?;
    let Some(websocket_url) = target.web_socket_debugger_url.clone() else {
        bail!("ZCode 页面缺少 CDP WebSocket 地址");
    };
    let script = injection_health_script(&info.endpoint, &info.token);
    let response = evaluate_script_return_by_value(&websocket_url, &script).await?;
    Ok(evaluate_bool(&response).unwrap_or(false))
}

async fn read_page_health(
    websocket_url: &str,
    info: &crate::voice_server::VoiceServerInfo,
) -> anyhow::Result<bool> {
    let response = evaluate_script_return_by_value(
        websocket_url,
        &injection_health_script(&info.endpoint, &info.token),
    )
    .await?;
    Ok(evaluate_bool(&response).unwrap_or(false))
}

/// 注册「页面加载后自动安装」的脚本（每个 target + 配置只注册一次）。
async fn register_page_script(
    websocket_url: &str,
    script: &str,
    port: u16,
    target_id: &str,
    info: &crate::voice_server::VoiceServerInfo,
) -> anyhow::Result<()> {
    // 旧配置先摘掉，避免页面 reload 时同时装新旧两份（脚本虽幂等，但会白跑）。
    if let Some(previous) = take_injection_record()
        && previous.port == port
        && previous.target_id == target_id
        && let Some(identifier) = previous.identifier
    {
        let _ = send_cdp_command(
            websocket_url,
            "Page.removeScriptToEvaluateOnNewDocument",
            json!({ "identifier": identifier }),
        )
        .await;
    }
    let response = add_script_to_new_documents(websocket_url, script)
        .await
        .context("向 ZCode 页面注册语音脚本失败")?;
    store_injection_record(InjectionRecord {
        port,
        target_id: target_id.to_string(),
        identifier: script_identifier(&response),
        endpoint: info.endpoint.clone(),
        token: info.token.clone(),
    });
    Ok(())
}

async fn inject_with_info(
    port: u16,
    info: &crate::voice_server::VoiceServerInfo,
) -> anyhow::Result<()> {
    let target = find_zcode_target(port).await?;
    let Some(websocket_url) = target.web_socket_debugger_url.clone() else {
        bail!("ZCode 页面缺少 CDP WebSocket 地址");
    };
    if read_page_health(&websocket_url, info)
        .await
        .unwrap_or(false)
    {
        // 已经是最新配置：守护线程每 3 秒会走到这里，不重复注入、也不刷日志。
        return Ok(());
    }
    let script = page_script(&info.endpoint, &info.token);
    if needs_script_registration(port, &target.id, &info.endpoint, &info.token) {
        register_page_script(&websocket_url, &script, port, &target.id, info).await?;
    }
    evaluate_script(&websocket_url, &script)
        .await
        .context("向 ZCode 页面注入语音脚本失败")?;
    // 脚本是幂等的；执行后立刻复查，页面还在加载时重试几次。
    let mut attempts = 0;
    loop {
        if read_page_health(&websocket_url, info)
            .await
            .unwrap_or(false)
        {
            break;
        }
        attempts += 1;
        if attempts >= INJECT_RETRY_ATTEMPTS {
            bail!("语音脚本已执行，但页面健康检查未通过（ZCode 页面可能还没加载完）");
        }
        async_delay(INJECT_RETRY_INTERVAL).await;
    }
    log_event(
        "zcode.injected",
        json!({
            "port": port,
            "targetId": target.id,
            "endpoint": info.endpoint,
        }),
    );
    Ok(())
}

async fn inject_at(port: u16) -> anyhow::Result<()> {
    let info = ensure_voice_server().await?;
    inject_with_info(port, &info).await
}

/// 关闭语音输入时尽量清掉页面入口（best-effort：页面不在/没端口就直接成功）。
pub async fn dispose() -> anyhow::Result<()> {
    let status = detect_off_thread().await;
    let Some(port) = status.debug_port else {
        return Ok(());
    };
    let Ok(target) = find_zcode_target(port).await else {
        return Ok(());
    };
    let Some(websocket_url) = target.web_socket_debugger_url.clone() else {
        return Ok(());
    };
    let _ = evaluate_script(&websocket_url, DISPOSE_SCRIPT).await;
    if let Some(previous) = take_injection_record()
        && previous.port == port
        && previous.target_id == target.id
        && let Some(identifier) = previous.identifier
    {
        let _ = send_cdp_command(
            &websocket_url,
            "Page.removeScriptToEvaluateOnNewDocument",
            json!({ "identifier": identifier }),
        )
        .await;
    }
    log_event("zcode.disposed", json!({ "port": port }));
    Ok(())
}

// ---------------------------------------------------------------- 启动/退出

fn voice_input_enabled() -> bool {
    SettingsStore::default()
        .load()
        .map(|settings| settings.zcode_app_voice_input)
        .unwrap_or(true)
}

/// 请求 ZCode 主进程退出。
///
/// 统一 SIGTERM：官方包与 ad-hoc 副本实测都能触发完整的 before-quit 流程，不需要
/// Automation/osascript 权限，也绝不发 SIGKILL（用户可能有未保存内容）。
fn request_exit(pid: u32) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        log_event("zcode.exit_request", json!({ "pid": pid, "signal": "TERM" }));
        let output = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("无法向 ZCode 发送退出信号")?;
        if !output.status.success() {
            // 竞态：探测到 PID 之后、发信号之前实例刚好自己退出了（ESRCH）。
            // 这种情况目标已经达成，不要报错卡住启动流程。
            if !process_alive(pid) {
                return Ok(());
            }
            bail!(
                "向 ZCode（PID {pid}）发送退出信号失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        bail!("当前平台不支持 ZCode 增强（仅 macOS）")
    }
}

/// 发信号前再核对一次 PID 是否仍是 ZCode 主进程。
///
/// 探测与发信号之间隔着几十毫秒，极端情况下 PID 可能已被无关进程复用；退出动作
/// 不可逆，值得多花一次 `ps` 确认，绝不按名字盲杀。
#[cfg(target_os = "macos")]
fn process_is_zcode_main(pid: u32) -> bool {
    let Ok(output) = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "pid=,command="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    parse_main_processes(&String::from_utf8_lossy(&output.stdout))
        .iter()
        .any(|process| process.pid == pid)
}

#[cfg(not(target_os = "macos"))]
fn process_is_zcode_main(_pid: u32) -> bool {
    false
}

async fn wait_process_exit(pid: u32) -> bool {
    let deadline = Instant::now() + EXIT_GRACE;
    loop {
        if !process_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        async_delay(EXIT_POLL).await;
    }
}

/// 直接 spawn `Contents/MacOS/ZCode` 并传调试端口参数。
///
/// 不能用 `open -n -a <副本>`：ad-hoc 签名的副本会被 AMFI/LaunchServices 拒绝
/// （实测 adhoc signed not valid），进程根本起不来。直接执行 bundle 内可执行文件
/// 官方包与副本都能工作；环境变量照常继承（副本 e2e 依赖这点隔离用户数据）。
fn spawn_zcode(app: &Path, port: u16) -> anyhow::Result<u32> {
    let executable = app_executable(app);
    ensure!(
        executable.is_file(),
        "ZCode 可执行文件不存在：{}",
        executable.display()
    );
    let args = launch_args(port);
    let mut command = Command::new(&executable);
    command
        .args(&args)
        // 父进程（管理器）可能是从 React/Node 开发环境启动的，这两个变量会让
        // Electron 以 Node 模式运行而不是 GUI，必须从子进程环境里摘掉。
        .env_remove("ELECTRON_RUN_AS_NODE")
        .env_remove("NODE_OPTIONS")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = command
        .spawn()
        .with_context(|| format!("启动 ZCode 失败：{}", executable.display()))?;
    let pid = child.id();
    // 立刻记下会话：Electron 起来后会改进程标题，ps 里就看不到端口参数了。
    write_session_record(&ZcodeSessionRecord {
        pid,
        debug_port: port,
        app_path: app.to_string_lossy().to_string(),
    });
    log_event(
        "zcode.launch_spawned",
        json!({
            "appPath": app.to_string_lossy(),
            "executable": executable.to_string_lossy(),
            "pid": pid,
            "args": args,
        }),
    );
    Ok(pid)
}

/// best-effort 唤起前台窗口（`open` 对 ad-hoc 副本可能失败，忽略）。
#[cfg(target_os = "macos")]
fn bring_to_front(app: &Path) {
    let _ = Command::new("/usr/bin/open")
        .arg(app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = append_diagnostic_log("zcode.activate", json!({ "appPath": app.to_string_lossy() }));
}

#[cfg(not(target_os = "macos"))]
fn bring_to_front(_app: &Path) {}

async fn wait_for_zcode_page(port: u16, pid: u32) -> anyhow::Result<CdpTarget> {
    let deadline = Instant::now() + DEBUG_PORT_WAIT;
    // 两个分支都会先赋值再读；不初始化可以避免「初始值从未被读取」告警。
    let mut last_error;
    // 子进程第一次被观察到退出的时刻；宽限 2 秒再判失败（见下）。
    let mut exited_at: Option<Instant> = None;
    loop {
        match cdp::list_targets(port).await {
            Ok(targets) => {
                if let Some(target) = select_zcode_page_target(&targets) {
                    return Ok(target.clone());
                }
                last_error = "调试端口已监听，但还没有 ZCode 页面目标".to_string();
            }
            Err(error) => last_error = format!("{error:#}"),
        }
        // 进程启动后很快退出：多半是单实例锁（同一 user-data 目录已有实例）或签名
        // 校验拦下，继续等 30 秒没有意义，尽早给出可排查的错误。
        // 留 2 秒宽限：极少数 Electron 启动器会 exec/re-exec，旧 PID 先退出但新进程
        // 很快就能监听端口，不能一看到 PID 消失就判失败。
        if process_alive(pid) {
            exited_at = None;
        } else {
            let first_seen = *exited_at.get_or_insert_with(Instant::now);
            if first_seen.elapsed() >= Duration::from_secs(2) {
                bail!(
                    "ZCode 启动后立即退出（PID {pid}），可能被单实例锁或签名校验拦下；请确认没有其它 ZCode 实例占用同一数据目录"
                );
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "ZCode 已启动，但 {} 秒内没有连上调试端口 {port}：{last_error}",
                DEBUG_PORT_WAIT.as_secs()
            );
        }
        async_delay(DEBUG_PORT_POLL).await;
    }
}

/// 若当前实例已经带调试端口，则补注入并返回最新状态；否则返回错误。
pub async fn reinject_if_possible() -> anyhow::Result<ZcodeStatus> {
    let status = detect_off_thread().await;
    ensure!(status.running, "没有检测到正在运行的 ZCode。");
    let Some(port) = status.debug_port else {
        bail!("ZCode 没有开启调试端口，请先用「启动 ZCode」重新带参启动。");
    };
    inject_at(port).await?;
    Ok(probe().await)
}

/// 启动（必要时先优雅退出旧实例）→ 等调试端口 → 注入 → 返回最新状态。
async fn launch() -> anyhow::Result<ZcodeStatus> {
    let initial = detect_off_thread().await;
    if !initial.supported {
        if let Some(override_value) = app_path_override() {
            bail!("CODEX_PLUS_ZCODE_APP_PATH 指向的 ZCode.app 不存在或不是目录：{override_value}");
        }
        bail!("没有找到 ZCode.app（支持 /Applications/ZCode.app 与 ~/Applications/ZCode.app）");
    }
    let app_path = PathBuf::from(&initial.app_path);
    ensure!(
        app_executable(&app_path).is_file(),
        "ZCode 可执行文件不存在：{}",
        app_executable(&app_path).display()
    );
    let voice_enabled = initial.voice_input;

    if initial.running {
        if let Some(port) = initial.debug_port {
            // 已有调试端口：不退出用户窗口，能补注入就补注入。
            let now_healthy = if voice_enabled {
                match inject_at(port).await {
                    Ok(()) => true,
                    Err(error) => {
                        log_event(
                            "zcode.attach_inject_failed",
                            json!({ "port": port, "message": format!("{error:#}") }),
                        );
                        false
                    }
                }
            } else {
                true
            };
            if now_healthy {
                return Ok(probe().await);
            }
            // 注入失败也先不重启：ZCode 可能是页面还没就绪，交给守护线程/用户重试，
            // 直接重启用户窗口代价太大。
            return Ok(probe().await);
        } else {
            let pid = initial
                .pid
                .ok_or_else(|| anyhow::anyhow!("检测到 ZCode 正在运行，但拿不到进程 PID"))?;
            ensure!(
                process_is_zcode_main(pid),
                "ZCode 进程状态已变化（PID {pid}），请重新点击启动。"
            );
            request_exit(pid)?;
            if !wait_process_exit(pid).await {
                bail!(
                    "ZCode（PID {pid}）正在运行但拒绝退出，请手动退出后重试（Codex++ 不会强制结束它）。"
                );
            }
            // 旧实例确认退出后清掉它可能留下的会话记录（下面会写新的）。
            clear_session_record();
            async_delay(EXIT_SETTLE).await;
        }
    }

    let port = pick_free_loopback_port()?;
    log_event(
        "zcode.launch_request",
        json!({
            "appPath": app_path.to_string_lossy(),
            "debugPort": port,
            "voiceInput": voice_enabled,
        }),
    );
    let launched_pid = match spawn_zcode(&app_path, port) {
        Ok(pid) => pid,
        Err(error) => {
            // 启动失败：清掉可能残留/刚写的记录，避免下一次 detect 误用端口。
            clear_session_record();
            return Err(error);
        }
    };
    let target = match wait_for_zcode_page(port, launched_pid).await {
        Ok(target) => target,
        Err(error) => {
            clear_session_record();
            return Err(error);
        }
    };
    log_event(
        "zcode.launch_ready",
        json!({ "debugPort": port, "targetId": target.id }),
    );
    bring_to_front(&app_path);
    if voice_enabled {
        match inject_at(port).await {
            Ok(()) => {}
            Err(error) => {
                log_event(
                    "zcode.launch_inject_failed",
                    json!({ "debugPort": port, "message": format!("{error:#}") }),
                );
            }
        }
    }
    Ok(probe().await)
}

/// 命令入口：detect / launch / reinject。
pub async fn action(action: &str) -> anyhow::Result<ZcodeStatus> {
    match action.trim() {
        "detect" => Ok(probe().await),
        "launch" => launch().await,
        "reinject" => reinject_if_possible().await,
        other => bail!("未知的 ZCode 操作：{other}"),
    }
}

// ---------------------------------------------------------------- 补注入守护线程

static SUPERVISOR_STOP: AtomicBool = AtomicBool::new(true);
static SUPERVISOR_RUNNING: AtomicBool = AtomicBool::new(false);
static SUPERVISOR_LAST_NOTE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn supervisor_last_note() -> &'static Mutex<Option<String>> {
    SUPERVISOR_LAST_NOTE.get_or_init(|| Mutex::new(None))
}

/// 启动补注入守护线程（幂等）。只做一件事：设置开着 + ZCode 已运行且带调试端口
/// + 页面注入不健康（含配置过期）时补一次注入。异常只写诊断日志，不弹窗。
pub fn start_supervisor(app_handle: tauri::AppHandle) {
    if SUPERVISOR_RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    SUPERVISOR_STOP.store(false, Ordering::Release);
    log_event("zcode.supervisor_started", json!({}));
    tauri::async_runtime::spawn(async move {
        // 只为生命周期语义持有：管理器退出后线程靠 SUPERVISOR_STOP / app_is_exiting 收尾。
        let _app_handle = app_handle;
        while !SUPERVISOR_STOP.load(Ordering::Acquire) {
            async_delay(SUPERVISOR_TICK).await;
            if SUPERVISOR_STOP.load(Ordering::Acquire) || crate::app_is_exiting() {
                break;
            }
            let note = supervisor_tick().await;
            publish_supervisor_note(note);
        }
        SUPERVISOR_RUNNING.store(false, Ordering::Release);
        log_event("zcode.supervisor_stopped", json!({}));
    });
}

/// 请求停止守护线程（程序退出时调用）。下次 tick 自行退出。
pub fn stop_supervisor() {
    SUPERVISOR_STOP.store(true, Ordering::Release);
}

async fn supervisor_tick() -> String {
    if !voice_input_enabled() {
        return "disabled".to_string();
    }
    let status = detect_off_thread().await;
    if !status.supported {
        return "unsupported".to_string();
    }
    if !status.running {
        return "not-running".to_string();
    }
    let Some(port) = status.debug_port else {
        return "no-debug-port".to_string();
    };
    // 健康判据要带当前语音服务的 endpoint/token；服务没起就先起（幂等），
    // 已起则直接复用，避免每 3 秒都走一次启动逻辑。
    let info = match crate::voice_server::current() {
        Some(info) => info,
        None => match ensure_voice_server().await {
            Ok(info) => info,
            Err(error) => return format!("voice-server-failed: {error:#}"),
        },
    };
    match inject_with_info(port, &info).await {
        Ok(()) => "injected".to_string(),
        Err(error) => format!("inject-failed: {error:#}"),
    }
}

/// 只在状态变化时写诊断日志：守护线程 3 秒一跳，正常时不能刷屏。
fn publish_supervisor_note(note: String) {
    let changed = {
        let mut last = supervisor_last_note()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if last.as_deref() == Some(note.as_str()) {
            false
        } else {
            *last = Some(note.clone());
            true
        }
    };
    if changed {
        log_event("zcode.supervisor", json!({ "state": note }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PS_SAMPLE: &str = "\
 101 /Applications/ZCode.app/Contents/MacOS/ZCode
 102 /Applications/ZCode.app/Contents/Frameworks/ZCode Helper.app/Contents/MacOS/ZCode Helper --type=gpu-process
 201 /tmp/zcode-e2e/ZCode.app/Contents/MacOS/ZCode --remote-debugging-port=53421 --remote-allow-origins=http://127.0.0.1:53421
 202 /tmp/zcode-e2e/ZCode.app/Contents/Frameworks/ZCode Helper (Renderer).app/Contents/MacOS/ZCode Helper (Renderer) --type=renderer
 301 /Applications/Other.app/Contents/MacOS/Other
 302 /Users/someone/Code/ZCode.app/Contents/MacOS/ZCode --remote-debugging-port 6123
 401 ZCode
 402 zcode-cli app-server --remote-debugging-port=6124
";

    #[test]
    fn parses_only_main_zcode_processes() {
        let processes = parse_main_processes_with(PS_SAMPLE, |_| None);
        let pids: Vec<u32> = processes.iter().map(|process| process.pid).collect();
        // 102/202 是 Chromium 子进程（--type=），不能算主进程；301 与 ZCode 无关；
        // 401 是 LaunchServices 正常启动的主进程（argv0 只有 ZCode），402 是包内 CLI。
        assert_eq!(pids, vec![101, 201, 302, 401]);
        assert_eq!(
            processes[0].app_bundle.as_deref(),
            Some(Path::new("/Applications/ZCode.app"))
        );
        assert_eq!(
            processes[0].executable.as_deref(),
            Some(Path::new("/Applications/ZCode.app/Contents/MacOS/ZCode"))
        );
        assert_eq!(processes[0].debug_port, None);
        assert_eq!(processes[1].debug_port, Some(53421));
        // 兼容 `--remote-debugging-port 6123` 的空格写法。
        assert_eq!(processes[2].debug_port, Some(6123));
        // 正常双击启动的实例没有路径信息，但仍要能被识别成「主进程在跑」。
        assert_eq!(processes[3].pid, 401);
        assert!(processes[3].app_bundle.is_none());
        assert_eq!(processes[3].debug_port, None);
    }

    #[test]
    fn ignores_helper_and_malformed_lines() {
        assert!(parse_main_processes_with("", |_| None).is_empty());
        assert!(parse_main_processes("not a ps line").is_empty());
        assert!(parse_main_processes(
            " 55 /Applications/ZCode.app/Contents/Frameworks/ZCode Helper.app/Contents/MacOS/ZCode Helper"
        )
        .is_empty());
    }

    #[test]
    fn selects_process_for_configured_copy_and_never_guesses() {
        let processes = parse_main_processes_with(PS_SAMPLE, |_| None);
        let copy = Path::new("/tmp/zcode-e2e/ZCode.app");
        // 环境变量指向副本：只能选副本进程，绝不能退到官方实例。
        let selected = select_main_process(&processes, Some(copy), false).expect("应选中副本进程");
        assert_eq!(selected.pid, 201);
        // 副本路径存在但没在跑：返回 None（不能误把官方实例当目标退掉）。
        assert!(select_main_process(&processes, Some(Path::new("/tmp/missing/ZCode.app")), false).is_none());
        // 没配置路径（正式用户）：优先精确匹配官方路径。
        let official = select_main_process(
            &processes,
            Some(Path::new("/Applications/ZCode.app")),
            true,
        )
        .expect("应选中官方实例");
        assert_eq!(official.pid, 101);
    }

    #[test]
    fn falls_back_to_any_instance_when_no_expected_path() {
        let processes = parse_main_processes_with(PS_SAMPLE, |_| None);
        // 默认路径解析不到时（例如用户从 Finder 拖出了副本）仍应能接管。
        let selected = select_main_process(&processes, None, true).expect("应退回任意主进程");
        assert_eq!(selected.pid, 101);
        assert!(select_main_process(&processes, None, false).is_none());
    }

    #[test]
    fn resolves_launch_services_instance_path_when_proc_pidpath_works() {
        // 实际 macOS 上 proc_pidpath 能补全路径；补全后就能按路径精确匹配，
        // 环境变量指向官方路径时也能正确识别（不会傻等新实例）。
        let processes = parse_main_processes_with(" 401 ZCode --remote-debugging-port=53421\n", |pid| {
            (pid == 401)
                .then(|| PathBuf::from("/Applications/ZCode.app/Contents/MacOS/ZCode"))
        });
        assert_eq!(processes.len(), 1);
        assert_eq!(
            processes[0].app_bundle.as_deref(),
            Some(Path::new("/Applications/ZCode.app"))
        );
        assert_eq!(processes[0].debug_port, Some(53421));
        let selected = select_main_process(
            &processes,
            Some(Path::new("/Applications/ZCode.app")),
            false,
        )
        .expect("补全路径后应能按官方路径匹配");
        assert_eq!(selected.pid, 401);
        // 环境变量指向副本时仍然不能选中官方实例。
        assert!(
            select_main_process(&processes, Some(Path::new("/tmp/copy/ZCode.app")), false).is_none()
        );
    }

    #[test]
    fn unrelated_process_named_zcode_is_rejected_when_path_resolution_says_so() {
        // 系统里恰好有个叫 ZCode 的无关进程：proc_pidpath 能解析出真实路径时，
        // 必须直接排除，绝不能把它当成 ZCode 去 SIGTERM。
        let processes = parse_main_processes_with(" 401 ZCode\n", |_| {
            Some(PathBuf::from("/usr/local/bin/ZCode"))
        });
        assert!(processes.is_empty());
        // 反过来，路径没法解析时保留「未知路径」候选（真实 ZCode 的兜底）。
        let processes = parse_main_processes_with(" 401 ZCode\n", |_| None);
        assert_eq!(processes.len(), 1);
        assert!(processes[0].app_bundle.is_none());
    }

    #[test]
    fn path_embedded_in_another_process_arguments_is_not_an_executable() {
        // `open /Applications/ZCode.app/Contents/MacOS/ZCode` 的参数里含完整路径，
        // 但 argv0 是 /usr/bin/open，不能被解析成 ZCode 主进程。
        let processes = parse_main_processes_with(
            " 500 /usr/bin/open /Applications/ZCode.app/Contents/MacOS/ZCode\n",
            |_| None,
        );
        assert!(processes.is_empty());
    }

    #[test]
    fn launch_services_started_instance_is_matched_only_without_path_override() {
        let processes = parse_main_processes_with(" 401 ZCode\n", |_| None);
        // 正式用户：双击启动的官方实例也要被识别（无端口 → 需要重启接管）。
        let selected = select_main_process(&processes, Some(Path::new("/Applications/ZCode.app")), true)
            .expect("应退回 argv0 只有 ZCode 的主进程");
        assert_eq!(selected.pid, 401);
        // 环境变量指向副本时：绝不能把这个「不知道路径」的实例当成副本目标，
        // 否则退出逻辑会误杀用户正在用的官方 ZCode。
        assert!(select_main_process(&processes, Some(Path::new("/tmp/copy/ZCode.app")), false).is_none());
    }

    #[test]
    fn chooses_app_path_override_before_default_candidates() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("ZCode.app");
        std::fs::create_dir_all(&existing).unwrap();
        let candidates = vec![temp.path().join("missing").join("ZCode.app"), existing.clone()];
        // 没有环境变量覆盖时取第一个存在的默认候选。
        assert_eq!(
            choose_app_path(None, &candidates).as_deref(),
            Some(existing.as_path())
        );
        // 环境变量覆盖优先，且即使目录不存在也原样返回（便于报「副本不存在」）。
        assert_eq!(
            choose_app_path(Some("  /tmp/custom/ZCode.app "), &candidates),
            Some(PathBuf::from("/tmp/custom/ZCode.app"))
        );
        // 空白覆盖按未设置处理。
        assert_eq!(
            choose_app_path(Some("   "), &candidates).as_deref(),
            Some(existing.as_path())
        );
        assert_eq!(choose_app_path(None, &[]), None);
    }

    #[test]
    fn parses_plist_version_from_xml_info_plist() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>dev.zcode.app</string>
  <key>CFBundleShortVersionString</key>
  <string>3.16.2</string>
  <key>CFBundleVersion</key>
  <string>31602</string>
</dict>
</plist>"#;
        assert_eq!(parse_plist_version(xml).as_deref(), Some("3.16.2"));
        assert_eq!(
            parse_plist_version("<dict><key>CFBundleShortVersionString</key><string></string></dict>"),
            None
        );
        assert_eq!(parse_plist_version("plain text"), None);
    }

    #[test]
    fn launch_args_carry_debug_port_and_allow_origins() {
        assert_eq!(
            launch_args(53421),
            vec![
                "--remote-debugging-port=53421".to_string(),
                "--remote-allow-origins=http://127.0.0.1:53421".to_string(),
            ]
        );
    }

    #[test]
    fn picks_a_usable_loopback_port() {
        let port = pick_free_loopback_port().expect("应能分配端口");
        assert!(port >= 1024);
        // 端口是「探测后释放」的，分配逻辑本身不应 panic；这里只验证范围。
        assert!(port > 0);
    }

    #[test]
    fn page_script_writes_config_before_page_source_and_escapes_strings() {
        let script = build_page_script(
            "http://127.0.0.1:53421",
            "token-with-\"quote\"-and-\\slash",
            "/* page source marker */",
        );
        assert!(
            script.starts_with("window.__zcodePlusVoiceConfig = { endpoint: \"http://127.0.0.1:53421\""),
            "配置必须先于页面脚本写入：{script}"
        );
        // 令牌里的引号/反斜杠必须按 JSON 转义，否则拼出来的 JS 是语法错误。
        assert!(script.contains(r#"token: "token-with-\"quote\"-and-\\slash""#));
        assert!(script.contains("/* page source marker */"));
        assert!(script.ends_with('\n'));
    }

    #[test]
    fn bundled_page_script_contains_installation_flag_contract() {
        // 页面脚本由 inject-page 队友维护；这里守住注入健康判据依赖的契约：
        // 脚本最终必须设置 window.__zcodePlusVoiceInstalled（占位阶段为 false 也算设置）。
        let script = page_script("http://127.0.0.1:1", "token");
        assert!(script.contains("window.__zcodePlusVoiceConfig"));
        assert!(
            script.contains("window.__zcodePlusVoiceInstalled"),
            "注入脚本必须维护 window.__zcodePlusVoiceInstalled 标志"
        );
    }

    #[test]
    fn injection_health_requires_matching_endpoint_and_token() {
        let script = injection_health_script("http://127.0.0.1:53421", "token-a");
        assert!(script.contains(r#"window.__zcodePlusVoiceConfig.endpoint === "http://127.0.0.1:53421""#));
        assert!(script.contains(r#"window.__zcodePlusVoiceConfig.token === "token-a""#));
        assert!(script.contains("window.__zcodePlusVoiceInstalled === true"));
        assert!(script.contains("!!window.__zcodePlusVoiceConfig"));
    }

    #[test]
    fn selects_zcode_file_page_targets_only() {
        let target = |id: &str, target_type: &str, title: &str, url: &str, ws: Option<&str>| CdpTarget {
            id: id.to_string(),
            target_type: target_type.to_string(),
            title: title.to_string(),
            url: url.to_string(),
            web_socket_debugger_url: ws.map(str::to_string),
        };
        let targets = vec![
            target("a", "browser", "", "", Some("ws://127.0.0.1:1/devtools/browser/x")),
            target(
                "b",
                "page",
                "DevTools",
                "devtools://devtools/bundled/inspector.html",
                Some("ws://127.0.0.1:1/devtools/page/b"),
            ),
            target("c", "page", "", "", None),
            target(
                "d",
                "page",
                "ZCode",
                "file:///tmp/copy/ZCode.app/Contents/Resources/app.asar/out/renderer/index.html",
                Some("ws://127.0.0.1:53421/devtools/page/d"),
            ),
        ];
        let selected = select_zcode_page_target(&targets).expect("应选中 ZCode 页面");
        assert_eq!(selected.id, "d");
    }

    #[test]
    fn script_identifier_and_bool_evaluation_are_extracted_from_cdp_response() {
        assert_eq!(
            script_identifier(&json!({ "result": { "identifier": "7" } })).as_deref(),
            Some("7")
        );
        assert_eq!(script_identifier(&json!({ "result": {} })), None);
        assert_eq!(
            evaluate_bool(&json!({ "result": { "result": { "type": "boolean", "value": true } } })),
            Some(true)
        );
        assert_eq!(
            evaluate_bool(&json!({ "result": { "result": { "type": "boolean", "value": false } } })),
            Some(false)
        );
        assert_eq!(evaluate_bool(&json!({})), None);
    }

    #[test]
    fn needs_script_registration_after_config_changes() {
        // 进程内静态状态：同一个测试里按顺序验证「首次注册 → 同配置复用 → 换配置重注册」。
        store_injection_record(InjectionRecord {
            port: 53421,
            target_id: "target-a".to_string(),
            identifier: Some("1".to_string()),
            endpoint: "http://127.0.0.1:53421".to_string(),
            token: "token-a".to_string(),
        });
        assert!(!needs_script_registration(
            53421,
            "target-a",
            "http://127.0.0.1:53421",
            "token-a"
        ));
        // 管理器重启后 voice server 端口/令牌变了：必须重新注册，否则页面 reload 装旧配置。
        assert!(needs_script_registration(
            53421,
            "target-a",
            "http://127.0.0.1:53422",
            "token-b"
        ));
        // ZCode 重启后 target id 变了：同样要重新注册。
        assert!(needs_script_registration(
            53421,
            "target-b",
            "http://127.0.0.1:53421",
            "token-a"
        ));
        let record = take_injection_record().expect("记录还在");
        assert_eq!(record.identifier.as_deref(), Some("1"));
        assert!(needs_script_registration(
            53421,
            "target-a",
            "http://127.0.0.1:53421",
            "token-a"
        ));
    }

    #[test]
    fn session_record_fills_debug_port_for_matching_live_instance() {
        // Electron 会把进程标题改成 ZCode，ps 里看不到端口参数；记录匹配时必须回填，
        // 否则已拉起且注入成功的实例会被误判成「需要重启」。
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("ZCode.app");
        std::fs::create_dir_all(&app).unwrap();
        let record = ZcodeSessionRecord {
            pid: 73_084,
            debug_port: 54_270,
            app_path: app.to_string_lossy().to_string(),
        };
        let mut status = ZcodeStatus {
            supported: true,
            running: true,
            pid: Some(73_084),
            debug_port: None,
            ..ZcodeStatus::default()
        };
        assert!(fill_debug_port_from_session_record(
            &mut status,
            Some(&record),
            Some(&app),
            |_| true
        ));
        assert_eq!(status.debug_port, Some(54_270));
        finalize_status(&mut status);
        assert!(!status.needs_restart, "回填端口后不应再提示需要重启");
        // ps 里已经解析到端口时（标题还没被改的窗口期）不覆盖记录值。
        let mut parsed = ZcodeStatus {
            running: true,
            pid: Some(73_084),
            debug_port: Some(54_271),
            ..ZcodeStatus::default()
        };
        assert!(fill_debug_port_from_session_record(
            &mut parsed,
            Some(&record),
            Some(&app),
            |_| true
        ));
        assert_eq!(parsed.debug_port, Some(54_271));
    }

    #[test]
    fn session_record_is_ignored_for_dead_pid_or_other_app() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("ZCode.app");
        std::fs::create_dir_all(&app).unwrap();
        let record = ZcodeSessionRecord {
            pid: 73_084,
            debug_port: 54_270,
            app_path: app.to_string_lossy().to_string(),
        };
        let running = |pid: u32| pid == 73_084;
        let mut status = ZcodeStatus {
            running: true,
            pid: Some(73_084),
            ..ZcodeStatus::default()
        };
        // pid 已不存活：忽略记录，并按「无端口」算需要重启。
        assert!(!fill_debug_port_from_session_record(
            &mut status,
            Some(&record),
            Some(&app),
            |_| false
        ));
        assert_eq!(status.debug_port, None);
        finalize_status(&mut status);
        assert!(status.needs_restart);

        // 记录的 pid 与当前实例不一致（PID 换代）：忽略。
        status.pid = Some(999);
        assert!(!fill_debug_port_from_session_record(
            &mut status,
            Some(&record),
            Some(&app),
            running
        ));

        // appPath 不一致（比如环境变量指向另一个副本）：忽略。
        status.pid = Some(73_084);
        let other_app = temp.path().join("Other.app");
        assert!(!fill_debug_port_from_session_record(
            &mut status,
            Some(&record),
            Some(&other_app),
            running
        ));
        // 连 expected app 都没有：不能确认归属，忽略。
        assert!(!fill_debug_port_from_session_record(
            &mut status,
            Some(&record),
            None,
            running
        ));
        assert_eq!(status.debug_port, None);
        // 没有记录时也不回填。
        assert!(!fill_debug_port_from_session_record(
            &mut status,
            None,
            Some(&app),
            running
        ));
    }

    #[test]
    fn session_record_discard_survives_the_spawn_loading_window() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("ZCode.app");
        std::fs::create_dir_all(&app).unwrap();
        let record = ZcodeSessionRecord {
            pid: 73_084,
            debug_port: 54_270,
            app_path: app.to_string_lossy().to_string(),
        };
        // 守护线程的 ps 快照可能恰好拍在 spawn 前后：进程活着但本轮没检测到，
        // 记录不能删（否则端口信息永久丢失）。
        assert!(!should_discard_session_record(
            &record,
            Some(&app),
            |_| true
        ));
        // 记录里的进程已退出：确定过期。
        assert!(should_discard_session_record(
            &record,
            Some(&app),
            |_| false
        ));
        // 期望的 app 换成了另一个副本/路径：记录不属于它。
        let other_app = temp.path().join("Other.app");
        assert!(should_discard_session_record(
            &record,
            Some(&other_app),
            |_| true
        ));
        // 没有可解析的期望路径时不主动删（无法判断归属，下次再说）。
        assert!(!should_discard_session_record(&record, None, |_| true));
    }

    #[test]
    fn needs_restart_requires_running_without_debug_port() {
        let mut status = ZcodeStatus {
            running: true,
            debug_port: None,
            ..ZcodeStatus::default()
        };
        finalize_status(&mut status);
        assert!(status.needs_restart);
        status.debug_port = Some(54_270);
        finalize_status(&mut status);
        assert!(!status.needs_restart);
        let mut stopped = ZcodeStatus {
            running: false,
            debug_port: None,
            ..ZcodeStatus::default()
        };
        finalize_status(&mut stopped);
        assert!(!stopped.needs_restart);
    }

    #[test]
    fn session_record_json_uses_the_agreed_camel_case_shape() {
        let record = ZcodeSessionRecord {
            pid: 73_084,
            debug_port: 54_270,
            app_path: "/tmp/zcode-e2e/ZCode.app".to_string(),
        };
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(encoded["pid"], json!(73_084));
        assert_eq!(encoded["debugPort"], json!(54_270));
        assert_eq!(encoded["appPath"], json!("/tmp/zcode-e2e/ZCode.app"));
        let decoded: ZcodeSessionRecord = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn status_default_reports_unsupported() {
        let status = ZcodeStatus::default();
        assert!(!status.supported);
        assert!(!status.running);
        assert!(status.pid.is_none());
        assert!(status.debug_port.is_none());
    }
}
