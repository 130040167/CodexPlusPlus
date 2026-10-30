use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use super::media::MAX_ATTACHMENT_BYTES;
pub const ATTACHMENT_TOOL: &str = "weixin_send_attachment";

#[derive(Clone, Debug, PartialEq, Eq)]
struct PolicyState {
    enabled: bool,
    peers: String,
    workspace: PathBuf,
    revision: u64,
}

#[derive(Clone, Debug)]
pub struct OutboundPolicy(watch::Sender<PolicyState>);

#[derive(Clone, Debug)]
pub(super) struct OutboundPermit {
    policy: OutboundPolicy,
    state: PolicyState,
    pub workspace: PathBuf,
    workspace_identity: (u64, u64),
    peer: String,
}

impl OutboundPolicy {
    pub fn new(enabled: bool, peers: &str, workspace: &str) -> Self {
        Self(watch::Sender::new(PolicyState {
            enabled,
            peers: peers.trim().to_string(),
            workspace: PathBuf::from(workspace.trim()),
            revision: 0,
        }))
    }

    pub fn set(&self, enabled: bool, peers: &str, workspace: &str) {
        self.0.send_if_modified(|state| {
            if state.enabled == enabled
                && state.peers == peers.trim()
                && state.workspace == Path::new(workspace.trim())
            {
                return false;
            }
            state.enabled = enabled;
            state.peers = peers.trim().to_string();
            state.workspace = PathBuf::from(workspace.trim());
            state.revision = state.revision.wrapping_add(1);
            true
        });
    }

    pub(super) fn permit(&self, peer: &str) -> anyhow::Result<OutboundPermit> {
        let state = self.0.borrow().clone();
        if !state.enabled {
            bail!("微信附件回传未开启");
        }
        validate_allow_from(&state.peers)?;
        if peer.trim().is_empty() || !super::is_allowed_peer(&state.peers, peer) {
            bail!("附件接收方不在允许发送方名单中");
        }
        let workspace = outbound_workspace(&state.workspace)?;
        let workspace_identity = file_identity(&open_directory(&workspace)?)?;
        Ok(OutboundPermit {
            policy: self.clone(),
            state,
            workspace,
            workspace_identity,
            peer: peer.to_string(),
        })
    }
}

pub fn validate_allow_from(peers: &str) -> anyhow::Result<()> {
    if !peers.split(',').any(|peer| !peer.trim().is_empty())
        || peers.split(',').any(|peer| peer.trim() == "*")
    {
        bail!("开启微信附件回传前必须填写允许的微信用户 ID");
    }
    Ok(())
}

pub fn validate_settings(enabled: bool, peers: &str, workspace: &str) -> anyhow::Result<()> {
    if enabled {
        validate_allow_from(peers)?;
        outbound_workspace(Path::new(workspace.trim()))?;
    }
    Ok(())
}

pub(super) fn outbound_workspace(workspace: &Path) -> anyhow::Result<PathBuf> {
    if workspace.as_os_str().is_empty() {
        bail!("外发附件必须显式设置工作目录");
    }
    reject_sensitive_path(workspace)?;
    let root = workspace.canonicalize().context("无法解析附件工作区")?;
    if !root.is_dir() || root.parent().is_none() {
        bail!("附件工作区必须是非根目录");
    }
    let mut homes = directories::BaseDirs::new()
        .map(|dirs| vec![dirs.home_dir().to_path_buf()])
        .unwrap_or_default();
    homes.extend(
        ["USERPROFILE", "HOME"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from),
    );
    for home in homes {
        if let Ok(home) = home.canonicalize()
            && home.starts_with(&root)
        {
            bail!("附件工作区不能是 home 或其祖先目录");
        }
    }
    reject_sensitive_path(&root)?;
    reject_state_path(&root)?;
    Ok(root)
}

fn reject_state_path(path: &Path) -> anyhow::Result<()> {
    let mut states = vec![
        crate::paths::default_app_state_dir(),
        crate::relay_config::default_codex_home_dir(),
    ];
    states.extend(
        [
            "CODEX_HOME",
            "CODEX_SQLITE_HOME",
            "CODEX_PLUS_STATE_DIR",
            "CODEX_ELECTRON_USER_DATA_PATH",
        ]
        .into_iter()
        .filter_map(std::env::var_os)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from),
    );
    if let Some(appdata) = std::env::var_os("APPDATA") {
        states.extend([
            PathBuf::from(&appdata).join("Codex++"),
            PathBuf::from(appdata).join("Codex"),
        ]);
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        states.extend([
            PathBuf::from(&local).join("OpenAI").join("Codex"),
            PathBuf::from(local).join("Codex"),
        ]);
    }
    require_outside_states(path, states)
}

fn require_outside_states(
    path: &Path,
    states: impl IntoIterator<Item = PathBuf>,
) -> anyhow::Result<()> {
    for state in states {
        if let Ok(state) = state.canonicalize()
            && path.starts_with(state)
        {
            bail!("不能外发 Codex 或管理器状态目录中的文件");
        }
    }
    Ok(())
}

fn reject_sensitive_path(path: &Path) -> anyhow::Result<()> {
    for component in path.components() {
        if let Component::Normal(name) = component {
            let name = name.to_string_lossy().to_ascii_lowercase();
            let name = name.trim_end_matches([' ', '.']);
            if name.contains(':')
                || name.starts_with(".env")
                || name.ends_with(".pem")
                || name.ends_with(".key")
                || name.ends_with(".p12")
                || name.ends_with(".pfx")
                || [
                    "auth.json",
                    "config.toml",
                    "settings.json",
                    "credentials",
                    "credentials.json",
                    "secrets.json",
                    ".codex",
                    ".ssh",
                    ".aws",
                    ".gnupg",
                    ".azure",
                    ".kube",
                    "id_rsa",
                    "id_ed25519",
                    "id_ecdsa",
                ]
                .iter()
                .any(|sensitive| {
                    name.strip_prefix(sensitive)
                        .is_some_and(|suffix| suffix.starts_with(['.', '-', '_']))
                })
                || matches!(
                    name,
                    ".git"
                        | ".ssh"
                        | ".gnupg"
                        | ".aws"
                        | ".azure"
                        | ".kube"
                        | ".config"
                        | ".codex"
                        | ".codex-session-delete"
                        | "codex++"
                        | "auth.json"
                        | "config.toml"
                        | "settings.json"
                        | "credentials"
                        | "credentials.json"
                        | "secrets.json"
                        | ".npmrc"
                        | ".netrc"
                        | ".pypirc"
                        | "id_rsa"
                        | "id_ed25519"
                        | "id_ecdsa"
                        | "login.keychain-db"
                )
            {
                bail!("不能外发敏感配置、凭据或状态目录中的文件");
            }
        }
    }
    Ok(())
}

fn open_directory(path: &Path) -> anyhow::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        return Ok(std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7)
            .custom_flags(0x02000000 | 0x00200000)
            .open(path)?);
    }
    #[cfg(not(windows))]
    open_verified(path)
}

fn require_path(path: &Path, root: &Path) -> anyhow::Result<()> {
    if path == root || !path.starts_with(root) {
        bail!("附件必须位于当前工作区内");
    }
    reject_sensitive_path(path)?;
    reject_state_path(path)
}

pub(super) fn read_workspace_file(path: &Path, root: &Path) -> anyhow::Result<(PathBuf, Vec<u8>)> {
    let root = outbound_workspace(root)?;
    let root_identity = file_identity(&open_directory(&root)?)?;
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    reject_sensitive_path(&candidate)?;
    let canonical = candidate.canonicalize().context("无法解析附件路径")?;
    require_path(&canonical, &root)?;
    let mut file = open_verified(&canonical)?;
    let before = file.metadata()?;
    if !before.is_file() || before.len() > MAX_ATTACHMENT_BYTES as u64 {
        bail!("附件必须是不超过 100 MiB 的普通文件");
    }
    verify_file(&file, &canonical, &root)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(MAX_ATTACHMENT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.ctime(), before.ctime_nsec()) != (after.ctime(), after.ctime_nsec()) {
            bail!("附件内容在读取期间发生变化");
        }
    }
    if bytes.len() > MAX_ATTACHMENT_BYTES
        || bytes.len() as u64 != before.len()
        || before.len() != after.len()
        || before.modified()? != after.modified()?
    {
        bail!("附件内容或大小在读取期间发生变化");
    }
    verify_file(&file, &canonical, &root)?;
    if candidate.canonicalize()? != canonical
        || file_identity(&open_directory(&root)?)? != root_identity
    {
        bail!("附件路径在读取期间发生变化");
    }
    Ok((canonical, bytes))
}

#[cfg(windows)]
fn open_verified(path: &Path) -> anyhow::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    Ok(std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .custom_flags(0x00200000)
        .open(path)?)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn open_verified(path: &Path) -> anyhow::Result<File> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    const FLAGS: i32 = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let mut directory = File::open("/")?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let name = std::ffi::CString::new(name.as_bytes())?;
        let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), FLAGS) };
        if descriptor < 0 {
            bail!("附件路径无法访问或在核验期间变为链接");
        }
        directory = unsafe { File::from_raw_fd(descriptor) };
    }
    Ok(directory)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn open_verified(_path: &Path) -> anyhow::Result<File> {
    bail!("当前平台不支持附件句柄核验")
}

#[cfg(windows)]
fn file_identity(file: &File) -> anyhow::Result<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle},
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info)?;
    }
    if file.metadata()?.is_file() && info.nNumberOfLinks != 1 {
        bail!("不能外发具有多个硬链接的文件");
    }
    Ok((
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    ))
}

#[cfg(unix)]
fn file_identity(file: &File) -> anyhow::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    if metadata.is_file() && metadata.nlink() != 1 {
        bail!("不能外发具有多个硬链接的文件");
    }
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(not(any(windows, unix)))]
fn file_identity(_file: &File) -> anyhow::Result<(u64, u64)> {
    bail!("当前平台不支持附件文件身份核验")
}

fn verify_file(file: &File, path: &Path, root: &Path) -> anyhow::Result<()> {
    require_path(&path.canonicalize()?, root)?;
    let target = open_verified(path)?;
    if !target.metadata()?.is_file() || file_identity(file)? != file_identity(&target)? {
        bail!("附件文件身份已改变");
    }
    #[cfg(windows)]
    {
        use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{FILE_NAME_NORMALIZED, GetFinalPathNameByHandleW},
        };
        let mut buffer = vec![0u16; 32768];
        let length = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(file.as_raw_handle()),
                &mut buffer,
                FILE_NAME_NORMALIZED,
            )
        } as usize;
        if length == 0 || length >= buffer.len() {
            bail!("无法核验附件句柄的实际路径");
        }
        let actual = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
        require_path(&actual, root)?;
        if actual != path {
            bail!("附件句柄路径与核准路径不同");
        }
    }
    Ok(())
}

impl OutboundPermit {
    pub fn check_peer(&self, peer: &str) -> anyhow::Result<()> {
        self.check()?;
        if self.peer != peer {
            bail!("附件回传授权不属于当前联系人");
        }
        Ok(())
    }
    pub fn check(&self) -> anyhow::Result<()> {
        if *self.policy.0.borrow() != self.state {
            bail!("附件回传权限已改变，请在新回合重试");
        }
        if outbound_workspace(&self.state.workspace)? != self.workspace
            || file_identity(&open_directory(&self.workspace)?)? != self.workspace_identity
        {
            bail!("附件工作区身份已改变");
        }
        Ok(())
    }

    pub async fn guard<T>(
        &self,
        future: impl std::future::Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        let mut changes = self.policy.0.subscribe();
        self.check()?;
        tokio::select! {
            biased;
            _ = changes.changed() => bail!("附件回传权限已改变，已取消发送"),
            result = future => { self.check()?; result }
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct AttachmentArguments {
    pub path: String,
    pub kind: String,
}

pub(super) struct AttachmentCalls {
    pub permit: Option<OutboundPermit>,
    peer: String,
    thread: String,
    pub turn: Option<String>,
    results: HashMap<String, (AttachmentArguments, Value)>,
    attempts: usize,
}

impl AttachmentCalls {
    pub fn new(peer: &str, thread: &str, permit: Option<OutboundPermit>) -> Self {
        Self {
            permit,
            peer: peer.to_string(),
            thread: thread.to_string(),
            turn: None,
            results: HashMap::new(),
            attempts: 0,
        }
    }

    pub fn validate(
        &self,
        message: &Value,
    ) -> anyhow::Result<(String, AttachmentArguments, Option<Value>)> {
        self.permit
            .as_ref()
            .context("微信附件回传未开启")?
            .check_peer(&self.peer)?;
        let params = &message["params"];
        if message["method"] != "item/tool/call"
            || params["tool"] != ATTACHMENT_TOOL
            || !params["namespace"].is_null()
        {
            bail!("未知附件工具");
        }
        if params["threadId"].as_str() != Some(self.thread.as_str())
            || self.turn.is_none()
            || params["turnId"].as_str() != self.turn.as_deref()
        {
            bail!("附件调用不属于当前微信线程或回合");
        }
        let call = params["callId"]
            .as_str()
            .filter(|call| !call.trim().is_empty() && call.len() <= 128)
            .context("附件调用缺少有效的调用标识")?
            .to_string();
        let arguments: AttachmentArguments =
            serde_json::from_value(params["arguments"].clone()).context("附件参数无效")?;
        if arguments.path.trim().is_empty()
            || arguments.path.len() > 32768
            || !matches!(arguments.kind.as_str(), "image" | "file")
        {
            bail!("附件必须提供有效路径及 image 或 file 类型");
        }
        let cached = if let Some((original, result)) = self.results.get(&call) {
            if original != &arguments {
                bail!("同一附件调用标识不能用于不同参数");
            }
            Some(result.clone())
        } else {
            None
        };
        Ok((call, arguments, cached))
    }

    pub fn begin_send(&mut self) -> anyhow::Result<()> {
        if self.attempts >= 10 {
            bail!("每回合最多回传 10 个附件");
        }
        self.attempts += 1;
        Ok(())
    }

    pub fn record(&mut self, call: String, arguments: AttachmentArguments, result: Value) {
        if self.results.len() < 32 {
            self.results.insert(call, (arguments, result));
        }
    }
}

pub(super) fn attachment_tool_spec() -> Value {
    json!({
        "name": ATTACHMENT_TOOL,
        "description": "将当前工作区中的图片或文件回传给当前微信用户。用户请求接收文件时实际调用本工具；不得只回复本地路径。kind=image 显示图片，kind=file 发送文件。",
        "inputSchema": {
            "type": "object", "properties": {
                "path": {"type": "string"}, "kind": {"type": "string", "enum": ["image", "file"]}
            }, "required": ["path", "kind"], "additionalProperties": false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(thread: &str, turn: &str, id: &str) -> Value {
        json!({ "id": 1, "method": "item/tool/call", "params": {
            "threadId": thread, "turnId": turn, "callId": id, "tool": ATTACHMENT_TOOL,
            "namespace": null, "arguments": { "path": "result.txt", "kind": "file" }
        }})
    }

    #[test]
    fn outbound_cache_requires_same_peer_thread_turn_call_and_arguments() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().to_str().unwrap();
        let policy = OutboundPolicy::new(true, "peer,other", root);
        let permit = policy.permit("peer").unwrap();
        let mut calls = AttachmentCalls::new("peer", "thread", Some(permit.clone()));
        let request = call("thread", "turn", "call");
        assert!(calls.validate(&request).is_err());
        calls.turn = Some("turn".to_string());
        let (id, arguments, cached) = calls.validate(&request).unwrap();
        assert!(cached.is_none());
        calls.record(id, arguments, json!({"success": true}));
        assert_eq!(
            calls.validate(&request).unwrap().2,
            Some(json!({"success": true}))
        );
        for invalid in [
            call("other", "turn", "call"),
            call("thread", "other", "call"),
            call("thread", "turn", ""),
        ] {
            assert!(calls.validate(&invalid).is_err());
        }
        let mut invalid = request.clone();
        invalid["params"]["arguments"]["path"] = json!("other.txt");
        assert!(calls.validate(&invalid).is_err());
        invalid = request.clone();
        invalid["params"]["tool"] = json!("other");
        assert!(calls.validate(&invalid).is_err());
        invalid = request.clone();
        invalid["params"]["namespace"] = json!("other");
        assert!(calls.validate(&invalid).is_err());
        invalid = request.clone();
        invalid["params"]["arguments"]["to_user_id"] = json!("other");
        assert!(calls.validate(&invalid).is_err());
        let mut wrong_peer = AttachmentCalls::new("other", "thread", Some(permit));
        wrong_peer.turn = Some("turn".to_string());
        assert!(wrong_peer.validate(&request).is_err());
        policy.set(false, "peer,other", root);
        assert!(calls.validate(&request).is_err());
        policy.set(true, "peer,other", root);
        assert!(calls.validate(&request).is_err());
        let mut next = AttachmentCalls::new("peer", "thread", policy.permit("peer").ok());
        next.turn = Some("next".to_string());
        assert!(
            next.validate(&call("thread", "next", "call"))
                .unwrap()
                .2
                .is_none()
        );
    }

    #[test]
    fn outbound_replaced_workspace_invalidates_existing_permit() {
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let policy = OutboundPolicy::new(true, "peer", workspace.to_str().unwrap());
        let permit = policy.permit("peer").unwrap();
        std::fs::rename(&workspace, parent.path().join("old-workspace")).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        assert!(permit.check().is_err());
    }

    #[test]
    fn outbound_defaults_and_empty_allowlist_are_denied() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().to_str().unwrap();
        assert!(
            OutboundPolicy::new(false, "peer", root)
                .permit("peer")
                .is_err()
        );
        for peers in ["", " , \n", "other", "*", "peer,*"] {
            assert!(
                OutboundPolicy::new(true, peers, root)
                    .permit("peer")
                    .is_err()
            );
        }
        assert!(OutboundPolicy::new(true, "peer", root).permit("").is_err());
        assert!(validate_settings(false, "", "").is_ok());
        assert!(validate_settings(true, "", root).is_err());
        assert!(validate_settings(true, "peer", "").is_err());
        assert!(validate_settings(true, "peer", root).is_ok());
    }

    #[tokio::test]
    async fn outbound_revoke_cancels_inflight_and_invalidates_old_permit() {
        let workspace = tempfile::tempdir().unwrap();
        let root = workspace.path().to_str().unwrap();
        let policy = OutboundPolicy::new(true, "peer", root);
        let permit = policy.permit("peer").unwrap();
        let old = permit.clone();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            permit
                .guard(async {
                    let _ = entered.send(());
                    std::future::pending::<anyhow::Result<()>>().await
                })
                .await
        });
        ready.await.unwrap();
        policy.set(false, "peer", root);
        assert!(task.await.unwrap().is_err());
        policy.set(true, "peer", root);
        assert!(old.check().is_err());
        assert!(policy.permit("peer").is_ok());
    }

    #[test]
    fn outbound_changed_recipient_and_workspace_revoke_permits() {
        let workspace = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let root = workspace.path().to_str().unwrap();
        let policy = OutboundPolicy::new(true, "peer", root);
        let permit = policy.permit("peer").unwrap();
        policy.set(true, "other", root);
        assert!(permit.check().is_err());
        assert!(policy.permit("peer").is_err());
        let permit = policy.permit("other").unwrap();
        policy.set(true, "other", other.path().to_str().unwrap());
        assert!(permit.check().is_err());
    }

    #[test]
    fn outbound_rejects_broad_roots_and_sensitive_components() {
        assert!(outbound_workspace(Path::new("")).is_err());
        if let Some(dirs) = directories::BaseDirs::new() {
            let home = dirs.home_dir().canonicalize().unwrap();
            for ancestor in home.ancestors() {
                assert!(outbound_workspace(ancestor).is_err(), "{ancestor:?}");
            }
        }
        for name in [
            ".codex/result.txt",
            ".ssh/key",
            ".aws/credentials",
            ".env.local",
            "AUTH.JSON",
            "config.toml",
            "key.pem",
            "settings.json",
            "settings.json.bak",
            ".ssh-backup/key",
            ".codex.bak/result.txt",
            "file:stream",
            ".git/config",
        ] {
            assert!(reject_sensitive_path(Path::new(name)).is_err(), "{name}");
        }
    }

    #[test]
    fn outbound_reads_only_the_verified_workspace_file() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let path = workspace.path().join("result.txt");
        std::fs::write(&path, b"result").unwrap();
        let (actual, bytes) =
            read_workspace_file(Path::new("result.txt"), workspace.path()).unwrap();
        assert_eq!(actual, path.canonicalize().unwrap());
        assert_eq!(bytes, b"result");
        assert!(read_workspace_file(outside.path(), workspace.path()).is_err());
        assert!(read_workspace_file(Path::new("."), workspace.path()).is_err());
        assert!(read_workspace_file(Path::new("missing"), workspace.path()).is_err());
        let wrong_handle = File::open(outside.path()).unwrap();
        assert!(
            verify_file(
                &wrong_handle,
                &actual,
                &workspace.path().canonicalize().unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn outbound_rejects_hardlink_aliases_and_oversized_files() {
        let workspace = tempfile::tempdir().unwrap();
        let original = workspace.path().join("original.txt");
        let alias = workspace.path().join("alias.txt");
        std::fs::write(&original, b"secret").unwrap();
        std::fs::hard_link(&original, &alias).unwrap();
        assert!(read_workspace_file(&alias, workspace.path()).is_err());
        let large = workspace.path().join("large.bin");
        File::create(&large)
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES as u64 + 1)
            .unwrap();
        assert!(read_workspace_file(&large, workspace.path()).is_err());
    }

    #[test]
    fn outbound_rejects_sensitive_files_inside_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        for name in [".codex", ".ssh", ".aws", "Codex++"] {
            let directory = workspace.path().join(name);
            std::fs::create_dir(&directory).unwrap();
            std::fs::write(directory.join("result.txt"), b"private").unwrap();
            assert!(outbound_workspace(&directory).is_err());
            assert!(read_workspace_file(&directory.join("result.txt"), workspace.path()).is_err());
        }
    }

    #[test]
    fn outbound_rejects_custom_state_directories_without_global_environment_changes() {
        let parent = tempfile::tempdir().unwrap();
        let state = parent.path().join("custom-state");
        std::fs::create_dir(&state).unwrap();
        let path = state.join("result.txt");
        std::fs::write(&path, b"private").unwrap();
        let canonical = path.canonicalize().unwrap();
        assert!(require_outside_states(&canonical, [state.clone()]).is_err());
        assert!(require_outside_states(&state.canonicalize().unwrap(), [state]).is_err());
        assert!(
            require_outside_states(&parent.path().canonicalize().unwrap(), [canonical]).is_ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn outbound_rejects_symlink_escape_and_replaced_file_identity() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let alias = workspace.path().join("alias");
        std::os::unix::fs::symlink(outside.path(), &alias).unwrap();
        assert!(read_workspace_file(&alias, workspace.path()).is_err());
        let original = workspace.path().join("result.txt");
        std::fs::write(&original, b"original").unwrap();
        let root = workspace.path().canonicalize().unwrap();
        let canonical = original.canonicalize().unwrap();
        let handle = open_verified(&canonical).unwrap();
        std::fs::rename(&original, workspace.path().join("old.txt")).unwrap();
        std::fs::write(&original, b"replacement").unwrap();
        assert!(verify_file(&handle, &canonical, &root).is_err());
    }
}
