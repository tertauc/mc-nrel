use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::blocking::Client;
use sha1::{Digest, Sha1};

pub fn client() -> Result<Client> {
    Client::builder()
        .user_agent(format!(
            "NREL/{} (Minecraft Launcher)",
            env!("CARGO_PKG_VERSION")
        ))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .build()
        .context("无法创建 HTTP 客户端")
}

pub fn get_text(client: &Client, url: &str) -> Result<String> {
    let resp = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("请求失败: {url}"))?;
    resp.text().context("读取响应内容失败")
}

#[derive(Debug, Clone)]
pub struct DownloadTask {
    pub url: String,
    pub path: PathBuf,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

impl DownloadTask {
    pub fn new(url: impl Into<String>, path: PathBuf) -> Self {
        Self {
            url: url.into(),
            path,
            sha1: None,
            size: None,
        }
    }

    pub fn with_hash(mut self, sha1: Option<String>) -> Self {
        self.sha1 = sha1;
        self
    }

    pub fn with_size(mut self, size: Option<u64>) -> Self {
        self.size = size;
        self
    }
}

pub fn file_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} {msg}  [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, 剩余 {eta})",
    )
    .unwrap()
    .progress_chars("=>-")
}

pub fn count_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg}  [{bar:40.cyan/blue}] {pos}/{len}")
        .unwrap()
        .progress_chars("=>-")
}

fn existing_is_ok(task: &DownloadTask) -> bool {
    let Ok(meta) = fs::metadata(&task.path) else {
        return false;
    };
    if let Some(size) = task.size {
        return meta.len() == size;
    }
    // 没有大小信息时，若提供了 SHA1 则校验本地文件。
    match &task.sha1 {
        Some(expected) => file_sha1(&task.path)
            .map(|actual| actual.eq_ignore_ascii_case(expected))
            .unwrap_or(false),
        None => false,
    }
}

fn file_sha1(path: &std::path::Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buf).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn download_once(
    client: &Client,
    task: &DownloadTask,
    pb: Option<&ProgressBar>,
) -> Result<()> {
    if existing_is_ok(task) {
        return Ok(());
    }

    if let Some(parent) = task.path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建目录 {}", parent.display()))?;
    }

    let resp = client
        .get(&task.url)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("下载失败: {}", task.url))?;

    let total = resp.content_length().or(task.size);
    if let Some(pb) = pb {
        if let Some(total) = total {
            pb.set_length(total);
        }
        pb.set_position(0);
        pb.set_message(short_name(&task.path));
    }

    let tmp = temp_path(&task.path);
    let mut file = fs::File::create(&tmp)
        .with_context(|| format!("无法写入文件 {}", tmp.display()))?;
    let mut hasher = Sha1::new();
    let mut reader = resp;
    let mut buf = vec![0u8; 16 * 1024];

    loop {
        let read = reader.read(&mut buf).context("读取下载数据失败")?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read]).context("写入文件失败")?;
        if let Some(pb) = pb {
            pb.inc(read as u64);
        }
    }
    drop(file);

    if let Some(expected) = &task.sha1 {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&tmp);
            bail!(
                "文件校验失败: {}（期望 {expected}，实际 {actual}）",
                task.path.display()
            );
        }
    }

    if task.path.exists() {
        let _ = fs::remove_file(&task.path);
    }
    fs::rename(&tmp, &task.path)
        .with_context(|| format!("无法保存文件 {}", task.path.display()))?;
    Ok(())
}

fn download_retry(client: &Client, task: &DownloadTask, pb: Option<&ProgressBar>) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..3 {
        match download_once(client, task, pb) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt < 2 {
                    thread::sleep(Duration::from_millis(800));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("未知下载错误")))
}

pub fn download_one(client: &Client, task: &DownloadTask, label: &str) -> Result<()> {
    if existing_is_ok(task) {
        return Ok(());
    }
    let pb = ProgressBar::new(task.size.unwrap_or(0));
    pb.set_style(file_style());
    pb.set_message(label.to_string());
    let result = download_retry(client, task, Some(&pb));
    pb.finish_and_clear();
    result
}

/// 多线程批量下载，已有且大小一致的文件会被跳过。
pub fn download_many(client: &Client, mut tasks: Vec<DownloadTask>, label: &str) -> Result<()> {
    tasks.sort_by(|a, b| a.path.cmp(&b.path));
    tasks.dedup_by(|a, b| a.path == b.path);
    if tasks.is_empty() {
        return Ok(());
    }

    let total = tasks.len() as u64;
    let pb = ProgressBar::new(total);
    pb.set_style(count_style());
    pb.set_message(label.to_string());

    let queue = Arc::new(Mutex::new(tasks.into_iter().collect::<VecDeque<_>>()));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let workers = (total as usize).clamp(1, 8);

    thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let errors = Arc::clone(&errors);
            let pb = pb.clone();
            scope.spawn(move || {
                loop {
                    let task = queue.lock().unwrap().pop_front();
                    let Some(task) = task else {
                        break;
                    };
                    if let Err(err) = download_retry(client, &task, None) {
                        errors
                            .lock()
                            .unwrap()
                            .push(format!("{}\n  {err:#}", task.url));
                    }
                    pb.inc(1);
                }
            });
        }
    });

    pb.finish_and_clear();
    let errors = errors.lock().unwrap();
    if !errors.is_empty() {
        bail!(
            "有 {} 个文件下载失败：\n{}",
            errors.len(),
            errors.join("\n")
        );
    }
    Ok(())
}

fn temp_path(path: &std::path::Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    path.with_file_name(format!("{name}.part"))
}

fn short_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
