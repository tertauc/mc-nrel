use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::blocking::Client;
use serde::Deserialize;

use crate::config::Config;
use crate::manifest::VersionEntry;
use crate::mirror::MirrorKind;
use crate::net::{self, DownloadTask};
use crate::ui;

pub const LIBRARIES_BASE: &str = "https://libraries.minecraft.net";
pub const ASSETS_BASE: &str = "https://resources.download.minecraft.net";

#[derive(Debug, Clone, Deserialize)]
pub struct VersionJson {
    pub id: String,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(rename = "mainClass")]
    pub main_class: String,
    #[serde(rename = "minecraftArguments", default)]
    pub minecraft_arguments: Option<String>,
    #[serde(default)]
    pub arguments: Option<Arguments>,
    #[serde(rename = "assetIndex", default)]
    pub asset_index: Option<AssetIndexRef>,
    #[serde(default)]
    pub assets: Option<String>,
    #[serde(default)]
    pub downloads: Option<Downloads>,
    #[serde(rename = "javaVersion", default)]
    pub java_version: Option<JavaVersionRef>,
    #[serde(default)]
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub logging: Option<Logging>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JavaVersionRef {
    #[serde(rename = "majorVersion", default)]
    pub major: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Arguments {
    #[serde(default)]
    pub game: Vec<ArgValue>,
    #[serde(default)]
    pub jvm: Vec<ArgValue>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    Plain(String),
    Ruled {
        rules: Vec<Rule>,
        value: ArgString,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ArgString {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct Rule {
    pub action: String,
    #[serde(default)]
    pub os: Option<OsRule>,
    #[serde(default)]
    pub features: Option<HashMap<String, bool>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OsRule {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arch: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Library {
    pub name: String,
    #[serde(default)]
    pub downloads: Option<LibraryDownloads>,
    #[serde(default)]
    pub natives: Option<HashMap<String, String>>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub extract: Option<Extract>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct LibraryDownloads {
    #[serde(default)]
    pub artifact: Option<Artifact>,
    #[serde(default)]
    pub classifiers: Option<HashMap<String, Artifact>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Extract {
    #[serde(default)]
    pub exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Downloads {
    #[serde(default)]
    pub client: Option<Artifact>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndexRef {
    pub id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default, rename = "virtual")]
    pub is_virtual: bool,
    #[serde(default, rename = "map_to_resources")]
    pub map_to_resources: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetIndexFile {
    #[serde(default)]
    pub objects: HashMap<String, AssetObject>,
    #[serde(default, rename = "virtual")]
    pub is_virtual: bool,
    #[serde(default, rename = "map_to_resources")]
    pub map_to_resources: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AssetObject {
    pub hash: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Logging {
    #[serde(default)]
    pub client: Option<LoggingClient>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingClient {
    #[serde(default)]
    pub argument: String,
    pub file: LoggingFile,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LoggingFile {
    pub id: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub sha1: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
}

pub fn version_dir(root: &Path, id: &str) -> PathBuf {
    root.join("versions").join(id)
}

pub fn version_jar_path(root: &Path, id: &str) -> PathBuf {
    version_dir(root, id).join(format!("{id}.jar"))
}

pub fn natives_dir(root: &Path, id: &str) -> PathBuf {
    version_dir(root, id).join("natives")
}

pub fn assets_dir(root: &Path) -> PathBuf {
    root.join("assets")
}

/// 判断某个资源索引是否启用了 virtual 目录（旧版本需要）。
pub fn asset_index_is_virtual(root: &Path, index_ref: &AssetIndexRef) -> bool {
    if index_ref.is_virtual {
        return true;
    }
    let path = assets_dir(root)
        .join("indexes")
        .join(format!("{}.json", index_ref.id));
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<AssetIndexFile>(&text).ok())
        .map(|index| index.is_virtual)
        .unwrap_or(false)
}

pub fn libraries_dir(root: &Path) -> PathBuf {
    root.join("libraries")
}

pub fn read_local_version(root: &Path, id: &str) -> Option<VersionJson> {
    let path = version_dir(root, id).join(format!("{id}.json"));
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn is_version_installed(root: &Path, id: &str) -> bool {
    let json = version_dir(root, id).join(format!("{id}.json"));
    let jar = version_jar_path(root, id);
    json.is_file() && jar.is_file()
}

/// 补全缺失的版本文件，已下载且大小正确的文件会自动跳过。
pub fn ensure_installed(
    client: &Client,
    cfg: &Config,
    entry: &VersionEntry,
) -> Result<VersionJson> {
    let root = cfg.game_dir();
    fs::create_dir_all(&root).with_context(|| format!("无法创建游戏目录 {}", root.display()))?;
    let vdir = version_dir(&root, &entry.id);
    fs::create_dir_all(&vdir)?;

    ui::step(format!("检查版本 {}", entry.id), "versions/".to_string());

    let version = match read_local_version(&root, &entry.id) {
        Some(v) => v,
        None => {
            let url = cfg.mirror.rewrite(&entry.url);
            let raw = net::get_text(client, &url)?;
            let parsed: VersionJson =
                serde_json::from_str(&raw).context("解析版本 JSON 失败")?;
            fs::write(vdir.join(format!("{}.json", entry.id)), raw)?;
            parsed
        }
    };

    if let Some(client_artifact) = version.downloads.as_ref().and_then(|d| d.client.as_ref()) {
        let jar_path = version_jar_path(&root, &entry.id);
        let url = client_artifact_url(client_artifact);
        let task = DownloadTask::new(cfg.mirror.rewrite(&url), jar_path.clone())
            .with_hash(client_artifact.sha1.clone())
            .with_size(client_artifact.size);
        if !jar_path.is_file() {
            ui::step("下载游戏客户端", entry.id.clone());
            net::download_one(client, &task, "下载游戏客户端")?;
        } else {
            net::download_one(client, &task, "校验游戏客户端")?;
        }
    }

    let features = launch_features();
    let plan = plan_libraries(&version, &root, cfg.mirror, &features)?;

    if !plan.downloads.is_empty() {
        ui::step("下载依赖库", format!("{} 个文件", plan.downloads.len()));
        net::download_many(client, plan.downloads, "下载依赖库")?;
    }

    if !plan.natives.is_empty() {
        let natives = natives_dir(&root, &entry.id);
        fs::create_dir_all(&natives)?;
        for native in &plan.natives {
            if native.jar.is_file() {
                extract_natives(&native.jar, &natives, &native.exclude)?;
            }
        }
    }

    if let Some(index_ref) = &version.asset_index {
        ensure_assets(client, cfg, &root, index_ref)?;
    }

    Ok(version)
}

fn client_artifact_url(artifact: &Artifact) -> String {
    if !artifact.url.is_empty() {
        return artifact.url.clone();
    }
    match &artifact.sha1 {
        Some(hash) => format!("https://launcher.mojang.com/v1/objects/{hash}/client.jar"),
        None => String::new(),
    }
}

fn ensure_assets(
    client: &Client,
    cfg: &Config,
    root: &Path,
    index_ref: &AssetIndexRef,
) -> Result<()> {
    let assets = assets_dir(root);
    let indexes = assets.join("indexes");
    fs::create_dir_all(&indexes)?;
    let index_path = indexes.join(format!("{}.json", index_ref.id));

    if !index_ref.url.is_empty() {
        let task = DownloadTask::new(cfg.mirror.rewrite(&index_ref.url), index_path.clone())
            .with_hash(index_ref.sha1.clone())
            .with_size(index_ref.size);
        net::download_one(client, &task, "下载资源索引")?;
    }

    if !index_path.is_file() {
        ui::warn("该版本没有可用的资源索引，跳过资源下载");
        return Ok(());
    }

    let text = fs::read_to_string(&index_path).context("读取资源索引失败")?;
    let index: AssetIndexFile = serde_json::from_str(&text).context("解析资源索引失败")?;

    let objects_dir = assets.join("objects");
    let mut tasks = Vec::with_capacity(index.objects.len());
    for object in index.objects.values() {
        let Some(prefix) = object.hash.get(0..2) else {
            continue;
        };
        let path = objects_dir.join(prefix).join(&object.hash);
        let url = format!("{ASSETS_BASE}/{prefix}/{}", object.hash);
        tasks.push(
            DownloadTask::new(cfg.mirror.rewrite(&url), path)
                .with_hash(Some(object.hash.clone()))
                .with_size(Some(object.size)),
        );
    }

    if !tasks.is_empty() {
        ui::step("下载游戏资源", format!("{} 个文件", tasks.len()));
        net::download_many(client, tasks, "下载游戏资源")?;
    }

    let is_virtual = index_ref.is_virtual || index.is_virtual;
    let map_to_resources = index_ref.map_to_resources || index.map_to_resources;

    if is_virtual || map_to_resources {
        let virtual_dir = assets.join("virtual").join(&index_ref.id);
        let resources_dir = root.join("resources");
        let mut copied = 0usize;
        for (name, object) in &index.objects {
            // 新版索引以 hash 作为键，没有原始文件名，仅旧版索引需要复制。
            if name.len() == 40 && name.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let source = objects_dir
                .join(&object.hash[0..2.min(object.hash.len())])
                .join(&object.hash);
            let Ok(meta) = fs::metadata(&source) else {
                continue;
            };
            if is_virtual {
                let dest = virtual_dir.join(name);
                copy_if_needed(&source, &dest, meta.len())?;
                copied += 1;
            }
            if map_to_resources {
                let dest = resources_dir.join(name);
                copy_if_needed(&source, &dest, meta.len())?;
                copied += 1;
            }
        }
        if copied > 0 {
            ui::step("整理旧版资源文件", format!("{copied} 个文件"));
        }
    }

    Ok(())
}

fn copy_if_needed(source: &Path, dest: &Path, size: u64) -> Result<()> {
    if let Ok(meta) = fs::metadata(dest)
        && meta.len() == size
    {
        return Ok(());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, dest)
        .with_context(|| format!("复制资源失败: {}", dest.display()))?;
    Ok(())
}

pub fn launch_features() -> HashMap<String, bool> {
    let mut features = HashMap::new();
    features.insert("is_demo_user".to_string(), false);
    features.insert("has_custom_resolution".to_string(), true);
    features.insert("has_quick_plays_support".to_string(), false);
    features.insert("is_quick_play_singleplayer".to_string(), false);
    features.insert("is_quick_play_multiplayer".to_string(), false);
    features.insert("is_quick_play_realms".to_string(), false);
    features
}

pub fn rules_allow(rules: &[Rule], features: &HashMap<String, bool>) -> bool {
    if rules.is_empty() {
        return true;
    }

    let mut allowed = false;
    for rule in rules {
        if !rule_matches(rule, features) {
            continue;
        }
        allowed = rule.action == "allow";
    }
    allowed
}

fn rule_matches(rule: &Rule, features: &HashMap<String, bool>) -> bool {
    if let Some(os) = &rule.os {
        if let Some(name) = &os.name
            && name != "windows"
        {
            return false;
        }
        if let Some(arch) = &os.arch {
            let ours = if cfg!(target_arch = "x86_64") {
                "x86_64"
            } else {
                "x86"
            };
            if arch != ours {
                return false;
            }
        }
        // Windows 版本条件（如 "^10\\."）默认视为满足。
        let _ = &os.version;
    }

    if let Some(features_required) = &rule.features {
        for (key, expected) in features_required {
            if features.get(key).copied().unwrap_or(false) != *expected {
                return false;
            }
        }
    }

    true
}

pub struct LibraryPlan {
    pub classpath: Vec<PathBuf>,
    pub downloads: Vec<DownloadTask>,
    pub natives: Vec<NativeJar>,
}

pub struct NativeJar {
    pub jar: PathBuf,
    pub exclude: Vec<String>,
}

struct ResolvedArtifact {
    full_path: PathBuf,
    url: String,
    sha1: Option<String>,
    size: Option<u64>,
}

pub fn plan_libraries(
    version: &VersionJson,
    root: &Path,
    mirror: MirrorKind,
    features: &HashMap<String, bool>,
) -> Result<LibraryPlan> {
    let lib_root = libraries_dir(root);
    let mut classpath = Vec::new();
    let mut downloads = Vec::new();
    let mut natives = Vec::new();
    let mut seen_classpath = HashSet::new();
    let mut seen_download = HashSet::new();

    for lib in &version.libraries {
        if !rules_allow(&lib.rules, features) {
            continue;
        }

        let artifact = lib.downloads.as_ref().and_then(|d| d.artifact.as_ref());
        let has_windows_natives = lib
            .natives
            .as_ref()
            .map(|natives| natives.contains_key("windows"))
            .unwrap_or(false);

        // 纯 native 库（如 jinput-platform）没有基础 jar，只下载分类器文件。
        if (artifact.is_some() || !has_windows_natives)
            && let Some(resolved) = resolve_artifact(&lib_root, &lib.name, artifact, None)?
        {
            if seen_classpath.insert(resolved.full_path.clone()) {
                classpath.push(resolved.full_path.clone());
            }
            if seen_download.insert(resolved.full_path.clone()) {
                downloads.push(
                    DownloadTask::new(mirror.rewrite(&resolved.url), resolved.full_path)
                        .with_hash(resolved.sha1)
                        .with_size(resolved.size),
                );
            }
        }

        let Some(classifier) = lib.natives.as_ref().and_then(|n| n.get("windows")) else {
            continue;
        };
        let native_artifact = lib
            .downloads
            .as_ref()
            .and_then(|d| d.classifiers.as_ref())
            .and_then(|c| c.get(classifier));
        if let Some(resolved) =
            resolve_artifact(&lib_root, &lib.name, native_artifact, Some(classifier))?
            && seen_download.insert(resolved.full_path.clone())
        {
            let mut exclude = lib
                .extract
                .as_ref()
                .map(|e| e.exclude.clone())
                .unwrap_or_default();
            if exclude.is_empty() {
                exclude.push("META-INF/".to_string());
            }
            natives.push(NativeJar {
                jar: resolved.full_path.clone(),
                exclude,
            });
            downloads.push(
                DownloadTask::new(mirror.rewrite(&resolved.url), resolved.full_path)
                    .with_hash(resolved.sha1)
                    .with_size(resolved.size),
            );
        }
    }

    if let Some(logging) = version.logging.as_ref().and_then(|l| l.client.as_ref())
        && !logging.file.url.is_empty()
    {
        let path = assets_dir(root).join("log_configs").join(&logging.file.id);
        downloads.push(
            DownloadTask::new(mirror.rewrite(&logging.file.url), path)
                .with_hash(logging.file.sha1.clone())
                .with_size(logging.file.size),
        );
    }

    // 旧版库可能没有 downloads 字段，此时会使用 Maven 默认地址补全。
    Ok(LibraryPlan {
        classpath,
        downloads,
        natives,
    })
}

fn resolve_artifact(
    base: &Path,
    name: &str,
    artifact: Option<&Artifact>,
    classifier: Option<&str>,
) -> Result<Option<ResolvedArtifact>> {
    let rel_path = match artifact {
        Some(a) if !a.path.is_empty() => a.path.clone(),
        _ => match classifier {
            Some(classifier) => {
                let coords = format!("{name}:{classifier}");
                match maven_path(&coords) {
                    Some(path) => path,
                    None => bail!("无法解析依赖路径: {coords}"),
                }
            }
            None => match maven_path(name) {
                Some(path) => path,
                None => bail!("无法解析依赖路径: {name}"),
            },
        },
    };

    let url = match artifact {
        Some(a) if !a.url.is_empty() => a.url.clone(),
        _ => format!("{LIBRARIES_BASE}/{rel_path}"),
    };

    Ok(Some(ResolvedArtifact {
        full_path: base.join(rel_path),
        url,
        sha1: artifact.and_then(|a| a.sha1.clone()),
        size: artifact.and_then(|a| a.size),
    }))
}

fn maven_path(coords: &str) -> Option<String> {
    let parts: Vec<&str> = coords.split(':').collect();
    if parts.len() < 3 {
        return None;
    }
    let (group, artifact, version) = (parts[0], parts[1], parts[2]);
    let classifier = parts.get(3).copied();
    let ext = parts.get(4).copied().unwrap_or("jar");
    let group_path = group.replace('.', "/");
    let file = match classifier {
        Some(classifier) => format!("{artifact}-{version}-{classifier}.{ext}"),
        None => format!("{artifact}-{version}.{ext}"),
    };
    Some(format!("{group_path}/{artifact}/{version}/{file}"))
}

fn extract_natives(jar: &Path, dest: &Path, exclude: &[String]) -> Result<()> {
    let file = fs::File::open(jar)
        .with_context(|| format!("无法打开本地库 {}", jar.display()))?;
    let mut archive = zip::ZipArchive::new(file).context("解析本地库失败")?;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("读取压缩包条目失败")?;
        let Some(entry_path) = entry.enclosed_name() else {
            continue;
        };
        let entry_name = entry_path.to_string_lossy().replace('\\', "/");
        if exclude
            .iter()
            .any(|prefix| entry_name.starts_with(prefix.as_str()))
        {
            continue;
        }
        if entry.is_dir() {
            continue;
        }

        let out = dest.join(&entry_path);
        if let Some(parent) = out.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out_file = fs::File::create(&out)
            .with_context(|| format!("无法解压文件 {}", out.display()))?;
        let mut buffer = Vec::new();
        entry.read_to_end(&mut buffer)?;
        std::io::Write::write_all(&mut out_file, &buffer)?;
    }
    Ok(())
}

/// 把版本 JSON 中的 `${placeholder}` 替换为实际值。
pub fn apply_placeholders(value: &str, vars: &HashMap<&str, String>) -> String {
    let mut result = value.to_string();
    for (key, replacement) in vars {
        let placeholder = format!("${{{key}}}");
        if result.contains(&placeholder) {
            result = result.replace(&placeholder, replacement);
        }
    }
    result
}

pub fn expand_argument(
    arg: &ArgValue,
    features: &HashMap<String, bool>,
    vars: &HashMap<&str, String>,
) -> Vec<String> {
    match arg {
        ArgValue::Plain(value) => vec![apply_placeholders(value, vars)],
        ArgValue::Ruled { rules, value } => {
            if !rules_allow(rules, features) {
                return Vec::new();
            }
            match value {
                ArgString::One(value) => vec![apply_placeholders(value, vars)],
                ArgString::Many(values) => values
                    .iter()
                    .map(|value| apply_placeholders(value, vars))
                    .collect(),
            }
        }
    }
}
