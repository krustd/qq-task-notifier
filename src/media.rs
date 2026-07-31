use std::{
    env,
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use md5::Md5;
use sha1::Sha1;
use sha2::Digest;
use tokio::io::AsyncReadExt;
use tracing::warn;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
#[derive(Clone, Copy)]
pub(crate) enum MediaType {
    Image,
    File,
}

impl MediaType {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Image => 1,
            Self::File => 4,
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::File => "file",
        }
    }
}

pub(crate) struct UploadedMedia {
    pub(crate) temporary_file: TemporaryFile,
    pub(crate) file_name: String,
    pub(crate) content_type: Option<String>,
    pub(crate) size: u64,
}

pub(crate) struct TemporaryFile {
    pub(crate) path: PathBuf,
}

impl TemporaryFile {
    pub(crate) async fn create() -> Result<(Self, tokio::fs::File)> {
        for _ in 0..16 {
            let sequence = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("qqbot-upload-{}-{sequence}", std::process::id()));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => return Ok((Self { path }, tokio::fs::File::from_std(file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("无法创建临时上传文件 {}", path.display()));
                }
            }
        }
        bail!("无法创建唯一的临时上传文件")
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!("无法删除临时上传文件 {}: {error}", self.path.display());
        }
    }
}
pub(crate) async fn file_digests(path: &Path) -> Result<(String, String, String)> {
    const FIRST_10_MB: usize = 10_002_432;

    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("无法读取临时上传文件 {}", path.display()))?;
    let mut full_md5 = Md5::new();
    let mut full_sha1 = Sha1::new();
    let mut first_10_mb_md5 = Md5::new();
    let mut first_10_mb_size = 0_usize;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .await
            .context("读取上传文件校验数据失败")?;
        if count == 0 {
            break;
        }
        let bytes = &buffer[..count];
        full_md5.update(bytes);
        full_sha1.update(bytes);
        if first_10_mb_size < FIRST_10_MB {
            let length = (FIRST_10_MB - first_10_mb_size).min(bytes.len());
            first_10_mb_md5.update(&bytes[..length]);
            first_10_mb_size += length;
        }
    }
    Ok((
        format!("{:x}", full_md5.finalize()),
        format!("{:x}", full_sha1.finalize()),
        format!("{:x}", first_10_mb_md5.finalize()),
    ))
}
