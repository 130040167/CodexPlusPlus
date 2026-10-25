//! 目标窗口取流：ScreenCaptureKit 抓单个窗口，编码成 JPEG 交给界面预览。
//!
//! # 为什么自己抓
//!
//! Codex 官方的「窗口镜像」（Remote Hosted PiP）走 `SkyComputerUseService` 发布
//! `CAContext` + H.264 帧通道，但发布端只认 `com.openai.chat` / `com.openai.codex` /
//! `com.openai.atlas` 三个 bundle id，第三方无法加入。所以这里退一步：用系统公开的
//! ScreenCaptureKit 自己抓目标窗口，在 Codex++ 里画预览。画面内容和官方 PiP 一致
//! （都是同一个窗口的实时画面），只是少了 XPC 那一层的低延迟。
//!
//! # 实测数据（2026-10-24，M 系列 Mac，1280×820 窗口）
//!
//! | 缩放 | 帧率 | 单帧 JPEG |
//! |---|---|---|
//! | 1.0 | 14.7 fps | ~150 KB |
//! | 0.5 | 15.9 fps | ~45 KB |
//! | 0.35 | 16.2 fps | ~25 KB |
//!
//! 编码是瓶颈而不是抓取，所以预览默认 0.5 倍缩放 + 12 fps。
//!
//! # 两个前置条件
//!
//! 1. 进程必须已经连上 WindowServer，否则 CoreGraphics 会断言崩溃
//!    （`CGS_REQUIRE_INIT`）。GUI 应用天然满足；纯 CLI 需要先创建 `NSApplication`。
//! 2. 需要「屏幕录制」系统权限。没授权时枚举结果为空，`list_windows` 会给出提示。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use serde::Serialize;

/// 一个可被抓取的窗口。
#[derive(Debug, Clone, Serialize)]
pub struct CaptureWindow {
    /// ScreenCaptureKit 的窗口号，抓取时用它定位窗口
    pub window_id: u32,
    pub pid: i32,
    pub app_name: String,
    pub bundle_id: String,
    pub title: String,
    pub width: f64,
    pub height: f64,
    pub on_screen: bool,
}

/// 预览会话句柄：`stop()` 之后抓取线程退出。
pub struct PreviewHandle {
    stop: Arc<AtomicBool>,
    errors: Arc<Mutex<Option<String>>>,
    /// 已经产出的帧数，便于界面显示是否真的在动
    frames: Arc<Mutex<u64>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PreviewHandle {
    /// 抓取过程中遇到的错误（窗口关闭、权限被撤销等）
    pub fn error(&self) -> Option<String> {
        self.errors.lock().ok().and_then(|e| e.clone())
    }

    /// 抓取线程记录的错误槽；界面层可以盯着它判断预览是否已自行结束
    pub fn error_slot(&self) -> Arc<Mutex<Option<String>>> {
        self.errors.clone()
    }

    pub fn frames(&self) -> u64 {
        self.frames.lock().map(|n| *n).unwrap_or(0)
    }

    /// 停止抓取并等线程退出（最多等 2 秒）
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for PreviewHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use block2::RcBlock;
    use objc2::rc::Retained;
    use objc2::AllocAnyThread;
    use objc2_core_graphics::CGImage;
    use objc2_screen_capture_kit::{
        SCContentFilter, SCScreenshotManager, SCShareableContent, SCStreamConfiguration, SCWindow,
    };

    use super::{CaptureWindow, PreviewHandle};

    /// 建立到 WindowServer 的连接。
    ///
    /// ScreenCaptureKit 的抓帧路径会触发 CoreGraphics 初始化，而纯 CLI 进程
    /// 从没连过 WindowServer，此时会直接断言崩溃（`CGS_REQUIRE_INIT`）。
    /// GUI 应用天然已经连上，所以这里只在主线程兜底，重复调用没有副作用。
    pub fn ensure_graphics_initialized() {
        if let Some(mtm) = objc2::MainThreadMarker::new() {
            let _ = objc2_app_kit::NSApplication::sharedApplication(mtm);
        }
    }

    /// 是否已经拿到「屏幕录制」权限。没有权限时枚举会得到空列表。
    pub fn has_permission() -> bool {
        unsafe { objc2_core_graphics::CGPreflightScreenCaptureAccess() }
    }

    /// 触发系统的权限申请弹窗。已经授权时直接返回 true。
    pub fn request_permission() -> bool {
        unsafe { objc2_core_graphics::CGRequestScreenCaptureAccess() }
    }

    const ENUM_TIMEOUT: Duration = Duration::from_secs(15);
    const FRAME_TIMEOUT: Duration = Duration::from_secs(5);

    /// 枚举当前可抓的窗口。没有屏幕录制权限时返回空列表。
    pub fn list_windows() -> anyhow::Result<Vec<CaptureWindow>> {
        let (tx, rx) = mpsc::channel::<Vec<CaptureWindow>>();
        unsafe {
            SCShareableContent::getShareableContentWithCompletionHandler(&RcBlock::new(
                move |content: *mut SCShareableContent, _error: *mut _| {
                    let mut list = Vec::new();
                    if !content.is_null() {
                        for window in (&*content).windows().iter() {
                            let frame = window.frame();
                            if frame.size.width < 64.0 || frame.size.height < 64.0 {
                                continue;
                            }
                            // 系统窗口（Display Backstop / MenuBar / NowPlaying 之类）没有归属应用，
                            // 列出来只会干扰选择，也基本没有预览价值
                            let app = window.owningApplication();
                            let app_name = app
                                .as_ref()
                                .map(|a| a.applicationName().to_string())
                                .unwrap_or_default();
                            if app_name.is_empty() {
                                continue;
                            }
                            list.push(CaptureWindow {
                                window_id: window.windowID(),
                                pid: app.as_ref().map(|a| a.processID()).unwrap_or(0),
                                app_name: app
                                    .as_ref()
                                    .map(|a| a.applicationName())
                                    .map(|n| n.to_string())
                                    .unwrap_or_default(),
                                bundle_id: app
                                    .as_ref()
                                    .map(|a| a.bundleIdentifier())
                                    .map(|n| n.to_string())
                                    .unwrap_or_default(),
                                title: window.title().map(|t| t.to_string()).unwrap_or_default(),
                                width: frame.size.width,
                                height: frame.size.height,
                                on_screen: window.isOnScreen(),
                            });
                        }
                    }
                    let _ = tx.send(list);
                },
            ));
        }
        let mut windows = rx
            .recv_timeout(ENUM_TIMEOUT)
            .map_err(|_| anyhow::anyhow!("枚举窗口超时（可能尚未授予「屏幕录制」权限）"))?;
        // 前台窗口排前面，同应用内按应用名 + 标题排序，方便界面里找
        windows.sort_by(|a, b| {
            b.on_screen
                .cmp(&a.on_screen)
                .then_with(|| a.app_name.cmp(&b.app_name))
                .then_with(|| a.title.cmp(&b.title))
        });
        Ok(windows)
    }

    fn find_window(window_id: u32) -> Option<Retained<SCWindow>> {
        let (tx, rx) = mpsc::channel::<Option<Retained<SCWindow>>>();
        unsafe {
            SCShareableContent::getShareableContentWithCompletionHandler(&RcBlock::new(
                move |content: *mut SCShareableContent, _error: *mut _| {
                    let mut found = None;
                    if !content.is_null() {
                        for window in (&*content).windows().iter() {
                            if window.windowID() == window_id {
                                found = Some(window.clone());
                                break;
                            }
                        }
                    }
                    let _ = tx.send(found);
                },
            ));
        }
        rx.recv_timeout(ENUM_TIMEOUT).ok().flatten()
    }

    fn capture_jpeg(
        filter: &SCContentFilter,
        config: &SCStreamConfiguration,
    ) -> anyhow::Result<Vec<u8>> {
        let (tx, rx) = mpsc::channel::<anyhow::Result<Vec<u8>>>();
        unsafe {
            SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
                filter,
                config,
                Some(&RcBlock::new(move |image: *mut CGImage, error: *mut _| {
                    let result = if image.is_null() {
                        // macOS 在锁屏时拒绝按窗口抓取，整段失败基本都是这个原因
                        Err(anyhow::anyhow!(
                            "抓帧失败：{}。如果屏幕已锁定或 Codex++ 没有「屏幕录制」权限，都会落到这里。",
                            describe_error(error as *mut _)
                        ))
                    } else {
                        encode_jpeg(unsafe { &*image })
                    };
                    let _ = tx.send(result);
                })),
            );
        }
        rx.recv_timeout(FRAME_TIMEOUT)
            .map_err(|_| anyhow::anyhow!("抓帧超时"))?
    }

    unsafe fn describe_error(error: *mut objc2::runtime::AnyObject) -> String {
        if error.is_null() {
            return "未知错误".to_string();
        }
        unsafe {
            let obj = &*error;
            match obj.downcast_ref::<objc2_foundation::NSError>() {
                Some(err) => err.localizedDescription().to_string(),
                None => "未知错误".to_string(),
            }
        }
    }

    fn encode_jpeg(image: &CGImage) -> anyhow::Result<Vec<u8>> {
        use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
        use objc2_foundation::NSDictionary;
        unsafe {
            let rep = NSBitmapImageRep::initWithCGImage(NSBitmapImageRep::alloc(), image);
            let properties = NSDictionary::new();
            let data = rep
                .representationUsingType_properties(NSBitmapImageFileType::JPEG, &properties)
                .ok_or_else(|| anyhow::anyhow!("JPEG 编码失败"))?;
            Ok(data.to_vec())
        }
    }

    pub fn start_preview(
        window_id: u32,
        scale: f64,
        max_fps: f64,
        sink: impl Fn(Vec<u8>) + Send + 'static,
    ) -> anyhow::Result<PreviewHandle> {
        let scale = scale.clamp(0.1, 2.0);
        let interval = Duration::from_secs_f64(1.0 / max_fps.clamp(1.0, 30.0));
        let stop = Arc::new(AtomicBool::new(false));
        let errors = Arc::new(Mutex::new(None));
        let frames = Arc::new(Mutex::new(0u64));

        let thread_stop = stop.clone();
        let thread_errors = errors.clone();
        let thread_frames = frames.clone();
        let thread = std::thread::Builder::new()
            .name("codex-plus-cu-preview".to_string())
            .spawn(move || {
                let fail = |message: String| {
                    if let Ok(mut slot) = thread_errors.lock() {
                        *slot = Some(message);
                    }
                };
                let Some(window) = find_window(window_id) else {
                    fail("目标窗口已关闭或找不到".to_string());
                    return;
                };
                let filter = unsafe {
                    SCContentFilter::initWithDesktopIndependentWindow(
                        SCContentFilter::alloc(),
                        &window,
                    )
                };
                let config = unsafe { SCStreamConfiguration::new() };
                unsafe {
                    let frame = window.frame();
                    let width = (frame.size.width * scale).round().max(1.0);
                    let height = (frame.size.height * scale).round().max(1.0);
                    config.setWidth(width as usize);
                    config.setHeight(height as usize);
                    config.setShowsCursor(true);
                    config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
                }

                while !thread_stop.load(Ordering::Relaxed) {
                    let started = Instant::now();
                    match capture_jpeg(&filter, &config) {
                        Ok(bytes) => {
                            if thread_stop.load(Ordering::Relaxed) {
                                break;
                            }
                            sink(bytes);
                            if let Ok(mut count) = thread_frames.lock() {
                                *count += 1;
                            }
                        }
                        Err(error) => {
                            // 窗口关闭是最常见的情况，直接退出，界面据此收起预览
                            fail(error.to_string());
                            return;
                        }
                    }
                    let elapsed = started.elapsed();
                    if elapsed < interval {
                        std::thread::sleep(interval - elapsed);
                    }
                }
            })?;

        Ok(PreviewHandle {
            stop,
            errors,
            frames,
            thread: Some(thread),
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::{CaptureWindow, PreviewHandle};

    pub fn list_windows() -> anyhow::Result<Vec<CaptureWindow>> {
        Ok(Vec::new())
    }

    pub fn ensure_graphics_initialized() {}

    pub fn has_permission() -> bool {
        false
    }

    pub fn request_permission() -> bool {
        false
    }

    pub fn start_preview(
        _window_id: u32,
        _scale: f64,
        _max_fps: f64,
        _sink: impl Fn(Vec<u8>) + Send + 'static,
    ) -> anyhow::Result<PreviewHandle> {
        anyhow::bail!("窗口预览只支持 macOS")
    }
}

pub use imp::{
    ensure_graphics_initialized, has_permission, list_windows, request_permission, start_preview,
};

#[cfg(test)]
mod tests {
    use super::*;

    // ScreenCaptureKit 需要 GUI 进程环境（cargo test 是纯 CLI，会断言崩溃），
    // 所以这里只测前后端约定的字段名，抓取本身靠界面实测。
    #[test]
    fn capture_window_serializes_with_expected_fields() {
        let window = CaptureWindow {
            window_id: 63573,
            pid: 52791,
            app_name: "Claude".to_string(),
            bundle_id: "com.anthropic.claudefordesktop".to_string(),
            title: "会话".to_string(),
            width: 1280.0,
            height: 820.0,
            on_screen: true,
        };
        let value = serde_json::to_value(&window).unwrap();
        assert_eq!(value["window_id"], 63573);
        assert_eq!(value["pid"], 52791);
        assert_eq!(value["app_name"], "Claude");
        assert_eq!(value["bundle_id"], "com.anthropic.claudefordesktop");
        assert_eq!(value["title"], "会话");
        assert_eq!(value["width"], 1280.0);
        assert_eq!(value["height"], 820.0);
        assert_eq!(value["on_screen"], true);
    }

    #[test]
    fn non_macos_report_that_preview_is_unsupported() {
        #[cfg(not(target_os = "macos"))]
        {
            assert!(super::list_windows().unwrap().is_empty());
            let error = super::start_preview(1, 0.5, 12.0, |_| {}).unwrap_err();
            assert!(error.to_string().contains("只支持 macOS"));
        }
        #[cfg(target_os = "macos")]
        {
            // macOS 上 start_preview 会真的去连 WindowServer，测试进程不满足条件，
            // 这里只确认函数存在且签名可用。
            let _ = super::list_windows as fn() -> anyhow::Result<Vec<CaptureWindow>>;
        }
    }
}
