mod archive;
mod auth;
mod config;
mod hosts;
mod install;
mod java;
mod launch;
mod logs;
mod manifest;
mod mirror;
mod net;
mod trash;
mod ui;
mod update;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use crossterm::event::{KeyCode, KeyModifiers};
use crossterm::style::Color;
use unicode_width::UnicodeWidthStr;

use crate::config::{Config, LogLevel, LoginMode, UpdateMode};
use crate::install::VersionJson;
use crate::manifest::{VersionEntry, VersionManifest};
use crate::mirror::MirrorKind;
use crate::net::HttpClient;
use crate::ui::{Ui, UiEvent};

const MAIN_MENU: [&str; 5] = [
    "启动游戏",
    "选择版本",
    "账户管理",
    "启动器设置",
    "退出",
];

/// 版本选择列表中的一行（官方清单版本 + 本地已有版本）。
struct VersionRow {
    id: String,
    kind: String,
    date: String,
    installed: bool,
    local_only: bool,
    /// 已安装时对应的版本文件夹名。
    folder: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum VersionScope {
    All,
    Installed,
    Local,
}

impl VersionScope {
    fn next(self) -> Self {
        match self {
            Self::All => Self::Installed,
            Self::Installed => Self::Local,
            Self::Local => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "全部",
            Self::Installed => "已安装",
            Self::Local => "本地",
        }
    }

    fn matches(self, row: &VersionRow) -> bool {
        match self {
            Self::All => true,
            Self::Installed => row.installed,
            Self::Local => row.local_only,
        }
    }
}

/// Minecraft 登录令牌的保守有效期，留一小时余量。
const MC_TOKEN_LIFETIME: i64 = 24 * 60 * 60;

fn main() {
    // 清理上次更新留下的旧版本文件。
    if let Ok(current) = std::env::current_exe()
        && let Some(dir) = current.parent()
    {
        let _ = std::fs::remove_file(dir.join("nrel-old.exe"));
    }
    hosts::install_console_handler();
    match Ui::enter() {
        Ok(mut ui) => {
            let result = run(&mut ui);
            drop(ui);
            hosts::restore();
            if let Err(err) = result {
                eprintln!("启动器异常退出：{err:#}");
            }
            if RELAUNCH.load(Ordering::SeqCst) {
                update::relaunch();
            }
        }
        Err(err) => eprintln!("无法初始化终端：{err:#}"),
    }
}

fn run(ui: &mut Ui) -> Result<()> {
    let mut cfg = Config::load();
    let client = net::client()?;

    // 每次启动创建独立的日志文件，放在 .nrel-cache 中。
    if let Err(err) = logs::init(cfg.log_level) {
        logs::error(format!("日志初始化失败：{err:#}"));
    }
    logs::info(format!("工作目录：{}", config::work_dir().display()));
    logs::info(format!(
        "账户：{} | 下载源：{} | 游戏目录：{} | 版本隔离：{}",
        cfg.account_label(),
        cfg.mirror.label(),
        cfg.game_dir().display(),
        cfg.isolation_label()
    ));

    if cfg.github_accel {
        match hosts::apply(&client) {
            Ok(()) => {}
            Err(err) => logs::info(format!("GitHub 加速未生效：{err:#}")),
        }
    } else {
        // 关闭加速时顺带清理上次可能残留的条目。
        hosts::restore();
    }

    if update::should_check(cfg.update_mode) {
        check_update(ui, &client, &cfg)?;
        if RELAUNCH.load(Ordering::SeqCst) {
            return Ok(());
        }
    }

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

    init_threads();

    let mut selected = 0usize;
    loop {
        if QUIT_REQUESTED.load(Ordering::SeqCst) {
            break;
        }
        if selected >= MAIN_MENU.len() {
            selected = 0;
        }
        let rows = overview_rows(&cfg, manifest.as_ref());
        let mut items: Vec<String> = MAIN_MENU.iter().map(|item| item.to_string()).collect();
        if let Some(manifest) = manifest.as_ref() {
            items[0] = launch_menu_label(&cfg, manifest);
        }
        let mirror_before = cfg.mirror;
        let pending = PENDING_ACTION.lock().unwrap().take();
        // 线程切回时恢复到它原先停留的界面。
        let action = match pending.or_else(|| active_screen().action()) {
            Some(action) => action,
            None => {
                let env = CmdEnv { cfg: &cfg };
                match main_menu(ui, &rows, &items, &mut selected, &env)? {
                    Some(action) => action,
                    // 切换线程等操作需要重新按当前线程的界面状态路由。
                    None => continue,
                }
            }
        };
        logs::info(format!("执行操作：{}", action.label()));
        let result = match action {
            MainAction::Launch => match &manifest {
                Some(manifest) => start_game(ui, &client, &mut cfg, manifest),
                None => missing_manifest(ui),
            },
            MainAction::Versions => match manifest.as_mut() {
                Some(manifest) => {
                    let owner = ACTIVE_THREAD.load(Ordering::SeqCst);
                    set_screen(ThreadScreen::Versions);
                    let result = match choose_version(ui, &client, &mut cfg, manifest)? {
                        Some(id) => {
                            cfg.last_version = Some(id);
                            cfg.save()?;
                            start_game(ui, &client, &mut cfg, &*manifest)
                        }
                        None => Ok(()),
                    };
                    // 正常退出该界面才回到主界面；若是切换线程离开，保留原界面。
                    if ACTIVE_THREAD.load(Ordering::SeqCst) == owner {
                        set_screen_at(owner, ThreadScreen::Main);
                    }
                    result
                }
                None => missing_manifest(ui),
            },
            MainAction::Account => {
                let owner = ACTIVE_THREAD.load(Ordering::SeqCst);
                set_screen(ThreadScreen::Account);
                let result = account_menu(ui, &client, &mut cfg);
                if ACTIVE_THREAD.load(Ordering::SeqCst) == owner {
                    set_screen_at(owner, ThreadScreen::Main);
                }
                result
            }
            MainAction::Settings => {
                let owner = ACTIVE_THREAD.load(Ordering::SeqCst);
                set_screen(ThreadScreen::Settings);
                let result = settings_menu(ui, &mut cfg);
                if ACTIVE_THREAD.load(Ordering::SeqCst) == owner {
                    set_screen_at(owner, ThreadScreen::Main);
                }
                result
            }
            MainAction::Refresh => refresh_manifest(ui, &client, &cfg).map(|fresh| manifest = Some(fresh)),
            MainAction::Exit => break,
        };

        if QUIT_REQUESTED.load(Ordering::SeqCst) {
            break;
        }
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
            Err(err) => {
                logs::error(format!("{err:#}"));
                ui.alert("操作未完成", &format!("{err:#}"))?
            }
        }
    }

    Ok(())
}

/// 主菜单可触发的动作。
#[derive(Clone, Copy)]
enum MainAction {
    Launch,
    Versions,
    Account,
    Settings,
    Refresh,
    Exit,
}

impl MainAction {
    fn label(self) -> &'static str {
        match self {
            Self::Launch => "启动游戏",
            Self::Versions => "选择版本",
            Self::Account => "账户管理",
            Self::Settings => "启动器设置",
            Self::Refresh => "刷新版本列表",
            Self::Exit => "退出",
        }
    }
}

/// 一条命令执行后的结果。
enum CommandResult {
    /// 已处理，继续停留在当前界面。
    Handled,
    /// 请返回主界面。
    Main,
    /// 退出启动器。
    Quit,
    /// 直接执行某个主菜单动作。
    Action(MainAction),
}

/// 供命令处理使用的应用上下文。
struct CmdEnv<'a> {
    cfg: &'a Config,
}

static QUIT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 更新完成后需要重启启动器。
static RELAUNCH: AtomicBool = AtomicBool::new(false);
static PENDING_ACTION: Mutex<Option<MainAction>> = Mutex::new(None);

/// 线程当前停留的界面，切换线程时据此恢复。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadScreen {
    Main,
    Versions,
    Account,
    Settings,
}

impl ThreadScreen {
    fn label(self) -> &'static str {
        match self {
            Self::Main => "主界面",
            Self::Versions => "版本选择",
            Self::Account => "账户管理",
            Self::Settings => "启动器设置",
        }
    }

    fn action(self) -> Option<MainAction> {
        match self {
            Self::Main => None,
            Self::Versions => Some(MainAction::Versions),
            Self::Account => Some(MainAction::Account),
            Self::Settings => Some(MainAction::Settings),
        }
    }
}

/// 一个启动器线程：拥有独立的后台任务状态与界面位置。
struct LauncherThread {
    status: Arc<Mutex<String>>,
    screen: ThreadScreen,
    /// 保留后台任务的句柄，方便查看运行状态。
    #[allow(dead_code)]
    handle: Option<JoinHandle<()>>,
}

static THREADS: Mutex<Vec<LauncherThread>> = Mutex::new(Vec::new());
static ACTIVE_THREAD: AtomicUsize = AtomicUsize::new(0);

fn init_threads() {
    let mut list = THREADS.lock().unwrap();
    if list.is_empty() {
        list.push(new_thread());
    }
    ACTIVE_THREAD.store(0, Ordering::SeqCst);
}

fn new_thread() -> LauncherThread {
    LauncherThread {
        status: Arc::new(Mutex::new("空闲".to_string())),
        screen: ThreadScreen::Main,
        handle: None,
    }
}

fn set_screen(screen: ThreadScreen) {
    let active = ACTIVE_THREAD.load(Ordering::SeqCst);
    if let Some(thread) = THREADS.lock().unwrap().get_mut(active) {
        thread.screen = screen;
    }
}

fn set_screen_at(index: usize, screen: ThreadScreen) {
    if let Some(thread) = THREADS.lock().unwrap().get_mut(index) {
        thread.screen = screen;
    }
}

fn active_screen() -> ThreadScreen {
    let active = ACTIVE_THREAD.load(Ordering::SeqCst);
    THREADS
        .lock()
        .unwrap()
        .get(active)
        .map(|thread| thread.screen)
        .unwrap_or(ThreadScreen::Main)
}

fn set_thread_status(status: String) {
    let active = ACTIVE_THREAD.load(Ordering::SeqCst);
    if let Some(thread) = THREADS.lock().unwrap().get(active) {
        *thread.status.lock().unwrap() = status;
    }
}

fn thread_status_arc() -> Option<Arc<Mutex<String>>> {
    let active = ACTIVE_THREAD.load(Ordering::SeqCst);
    THREADS
        .lock()
        .unwrap()
        .get(active)
        .map(|thread| Arc::clone(&thread.status))
}

fn thread_status(index: usize) -> String {
    THREADS
        .lock()
        .unwrap()
        .get(index)
        .map(|thread| thread.status.lock().unwrap().clone())
        .unwrap_or_else(|| "空闲".to_string())
}

fn thread_label() -> String {
    let index = ACTIVE_THREAD.load(Ordering::SeqCst);
    format!(
        "线程 {}（{}·{}）",
        index + 1,
        thread_status(index),
        active_screen().label()
    )
}

fn threads_report() -> String {
    let list = THREADS.lock().unwrap();
    let active = ACTIVE_THREAD.load(Ordering::SeqCst);
    let mut text = String::new();
    for (index, thread) in list.iter().enumerate() {
        let marker = if index == active { "▶ " } else { "  " };
        let status = thread.status.lock().unwrap().clone();
        text.push_str(&format!(
            "{marker}线程 {}    {status}    界面：{}\n",
            index + 1,
            thread.screen.label()
        ));
    }
    text.push_str("\n/thread new 新建线程（/t new）    /thread N 切换线程（/t N）    /thread del N 删除线程（/t del N）");
    text
}

fn create_thread() -> usize {
    let mut list = THREADS.lock().unwrap();
    list.push(new_thread());
    let index = list.len() - 1;
    ACTIVE_THREAD.store(index, Ordering::SeqCst);
    index
}

fn switch_thread(index: usize) -> Result<usize> {
    let list = THREADS.lock().unwrap();
    if index >= list.len() {
        bail!("线程 {} 不存在", index + 1);
    }
    drop(list);
    ACTIVE_THREAD.store(index, Ordering::SeqCst);
    Ok(index)
}

/// 删除一个空闲线程，并自动修正当前线程下标。
fn delete_thread(index: usize) -> Result<usize> {
    let mut list = THREADS.lock().unwrap();
    if list.len() <= 1 {
        bail!("至少需要保留一个线程");
    }
    if index >= list.len() {
        bail!("线程 {} 不存在", index + 1);
    }
    let status = list[index].status.lock().unwrap().clone();
    if status.starts_with("启动中") || status.starts_with("运行中") {
        bail!("线程 {} 正在运行游戏，无法删除", index + 1);
    }
    let len = list.len();
    list.remove(index);
    let mut active = ACTIVE_THREAD.load(Ordering::SeqCst);
    if active > index {
        active -= 1;
    }
    active = active.min(len - 2);
    ACTIVE_THREAD.store(active, Ordering::SeqCst);
    Ok(active)
}

const MAIN_HINT: &str = "↑↓/滚轮 选择    Enter/点击 确认    / 命令";

fn main_menu(
    ui: &mut Ui,
    card: &[(&str, String)],
    items: &[String],
    selected: &mut usize,
    env: &CmdEnv,
) -> Result<Option<MainAction>> {
    loop {
        if *selected >= items.len() {
            *selected = 0;
        }
        ui.header("Minecraft 启动器", MAIN_HINT)?;
        ui.card("", card)?;
        ui.blank()?;
        ui.menu(items, *selected)?;
        ui.footer(MAIN_HINT, false)?;
        ui.flush()?;

        let event = ui.wait()?;
        if let Some(result) = maybe_command(ui, &event, env, true)? {
            match result {
                CommandResult::Action(action) => return Ok(Some(action)),
                CommandResult::Quit => return Ok(Some(MainAction::Exit)),
                // 线程切换或 /main：交回主循环按新线程的界面状态重新路由。
                CommandResult::Main | CommandResult::Handled => return Ok(None),
            }
        }
        match event {
            UiEvent::Key(key) => match key.code {
                KeyCode::Up => *selected = ui::cycle_step(*selected, items.len(), -1),
                KeyCode::Down => *selected = ui::cycle_step(*selected, items.len(), 1),
                KeyCode::Enter => return Ok(Some(action_at(*selected))),
                _ => {}
            },
            UiEvent::Click(index) => return Ok(Some(action_at(index))),
            UiEvent::ScrollUp => *selected = ui::cycle_step(*selected, items.len(), -1),
            UiEvent::ScrollDown => *selected = ui::cycle_step(*selected, items.len(), 1),
            UiEvent::Back | UiEvent::Redraw => {}
        }
    }
}

fn action_at(index: usize) -> MainAction {
    match index {
        0 => MainAction::Launch,
        1 => MainAction::Versions,
        2 => MainAction::Account,
        3 => MainAction::Settings,
        _ => MainAction::Exit,
    }
}

/// vim 风格的底部命令模式：按 `/` 唤起，删到连 `/` 都没有时自动退出。
fn command_mode(ui: &mut Ui) -> Result<Option<String>> {
    let mut buffer = String::from("/");
    let mut blink = true;

    loop {
        let caret = if blink { "▏" } else { " " };
        ui.prompt_line(&format!("  {buffer}{caret}"))?;
        ui.status_line("  命令模式    Enter 执行    Esc 取消    /help 查看命令", false)?;
        ui.flush()?;

        let Some(event) = ui.poll_event(ui::BLINK_INTERVAL)? else {
            blink = !blink;
            continue;
        };
        blink = true;
        let UiEvent::Key(key) = event else {
            continue;
        };
        match key.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Enter => {
                let command = buffer.trim().to_string();
                if command == "/" {
                    return Ok(None);
                }
                return Ok(Some(command));
            }
            KeyCode::Backspace => {
                buffer.pop();
                if buffer.is_empty() {
                    return Ok(None);
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.push(c);
            }
            _ => {}
        }
    }
}

/// 任意界面按 `/` 即进入命令模式；返回 `None` 表示该事件不是命令。
fn maybe_command(
    ui: &mut Ui,
    event: &UiEvent,
    env: &CmdEnv,
    in_main: bool,
) -> Result<Option<CommandResult>> {
    let UiEvent::Key(key) = event else {
        return Ok(None);
    };
    if key.code != KeyCode::Char('/')
        || key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
    {
        return Ok(None);
    }

    let result = match command_mode(ui)? {
        Some(command) => {
            logs::detail(|| format!("执行命令：{command}"));
            dispatch_command(ui, env, &command, in_main)?
        }
        None => return Ok(None),
    };
    ui.clear_bottom()?;
    ui.flush()?;
    Ok(Some(result))
}

fn dispatch_command(
    ui: &mut Ui,
    env: &CmdEnv,
    command: &str,
    in_main: bool,
) -> Result<CommandResult> {
    let raw = command.trim();
    let normalized = raw
        .strip_prefix("/t ")
        .or_else(|| raw.strip_prefix("/th "))
        .map(|rest| format!("/thread {}", rest.trim()));
    let cmd = normalized.as_deref().unwrap_or(raw);
    match cmd {
        "/quit" | "/q" | "/exit" => {
            QUIT_REQUESTED.store(true, Ordering::SeqCst);
            return Ok(CommandResult::Quit);
        }
        "/main" | "/home" => {
            return Ok(if in_main {
                CommandResult::Handled
            } else {
                CommandResult::Main
            });
        }
        _ => {}
    }

    let action = match cmd {
        "/launch" | "/l" | "/play" => Some(MainAction::Launch),
        "/versions" | "/v" | "/select" => Some(MainAction::Versions),
        "/account" | "/a" => Some(MainAction::Account),
        "/settings" | "/s" => Some(MainAction::Settings),
        "/refresh" | "/r" => Some(MainAction::Refresh),
        _ => None,
    };
    if let Some(action) = action {
        if in_main {
            return Ok(CommandResult::Action(action));
        }
        *PENDING_ACTION.lock().unwrap() = Some(action);
        return Ok(CommandResult::Main);
    }

    match cmd {
        "/help" | "/h" | "/?" => {
            ui.notice(
                "命令帮助",
                "/help       显示本帮助\n\
                 /quit       退出启动器（别名 /q、/exit）\n\
                 /main       返回主界面\n\
                 /launch     启动当前版本（别名 /l）\n\
                 /versions   打开版本选择（别名 /v）\n\
                 /account    打开账户管理（别名 /a）\n\
                 /settings   打开启动器设置（别名 /s）\n\
                 /refresh    刷新版本列表（别名 /r）\n\
                 /thread     查看所有启动器线程（别名 /t）\n\
                 /thread new 新建线程并切换过去（/t new）\n\
                 /thread N   切换到第 N 个线程（/t N）\n\
                 /thread del N 删除空闲线程（/t del N）\n\
                 /version    查看启动器版本与目录\n\
                 /dir        查看游戏目录",
            )?;
        }
        "/version" => {
            ui.notice(
                "版本信息",
                &format!(
                    "NREL 启动器 v{}\n游戏目录：{}\n{}",
                    env!("CARGO_PKG_VERSION"),
                    env.cfg.game_dir().display(),
                    thread_label()
                ),
            )?;
        }
        "/dir" => {
            ui.notice("游戏目录", &env.cfg.game_dir().display().to_string())?;
        }
        "/thread" | "/threads" | "/t" | "/th" => {
            ui.notice("启动器线程", &threads_report())?;
        }
        "/thread new" => {
            let index = create_thread();
            logs::info(format!("新建启动器线程 {}", index + 1));
            ui.notice(
                "新建线程",
                &format!("已创建并切换到线程 {}。\n\n{}", index + 1, threads_report()),
            )?;
            return Ok(CommandResult::Main);
        }
        _ if cmd.starts_with("/thread del") => {
            let value = cmd.trim_start_matches("/thread del").trim();
            match value.parse::<usize>() {
                Ok(number) if number >= 1 => match delete_thread(number - 1) {
                    Ok(_) => {
                        ui.notice(
                            "删除线程",
                            &format!("已删除线程 {number}。\n\n{}", threads_report()),
                        )?;
                        return Ok(CommandResult::Main);
                    }
                    Err(err) => {
                        ui.alert("无法删除线程", &format!("{err:#}"))?;
                    }
                },
                _ => {
                    ui.notice(
                        "命令用法",
                        "/thread del N 删除第 N 个空闲线程（/t del N）",
                    )?;
                }
            }
        }
        _ if cmd.starts_with("/thread ") => {
            let value = cmd.trim_start_matches("/thread ").trim();
            match value.parse::<usize>() {
                Ok(number) if number >= 1 => match switch_thread(number - 1) {
                    Ok(index) => {
                        ui.notice(
                            "切换线程",
                            &format!("已切换到线程 {}。\n\n{}", index + 1, threads_report()),
                        )?;
                        return Ok(CommandResult::Main);
                    }
                    Err(err) => {
                        ui.alert("无法切换线程", &format!("{err:#}"))?;
                    }
                },
                _ => {
                    ui.notice(
                        "命令用法",
                        "/thread new 新建线程（/t new）\n/thread N 切换到第 N 个线程（/t N）\n/thread del N 删除第 N 个空闲线程（/t del N）",
                    )?;
                }
            }
        }
        _ => {
            ui.alert(
                "未知命令",
                &format!(
                    "{cmd}\n\n输入 /help 查看可用命令。",
                ),
            )?;
        }
    }
    Ok(CommandResult::Handled)
}

fn load_manifest(ui: &mut Ui, client: &HttpClient, cfg: &Config, force: bool) -> Result<VersionManifest> {
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
    logs::info(format!(
        "{}版本列表（下载源：{}）",
        if force { "刷新" } else { "获取" },
        cfg.mirror.label()
    ));
    let (manifest, _) = manifest::fetch(client, cfg.mirror, force)?;
    logs::info(format!(
        "版本列表获取成功：共 {} 个版本，最新正式版 {}，最新快照 {}",
        manifest.versions.len(),
        manifest.latest.release,
        manifest.latest.snapshot
    ));
    Ok(manifest)
}

fn refresh_manifest(ui: &mut Ui, client: &HttpClient, cfg: &Config) -> Result<VersionManifest> {
    load_manifest(ui, client, cfg, true)
}

fn missing_manifest(ui: &mut Ui) -> Result<()> {
    ui.alert(
        "尚未获取版本列表",
        "请先在“启动器设置”中选择可用的下载源，再使用“刷新版本列表”。",
    )
}

/// 同一个菜单循环：始终全屏重绘，Esc 返回。返回被确认的选项下标。
#[allow(clippy::too_many_arguments)]
fn menu_loop(
    ui: &mut Ui,
    title: &str,
    hint: &str,
    back: bool,
    card: &[(&str, String)],
    items: &[String],
    initial: usize,
    env: &CmdEnv,
) -> Result<Option<usize>> {
    let mut selected = initial.min(items.len().saturating_sub(1));
    loop {
        ui.header(title, hint)?;
        if !card.is_empty() {
            ui.card("", card)?;
            ui.blank()?;
        }
        ui.menu(items, selected)?;
        ui.footer(hint, back)?;
        ui.flush()?;

        let event = ui.wait()?;
        if let Some(result) = maybe_command(ui, &event, env, false)? {
            if matches!(result, CommandResult::Main | CommandResult::Quit) {
                return Ok(None);
            }
            continue;
        }
        match event {
            UiEvent::Key(key) => match key.code {
                KeyCode::Up => selected = ui::cycle_step(selected, items.len(), -1),
                KeyCode::Down => selected = ui::cycle_step(selected, items.len(), 1),
                KeyCode::Enter => return Ok(Some(selected)),
                KeyCode::Esc => return Ok(None),
                _ => {}
            },
            UiEvent::Click(index) => return Ok(Some(index)),
            UiEvent::ScrollUp => selected = ui::cycle_step(selected, items.len(), -1),
            UiEvent::ScrollDown => selected = ui::cycle_step(selected, items.len(), 1),
            UiEvent::Back => return Ok(None),
            UiEvent::Redraw => {}
        }
    }
}

fn overview_rows(cfg: &Config, manifest: Option<&VersionManifest>) -> Vec<(&'static str, String)> {
    let mut rows: Vec<(&'static str, String)> = vec![
        ("账户", cfg.account_label()),
        ("线程", thread_label()),
    ];
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
    rows.push(("版本隔离", cfg.isolation_label().to_string()));
    let log_name = logs::path()
        .and_then(|path| path.file_name().map(|name| name.to_string_lossy().to_string()))
        .unwrap_or_else(|| "不可用".to_string());
    rows.push(("日志", log_name));
    rows
}

fn current_version_id(cfg: &Config, manifest: &VersionManifest) -> String {
    if let Some(id) = &cfg.last_version
        && (manifest.contains(id)
            || install::is_version_installed(&cfg.game_dir(), id)
            || install::has_local_version(&cfg.game_dir(), id))
    {
        return id.clone();
    }
    manifest.latest.release.clone()
}

/// 主菜单“启动游戏”项：附带当前版本号与版本文件夹名。
fn launch_menu_label(cfg: &Config, manifest: &VersionManifest) -> String {
    let id = current_version_id(cfg, manifest);
    let locals = install::list_local_versions(&cfg.game_dir());
    let folder = locals
        .iter()
        .find(|local| local.id == id && local.installed)
        .or_else(|| locals.iter().find(|local| local.id == id))
        .map(|local| local.folder.clone());
    match folder {
        Some(folder) => format!("启动游戏 - {id} ({folder})"),
        None => format!("启动游戏 - {id}"),
    }
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

/// 前台启动：安装与启动过程在当前线程界面展示，启动后回到界面并显示运行状态。
/// 检查并安装启动器更新。
fn check_update(ui: &mut Ui, client: &HttpClient, cfg: &Config) -> Result<()> {
    let release = match update::check(client) {
        Ok(Some(release)) => release,
        Ok(None) => return Ok(()),
        Err(err) => {
            logs::info(format!("检查更新失败：{err:#}"));
            return Ok(());
        }
    };
    logs::info(format!("发现新版本 {}", release.version));

    if cfg.update_mode == UpdateMode::Ask {
        let choices = vec!["稍后".to_string(), format!("更新到 {}", release.version)];
        let Some(choice) = ui.pick(
            "发现新版本",
            &format!(
                "当前版本 {}，最新版本 {}",
                env!("CARGO_PKG_VERSION"),
                release.version
            ),
            &choices,
            1,
        )?
        else {
            return Ok(());
        };
        if choice != 1 {
            return Ok(());
        }
    }

    ui.begin_work("正在更新启动器", format!("下载 {}", release.version))?;
    ui.flush()?;
    if let Err(err) = update::install(client, &release) {
        logs::error(format!("更新失败：{err:#}"));
        ui.alert("更新失败", &format!("{err:#}"))?;
        return Ok(());
    }
    RELAUNCH.store(true, Ordering::SeqCst);
    Ok(())
}

/// 带进度显示与取消能力的版本安装。
fn install_with_screen(
    ui: &mut Ui,
    client: &HttpClient,
    cfg: &Config,
    entry: &VersionEntry,
) -> Result<VersionJson> {
    let installed_before = install::is_version_installed(&cfg.game_dir(), &entry.id);
    let progress = net::begin_shared(&format!("准备安装 {}", entry.id));
    let worker = {
        let client = client.clone();
        let cfg = cfg.clone();
        let entry = entry.clone();
        std::thread::spawn(move || install::ensure_installed(&client, &cfg, &entry))
    };

    let mut cancelled = false;
    let mut last_draw: Option<(String, u64, u64, bool, u16, u16)> = None;
    let result = loop {
        if worker.is_finished() {
            break worker
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("安装线程异常结束")));
        }
        let (label, done, total) = progress.snapshot();
        let key = (label, done, total, cancelled, ui.width(), ui.height());
        // 内容没变化就不重绘，减少闪动。
        if last_draw.as_ref() != Some(&key) {
            draw_install_screen(ui, &key.0, done, total, &entry.id, cancelled)?;
            last_draw = Some(key);
        }
        let Some(event) = ui.poll_event(Duration::from_millis(150))? else {
            continue;
        };
        match event {
            UiEvent::Key(key) => match key.code {
                KeyCode::Esc => {
                    progress.cancel();
                    cancelled = true;
                }
                KeyCode::Char('/') => {
                    if let Some(command) = command_mode(ui)? {
                        match command.trim() {
                            "/cancel" => {
                                progress.cancel();
                                cancelled = true;
                            }
                            "/quit" | "/q" | "/exit" => {
                                progress.cancel();
                                QUIT_REQUESTED.store(true, Ordering::SeqCst);
                            }
                            "/help" | "/h" | "/?" => {
                                ui.notice(
                                    "下载中的命令",
                                    "/cancel 取消下载\n/quit 退出启动器\n/help 查看帮助",
                                )?;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            UiEvent::Click(0) | UiEvent::Back => {
                progress.cancel();
                cancelled = true;
            }
            _ => {}
        }
    };
    net::end_shared();

    if cancelled && !installed_before {
        // 取消后清理本次下载产生的文件，避免留下半成品。
        let root = cfg.game_dir();
        let _ = std::fs::remove_dir_all(install::version_dir(&root, &entry.id));
        let _ = std::fs::remove_dir_all(install::natives_dir(&root, &entry.id));
    }
    result
}

fn draw_install_screen(
    ui: &mut Ui,
    label: &str,
    done: u64,
    total: u64,
    version: &str,
    cancelling: bool,
) -> Result<()> {
    ui.header("安装游戏", version)?;
    ui.blank()?;
    ui.line(&format!("  {label}"))?;
    ui.blank()?;

    let width = (ui.width() as usize).saturating_sub(8).clamp(20, 50);
    let (percent, filled) = if total > 0 {
        let percent = done.saturating_mul(100).checked_div(total).unwrap_or(0).min(100);
        (percent, (percent as usize * width) / 100)
    } else {
        (0, 0)
    };
    let bar = format!(
        "  {}{}  {percent}%",
        "█".repeat(filled),
        "░".repeat(width - filled)
    );
    ui.line(&bar)?;
    ui.blank()?;
    if cancelling {
        ui.dim_line("  正在取消，请稍候…")?;
    } else {
        ui.dim_line("  Esc 取消下载    / 命令")?;
    }
    ui.blank()?;
    ui.menu(&["取消下载".to_string()], 0)?;
    ui.footer("↑↓/滚轮 选择    Enter/点击 取消下载    Esc 取消    / 命令", false)
}

fn start_game(
    ui: &mut Ui,
    client: &HttpClient,
    cfg: &mut Config,
    manifest: &VersionManifest,
) -> Result<()> {
    let id = current_version_id(cfg, manifest);
    set_thread_status(format!("启动中：{id}"));

    if cfg.login_mode == LoginMode::Offline && cfg.username.trim().is_empty() {
        set_thread_status("空闲".to_string());
        ui.alert(
            "还没有离线用户名",
            "请先在“账户管理”中设置用户名，或登录微软账户。",
        )?;
        return Ok(());
    }

    let root = cfg.game_dir();
    let installed = install::is_version_installed(&root, &id);
    logs::info(format!(
        "准备启动版本 {id}（{}，隔离目录 {}）",
        if installed { "已安装" } else { "需安装" },
        cfg.version_game_dir(&id).display()
    ));
    ui.begin_work(
        "准备启动 Minecraft",
        format!("版本 {id}"),
    )?;
    ui.flush()?;

    let version = match manifest.find(&id) {
        Some(entry) => {
            if install::is_version_installed(&root, &entry.id) {
                install::ensure_installed(client, cfg, entry)?
            } else {
                install_with_screen(ui, client, cfg, entry)?
            }
        }
        None => {
            let local = install::load_version_merged(&root, &id).with_context(|| {
                format!("无法读取本地版本 {id}（可能缺少父版本 JSON）")
            })?;
            if !installed {
                bail!("本地版本 {id} 缺少客户端 jar，请确认 versions/{id}/{id}.jar 存在");
            }
            local
        }
    };
    let required = launch::required_java_major(&version);
    logs::info(format!(
        "版本文件已就绪：{}（需要 Java {}）",
        version.id, required
    ));

    let java = match java::pick(cfg.java_path.as_deref(), required) {
        Ok(java) => java,
        Err(err) => {
            ui.alert(
                "自动选择 Java 失败",
                &format!("{err:#}\n\n请手动选择一个可用的 Java 运行环境。"),
            )?;
            match choose_java(ui, cfg, required)? {
                Some(java) => java,
                None => {
                    set_thread_status("空闲".to_string());
                    return Ok(());
                }
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
        set_thread_status("空闲".to_string());
        return Ok(());
    }
    logs::info(format!(
        "Java 运行环境：{}（主版本 {}，版本需要 {}）",
        java.path.display(),
        java.major,
        required
    ));

    let Some(session) = ensure_session(ui, client, cfg)? else {
        set_thread_status("空闲".to_string());
        return Ok(());
    };

    let window = if cfg.fullscreen {
        "全屏".to_string()
    } else {
        format!("{} × {}", cfg.width, cfg.height)
    };
    ui.begin()?;
    ui.line(&format!(
        "  正在启动 {}（Java {}，{} MB）…",
        version.id,
        java.label(),
        cfg.effective_memory_mb()
    ))?;
    ui.flush()?;

    // 首次启动前写好中文语言与无障碍设置，跳过欢迎引导界面。
    if let Err(err) = launch::prepare_options(cfg, &version) {
        logs::error(format!("写入游戏初始设置失败：{err:#}"));
    }

    let mut command = launch::build_command(cfg, &java, &version, &session)?;
    // 游戏自带日志文件，控制台输出直接丢弃，避免占满启动器界面。
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    logs::detail(|| {
        format!(
            "Java 程序：{}，启动参数数量：{}",
            java.path.display(),
            command.get_args().count()
        )
    });
    logs::info(format!(
        "启动参数：版本 {}，内存 {} MB，窗口 {}，游戏目录 {}",
        version.id,
        cfg.effective_memory_mb(),
        window,
        cfg.version_game_dir(&version.id).display()
    ));
    let mut child = command
        .spawn()
        .with_context(|| format!("无法启动 Java：{}", java.path.display()))?;

    let pid = child.id();
    logs::info(format!("游戏进程已启动，PID {pid}"));
    set_thread_status(format!("运行中：{}（PID {pid}）", version.id));
    // 在后台等待进程退出，让线程状态能及时恢复为空闲。
    if let Some(status) = thread_status_arc() {
        std::thread::spawn(move || {
            if let Ok(code) = child.wait() {
                logs::info(format!("游戏进程退出：{code}"));
                *status.lock().unwrap() = format!("空闲（{} 已退出）", code);
            }
        });
    }
    ui.notice(
        "游戏运行中",
        &format!(
            "版本 {} 正在运行（PID {pid}）。\n\n启动器已回到界面，可继续使用 / 命令（/thread 查看线程状态）。",
            version.id
        ),
    )
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
    items.push("手动输入 Java 路径".to_string());

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

    let Some(value) = ui.input("Java 路径", "Java 可执行文件的完整路径", "", false, None)? else {
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

fn collect_versions(root: &Path, manifest: &VersionManifest) -> Vec<VersionRow> {
    // 每次打开版本列表都重新扫描 versions 文件夹，确保已安装状态最新。
    let locals = install::list_local_versions(root);
    let mut rows: Vec<VersionRow> = manifest
        .versions
        .iter()
        .map(|entry| {
            let local = locals.iter().find(|local| local.id == entry.id);
            VersionRow {
                id: entry.id.clone(),
                kind: entry.kind_label().to_string(),
                date: entry.date(),
                installed: local.is_some_and(|local| local.installed),
                local_only: false,
                folder: local.map(|local| local.folder.clone()),
            }
        })
        .collect();

    for local in locals {
        if rows.iter().any(|row| row.id == local.id) {
            continue;
        }
        let kind = local.kind_label().to_string();
        rows.push(VersionRow {
            installed: local.installed,
            folder: Some(local.folder),
            id: local.id,
            kind,
            date: "本地文件".to_string(),
            local_only: true,
        });
    }

    rows
}

/// 二选一确认框，返回是否选择了“确认”。
fn confirm_dialog(ui: &mut Ui, title: &str, message: &str) -> Result<bool> {
    let items = vec!["取消".to_string(), "移入回收站".to_string()];
    Ok(ui.pick(title, message, &items, 0)? == Some(1))
}

fn choose_version(
    ui: &mut Ui,
    client: &HttpClient,
    cfg: &mut Config,
    manifest: &mut VersionManifest,
) -> Result<Option<String>> {
    let root = cfg.game_dir();
    let current = current_version_id(cfg, manifest);
    let mut rows = collect_versions(&root, manifest);
    let mut refreshed = false;

    let mut filter = String::new();
    let mut scope = VersionScope::All;
    let mut start = 0usize;
    let mut selected = rows.iter().position(|row| row.id == current).unwrap_or(0);
    let mut blink = true;

    loop {
        let needle = filter.to_lowercase();
        let visible: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, row)| scope.matches(row))
            .filter(|(_, row)| {
                needle.is_empty()
                    || row.id.to_lowercase().contains(&needle)
                    || row.kind.contains(&filter)
                    || row
                        .folder
                        .as_ref()
                        .is_some_and(|folder| folder.to_lowercase().contains(&needle))
            })
            .map(|(index, _)| index)
            .collect();

        if visible.is_empty() {
            selected = 0;
        } else if selected >= visible.len() {
            selected = visible.len() - 1;
        }

        let items: Vec<String> = visible
            .iter()
            .map(|&index| {
                let row = &rows[index];
                let marker = if row.installed { "●" } else { "○" };
                let name = match (&row.folder, row.installed) {
                    (Some(folder), true) => format!("{}（{}）", row.id, folder),
                    _ => row.id.clone(),
                };
                let mut suffix = String::new();
                if row.local_only {
                    suffix.push_str("  ◆ 本地");
                }
                if row.id == current {
                    suffix.push_str("  ★ 当前");
                }
                format!(
                    "{marker} {}  {}  {}{suffix}",
                    pad_label(&name, 24),
                    pad_label(&row.kind, 10),
                    row.date
                )
            })
            .collect();

        let installed_count = rows.iter().filter(|row| row.installed).count();
        ui.header(
            "版本选择",
            "F1 最新正式版    F2 最新快照    F5 刷新列表    Tab 切换范围    Enter/点击 选择并启动    Del 删除版本    Esc 返回",
        )?;
        ui.card(
            "快捷启动",
            &[
                (
                    "[F1] 最新正式版",
                    format!("{}  ·  一键下载安装并启动", manifest.latest.release),
                ),
                (
                    "[F2] 最新快照",
                    format!("{}  ·  一键下载安装并启动", manifest.latest.snapshot),
                ),
            ],
        )?;
        ui.blank()?;
        let filter_row = ui.row();
        ui.line(&format!(
            "  筛选：{filter}{}        范围：{}（Tab 切换）",
            if blink { "▏" } else { " " },
            scope.label()
        ))?;
        ui.blank()?;

        let max_rows = (ui.height().saturating_sub(ui.row().saturating_add(4)) as usize).max(1);
        if selected >= start + max_rows {
            start = selected + 1 - max_rows;
        }
        if selected < start {
            start = selected;
        }
        ui.list(&items, selected, start, max_rows)?;
        ui.dim_line(&format!(
            "  共 {} 个版本 · 已安装 {} 个 · 当前显示 {} 个{}",
            rows.len(),
            installed_count,
            visible.len(),
            if refreshed { " · 已刷新" } else { "" }
        ))?;
        if let Some(&index) = visible.get(selected) {
            let row = &rows[index];
            let status = if row.local_only {
                "本地版本，直接使用现有文件"
            } else if row.installed {
                "已安装，直接启动"
            } else {
                "未安装，将自动下载后启动"
            };
            ui.blank()?;
            ui.line(&format!("  已选 {} · {} · {status}", row.id, row.kind))?;
        }
        ui.footer(
            "↑↓/滚轮 选择    Enter/点击 选择并启动    F1/F2 快捷启动    F5 刷新    Tab 切换    Del 删除    Esc 返回",
            true,
        )?;
        ui.flush()?;

        // 等待输入期间让筛选框的光标闪烁，只重绘筛选行。
        let event = loop {
            if let Some(event) = ui.poll_event(ui::BLINK_INTERVAL)? {
                // 鼠标移动只触发重绘，不能重置光标相位，否则会快速闪烁。
                if matches!(event, UiEvent::Key(_)) {
                    blink = true;
                }
                break event;
            }
            blink = !blink;
            ui.rewrite_line(
                filter_row,
                &format!(
                    "  筛选：{filter}{}        范围：{}（Tab 切换）",
                    if blink { "▏" } else { " " },
                    scope.label()
                ),
            )?;
            ui.flush()?;
        };

        if let Some(result) = maybe_command(
            ui,
            &event,
            &CmdEnv { cfg: &*cfg },
            false,
        )? {
            if matches!(result, CommandResult::Main | CommandResult::Quit) {
                return Ok(None);
            }
            continue;
        }

        let mut confirm = false;
        refreshed = false;
        match event {
            UiEvent::Click(index) => {
                selected = index;
                confirm = true;
            }
            UiEvent::ScrollUp => selected = ui::cycle_step(selected, visible.len(), -1),
            UiEvent::ScrollDown => selected = ui::cycle_step(selected, visible.len(), 1),
            UiEvent::Back => return Ok(None),
            UiEvent::Redraw => {}
            UiEvent::Key(key) => match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::F(1) => return Ok(Some(manifest.latest.release.clone())),
                KeyCode::F(2) => return Ok(Some(manifest.latest.snapshot.clone())),
                KeyCode::F(5) => {
                    match refresh_manifest(ui, client, cfg) {
                        Ok(fresh) => {
                            *manifest = fresh;
                            rows = collect_versions(&root, manifest);
                            let current = current_version_id(cfg, manifest);
                            selected = rows
                                .iter()
                                .position(|row| row.id == current)
                                .unwrap_or(0);
                            start = 0;
                            refreshed = true;
                        }
                        Err(err) => {
                            logs::error(format!("刷新版本列表失败：{err:#}"));
                            ui.alert("刷新版本列表失败", &format!("{err:#}"))?;
                        }
                    }
                    continue;
                }
                KeyCode::Enter => confirm = true,
                KeyCode::Delete => {
                    let Some(&index) = visible.get(selected) else {
                        continue;
                    };
                    let (folder, id, installed) = {
                        let row = &rows[index];
                        (row.folder.clone(), row.id.clone(), row.installed)
                    };
                    let Some(folder) = folder else {
                        ui.alert("无法删除", "该版本没有对应的本地文件夹。")?;
                        continue;
                    };
                    if !installed {
                        ui.alert("无法删除", "该版本尚未安装，没有可删除的文件。")?;
                        continue;
                    }
                    let path = root.join("versions").join(&folder);
                    if !path.is_dir() {
                        ui.alert(
                            "无法删除",
                            &format!("没有找到版本目录：{}", path.display()),
                        )?;
                        continue;
                    }
                    if !confirm_dialog(ui, "删除版本", &format!("将 {folder} 移入回收站？"))? {
                        continue;
                    }
                    if let Err(err) = trash::move_to_trash(&path) {
                        logs::error(format!("删除版本 {folder} 失败：{err:#}"));
                        ui.alert("删除失败", &format!("{err:#}"))?;
                        continue;
                    }
                    logs::info(format!("版本 {folder} 已移入回收站"));
                    if cfg.last_version.as_deref() == Some(id.as_str()) {
                        cfg.last_version = None;
                        cfg.save()?;
                    }
                    rows = collect_versions(&root, manifest);
                    selected = 0;
                    start = 0;
                }
                KeyCode::Tab => {
                    scope = scope.next();
                    selected = 0;
                    start = 0;
                }
                KeyCode::Up => selected = ui::cycle_step(selected, visible.len(), -1),
                KeyCode::Down => selected = ui::cycle_step(selected, visible.len(), 1),
                KeyCode::PageUp => selected = selected.saturating_sub(10),
                KeyCode::PageDown => {
                    if !visible.is_empty() {
                        selected = (selected + 10).min(visible.len() - 1);
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
            },
        }
        if confirm
            && let Some(&index) = visible.get(selected)
        {
            return Ok(Some(rows[index].id.clone()));
        }
    }
}

fn account_menu(ui: &mut Ui, client: &HttpClient, cfg: &mut Config) -> Result<()> {
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
        ];

        let Some(index) = ({
            let env = CmdEnv { cfg: &*cfg };
            menu_loop(
                ui,
                "账户管理",
                "↑↓/滚轮 选择    Enter/点击 确认    Esc 返回",
                true,
                &rows,
                &items,
                selected,
                &env,
            )
        })?
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
                "{}  {}",
                pad_label("版本隔离", 13),
                cfg.isolation_label()
            ),
            format!(
                "{}  {}",
                pad_label("日志等级", 13),
                cfg.log_level.label()
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
            format!(
                "{}  {}",
                pad_label("启动器更新", 13),
                cfg.update_mode.label()
            ),
            format!(
                "{}  {}",
                pad_label("GitHub 加速", 13),
                if cfg.github_accel { "开启" } else { "关闭" }
            ),
        ];

        let Some(index) = ({
            let env = CmdEnv { cfg: &*cfg };
            menu_loop(
                ui,
                "启动器设置",
                "↑↓/滚轮 选择    Enter/点击 修改    Esc 返回",
                true,
                &[],
                &items,
                selected,
                &env,
            )
        })?
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
                let choices = vec!["关闭".to_string(), "开启".to_string()];
                let default = if cfg.isolate_versions { 1 } else { 0 };
                let Some(choice) = ui.pick(
                    "版本隔离",
                    "开启后每个版本使用独立的 versions/<版本> 目录（存档、模组、设置各自分开）",
                    &choices,
                    default,
                )?
                else {
                    continue;
                };
                cfg.isolate_versions = choice == 1;
                cfg.save()?;
            }
            5 => {
                let kinds = [LogLevel::Info, LogLevel::All];
                let choices: Vec<String> =
                    kinds.iter().map(|kind| kind.label().to_string()).collect();
                let default = kinds
                    .iter()
                    .position(|kind| *kind == cfg.log_level)
                    .unwrap_or(0);
                let Some(choice) = ui.pick(
                    "日志等级",
                    "“信息及以上”记录关键流程；“全部”额外记录调试细节",
                    &choices,
                    default,
                )?
                else {
                    continue;
                };
                cfg.log_level = kinds[choice];
                logs::set_level(cfg.log_level);
                logs::info(format!("日志等级切换为：{}", cfg.log_level.label()));
                cfg.save()?;
            }
            6 => {
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
            7 => {
                let choices = vec!["关闭".to_string(), "开启".to_string()];
                let default = if cfg.fullscreen { 1 } else { 0 };
                let Some(choice) = ui.pick("全屏模式", "启动游戏时是否全屏", &choices, default)?
                else {
                    continue;
                };
                cfg.fullscreen = choice == 1;
                cfg.save()?;
            }
            8 => {
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
            9 => {
                let kinds = [UpdateMode::Auto, UpdateMode::Ask, UpdateMode::Off];
                let choices: Vec<String> =
                    kinds.iter().map(|kind| kind.label().to_string()).collect();
                let default = kinds
                    .iter()
                    .position(|kind| *kind == cfg.update_mode)
                    .unwrap_or(0);
                let Some(choice) = ui.pick(
                    "启动器更新",
                    "发现新版本时如何处理",
                    &choices,
                    default,
                )?
                else {
                    continue;
                };
                cfg.update_mode = kinds[choice];
                cfg.save()?;
            }
            10 => {
                let choices = vec![
                    "关闭".to_string(),
                    "开启（需以管理员权限运行启动器）".to_string(),
                ];
                let default = if cfg.github_accel { 1 } else { 0 };
                let Some(choice) = ui.pick(
                    "GitHub 加速",
                    "开启后改写 hosts 加速访问",
                    &choices,
                    default,
                )?
                else {
                    continue;
                };
                if choice == 1 && !hosts::is_admin() {
                    ui.alert(
                        "无法开启加速",
                        "当前不是管理员权限，请右键启动器选择“以管理员身份运行”后再开启。",
                    )?;
                    continue;
                }
                cfg.github_accel = choice == 1;
                cfg.save()?;
                // 开关变化后立即同步到 hosts；没有管理员权限时静默跳过。
                if cfg.github_accel {
                    if let Ok(client) = net::client()
                        && let Err(err) = hosts::apply(&client)
                    {
                        logs::info(format!("GitHub 加速未生效：{err:#}"));
                    }
                } else {
                    hosts::restore();
                }
            }
            _ => {}
        }
    }
}

fn java_settings(ui: &mut Ui, cfg: &mut Config) -> Result<()> {
    let detected = java::detect_all();
    let mut items: Vec<String> = vec!["自动选择（推荐）".to_string()];
    items.extend(detected.iter().map(java::JavaInfo::label));
    items.push("手动输入 Java 路径".to_string());

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
        let Some(value) = ui.input("Java 路径", "Java 可执行文件的完整路径", "", false, None)? else {
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

fn ensure_session(ui: &mut Ui, client: &HttpClient, cfg: &mut Config) -> Result<Option<auth::Session>> {
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
            logs::info(format!("使用离线账户：{name}"));
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
                logs::info(format!("使用已保存的微软令牌：{name}"));
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
                        logs::info(format!("微软令牌刷新成功：{}", session.username));
                        store_ms_login(cfg, &tokens, &session);
                        cfg.save()?;
                        return Ok(Some(session));
                    }
                    Err(err) => {
                        logs::error(format!("刷新微软令牌失败：{err:#}"));
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

fn microsoft_login(ui: &mut Ui, client: &HttpClient, cfg: &mut Config) -> Result<Option<auth::Session>> {
    let client_id = cfg.ms_client_id();

    logs::info(format!("开始微软设备码登录（客户端 ID：{client_id}）"));

    // 设备代码是一次性的：若已被使用（例如在别的窗口兑换过令牌），
    // 自动重新申请一个新的设备代码，避免登录直接失败。
    let mut attempts = 0u32;
    'login: loop {
        attempts += 1;
        ui.begin_work("微软登录", "正在申请设备代码…")?;
        ui.flush()?;
        let code = match auth::request_device_code(client, &client_id) {
            Ok(code) => code,
            Err(err) => {
                ui.alert("无法开始微软登录", &format!("{err:#}"))?;
                return Ok(None);
            }
        };

        let copied = auth::copy_to_clipboard(&code.user_code);
        auth::open_browser(&code.verification_uri);

        let mut interval = code.interval.max(1);
        let deadline = Instant::now() + Duration::from_secs(code.expires_in);

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
            draw_device_code(ui, &code, remaining, copied)?;
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
                auth::PollResult::Used => {
                    logs::error("设备代码已被使用，重新申请新的设备代码");
                    if attempts >= 3 {
                        ui.alert(
                            "微软登录失败",
                            "设备代码已被使用且无法自动恢复，请稍后重新登录。",
                        )?;
                        return Ok(None);
                    }
                    ui.alert(
                        "设备代码已失效",
                        "该设备代码已经被使用过了，需要重新授权。\n\n按任意键后会申请新的设备代码，请重新在浏览器中完成授权。",
                    )?;
                    continue 'login;
                }
                auth::PollResult::Error(err) => {
                    logs::error(format!("微软设备码轮询失败：{err}"));
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
                            logs::error(format!("微软账户验证失败：{err:#}"));
                            ui.alert("微软账户无法启动 Minecraft", &format!("{err:#}"))?;
                            return Ok(None);
                        }
                    }
                }
            }
        }
    }
}

fn draw_device_code(
    ui: &mut Ui,
    code: &auth::DeviceCode,
    remaining: u64,
    copied: bool,
) -> Result<()> {
    ui.header("微软登录", "在浏览器中完成授权，然后回到这里")?;
    ui.blank()?;
    ui.line("  1. 浏览器已自动打开授权页面；如果没有打开，请手动访问：")?;
    ui.line(&format!("     {}", code.verification_uri))?;
    ui.blank()?;
    ui.line("  2. 在页面中输入下面的代码：")?;
    ui.blank()?;
    ui.centered(&code.user_code, Color::Cyan, true)?;
    if copied {
        ui.dim_line("  代码已自动复制到剪贴板，可直接粘贴")?;
    } else {
        ui.dim_line("  未能写入剪贴板，请手动输入上面的代码")?;
    }
    ui.blank()?;
    ui.dim_line(&format!("  等待授权完成… 剩余 {remaining} 秒"))?;
    ui.blank()?;
    ui.dim_line("  首次登录的账户可能需要先在 Xbox 官网创建档案，否则会提示没有游戏档案")?;
    ui.footer("Esc 取消登录", false)?;
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
    logs::info(format!("微软登录成功：{}（{}）", session.username, session.uuid));
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
