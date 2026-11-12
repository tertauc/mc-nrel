//! 启动器自身的更新检查和安装。

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::UpdateMode;
use crate::net::{self, DownloadTask, HttpClient};

const API: &str = "https://api.github.com/repos/tertauc/mc-nrel/releases/latest";

pub struct Release {
    pub version: String,
    pub url: String,
}

/// 查询是否有比当前版本更新的发布。
pub fn check(client: &HttpClient) -> Result<Option<Release>> {
    let text = client.get_text(API).context("无法访问更新服务")?;
    let json: serde_json::Value =
        serde_json::from_str(&text).context("解析更新信息失败")?;
    let tag = json
        .get("tag_name")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .trim_start_matches(['v', 'V'])
        .to_string();
    if tag.is_empty() || !is_newer(&tag) {
        return Ok(None);
    }
    let url = json
        .get("assets")
        .and_then(|value| value.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                let name = asset.get("name")?.as_str()?;
                if !name.to_ascii_lowercase().ends_with(".exe") {
                    return None;
                }
                asset
                    .get("browser_download_url")
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
        });
    let Some(url) = url else {
        return Ok(None);
    };
    Ok(Some(Release { version: tag, url }))
}

fn is_newer(remote: &str) -> bool {
    parse_version(remote) > parse_version(env!("CARGO_PKG_VERSION"))
}

fn parse_version(text: &str) -> (u64, u64, u64) {
    let mut parts = text
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u64>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

/// 下载新版本并替换当前可执行文件，成功后需要重启生效。
pub fn install(client: &HttpClient, release: &Release) -> Result<()> {
    let current = std::env::current_exe().context("无法定位启动器本体")?;
    let dir = current
        .parent()
        .map(PathBuf::from)
        .context("无法定位启动器目录")?;
    let new_file = dir.join("nrel-new.exe");
    let old_file = dir.join("nrel-old.exe");

    let task = DownloadTask::new(&release.url, new_file.clone());
    net::download_one(client, &task, "下载启动器更新")?;
    if !new_file.is_file() {
        bail!("更新文件下载失败");
    }

    let _ = std::fs::remove_file(&old_file);
    // 运行中的可执行文件不能直接覆盖，先改名再放入新文件。
    std::fs::rename(&current, &old_file).context("无法替换启动器（可能没有写入权限）")?;
    if let Err(err) = std::fs::rename(&new_file, &current) {
        let _ = std::fs::rename(&old_file, &current);
        return Err(err).context("写入新版本失败");
    }
    crate::logs::info(format!("启动器已更新到 {}", release.version));
    Ok(())
}

/// 启动更新后的启动器。
pub fn relaunch() {
    if let Ok(current) = std::env::current_exe() {
        let _ = std::process::Command::new(current).spawn();
    }
}

pub fn should_check(mode: UpdateMode) -> bool {
    mode != UpdateMode::Off
}
