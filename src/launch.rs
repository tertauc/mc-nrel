use std::collections::HashMap;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::auth::Session;
use crate::config::Config;
use crate::install::{self, VersionJson};
use crate::java::JavaInfo;

/// 首次启动前写入 options.txt 的初始项：简体中文、关闭旁白与无障碍引导。
///
/// 只在该文件仍是“未完成首次引导”的状态时改动语言和旁白，
/// 之后玩家在游戏里自行调整的设置不会被覆盖。
pub fn prepare_options(cfg: &Config, version: &VersionJson) -> Result<()> {
    let path = cfg.version_game_dir(&version.id).join("options.txt");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("无法创建目录 {}", parent.display()))?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = existing.lines().map(|line| line.to_string()).collect();

    let onboard = lines
        .iter()
        .find(|line| line.starts_with("onboardAccessibility:"));
    let first_run = match onboard {
        Some(line) => line.ends_with(":true"),
        None => !path.is_file(),
    };

    if first_run {
        set_option(&mut lines, "lang", language_code(&version.id));
        set_option(&mut lines, "narrator", "0");
    }
    set_option(&mut lines, "onboardAccessibility", "false");

    let mut text = lines.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    if text != existing {
        std::fs::write(&path, text)
            .with_context(|| format!("无法写入 {}", path.display()))?;
    }
    Ok(())
}

/// 1.13 之前的版本使用大写的地区代码（zh_CN）。
fn language_code(version_id: &str) -> &'static str {
    let lower = version_id.to_ascii_lowercase();
    if lower.starts_with('a') || lower.starts_with('b') {
        return "zh_CN";
    }
    if let Some(rest) = lower.strip_prefix("1.")
        && let Some(minor) = rest.split('.').next()
        && minor.parse::<u32>().is_ok_and(|minor| minor < 13)
    {
        return "zh_CN";
    }
    "zh_cn"
}

fn set_option(lines: &mut Vec<String>, key: &str, value: &str) {
    let prefix = format!("{key}:");
    let new_line = format!("{key}:{value}");
    if let Some(line) = lines.iter_mut().find(|line| line.starts_with(&prefix)) {
        *line = new_line;
    } else {
        lines.push(new_line);
    }
}

pub fn required_java_major(version: &VersionJson) -> u32 {
    version
        .java_version
        .as_ref()
        .map(|j| j.major)
        .filter(|major| *major > 0)
        .unwrap_or(8)
}

pub fn build_command(
    cfg: &Config,
    java: &JavaInfo,
    version: &VersionJson,
    session: &Session,
) -> Result<Command> {
    let root = cfg.game_dir();
    if version.main_class.is_empty() {
        bail!("版本 {} 的 JSON 缺少 mainClass，无法启动", version.id);
    }
    let game_jar = install::version_jar_path(&root, &version.id);
    if !game_jar.is_file() {
        bail!("客户端文件缺失: {}", game_jar.display());
    }

    let natives = install::natives_dir(&root, &version.id);
    std::fs::create_dir_all(&natives)?;

    let features = install::launch_features();
    let plan = install::plan_libraries(version, &root, cfg.mirror, &features)?;

    let mut classpath_entries: Vec<String> = plan
        .classpath
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    classpath_entries.push(game_jar.display().to_string());
    let separator = if cfg!(windows) { ";" } else { ":" };
    let classpath = classpath_entries.join(separator);

    let assets_root = install::assets_dir(&root);
    let assets_index = version
        .asset_index
        .as_ref()
        .map(|index| index.id.clone())
        .or_else(|| version.assets.clone())
        .unwrap_or_else(|| "legacy".to_string());
    let game_assets = if version
        .asset_index
        .as_ref()
        .map(|index| install::asset_index_is_virtual(&root, index))
        .unwrap_or(false)
    {
        assets_root.join("virtual").join(&assets_index)
    } else {
        assets_root.clone()
    };

    let memory = cfg.effective_memory_mb();
    let min_memory = memory.min(1024);

    let game_dir = cfg.version_game_dir(&version.id);
    std::fs::create_dir_all(&game_dir)
        .with_context(|| format!("无法创建游戏目录 {}", game_dir.display()))?;

    let mut vars: HashMap<&str, String> = HashMap::new();
    vars.insert("auth_player_name", session.username.clone());
    vars.insert("version_name", version.id.clone());
    vars.insert("game_directory", game_dir.display().to_string());
    vars.insert("assets_root", assets_root.display().to_string());
    vars.insert("game_assets", game_assets.display().to_string());
    vars.insert("assets_index_name", assets_index.clone());
    vars.insert("auth_uuid", session.uuid.clone());
    vars.insert("auth_access_token", session.access_token.clone());
    vars.insert(
        "auth_session",
        format!("token:{}:{}", session.access_token, session.uuid),
    );
    vars.insert("auth_client_token", "0".to_string());
    vars.insert("client_token", "0".to_string());
    vars.insert("auth_xuid", "0".to_string());
    vars.insert("clientid", "0".to_string());
    vars.insert("user_type", session.user_type.clone());
    vars.insert("version_type", version.kind.clone());
    vars.insert("user_properties", "{}".to_string());
    vars.insert("natives_directory", natives.display().to_string());
    vars.insert("launcher_name", "NREL".to_string());
    vars.insert("launcher_version", env!("CARGO_PKG_VERSION").to_string());
    vars.insert("classpath", classpath.clone());
    vars.insert("classpath_separator", separator.to_string());
    vars.insert(
        "library_directory",
        install::libraries_dir(&root).display().to_string(),
    );
    vars.insert("resolution_width", cfg.width.to_string());
    vars.insert("resolution_height", cfg.height.to_string());

    let mut jvm_args = vec![format!("-Xms{min_memory}M"), format!("-Xmx{memory}M")];
    jvm_args.extend(
        cfg.extra_jvm_args
            .split_whitespace()
            .map(|arg| arg.to_string()),
    );

    if let Some(arguments) = &version.arguments {
        for arg in &arguments.jvm {
            jvm_args.extend(install::expand_argument(arg, &features, &vars));
        }
    }

    // 少数版本 JSON 没有提供完整的 -cp / java.library.path，这里补齐。
    if !jvm_args
        .iter()
        .any(|arg| arg.starts_with("-Djava.library.path"))
    {
        jvm_args.push(format!("-Djava.library.path={}", natives.display()));
    }
    if !jvm_args.iter().any(|arg| arg == "-cp" || arg == "-classpath") {
        jvm_args.push("-cp".to_string());
        jvm_args.push(classpath.clone());
    }

    if let Some(logging) = version.logging.as_ref().and_then(|l| l.client.as_ref()) {
        let log_path = assets_root.join("log_configs").join(&logging.file.id);
        if log_path.is_file() {
            let mut log_vars = HashMap::new();
            log_vars.insert("path", log_path.display().to_string());
            jvm_args.push(install::apply_placeholders(&logging.argument, &log_vars));
        }
    }

    // 新版版本 JSON 会把 native 工作目录指向 natives 的子目录，提前创建它们。
    let natives_root = natives.display().to_string();
    for arg in &jvm_args {
        if let Some((flag, value)) = arg.split_once('=')
            && flag.starts_with("-D")
            && value.starts_with(&natives_root)
        {
            let dir = std::path::Path::new(value);
            if !dir.exists() {
                let _ = std::fs::create_dir_all(dir);
            }
        }
    }

    let mut game_args = if let Some(arguments) = &version.arguments {
        let mut args = Vec::new();
        for arg in &arguments.game {
            args.extend(install::expand_argument(arg, &features, &vars));
        }
        args
    } else if let Some(legacy) = &version.minecraft_arguments {
        legacy
            .split_whitespace()
            .map(|arg| install::apply_placeholders(arg, &vars))
            .collect()
    } else {
        Vec::new()
    };

    if cfg.fullscreen {
        game_args.push("--fullscreen".to_string());
    }

    let mut command = Command::new(&java.path);
    command
        .args(&jvm_args)
        .arg(&version.main_class)
        .args(&game_args)
        .current_dir(&root);

    // 游戏运行在独立的控制台中，关闭启动器窗口时不会被一起结束。
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }

    Ok(command)
}
