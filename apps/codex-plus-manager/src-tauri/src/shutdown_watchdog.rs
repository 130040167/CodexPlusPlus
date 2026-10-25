use std::time::Duration;

/// 只有进程真正退出才解除兜底，不能用后台清理完成代替退出确认。
pub(crate) fn arm(timeout: Duration, record_timeout: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        // 诊断写盘也可能阻塞，不能让日志阻止最终退出。
        let (recorded, completion) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            record_timeout();
            let _ = recorded.send(());
        });
        let _ = completion.recv_timeout(Duration::from_millis(250));
        std::process::exit(0);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    struct ChildGuard(Child);

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "subprocess fixture; invoked by watchdog_covers_the_entire_exit"]
    fn watchdog_child() {
        let mode = std::env::var("CODEX_PLUS_SHUTDOWN_WATCHDOG_TEST").unwrap();
        let cleanup_complete = AtomicBool::new(mode == "complete");
        let block_diagnostic = mode == "blocked_diagnostic";
        arm(Duration::from_millis(100), move || {
            if block_diagnostic {
                loop {
                    std::thread::park();
                }
            }
            println!(
                "cleanupComplete={}",
                cleanup_complete.load(Ordering::SeqCst)
            );
        });
        if mode == "normal_exit" {
            std::process::exit(23);
        }
        // 模拟最终退出事件未生效或清理阻塞；不能让测试框架自行退出掩盖问题。
        loop {
            std::thread::park();
        }
    }

    #[test]
    fn watchdog_covers_the_entire_exit() {
        for mode in [
            "incomplete",
            "complete",
            "normal_exit",
            "blocked_diagnostic",
        ] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "shutdown_watchdog::tests::watchdog_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CODEX_PLUS_SHUTDOWN_WATCHDOG_TEST", mode)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x08000000);
            }
            let mut child = ChildGuard(command.spawn().unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    break status;
                }
                assert!(
                    Instant::now() < deadline,
                    "watchdog did not terminate {mode}"
                );
                std::thread::sleep(Duration::from_millis(10));
            };
            assert_eq!(
                status.code(),
                Some(if mode == "normal_exit" { 23 } else { 0 }),
                "{mode}"
            );
            use std::io::Read;
            let mut stdout = String::new();
            child
                .0
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut stdout)
                .unwrap();
            if mode != "normal_exit" && mode != "blocked_diagnostic" {
                assert!(
                    stdout.contains(&format!("cleanupComplete={}", mode == "complete")),
                    "{stdout}"
                );
            } else {
                assert!(!stdout.contains("cleanupComplete="), "{stdout}");
            }
        }
    }
}
