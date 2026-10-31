use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::SystemTime;

use crate::config;
use crate::mirror::MirrorKind;
use crate::net;

const CACHE_FILE: &str = "version_manifest_v2.json";
/// 缓存有效期：6 小时内直接复用，避免每次启动都等待网络。
const CACHE_TTL_SECS: u64 = 6 * 60 * 60;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionManifest {
    pub latest: LatestVersions,
    pub versions: Vec<VersionEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LatestVersions {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(rename = "releaseTime")]
    pub release_time: String,
}

impl VersionManifest {
    pub fn find(&self, id: &str) -> Option<&VersionEntry> {
        self.versions.iter().find(|v| v.id == id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.find(id).is_some()
    }
}

impl VersionEntry {
    pub fn kind_label(&self) -> &'static str {
        match self.kind.as_str() {
            "release" => "正式版",
            "snapshot" => "快照",
            "old_beta" => "远古 Beta",
            "old_alpha" => "远古 Alpha",
            _ => "其他",
        }
    }

    pub fn date(&self) -> String {
        self.release_time.chars().take(10).collect()
    }
}

/// 获取版本清单。优先读 30 分钟内的缓存，返回的 `bool` 表示数据是否来自缓存。
pub fn fetch(
    client: &reqwest::blocking::Client,
    mirror: MirrorKind,
    force: bool,
) -> Result<(VersionManifest, bool)> {
    let cache = config::cache_dir().join(CACHE_FILE);

    if !force
        && cache_is_fresh(&cache)
        && let Ok(manifest) = read_cache(&cache)
    {
        return Ok((manifest, true));
    }

    match net::get_text(client, mirror.manifest_url()) {
        Ok(text) => {
            let manifest: VersionManifest =
                serde_json::from_str(&text).context("解析版本清单失败")?;
            let _ = fs::create_dir_all(config::cache_dir());
            let _ = fs::write(&cache, &text);
            Ok((manifest, false))
        }
        Err(err) => {
            if let Ok(manifest) = read_cache(&cache) {
                Ok((manifest, true))
            } else {
                Err(err)
            }
        }
    }
}

fn cache_is_fresh(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age.as_secs() <= CACHE_TTL_SECS
}

fn read_cache(path: &Path) -> Result<VersionManifest> {
    let text = fs::read_to_string(path).context("读取版本列表缓存失败")?;
    serde_json::from_str(&text).context("解析版本列表缓存失败")
}
