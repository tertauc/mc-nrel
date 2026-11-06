use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct JavaInfo {
    pub path: PathBuf,
    pub major: u32,
    pub bits: u32,
}

impl JavaInfo {
    pub fn label(&self) -> String {
        format!(
            "Java {}（{} 位）· {}",
            self.major,
            self.bits,
            self.path.display()
        )
    }
}

static CACHE: OnceLock<Vec<JavaInfo>> = OnceLock::new();

pub fn detect_all() -> Vec<JavaInfo> {
    CACHE.get_or_init(scan_all).clone()
}

fn scan_all() -> Vec<JavaInfo> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();

    let mut push = |path: PathBuf| {
        if !path.is_file() {
            return;
        }
        let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            candidates.push(path);
        }
    };

    if let Some(home) = env::var_os("JAVA_HOME") {
        push(PathBuf::from(home).join("bin").join(java_exe()));
    }

    if let Some(path_var) = env::var_os("PATH") {
        for dir in env::split_paths(&path_var) {
            push(dir.join(java_exe()));
        }
    }

    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(program_files) = env::var_os("ProgramFiles") {
        push_vendor_dirs(&PathBuf::from(program_files), &mut bases);
    }
    if let Some(program_files) = env::var_os("ProgramFiles(x86)") {
        push_vendor_dirs(&PathBuf::from(program_files), &mut bases);
    }
    if let Some(local) = env::var_os("LOCALAPPDATA") {
        let programs = PathBuf::from(local).join("Programs");
        push_vendor_dirs(&programs, &mut bases);
    }
    if let Some(home) = env::var_os("USERPROFILE") {
        let home = PathBuf::from(home);
        bases.push(home.join(".jdks"));
        bases.push(home.join(".gradle").join("jdks"));
    }
    #[cfg(target_os = "macos")]
    {
        bases.push(PathBuf::from("/Library/Java/JavaVirtualMachines"));
        if let Some(home) = env::var_os("HOME") {
            bases.push(PathBuf::from(home).join("Library/Java/JavaVirtualMachines"));
        }
    }
    #[cfg(target_os = "linux")]
    {
        bases.push(PathBuf::from("/usr/lib/jvm"));
        bases.push(PathBuf::from("/usr/share/jvm"));
        if let Some(home) = env::var_os("HOME") {
            bases.push(PathBuf::from(home).join(".sdkman/candidates/java"));
        }
    }

    for base in &bases {
        scan_base(base, &mut push);
    }

    let mut result: Vec<JavaInfo> = candidates
        .iter()
        .filter_map(|path| probe(path.as_path()))
        .collect();
    result.sort_by(|a, b| {
        (a.major, std::cmp::Reverse(a.bits), &a.path).cmp(&(b.major, std::cmp::Reverse(b.bits), &b.path))
    });
    result
}

fn push_vendor_dirs(root: &Path, bases: &mut Vec<PathBuf>) {
    for name in [
        "Java",
        "Eclipse Adoptium",
        "AdoptOpenJDK",
        "Microsoft",
        "Zulu",
        "BellSoft",
        "Amazon Corretto",
        "Semeru",
        "IBM",
        "JetBrains",
    ] {
        let dir = root.join(name);
        if dir.is_dir() {
            bases.push(dir);
        }
    }
}

fn scan_base(base: &Path, push: &mut impl FnMut(PathBuf)) {
    push(base.join("bin").join(java_exe()));
    push(base.join("jbr").join("bin").join(java_exe()));

    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        push(path.join("bin").join(java_exe()));
        push(path.join("jbr").join("bin").join(java_exe()));
        push(path.join("Contents").join("Home").join("bin").join(java_exe()));
    }
}

fn java_exe() -> &'static str {
    if cfg!(windows) { "java.exe" } else { "java" }
}

pub fn probe(path: &Path) -> Option<JavaInfo> {
    let mut command = Command::new(path);
    command
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let output = command.output().ok()?;
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stdout).into_owned();
    }
    if text.trim().is_empty() {
        return None;
    }

    let version = text
        .split('"')
        .nth(1)
        .map(|value| value.to_string())
        .or_else(|| {
            text.lines().next().and_then(|line| {
                line.split_whitespace()
                    .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
                    .map(|token| token.trim_matches(|c: char| c == '"' || c == ',').to_string())
            })
        })?;

    let major = parse_major(&version);
    if major == 0 {
        return None;
    }
    let bits = if text.contains("64-Bit") || text.contains("64-bit") {
        64
    } else {
        32
    };

    Some(JavaInfo {
        path: path.to_path_buf(),
        major,
        bits,
    })
}

fn parse_major(version: &str) -> u32 {
    let cleaned = version
        .split(['-', '+', '_'])
        .next()
        .unwrap_or(version)
        .trim();
    let mut parts = cleaned.split('.');
    let first: u32 = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
    if first == 1 {
        parts.next().and_then(|part| part.parse().ok()).unwrap_or(0)
    } else {
        first
    }
}

pub fn pick(configured: Option<&str>, required: u32) -> Result<JavaInfo> {
    if let Some(path) = configured {
        let path = PathBuf::from(path);
        if path.is_file()
            && let Some(info) = probe(&path)
            && info.major >= required
        {
            return Ok(info);
        }
    }

    let mut candidates: Vec<JavaInfo> = detect_all()
        .into_iter()
        .filter(|java| java.major >= required)
        .collect();
    if candidates.is_empty() {
        bail!(
            "未找到满足 Java {required}+ 的运行环境，请在“启动器设置 → Java”中手动指定 {} 路径",
            java_exe()
        );
    }
    candidates.sort_by_key(|java| (java.major, if java.bits == 64 { 0 } else { 1 }));
    Ok(candidates.remove(0))
}
