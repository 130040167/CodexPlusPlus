use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::media::LocalAttachment;

const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_DIRECTORIES: usize = 100;
const MARKER: &str = ".weixin-media.json";

#[derive(Deserialize, Serialize)]
struct Manifest {
    version: u8,
    directory: String,
    files: Vec<(String, u64, u64)>,
}

// Windows 持有不共享删除权限的父目录；Unix 的读写/删除始终相对目录句柄。
struct Directory {
    path: PathBuf,
    _file: File,
}

impl Directory {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let file = open_path(path, true, false)?;
        Ok(Self {
            path: path.into(),
            _file: file,
        })
    }

    fn child(&self, name: &str) -> anyhow::Result<Self> {
        let file = self.open_file(name, true, false)?;
        Ok(Self {
            path: self.path.join(name),
            _file: file,
        })
    }

    fn create_child(&self, name: &str) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = std::ffi::CString::new(name)?;
            if unsafe { libc::mkdirat(self._file.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        std::fs::create_dir(self.path.join(name))
    }

    fn open_file(&self, name: &str, directory: bool, create: bool) -> anyhow::Result<File> {
        if !matches!(
            Path::new(name).components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ) {
            bail!("附件目录包含无效文件名");
        }
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            let name = std::ffi::CString::new(name)?;
            let flags = libc::O_CLOEXEC
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | if directory { libc::O_DIRECTORY } else { 0 }
                | if create {
                    libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL
                } else {
                    libc::O_RDONLY
                };
            let fd = unsafe { libc::openat(self._file.as_raw_fd(), name.as_ptr(), flags, 0o600) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            let metadata = file.metadata()?;
            if metadata.is_dir() != directory || (!directory && !metadata.is_file()) {
                bail!("附件目录存在特殊文件");
            }
            Ok(file)
        }
        #[cfg(not(unix))]
        open_path(&self.path.join(name), directory, create)
    }

    fn remove(&self, name: &str, directory: bool) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = std::ffi::CString::new(name)?;
            if unsafe {
                libc::unlinkat(
                    self._file.as_raw_fd(),
                    name.as_ptr(),
                    if directory { libc::AT_REMOVEDIR } else { 0 },
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(unix))]
        if directory {
            std::fs::remove_dir(self.path.join(name))?;
        } else {
            std::fs::remove_file(self.path.join(name))?;
        }
        Ok(())
    }

    fn publish(&self, temporary: &str, filename: &str) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let temporary = std::ffi::CString::new(temporary)?;
            let filename = std::ffi::CString::new(filename)?;
            let descriptor = self._file.as_raw_fd();
            if unsafe {
                libc::linkat(
                    descriptor,
                    temporary.as_ptr(),
                    descriptor,
                    filename.as_ptr(),
                    0,
                )
            } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(unix))]
        std::fs::hard_link(self.path.join(temporary), self.path.join(filename))?;
        Ok(())
    }
}

fn open_path(path: &Path, directory: bool, create: bool) -> anyhow::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(!create).write(create).create_new(create);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options
            .share_mode(1 | 2)
            .custom_flags(0x0020_0000 | 0x0200_0000);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(
            libc::O_NOFOLLOW | libc::O_NONBLOCK | if directory { libc::O_DIRECTORY } else { 0 },
        );
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("附件目录不允许链接或重解析点");
        }
    }
    if metadata.is_dir() != directory || (!directory && !metadata.is_file()) {
        bail!("附件目录存在特殊文件");
    }
    Ok(file)
}

pub(super) struct Inbox {
    // 保持整个目录链的句柄直到下载和登记完成。
    _parents: Vec<Directory>,
    directory: Directory,
}

impl Inbox {
    pub(super) fn create(workspace: &Path) -> anyhow::Result<Self> {
        let root = workspace.canonicalize().context("无法读取微信工作目录")?;
        let mut parents = vec![Directory::open(&root)?];
        for name in [".codex-plus", "weixin", "inbox"] {
            let parent = parents.last().unwrap();
            match parent.create_child(name) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
            parents.push(parent.child(name).context("微信附件目录不允许链接")?);
        }
        let inbox = parents.last().unwrap();
        cleanup(inbox, SystemTime::now(), MAX_DIRECTORIES - 1)?;
        let name = uuid::Uuid::new_v4().to_string();
        inbox.create_child(&name)?;
        let directory = inbox.child(&name)?;
        let result = Self {
            _parents: parents,
            directory,
        };
        result.record(&[])?;
        Ok(result)
    }

    pub(super) fn path(&self) -> &Path {
        &self.directory.path
    }

    pub(super) async fn save(&self, filename: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
        if !matches!(
            Path::new(filename)
                .components()
                .collect::<Vec<_>>()
                .as_slice(),
            [Component::Normal(_)]
        ) || filename.contains(['/', '\\', ':'])
        {
            bail!("附件文件名必须是单个路径组件");
        }
        let filename = format!("{}-{filename}", uuid::Uuid::new_v4().simple());
        let temporary = format!(".{}.part", uuid::Uuid::new_v4().simple());
        let file = self.directory.open_file(&temporary, false, true)?;
        let mut file = tokio::fs::File::from_std(file);
        let saved = async {
            file.write_all(bytes).await?;
            file.flush().await?;
            file.sync_all().await
        }
        .await;
        drop(file);
        if let Err(error) = saved {
            let _ = self.directory.remove(&temporary, false);
            return Err(error).context("无法写入微信附件");
        }
        let published = self.directory.publish(&temporary, &filename);
        let removed = self.directory.remove(&temporary, false);
        published.context("无法原子保存微信附件")?;
        removed.context("无法移除微信附件临时文件")?;
        Ok(self.path().join(filename))
    }

    pub(super) fn record(&self, attachments: &[LocalAttachment]) -> anyhow::Result<()> {
        let mut files = Vec::new();
        for attachment in attachments {
            let parent = attachment.path.parent().context("附件缺少收件目录")?;
            if parent.canonicalize()? != self.path() {
                bail!("附件不属于本次收件目录");
            }
            let name = attachment
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .context("附件文件名无效")?;
            let file = self.directory.open_file(name, false, false)?;
            let metadata = file.metadata()?;
            files.push((name.to_owned(), metadata.len(), modified_nanos(&metadata)?));
        }
        let marker = Manifest {
            version: 1,
            directory: self
                .path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            files,
        };
        // 已有标记只在本次持有目录链期间更新；清理从不推断旧目录的归属。
        if self.directory.open_file(MARKER, false, false).is_ok() {
            self.directory.remove(MARKER, false)?;
        }
        let mut file = self.directory.open_file(MARKER, false, true)?;
        file.write_all(&serde_json::to_vec(&marker)?)?;
        file.sync_all()?;
        Ok(())
    }
}

fn modified_nanos(metadata: &std::fs::Metadata) -> anyhow::Result<u64> {
    Ok(metadata
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos()
        .try_into()?)
}

fn owned_manifest(directory: &Directory, name: &str) -> anyhow::Result<(Manifest, SystemTime)> {
    if uuid::Uuid::parse_str(name).is_err() {
        bail!("不是微信生成的目录");
    }
    let file = directory.open_file(MARKER, false, false)?;
    let metadata = file.metadata()?;
    if metadata.len() > 64 * 1024 {
        bail!("收件目录标记过大");
    }
    let marker: Manifest = serde_json::from_reader(file.take(64 * 1024 + 1))?;
    if marker.version != 1 || marker.directory != name || marker.files.len() > 10 {
        bail!("收件目录标记无效");
    }
    let entries = std::fs::read_dir(&directory.path)?.collect::<Result<Vec<_>, _>>()?;
    if entries.len() != marker.files.len() + 1 {
        bail!("收件目录含有未登记内容");
    }
    let mut names = std::collections::HashSet::new();
    for (name, size, modified) in &marker.files {
        if name == MARKER || !names.insert(name) {
            bail!("收件标记包含重复文件");
        }
        let file = directory.open_file(name, false, false)?;
        let metadata = file.metadata()?;
        if metadata.len() != *size || modified_nanos(&metadata)? != *modified {
            bail!("收件文件已被修改");
        }
    }
    Ok((marker, metadata.modified()?))
}

fn cleanup(inbox: &Directory, now: SystemTime, retain: usize) -> anyhow::Result<()> {
    let mut owned = Vec::new();
    for entry in std::fs::read_dir(&inbox.path)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(directory) = inbox.child(&name) else {
            continue;
        };
        if let Ok((_, modified)) = owned_manifest(&directory, &name) {
            owned.push((modified, name));
        }
    }
    owned.sort();
    let excess = owned.len().saturating_sub(retain);
    for (index, (modified, name)) in owned.into_iter().enumerate() {
        if index >= excess && now.duration_since(modified).unwrap_or_default() <= RETENTION {
            continue;
        }
        let directory = inbox.child(&name)?;
        let Ok((marker, current_modified)) = owned_manifest(&directory, &name) else {
            continue;
        };
        if current_modified != modified {
            continue;
        }
        for (filename, _, _) in marker.files {
            directory.remove(&filename, false)?;
        }
        directory.remove(MARKER, false)?;
        drop(directory);
        // 仅移除空目录；额外内容永不递归删除。
        inbox.remove(&name, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::media::AttachmentKind;
    use super::*;

    #[tokio::test]
    async fn atomic_save_and_record_keep_unique_complete_files_without_temporary_entries() {
        let root = tempfile::tempdir().unwrap();
        let inbox = Inbox::create(root.path()).unwrap();
        let payload = vec![42; 128 * 1024];
        let first = inbox.save("report.txt", &payload).await.unwrap();
        let second = inbox.save("report.txt", b"second").await.unwrap();
        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), payload);
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
        let attachments = [first, second].map(|path| LocalAttachment {
            path,
            kind: AttachmentKind::File,
        });
        inbox.record(&attachments).unwrap();
        let (marker, _) = owned_manifest(
            &inbox.directory,
            inbox.path().file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        assert_eq!(marker.files.len(), 2);
        assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 3);
        assert!(std::fs::read_dir(inbox.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part")
        }));
    }

    #[tokio::test]
    async fn rejects_path_components_and_recording_files_from_other_directories() {
        let root = tempfile::tempdir().unwrap();
        let inbox = Inbox::create(root.path()).unwrap();
        for name in [
            "",
            ".",
            "..",
            "../escape",
            "sub/file",
            "sub\\file",
            "C:escape",
        ] {
            assert!(inbox.save(name, b"invalid").await.is_err(), "{name}");
        }
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, b"keep").unwrap();
        assert!(
            inbox
                .record(&[LocalAttachment {
                    path: outside.clone(),
                    kind: AttachmentKind::File,
                }])
                .is_err()
        );
        assert_eq!(std::fs::read(outside).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn record_accepts_canonical_alias_of_the_same_inbox() {
        let root = tempfile::tempdir().unwrap();
        let inbox = Inbox::create(root.path()).unwrap();
        let saved = inbox.save("file.txt", b"owned").await.unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(inbox.path(), &alias).unwrap();
        inbox
            .record(&[LocalAttachment {
                path: alias.join(saved.file_name().unwrap()),
                kind: AttachmentKind::File,
            }])
            .unwrap();
        assert!(
            owned_manifest(
                &inbox.directory,
                inbox.path().file_name().unwrap().to_str().unwrap(),
            )
            .is_ok()
        );
    }

    #[test]
    fn count_limit_includes_the_new_directory_and_preserves_unowned_content() {
        let root = tempfile::tempdir().unwrap();
        let first = Inbox::create(root.path()).unwrap();
        let inbox = first.path().parent().unwrap().to_path_buf();
        let foreign = inbox.join("user-files");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("keep.txt"), b"keep").unwrap();
        let unmarked = inbox.join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir(&unmarked).unwrap();
        std::fs::write(unmarked.join("keep.txt"), b"keep").unwrap();
        drop(first);
        for _ in 0..MAX_DIRECTORIES + 1 {
            drop(Inbox::create(root.path()).unwrap());
        }
        let owned = std::fs::read_dir(&inbox)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join(MARKER).is_file())
            .count();
        assert_eq!(owned, MAX_DIRECTORIES);
        assert_eq!(std::fs::read(foreign.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(std::fs::read(unmarked.join("keep.txt")).unwrap(), b"keep");
    }

    #[test]
    fn ttl_boundary_and_modified_or_foreign_content_are_preserved() {
        let root = tempfile::tempdir().unwrap();
        let managed = Inbox::create(root.path()).unwrap();
        let path = managed.path().to_path_buf();
        let file = path.join("attachment.bin");
        std::fs::write(&file, b"owned").unwrap();
        managed
            .record(&[LocalAttachment {
                path: file.clone(),
                kind: AttachmentKind::Image,
            }])
            .unwrap();
        let inbox = managed._parents.last().unwrap();
        let modified = std::fs::metadata(path.join(MARKER))
            .unwrap()
            .modified()
            .unwrap();
        cleanup(inbox, modified + RETENTION, MAX_DIRECTORIES).unwrap();
        assert!(file.exists());
        std::fs::write(path.join("keep.txt"), b"foreign").unwrap();
        cleanup(
            inbox,
            modified + RETENTION + Duration::from_secs(1),
            MAX_DIRECTORIES,
        )
        .unwrap();
        assert!(file.exists() && path.join("keep.txt").exists());
        // 未登记文件移出后，已修改附件仍不能被删除。
        std::fs::rename(path.join("keep.txt"), root.path().join("keep.txt")).unwrap();
        let original_time = std::fs::metadata(&file).unwrap().modified().unwrap();
        std::fs::write(&file, b"other").unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(original_time + Duration::from_secs(1)),
            )
            .unwrap();
        cleanup(
            inbox,
            modified + RETENTION + Duration::from_secs(1),
            MAX_DIRECTORIES,
        )
        .unwrap();
        assert!(file.exists());
        std::fs::write(&file, b"owned").unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_time))
            .unwrap();
        // Windows 删除目录前须释放本次持有的目录句柄。
        let inbox_path = inbox.path.clone();
        drop(managed);
        cleanup(
            &Directory::open(&inbox_path).unwrap(),
            modified + RETENTION + Duration::from_secs(1),
            MAX_DIRECTORIES,
        )
        .unwrap();
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(root.path().join("keep.txt")).unwrap(),
            b"foreign"
        );
    }

    #[test]
    fn malformed_marker_and_nested_content_are_never_deleted() {
        let root = tempfile::tempdir().unwrap();
        let managed = Inbox::create(root.path()).unwrap();
        std::fs::create_dir(managed.path().join("user-subdir")).unwrap();
        std::fs::write(managed.path().join("user-subdir/keep"), b"keep").unwrap();
        cleanup(
            managed._parents.last().unwrap(),
            SystemTime::now() + RETENTION * 2,
            0,
        )
        .unwrap();
        assert!(managed.path().join("user-subdir/keep").exists());
        std::fs::write(managed.path().join(MARKER), b"invalid").unwrap();
        cleanup(
            managed._parents.last().unwrap(),
            SystemTime::now() + RETENTION * 2,
            0,
        )
        .unwrap();
        assert!(managed.path().join("user-subdir/keep").exists());
    }

    #[test]
    fn inbox_rejects_linked_ancestors_before_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), b"keep").unwrap();
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(root.path().join(".codex-plus"))
                .arg(outside.path())
                .output()
                .unwrap();
            assert!(output.status.success());
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), root.path().join(".codex-plus")).unwrap();
        assert!(Inbox::create(root.path()).is_err());
        assert_eq!(std::fs::read(outside.path().join("keep")).unwrap(), b"keep");
        assert!(!outside.path().join("weixin").exists());
    }
}
