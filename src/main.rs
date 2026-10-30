mod auth;
mod config;
mod install;
mod java;
mod launch;
mod manifest;
mod mirror;
mod net;
mod ui;

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::Color;
use reqwest::blocking::Client;
use unicode_width::UnicodeWidthStr;

use crate::config::{Config, LoginMode};
use crate::manifest::VersionManifest;
use crate::mirror::MirrorKind;
use crate::ui::Ui;

const MAIN_MENU: [&str; 6] = [
    "启动游戏",
    "选择版本",
    "账户管理",
    "启动器设置",
    "刷新版本列表",
    "退出",
];

/// Minecraft 登录令牌的保守有效期，留一小时余量。
const MC_TOKEN_LIFETIME: i64 = 24 * 60 * 60;

fn main() {
    match Ui::enter() {
        Ok(mut ui) => {
            let result = run(&mut ui);
            drop(ui);
            if let Err(err) = result {
                eprintln!("启动器异常退出：{err:#}");
            }
        }
        Err(err) => eprintln!("无法初始化终端：{err:#}"),
    }
}

fn run(ui: &mut Ui) -> Result<()> {
    let mut cfg = Config::load();
    let client = net::client()?;

    let mut manifest = match load_manifest(ui, &client, &cfg, false) {
        Ok(manifest) => Some(manifest),
        Err(err) => {
            ui.alert(
                "获取版本列表失败",
                &format!(
                    "{err:#}\n\n可以在“启动器设置”里切换下载源，再使用“刷新版本列表”重试。"
                ),
            )?;
            None
        }
    };

    let mut selected = 0usize;
    loop {
        if selected >= MAIN_MENU.len() {
            selected = 0;
        }
        let rows = overview_rows(&cfg, manifest.as_ref());
        let items: Vec<String> = MAIN_MENU.iter().map(|item| item.to_string()).collect();
        let Some(index) = menu_loop(
            ui,
            "Minecraft 启动器",
            "↑↓ 选择，Enter 确认，Esc 退出",
            &rows,
            &items,
            selected,
        )?
        else {
            break;
        };
        selected = index;

        let mirror_before = cfg.mirror;
        let result = match index {
            0 => match &manifest {
                Some(manifest) => start_game(ui, &client, &mut cfg, manifest),
                None => missing_manifest(ui),
            },
            1 => match &manifest {
                Some(manifest) => choose_version(ui, &mut cfg, manifest),
                None => missing_manifest(ui),
            },
            2 => account_menu(ui, &client, &mut cfg),
            3 => settings_menu(ui, &mut cfg),
            4 => refresh_manifest(ui, &client, &cfg).map(|fresh| manifest = Some(fresh)),
            _ => break,
        };

        match result {
            Ok(()) => {
                if cfg.mirror != mirror_before {
                    match refresh_manifest(ui, &client, &cfg) {
                        Ok(fresh) => manifest = Some(fresh),
                        Err(err) => {
                            ui.alert(
                                "切换下载源后获取版本列表失败",
                                &format!("{err:#}"),
                            )?;
                            manifest = None;
                        }
                    }
                }
            }
            Err(err) => ui.alert("操作未完成", &format!("{err:#}"))?,
        }
    }

    Ok(())
}

fn load_manifest(ui: &mut Ui, client: &Client, cfg: &Config, force: bool) -> Result<VersionManifest> {
    ui.begin_work(
        if force {
            "正在刷新版本列表"
        } else {
            "正在获取版本列表"
        },
        format!("下载源：{}", cfg.mirror.label()),
    )?;
    ui.work_line("正在连接版本服务器…")?;
    ui.flush()?;
    let (manifest, _) = manifest::fetch(client, cfg.mirror, force)?;
    Ok(manifest)
}

fn refresh_manifest(ui: &mut Ui, client: &Client, cfg: &Config) -> Result<VersionManifest> {
    load_manifest(ui, client, cfg, true)
}

fn missing_manifest(ui: &mut Ui) -> Result<()> {
    ui.alert(
        "尚未获取版本列表",
        "请先在“启动器设置”中选择可用的下载源，再使用“刷新版本列表”。",
    )
}

/// 同一个菜单循环：始终全屏重绘，Esc 返回。返回被确认的选项下标。
fn menu_loop(
    ui: &mut Ui,
    title: &str,
    subtitle: &str,
    card: &[(&str, String)],
    items: &[String],
    initial: usize,
) -> Result<Option<usize>> {
    let mut selected = initial.min(items.len().saturating_sub(1));
    loop {
        ui.header(title, subtitle)?;
        if !card.is_empty() {
            ui.card("", card)?;
            ui.blank()?;
        }
        ui.menu(items, selected)?;
        ui.footer("↑↓ 选择    Enter 确认    Esc 返回")?;
        ui.flush()?;

        match ui.key()?.code {
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(items.len().saturating_sub(1)),
            KeyCode::Enter => return Ok(Some(selected)),
            KeyCode::Esc => return Ok(None),
            _ => {}
        }
    }
}

fn overview_rows(cfg: &Config, manifest: Option<&VersionManifest>) -> Vec<(&'static str, String)> {
    let mut rows: Vec<(&'static str, String)> = vec![("账户", cfg.account_label())];
    match manifest {
        Some(manifest) => {
            let id = current_version_id(cfg, manifest);
            let installed = if install::is_version_installed(&cfg.game_dir(), &id) {
                "已安装"
            } else {
                "未安装"
            };
            rows.push(("当前版本", format!("{id}（{installed}）")));
            rows.push(("最新正式版", manifest.latest.release.clone()));
            rows.push(("最新快照", manifest.latest.snapshot.clone()));
        }
        None => rows.push(("版本列表", "未获取，请刷新".to_string())),
    }
    rows.push(("内存", memory_label(cfg)));
    rows.push(("下载源", cfg.mirror.label().to_string()));
    rows.push(("游戏目录", cfg.game_dir().display().to_string()));
    rows
}

fn current_version_id(cfg: &Config, manifest: &VersionManifest) -> String {
    cfg.last_version
        .clone()
        .filter(|id| manifest.contains(id))
        .unwrap_or_else(|| manifest.latest.release.clone())
}

fn memory_label(cfg: &Config) -> String {
    match cfg.memory_mb {
        Some(memory) => format!("{memory} MB（手动）"),
        None => format!("{} MB（自动）", cfg.effective_memory_mb()),
    }
}

fn java_label(cfg: &Config) -> String {
    match &cfg.java_path {
        Some(path) => format!("手动 · {path}"),
        None => "自动选择".to_string(),
    }
}

fn start_game(
    ui: &mut Ui,
    client: &Client,
    cfg: &mut Config,
    manifest: &VersionManifest,
) -> Result<()> {
    let id = current_version_id(cfg, manifest);
    let entry = manifest
        .find(&id)
        .cloned()
        .with_context(|| format!("版本列表中不存在 {id}"))?;

    if cfg.login_mode == LoginMode::Offline && cfg.username.trim().is_empty() {
        ui.alert(
            "还没有离线用户名",
            "请先在“账户管理”中设置用户名，或登录微软账户。",
        )?;
        return Ok(());
    }

    let root = cfg.game_dir();
    let installed = install::is_version_installed(&root, &id);
    ui.begin_work(
        "准备启动 Minecraft",
        format!(
            "版本 {id} · {}",
            if installed {
                "已安装"
            } else {
                "未安装，将自动下载"
            }
        ),
    )?;
    ui.work_line(&format!("游戏目录：{}", root.display()))?;
    ui.work_line(&format!("下载源：{}", cfg.mirror.label()))?;
    ui.blank()?;
    ui.flush()?;

    let version = install::ensure_installed(client, cfg, &entry)?;
    let required = launch::required_java_major(&version);

    let java = match java::pick(cfg.java_path.as_deref(), required) {
        Ok(java) => java,
        Err(err) => {
            ui.alert(
                "自动选择 Java 失败",
                &format!("{err:#}\n\n请手动选择一个可用的 Java 运行环境。"),
            )?;
            match choose_java(ui, cfg, required)? {
                Some(java) => java,
                None => return Ok(()),
            }
        }
    };

    if java.major < required {
        ui.alert(
            "Java 版本过低",
            &format!(
                "该版本需要 Java {required} 或更高，而所选 Java 为 {}。\n请在“启动器设置 → Java 运行环境”中更换。",
                java.major
            ),
        )?;
        return Ok(());
    }

    let Some(session) = ensure_session(ui, client, cfg)? else {
        return Ok(());
    };

    let window = if cfg.fullscreen {
        "全屏".to_string()
    } else {
        format!("{} × {}", cfg.width, cfg.height)
    };
    ui.begin()?;
    ui.header("启动 Minecraft", "构建启动参数并启动游戏")?;
    ui.card(
        "",
        &[
            ("账户", cfg.account_label()),
            ("版本", version.id.clone()),
            ("Java", java.label()),
            ("内存", format!("{} MB", cfg.effective_memory_mb())),
            ("窗口", window),
            ("游戏目录", root.display().to_string()),
        ],
    )?;
    ui.blank()?;
    ui.line("  正在启动游戏进程…")?;
    ui.flush()?;

    let mut command = launch::build_command(cfg, &java, &version, &session)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command
        .spawn()
        .with_context(|| format!("无法启动 Java：{}", java.path.display()))?;

    ui.line(&format!("  游戏已启动（PID {}），等待退出…", child.id()))?;
    ui.flush()?;
    let status = child.wait().context("等待游戏进程时出错")?;

    // 游戏日志保留在屏幕上，只追加结果。
    println!();
    if status.success() {
        ui::ok("游戏已正常退出");
    } else {
        ui::warn(format!("游戏进程结束：{status}"));
    }
    println!();
    println!("  按任意键返回主菜单…");
    ui.wait_key()
}

fn choose_java(ui: &mut Ui, cfg: &mut Config, required: u32) -> Result<Option<java::JavaInfo>> {
    let detected = java::detect_all();
    let mut items: Vec<String> = detected
        .iter()
        .map(|info| {
            let tag = if info.major >= required {
                "可用"
            } else {
                "版本过低"
            };
            format!("[{tag}] {}", info.label())
        })
        .collect();
    items.push("手动输入 java.exe 路径".to_string());

    let Some(index) = ui.pick(
        "选择 Java 运行环境",
        &format!("该版本需要 Java {required}+"),
        &items,
        0,
    )?
    else {
        return Ok(None);
    };

    if index < detected.len() {
        return Ok(Some(detected[index].clone()));
    }

    let Some(value) = ui.input("Java 路径", "java.exe 的完整路径", "", false, None)? else {
        return Ok(None);
    };
    let path = PathBuf::from(value.trim_matches('"'));
    match java::probe(&path) {
        Some(info) => {
            cfg.java_path = Some(path.display().to_string());
            cfg.save()?;
            Ok(Some(info))
        }
        None => {
            ui.alert(
                "无法识别 Java",
                &format!("无法从该路径读取 Java 版本：\n{}", path.display()),
            )?;
            Ok(None)
        }
    }
}

fn choose_version(ui: &mut Ui, cfg: &mut Config, manifest: &VersionManifest) -> Result<()> {
    let root = cfg.game_dir();
    let current = current_version_id(cfg, manifest);
    let mut filter = String::new();
    let mut start = 0usize;
    let mut selected = manifest
        .versions
        .iter()
        .position(|entry| entry.id == current)
        .unwrap_or(0);

    loop {
        let needle = filter.to_lowercase();
        let filtered: Vec<usize> = manifest
            .versions
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                needle.is_empty()
                    || entry.id.to_lowercase().contains(&needle)
                    || entry.kind_label().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect();

        if filtered.is_empty() {
            selected = 0;
        } else if selected >= filtered.len() {
            selected = filtered.len() - 1;
        }

        let items: Vec<String> = filtered
            .iter()
            .map(|&index| {
                let entry = &manifest.versions[index];
                let mut text = format!(
                    "{:<10}  {}  {}",
                    entry.id,
                    entry.kind_label(),
                    entry.date()
                );
                if install::is_version_installed(&root, &entry.id) {
                    text.push_str("  [已安装]");
                }
                if entry.id == current {
                    text.push_str("  ← 当前");
                }
                text
            })
            .collect();

        ui.header(
            "选择版本",
            "输入关键字筛选，↑↓ 选择，Enter 确认，Esc 返回",
        )?;
        ui.line(&format!("  筛选：{filter}▏"))?;
        ui.blank()?;
        let max_rows = (ui.height().saturating_sub(ui.row() + 2) as usize).max(1);
        if selected >= start + max_rows {
            start = selected + 1 - max_rows;
        }
        if selected < start {
            start = selected;
        }
        ui.list(&items, selected, start, max_rows)?;
        ui.dim_line(&format!("  共 {} 个版本", items.len()))?;
        ui.footer("输入筛选    ↑↓ 选择    Enter 确认    Esc 返回")?;
        ui.flush()?;

        let key = ui.key()?;
        match key.code {
            KeyCode::Esc => return Ok(()),
            KeyCode::Enter => {
                if let Some(&index) = filtered.get(selected) {
                    let id = manifest.versions[index].id.clone();
                    cfg.last_version = Some(id);
                    cfg.save()?;
                    return Ok(());
                }
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Down => {
                if !filtered.is_empty() {
                    selected = (selected + 1).min(filtered.len() - 1);
                }
            }
            KeyCode::PageUp => selected = selected.saturating_sub(10),
            KeyCode::PageDown => {
                if !filtered.is_empty() {
                    selected = (selected + 10).min(filtered.len() - 1);
                }
            }
            KeyCode::Backspace => {
                filter.pop();
                selected = 0;
                start = 0;
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                filter.push(c);
                selected = 0;
                start = 0;
            }
            _ => {}
        }
    }
}

fn account_menu(ui: &mut Ui, client: &Client, cfg: &mut Config) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let rows = vec![
            ("登录方式", cfg.login_mode.label().to_string()),
            (
                "当前账户",
                match cfg.login_mode {
                    LoginMode::Offline => cfg.username.clone(),
                    LoginMode::Microsoft => cfg
                        .ms_name
                        .clone()
                        .unwrap_or_else(|| "未登录".to_string()),
                },
            ),
        ];
        let items = vec![
            "离线登录（自定义用户名）".to_string(),
            "微软登录（浏览器设备码）".to_string(),
            "退出微软账户".to_string(),
            "微软客户端 ID（高级）".to_string(),
        ];

        let Some(index) = menu_loop(
            ui,
            "账户管理",
            "↑↓ 选择   Enter 确认   Esc 返回",
            &rows,
            &items,
            selected,
        )?
        else {
            return Ok(());
        };
        selected = index;

        match index {
            0 => {
                let Some(name) = ui.input(
                    "离线登录",
                    "用户名（1 - 16 个字符）",
                    &cfg.username,
                    false,
                    Some(&validate_username),
                )?
                else {
                    continue;
                };
                cfg.username = name;
                cfg.login_mode = LoginMode::Offline;
                cfg.save()?;
            }
            1 => {
                let _ = microsoft_login(ui, client, cfg)?;
            }
            2 => {
                if cfg.ms_name.is_none() && cfg.ms_refresh_token.is_none() {
                    ui.alert("没有微软账户", "当前还没有登录微软账户。")?;
                } else {
                    cfg.ms_name = None;
                    cfg.ms_uuid = None;
                    cfg.ms_access_token = None;
                    cfg.ms_refresh_token = None;
                    cfg.ms_expires_at = 0;
                    if cfg.login_mode == LoginMode::Microsoft {
                        cfg.login_mode = LoginMode::Offline;
                    }
                    cfg.save()?;
                }
            }
            3 => {
                let current = cfg.ms_client_id();
                let Some(value) = ui.input(
                    "微软客户端 ID",
                    "留空则使用内置的公开客户端 ID",
                    &current,
                    true,
                    None,
                )?
                else {
                    continue;
                };
                cfg.ms_client_id = if value.is_empty() { None } else { Some(value) };
                cfg.save()?;
            }
            _ => {}
        }
    }
}

fn settings_menu(ui: &mut Ui, cfg: &mut Config) -> Result<()> {
    let mut selected = 0usize;
    loop {
        let items = vec![
            format!("{}  {}", pad_label("内存分配", 13), memory_label(cfg)),
            format!("{}  {}", pad_label("下载源", 13), cfg.mirror.label()),
            format!("{}  {}", pad_label("Java 运行环境", 13), java_label(cfg)),
            format!(
                "{}  {}",
                pad_label("游戏目录", 13),
                cfg.game_dir().display()
            ),
            format!(
                "{}  {} × {}",
                pad_label("窗口分辨率", 13),
                cfg.width,
                cfg.height
            ),
            format!(
                "{}  {}",
                pad_label("全屏模式", 13),
                if cfg.fullscreen { "开启" } else { "关闭" }
            ),
            format!(
                "{}  {}",
                pad_label("额外 JVM 参数", 13),
                if cfg.extra_jvm_args.trim().is_empty() {
                    "无"
                } else {
                    cfg.extra_jvm_args.trim()
                }
            ),
        ];

        let Some(index) = menu_loop(
            ui,
            "启动器设置",
            "↑↓ 选择   Enter 修改   Esc 返回",
            &[],
            &items,
            selected,
        )?
        else {
            return Ok(());
        };
        selected = index;

        match index {
            0 => {
                let choices = vec![
                    "自动分配（推荐）".to_string(),
                    "手动分配".to_string(),
                ];
                let default = if cfg.memory_mb.is_some() { 1 } else { 0 };
                let Some(choice) = ui.pick("内存分配", "选择内存分配方式", &choices, default)?
                else {
                    continue;
                };
                if choice == 0 {
                    cfg.memory_mb = None;
                } else {
                    let Some(value) = ui.input(
                        "手动内存",
                        "单位 MB（512 - 65536）",
                        &cfg.effective_memory_mb().to_string(),
                        false,
                        Some(&validate_memory),
                    )?
                    else {
                        continue;
                    };
                    cfg.memory_mb = value.parse().ok();
                }
                cfg.save()?;
            }
            1 => {
                let kinds = [MirrorKind::Bmclapi, MirrorKind::Official];
                let choices: Vec<String> =
                    kinds.iter().map(|kind| kind.label().to_string()).collect();
                let default = kinds
                    .iter()
                    .position(|kind| *kind == cfg.mirror)
                    .unwrap_or(0);
                let Some(choice) = ui.pick("下载源", "选择 Minecraft 文件下载源", &choices, default)?
                else {
                    continue;
                };
                cfg.mirror = kinds[choice];
                cfg.save()?;
            }
            2 => java_settings(ui, cfg)?,
            3 => {
                let current = cfg.game_dir().display().to_string();
                let Some(value) = ui.input(
                    "游戏目录",
                    "留空恢复默认（当前目录下的 .minecraft）",
                    &current,
                    true,
                    None,
                )?
                else {
                    continue;
                };
                if value.is_empty() {
                    cfg.game_dir = None;
                } else {
                    let path = PathBuf::from(value.trim_matches('"'));
                    std::fs::create_dir_all(&path)
                        .with_context(|| format!("无法创建目录 {}", path.display()))?;
                    cfg.game_dir = Some(path.display().to_string());
                }
                cfg.save()?;
            }
            4 => {
                let Some(value) = ui.input(
                    "窗口分辨率",
                    "格式：854x480",
                    &format!("{}x{}", cfg.width, cfg.height),
                    false,
                    Some(&validate_resolution),
                )?
                else {
                    continue;
                };
                if let Some((width, height)) = parse_resolution(&value) {
                    cfg.width = width;
                    cfg.height = height;
                    cfg.save()?;
                }
            }
            5 => {
                let choices = vec!["关闭".to_string(), "开启".to_string()];
                let default = if cfg.fullscreen { 1 } else { 0 };
                let Some(choice) = ui.pick("全屏模式", "启动游戏时是否全屏", &choices, default)?
                else {
                    continue;
                };
                cfg.fullscreen = choice == 1;
                cfg.save()?;
            }
            6 => {
                let Some(value) = ui.input(
                    "额外 JVM 参数",
                    "例如 -XX:+UseG1GC，留空表示不使用",
                    &cfg.extra_jvm_args,
                    true,
                    None,
                )?
                else {
                    continue;
                };
                cfg.extra_jvm_args = value;
                cfg.save()?;
            }
            _ => {}
        }
    }
}

fn java_settings(ui: &mut Ui, cfg: &mut Config) -> Result<()> {
    let detected = java::detect_all();
    let mut items: Vec<String> = vec!["自动选择（推荐）".to_string()];
    items.extend(detected.iter().map(java::JavaInfo::label));
    items.push("手动输入 java.exe 路径".to_string());

    let default = match &cfg.java_path {
        Some(path) => detected
            .iter()
            .position(|info| info.path.display().to_string() == *path)
            .map(|index| index + 1)
            .unwrap_or(0),
        None => 0,
    };

    let Some(index) = ui.pick(
        "Java 运行环境",
        "自动检测 JAVA_HOME、PATH 与常见安装目录",
        &items,
        default,
    )?
    else {
        return Ok(());
    };

    if index == 0 {
        cfg.java_path = None;
        cfg.save()?;
    } else if index <= detected.len() {
        cfg.java_path = Some(detected[index - 1].path.display().to_string());
        cfg.save()?;
    } else {
        let Some(value) = ui.input("Java 路径", "java.exe 的完整路径", "", false, None)? else {
            return Ok(());
        };
        let path = PathBuf::from(value.trim_matches('"'));
        if let Some(_info) = java::probe(&path) {
            cfg.java_path = Some(path.display().to_string());
            cfg.save()?;
        } else {
            ui.alert(
                "无法识别 Java",
                &format!("无法从该路径读取 Java 版本：\n{}", path.display()),
            )?;
        }
    }
    Ok(())
}

fn ensure_session(ui: &mut Ui, client: &Client, cfg: &mut Config) -> Result<Option<auth::Session>> {
    match cfg.login_mode {
        LoginMode::Offline => {
            let name = cfg.username.trim().to_string();
            if name.is_empty() {
                ui.alert(
                    "还没有离线用户名",
                    "请先在“账户管理”中设置用户名，或登录微软账户。",
                )?;
                return Ok(None);
            }
            Ok(Some(auth::Session::offline(&name)))
        }
        LoginMode::Microsoft => {
            if let (Some(token), Some(name), Some(uuid)) = (
                cfg.ms_access_token.clone(),
                cfg.ms_name.clone(),
                cfg.ms_uuid.clone(),
            ) && cfg.ms_expires_at > now_secs()
                && !token.is_empty()
            {
                return Ok(Some(auth::Session {
                    username: name,
                    uuid,
                    access_token: token,
                    user_type: "msa".to_string(),
                }));
            }

            if let Some(refresh_token) = cfg.ms_refresh_token.clone() {
                ui.begin_work("刷新微软登录", "正在刷新登录令牌…")?;
                ui.flush()?;
                let refreshed = match auth::refresh(client, &cfg.ms_client_id(), &refresh_token) {
                    Ok(tokens) => auth::login_with_ms(client, &tokens.access_token)
                        .map(|session| (tokens, session)),
                    Err(err) => Err(err),
                };
                match refreshed {
                    Ok((tokens, session)) => {
                        store_ms_login(cfg, &tokens, &session);
                        cfg.save()?;
                        return Ok(Some(session));
                    }
                    Err(err) => {
                        ui.alert(
                            "微软登录需要重新验证",
                            &format!("{err:#}\n\n接下来会重新走一次设备码登录。"),
                        )?;
                    }
                }
            }

            microsoft_login(ui, client, cfg)
        }
    }
}

fn microsoft_login(ui: &mut Ui, client: &Client, cfg: &mut Config) -> Result<Option<auth::Session>> {
    let client_id = cfg.ms_client_id();

    ui.begin_work("微软登录", "正在申请设备代码…")?;
    ui.flush()?;
    let code = match auth::request_device_code(client, &client_id) {
        Ok(code) => code,
        Err(err) => {
            ui.alert("无法开始微软登录", &format!("{err:#}"))?;
            return Ok(None);
        }
    };

    auth::open_browser(&code.verification_uri);

    let mut interval = code.interval.max(1);
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
        draw_device_code(ui, &code, remaining)?;
        if wait_cancel(ui, interval)? {
            return Ok(None);
        }

        match auth::poll_device_code(client, &client_id, &code.device_code) {
            auth::PollResult::Pending => {}
            auth::PollResult::SlowDown => interval += 5,
            auth::PollResult::Expired => {
                ui.alert("设备代码已过期", "授权等待超时，请重新登录。")?;
                return Ok(None);
            }
            auth::PollResult::Declined => {
                ui.alert("已取消授权", "你取消了微软账户授权。")?;
                return Ok(None);
            }
            auth::PollResult::Error(err) => {
                ui.alert("微软登录失败", &err)?;
                return Ok(None);
            }
            auth::PollResult::Token(tokens) => {
                ui.begin_work("微软登录", "正在验证 Xbox 与 Minecraft 账户…")?;
                ui.work_line("微软授权成功，正在获取游戏档案…")?;
                ui.flush()?;
                match auth::login_with_ms(client, &tokens.access_token) {
                    Ok(session) => {
                        store_ms_login(cfg, &tokens, &session);
                        cfg.save()?;
                        return Ok(Some(session));
                    }
                    Err(err) => {
                        ui.alert("微软账户无法启动 Minecraft", &format!("{err:#}"))?;
                        return Ok(None);
                    }
                }
            }
        }
    }
}

fn draw_device_code(ui: &mut Ui, code: &auth::DeviceCode, remaining: u64) -> Result<()> {
    ui.header("微软登录", "在浏览器中完成授权，然后回到这里")?;
    ui.blank()?;
    ui.line("  1. 浏览器已自动打开授权页面；如果没有打开，请手动访问：")?;
    ui.line(&format!("     {}", code.verification_uri))?;
    ui.blank()?;
    ui.line("  2. 在页面中输入下面的代码：")?;
    ui.blank()?;
    ui.centered(&code.user_code, Color::Cyan, true)?;
    ui.blank()?;
    ui.dim_line(&format!("  等待授权完成… 剩余 {remaining} 秒"))?;
    ui.blank()?;
    ui.dim_line("  首次登录的账户可能需要先在 Xbox 官网创建档案，否则会提示没有游戏档案")?;
    ui.footer("Esc 取消登录")?;
    ui.flush()
}

fn wait_cancel(ui: &mut Ui, seconds: u64) -> Result<bool> {
    let end = Instant::now() + Duration::from_secs(seconds);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        if ui.poll_cancel(left.min(Duration::from_millis(100)))? {
            return Ok(true);
        }
    }
}

fn store_ms_login(cfg: &mut Config, tokens: &auth::MsTokens, session: &auth::Session) {
    cfg.login_mode = LoginMode::Microsoft;
    if let Some(refresh) = &tokens.refresh_token {
        cfg.ms_refresh_token = Some(refresh.clone());
    }
    cfg.ms_access_token = Some(session.access_token.clone());
    cfg.ms_expires_at = now_secs() + MC_TOKEN_LIFETIME - 3600;
    cfg.ms_name = Some(session.username.clone());
    cfg.ms_uuid = Some(session.uuid.clone());
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn parse_resolution(input: &str) -> Option<(u32, u32)> {
    let (width, height) = input.trim().split_once(['x', 'X', '*'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

fn validate_resolution(text: &str) -> std::result::Result<(), String> {
    match parse_resolution(text) {
        Some((width, height))
            if (320..=7680).contains(&width) && (240..=4320).contains(&height) =>
        {
            Ok(())
        }
        _ => Err("请输入类似 854x480 的格式（宽 320-7680，高 240-4320）".to_string()),
    }
}

fn validate_memory(text: &str) -> std::result::Result<(), String> {
    match text.parse::<u64>() {
        Ok(value) if (512..=65536).contains(&value) => Ok(()),
        _ => Err("请输入 512 到 65536 之间的数值".to_string()),
    }
}

fn validate_username(text: &str) -> std::result::Result<(), String> {
    match text.chars().count() {
        0 => Err("用户名不能为空".to_string()),
        count if count > 16 => Err("用户名最多 16 个字符".to_string()),
        _ => Ok(()),
    }
}

fn pad_label(label: &str, width: usize) -> String {
    let label_width = UnicodeWidthStr::width(label);
    format!("{label}{}", " ".repeat(width.saturating_sub(label_width)))
}
