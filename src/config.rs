use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::mirror::MirrorKind;

const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginMode {
    #[default]
    Offline,
    Microsoft,
}

impl LoginMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Offline => "离线",
            Self::Microsoft => "微软",
        }
    }
}

/// 日志记录等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    /// 仅记录信息级及以上。
    #[default]
    Info,
    /// 记录全部日志（含调试细节）。
    All,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "信息及以上",
            Self::All => "全部（含调试）",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub username: String,
    pub login_mode: LoginMode,
    pub last_version: Option<String>,
    /// `None` 表示自动分配内存。
    pub memory_mb: Option<u64>,
    pub java_path: Option<String>,
    pub mirror: MirrorKind,
    pub game_dir: Option<String>,
    /// 每个版本使用独立的 `versions/<id>` 作为游戏目录。
    pub isolate_versions: bool,
    /// 日志记录等级。
    pub log_level: LogLevel,
    pub width: u32,
    pub height: u32,
    pub fullscreen: bool,
    pub extra_jvm_args: String,

    pub ms_client_id: Option<String>,
    pub ms_refresh_token: Option<String>,
    pub ms_access_token: Option<String>,
    pub ms_expires_at: i64,
    pub ms_name: Option<String>,
    pub ms_uuid: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            username: "Steve".to_string(),
            login_mode: LoginMode::Offline,
            last_version: None,
            memory_mb: None,
            java_path: None,
            mirror: MirrorKind::default(),
            game_dir: None,
            isolate_versions: true,
            log_level: LogLevel::default(),
            width: 854,
            height: 480,
            fullscreen: false,
            extra_jvm_args: String::new(),
            ms_client_id: None,
            ms_refresh_token: None,
            ms_access_token: None,
            ms_expires_at: 0,
            ms_name: None,
            ms_uuid: None,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        match fs::read_to_string(Self::path()) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        fs::write(Self::path(), text).with_context(|| "无法保存启动器配置")?;
        Ok(())
    }

    pub fn path() -> PathBuf {
        work_dir().join(CONFIG_FILE)
    }

    pub fn game_dir(&self) -> PathBuf {
        match &self.game_dir {
            Some(dir) if !dir.trim().is_empty() => PathBuf::from(dir),
            _ => default_game_dir(),
        }
    }

    /// 游戏实际使用的工作目录：开启版本隔离时为 `versions/<id>`。
    pub fn version_game_dir(&self, version_id: &str) -> PathBuf {
        if self.isolate_versions {
            self.game_dir().join("versions").join(version_id)
        } else {
            self.game_dir()
        }
    }

    pub fn isolation_label(&self) -> &'static str {
        if self.isolate_versions { "开启" } else { "关闭" }
    }

    pub fn effective_memory_mb(&self) -> u64 {
        self.memory_mb.unwrap_or_else(auto_memory_mb)
    }

    pub fn ms_client_id(&self) -> String {
        match &self.ms_client_id {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => crate::auth::DEFAULT_CLIENT_ID.to_string(),
        }
    }

    pub fn account_label(&self) -> String {
        match self.login_mode {
            LoginMode::Offline => format!("离线 · {}", self.username),
            LoginMode::Microsoft => match &self.ms_name {
                Some(name) => format!("微软 · {name}"),
                None => "微软 · 未登录".to_string(),
            },
        }
    }
}

/// 启动器工作目录：当前命令行所在目录。
pub fn work_dir() -> PathBuf {
    env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

pub fn cache_dir() -> PathBuf {
    work_dir().join(".nrel-cache")
}

fn default_game_dir() -> PathBuf {
    work_dir().join(".minecraft")
}

/// 自动内存：取物理内存的一半，限制在 2GB ~ 8GB，并按 512MB 取整。
pub fn auto_memory_mb() -> u64 {
    let total = total_memory_mb();
    if total == 0 {
        return 4096;
    }
    let raw = (total / 2).clamp(2048, 8192);
    (raw / 512) * 512
}

pub fn total_memory_mb() -> u64 {
    platform_total_memory_mb()
}

#[cfg(windows)]
fn platform_total_memory_mb() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetPhysicallyInstalledSystemMemory;

    let mut kb: u64 = 0;
    // SAFETY: 该 API 只写入调用方提供的 u64 指针。
    let ok = unsafe { GetPhysicallyInstalledSystemMemory(&mut kb) };
    if ok != 0 { kb / 1024 } else { 0 }
}

#[cfg(not(windows))]
fn platform_total_memory_mb() -> u64 {
    // 非 Windows 平台仅做兜底估算。
    8192
}
