use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=res/nrel.rc");
    println!("cargo:rerun-if-changed=res/icon.ico");

    // 仅在 Windows 目标下嵌入图标资源，其他平台无需处理。
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("缺少 OUT_DIR"));
    let rc_path = Path::new("res/nrel.rc");
    let msvc = env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");

    // MSVC 链接器接受 .res，GNU 链接器需要 COFF 对象。
    let (format, file_name) = if msvc {
        ("res", "nrel.res")
    } else {
        ("coff", "nrel_icon.o")
    };
    let output = out_dir.join(file_name);

    let result = match compile_with_windres(rc_path, &output, format) {
        Ok(()) => Ok(()),
        Err(windres_err) if msvc => compile_with_rc(rc_path, &output)
            .map_err(|rc_err| format!("{windres_err}；{rc_err}")),
        Err(windres_err) => Err(windres_err),
    };

    match result {
        Ok(()) => println!("cargo:rustc-link-arg={}", output.display()),
        Err(err) => println!("cargo:warning=未能嵌入 Windows 图标：{err}"),
    }
}

/// 在 PATH 中查找可执行文件。
///
/// 这里按 `;` 手工拆分并去掉多余引号，避免 PATH 中个别带引号的条目
/// 被当成一条超长路径，导致后面的编译器目录被漏掉。
fn find_tool(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    let path = path.to_string_lossy();
    path.split(';')
        .map(|entry| entry.trim().trim_matches('"'))
        .filter(|entry| !entry.is_empty())
        .map(|entry| Path::new(entry).join(name))
        .find(|candidate| candidate.is_file())
}

/// 使用 MinGW 的 windres 编译资源，输出到指定路径。
fn compile_with_windres(rc_path: &Path, output: &Path, format: &str) -> Result<(), String> {
    let windres = find_tool("windres.exe").ok_or_else(|| "未找到 windres.exe".to_string())?;
    let output_result = Command::new(windres)
        .arg(rc_path)
        .args(["-O", format, "-I", "res", "-o"])
        .arg(output)
        .output()
        .map_err(|err| format!("windres 启动失败: {err}"))?;
    if output_result.status.success() {
        return Ok(());
    }
    Err(format!(
        "windres 编译失败: {}",
        String::from_utf8_lossy(&output_result.stderr).trim()
    ))
}

/// 使用 Windows SDK 的 rc.exe 编译资源。
fn compile_with_rc(rc_path: &Path, output: &Path) -> Result<(), String> {
    let rc = find_tool("rc.exe").ok_or_else(|| "未找到 rc.exe".to_string())?;
    let output_result = Command::new(rc)
        .args(["/nologo", "/i", "res", "/fo"])
        .arg(output)
        .arg(rc_path)
        .output()
        .map_err(|err| format!("rc.exe 启动失败: {err}"))?;
    if output_result.status.success() {
        return Ok(());
    }
    Err(format!(
        "rc.exe 编译失败: {}",
        String::from_utf8_lossy(&output_result.stderr).trim()
    ))
}
