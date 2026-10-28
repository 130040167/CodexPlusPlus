use super::*;
use anyhow::{Context, ensure};
use codex_plus_core::multi_worker::CodexInstanceIdentity;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const OPTIONS: &str = "worker-options.json";
const READY: &str = "worker-ready.json";
const REQUEST: &str = "worker-handoff.json";
const RELEASE: &str = "worker-handoff-release.json";
const COMMIT: &str = "worker-handoff-commit.json";
const CANDIDATE: &str = "worker-handoff-candidate.json";
const HANDOFF_ENV: &str = "CODEX_PLUS_WORKER_HANDOFF";
const RESUME_LOCK: &str = "worker-resume.lock";
const OUTCOME: &str = "worker-resume-result.json";
const STAGE_TIMEOUT: Duration = Duration::from_secs(90);
const HANDOFF_LIFETIME: Duration = Duration::from_secs(180);
const HANDOFF_PROTOCOL: u32 = 2;
const POLL: Duration = Duration::from_millis(100);

/// 记录 worker 启动时的文件属性；原路径被替换后不能据路径相同复用旧构建。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct BinaryStamp {
    executable: PathBuf,
    size: u64,
    modified_ns: String,
    version: String,
}

impl BinaryStamp {
    pub(super) fn current() -> anyhow::Result<Self> {
        Self::at(&std::env::current_exe()?)
    }

    fn at(path: &Path) -> anyhow::Result<Self> {
        let executable = path.canonicalize().context("无法定位新版 worker 二进制")?;
        let file = fs::File::open(&executable).context("新版 worker 二进制不可读")?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file() && metadata.len() != 0,
            "新版 worker 二进制无效"
        );
        Ok(Self {
            executable,
            size: metadata.len(),
            modified_ns: metadata
                .modified()?
                .duration_since(UNIX_EPOCH)?
                .as_nanos()
                .to_string(),
            version: env!("CARGO_PKG_VERSION").into(),
        })
    }
}

/// 交接始终绑定完整 ready；v1 worker 不理解撤销，不能向其发送退出请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WorkerReady {
    protocol: u32,
    instance_id: String,
    generation: u64,
    pid: u32,
    worker_birth_id: Option<u64>,
    token: String,
    options_path: PathBuf,
    identity: CodexInstanceIdentity,
    binary: BinaryStamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HandoffRequest {
    protocol: u32,
    token: String,
    worker: WorkerReady,
    requester_pid: u32,
    requester_birth: String,
    issued_at_ms: u64,
    expires_at_ms: u64,
    successor_token: String,
    successor_binary: BinaryStamp,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HandoffRelease {
    request: HandoffRequest,
    options: CodexMultiWorkerOptions,
    released: bool,
}

#[derive(Serialize, Deserialize)]
struct HandoffCommit {
    request: HandoffRequest,
    successor: WorkerReady,
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn open_resume_lock(root: &Path) -> anyhow::Result<fs::File> {
    let path = root.join(RESUME_LOCK);
    if let Ok(metadata) = fs::symlink_metadata(&path) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "worker 交接锁不能重定向"
        );
    }
    Ok(fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?)
}

fn process_birth(pid: u32) -> Option<String> {
    #[cfg(windows)]
    {
        codex_plus_core::windows_process_birth_id(pid).map(|birth| birth.to_string())
    }
    #[cfg(not(windows))]
    {
        match codex_plus_core::watcher::inspect_process_instance(pid) {
            codex_plus_core::watcher::ProcessInstanceState::Running { birth_id, .. } => birth_id,
            _ => None,
        }
    }
}

fn request_matches(
    request: &HandoffRequest,
    ready: &WorkerReady,
    options_path: &Path,
    now: u64,
) -> bool {
    request.protocol == HANDOFF_PROTOCOL
        && ready.protocol == HANDOFF_PROTOCOL
        && request.worker == *ready
        && options_path.canonicalize().ok().as_ref() == Some(&ready.options_path)
        && request.requester_pid != 0
        && request.requester_pid != ready.pid
        && !request.requester_birth.is_empty()
        && !request.token.is_empty()
        && !request.successor_token.is_empty()
        && request.successor_token != ready.token
        && request.issued_at_ms <= now
        && now < request.expires_at_ms
        && request.expires_at_ms.saturating_sub(request.issued_at_ms)
            <= HANDOFF_LIFETIME.as_millis() as u64
}

fn request_active(root: &Path, request: &HandoffRequest, ready: &WorkerReady) -> bool {
    if !request_matches(request, ready, &root.join(OPTIONS), current_timestamp_ms())
        || read_json::<HandoffRequest>(&root.join(REQUEST)).as_ref() != Some(request)
        || process_birth(request.requester_pid).as_ref() != Some(&request.requester_birth)
    {
        return false;
    }
    // 请求必须仍由发起 manager 持锁；崩溃会释放锁，即使磁盘请求尚未来得及撤销。
    let Ok(lock) = open_resume_lock(root) else {
        return false;
    };
    matches!(lock.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

struct PendingHandoff {
    root: PathBuf,
    request: HandoffRequest,
}

impl PendingHandoff {
    fn revoke(&self) -> anyhow::Result<()> {
        // 只撤销本次请求，不覆盖另一代交接；用 tombstone 避免按路径删除的竞争。
        if read_json::<HandoffRequest>(&self.root.join(REQUEST)).as_ref() == Some(&self.request) {
            write_json(&self.root.join(REQUEST), &serde_json::Value::Null)?;
        }
        Ok(())
    }
}

impl Drop for PendingHandoff {
    fn drop(&mut self) {
        if let Err(error) = self.revoke() {
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                "manager.multi_worker_revoke_failed",
                json!({"message": format!("{error:#}")}),
            );
        }
    }
}

fn new_token() -> String {
    static SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        SERIAL.fetch_add(1, Ordering::Relaxed),
    )
}

fn write_json(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        ensure!(!metadata.file_type().is_symlink(), "续接文件不能是符号链接");
    }
    codex_plus_core::settings::atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

pub(super) fn publish_worker_ready(
    options_path: &Path,
    options: &CodexMultiWorkerOptions,
    identity: &CodexInstanceIdentity,
    binary: BinaryStamp,
) -> anyhow::Result<WorkerReady> {
    let ready = worker_ready(options_path, options, identity, binary)?;
    validate_candidate(&ready, false)?;
    let root = ready.options_path.parent().context("缺少 worker 目录")?;
    write_json(&root.join(READY), &ready)?;
    Ok(ready)
}

fn worker_ready(
    options_path: &Path,
    options: &CodexMultiWorkerOptions,
    identity: &CodexInstanceIdentity,
    binary: BinaryStamp,
) -> anyhow::Result<WorkerReady> {
    ensure!(
        identity.instance_id == options.instance_id && identity.generation == options.generation,
        "worker 就绪身份不匹配"
    );
    let options_path = options_path.canonicalize()?;
    let ready = WorkerReady {
        protocol: HANDOFF_PROTOCOL,
        instance_id: options.instance_id.clone(),
        generation: options.generation,
        pid: std::process::id(),
        #[cfg(windows)]
        worker_birth_id: codex_plus_core::windows_process_birth_id(std::process::id()),
        #[cfg(not(windows))]
        worker_birth_id: None,
        token: options.resume_token.clone().unwrap_or_else(new_token),
        options_path: options_path.clone(),
        identity: identity.clone(),
        binary,
    };
    ensure!(!ready.token.is_empty(), "worker token 为空");
    Ok(ready)
}

fn validate_candidate(ready: &WorkerReady, register: bool) -> anyhow::Result<()> {
    let Some(request) = std::env::var_os(HANDOFF_ENV) else {
        return Ok(());
    };
    let request: HandoffRequest =
        serde_json::from_str(request.to_str().context("候选 worker 交接标记无效")?)?;
    validate_candidate_request(&request, ready, register)
}

fn validate_candidate_request(
    request: &HandoffRequest,
    ready: &WorkerReady,
    register: bool,
) -> anyhow::Result<()> {
    ensure!(
        successor_matches(request, ready),
        "候选 worker 与交接身份不一致"
    );
    let root = ready.options_path.parent().context("缺少 worker 目录")?;
    // 提交绑定完整 PID/birth，不能用过期请求或另一候选的 commit 放行。
    if read_json::<HandoffCommit>(&root.join(COMMIT))
        .is_some_and(|commit| commit.request == *request && commit.successor == *ready)
    {
        return Ok(());
    }
    if register {
        ensure!(
            request_active(root, request, &request.worker),
            "候选 worker 启动前交接已失效"
        );
        // 先注册再复验：manager 在 spawn/manifest 之间退出时，回滚仍能发现候选。
        write_json(&root.join(CANDIDATE), ready)?;
    }
    ensure!(
        request_active(root, request, &request.worker),
        "交接已取消或发起 manager 已退出，候选 worker 不得启动服务或发布就绪"
    );
    Ok(())
}

pub(super) fn handoff_requested(
    options_path: &Path,
    ready: &WorkerReady,
    checkpoint: impl FnOnce() -> anyhow::Result<()>,
) -> bool {
    if ready.pid != std::process::id() {
        return false;
    }
    let Some(root) = options_path.parent() else {
        return false;
    };
    let release = read_json::<HandoffRelease>(&root.join(RELEASE))
        .filter(|release| release.request.worker == *ready);
    if let Some(release) = release {
        let committed = committed_successor(root, &release.request);
        let active = !committed && request_active(root, &release.request, ready);
        match finish_handoff(committed, active, || {
            restore_released_worker(root, ready, &release)
        }) {
            Ok(true) => return true,
            Err(error) => {
                // 保留 release 回执，下轮继续恢复；不能丢掉旧 worker 的恢复责任。
                let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                    "manager.multi_worker_rollback_pending",
                    json!({"message": format!("{error:#}")}),
                );
            }
            Ok(false) => {}
        }
        if active && !release.released {
            release_worker_services(root, ready, release, checkpoint);
        }
        return false;
    }
    let Some(request) = read_json::<HandoffRequest>(&root.join(REQUEST))
        .filter(|request| request_active(root, request, ready))
    else {
        return false;
    };
    let Some(options) = read_json::<CodexMultiWorkerOptions>(options_path) else {
        return false;
    };
    let release = HandoffRelease {
        request,
        options,
        released: false,
    };
    // 先持久化恢复材料，stop 或回执写入失败后仍能在下一轮恢复。
    if write_json(&root.join(RELEASE), &release).is_ok() {
        release_worker_services(root, ready, release, checkpoint);
    }
    false
}

fn finish_handoff(
    committed: bool,
    active: bool,
    restore: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<bool> {
    if committed {
        return Ok(true);
    }
    if !active {
        restore()?;
    }
    Ok(false)
}

fn release_worker_services(
    root: &Path,
    ready: &WorkerReady,
    mut release: HandoffRelease,
    checkpoint: impl FnOnce() -> anyhow::Result<()>,
) {
    let result = (|| -> anyhow::Result<()> {
        ensure!(
            request_active(root, &release.request, ready),
            "交接请求已失效"
        );
        checkpoint()?;
        ensure!(
            request_active(root, &release.request, ready),
            "保存 checkpoint 期间交接已取消"
        );
        codex_plus_launcher::stop()?;
        ensure!(
            request_active(root, &release.request, ready),
            "服务释放期间交接已取消"
        );
        release.released = true;
        write_json(&root.join(RELEASE), &release)
    })();
    if let Err(error) = result {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "manager.multi_worker_release_failed",
            json!({"message": format!("{error:#}")}),
        );
    }
}

fn successor_matches(request: &HandoffRequest, successor: &WorkerReady) -> bool {
    successor.protocol == HANDOFF_PROTOCOL
        && successor.instance_id == request.worker.instance_id
        && successor.generation == request.worker.generation
        && successor.identity == request.worker.identity
        && successor.options_path == request.worker.options_path
        && successor.pid != 0
        && successor.pid != request.worker.pid
        && successor.token == request.successor_token
        && successor.binary == request.successor_binary
}

fn committed_successor(root: &Path, request: &HandoffRequest) -> bool {
    let Some(commit) = read_json::<HandoffCommit>(&root.join(COMMIT)).filter(|commit| {
        commit.request == *request && successor_matches(request, &commit.successor)
    }) else {
        return false;
    };
    let Ok(instance) = load_multi_instance_manifest(root) else {
        return false;
    };
    ready_matches(&commit.successor, &instance, &root.join(OPTIONS))
        && worker_present(&instance, &root.join(OPTIONS)).unwrap_or(false)
}

fn restore_released_worker(
    root: &Path,
    ready: &WorkerReady,
    release: &HandoffRelease,
) -> anyhow::Result<()> {
    restore_released_worker_with(root, ready, release, terminate_multi_worker, |options| {
        let receipt =
            codex_plus_launcher::resume_with_receipt(codex_plus_launcher::LaunchOptions {
                app_dir: Some(PathBuf::from(&options.app_path)),
                debug_port: ready.identity.debug_port,
                helper_port: ready.identity.helper_port,
                status_store: StatusStore::new(
                    Path::new(&options.state_dir).join("latest-status.json"),
                ),
                state_dir: Some(PathBuf::from(&options.state_dir)),
                codex_home: std::env::var_os("CODEX_HOME").map(PathBuf::from),
                sqlite_home: std::env::var_os("CODEX_SQLITE_HOME").map(PathBuf::from),
            })?
            .context("原桌面暂不可续接，保留旧 worker 稍后重试")?;
        confirm_resumed_identity(&ready.identity, &receipt)
    })
}

fn restore_released_worker_with(
    root: &Path,
    ready: &WorkerReady,
    release: &HandoffRelease,
    stop_candidate: impl FnOnce(u32, &Path, Option<u64>) -> anyhow::Result<()>,
    resume: impl FnOnce(&CodexMultiWorkerOptions) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    // manager 先回收自己持有 Child 的候选 worker 并退出事务；旧 worker 随后恢复。
    let lock = open_resume_lock(root)?;
    lock.try_lock().context("等待 manager 结束交接事务")?;
    let mut instance = load_multi_instance_manifest(root)?;
    ensure!(
        instance.identity.as_ref() == Some(&ready.identity)
            && instance.instance_id == ready.instance_id
            && instance.generation == ready.generation,
        "恢复时桌面身份已变化，未触动其他代次"
    );
    let registered = read_json::<WorkerReady>(&root.join(CANDIDATE))
        .filter(|candidate| successor_matches(&release.request, candidate));
    let candidate = registered
        .as_ref()
        .map(|candidate| (candidate.pid, candidate.worker_birth_id))
        .or_else(|| {
            instance
                .pid
                .filter(|pid| *pid != ready.pid)
                .map(|pid| (pid, instance.worker_birth_id))
        });
    if let Some((pid, birth)) = candidate {
        ensure!(
            instance
                .pid
                .is_none_or(|current| current == ready.pid || current == pid),
            "清单指向其他 worker，拒绝停止"
        );
        let options: CodexMultiWorkerOptions =
            read_json(&root.join(OPTIONS)).context("缺少候选 worker 参数")?;
        ensure!(
            options.resume_token.as_ref() == Some(&release.request.successor_token)
                && options.resume_identity.as_ref() == Some(&ready.identity),
            "候选 worker 不属于本次交接，拒绝停止"
        );
        // 发起 manager 崩溃时 Child 不会自动退出；仅清理本次完整身份的候选 worker。
        stop_candidate(pid, &root.join(OPTIONS), birth)?;
        write_json(&root.join(CANDIDATE), &serde_json::Value::Null)?;
    }
    write_json(&root.join(OPTIONS), &release.options)?;
    instance.pid = Some(ready.pid);
    instance.worker_birth_id = ready.worker_birth_id;
    save_multi_instance_manifest(root, &instance)?;
    resume(&release.options)?;
    write_json(&root.join(READY), ready)?;
    write_json(&root.join(RELEASE), &serde_json::Value::Null)?;
    Ok(())
}

fn desktop_matches(identity: &CodexInstanceIdentity, deadline: Instant) -> anyhow::Result<()> {
    // 外部 CDP 验证只读；调用方即使超时也不会留下延迟执行的 stop/spawn。
    let identity = identity.clone();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = tauri::async_runtime::block_on(
            codex_plus_core::multi_worker::codex_instance_identity_matches(&identity),
        );
        let _ = sender.send(result);
    });
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "验证原 Codex 身份超时，可重试");
    ensure!(
        receiver
            .recv_timeout(remaining)
            .context("验证原 Codex 身份超时，可重试")??,
        "原 Codex 身份已变化或连接暂不可用，已保留桌面，可重试"
    );
    Ok(())
}

pub(super) fn validate_worker_resume(options: &CodexMultiWorkerOptions) -> anyhow::Result<()> {
    let candidate_request = std::env::var_os(HANDOFF_ENV)
        .map(|value| -> anyhow::Result<HandoffRequest> {
            Ok(serde_json::from_str(
                value.to_str().context("候选 worker 交接标记无效")?,
            )?)
        })
        .transpose()?;
    let candidate = candidate_request
        .as_ref()
        .map(|request| {
            worker_ready(
                &request.worker.options_path,
                options,
                options
                    .resume_identity
                    .as_ref()
                    .context("候选缺少桌面身份")?,
                BinaryStamp::current()?,
            )
        })
        .transpose()?;
    if let Some(ready) = &candidate {
        validate_candidate(ready, true)?;
    }
    if let Some(identity) = &options.resume_identity {
        ensure!(
            options
                .resume_token
                .as_ref()
                .is_some_and(|token| !token.is_empty())
                && identity.instance_id == options.instance_id
                && identity.generation == options.generation
                && identity.debug_port == options.debug_port
                && identity.helper_port == options.helper_port
                && Path::new(&options.user_data_dir).canonicalize()? == identity.user_data_dir
                && desktop_executable(Path::new(&options.app_path)) == identity.executable,
            "续接参数与原 Codex 身份不一致"
        );
        desktop_matches(identity, Instant::now() + STAGE_TIMEOUT)?;
    }
    if let Some(ready) = &candidate {
        validate_candidate(ready, false)?;
    }
    Ok(())
}

pub(super) fn confirm_resumed_identity(
    identity: &CodexInstanceIdentity,
    receipt: &codex_plus_launcher::LaunchReceipt,
) -> anyhow::Result<()> {
    ensure!(
        receipt.debug_port == identity.debug_port
            && receipt.helper_port == identity.helper_port
            && desktop_executable(&receipt.app_dir) == identity.executable,
        "续接端口或应用发生变化，拒绝接管"
    );
    desktop_matches(identity, Instant::now() + STAGE_TIMEOUT)
}

fn desktop_executable(app_dir: &Path) -> PathBuf {
    let executable = codex_plus_core::app_paths::build_codex_executable(app_dir);
    executable.canonicalize().unwrap_or(executable)
}

fn scoped_options(
    root: &Path,
    parent: &Path,
    instance: &CodexMultiInstance,
    legacy_parent: Option<&Path>,
) -> anyhow::Result<CodexMultiWorkerOptions> {
    let root = root.canonicalize()?;
    let parent = parent.canonicalize()?;
    ensure!(
        root.parent() == Some(parent.as_path())
            && root.file_name().and_then(|name| name.to_str())
                == Some(multi_instance_slug(&instance.instance_id).as_str()),
        "多开目录归属不匹配"
    );
    let identity = instance
        .identity
        .as_ref()
        .context("缺少原 Codex 完整身份")?;
    let options_path = root.join(OPTIONS);
    ensure!(
        options_path.canonicalize()? == options_path,
        "worker options 路径发生重定向"
    );
    let mut options: CodexMultiWorkerOptions = serde_json::from_slice(&fs::read(options_path)?)?;
    let state = root
        .join("worker-home/.codex-session-delete")
        .canonicalize()?;
    let user_data = identity.user_data_dir.canonicalize()?;
    let legacy_user_data = legacy_parent
        .map(|parent| parent.join(multi_instance_slug(&instance.instance_id)))
        .and_then(|path| path.canonicalize().ok());
    let configured_user_data = std::env::var_os("CODEX_ELECTRON_USER_DATA_PATH")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|parent| {
            parent
                .join("multi")
                .join(multi_instance_slug(&instance.instance_id))
        })
        .and_then(|path| path.canonicalize().ok());
    ensure!(
        multi_manifest_identity_matches(instance, identity)
            && options.instance_id == instance.instance_id
            && options.generation == instance.generation
            && instance.generation != 0
            && state.starts_with(&root)
            && Path::new(&options.state_dir).canonicalize()? == state
            && Path::new(&options.user_data_dir).canonicalize()? == user_data
            && (user_data.starts_with(&root)
                || legacy_user_data.as_ref() == Some(&user_data)
                || configured_user_data.as_ref() == Some(&user_data))
            && instance
                .codex_home
                .as_ref()
                .map(Path::new)
                .map(Path::canonicalize)
                .transpose()?
                == Some(root.join("codex-home").canonicalize()?)
            && instance
                .sqlite_home
                .as_ref()
                .map(Path::new)
                .map(Path::canonicalize)
                .transpose()?
                == Some(root.join("sqlite-home").canonicalize()?),
        "manifest、窗口或隔离路径不完整，未触动旧 worker"
    );
    ensure!(
        root.join("codex-home").canonicalize()?.starts_with(&root)
            && root.join("sqlite-home").canonicalize()?.starts_with(&root)
            && root.join("worker-home").canonicalize()?.starts_with(&root),
        "多开隔离目录发生重定向"
    );
    // 不读取供应商内容或主配置，更不从主实例重建 worker 的 provider。
    ensure!(
        state.join("settings.json").is_file(),
        "缺少原 worker 设置，已保留旧 worker"
    );
    let _settings = fs::File::open(state.join("settings.json"))?;
    options.app_path = instance.app_path.clone().context("缺少原应用目录")?;
    // WindowsApps 映像可能拒绝 metadata；实际 exe 归属由桌面身份核验。
    let _app_entries = fs::read_dir(&options.app_path).context("原 Codex 应用目录不可访问")?;
    options.debug_port = identity.debug_port;
    options.helper_port = identity.helper_port;
    options.resume_identity = Some(identity.clone());
    options.resume_token = Some(new_token());
    Ok(options)
}

fn legacy_parent(parent: &Path) -> Option<PathBuf> {
    let dirs = directories::BaseDirs::new()?;
    let production = dirs.home_dir().join(".codex-session-delete/codex-multi");
    // 隔离 manager 不能以旧布局为由借用生产窗口。
    (parent.canonicalize().ok()? == production.canonicalize().ok()?)
        .then(|| dirs.data_local_dir().join("CodexPlusPlus/multi"))
}

fn ready_matches(ready: &WorkerReady, instance: &CodexMultiInstance, options_path: &Path) -> bool {
    matches!(ready.protocol, 1 | HANDOFF_PROTOCOL)
        && ready.pid != 0
        && Some(ready.pid) == instance.pid
        && ready.worker_birth_id == instance.worker_birth_id
        && ready.instance_id == instance.instance_id
        && ready.generation == instance.generation
        && !ready.token.is_empty()
        && instance.identity.as_ref() == Some(&ready.identity)
        && options_path.canonicalize().ok().as_ref() == Some(&ready.options_path)
}

fn read_ready(root: &Path) -> anyhow::Result<Option<WorkerReady>> {
    match fs::read(root.join(READY)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("worker ready 协议损坏")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn worker_present(instance: &CodexMultiInstance, options_path: &Path) -> anyhow::Result<bool> {
    let Some(pid) = instance.pid else {
        return Ok(false);
    };
    #[cfg(windows)]
    {
        codex_plus_core::multi_worker::verified_worker_is_running(
            pid,
            options_path,
            instance.worker_birth_id,
        )
    }
    #[cfg(not(windows))]
    {
        let _ = options_path;
        match codex_plus_core::watcher::inspect_process_instance(pid) {
            codex_plus_core::watcher::ProcessInstanceState::NotRunning => Ok(false),
            _ => anyhow::bail!("当前系统缺少精确 worker 核验接口，已保留旧 worker"),
        }
    }
}

fn verify_legacy_services(
    instance: &CodexMultiInstance,
    options_path: &Path,
) -> anyhow::Result<()> {
    verify_legacy_services_with(
        || worker_present(instance, options_path),
        || {
            tauri::async_runtime::block_on(async {
                let client = codex_plus_core::http_client::local_helper_client()?;
                let response = client
                    .get(format!(
                        "http://127.0.0.1:{}/backend/status",
                        instance.helper_port
                    ))
                    .send()
                    .await?
                    .error_for_status()?;
                let status: serde_json::Value = response.json().await?;
                ensure!(
                    status["status"] == "ok" && status["transport"] == "http-helper",
                    "旧 worker 的 helper 尚未确认健康，已保留并延期交接"
                );
                Ok::<_, anyhow::Error>(())
            })
        },
    )
}

fn verify_legacy_services_with(
    mut present: impl FnMut() -> anyhow::Result<bool>,
    healthy: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    ensure!(present()?, "旧 worker 已退出");
    healthy()?;
    ensure!(present()?, "健康检查期间旧 worker 已变化");
    Ok(())
}

fn generation_may_be_alive(
    expected_birth: Option<u64>,
    state: codex_plus_core::watcher::ProcessInstanceState,
) -> bool {
    use codex_plus_core::watcher::ProcessInstanceState;
    match state {
        ProcessInstanceState::NotRunning => false,
        ProcessInstanceState::Running {
            birth_id: Some(birth),
            ..
        } => !expected_birth
            .zip(birth.parse::<u64>().ok())
            .is_some_and(|(a, b)| a != b),
        _ => true,
    }
}

fn worker_could_hold_handles(root: &Path) -> bool {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct WorkerHint {
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        worker_birth_id: Option<u64>,
    }
    let Ok(bytes) = fs::read(multi_instance_manifest_path(root)) else {
        return false;
    };
    let Ok(hint) = serde_json::from_slice::<WorkerHint>(&bytes) else {
        return false;
    };
    hint.pid.is_some_and(|pid| {
        generation_may_be_alive(
            hint.worker_birth_id,
            codex_plus_core::watcher::inspect_process_instance(pid),
        )
    })
}

fn check_active(cancel: &AtomicBool, deadline: Instant) -> anyhow::Result<()> {
    ensure!(
        !crate::app_is_exiting() && !cancel.load(Ordering::Acquire),
        "管理器恢复已取消，原 Codex 保持运行，可重试"
    );
    ensure!(
        Instant::now() < deadline,
        "worker 交接阶段超过 90 秒，可重试"
    );
    Ok(())
}

fn same_generation(current: &CodexMultiInstance, original: &CodexMultiInstance) -> bool {
    current.instance_id == original.instance_id
        && current.generation == original.generation
        && current.identity == original.identity
}

struct PendingWorker {
    child: Child,
    armed: bool,
}

impl PendingWorker {
    fn reclaim(&mut self) -> anyhow::Result<()> {
        if self.child.try_wait()?.is_some() {
            self.armed = false;
            return Ok(());
        }
        // 持有本次 spawn 的 Child 句柄；绝不按 PID 或程序名结束其他进程。
        self.child.kill().context("回收本次未就绪 worker 失败")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                self.armed = false;
                return Ok(());
            }
            std::thread::sleep(POLL);
        }
        anyhow::bail!("本次未就绪 worker 尚未确认退出")
    }
}

impl Drop for PendingWorker {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.reclaim();
        }
    }
}

fn spawn_resume(
    root: &Path,
    binary: &BinaryStamp,
    options: &CodexMultiWorkerOptions,
    instance: &CodexMultiInstance,
    handoff: Option<&HandoffRequest>,
) -> anyhow::Result<PendingWorker> {
    ensure!(
        BinaryStamp::at(&binary.executable)? == *binary,
        "新版二进制预检后发生变化，可重试"
    );
    let mut command = Command::new(&binary.executable);
    command.env_remove(HANDOFF_ENV);
    if let Some(request) = handoff {
        command.env(HANDOFF_ENV, serde_json::to_string(request)?);
    }
    command
        .arg("--codex-plus-multi-worker")
        .arg(root.join(OPTIONS))
        .env("HOME", root.join("worker-home"))
        .env("CODEX_HOME", root.join("codex-home"))
        .env("CODEX_SQLITE_HOME", root.join("sqlite-home"))
        .env("CODEX_ELECTRON_USER_DATA_PATH", &options.user_data_dir)
        .env("CODEX_PLUS_MULTI_INSTANCE", "1")
        .env("CODEX_PLUS_INSTANCE_ID", &instance.instance_id)
        .env("CODEX_PLUS_INSTANCE_LABEL", &instance.profile_name)
        .env(
            "CODEX_PLUS_LAUNCHER_GUARD_PORT",
            select_multi_instance_port(0, &format!("{}-guard", instance.instance_id)).to_string(),
        )
        .env(
            "CODEX_PLUS_PROTOCOL_PROXY_PORT",
            options.helper_port.to_string(),
        )
        .env(codex_plus_core::paths::STATE_DIR_ENV, &options.state_dir)
        .env(
            "XDG_CONFIG_HOME",
            directories::BaseDirs::new()
                .context("无法定位共享脚本目录")?
                .home_dir()
                .join(".config"),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(codex_plus_core::windows_create_no_window());
    }
    Ok(PendingWorker {
        child: command.spawn()?,
        armed: true,
    })
}

fn resume_one(root: &Path, parent: &Path, cancel: &AtomicBool) -> anyhow::Result<&'static str> {
    let root = root.canonicalize()?;
    ensure!(
        root.parent() == Some(parent.canonicalize()?.as_path()),
        "实例目录越界"
    );
    let lock = open_resume_lock(&root)?;
    lock.try_lock()
        .context("另一个管理器正在交接此 worker，可重试")?;
    let mut instance = load_multi_instance_manifest(&root)?;
    let options = scoped_options(&root, parent, &instance, legacy_parent(parent).as_deref())?;
    let binary = BinaryStamp::current()?;
    let options_path = root.join(OPTIONS);
    let identity = options.resume_identity.as_ref().context("缺少续接身份")?;
    let preflight_deadline = Instant::now() + STAGE_TIMEOUT;
    check_active(cancel, preflight_deadline)?;
    desktop_matches(identity, preflight_deadline)?;
    let mut handoff = None;
    if worker_present(&instance, &options_path)? {
        if let Some(ready) = read_ready(&root)? {
            ensure!(
                ready_matches(&ready, &instance, &options_path),
                "旧 worker 协议身份不一致，已保留原 worker，可重试"
            );
            if ready.protocol == 1 {
                verify_legacy_services(&instance, &options_path)
                    .context("legacy worker 健康状态不明确，已保留并延期交接")?;
                desktop_matches(identity, preflight_deadline)?;
                check_active(cancel, preflight_deadline)?;
                return Ok("reused_legacy");
            }
            if ready.binary == binary
                && !read_json::<HandoffRelease>(&root.join(RELEASE))
                    .is_some_and(|release| release.request.worker == ready)
            {
                return Ok("reused");
            }
            ensure!(
                BinaryStamp::current()? == binary,
                "新版二进制已变化，保留旧 worker"
            );
            check_active(cancel, preflight_deadline)?;
            let issued_at_ms = current_timestamp_ms();
            let request = HandoffRequest {
                protocol: HANDOFF_PROTOCOL,
                token: new_token(),
                worker: ready,
                requester_pid: std::process::id(),
                requester_birth: process_birth(std::process::id())
                    .context("无法核验发起 manager 的创建身份，未请求交接")?,
                issued_at_ms,
                expires_at_ms: issued_at_ms.saturating_add(HANDOFF_LIFETIME.as_millis() as u64),
                successor_token: options.resume_token.clone().context("缺少续接 token")?,
                successor_binary: binary.clone(),
            };
            handoff = Some(PendingHandoff {
                root: root.clone(),
                request,
            });
            let request = &handoff.as_ref().unwrap().request;
            write_json(&root.join(REQUEST), request)?;
            let release_deadline = Instant::now() + STAGE_TIMEOUT;
            loop {
                check_active(cancel, release_deadline)?;
                ensure!(
                    worker_present(&instance, &options_path)?,
                    "旧 worker 在交接提交前退出，未继续启动"
                );
                if read_json::<HandoffRelease>(&root.join(RELEASE))
                    .is_some_and(|release| release.request == *request && release.released)
                {
                    break;
                }
                std::thread::sleep(POLL);
            }
        } else {
            verify_legacy_services(&instance, &options_path)
                .context("无 ready 的 legacy worker 尚未确认健康，已保留并延期交接")?;
            desktop_matches(identity, preflight_deadline)?;
            check_active(cancel, preflight_deadline)?;
            return Ok("reused_legacy");
        }
    }
    let ready_deadline = Instant::now() + STAGE_TIMEOUT;
    check_active(cancel, ready_deadline)?;
    desktop_matches(identity, ready_deadline)?;
    {
        let _guard = multi_instance_mutation_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let current = load_multi_instance_manifest(&root)?;
        ensure!(
            same_generation(&current, &instance) && current.pid == instance.pid,
            "交接期间实例代次或 worker 发生变化，可重试"
        );
        write_json(&options_path, &options)?;
    }
    check_active(cancel, ready_deadline)?;
    let mut pending = spawn_resume(
        &root,
        &binary,
        &options,
        &instance,
        handoff.as_ref().map(|handoff| &handoff.request),
    )?;
    let new_pid = pending.child.id();
    let new_birth = codex_plus_core::multi_worker::worker_birth_id(&pending.child);
    let result = (|| {
        {
            let _guard = multi_instance_mutation_lock()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let current = load_multi_instance_manifest(&root)?;
            ensure!(
                same_generation(&current, &instance) && current.pid == instance.pid,
                "启动续接 worker 时清单已变化"
            );
            instance.pid = Some(new_pid);
            instance.worker_birth_id = new_birth;
            save_multi_instance_manifest(&root, &instance)?;
        }
        loop {
            check_active(cancel, ready_deadline)?;
            ensure!(
                pending.child.try_wait()?.is_none(),
                "新版 worker 提前退出，原桌面保持运行，可重试"
            );
            if let Ok(Some(ready)) = read_ready(&root) {
                if ready_matches(&ready, &instance, &options_path)
                    && options.resume_token.as_ref() == Some(&ready.token)
                    && ready.binary == binary
                {
                    desktop_matches(identity, ready_deadline)?;
                    let _guard = multi_instance_mutation_lock()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    let current = load_multi_instance_manifest(&root)?;
                    ensure!(
                        same_generation(&current, &instance) && current.pid == Some(new_pid),
                        "续接期间清单已变化，已回收本次 worker"
                    );
                    check_active(cancel, ready_deadline)?;
                    if let Some(handoff) = &handoff {
                        ensure!(
                            request_active(&root, &handoff.request, &handoff.request.worker),
                            "交接请求已过期或撤销，未提交候选 worker"
                        );
                        write_json(
                            &root.join(COMMIT),
                            &HandoffCommit {
                                request: handoff.request.clone(),
                                successor: ready,
                            },
                        )?;
                    }
                    return Ok("resumed");
                }
            }
            std::thread::sleep(POLL);
        }
    })();
    match result {
        Ok(outcome) => {
            pending.armed = false;
            Ok(outcome)
        }
        Err(error) => {
            let cleanup = pending.reclaim();
            if cleanup.is_ok() {
                let _guard = multi_instance_mutation_lock()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Ok(mut current) = load_multi_instance_manifest(&root) {
                    if same_generation(&current, &instance) && current.pid == Some(new_pid) {
                        current.pid = None;
                        current.worker_birth_id = None;
                        // 保留原 desktop identity 与隔离配置，重开 manager 即可重试 attach。
                        save_multi_instance_manifest(&root, &current)?;
                    }
                }
            }
            match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "{error:#}；未就绪 worker 回收失败：{cleanup:#}"
                )),
            }
        }
    }
}

fn worker_resume_roots(parent: &Path, windows: bool) -> Vec<PathBuf> {
    if !windows {
        return Vec::new();
    }
    fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .filter(|root| root.join("instance.json").is_file())
        .collect()
}

/// Windows worker 并行交接后恢复主服务；其他平台保留原 worker，不设全局门槛。
pub(crate) fn resume_on_manager_start(app: tauri::AppHandle, start_if_missing: bool) {
    tauri::async_runtime::spawn_blocking(move || {
        let parent = codex_plus_core::paths::default_app_state_dir().join("codex-multi");
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let mut count = 0;
        for root in worker_resume_roots(&parent, cfg!(windows)) {
            count += 1;
            let sender = sender.clone();
            let parent = parent.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let result =
                    (|| {
                        let instance = load_multi_instance_manifest(&root)?;
                        // 确认桌面退出才跳过；缺身份/临时 CDP 失联必须报告而非误认停止。
                        if instance.identity.as_ref().is_some_and(
                            codex_plus_core::multi_worker::codex_instance_process_exited,
                        ) || (instance.identity.is_none() && !worker_could_hold_handles(&root))
                        {
                            return Ok("not_running");
                        }
                        resume_one(&root, &parent, &cancel)
                    })();
                let outcome = json!({
                    "status": result.as_ref().copied().unwrap_or("deferred"),
                    "retryable": result.is_err(),
                    "message": result.as_ref().err().map(|error| format!("{error:#}")),
                    "finishedAtMs": current_timestamp_ms(),
                });
                let _ = write_json(&root.join(OUTCOME), &outcome);
                let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                    "manager.multi_worker_resume",
                    &outcome,
                );
                // 已退出的坏历史不阻断主恢复；仍活着的失败 worker 可能遗留监听。
                let _ = sender.send(result.is_ok() || !worker_could_hold_handles(&root));
            });
        }
        drop(sender);
        // 三个阶段各 90s；实例并行，不按实例数量累加等待。
        let deadline = Instant::now() + STAGE_TIMEOUT * 3 + Duration::from_secs(5);
        let mut complete = true;
        for _ in 0..count {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(ok) => complete &= ok,
                Err(_) => {
                    complete = false;
                    cancel.store(true, Ordering::Release);
                    break;
                }
            }
        }
        if !complete {
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                "manager.background_restore_deferred",
                json!({"message": "多开交接未完成；保留原桌面，主 helper/Gateway 恢复已延期，可重开管理器重试"}),
            );
        } else if !crate::app_is_exiting() {
            resume_existing_codex_background(app.clone(), start_if_missing);
            crate::gateway_commands::start_from_saved_state();
            crate::gateway_commands::start_model_watch();
            crate::auto_takeover::start_supervisor();
        }
        let _ = app.emit("runtime-health-changed", ());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(root: &Path) -> CodexInstanceIdentity {
        CodexInstanceIdentity {
            instance_id: "test".into(),
            generation: 7,
            debug_port: 49101,
            helper_port: 49102,
            user_data_dir: root.into(),
            browser_id: "browser".into(),
            process_id: 123,
            process_birth_id: 456,
            executable: root.join("Codex.exe"),
        }
    }

    #[test]
    fn handoff_requires_every_ready_field_and_exact_options_path() {
        let temp = tempfile::tempdir().unwrap();
        let options = temp.path().join(OPTIONS);
        fs::write(&options, b"{}").unwrap();
        let ready = WorkerReady {
            protocol: HANDOFF_PROTOCOL,
            instance_id: "test".into(),
            generation: 7,
            pid: 100,
            worker_birth_id: Some(1000),
            token: "fresh".into(),
            options_path: options.canonicalize().unwrap(),
            identity: identity(temp.path()),
            binary: BinaryStamp {
                executable: temp.path().join("manager.exe"),
                size: 100,
                modified_ns: "2".into(),
                version: "test".into(),
            },
        };
        assert!(!handoff_requested(&options, &ready, || panic!(
            "no request"
        )));
        let request = request_fixture(&ready);
        assert!(request_matches(&request, &ready, &options, 1000));
        let value = serde_json::to_value(&ready).unwrap();
        for field in [
            "pid",
            "generation",
            "token",
            "optionsPath",
            "identity",
            "workerBirthId",
            "binary",
        ] {
            let mut invalid = value.clone();
            invalid[field] = serde_json::Value::Null;
            let mut invalid_request = serde_json::to_value(&request).unwrap();
            invalid_request["worker"] = invalid;
            write_json(&temp.path().join(REQUEST), &invalid_request).unwrap();
            assert!(
                read_json::<HandoffRequest>(&temp.path().join(REQUEST))
                    .is_none_or(|request| !request_matches(&request, &ready, &options, 1000)),
                "{field}"
            );
        }
        // v1 是直接比较完整 WorkerReady；v2 使用不同 envelope，旧版无法误消费。
        write_json(&temp.path().join(REQUEST), &ready).unwrap();
        assert!(read_json::<HandoffRequest>(&temp.path().join(REQUEST)).is_none());
        assert!(
            serde_json::from_value::<WorkerReady>(serde_json::to_value(&request).unwrap()).is_err()
        );
        let mut wrong = ready.clone();
        wrong.token = "stale".into();
        assert!(!request_matches(&request, &wrong, &options, 1000));
        assert!(!request_matches(
            &request,
            &ready,
            &temp.path().join("other"),
            1000
        ));
    }

    fn request_fixture(ready: &WorkerReady) -> HandoffRequest {
        HandoffRequest {
            protocol: HANDOFF_PROTOCOL,
            token: "request".into(),
            worker: ready.clone(),
            requester_pid: 999,
            requester_birth: "9999".into(),
            issued_at_ms: 1000,
            expires_at_ms: 1000 + HANDOFF_LIFETIME.as_millis() as u64,
            successor_token: "successor".into(),
            successor_binary: ready.binary.clone(),
        }
    }

    fn ready_fixture(root: &Path) -> WorkerReady {
        let options_path = root.join(OPTIONS);
        fs::write(&options_path, b"{}").unwrap();
        WorkerReady {
            protocol: HANDOFF_PROTOCOL,
            instance_id: "test".into(),
            generation: 7,
            pid: 100,
            worker_birth_id: Some(1000),
            token: "original".into(),
            options_path: options_path.canonicalize().unwrap(),
            identity: identity(root),
            binary: BinaryStamp {
                executable: root.join("manager.exe"),
                size: 100,
                modified_ns: "2".into(),
                version: "test".into(),
            },
        }
    }

    #[test]
    fn expired_future_overlong_and_legacy_requests_never_release_services() {
        let temp = tempfile::tempdir().unwrap();
        let ready = ready_fixture(temp.path());
        let request = request_fixture(&ready);
        assert!(!request_matches(&request, &ready, &ready.options_path, 999));
        assert!(!request_matches(
            &request,
            &ready,
            &ready.options_path,
            request.expires_at_ms
        ));
        let mut overlong = request.clone();
        overlong.expires_at_ms += 1;
        assert!(!request_matches(
            &overlong,
            &ready,
            &ready.options_path,
            1000
        ));
        let mut legacy = ready.clone();
        legacy.protocol = 1;
        let legacy_request = request_fixture(&legacy);
        assert!(!request_matches(
            &legacy_request,
            &legacy,
            &legacy.options_path,
            1000
        ));
        write_json(&temp.path().join(REQUEST), &request).unwrap();
        assert!(!handoff_requested(&ready.options_path, &ready, || panic!(
            "expired request"
        )));
        assert!(!temp.path().join(RELEASE).exists());
    }

    #[test]
    fn cancellation_revokes_only_its_request_and_process_or_lock_loss_invalidates_it() {
        let temp = tempfile::tempdir().unwrap();
        let ready = ready_fixture(temp.path());
        let mut request = request_fixture(&ready);
        request.requester_pid = std::process::id();
        request.requester_birth = process_birth(request.requester_pid).unwrap();
        request.issued_at_ms = current_timestamp_ms();
        request.expires_at_ms = request.issued_at_ms + HANDOFF_LIFETIME.as_millis() as u64;
        let lock = open_resume_lock(temp.path()).unwrap();
        lock.try_lock().unwrap();
        write_json(&temp.path().join(REQUEST), &request).unwrap();
        assert!(request_active(temp.path(), &request, &ready));
        let pending = PendingHandoff {
            root: temp.path().to_path_buf(),
            request: request.clone(),
        };
        drop(pending);
        assert!(!request_active(temp.path(), &request, &ready));
        write_json(&temp.path().join(REQUEST), &request).unwrap();
        drop(lock);
        assert!(!request_active(temp.path(), &request, &ready));
        let lock = open_resume_lock(temp.path()).unwrap();
        lock.try_lock().unwrap();
        let mut recycled = request.clone();
        recycled.requester_birth.push('0');
        write_json(&temp.path().join(REQUEST), &recycled).unwrap();
        assert!(!request_active(temp.path(), &recycled, &ready));
        let mut exited = request.clone();
        exited.requester_pid = u32::MAX;
        write_json(&temp.path().join(REQUEST), &exited).unwrap();
        assert!(!request_active(temp.path(), &exited, &ready));
        let mut replacement = request.clone();
        replacement.token = "different-request".into();
        write_json(&temp.path().join(REQUEST), &replacement).unwrap();
        drop(PendingHandoff {
            root: temp.path().to_path_buf(),
            request,
        });
        assert_eq!(
            read_json::<HandoffRequest>(&temp.path().join(REQUEST)),
            Some(replacement)
        );
    }

    #[test]
    fn cancellation_after_release_reclaims_only_its_candidate_and_retries_restore() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let ready = ready_fixture(root);
        let request = request_fixture(&ready);
        let original: CodexMultiWorkerOptions = serde_json::from_value(json!({
            "instanceId": "test", "generation": 7, "appPath": root,
            "userDataDir": root, "stateDir": root,
            "debugPort": 49101, "helperPort": 49102,
        }))
        .unwrap();
        let mut candidate = original.clone();
        candidate.resume_token = Some(request.successor_token.clone());
        candidate.resume_identity = Some(ready.identity.clone());
        write_json(&root.join(OPTIONS), &candidate).unwrap();
        let instance: CodexMultiInstance = serde_json::from_value(json!({
            "instanceId": "test", "generation": 7, "identity": ready.identity,
            "profileId": "test", "profileName": "test", "pid": ready.pid,
            "workerBirthId": ready.worker_birth_id, "debugPort": 49101, "helperPort": 49102,
            "status": "running", "running": true, "startedAtMs": 1, "isMain": false,
        }))
        .unwrap();
        save_multi_instance_manifest(root, &instance).unwrap();
        // manager 在 spawn 后、清单写入前退出：仅候选自己的持久化注册可发现它。
        let mut registered = ready.clone();
        registered.pid = 200;
        registered.worker_birth_id = Some(2000);
        registered.token = request.successor_token.clone();
        write_json(&root.join(CANDIDATE), &registered).unwrap();
        let release = HandoffRelease {
            request,
            options: original,
            released: true,
        };
        write_json(&root.join(RELEASE), &release).unwrap();
        let lock = open_resume_lock(root).unwrap();
        lock.try_lock().unwrap();
        assert!(
            restore_released_worker_with(
                root,
                &ready,
                &release,
                |_, _, _| panic!("manager still owns transaction"),
                |_| panic!("manager still owns transaction")
            )
            .is_err()
        );
        drop(lock);
        let stopped = std::cell::Cell::new(false);
        let result = restore_released_worker_with(
            root,
            &ready,
            &release,
            |pid, path, birth| {
                assert_eq!((pid, birth), (200, Some(2000)));
                assert_eq!(path, root.join(OPTIONS));
                stopped.set(true);
                Ok(())
            },
            |options| {
                assert!(stopped.get());
                assert!(options.resume_token.is_none());
                assert_eq!(load_multi_instance_manifest(root)?.pid, Some(ready.pid));
                anyhow::bail!("temporary attach failure")
            },
        );
        assert!(result.is_err());
        assert!(read_json::<HandoffRelease>(&root.join(RELEASE)).is_some());
        restore_released_worker_with(
            root,
            &ready,
            &release,
            |_, _, _| panic!("candidate already reclaimed"),
            |_| Ok(()),
        )
        .unwrap();
        assert!(read_json::<HandoffRelease>(&root.join(RELEASE)).is_none());
        assert_eq!(read_ready(root).unwrap(), Some(ready));
    }

    #[test]
    fn candidate_registration_rejects_lost_requester_and_commit_survives_revoke() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let old = ready_fixture(root);
        let mut request = request_fixture(&old);
        request.requester_pid = std::process::id();
        request.requester_birth = process_birth(request.requester_pid).unwrap();
        request.issued_at_ms = current_timestamp_ms();
        request.expires_at_ms = request.issued_at_ms + HANDOFF_LIFETIME.as_millis() as u64;
        let mut candidate = old.clone();
        candidate.pid = 200;
        candidate.worker_birth_id = Some(2000);
        candidate.token = request.successor_token.clone();
        let lock = open_resume_lock(root).unwrap();
        lock.try_lock().unwrap();
        write_json(&root.join(REQUEST), &request).unwrap();
        validate_candidate_request(&request, &candidate, true).unwrap();
        assert_eq!(
            read_json::<WorkerReady>(&root.join(CANDIDATE)),
            Some(candidate.clone())
        );
        // 模拟注册后 manager 退出/释放事务锁，发布 ready 必须失败。
        drop(lock);
        assert!(validate_candidate_request(&request, &candidate, false).is_err());
        let lock = open_resume_lock(root).unwrap();
        lock.try_lock().unwrap();
        for change in 0..3 {
            let mut invalid = request.clone();
            match change {
                0 => invalid.requester_birth.push('0'),
                1 => invalid.requester_pid = u32::MAX,
                _ => invalid.expires_at_ms = current_timestamp_ms(),
            }
            write_json(&root.join(REQUEST), &invalid).unwrap();
            assert!(validate_candidate_request(&invalid, &candidate, true).is_err());
        }
        write_json(&root.join(REQUEST), &serde_json::Value::Null).unwrap();
        assert!(validate_candidate_request(&request, &candidate, false).is_err());
        write_json(
            &root.join(COMMIT),
            &HandoffCommit {
                request: request.clone(),
                successor: candidate.clone(),
            },
        )
        .unwrap();
        // 已提交代次不再依赖 manager 的请求、生命周期或 deadline。
        drop(lock);
        validate_candidate_request(&request, &candidate, false).unwrap();
        candidate.worker_birth_id = Some(2001);
        assert!(validate_candidate_request(&request, &candidate, false).is_err());
    }

    #[test]
    fn successor_commit_requires_exact_generation_token_binary_and_identity() {
        let temp = tempfile::tempdir().unwrap();
        let ready = ready_fixture(temp.path());
        let request = request_fixture(&ready);
        let mut successor = ready.clone();
        successor.pid = 200;
        successor.worker_birth_id = Some(2000);
        successor.token = request.successor_token.clone();
        assert!(successor_matches(&request, &successor));
        for change in 0..5 {
            let mut wrong = successor.clone();
            match change {
                0 => wrong.generation += 1,
                1 => wrong.token = "foreign".into(),
                2 => wrong.binary.size += 1,
                3 => wrong.identity.process_birth_id += 1,
                _ => wrong.pid = ready.pid,
            }
            assert!(!successor_matches(&request, &wrong));
        }
    }

    #[test]
    fn committed_handoff_never_restores_old_services_but_revoked_release_does() {
        assert!(
            finish_handoff(true, false, || panic!("committed successor owns services")).unwrap()
        );
        assert!(!finish_handoff(false, true, || panic!("manager still holds lease")).unwrap());
        let restored = std::cell::Cell::new(false);
        assert!(
            !finish_handoff(false, false, || {
                restored.set(true);
                Ok(())
            })
            .unwrap()
        );
        assert!(restored.get());
        assert!(finish_handoff(false, false, || anyhow::bail!("retry restore")).is_err());
    }

    #[test]
    fn legacy_reuse_requires_healthy_helper_between_two_identity_checks() {
        let checks = std::cell::Cell::new(0);
        verify_legacy_services_with(
            || {
                checks.set(checks.get() + 1);
                Ok(true)
            },
            || {
                assert_eq!(checks.get(), 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(checks.get(), 2);
        assert!(
            verify_legacy_services_with(
                || Ok(false),
                || panic!("unverified process cannot authorize helper reuse"),
            )
            .is_err()
        );
        assert!(
            verify_legacy_services_with(
                || Ok(true),
                || anyhow::bail!("unhealthy helper; keep worker and defer"),
            )
            .is_err()
        );
        let checks = std::cell::Cell::new(0);
        assert!(
            verify_legacy_services_with(
                || {
                    checks.set(checks.get() + 1);
                    Ok(checks.get() == 1)
                },
                || Ok(()),
            )
            .is_err()
        );
    }

    #[test]
    fn non_windows_never_waits_on_existing_workers_before_starting_main_services() {
        let temp = tempfile::tempdir().unwrap();
        let worker = temp.path().join("live-worker");
        fs::create_dir(&worker).unwrap();
        // 即使存在损坏/不可核验的历史清单，非 Windows 的等待集合也必须为空。
        fs::write(worker.join("instance.json"), b"unknown legacy worker").unwrap();
        assert_eq!(worker_resume_roots(temp.path(), true), vec![worker]);
        assert!(worker_resume_roots(temp.path(), false).is_empty());
        assert!(worker_resume_roots(&temp.path().join("missing"), false).is_empty());
    }

    #[test]
    fn binary_stamp_does_not_reuse_replaced_same_path() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("manager.exe");
        fs::write(&exe, b"old").unwrap();
        let before = BinaryStamp::at(&exe).unwrap();
        fs::write(&exe, b"new worker build").unwrap();
        assert_ne!(before, BinaryStamp::at(&exe).unwrap());
    }

    #[test]
    fn legacy_options_default_to_normal_start_and_resume_requires_token() {
        let options: CodexMultiWorkerOptions = serde_json::from_value(json!({
            "appPath": "old", "debugPort": 1, "helperPort": 2,
        }))
        .unwrap();
        assert!(options.resume_identity.is_none());
        assert!(options.resume_token.is_none());
        let mut resume = options;
        resume.resume_identity = Some(identity(Path::new("unused")));
        assert!(validate_worker_resume(&resume).is_err());
    }

    #[test]
    fn dead_and_reused_worker_generations_do_not_block_main_restore() {
        use codex_plus_core::watcher::ProcessInstanceState;
        assert!(!generation_may_be_alive(
            Some(1),
            ProcessInstanceState::NotRunning
        ));
        assert!(!generation_may_be_alive(
            Some(1),
            ProcessInstanceState::Running {
                started_at_secs: None,
                birth_id: Some("2".into()),
            }
        ));
        assert!(generation_may_be_alive(
            Some(1),
            ProcessInstanceState::Running {
                started_at_secs: None,
                birth_id: Some("1".into()),
            }
        ));
        assert!(generation_may_be_alive(
            Some(1),
            ProcessInstanceState::Unknown
        ));
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("instance.json"),
            b"invalid historical json",
        )
        .unwrap();
        assert!(!worker_could_hold_handles(temp.path()));
    }

    #[test]
    fn cancelled_or_expired_stage_stops_before_next_side_effect() {
        assert!(check_active(&AtomicBool::new(true), Instant::now() + STAGE_TIMEOUT).is_err());
        assert!(check_active(&AtomicBool::new(false), Instant::now()).is_err());
    }
}
