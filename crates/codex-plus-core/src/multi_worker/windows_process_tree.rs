use super::{CodexInstanceIdentity, codex_identity_arguments_match};
use std::path::Path;

#[cfg(windows)]
use crate::windows_integration::{VerifiedStopProcess, bounded_process_snapshot};
#[cfg(windows)]
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum Role {
    Desktop,
    Electron,
    AppServer,
}

fn same_image(actual: &Path, expected: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .trim_start_matches(r"\\?\")
            .replace('/', "\\")
            .to_ascii_lowercase()
    };
    normalize(actual) == normalize(expected)
}

fn flag_values<'a>(args: &'a [String], flag: &str) -> Option<Vec<&'a str>> {
    let mut values = Vec::new();
    let mut tokens = args.iter().skip(1);
    while let Some(token) = tokens.next() {
        if token == flag {
            values.push(tokens.next()?.as_str());
        } else if let Some(value) = token.strip_prefix(flag).and_then(|s| s.strip_prefix('=')) {
            values.push(value);
        }
    }
    Some(values)
}

fn child_role(
    identity: &CodexInstanceIdentity,
    parent_role: Role,
    image: &Path,
    args: &[String],
) -> Option<Role> {
    // app-server 派生的终端、工具、用户代码不属于桌面的可清理进程树。
    if parent_role == Role::AppServer || args.is_empty() {
        return None;
    }
    if same_image(image, &identity.executable) {
        let types = flag_values(args, "--type")?;
        if types.len() != 1
            || !matches!(
                types[0],
                "renderer" | "gpu-process" | "utility" | "zygote" | "sandbox-helper"
            )
        {
            return None;
        }
        // Chromium 子进程可不带这些参数；如出现，必须精确指向自己的实例。
        let user = flag_values(args, "--user-data-dir")?;
        let port = flag_values(args, "--remote-debugging-port")?;
        if !user.is_empty() || !port.is_empty() {
            let mut check = args.to_vec();
            if user.is_empty() && !identity.user_data_dir.as_os_str().is_empty() {
                check.push(format!(
                    "--user-data-dir={}",
                    identity.user_data_dir.display()
                ));
            }
            if port.is_empty() {
                check.push(format!("--remote-debugging-port={}", identity.debug_port));
            }
            if !codex_identity_arguments_match(&check, identity) {
                return None;
            }
        }
        return Some(Role::Electron);
    }
    let trusted = identity
        .executable
        .parent()?
        .join("resources")
        .join("codex.exe");
    // 不接受 resources 的目录联接/符号链接跳转到安装目录之外。
    let canonical = trusted.canonicalize().ok()?;
    if !same_image(&canonical, &trusted) || !same_image(image, &canonical) {
        return None;
    }
    // 桌面内置 CLI 的角色必须是精确的第一个子命令。
    if args.get(1).map(String::as_str) == Some("app-server") {
        Some(Role::AppServer)
    } else {
        None
    }
}

fn child_lifetime_matches(
    expected_parent: u32,
    actual_parent: u32,
    parent_birth: u64,
    parent_exit: u64,
    child_birth: u64,
) -> bool {
    expected_parent == actual_parent
        && child_birth >= parent_birth
        && (parent_exit == 0 || child_birth <= parent_exit)
}

#[cfg(windows)]
pub(super) fn capture_root(
    identity: &CodexInstanceIdentity,
    terminate: bool,
) -> anyhow::Result<VerifiedStopProcess> {
    anyhow::ensure!(
        super::valid_stop_identity(identity),
        "多开实例身份字段不完整"
    );
    let process = VerifiedStopProcess::capture(identity.process_id, terminate)?
        .ok_or_else(|| anyhow::anyhow!("原代多开进程已退出"))?;
    anyhow::ensure!(
        process.birth_id == identity.process_birth_id,
        "多开进程创建身份冲突"
    );
    anyhow::ensure!(
        same_image(&process.image()?, &identity.executable),
        "多开进程映像身份冲突"
    );
    let args = process.arguments()?;
    anyhow::ensure!(
        codex_identity_arguments_match(&args, identity)
            && flag_values(&args, "--type").is_some_and(|types| types.is_empty()),
        "多开进程参数身份冲突"
    );
    anyhow::ensure!(process.is_alive()?, "原代多开进程已退出");
    Ok(process)
}

/// worker 在 ready 时捕获并持续 refresh；保留所有已收养代的句柄直到清理确认。
/// 只收养桌面自身的 Electron 角色和安装目录内的 app-server，绝不遍历用户工具。
#[cfg(windows)]
pub struct CodexInstanceProcessTree {
    identity: CodexInstanceIdentity,
    processes: Vec<(VerifiedStopProcess, Role)>,
    lineage: Vec<TreeMember>,
}

#[cfg(windows)]
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct TreeMember {
    pid: u32,
    birth: u64,
    parent: u32,
    exit: u64,
    role: Role,
}

#[cfg(windows)]
#[derive(serde::Serialize, serde::Deserialize)]
struct TreeCheckpoint {
    identity: CodexInstanceIdentity,
    members: Vec<TreeMember>,
}

#[cfg(windows)]
impl CodexInstanceProcessTree {
    const MAX_TRACKED: usize = 512;
    const MAX_SNAPSHOT: usize = 32768;
    const MAX_REFRESH_TIME: Duration = Duration::from_secs(2);
    const STOP_TIMEOUT: Duration = Duration::from_secs(8);
    const STOP_PASSES: usize = 8;

    pub fn capture(identity: &CodexInstanceIdentity) -> anyhow::Result<Self> {
        let root = capture_root(identity, true)?;
        let mut tree = Self {
            identity: identity.clone(),
            processes: vec![(root, Role::Desktop)],
            lineage: Vec::new(),
        };
        tree.refresh()?;
        Ok(tree)
    }

    pub fn refresh(&mut self) -> anyhow::Result<()> {
        self.refresh_until(Instant::now() + Self::MAX_REFRESH_TIME)
    }

    /// 交接保留已确认的祖先链，避免中间父进程退出后丢失仍活着的后代。
    pub fn save_handoff_checkpoint(&mut self, path: &Path) -> anyhow::Result<()> {
        self.refresh()?;
        let mut members = self.lineage.clone();
        for (process, role) in &self.processes {
            if let Some(member) = members.iter_mut()
                .find(|member| member.pid == process.process_id && member.birth == process.birth_id)
            {
                member.exit = process.exit_time()?;
                continue;
            }
            members.push(TreeMember {
                pid: process.process_id,
                birth: process.birth_id,
                parent: process.parent_id()?,
                exit: process.exit_time()?,
                role: *role,
            });
        }
        anyhow::ensure!(members.len() <= Self::MAX_TRACKED, "进程树交接祖先记录超过安全上限");
        let checkpoint = TreeCheckpoint { identity: self.identity.clone(), members };
        crate::settings::atomic_write(path, &serde_json::to_vec(&checkpoint)?)
    }

    pub fn restore_handoff_checkpoint(&mut self, path: &Path) -> anyhow::Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        anyhow::ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 262144,
            "进程树交接记录类型或大小无效"
        );
        let checkpoint: TreeCheckpoint = serde_json::from_slice(&std::fs::read(path)?)?;
        anyhow::ensure!(
            checkpoint.identity == self.identity
                && !checkpoint.members.is_empty()
                && checkpoint.members.len() <= Self::MAX_TRACKED,
            "进程树交接记录不属于当前实例"
        );
        let root = &checkpoint.members[0];
        anyhow::ensure!(
            root.pid == self.identity.process_id
                && root.birth == self.identity.process_birth_id
                && root.role == Role::Desktop,
            "进程树交接记录缺少原代根"
        );
        let mut restored = Vec::new();
        let mut seen = std::collections::HashSet::from([root.pid]);
        for (index, member) in checkpoint.members.iter().enumerate().skip(1) {
            anyhow::ensure!(seen.insert(member.pid), "进程树交接记录包含重复 PID");
            let parent = checkpoint.members[..index].iter()
                .find(|parent| parent.pid == member.parent)
                .ok_or_else(|| anyhow::anyhow!("进程树交接记录祖先链断裂"))?;
            anyhow::ensure!(
                parent.role != Role::AppServer
                    && member.role != Role::Desktop
                    && child_lifetime_matches(
                        parent.pid, member.parent, parent.birth, parent.exit, member.birth
                    ),
                "进程树交接记录包含无效后代"
            );
            let Some(process) = VerifiedStopProcess::capture(member.pid, true)? else {
                continue;
            };
            // PID 已复用表示原代退出；不收养或结束复用 PID 的新进程。
            if process.birth_id != member.birth || !process.is_alive()? {
                continue;
            }
            anyhow::ensure!(
                process.parent_id()? == member.parent
                    && child_role(&self.identity, parent.role, &process.image()?, &process.arguments()?)
                        == Some(member.role),
                "进程树交接后代身份冲突"
            );
            if !self.processes.iter().any(|(known, _)| known.process_id == member.pid) {
                restored.push((process, member.role));
            }
        }
        anyhow::ensure!(
            self.processes.len() + restored.len() <= Self::MAX_TRACKED,
            "续接后的自有进程数量超过安全上限"
        );
        self.processes.extend(restored);
        self.lineage = checkpoint.members;
        self.refresh()
    }

    fn refresh_until(&mut self, deadline: Instant) -> anyhow::Result<()> {
        use std::collections::HashSet;
        let snapshot = bounded_process_snapshot(Self::MAX_SNAPSHOT)?;
        let mut known: HashSet<u32> = self.processes.iter().map(|(p, _)| p.process_id).collect();
        let mut index = 0;
        while index < self.processes.len() {
            anyhow::ensure!(Instant::now() < deadline, "多开进程树追踪超时");
            let (parent, parent_role) = &self.processes[index];
            let (parent_pid, parent_birth, parent_role) =
                (parent.process_id, parent.birth_id, *parent_role);
            if parent_role == Role::AppServer {
                index += 1;
                continue;
            }
            for candidate in snapshot
                .iter()
                .filter(|p| p.parent_process_id == parent_pid)
            {
                anyhow::ensure!(Instant::now() < deadline, "多开进程树追踪超时");
                if known.contains(&candidate.process_id) {
                    continue;
                }
                // 先按快照文件名排除 shell 等无关工具，随后仍从持有句柄核验完整映像。
                let desktop_name = self.identity.executable.file_name().unwrap_or_default();
                if !candidate
                    .exe_file
                    .eq_ignore_ascii_case(&desktop_name.to_string_lossy())
                    && !candidate.exe_file.eq_ignore_ascii_case("codex.exe")
                {
                    continue;
                }
                let Some(child) = VerifiedStopProcess::capture(candidate.process_id, true)? else {
                    continue;
                };
                if !child.is_alive()? {
                    continue;
                }
                // parent_exit 必须在捕获子代后读取，防止父代先退出、PID 后复用。
                if !child_lifetime_matches(
                    parent_pid,
                    child.parent_id()?,
                    parent_birth,
                    self.processes[index].0.exit_time()?,
                    child.birth_id,
                ) {
                    continue;
                }
                let facts = child
                    .image()
                    .and_then(|image| child.arguments().map(|args| (image, args)));
                let (image, args) = match facts {
                    Ok(facts) => facts,
                    Err(_) if !child.is_alive()? => continue,
                    Err(error) => return Err(error),
                };
                let Some(role) = child_role(&self.identity, parent_role, &image, &args) else {
                    continue;
                };
                anyhow::ensure!(
                    self.processes.len() < Self::MAX_TRACKED,
                    "多开自有进程数量超过安全上限"
                );
                known.insert(child.process_id);
                self.processes.push((child, role));
            }
            index += 1;
        }
        Ok(())
    }

    pub fn cleanup_after_exit(mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.processes[0].0.is_alive()?,
            "原代 Codex 仍存活，拒绝残留清理"
        );
        self.stop_and_wait()
    }

    pub(super) fn stop_and_wait(&mut self) -> anyhow::Result<()> {
        // 从第一轮捕获至最后一次退出确认均持有原句柄。共享总时间，避免 N * timeout。
        let deadline = Instant::now() + Self::STOP_TIMEOUT;
        self.refresh_until(deadline)?;
        self.processes[0].0.terminate()?;
        for _ in 0..Self::STOP_PASSES {
            anyhow::ensure!(Instant::now() < deadline, "多开自有进程清理超时");
            self.refresh_until(deadline)?;
            for (process, _) in self.processes.iter().rev() {
                anyhow::ensure!(Instant::now() < deadline, "多开自有进程清理超时");
                process.terminate()?;
            }
            loop {
                let mut alive = false;
                for (process, _) in &self.processes {
                    alive |= process.is_alive()?;
                }
                if !alive {
                    break;
                }
                anyhow::ensure!(Instant::now() < deadline, "多开自有进程尚未确认退出");
                std::thread::sleep(Duration::from_millis(20));
            }
            let before = self.processes.len();
            self.refresh_until(deadline)?;
            if self.processes.len() == before {
                return Ok(());
            }
        }
        anyhow::bail!("多开自有进程持续派生，未确认清理完成")
    }
}

#[cfg(all(test, windows))]
#[path = "windows_process_tree_tests.rs"]
mod native_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(root: &Path) -> CodexInstanceIdentity {
        CodexInstanceIdentity {
            instance_id: "own".into(),
            generation: 1,
            debug_port: 9341,
            helper_port: 9342,
            user_data_dir: root.into(),
            browser_id: "browser-own".into(),
            process_id: 42,
            process_birth_id: 100,
            executable: root.join("ChatGPT.exe"),
        }
    }

    #[test]
    fn only_desktop_electron_roles_and_trusted_app_server_are_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        std::fs::create_dir(dir.path().join("resources")).unwrap();
        let cli = dir.path().join("resources/codex.exe");
        std::fs::write(&cli, "").unwrap();
        let cli = cli.canonicalize().unwrap();
        for role in [
            "renderer",
            "gpu-process",
            "utility",
            "zygote",
            "sandbox-helper",
        ] {
            let args = vec!["ChatGPT.exe".into(), format!("--type={role}")];
            assert_eq!(
                child_role(&identity, Role::Desktop, &identity.executable, &args),
                Some(Role::Electron)
            );
            assert_eq!(
                child_role(&identity, Role::AppServer, &identity.executable, &args),
                None
            );
        }
        let args = vec![
            "codex.exe".into(),
            "app-server".into(),
            "--analytics-default-enabled".into(),
        ];
        assert_eq!(
            child_role(&identity, Role::Desktop, &cli, &args),
            Some(Role::AppServer)
        );
        assert_eq!(child_role(&identity, Role::AppServer, &cli, &args), None);
        assert_eq!(
            child_role(
                &identity,
                Role::Desktop,
                &dir.path().join("codex.exe"),
                &args
            ),
            None
        );
        assert_eq!(
            child_role(
                &identity,
                Role::Desktop,
                &cli,
                &["codex.exe".into(), "exec".into()]
            ),
            None
        );
        assert_eq!(
            child_role(
                &identity,
                Role::Desktop,
                Path::new("cmd.exe"),
                &["cmd.exe".into(), "/c".into(), "tool".into()]
            ),
            None
        );
    }

    #[test]
    fn independent_window_neighbor_and_ambiguous_roles_are_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let identity = identity(dir.path());
        for args in [
            vec!["ChatGPT.exe".into()],
            vec!["ChatGPT.exe".into(), "--type=browser".into()],
            vec![
                "ChatGPT.exe".into(),
                "--type=renderer".into(),
                "--type=utility".into(),
            ],
            vec!["ChatGPT.exe".into(), "--type".into()],
            vec![
                "ChatGPT.exe".into(),
                "--type=renderer".into(),
                "--remote-debugging-port=9343".into(),
            ],
            vec![
                "ChatGPT.exe".into(),
                "--type=renderer".into(),
                "--user-data-dir=missing-neighbor".into(),
            ],
        ] {
            assert_eq!(
                child_role(&identity, Role::Desktop, &identity.executable, &args),
                None
            );
        }
    }

    #[test]
    fn parent_birth_exit_interval_rejects_recycled_parent_and_unrelated_child() {
        assert!(child_lifetime_matches(10, 10, 100, 200, 150));
        assert!(child_lifetime_matches(10, 10, 100, 0, 250));
        assert!(!child_lifetime_matches(10, 11, 100, 0, 150));
        assert!(!child_lifetime_matches(10, 10, 100, 200, 201));
        assert!(!child_lifetime_matches(10, 10, 100, 0, 99));
    }
}
