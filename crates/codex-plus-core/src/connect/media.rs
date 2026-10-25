use std::path::{Path, PathBuf};
use std::time::Duration;

use aes_gcm::aead::{OsRng, rand_core::RngCore};
use aes_gcm::aes::{
    Aes128,
    cipher::{BlockDecryptMut, BlockEncryptMut, KeyInit, block_padding::Pkcs7},
};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use md5::{Digest, Md5};
use reqwest::{Url, redirect::Policy};
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use super::weixin::{WeixinMessage, WeixinMessageItem};

pub const DEFAULT_CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
pub const MAX_ATTACHMENT_BYTES: usize = 100 * 1024 * 1024;
pub(super) const MAX_REFERENCE_DEPTH: usize = 4;
const MAX_ATTACHMENTS: usize = 10;
const MEDIA_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Image,
    File,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAttachment {
    pub path: PathBuf,
    pub kind: AttachmentKind,
}

#[derive(Debug, Default)]
pub struct DownloadedAttachments {
    pub attachments: Vec<LocalAttachment>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct WeixinCdnMedia {
    #[serde(default)]
    pub encrypt_query_param: String,
    #[serde(default)]
    pub aes_key: String,
    #[serde(default)]
    pub full_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeixinImageItem {
    #[serde(default)]
    pub media: Option<WeixinCdnMedia>,
    #[serde(default)]
    pub aeskey: String,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeixinFileItem {
    #[serde(default)]
    pub media: Option<WeixinCdnMedia>,
    #[serde(default)]
    pub file_name: String,
    #[serde(default)]
    pub len: String,
    #[serde(default)]
    pub md5: String,
}

#[derive(Default, Deserialize)]
pub(super) struct MediaApiResponse {
    #[serde(default)]
    pub ret: i64,
    #[serde(default)]
    pub errcode: i64,
    pub upload_param: Option<String>,
    pub upload_full_url: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct MediaClient {
    client: reqwest::Client,
    base_url: Url,
    local_origin: Option<Url>,
}

impl MediaClient {
    pub(super) fn new(base_url: &str) -> anyhow::Result<Self> {
        let base_url = parse_url(base_url.trim())?;
        let local_origin = is_loopback(&base_url).then(|| base_url.clone());
        validate_url(&base_url, local_origin.as_ref())?;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(MEDIA_TIMEOUT)
            .redirect(Policy::none());
        if local_origin.is_some() {
            builder = builder.no_proxy();
        }
        // A separate client ensures bot headers/cookies never reach media endpoints.
        let client = builder.build().context("无法创建微信媒体客户端")?;
        Ok(Self {
            client,
            base_url,
            local_origin,
        })
    }

    pub(super) async fn download_attachments(
        &self,
        message: &WeixinMessage,
        directory: &Path,
    ) -> DownloadedAttachments {
        let mut result = DownloadedAttachments::default();
        let (items, truncated) = attachment_items(message);
        if truncated {
            result
                .warnings
                .push("微信附件数量或引用层数超过限制".into());
        }
        let mut remaining = MAX_ATTACHMENT_BYTES;
        for (index, item) in items.into_iter().enumerate() {
            match self.download_item(item, directory, remaining).await {
                Ok((attachment, size)) => {
                    remaining -= size;
                    result.attachments.push(attachment);
                }
                Err(error) => {
                    result
                        .warnings
                        .push(format!("附件 {}：{error:#}", index + 1));
                }
            }
        }
        result
    }

    async fn download_item(
        &self,
        item: &WeixinMessageItem,
        directory: &Path,
        max_bytes: usize,
    ) -> anyhow::Result<(LocalAttachment, usize)> {
        let (kind, media, hex_key, filename, expected_size, expected_md5, image_url) =
            match item.item_type {
                2 => {
                    let image = item.image_item.as_ref().context("缺少图片信息")?;
                    (
                        AttachmentKind::Image,
                        image.media.as_ref(),
                        image.aeskey.as_str(),
                        "",
                        None,
                        "",
                        image.url.as_str(),
                    )
                }
                4 => {
                    let file = item.file_item.as_ref().context("缺少文件信息")?;
                    let size = if file.len.is_empty() {
                        None
                    } else {
                        Some(file.len.parse::<u64>().context("附件大小格式无效")?)
                    };
                    (
                        AttachmentKind::File,
                        file.media.as_ref(),
                        "",
                        file.file_name.as_str(),
                        size,
                        file.md5.as_str(),
                        "",
                    )
                }
                _ => bail!("不支持此附件类型"),
            };
        if expected_size.is_some_and(|size| size > max_bytes as u64) || max_bytes == 0 {
            bail!("本条消息的附件总大小超过 100 MiB 限制");
        }
        let key = if !hex_key.trim().is_empty() {
            Some(parse_hex_key(hex_key)?)
        } else if let Some(media) = media.filter(|media| !media.aes_key.trim().is_empty()) {
            Some(parse_base64_key(&media.aes_key)?)
        } else {
            None
        };
        if kind == AttachmentKind::File && key.is_none() {
            bail!("文件附件缺少加密密钥");
        }
        let url = self.download_url(media, image_url)?;
        let response = self.client.get(url).send().await.map_err(network_error)?;
        let status = response.status();
        if !status.is_success() {
            bail!("微信 CDN 下载失败：HTTP {status}");
        }
        let wire_limit = max_bytes + if key.is_some() { 16 } else { 0 };
        let bytes = read_bytes_limited(response, wire_limit).await?;
        let bytes = match key {
            Some(key) => decrypt(&bytes, &key)?,
            None => bytes,
        };
        if bytes.len() > max_bytes {
            bail!("本条消息的附件总大小超过 100 MiB 限制");
        }
        if expected_size.is_some_and(|size| size != bytes.len() as u64) {
            bail!("附件实际大小与文件信息不符");
        }
        if !expected_md5.is_empty()
            && !format!("{:x}", Md5::digest(&bytes)).eq_ignore_ascii_case(expected_md5)
        {
            bail!("附件 MD5 校验失败");
        }
        let filename = if kind == AttachmentKind::Image {
            format!("image.{}", image_extension(&bytes))
        } else {
            safe_filename(filename)
        };
        let path = save_new(directory, &filename, &bytes).await?;
        Ok((LocalAttachment { path, kind }, bytes.len()))
    }

    fn download_url(&self, media: Option<&WeixinCdnMedia>, image_url: &str) -> anyhow::Result<Url> {
        let url = if let Some(media) = media.filter(|media| !media.full_url.trim().is_empty()) {
            parse_url(media.full_url.trim())?
        } else if let Some(media) =
            media.filter(|media| !media.encrypt_query_param.trim().is_empty())
        {
            let mut url = self.endpoint("download")?;
            url.query_pairs_mut()
                .append_pair("encrypted_query_param", &media.encrypt_query_param);
            url
        } else if !image_url.trim().is_empty() {
            parse_url(image_url.trim())?
        } else {
            bail!("附件缺少 CDN 下载信息");
        };
        validate_url(&url, self.local_origin.as_ref())?;
        Ok(url)
    }

    pub(super) async fn upload(
        &self,
        upload: &PreparedUpload,
        response: &MediaApiResponse,
    ) -> anyhow::Result<String> {
        let url = if let Some(full_url) = response
            .upload_full_url
            .as_deref()
            .filter(|url| !url.trim().is_empty())
        {
            parse_url(full_url.trim())?
        } else if let Some(param) = response
            .upload_param
            .as_deref()
            .filter(|param| !param.trim().is_empty())
        {
            let mut url = self.endpoint("upload")?;
            url.query_pairs_mut()
                .append_pair("encrypted_query_param", param)
                .append_pair("filekey", &upload.filekey);
            url
        } else {
            bail!("微信响应缺少 CDN 上传信息");
        };
        validate_url(&url, self.local_origin.as_ref())?;
        let response = self
            .client
            .post(url)
            .header("content-type", "application/octet-stream")
            .body(upload.ciphertext.clone())
            .send()
            .await
            .map_err(network_error)?;
        let status = response.status();
        if status != reqwest::StatusCode::OK {
            bail!("微信 CDN 上传失败：HTTP {status}");
        }
        let param = response
            .headers()
            .get("x-encrypted-param")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .context("微信 CDN 上传响应缺少下载参数")?;
        Ok(param.to_owned())
    }

    fn endpoint(&self, path: &str) -> anyhow::Result<Url> {
        let mut url = self.base_url.clone();
        url.set_path(&format!(
            "{}/{}",
            self.base_url.path().trim_end_matches('/'),
            path
        ));
        url.set_query(None);
        Ok(url)
    }
}

pub(super) struct PreparedUpload {
    pub ciphertext: Vec<u8>,
    pub plaintext_size: usize,
    pub md5: String,
    pub key_hex: String,
    pub filekey: String,
    pub filename: String,
}

impl PreparedUpload {
    pub(super) async fn read(path: &Path) -> anyhow::Result<Self> {
        Self::read_with_workspace(path, None).await
    }

    pub(super) async fn read_workspace(path: &Path, workspace: &Path) -> anyhow::Result<Self> {
        Self::read_with_workspace(path, Some(workspace)).await
    }

    async fn read_with_workspace(path: &Path, workspace: Option<&Path>) -> anyhow::Result<Self> {
        let path = path.to_owned();
        let workspace = workspace.map(Path::to_owned);
        tokio::task::spawn_blocking(move || {
            let workspace = workspace
                .map(|root| {
                    let root = std::fs::canonicalize(root).context("无法解析附件工作区")?;
                    if !root.is_dir() {
                        bail!("附件工作区必须是目录");
                    }
                    Ok::<_, anyhow::Error>(root)
                })
                .transpose()?;
            let path = if let Some(root) = workspace.as_deref() {
                let candidate = if path.is_absolute() {
                    path
                } else {
                    root.join(path)
                };
                let candidate = std::fs::canonicalize(candidate).context("无法解析附件路径")?;
                require_workspace_path(&candidate, root)?;
                candidate
            } else {
                path
            };
            let mut options = std::fs::OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                // 普通文件不受影响，管道等特殊文件不能在类型核验前阻塞。
                options.custom_flags(libc::O_NONBLOCK);
            }
            let file = options
                .open(&path)
                .map_err(|_| anyhow::anyhow!("无法打开附件文件"))?;
            Self::from_opened(file, &path, workspace.as_deref())
        })
        .await
        .context("读取附件任务失败")?
    }

    fn from_opened(
        mut file: std::fs::File,
        path: &Path,
        workspace: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let metadata = file.metadata().context("无法检查附件文件")?;
        if !metadata.is_file() || metadata.len() > MAX_ATTACHMENT_BYTES as u64 {
            bail!("附件必须是不超过 100 MiB 的普通文件");
        }
        if let Some(root) = workspace {
            verify_opened_workspace_file(&file, path, root)?;
        }
        let mut plaintext = Vec::new();
        // 边界核验、大小限制和实际读取绑定同一文件，之后不按路径重开。
        std::io::Read::read_to_end(
            &mut std::io::Read::take(&mut file, MAX_ATTACHMENT_BYTES as u64 + 1),
            &mut plaintext,
        )
        .context("无法读取附件文件")?;
        if plaintext.len() > MAX_ATTACHMENT_BYTES {
            bail!("附件超过 100 MiB 大小限制");
        }
        if let Some(root) = workspace {
            verify_opened_workspace_file(&file, path, root)?;
        }
        let mut key = [0u8; 16];
        OsRng
            .try_fill_bytes(&mut key)
            .map_err(|_| anyhow::anyhow!("无法生成微信媒体加密密钥"))?;
        Ok(Self {
            ciphertext: encrypt(&plaintext, &key),
            plaintext_size: plaintext.len(),
            md5: format!("{:x}", Md5::digest(&plaintext)),
            key_hex: hex(&key),
            filekey: Uuid::new_v4().simple().to_string(),
            filename: safe_filename(
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("file.bin"),
            ),
        })
    }
}

fn require_workspace_path(path: &Path, workspace: &Path) -> anyhow::Result<()> {
    if path == workspace || !path.starts_with(workspace) {
        bail!("附件实际文件必须位于当前工作区内");
    }
    Ok(())
}

#[cfg(windows)]
pub(super) fn verify_opened_workspace_file(
    file: &std::fs::File,
    _path: &Path,
    workspace: &Path,
) -> anyhow::Result<()> {
    use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{
            FILE_NAME_NORMALIZED, GETFINALPATHNAMEBYHANDLE_FLAGS, GetFinalPathNameByHandleW,
            VOLUME_NAME_DOS,
        },
    };
    let mut buffer = vec![0u16; 512];
    loop {
        // file 在调用和读取完成前始终持有句柄，借用 HANDLE 不转移所有权。
        let length = unsafe {
            GetFinalPathNameByHandleW(
                HANDLE(file.as_raw_handle()),
                &mut buffer,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
            )
        } as usize;
        if length == 0 || length > 32_768 {
            bail!("无法核验附件文件句柄的实际路径");
        }
        if length >= buffer.len() {
            buffer.resize(length + 1, 0);
            continue;
        }
        let actual = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
        return require_workspace_path(&actual, workspace);
    }
}

#[cfg(unix)]
pub(super) fn verify_opened_workspace_file(
    file: &std::fs::File,
    path: &Path,
    workspace: &Path,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let canonical = std::fs::canonicalize(path).context("无法复核附件路径")?;
    require_workspace_path(&canonical, workspace)?;
    let opened = file.metadata().context("无法核验附件文件身份")?;
    let target = open_unix_canonical_target(&canonical)?
        .metadata()
        .context("无法核验附件目标身份")?;
    if !target.is_file() || opened.dev() != target.dev() || opened.ino() != target.ino() {
        bail!("附件文件身份已改变，请重新发送");
    }
    Ok(())
}

#[cfg(unix)]
fn open_unix_canonical_target(path: &Path) -> anyhow::Result<std::fs::File> {
    use std::os::{
        fd::{AsRawFd, FromRawFd},
        unix::ffi::OsStrExt,
    };
    let mut directory = std::fs::File::open("/").context("无法核验附件根目录")?;
    let mut components = path
        .components()
        .filter(|part| !matches!(part, std::path::Component::RootDir))
        .peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            bail!("附件规范路径无效");
        };
        let name = std::ffi::CString::new(name.as_bytes()).context("附件路径包含无效字符")?;
        let flags = libc::O_RDONLY
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if components.peek().is_some() {
                libc::O_DIRECTORY
            } else {
                0
            };
        // 从已打开目录逐层核验，拒绝核验期间替换的文件和目录链接。
        let descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if descriptor < 0 {
            bail!("附件路径在核验期间发生变化或无法访问");
        }
        directory = unsafe { std::fs::File::from_raw_fd(descriptor) };
    }
    Ok(directory)
}

#[cfg(not(any(windows, unix)))]
pub(super) fn verify_opened_workspace_file(
    _file: &std::fs::File,
    _path: &Path,
    _workspace: &Path,
) -> anyhow::Result<()> {
    bail!("当前平台不支持严格核验附件文件身份");
}

fn encrypt(plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    ecb::Encryptor::<Aes128>::new(key.into()).encrypt_padded_vec_mut::<Pkcs7>(plaintext)
}

fn decrypt(ciphertext: &[u8], key: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    ecb::Decryptor::<Aes128>::new(key.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .map_err(|_| anyhow::anyhow!("微信媒体密文或 PKCS7 填充无效"))
}

fn parse_hex_key(encoded: &str) -> anyhow::Result<[u8; 16]> {
    let encoded = encoded.trim();
    if encoded.len() != 32 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("微信媒体十六进制密钥无效");
    }
    let mut key = [0u8; 16];
    for (index, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&encoded[index * 2..index * 2 + 2], 16)
            .map_err(|_| anyhow::anyhow!("微信媒体十六进制密钥无效"))?;
    }
    Ok(key)
}

fn parse_base64_key(encoded: &str) -> anyhow::Result<[u8; 16]> {
    if encoded.len() > 128 {
        bail!("微信媒体 base64 密钥无效");
    }
    let encoded: String = encoded
        .chars()
        .filter(|ch| !ch.is_ascii_whitespace())
        .collect();
    let decoded = STANDARD
        .decode(&encoded)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&encoded))
        .map_err(|_| anyhow::anyhow!("微信媒体 base64 密钥无效"))?;
    match decoded.len() {
        16 => decoded
            .try_into()
            .map_err(|_| anyhow::anyhow!("微信媒体密钥长度无效")),
        32 => parse_hex_key(
            std::str::from_utf8(&decoded).map_err(|_| anyhow::anyhow!("微信媒体密钥编码无效"))?,
        ),
        _ => bail!("微信媒体密钥长度无效"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_url(value: &str) -> anyhow::Result<Url> {
    Url::parse(value).map_err(|_| anyhow::anyhow!("微信 CDN 地址无效"))
}

fn is_loopback(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        host == "localhost"
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    })
}

fn validate_url(url: &Url, local_origin: Option<&Url>) -> anyhow::Result<()> {
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("微信 CDN 地址不能包含登录凭据或片段标识");
    }
    let official = url.scheme() == "https"
        && url.host_str() == Some("novac2c.cdn.weixin.qq.com")
        && url.port_or_known_default() == Some(443);
    let local = local_origin.is_some_and(|base| {
        is_loopback(url)
            && matches!(url.scheme(), "http" | "https")
            && url.origin() == base.origin()
    });
    if !official && !local {
        bail!("微信媒体地址必须使用官方 CDN 或显式配置的本地回环端点");
    }
    Ok(())
}

fn network_error(error: reqwest::Error) -> anyhow::Error {
    // Do not retain reqwest's URL or server text: both can contain media keys.
    if error.is_timeout() {
        anyhow::anyhow!("微信 CDN 请求超时")
    } else {
        anyhow::anyhow!("微信 CDN 网络请求失败")
    }
}

async fn read_bytes_limited(
    response: reqwest::Response,
    max_bytes: usize,
) -> anyhow::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|size| size > max_bytes as u64)
    {
        bail!("微信 CDN 响应超过附件大小限制");
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(network_error)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            bail!("微信 CDN 响应超过附件大小限制");
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn attachment_items(message: &WeixinMessage) -> (Vec<&WeixinMessageItem>, bool) {
    let mut items = Vec::new();
    let mut truncated = false;
    for item in &message.item_list {
        let mut current = Some(item);
        let mut depth = 0;
        while let Some(item) = current {
            if matches!(item.item_type, 2 | 4) {
                if items.len() == MAX_ATTACHMENTS {
                    return (items, true);
                }
                items.push(item);
            }
            current = item
                .ref_msg
                .as_ref()
                .and_then(|reference| reference.message_item.as_deref());
            if depth == MAX_REFERENCE_DEPTH {
                truncated |= current.is_some();
                break;
            }
            depth += 1;
        }
    }
    (items, truncated)
}

pub(super) fn has_attachments(message: &WeixinMessage) -> bool {
    !attachment_items(message).0.is_empty()
}

fn safe_filename(filename: &str) -> String {
    let basename = filename.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut result = String::new();
    for ch in basename.chars() {
        if result.len() + ch.len_utf8() > 160 {
            break;
        }
        result.push(if ch.is_control() || "<>:\"|?*".contains(ch) {
            '_'
        } else {
            ch
        });
    }
    let result = result.trim_matches([' ', '.']);
    if result.is_empty() {
        "file.bin".into()
    } else {
        result.into()
    }
}

async fn save_new(directory: &Path, filename: &str, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(directory)
        .await
        .context("无法创建附件目录")?;
    let directory = tokio::fs::canonicalize(directory)
        .await
        .context("无法解析附件目录")?;
    // Unique create_new prevents overwriting files and following existing file symlinks.
    let path = directory.join(format!(
        "{}-{}",
        Uuid::new_v4().simple(),
        safe_filename(filename)
    ));
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|_| anyhow::anyhow!("无法创建附件文件"))?;
    let saved = async {
        file.write_all(bytes).await?;
        // 等待 Tokio 后台写入完成，返回后同步读取也必须看到完整附件。
        file.flush().await
    }
    .await;
    if let Err(error) = saved {
        drop(file);
        let _ = tokio::fs::remove_file(&path).await;
        return Err(error).context("无法保存附件文件");
    }
    Ok(path)
}

fn image_extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "webp"
    } else if bytes.starts_with(b"BM") {
        "bmp"
    } else if bytes.starts_with(b"II\x2a\0") || bytes.starts_with(b"MM\0\x2a") {
        "tiff"
    } else {
        "img"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::weixin::WeixinClient;
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, header, method, path, query_param},
    };

    const KEY: &str = "000102030405060708090a0b0c0d0e0f";
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nweixin-test-image";

    #[tokio::test]
    async fn saved_attachment_is_immediately_readable_by_std_fs() {
        let dir = tempfile::tempdir().unwrap();
        for bytes in [b"report data".as_slice(), PNG, &[42u8; 128 * 1024]] {
            for _ in 0..8 {
                let path = save_new(dir.path(), "attachment.bin", bytes).await.unwrap();
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
        }
    }

    fn client(server: &MockServer) -> WeixinClient {
        WeixinClient::new(&server.uri(), "bot-secret", "route-secret")
            .unwrap()
            .with_media_base_url(&format!("{}/cdn", server.uri()))
            .unwrap()
    }

    fn message(items: serde_json::Value) -> WeixinMessage {
        serde_json::from_value(json!({
            "from_user_id": "peer",
            "message_type": 1,
            "message_state": 2,
            "item_list": items,
            "context_token": "context-secret"
        }))
        .unwrap()
    }

    #[test]
    fn aes_ecb_pkcs7_matches_official_node_crypto_for_block_boundaries() {
        // Generated with the official aes-ecb.ts createCipheriv("aes-128-ecb") API.
        let key = parse_hex_key(KEY).unwrap();
        for (plain, expected) in [
            ("", "954f64f2e4e86e9eee82d20216684899"),
            ("hello, weixin", "be8ead8f5e466a5bffe99f774a2d7290"),
            (
                "0123456789abcdef",
                "281567ab2f4cf0d73d3198225b8b8393954f64f2e4e86e9eee82d20216684899",
            ),
            (
                "0123456789abcdefg",
                "281567ab2f4cf0d73d3198225b8b839359d696e1e6c1040eaf9b614dfc82cf8c",
            ),
        ] {
            let ciphertext = encrypt(plain.as_bytes(), &key);
            assert_eq!(hex(&ciphertext), expected);
            assert_eq!(decrypt(&ciphertext, &key).unwrap(), plain.as_bytes());
            assert_eq!(ciphertext.len(), (plain.len() / 16 + 1) * 16);
        }
        assert!(decrypt(&[0; 15], &key).is_err());
        assert!(decrypt(&[], &key).is_err());
    }

    #[test]
    fn accepts_both_official_base64_key_formats_without_echoing_bad_keys() {
        let key = parse_hex_key(KEY).unwrap();
        assert_eq!(parse_base64_key(&STANDARD.encode(key)).unwrap(), key);
        assert_eq!(parse_base64_key(&STANDARD.encode(KEY)).unwrap(), key);
        assert_eq!(parse_hex_key(&KEY.to_uppercase()).unwrap(), key);
        for encoded in ["secret-key-invalid", "token=secret", ""] {
            let error = format!("{:#}", parse_base64_key(encoded).unwrap_err());
            if !encoded.is_empty() {
                assert!(!error.contains(encoded));
            }
        }
    }

    #[test]
    fn rejects_untrusted_urls_and_keeps_remote_names_in_one_path_component() {
        let official = MediaClient::new(DEFAULT_CDN_BASE_URL).unwrap();
        for value in [
            "https://example.com/file?token=secret",
            "https://novac2c.cdn.weixin.qq.com.evil.test/file",
            "http://novac2c.cdn.weixin.qq.com/file",
            "https://novac2c.cdn.weixin.qq.com:444/file",
            "https://user:secret@novac2c.cdn.weixin.qq.com/file",
            "http://127.0.0.1:1234/file",
            "file:///etc/passwd",
        ] {
            let media = WeixinCdnMedia {
                full_url: value.into(),
                ..Default::default()
            };
            let error = official
                .download_url(Some(&media), "")
                .unwrap_err()
                .to_string();
            assert!(!error.contains("secret"));
        }
        assert!(MediaClient::new("https://example.com").is_err());
        let local = MediaClient::new("http://127.0.0.1:1234/cdn").unwrap();
        assert!(
            local
                .download_url(None, "http://127.0.0.1:1234/plain")
                .is_ok()
        );
        assert!(
            local
                .download_url(None, "http://127.0.0.1:1235/plain")
                .is_err()
        );
        assert_eq!(safe_filename("../../report.txt"), "report.txt");
        assert_eq!(
            safe_filename("C:\\Windows\\..\\report:ads.txt"),
            "report_ads.txt"
        );
        assert_eq!(safe_filename(".."), "file.bin");
        assert!(!safe_filename("nul\0.txt").contains('\0'));
        assert!(safe_filename(&"a".repeat(500)).len() <= 160);
    }

    #[tokio::test]
    async fn downloads_encrypted_image_and_quoted_file_without_bot_headers() {
        let server = MockServer::start().await;
        let key = parse_hex_key(KEY).unwrap();
        Mock::given(method("GET"))
            .and(path("/cdn/download"))
            .and(query_param("encrypted_query_param", "image+/=&"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(encrypt(PNG, &key)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/file"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(encrypt(b"report data", &key)))
            .expect(1)
            .mount(&server)
            .await;
        let message = message(json!([
            {
                "type": 2,
                "image_item": {
                    "aeskey": KEY,
                    "media": {"encrypt_query_param": "image+/=&", "aes_key": "bad-key"}
                }
            },
            {
                "type": 1,
                "text_item": {"text": "Please process these"},
                "ref_msg": {"message_item": {
                    "type": 4,
                    "file_item": {
                        "file_name": "../../report.txt",
                        "len": "11",
                        "md5": format!("{:x}", Md5::digest(b"report data")),
                        "media": {"full_url": format!("{}/file", server.uri()), "aes_key": STANDARD.encode(KEY)}
                    }
                }}
            }
        ]));
        assert!(message.has_attachments());
        let dir = tempfile::tempdir().unwrap();
        let result = client(&server)
            .download_attachments(&message, dir.path())
            .await;
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.attachments.len(), 2);
        let image = &result.attachments[0];
        assert_eq!(image.kind, AttachmentKind::Image);
        assert_eq!(image.path.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&image.path).unwrap(), PNG);
        let file = &result.attachments[1];
        assert_eq!(file.kind, AttachmentKind::File);
        assert!(
            file.path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-report.txt")
        );
        assert_eq!(
            file.path.parent().unwrap(),
            std::fs::canonicalize(dir.path()).unwrap()
        );
        assert_eq!(std::fs::read(&file.path).unwrap(), b"report data");
        for request in server.received_requests().await.unwrap() {
            for secret_header in ["authorization", "authorizationtype", "skroutetag", "cookie"] {
                assert!(!request.headers.contains_key(secret_header));
            }
        }
    }

    #[tokio::test]
    async fn accepts_raw_key_and_plain_image_full_urls_without_following_redirects() {
        let server = MockServer::start().await;
        let key = parse_hex_key(KEY).unwrap();
        Mock::given(path("/raw"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(encrypt(PNG, &key)))
            .mount(&server)
            .await;
        Mock::given(path("/plain"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG))
            .mount(&server)
            .await;
        Mock::given(path("/redirect"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/never?key=secret", server.uri())),
            )
            .mount(&server)
            .await;
        Mock::given(path("/never"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let message = message(json!([
            {"type": 2, "image_item": {"media": {
                "full_url": format!("{}/raw", server.uri()), "aes_key": STANDARD.encode(key)
            }}},
            {"type": 2, "image_item": {"url": format!("{}/plain", server.uri())}},
            {"type": 2, "image_item": {"media": {"full_url": format!("{}/redirect?key=secret", server.uri())}}}
        ]));
        let dir = tempfile::tempdir().unwrap();
        let result = client(&server)
            .download_attachments(&message, dir.path())
            .await;
        assert_eq!(result.attachments.len(), 2);
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("302"));
        assert!(!result.warnings[0].contains("secret"));
        assert_ne!(result.attachments[0].path, result.attachments[1].path);
    }

    #[tokio::test]
    async fn attachment_failures_leave_caption_and_other_attachments_available() {
        let server = MockServer::start().await;
        Mock::given(path("/denied"))
            .respond_with(ResponseTemplate::new(403).set_body_string("token=secret aes_key=secret"))
            .mount(&server)
            .await;
        let message = message(json!([
            {"type": 1, "text_item": {"text": "Keep this text"}},
            {"type": 2, "image_item": {"media": {"full_url": format!("{}/denied?key=secret", server.uri())}}},
            {"type": 4, "file_item": {"media": {"aes_key": "secret-invalid-key"}}},
            {"type": 2, "image_item": {"url": "https://evil.test/file?token=secret"}}
        ]));
        let dir = tempfile::tempdir().unwrap();
        let result = client(&server)
            .download_attachments(&message, dir.path())
            .await;
        assert_eq!(message.text().as_deref(), Some("Keep this text"));
        assert!(result.attachments.is_empty());
        assert_eq!(result.warnings.len(), 3);
        assert!(
            result
                .warnings
                .iter()
                .all(|warning| !warning.contains("secret"))
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rejects_truncated_media_metadata_mismatches_and_oversized_bodies() {
        let server = MockServer::start().await;
        Mock::given(path("/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 15]))
            .mount(&server)
            .await;
        Mock::given(path("/wrong-size"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(encrypt(b"abc", &parse_hex_key(KEY).unwrap())),
            )
            .mount(&server)
            .await;
        Mock::given(path("/large"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 65]))
            .mount(&server)
            .await;
        let message = message(json!([
            {"type": 2, "image_item": {"media": {
                "full_url": format!("{}/bad", server.uri()), "aes_key": STANDARD.encode(KEY)
            }}},
            {"type": 4, "file_item": {"len": "100", "media": {
                "full_url": format!("{}/wrong-size", server.uri()), "aes_key": STANDARD.encode(KEY)
            }}}
        ]));
        let dir = tempfile::tempdir().unwrap();
        let result = client(&server)
            .download_attachments(&message, dir.path())
            .await;
        assert!(result.attachments.is_empty());
        assert_eq!(result.warnings.len(), 2);
        let response = reqwest::get(format!("{}/large", server.uri()))
            .await
            .unwrap();
        assert!(read_bytes_limited(response, 64).await.is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn quote_depth_and_attachment_count_are_bounded() {
        let mut nested = json!({"type": 2, "image_item": {}});
        for _ in 0..8 {
            nested = json!({"type": 1, "text_item": {"text": "quote"}, "ref_msg": {"message_item": nested}});
        }
        let deeply_quoted = message(json!([nested]));
        assert!(!deeply_quoted.has_attachments());
        assert!(deeply_quoted.text().unwrap().len() < 200);
        let result = MediaClient::new(DEFAULT_CDN_BASE_URL)
            .unwrap()
            .download_attachments(&deeply_quoted, Path::new("unused"))
            .await;
        assert_eq!(result.warnings.len(), 1);
        let many = message(serde_json::Value::Array(
            (0..20).map(|_| json!({"type": 2})).collect(),
        ));
        let (items, truncated) = attachment_items(&many);
        assert_eq!(items.len(), MAX_ATTACHMENTS);
        assert!(truncated);
    }

    #[tokio::test]
    async fn downloads_encrypted_and_quoted_images_without_bot_headers() {
        let server = MockServer::start().await;
        let key = parse_hex_key(KEY).unwrap();
        Mock::given(method("GET"))
            .and(path("/cdn/download"))
            .and(query_param("encrypted_query_param", "image+/=&"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(encrypt(PNG, &key)))
            .expect(2)
            .mount(&server)
            .await;
        let image = json!({"type":2,"image_item":{"aeskey":KEY,
            "media":{"encrypt_query_param":"image+/=&","aes_key":"ignored"}}});
        let message = message(json!([image,
            {"type":1,"text_item":{"text":"caption"},"ref_msg":{"message_item":image}}]));
        let dir = tempfile::tempdir().unwrap();
        let result = client(&server)
            .download_attachments(&message, dir.path())
            .await;
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        assert_eq!(result.attachments.len(), 2);
        assert_eq!(message.text().as_deref(), Some("caption"));
        for attachment in result.attachments {
            assert_eq!(attachment.kind, AttachmentKind::Image);
            assert_eq!(attachment.path.extension().unwrap(), "png");
            assert_eq!(std::fs::read(attachment.path).unwrap(), PNG);
        }
        for request in server.received_requests().await.unwrap() {
            for header in ["authorization", "authorizationtype", "skroutetag", "cookie"] {
                assert!(!request.headers.contains_key(header));
            }
        }
        server.verify().await;
    }

    #[tokio::test]
    async fn rejects_truncated_images_and_oversized_response_bodies() {
        let server = MockServer::start().await;
        Mock::given(path("/bad"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 15]))
            .mount(&server)
            .await;
        Mock::given(path("/large"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0; 65]))
            .mount(&server)
            .await;
        let message = message(json!([{"type":2,"image_item":{"media":{
            "full_url":format!("{}/bad",server.uri()),"aes_key":STANDARD.encode(KEY)}}}]));
        let dir = tempfile::tempdir().unwrap();
        let result = client(&server)
            .download_attachments(&message, dir.path())
            .await;
        assert!(result.attachments.is_empty());
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        let response = reqwest::get(format!("{}/large", server.uri()))
            .await
            .unwrap();
        assert!(read_bytes_limited(response, 64).await.is_err());
    }

    #[tokio::test]
    async fn rejects_file_checksum_mismatch_before_saving() {
        let server = MockServer::start().await;
        Mock::given(path("/file"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(encrypt(b"report data", &parse_hex_key(KEY).unwrap())),
            )
            .mount(&server)
            .await;
        let message = message(json!([{"type":4,"file_item":{
            "file_name":"report.txt","len":"11","md5":"00000000000000000000000000000000",
            "media":{"full_url":format!("{}/file",server.uri()),"aes_key":STANDARD.encode(KEY)}
        }}]));
        let dir = tempfile::tempdir().unwrap();
        let result = client(&server)
            .download_attachments(&message, dir.path())
            .await;
        assert!(message.has_attachments());
        assert!(result.attachments.is_empty());
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("MD5"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    async fn assert_upload_pipeline(kind: AttachmentKind, full_url: bool, workspace_bound: bool) {
        let server = MockServer::start().await;
        let upload_response = if full_url {
            json!({"upload_full_url": format!("{}/full-upload?ticket=upload-secret", server.uri())})
        } else {
            json!({"upload_param": "upload+/=&"})
        };
        Mock::given(method("POST"))
            .and(path("/ilink/bot/getuploadurl"))
            .and(header("authorization", "Bearer bot-secret"))
            .and(body_partial_json(json!({
                "media_type": if kind == AttachmentKind::Image { 1 } else { 3 },
                "rawsize": 3,
                "filesize": 16,
                "rawfilemd5": "900150983cd24fb0d6963f7d28e17f72",
                "no_need_thumb": true
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(upload_response))
            .expect(1)
            .mount(&server)
            .await;
        let upload_path = if full_url {
            "/full-upload"
        } else {
            "/cdn/upload"
        };
        Mock::given(method("POST"))
            .and(path(upload_path))
            .and(header("content-type", "application/octet-stream"))
            .respond_with(
                ResponseTemplate::new(200).insert_header("x-encrypted-param", "download-secret"),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/ilink/bot/sendmessage"))
            .and(header("authorization", "Bearer bot-secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ret": 0})))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("report.txt");
        std::fs::write(&file_path, b"abc").unwrap();
        if workspace_bound {
            client(&server)
                .send_workspace_attachment("peer", &file_path, kind, "context-secret", dir.path())
                .await
                .unwrap();
        } else {
            client(&server)
                .send_attachment("peer", &file_path, kind, "context-secret")
                .await
                .unwrap();
        }
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3);
        let info: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let key = parse_hex_key(info["aeskey"].as_str().unwrap()).unwrap();
        assert_eq!(decrypt(&requests[1].body, &key).unwrap(), b"abc");
        assert!(!requests[1].headers.contains_key("authorization"));
        assert!(!requests[1].headers.contains_key("skroutetag"));
        if !full_url {
            let params = requests[1].url.query_pairs().collect::<Vec<_>>();
            assert!(
                params
                    .iter()
                    .any(|(key, value)| key == "encrypted_query_param" && value == "upload+/=&")
            );
            assert!(
                params
                    .iter()
                    .any(|(key, value)| key == "filekey"
                        && value == info["filekey"].as_str().unwrap())
            );
        }
        let sent: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
        assert_eq!(sent["msg"]["context_token"], "context-secret");
        assert_eq!(sent["msg"]["message_state"], 2);
        let item = &sent["msg"]["item_list"][0];
        let media = if kind == AttachmentKind::Image {
            assert_eq!(item["type"], 2);
            assert_eq!(item["image_item"]["mid_size"], 16);
            &item["image_item"]["media"]
        } else {
            assert_eq!(item["type"], 4);
            assert_eq!(item["file_item"]["len"], "3");
            assert_eq!(item["file_item"]["file_name"], "report.txt");
            &item["file_item"]["media"]
        };
        assert_eq!(media["encrypt_query_param"], "download-secret");
        assert_eq!(media["encrypt_type"], 1);
        assert_eq!(
            STANDARD.decode(media["aes_key"].as_str().unwrap()).unwrap(),
            info["aeskey"].as_str().unwrap().as_bytes()
        );
    }

    #[tokio::test]
    async fn sends_image_through_getuploadurl_encrypted_cdn_and_sendmessage() {
        assert_upload_pipeline(AttachmentKind::Image, false, false).await;
    }

    #[tokio::test]
    async fn sends_file_with_full_upload_url_and_plaintext_size() {
        assert_upload_pipeline(AttachmentKind::File, true, false).await;
    }

    #[tokio::test]
    async fn sends_workspace_attachment_through_the_same_protocol_pipeline() {
        assert_upload_pipeline(AttachmentKind::File, false, true).await;
    }

    #[tokio::test]
    async fn workspace_api_rejects_outside_paths_and_directories_before_network() {
        let server = MockServer::start().await;
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let sibling = parent.path().join("workspace-other");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&sibling).unwrap();
        let outside = sibling.join("secret.txt");
        std::fs::write(&outside, b"outside secret").unwrap();
        for path in [
            outside.clone(),
            workspace.join("../workspace-other/secret.txt"),
            workspace.clone(),
        ] {
            let error = client(&server)
                .send_workspace_attachment(
                    "peer",
                    &path,
                    AttachmentKind::File,
                    "context",
                    &workspace,
                )
                .await
                .err()
                .expect("outside paths and directories must be rejected");
            assert!(error.to_string().contains("工作区") || error.to_string().contains("文件"));
            assert!(!format!("{error:#}").contains("outside secret"));
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn rejects_outside_open_handle_even_when_claimed_path_is_a_valid_workspace_file() {
        let workspace = tempfile::tempdir().unwrap();
        let allowed = workspace.path().join("allowed.txt");
        std::fs::write(&allowed, b"allowed").unwrap();
        let canonical_path = std::fs::canonicalize(&allowed).unwrap();
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), b"outside secret").unwrap();
        // 模拟路径预检后被替换：句柄已指向区外，路径现在又变回区内正常文件。
        let outside_handle = std::fs::File::open(outside.path()).unwrap();
        let error = PreparedUpload::from_opened(outside_handle, &canonical_path, Some(&root))
            .err()
            .expect("an outside handle must not pass workspace verification");
        assert!(error.to_string().contains("工作区") || error.to_string().contains("身份"));
        assert!(!format!("{error:#}").contains("outside secret"));
    }

    #[test]
    fn replacement_after_open_never_changes_the_file_whose_bytes_are_read() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("original.txt");
        std::fs::write(&path, b"original allowed data").unwrap();
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let path = std::fs::canonicalize(&path).unwrap();
        let generic_handle = std::fs::File::open(&path).unwrap();
        let strict_handle = std::fs::File::open(&path).unwrap();
        std::fs::rename(&path, workspace.path().join("moved.txt")).unwrap();
        std::fs::write(&path, b"replacement must not be read").unwrap();
        let generic = PreparedUpload::from_opened(generic_handle, &path, None).unwrap();
        assert_eq!(
            decrypt(
                &generic.ciphertext,
                &parse_hex_key(&generic.key_hex).unwrap()
            )
            .unwrap(),
            b"original allowed data"
        );
        let strict = PreparedUpload::from_opened(strict_handle, &path, Some(&root));
        #[cfg(windows)]
        {
            let strict = strict.unwrap();
            assert_eq!(
                decrypt(&strict.ciphertext, &parse_hex_key(&strict.key_hex).unwrap()).unwrap(),
                b"original allowed data"
            );
        }
        #[cfg(unix)]
        assert!(strict.err().unwrap().to_string().contains("身份"));
    }

    #[tokio::test]
    async fn workspace_read_accepts_relative_unicode_paths_and_checks_same_handle_size() {
        let workspace = tempfile::tempdir().unwrap();
        let path = workspace.path().join("附件.txt");
        std::fs::write(&path, b"allowed").unwrap();
        let upload = PreparedUpload::read_workspace(Path::new("附件.txt"), workspace.path())
            .await
            .unwrap();
        assert_eq!(
            decrypt(&upload.ciphertext, &parse_hex_key(&upload.key_hex).unwrap()).unwrap(),
            b"allowed"
        );
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 7);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_ATTACHMENT_BYTES as u64 + 1)
            .unwrap();
        let root = std::fs::canonicalize(workspace.path()).unwrap();
        let error = PreparedUpload::from_opened(file, &path, Some(&root))
            .err()
            .expect("the opened file's updated size must be checked");
        assert!(error.to_string().contains("100 MiB"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_read_rejects_file_and_directory_symlinks_to_outside() {
        use std::os::unix::fs::symlink;
        let parent = tempfile::tempdir().unwrap();
        let workspace = parent.path().join("workspace");
        let outside = parent.path().join("outside");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"secret").unwrap();
        symlink(outside.join("secret.txt"), workspace.join("file-link.txt")).unwrap();
        symlink(&outside, workspace.join("directory-link")).unwrap();
        for path in [
            workspace.join("file-link.txt"),
            workspace.join("directory-link/secret.txt"),
        ] {
            assert!(
                PreparedUpload::read_workspace(&path, &workspace)
                    .await
                    .is_err()
            );
        }
        let inside = workspace.join("inside.txt");
        std::fs::write(&inside, b"allowed").unwrap();
        symlink(&inside, workspace.join("inside-link.txt")).unwrap();
        assert!(
            PreparedUpload::read_workspace(&workspace.join("inside-link.txt"), &workspace)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn rejects_upload_errors_without_leaking_server_text_or_sending_message() {
        let server = MockServer::start().await;
        Mock::given(path("/ilink/bot/getuploadurl"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"upload_param": "secret"})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/cdn/upload"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_string("key=secret")
                    .insert_header("x-error-message", "token=secret"),
            )
            .mount(&server)
            .await;
        Mock::given(path("/ilink/bot/sendmessage"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let file = tempfile::NamedTempFile::new().unwrap();
        let error = client(&server)
            .send_attachment("peer", file.path(), AttachmentKind::File, "context-secret")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("403"));
        assert!(!format!("{error:#}").contains("secret"));
    }

    #[tokio::test]
    async fn rejects_api_business_errors_without_echoing_errmsg() {
        let server = MockServer::start().await;
        Mock::given(path("/ilink/bot/getuploadurl"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": -1, "errmsg": "secret-key and token=secret",
                "upload_full_url": "https://evil.test/?token=secret"
            })))
            .mount(&server)
            .await;
        let file = tempfile::NamedTempFile::new().unwrap();
        let error = client(&server)
            .send_attachment("peer", file.path(), AttachmentKind::Image, "context-secret")
            .await
            .unwrap_err();
        assert!(!format!("{error:#}").contains("secret"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn validates_context_and_oversized_files_before_network_requests() {
        let server = MockServer::start().await;
        let file = tempfile::NamedTempFile::new().unwrap();
        let client = client(&server);
        assert!(
            client
                .send_attachment("peer", file.path(), AttachmentKind::File, "")
                .await
                .is_err()
        );
        file.as_file()
            .set_len(MAX_ATTACHMENT_BYTES as u64 + 1)
            .unwrap();
        assert!(
            client
                .send_attachment("peer", file.path(), AttachmentKind::File, "context")
                .await
                .is_err()
        );
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn media_timeout_is_bounded_and_redacted() {
        let server = MockServer::start().await;
        Mock::given(path("/slow"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(150))
                    .set_body_bytes(PNG),
            )
            .mount(&server)
            .await;
        let mut media = MediaClient::new(&server.uri()).unwrap();
        media.client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_millis(30))
            .build()
            .unwrap();
        let message = message(json!([{
            "type": 2, "image_item": {"url": format!("{}/slow?key=secret", server.uri())}
        }]));
        let dir = tempfile::tempdir().unwrap();
        let result = media.download_attachments(&message, dir.path()).await;
        assert!(result.attachments.is_empty());
        assert_eq!(result.warnings.len(), 1);
        assert!(result.warnings[0].contains("超时"));
        assert!(!result.warnings[0].contains("secret"));
    }
}
