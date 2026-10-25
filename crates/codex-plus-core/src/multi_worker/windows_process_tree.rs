use super::VerifiedStopProcess;
use crate::windows_integration::{request_process_close, try_enumerate_processes};
use std::collections::HashSet;
use std::os::windows::io::AsRawHandle;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{GetProcessTimes, WaitForSingleObject};

/// 捕获后始终持有同代句柄；父进程退出后仍可核对其生命周期，排除 PID 复用。
pub(crate) struct VerifiedStopTree {
    processes: Vec<VerifiedStopProcess>,
}

impl VerifiedStopProcess {
    fn is_alive(&self) -> anyhow::Result<bool> {
        match unsafe { WaitForSingleObject(HANDLE(self.handle.as_raw_handle()), 0) } {
            WAIT_OBJECT_0 => Ok(false),
            WAIT_TIMEOUT => Ok(true),
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }

    fn exit_time(&self) -> anyhow::Result<u64> {
        let mut birth = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        unsafe {
            GetProcessTimes(
                HANDLE(self.handle.as_raw_handle()),
                &mut birth,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        }?;
        Ok(((exit.dwHighDateTime as u64) << 32) | exit.dwLowDateTime as u64)
    }
}

fn child_lifetime_matches(parent_birth: u64, parent_exit: u64, child_birth: u64) -> bool {
    child_birth >= parent_birth && (parent_exit == 0 || child_birth <= parent_exit)
}

impl VerifiedStopTree {
    pub(crate) fn capture(root: VerifiedStopProcess) -> anyhow::Result<Self> {
        let mut tree = Self {
            processes: vec![root],
        };
        tree.refresh()?;
        Ok(tree)
    }

    pub(super) fn refresh(&mut self) -> anyhow::Result<usize> {
        let snapshot = try_enumerate_processes()?;
        let mut known: HashSet<u32> = self.processes.iter().map(|p| p.process_id).collect();
        let before = known.len();
        let mut index = 0;
        while index < self.processes.len() {
            let parent = &self.processes[index];
            let parent_pid = parent.process_id;
            let parent_birth = parent.birth_id;
            let parent_exit = parent.exit_time()?;
            for child in snapshot
                .iter()
                .filter(|p| p.parent_process_id == parent_pid)
            {
                if known.contains(&child.process_id) {
                    continue;
                }
                let Some(executable) = &child.executable_path else {
                    anyhow::ensure!(
                        matches!(
                            crate::watcher::inspect_process_instance(child.process_id),
                            crate::watcher::ProcessInstanceState::NotRunning
                        ),
                        "无法核验 Codex 子进程 {} 的映像，未确认清理完成",
                        child.process_id
                    );
                    continue;
                };
                let captured =
                    match VerifiedStopProcess::capture(child.process_id, executable, None) {
                        Ok(captured) => captured,
                        Err(error)
                            if matches!(
                                crate::watcher::inspect_process_instance(child.process_id),
                                crate::watcher::ProcessInstanceState::NotRunning
                            ) =>
                        {
                            let _ = error;
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                let Some(captured) = captured else { continue };
                if !child_lifetime_matches(parent_birth, parent_exit, captured.birth_id) {
                    continue;
                }
                anyhow::ensure!(
                    self.processes.len() < 4096,
                    "Codex 子进程数量超过安全清理上限"
                );
                known.insert(child.process_id);
                self.processes.push(captured);
            }
            index += 1;
        }
        Ok(known.len() - before)
    }

    pub(super) fn root_exited(&self) -> anyhow::Result<bool> {
        Ok(!self.processes[0].is_alive()?)
    }

    pub(crate) fn stop_and_wait(mut self) -> anyhow::Result<()> {
        let root = &self.processes[0];
        if root.is_alive()? {
            request_process_close(root.process_id);
            let deadline = Instant::now()
                + Duration::from_millis(crate::watcher::RESTART_STOP_WAIT_TIMEOUT_MS);
            while self.processes[0].is_alive()? && Instant::now() < deadline {
                self.refresh()?;
                std::thread::sleep(Duration::from_millis(100));
            }
        }
        // 先固定所有当前后代，再停止主进程，阻止它继续派生后台任务。
        self.refresh()?;
        if self.processes.iter().any(|process| process.is_alive().unwrap_or(true)) {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "launcher.verified_tree_stop_fallback",
                serde_json::json!({
                    "root_pid": self.processes[0].process_id,
                    "tracked_process_count": self.processes.len(),
                }),
            );
        }
        self.processes[0].terminate_and_wait()?;
        for _ in 0..8 {
            self.refresh()?;
            for process in self.processes.iter().rev() {
                process.terminate_and_wait()?;
            }
            if self.refresh()? == 0 {
                anyhow::ensure!(
                    self.processes
                        .iter()
                        .all(|p| p.is_alive().is_ok_and(|alive| !alive)),
                    "Codex 进程树尚未确认退出，未启动新实例"
                );
                return Ok(());
            }
        }
        anyhow::bail!("Codex 子进程持续变化，未确认清理完成")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycled_parent_pid_does_not_adopt_a_later_child() {
        assert!(child_lifetime_matches(100, 200, 150));
        assert!(child_lifetime_matches(100, 0, 250));
        assert!(!child_lifetime_matches(100, 200, 250));
        assert!(!child_lifetime_matches(100, 0, 50));
    }

    #[test]
    fn stopped_root_does_not_leave_its_captured_child_or_stop_a_neighbor() {
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let executable = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let marker = temp.path().join("child.pid");
        let script = temp.path().join("parent.ps1");
        std::fs::write(&script, r#"param([string]$Marker)
$child = Start-Process -FilePath (Join-Path $PSHOME 'powershell.exe') -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60' -WindowStyle Hidden -PassThru
[IO.File]::WriteAllText($Marker, [string]$child.Id)
Start-Sleep -Seconds 60
"#).unwrap();
        let mut root = ChildGuard(
            std::process::Command::new(&executable)
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ])
                .arg(&script)
                .arg(&marker)
                .spawn()
                .unwrap(),
        );
        let mut neighbor = ChildGuard(
            std::process::Command::new(&executable)
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Start-Sleep -Seconds 60",
                ])
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let child_pid: u32 = loop {
            if let Ok(value) = std::fs::read_to_string(&marker)
                && let Ok(pid) = value.parse()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "child did not start");
            std::thread::sleep(Duration::from_millis(50));
        };
        let child = VerifiedStopProcess::capture(child_pid, &executable, None)
            .unwrap()
            .unwrap();
        struct ProcessGuard(VerifiedStopProcess);
        impl Drop for ProcessGuard {
            fn drop(&mut self) {
                let _ = self.0.terminate_and_wait();
            }
        }
        let child = ProcessGuard(child);
        let root_process = VerifiedStopProcess::capture(root.0.id(), &executable, None)
            .unwrap()
            .unwrap();
        let tree = VerifiedStopTree::capture(root_process).unwrap();
        // 模拟主进程已退出、子进程仍存活；清理必须使用捕获的生命周期。
        root.0.kill().unwrap();
        root.0.wait().unwrap();
        assert!(child.0.is_alive().unwrap());
        tree.stop_and_wait().unwrap();
        assert!(!child.0.is_alive().unwrap());
        assert!(neighbor.0.try_wait().unwrap().is_none());
    }
}
