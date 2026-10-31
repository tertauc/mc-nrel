use anyhow::{Context, Result, bail};
use md5::{Digest, Md5};
use reqwest::blocking::Client;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};

/// Prism Launcher 的公开 Azure 应用 ID，可用于设备码登录。
pub const DEFAULT_CLIENT_ID: &str = "c36a9fb6-4f2a-41ff-90bd-ae7cc92031eb";

const SCOPE: &str = "XboxLive.signin offline_access";
const DEVICE_CODE_URL: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/devicecode";
const TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const XBL_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MC_LOGIN_URL: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const MC_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

#[derive(Debug, Clone)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Debug, Clone)]
pub struct MsTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

#[derive(Debug, Clone)]
pub enum PollResult {
    Pending,
    SlowDown,
    Expired,
    Declined,
    /// 设备代码已被使用（令牌已下发过一次），只能重新申请设备代码。
    Used,
    Token(MsTokens),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct Session {
    pub username: String,
    pub uuid: String,
    pub access_token: String,
    pub user_type: String,
}

impl Session {
    pub fn offline(name: &str) -> Self {
        Self {
            username: name.to_string(),
            uuid: offline_uuid(name),
            access_token: "0".to_string(),
            user_type: "legacy".to_string(),
        }
    }
}

/// 与 Java 的 `UUID.nameUUIDFromBytes("OfflinePlayer:" + name)` 保持一致。
pub fn offline_uuid(name: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(format!("OfflinePlayer:{name}").as_bytes());
    let mut bytes = hasher.finalize().to_vec();
    bytes[6] = (bytes[6] & 0x0f) | 0x30;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let mut out = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    #[serde(default = "default_interval")]
    interval: u64,
}

fn default_interval() -> u64 {
    5
}

pub fn request_device_code(client: &Client, client_id: &str) -> Result<DeviceCode> {
    let (status, value) = post_form(
        client,
        DEVICE_CODE_URL,
        &[("client_id", client_id), ("scope", SCOPE)],
    )?;

    if !status.is_success() {
        bail!("无法获取微软设备代码：{}", error_text(&value));
    }

    let parsed: DeviceCodeResponse = serde_json::from_value(value)
        .context("微软设备代码响应格式不正确")?;

    Ok(DeviceCode {
        device_code: parsed.device_code,
        user_code: parsed.user_code,
        verification_uri: parsed.verification_uri,
        expires_in: parsed.expires_in,
        interval: parsed.interval.max(1),
    })
}

pub fn poll_device_code(client: &Client, client_id: &str, device_code: &str) -> PollResult {
    let result = post_form(
        client,
        TOKEN_URL,
        &[
            (
                "grant_type",
                "urn:ietf:params:oauth:grant-type:device_code",
            ),
            ("client_id", client_id),
            ("device_code", device_code),
        ],
    );

    let (status, value) = match result {
        Ok(result) => result,
        Err(err) => return PollResult::Error(format!("{err:#}")),
    };

    if status.is_success() {
        return match parse_tokens(&value) {
            Some(tokens) => PollResult::Token(tokens),
            None => PollResult::Error("微软登录响应缺少访问令牌".to_string()),
        };
    }

    match value.get("error").and_then(Value::as_str).unwrap_or("") {
        "authorization_pending" => PollResult::Pending,
        "slow_down" => PollResult::SlowDown,
        "expired_token" | "bad_verification_code" => PollResult::Expired,
        "authorization_declined" | "access_denied" => PollResult::Declined,
        _ => {
            let detail = error_text(&value);
            // 设备代码是一次性的：已经兑换过令牌后再轮询会返回该错误。
            if detail.contains("AADSTS70000") || detail.contains("already been used") {
                PollResult::Used
            } else {
                PollResult::Error(detail)
            }
        }
    }
}

pub fn refresh(client: &Client, client_id: &str, refresh_token: &str) -> Result<MsTokens> {
    let (status, value) = post_form(
        client,
        TOKEN_URL,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", client_id),
            ("refresh_token", refresh_token),
            ("scope", SCOPE),
        ],
    )?;

    if !status.is_success() {
        bail!("微软登录已过期：{}", error_text(&value));
    }

    parse_tokens(&value).context("微软刷新响应缺少访问令牌")
}

fn parse_tokens(value: &Value) -> Option<MsTokens> {
    Some(MsTokens {
        access_token: value.get("access_token")?.as_str()?.to_string(),
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn post_form(
    client: &Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<(StatusCode, Value)> {
    let resp = client
        .post(url)
        .form(form)
        .send()
        .with_context(|| format!("无法连接微软登录服务：{url}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    Ok((status, value))
}

fn post_json(client: &Client, url: &str, body: &Value) -> Result<(StatusCode, Value)> {
    let resp = client
        .post(url)
        .json(body)
        .send()
        .with_context(|| format!("请求失败：{url}"))?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));
    Ok((status, value))
}

fn error_text(value: &Value) -> String {
    for key in ["error_description", "Message", "error"] {
        if let Some(text) = value.get(key).and_then(Value::as_str) {
            return text.to_string();
        }
    }
    if let Some(raw) = value.get("raw").and_then(Value::as_str) {
        return raw.chars().take(200).collect();
    }
    let text = value.to_string();
    text.chars().take(200).collect()
}

/// 完整认证链：微软令牌 → Xbox Live → XSTS → Minecraft → 玩家档案。
pub fn login_with_ms(client: &Client, ms_access_token: &str) -> Result<Session> {
    // 1. Xbox Live 用户令牌
    let (status, value) = post_json(
        client,
        XBL_URL,
        &json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_access_token}")
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }),
    )?;
    if !status.is_success() {
        bail!("Xbox Live 认证失败：{}", error_text(&value));
    }
    let xbl_token = field(&value, &["Token"])?;
    let uhs = value
        .get("DisplayClaims")
        .and_then(|v| v.get("xui"))
        .and_then(|v| v.get(0))
        .and_then(|v| v.get("uhs"))
        .and_then(Value::as_str)
        .context("Xbox Live 响应缺少用户哈希")?
        .to_string();

    // 2. XSTS 授权
    let (status, value) = post_json(
        client,
        XSTS_URL,
        &json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [xbl_token]
            },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        }),
    )?;
    if !status.is_success() {
        let xerr = value.get("XErr").and_then(Value::as_i64).unwrap_or(0);
        let message = match xerr {
            2148916233 => "该微软账户还没有 Xbox 账户，请先在 Xbox 官网创建档案",
            2148916235 => "该账户所在的国家或地区无法使用 Xbox Live",
            2148916227 => "该 Xbox 账户已被封禁",
            2148916236 | 2148916237 => "该账户需要先完成 Xbox 成人验证",
            2148916238 => "该账户是未成年账户，需要由家长加入家庭组后才能登录",
            _ => "Xbox 授权失败",
        };
        bail!("{message}（XErr {xerr}）");
    }
    let xsts_token = field(&value, &["Token"])?;

    // 3. Minecraft 服务令牌
    let (status, value) = post_json(
        client,
        MC_LOGIN_URL,
        &json!({ "identityToken": format!("XBL3.0 x={uhs};{xsts_token}") }),
    )?;
    if !status.is_success() {
        bail!("Minecraft 登录失败：{}", error_text(&value));
    }
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .context("Minecraft 登录响应缺少访问令牌")?
        .to_string();

    // 4. 玩家档案（确认是否拥有游戏）
    let resp = client
        .get(MC_PROFILE_URL)
        .bearer_auth(&access_token)
        .send()
        .context("无法获取 Minecraft 玩家档案")?;
    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    let value: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text }));

    if status == StatusCode::NOT_FOUND {
        bail!("该微软账户未拥有 Minecraft Java 版");
    }
    if !status.is_success() {
        bail!("获取 Minecraft 玩家档案失败：{}", error_text(&value));
    }

    let username = value
        .get("name")
        .and_then(Value::as_str)
        .context("玩家档案缺少名称")?
        .to_string();
    let uuid = value
        .get("id")
        .and_then(Value::as_str)
        .context("玩家档案缺少 UUID")?
        .to_string();

    Ok(Session {
        username,
        uuid,
        access_token,
        user_type: "msa".to_string(),
    })
}

fn field(value: &Value, keys: &[&str]) -> Result<String> {
    for key in keys {
        if let Some(text) = value.get(*key).and_then(Value::as_str) {
            return Ok(text.to_string());
        }
    }
    bail!("认证响应缺少字段：{}", keys.join("/"))
}

/// Azure 应用注册（新建应用）页面，用于自定义登录页显示的应用名称与图标。
pub const APP_REGISTRATION_URL: &str =
    "https://portal.azure.com/#view/Microsoft_AAD_RegisteredApps/CreateApplicationBlade";

/// 把文本写入系统剪贴板，返回是否成功。
pub fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(windows)]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // 设备代码只包含 ASCII 字符，直接通过 clip.exe 写入即可。
        let Ok(mut child) = Command::new("clip")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        let written = match child.stdin.take() {
            Some(mut stdin) => stdin.write_all(text.as_bytes()).is_ok(),
            None => false,
        };
        let _ = child.wait();
        written
    }
    #[cfg(not(windows))]
    {
        let _ = text;
        false
    }
}

pub fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        use std::process::Command;
        let _ = Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(windows))]
    {
        let _ = url;
    }
}
