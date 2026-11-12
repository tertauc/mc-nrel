//! 把文件或目录移入系统回收站，而不是直接删除。

use std::path::Path;

use anyhow::Result;
#[cfg(windows)]
use anyhow::bail;
#[cfg(not(windows))]
use anyhow::Context;
#[cfg(not(windows))]
use std::path::PathBuf;

#[cfg(windows)]
pub fn move_to_trash(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        SHFileOperationW, SHFILEOPSTRUCTW,
    };

    // pFrom 需要以连续两个 0 结尾。
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide.push(0);

    let mut operation: SHFILEOPSTRUCTW = unsafe { std::mem::zeroed() };
    operation.wFunc = FO_DELETE;
    operation.pFrom = wide.as_ptr();
    operation.fFlags =
        (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI) as u16;

    let code = unsafe { SHFileOperationW(&mut operation) };
    if code != 0 || operation.fAnyOperationsAborted != 0 {
        bail!("移入回收站失败（错误码 {code}）");
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn move_to_trash(path: &Path) -> Result<()> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "deleted".to_string());

    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").context("无法定位用户主目录")?;
        let trash = PathBuf::from(home).join(".Trash");
        std::fs::create_dir_all(&trash)?;
        let target = unique_path(&trash, &name);
        std::fs::rename(path, &target)
            .or_else(|_| copy_then_remove(path, &target))
            .with_context(|| format!("移入回收站失败：{}", path.display()))?;
        return Ok(());
    }

    // Linux 遵循 XDG 回收站规范。
    let home = std::env::var_os("HOME").context("无法定位用户主目录")?;
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"));
    let base = data.join("Trash");
    let files = base.join("files");
    let info = base.join("info");
    std::fs::create_dir_all(&files)?;
    std::fs::create_dir_all(&info)?;

    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let target = unique_path(&files, &name);
    std::fs::rename(path, &target)
        .or_else(|_| copy_then_remove(path, &target))
        .with_context(|| format!("移入回收站失败：{}", path.display()))?;

    let mut entry = String::from("[Trash Info]\n");
    entry.push_str(&format!("Path={}\n", abs.display()));
    entry.push_str(&format!(
        "DeletionDate={}\n",
        chrono_like_now()
    ));
    std::fs::write(
        info.join(format!(
            "{}.trashinfo",
            target
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "deleted".to_string())
        )),
        entry,
    )?;
    Ok(())
}

#[cfg(not(windows))]
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let mut candidate = dir.join(name);
    let mut index = 1;
    while candidate.exists() {
        candidate = dir.join(format!("{name}.{index}"));
        index += 1;
    }
    candidate
}

#[cfg(not(windows))]
fn copy_then_remove(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// 供 `.trashinfo` 使用的本地时间，格式与 XDG 规范一致。
#[cfg(not(windows))]
fn chrono_like_now() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = seconds.div_euclid(86_400);
    let rem = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

#[cfg(not(windows))]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
