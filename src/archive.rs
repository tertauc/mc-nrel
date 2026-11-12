//! 极简 ZIP 解包，只支持存储与 deflate 两种方式，用于解出 natives。

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    let bytes = buf.get(at..at + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    let bytes = buf.get(at..at + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// 去掉绝对路径与 `..`，防止解压到目标目录之外。
fn safe_relative(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return None;
    }
    Some(path.to_path_buf())
}

pub fn extract(jar: &Path, dest: &Path, exclude: &[String]) -> Result<()> {
    let mut file = fs::File::open(jar)
        .with_context(|| format!("无法打开本地库 {}", jar.display()))?;
    let len = file.metadata()?.len();
    let tail_len = len.min(66_000) as usize;
    if tail_len < 22 {
        bail!("本地库不是有效的 zip：{}", jar.display());
    }
    file.seek(SeekFrom::Start(len - tail_len as u64))?;
    let mut tail = vec![0u8; tail_len];
    file.read_exact(&mut tail)?;

    let eocd = (0..tail.len().saturating_sub(3))
        .rev()
        .find(|&index| tail[index..index + 4] == [0x50, 0x4b, 0x05, 0x06])
        .context("本地库不是有效的 zip")?;
    let count = u16_at(&tail, eocd + 10).context("zip 目录损坏")? as usize;
    let size = u32_at(&tail, eocd + 12).context("zip 目录损坏")? as usize;
    let offset = u32_at(&tail, eocd + 16).context("zip 目录损坏")? as u64;

    let mut dir = vec![0u8; size];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut dir)?;

    let mut pos = 0usize;
    for _ in 0..count {
        if u32_at(&dir, pos) != Some(0x0201_4b50) {
            break;
        }
        let method = u16_at(&dir, pos + 10).context("zip 条目损坏")?;
        let packed = u32_at(&dir, pos + 20).context("zip 条目损坏")? as u64;
        let name_len = u16_at(&dir, pos + 28).context("zip 条目损坏")? as usize;
        let extra_len = u16_at(&dir, pos + 30).context("zip 条目损坏")? as usize;
        let comment_len = u16_at(&dir, pos + 32).context("zip 条目损坏")? as usize;
        let local = u32_at(&dir, pos + 42).context("zip 条目损坏")? as u64;
        let Some(raw_name) = dir.get(pos + 46..pos + 46 + name_len) else {
            bail!("zip 条目损坏");
        };
        let name = String::from_utf8_lossy(raw_name).replace('\\', "/");
        pos += 46 + name_len + extra_len + comment_len;

        if name.ends_with('/') || exclude.iter().any(|prefix| name.starts_with(prefix.as_str()))
        {
            continue;
        }
        let Some(relative) = safe_relative(&name) else {
            continue;
        };

        file.seek(SeekFrom::Start(local))?;
        let mut header = [0u8; 30];
        file.read_exact(&mut header)?;
        if u32_at(&header, 0) != Some(0x0403_4b50) {
            bail!("zip 条目头部损坏：{name}");
        }
        let header_name = u16_at(&header, 26).context("zip 条目损坏")? as usize;
        let header_extra = u16_at(&header, 28).context("zip 条目损坏")? as usize;
        file.seek(SeekFrom::Start(
            local + 30 + header_name as u64 + header_extra as u64,
        ))?;

        let out: PathBuf = dest.join(&relative);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file =
            fs::File::create(&out).with_context(|| format!("无法解压文件 {}", out.display()))?;
        match method {
            0 => {
                std::io::copy(&mut file.by_ref().take(packed), &mut out_file)?;
            }
            8 => {
                let mut decoder =
                    flate2::read::DeflateDecoder::new(file.by_ref().take(packed));
                std::io::copy(&mut decoder, &mut out_file)?;
            }
            _ => continue,
        }
    }
    Ok(())
}
