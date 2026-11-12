//! 通过改写系统 hosts 加速 GitHub 访问，退出时自动还原。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};

use crate::net::HttpClient;

const BEGIN: &str = "# >>> NREL github accelerator begin >>>";
const END: &str = "# <<< NREL github accelerator end <<<";
/// 公开维护的 GitHub 可用地址列表。
const SOURCE: &str = "https://raw.hellogithub.com/hosts";

static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::SeqCst)
}

fn hosts_path() -> PathBuf {
    #[cfg(windows)]
    {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
        PathBuf::from(root).join("System32\\drivers\\etc\\hosts")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/etc/hosts")
    }
}

/// 写入加速条目，失败通常是因为没有管理员权限。
pub fn apply(client: &HttpClient) -> Result<()> {
    if is_active() {
        return Ok(());
    }
    let path = hosts_path();
    // 没有写权限时直接跳过，避免白跑一次网络请求。
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .with_context(|| "没有修改 hosts 的权限，请以管理员身份运行")?;

    let list = client
        .get_text(SOURCE)
        .context("获取 GitHub 加速地址失败")?;
    let entries: Vec<&str> = list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("github") || lower.contains("githubusercontent")
        })
        .collect();
    if entries.is_empty() {
        bail!("没有可用的加速地址");
    }

    let original = std::fs::read_to_string(&path).unwrap_or_default();
    let mut text = strip(&original);
    if !text.is_empty() && !text.ends_with('\n') {
        text.push_str("\r\n");
    }
    text.push_str(BEGIN);
    text.push_str("\r\n");
    for entry in &entries {
        text.push_str(entry);
        text.push_str("\r\n");
    }
    text.push_str(END);
    text.push_str("\r\n");
    std::fs::write(&path, text)
        .with_context(|| format!("无法写入 hosts（可能需要管理员权限）：{}", path.display()))?;
    ACTIVE.store(true, Ordering::SeqCst);
    crate::logs::info(format!("已写入 GitHub 加速条目：{} 条", entries.len()));
    Ok(())
}

/// 删除自己追加的内容，保留系统原有条目。
pub fn restore() {
    let path = hosts_path();
    ACTIVE.store(false, Ordering::SeqCst);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if !text.contains(BEGIN) {
        return;
    }
    let cleaned = strip(&text);
    if std::fs::write(&path, cleaned).is_ok() {
        crate::logs::info("已还原 hosts");
    } else {
        crate::logs::error("还原 hosts 失败，请手动删除 NREL 相关条目");
    }
}

fn strip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut inside = false;
    for line in text.lines() {
        if line.trim() == BEGIN {
            inside = true;
            continue;
        }
        if line.trim() == END {
            inside = false;
            continue;
        }
        if inside {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out
}

/// 注册控制台事件处理：关闭窗口、注销或关机时也要还原 hosts。
#[cfg(windows)]
pub fn install_console_handler() {
    use windows_sys::Win32::System::Console::{CTRL_CLOSE_EVENT, SetConsoleCtrlHandler};

    unsafe extern "system" fn handler(event: u32) -> i32 {
        restore();
        // 关闭窗口时让系统继续按默认流程结束进程。
        if event == CTRL_CLOSE_EVENT { 0 } else { 1 }
    }

    unsafe {
        SetConsoleCtrlHandler(Some(handler), 1);
    }
}

#[cfg(not(windows))]
pub fn install_console_handler() {
    use std::sync::Once;

    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // 非 Windows 平台通过 ctrlc 行为受限，只能依赖正常退出。
    });
}
