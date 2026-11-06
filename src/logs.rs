use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};

use crate::config::{cache_dir, LogLevel};

/// 当前日志文件。
static FILE: Mutex<Option<File>> = Mutex::new(None);
/// 当前日志文件路径。
static PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
/// 是否记录“全部”（含调试）日志。
static ALL: AtomicBool = AtomicBool::new(false);

/// 创建本次运行的日志文件，文件名为 `YYYYMMdd-hh-mm-ss.log`，存放于 `.nrel-cache/logs`。
///
/// Windows 文件名不允许出现冒号，因此时间部分用短横线分隔。
pub fn init(level: LogLevel) -> Result<PathBuf> {
    let dir = cache_dir().join("logs");
    fs::create_dir_all(&dir).with_context(|| format!("无法创建日志目录 {}", dir.display()))?;
    let (date, time) = now_parts();
    let stem = format!(
        "{date:08}-{:02}-{:02}-{:02}",
        time.0, time.1, time.2
    );
    let path = dir.join(format!("{stem}.log"));
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("无法创建日志文件 {}", path.display()))?;

    if let Ok(mut guard) = FILE.lock() {
        *guard = Some(file);
    }
    if let Ok(mut guard) = PATH.lock() {
        *guard = Some(path.clone());
    }
    set_level(level);
    info(format!(
        "==== NREL 启动器 v{} 启动，日志等级：{} ====",
        env!("CARGO_PKG_VERSION"),
        level.label()
    ));
    Ok(path)
}

pub fn set_level(level: LogLevel) {
    ALL.store(matches!(level, LogLevel::All), Ordering::Relaxed);
}

/// 当前日志文件路径。
pub fn path() -> Option<PathBuf> {
    PATH.lock().ok().and_then(|guard| guard.clone())
}

/// 记录信息级日志。
pub fn info(message: impl fmt::Display) {
    write("INFO", &message.to_string());
}

/// 记录错误级日志。
pub fn error(message: impl fmt::Display) {
    write("ERROR", &message.to_string());
}

/// 仅在“全部”等级下记录调试日志（消息按需构造）。
pub fn detail(message: impl FnOnce() -> String) {
    if ALL.load(Ordering::Relaxed) {
        write("DEBUG", &message());
    }
}

fn write(tag: &str, message: &str) {
    let (_, time) = now_parts();
    if let Ok(mut guard) = FILE.lock()
        && let Some(file) = guard.as_mut()
    {
        let _ = writeln!(
            file,
            "[{:02}:{:02}:{:02}] [{tag}] {message}",
            time.0, time.1, time.2
        );
        let _ = file.flush();
    }
}

/// 返回本地日期（YYYYMMDD）与时间（时、分、秒）。
fn now_parts() -> (u32, (u8, u8, u8)) {
    let (year, month, day, hour, minute, second) = platform_parts();
    (
        year as u32 * 10000 + month as u32 * 100 + day as u32,
        (hour, minute, second),
    )
}

#[cfg(windows)]
fn platform_parts() -> (u16, u8, u8, u8, u8, u8) {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;

    // SAFETY: GetLocalTime 只写入调用方提供的 SYSTEMTIME。
    let mut st: SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { GetLocalTime(&mut st) };
    (
        st.wYear,
        st.wMonth as u8,
        st.wDay as u8,
        st.wHour as u8,
        st.wMinute as u8,
        st.wSecond as u8,
    )
}

#[cfg(not(windows))]
fn platform_parts() -> (u16, u8, u8, u8, u8, u8) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        + local_offset_secs();
    let days = seconds.div_euclid(86400);
    let rest = seconds.rem_euclid(86400);
    let (year, month, day) = civil_from_days(days);
    (
        year as u16,
        month as u8,
        day as u8,
        (rest / 3600) as u8,
        ((rest / 60) % 60) as u8,
        (rest % 60) as u8,
    )
}

/// 非 Windows 平台通过 `date +%z` 取一次本地时区偏移。
#[cfg(not(windows))]
fn local_offset_secs() -> i64 {
    use std::sync::OnceLock;

    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        let Ok(output) = std::process::Command::new("date").arg("+%z").output() else {
            return 0;
        };
        let text = String::from_utf8_lossy(&output.stdout);
        let text = text.trim();
        let bytes = text.as_bytes();
        if bytes.len() < 5 {
            return 0;
        }
        let sign = match bytes[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return 0,
        };
        let hours: i64 = text[1..3].parse().unwrap_or(0);
        let minutes: i64 = text[3..5].parse().unwrap_or(0);
        sign * (hours * 3600 + minutes * 60)
    })
}

/// 由 Unix 天数换算公历年月日（UTC）。
#[cfg(not(windows))]
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    (year as i32, month as u32, day as u32)
}
