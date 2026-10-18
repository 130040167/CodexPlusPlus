use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
#[cfg(windows)]
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalDictationConfig {
    pub enabled: bool,
    pub hotkey: String,
}

impl Default for GlobalDictationConfig {
    fn default() -> Self {
        Self { enabled: false, hotkey: "Ctrl+Shift+Space".to_string() }
    }
}

fn config_path() -> PathBuf {
    crate::paths::default_app_state_dir().join("global-dictation.json")
}

pub fn load_config() -> GlobalDictationConfig {
    let Ok(text) = fs::read_to_string(config_path()) else { return GlobalDictationConfig::default(); };
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn save_config(config: &GlobalDictationConfig) -> anyhow::Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() { fs::create_dir_all(parent)?; }
    crate::settings::atomic_write(&path, serde_json::to_string_pretty(config)?.as_bytes())
}

#[derive(Debug)]
pub struct GlobalHotkeyHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for GlobalHotkeyHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

#[cfg(windows)]
fn parse_hotkey(value: &str) -> anyhow::Result<(u32, u32)> {
    let mut modifiers = 0_u32;
    let mut key = None;
    for part in value.split('+').map(str::trim).filter(|part| !part.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= 0x0002,
            "shift" => modifiers |= 0x0004,
            "alt" => modifiers |= 0x0001,
            "win" | "meta" | "command" => modifiers |= 0x0008,
            "space" => key = Some(0x20),
            value if value.len() == 1 && value.as_bytes()[0].is_ascii_alphanumeric() => key = Some(value.as_bytes()[0].to_ascii_uppercase() as u32),
            value if value.starts_with('f') && value[1..].parse::<u32>().is_ok() => {
                let number = value[1..].parse::<u32>().unwrap();
                anyhow::ensure!((1..=12).contains(&number), "F 键范围只支持 F1-F12");
                key = Some(0x70 + number - 1);
            }
            _ => anyhow::bail!("不支持的全局热键按键：{part}"),
        }
    }
    anyhow::ensure!(modifiers != 0 && key.is_some(), "全局热键需要修饰键和按键，例如 Ctrl+Shift+Space");
    Ok((modifiers, key.unwrap()))
}

#[cfg(windows)]
pub fn start<F>(hotkey: &str, callback: F) -> anyhow::Result<GlobalHotkeyHandle>
where F: Fn() + Send + 'static {
    use std::ffi::c_void;
    #[repr(C)] struct Msg { hwnd: *mut c_void, message: u32, w_param: usize, l_param: isize, time: u32, point: [i32; 2], private: u32 }
    unsafe extern "system" {
        fn RegisterHotKey(hwnd: *mut c_void, id: i32, modifiers: u32, key: u32) -> i32;
        fn UnregisterHotKey(hwnd: *mut c_void, id: i32) -> i32;
        fn PeekMessageW(msg: *mut Msg, hwnd: *mut c_void, min: u32, max: u32, remove: u32) -> i32;
    }
    let (modifiers, key) = parse_hotkey(hotkey)?;
    let id = 0xC0D3_0001_i32;
    if unsafe { RegisterHotKey(std::ptr::null_mut(), id, modifiers, key) } == 0 {
        anyhow::bail!("全局热键注册失败，可能已被其他程序占用：{hotkey}");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = stop.clone();
    let thread = std::thread::Builder::new().name("codex-plus-global-dictation".into()).spawn(move || {
        while !thread_stop.load(Ordering::SeqCst) {
            let mut msg = Msg { hwnd: std::ptr::null_mut(), message: 0, w_param: 0, l_param: 0, time: 0, point: [0, 0], private: 0 };
            while unsafe { PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, 1) } != 0 {
                if msg.message == 0x0312 && msg.w_param == id as usize { callback(); }
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        unsafe { UnregisterHotKey(std::ptr::null_mut(), id); }
    })?;
    Ok(GlobalHotkeyHandle { stop, thread: Some(thread) })
}

#[cfg(not(windows))]
pub fn start<F>(_hotkey: &str, _callback: F) -> anyhow::Result<GlobalHotkeyHandle>
where F: Fn() + Send + 'static {
    anyhow::bail!("全局听写热键首版仅支持 Windows");
}

static HANDLE: OnceLock<Mutex<Option<GlobalHotkeyHandle>>> = OnceLock::new();

pub fn replace_handle(handle: Option<GlobalHotkeyHandle>) {
    let slot = HANDLE.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap() = handle;
}

pub fn validate_hotkey(value: &str) -> anyhow::Result<()> {
    #[cfg(windows)] { let _ = parse_hotkey(value)?; }
    #[cfg(not(windows))] { anyhow::ensure!(!value.trim().is_empty(), "热键不能为空"); }
    Ok(())
}
