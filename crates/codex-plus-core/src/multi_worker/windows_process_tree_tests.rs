use super::*;
use crate::multi_worker::{
    codex_instance_needs_worker, codex_instance_process_exited, terminate_verified_codex_instance,
    verified_worker_is_running,
};
use crate::windows_integration::{CREATE_NO_WINDOW, child_process_birth_id};
use serde::{Deserialize, Serialize};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

fn short_path(path: &Path) -> PathBuf {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows::Win32::Storage::FileSystem::GetShortPathNameW;
    let input: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut output = vec![0_u16; 32768];
    let length =
        unsafe { GetShortPathNameW(windows::core::PCWSTR(input.as_ptr()), Some(&mut output)) }
            as usize;
    assert!(
        length > 0 && length < output.len(),
        "short path: {}",
        std::io::Error::last_os_error()
    );
    PathBuf::from(std::ffi::OsString::from_wide(&output[..length]))
}

// 只串行化会拷贝并派生原生进程的测试；纯规则和 CDP 单元测试仍并行。
static FIXTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const FIXTURE_NAME: &str = "multi_worker::windows_process_tree::native_tests::fixture_process";
const FIXTURE_ENV: &str = "CODEX_PLUS_CORE_STOP_FIXTURE_DIR";
const MODES: &[&str] = &[
    "root",
    "renderer",
    "gpu",
    "appserver",
    "independent",
    "shell",
    "user-tool",
    "neighbor",
];

#[derive(Serialize, Deserialize)]
struct Record {
    pid: u32,
    birth: u64,
    image: PathBuf,
}

fn record_child(dir: &Path, mode: &str, child: &Child, image: &Path) {
    let record = Record {
        pid: child.id(),
        birth: child_process_birth_id(child).unwrap(),
        image: image.canonicalize().unwrap(),
    };
    let pending = dir.join(format!("{mode}.json.pending"));
    std::fs::write(&pending, serde_json::to_vec(&record).unwrap())
        .unwrap_or_else(|error| panic!("write {mode} record: {error}"));
    std::fs::rename(&pending, dir.join(format!("{mode}.json")))
        .unwrap_or_else(|error| panic!("publish {mode} record: {error}"));
}

fn capture_logs(command: &mut Command, dir: &Path, mode: &str) {
    let stdout = std::fs::File::create(dir.join(format!("{mode}.stdout.log"))).unwrap();
    let stderr = std::fs::File::create(dir.join(format!("{mode}.stderr.log"))).unwrap();
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
}

fn log_tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let _ = file.seek(SeekFrom::Start(length.saturating_sub(2048)));
    let mut bytes = Vec::new();
    let _ = file.take(2048).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

// 返回 (已就绪, 确认失败, 可打印状态)。缺记录可等待，已退出/身份冲突则立即失败。
fn fixture_status(dir: &Path, mode: &str) -> (bool, bool, String) {
    let ready = mode == "shell" || dir.join(format!("{mode}.ready")).is_file();
    let bytes = match std::fs::read(dir.join(format!("{mode}.json"))) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                false,
                false,
                format!("{mode}: missing record, ready={ready}"),
            );
        }
        Err(error) => return (false, true, format!("{mode}: record read failed: {error}")),
    };
    let record: Record = match serde_json::from_slice(&bytes) {
        Ok(record) => record,
        Err(error) => return (false, true, format!("{mode}: invalid record: {error}")),
    };
    let state = (|| -> anyhow::Result<(bool, String)> {
        let Some(process) = VerifiedStopProcess::capture(record.pid, false)? else {
            return Ok((false, "process missing; exit code unavailable".into()));
        };
        anyhow::ensure!(
            process.birth_id == record.birth,
            "recorded PID generation changed"
        );
        if !process.is_alive()? {
            return Ok((
                false,
                match process.exit_code()? {
                    Some(code) => format!("exited code={code} (0x{code:08X})"),
                    None => "exit status uncertain".into(),
                },
            ));
        }
        anyhow::ensure!(
            same_image(&process.image()?, &record.image),
            "recorded image changed"
        );
        Ok((true, "running".into()))
    })();
    match state {
        Ok((alive, state)) => (
            ready && alive,
            !alive,
            format!(
                "{mode}: pid={}, birth={}, ready={ready}, {state}",
                record.pid, record.birth
            ),
        ),
        Err(error) => (
            false,
            true,
            format!(
                "{mode}: pid={}, birth={}, ready={ready}, probe failed: {error:#}",
                record.pid, record.birth,
            ),
        ),
    }
}

fn wait_for_fixtures(dir: &Path, modes: &[&str], timeout: Duration) {
    let started = Instant::now();
    loop {
        let statuses: Vec<_> = modes.iter().map(|mode| fixture_status(dir, mode)).collect();
        if statuses.iter().all(|(ready, _, _)| *ready) {
            return;
        }
        if statuses.iter().any(|(_, failed, _)| *failed) || started.elapsed() >= timeout {
            let mut diagnostics = statuses
                .into_iter()
                .map(|(_, _, line)| line)
                .collect::<Vec<_>>()
                .join("\n");
            for mode in modes {
                for stream in ["stdout", "stderr"] {
                    let tail = log_tail(&dir.join(format!("{mode}.{stream}.log")));
                    if !tail.trim().is_empty() {
                        diagnostics.push_str(&format!("\n--- {mode} {stream} tail ---\n{tail}"));
                    }
                }
            }
            panic!(
                "hidden lifecycle fixtures failed after {:?}, directory={}\n{diagnostics}",
                started.elapsed(),
                dir.display(),
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn spawn_fixture(dir: &Path, mode: &str, port: u16) -> Child {
    let image = dir.join(if mode == "appserver" {
        "resources/codex.exe"
    } else {
        "Desktop fixture.exe"
    });
    let mut command = Command::new(&image);
    // 拷贝的测试二进制仍从原测试目录解析可选的运行时 DLL，不改全局环境。
    let original = std::env::current_exe().unwrap();
    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let search_path = std::env::join_paths(
        original
            .parent()
            .into_iter()
            .map(Path::to_path_buf)
            .chain(std::env::split_paths(&inherited_path)),
    )
    .unwrap();
    command.env("PATH", search_path);
    if mode == "appserver" {
        command.arg("app-server");
    }
    command
        .args(["--ignored", "--exact", FIXTURE_NAME, "--nocapture", "--"])
        .env(FIXTURE_ENV, dir)
        .env("CODEX_PLUS_CORE_STOP_FIXTURE_MODE", mode)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null());
    capture_logs(&mut command, dir, mode);
    match mode {
        "root" | "default-main" => {
            command.arg(format!("--remote-debugging-port={port}"));
            if mode == "root" {
                command.arg(format!(
                    "--user-data-dir={}",
                    dir.join("user data").display()
                ));
            }
        }
        "renderer" | "user-tool" => {
            command.arg("--type=renderer");
        }
        "gpu" => {
            command.arg("--type=gpu-process");
        }
        _ => {}
    }
    let child = command.spawn().unwrap_or_else(|error| {
        panic!(
            "spawn fixture mode={mode}, image={}: {error}",
            image.display()
        )
    });
    record_child(dir, mode, &child, &image);
    child
}

/// 仅由本模块显式运行；测试二进制扮演 Electron/CLI，不启动真实桌面或模型。
#[test]
#[ignore]
fn fixture_process() {
    let Some(dir) = std::env::var_os(FIXTURE_ENV).map(PathBuf::from) else {
        return;
    };
    let mode = std::env::var("CODEX_PLUS_CORE_STOP_FIXTURE_MODE").unwrap();
    let mut children = Vec::new();
    if mode == "root" {
        for child_mode in ["renderer", "appserver", "independent"] {
            children.push(spawn_fixture(&dir, child_mode, 0));
        }
        let shell = PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let mut command = Command::new(&shell);
        command
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null());
        capture_logs(&mut command, &dir, "shell");
        let child = command.spawn().unwrap();
        record_child(&dir, "shell", &child, &shell);
        children.push(child);
    } else if mode == "renderer" {
        children.push(spawn_fixture(&dir, "gpu", 0));
    } else if mode == "appserver" {
        // 即使用户工具用了相同 Electron 映像和 --type，也不能越过 app-server 边界收养。
        children.push(spawn_fixture(&dir, "user-tool", 0));
    }
    std::fs::write(dir.join(format!("{mode}.ready")), "ready").unwrap();
    std::thread::sleep(Duration::from_secs(30));
    for mut child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct FixtureGroup {
    dir: tempfile::TempDir,
    root: Option<Child>,
    neighbor: Option<Child>,
}

impl FixtureGroup {
    fn start(port: u16) -> Self {
        Self::start_with_paths(port, false)
    }

    fn start_with_paths(port: u16, use_short_paths: bool) -> Self {
        let mut group = Self {
            dir: tempfile::Builder::new()
                .prefix("Codex process safety fixture ")
                .tempdir()
                .unwrap(),
            root: None,
            neighbor: None,
        };
        let dir = group.dir.path();
        std::fs::create_dir(dir.join("resources")).unwrap();
        std::fs::create_dir(dir.join("user data")).unwrap();
        let image = std::env::current_exe().unwrap();
        std::fs::copy(&image, dir.join("Desktop fixture.exe")).unwrap();
        std::fs::copy(&image, dir.join("resources/codex.exe")).unwrap();
        let launch_dir = if use_short_paths {
            let short = short_path(&dir.canonicalize().unwrap());
            assert!(
                !same_resolved_path(&short, &dir.canonicalize().unwrap()),
                "regression requires a real 8.3 alias"
            );
            short
        } else {
            dir.to_path_buf()
        };
        group.root = Some(spawn_fixture(&launch_dir, "root", port));
        group.neighbor = Some(spawn_fixture(&launch_dir, "neighbor", 0));
        wait_for_fixtures(dir, MODES, Duration::from_secs(10));
        group
    }

    fn process(&self, mode: &str) -> VerifiedStopProcess {
        let record: Record = serde_json::from_slice(
            &std::fs::read(self.dir.path().join(format!("{mode}.json"))).unwrap(),
        )
        .unwrap();
        let process = VerifiedStopProcess::capture(record.pid, true)
            .unwrap()
            .unwrap();
        assert_eq!(record.birth, process.birth_id);
        assert!(same_image(&process.image().unwrap(), &record.image));
        process
    }

    fn identity(&self, port: u16) -> CodexInstanceIdentity {
        let root = self.process("root");
        CodexInstanceIdentity {
            instance_id: "fixture-own".into(),
            generation: 1,
            debug_port: port,
            helper_port: if port == 65535 { 65534 } else { port + 1 },
            user_data_dir: self.dir.path().join("user data").canonicalize().unwrap(),
            browser_id: "fixture-browser".into(),
            process_id: root.process_id,
            process_birth_id: root.birth_id,
            executable: root.image().unwrap(),
        }
    }
}

fn wait_for_exit(process: &VerifiedStopProcess) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while process.is_alive().unwrap() {
        assert!(Instant::now() < deadline, "fixture process did not exit");
        std::thread::sleep(Duration::from_millis(10));
    }
}

impl Drop for FixtureGroup {
    fn drop(&mut self) {
        // 先停止自建根以封住派生，再核对每个记录的 birth/image，回收所有保留正反例。
        if let Some(child) = &mut self.root {
            let _ = child.kill();
            let _ = child.wait();
        }
        for mode in MODES {
            let Ok(bytes) = std::fs::read(self.dir.path().join(format!("{mode}.json"))) else {
                continue;
            };
            let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
                continue;
            };
            let Ok(Some(process)) = VerifiedStopProcess::capture(record.pid, true) else {
                continue;
            };
            if process.birth_id == record.birth
                && process
                    .image()
                    .is_ok_and(|image| same_image(&image, &record.image))
            {
                let _ = process.terminate();
                let deadline = Instant::now() + Duration::from_secs(2);
                while process.is_alive().unwrap_or(false) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        if let Some(child) = &mut self.neighbor {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn short_path_launch_matches_long_identity_and_preserves_unowned_processes() {
    let _guard = FIXTURE_LOCK.lock().await;
    let port = unused_port();
    let fixtures = FixtureGroup::start_with_paths(port, true);
    let mut identity = fixtures.identity(port);
    identity.executable = fixtures
        .dir
        .path()
        .join("Desktop fixture.exe")
        .canonicalize()
        .unwrap();
    let short_image = short_path(&identity.executable);
    assert!(same_image(&short_image, &identity.executable));
    assert!(same_image(&identity.executable, &short_image));
    let cli = fixtures
        .dir
        .path()
        .join("resources/codex.exe")
        .canonicalize()
        .unwrap();
    assert_eq!(
        child_role(
            &identity,
            Role::Desktop,
            &short_path(&cli),
            &["codex.exe".into(), "app-server".into()]
        ),
        Some(Role::AppServer)
    );
    let mut tree = CodexInstanceProcessTree::capture(&identity).unwrap();
    assert_eq!(tree.processes.len(), 4);
    let owned: Vec<_> = ["root", "renderer", "gpu", "appserver"]
        .iter()
        .map(|mode| fixtures.process(mode))
        .collect();
    let preserved: Vec<_> = ["shell", "independent", "user-tool", "neighbor"]
        .iter()
        .map(|mode| fixtures.process(mode))
        .collect();
    tree.stop_and_wait().unwrap();
    assert!(owned.iter().all(|process| !process.is_alive().unwrap()));
    assert!(preserved.iter().all(|process| process.is_alive().unwrap()));
}

#[tokio::test]
async fn concurrent_stops_confirm_exit_of_the_same_owned_tree() {
    let _guard = FIXTURE_LOCK.lock().await;
    let port = unused_port();
    let fixtures = FixtureGroup::start(port);
    let identity = fixtures.identity(port);
    let mut first = CodexInstanceProcessTree::capture(&identity).unwrap();
    let mut second = CodexInstanceProcessTree::capture(&identity).unwrap();
    let preserved: Vec<_> = ["shell", "independent", "user-tool", "neighbor"]
        .iter()
        .map(|mode| fixtures.process(mode))
        .collect();
    std::thread::scope(|scope| {
        let one = scope.spawn(|| first.stop_and_wait());
        let two = scope.spawn(|| second.stop_and_wait());
        one.join().unwrap().unwrap();
        two.join().unwrap().unwrap();
    });
    assert!(codex_instance_process_exited(&identity));
    assert!(preserved.iter().all(|process| process.is_alive().unwrap()));
}

#[tokio::test]
async fn ready_tracking_cleans_owned_orphans_and_preserves_shell_tools_and_neighbors() {
    let _fixture_lock = FIXTURE_LOCK.lock().await;
    let port = unused_port();
    let fixtures = FixtureGroup::start(port);
    let identity = fixtures.identity(port);
    let mut tree = CodexInstanceProcessTree::capture(&identity).unwrap();
    assert_eq!(tree.processes.len(), 4);
    assert!(tree.cleanup_after_exit().is_err());
    tree = CodexInstanceProcessTree::capture(&identity).unwrap();
    let owned: Vec<_> = ["root", "renderer", "gpu", "appserver"]
        .iter()
        .map(|mode| fixtures.process(mode))
        .collect();
    let preserved: Vec<_> = ["shell", "independent", "user-tool", "neighbor"]
        .iter()
        .map(|mode| fixtures.process(mode))
        .collect();
    assert!(codex_instance_needs_worker(&identity).await.unwrap());
    owned[0].terminate().unwrap();
    wait_for_exit(&owned[0]);
    assert!(codex_instance_process_exited(&identity));
    assert!(!codex_instance_needs_worker(&identity).await.unwrap());
    assert!(owned[1].is_alive().unwrap());
    tree.refresh().unwrap();
    tree.cleanup_after_exit().unwrap();
    assert!(owned.iter().all(|p| !p.is_alive().unwrap()));
    assert!(preserved.iter().all(|p| p.is_alive().unwrap()));
}

#[tokio::test]
async fn handoff_checkpoint_preserves_orphans_across_two_worker_replacements() {
    let _fixture_lock = FIXTURE_LOCK.lock().await;
    let port = unused_port();
    let fixtures = FixtureGroup::start(port);
    let identity = fixtures.identity(port);
    let mut original = CodexInstanceProcessTree::capture(&identity).unwrap();
    let renderer = fixtures.process("renderer");
    let gpu = fixtures.process("gpu");
    let preserved: Vec<_> = ["shell", "independent", "user-tool", "neighbor"]
        .iter()
        .map(|mode| fixtures.process(mode))
        .collect();
    renderer.terminate().unwrap();
    wait_for_exit(&renderer);
    assert!(gpu.is_alive().unwrap());
    let checkpoint = fixtures.dir.path().join("handoff.json");
    original.save_handoff_checkpoint(&checkpoint).unwrap();
    drop(original);
    for _ in 0..2 {
        let mut resumed = CodexInstanceProcessTree::capture(&identity).unwrap();
        assert!(
            !resumed
                .processes
                .iter()
                .any(|(p, _)| p.process_id == gpu.process_id)
        );
        resumed.restore_handoff_checkpoint(&checkpoint).unwrap();
        assert!(
            resumed
                .processes
                .iter()
                .any(|(p, _)| p.process_id == gpu.process_id)
        );
        resumed.save_handoff_checkpoint(&checkpoint).unwrap();
    }
    let mut final_worker = CodexInstanceProcessTree::capture(&identity).unwrap();
    final_worker
        .restore_handoff_checkpoint(&checkpoint)
        .unwrap();
    let mut wrong: TreeCheckpoint =
        serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    wrong.identity.generation += 1;
    std::fs::write(&checkpoint, serde_json::to_vec(&wrong).unwrap()).unwrap();
    assert!(
        final_worker
            .restore_handoff_checkpoint(&checkpoint)
            .is_err()
    );
    assert!(gpu.is_alive().unwrap());
    final_worker.stop_and_wait().unwrap();
    assert!(!gpu.is_alive().unwrap());
    assert!(preserved.iter().all(|p| p.is_alive().unwrap()));
}

#[tokio::test]
async fn wrong_birth_image_arguments_and_browser_conflict_never_stop_the_root() {
    let _fixture_lock = FIXTURE_LOCK.lock().await;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    // 专属服务才能通过 drop 模拟端口失联；默认池化服务会继续返回 HTTP 404。
    let server = MockServer::builder().start().await;
    let port = server.address().port();
    let fixtures = FixtureGroup::start(port);
    let identity = fixtures.identity(port);
    let root = fixtures.process("root");
    let mut wrong = identity.clone();
    wrong.process_birth_id += 1;
    assert!(terminate_verified_codex_instance(&wrong).await.is_err());
    assert!(root.is_alive().unwrap());
    wrong = identity.clone();
    wrong.executable = fixtures.dir.path().join("resources/codex.exe");
    assert!(terminate_verified_codex_instance(&wrong).await.is_err());
    assert!(codex_instance_needs_worker(&wrong).await.is_err());
    assert!(root.is_alive().unwrap());
    wrong = identity.clone();
    wrong.user_data_dir = fixtures.dir.path().canonicalize().unwrap();
    assert!(terminate_verified_codex_instance(&wrong).await.is_err());
    assert!(codex_instance_needs_worker(&wrong).await.is_err());
    assert!(root.is_alive().unwrap());
    wrong = identity.clone();
    wrong.debug_port = if port == 65535 { port - 1 } else { port + 1 };
    assert!(terminate_verified_codex_instance(&wrong).await.is_err());
    assert!(root.is_alive().unwrap());
    Mock::given(path("/json/version"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "Browser": "fixture",
            "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/browser/neighbor")
        })))
        .mount(&server)
        .await;
    assert!(terminate_verified_codex_instance(&identity).await.is_err());
    assert!(codex_instance_needs_worker(&identity).await.is_err());
    assert!(!codex_instance_process_exited(&identity));
    assert!(root.is_alive().unwrap());
    drop(server);
    let deadline = Instant::now() + Duration::from_secs(2);
    while std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(20),
    )
    .is_ok()
    {
        assert!(
            Instant::now() < deadline,
            "fixture CDP listener did not close"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(codex_instance_needs_worker(&identity).await.unwrap());
    let preserved: Vec<_> = ["shell", "independent", "user-tool", "neighbor"]
        .iter()
        .map(|mode| fixtures.process(mode))
        .collect();
    assert!(terminate_verified_codex_instance(&identity).await.unwrap());
    assert!(codex_instance_process_exited(&identity));
    assert!(preserved.iter().all(|p| p.is_alive().unwrap()));
}

#[tokio::test]
async fn default_main_without_user_data_can_be_captured_and_stopped_by_saved_identity() {
    let _fixture_lock = FIXTURE_LOCK.lock().await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        std::env::current_exe().unwrap(),
        dir.path().join("Desktop fixture.exe"),
    )
    .unwrap();
    struct Guard(Child);
    impl Drop for Guard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let port = unused_port();
    let child = Guard(spawn_fixture(dir.path(), "default-main", port));
    wait_for_fixtures(dir.path(), &["default-main"], Duration::from_secs(5));
    let identity = CodexInstanceIdentity {
        instance_id: "main".into(),
        generation: 1,
        debug_port: port,
        helper_port: 9342,
        user_data_dir: PathBuf::new(),
        browser_id: "fixture-browser".into(),
        process_id: child.0.id(),
        process_birth_id: child_process_birth_id(&child.0).unwrap(),
        executable: dir
            .path()
            .join("Desktop fixture.exe")
            .canonicalize()
            .unwrap(),
    };
    // CDP 已断连，仍可从保存的同代身份捕获停止树。
    let tree = CodexInstanceProcessTree::capture(&identity).unwrap();
    assert_eq!(tree.processes.len(), 1);
    assert!(codex_instance_needs_worker(&identity).await.unwrap());
    let mut wrong = identity.clone();
    wrong.user_data_dir = dir.path().canonicalize().unwrap();
    assert!(terminate_verified_codex_instance(&wrong).await.is_err());
    assert!(terminate_verified_codex_instance(&identity).await.unwrap());
    assert!(codex_instance_process_exited(&identity));
}

#[tokio::test]
async fn read_only_worker_probe_reports_conflicts_and_only_confirms_same_generation_exit() {
    let _fixture_lock = FIXTURE_LOCK.lock().await;
    let fixtures = FixtureGroup::start(unused_port());
    let root = fixtures.process("root");
    let options = fixtures.dir.path().join("worker-options.json");
    std::fs::write(&options, "{}").unwrap();
    assert!(verified_worker_is_running(root.process_id, &options, None).is_err());
    assert!(
        verified_worker_is_running(root.process_id, &options, Some(root.birth_id + 1)).is_err()
    );
    // fixture 的 argv 不是固定的三段 worker argv，存活查询必须报冲突且不结束它。
    assert!(verified_worker_is_running(root.process_id, &options, Some(root.birth_id)).is_err());
    assert!(root.is_alive().unwrap());
    root.terminate().unwrap();
    wait_for_exit(&root);
    assert!(!verified_worker_is_running(root.process_id, &options, Some(root.birth_id)).unwrap());
}
