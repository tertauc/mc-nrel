use std::collections::VecDeque;
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};

/// 轻量 HTTP 客户端封装，TLS 使用各平台系统实现。
#[derive(Clone)]
pub struct HttpClient {
    user_agent: String,
}

#[cfg(not(windows))]
const REQUEST_TIMEOUT_SECS: u64 = 600;

/// 统一的响应类型：状态码、响应头与可流式读取的正文。
pub struct Response {
    pub status_code: u16,
    pub headers: HashMap<String, String>,
    reader: Box<dyn Read>,
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}

impl HttpClient {
    pub fn get_text(&self, url: &str) -> Result<String> {
        let resp = self.send("GET", url, &[], None)?;
        if !(200..300).contains(&resp.status_code) {
            bail!("请求失败: {url}（HTTP {}）", resp.status_code);
        }
        read_text(resp)
    }

    /// 获取响应流，非 2xx 直接报错。
    pub fn get_stream(&self, url: &str, bearer: Option<&str>) -> Result<Response> {
        let headers: Vec<(&str, String)> = match bearer {
            Some(token) => vec![("Authorization", format!("Bearer {token}"))],
            None => Vec::new(),
        };
        let resp = self.send("GET", url, &headers, None)?;
        if !(200..300).contains(&resp.status_code) {
            bail!("请求失败: {url}（HTTP {}）", resp.status_code);
        }
        Ok(resp)
    }

    /// 获取文本响应与状态码（4xx/5xx 也会返回）。
    pub fn get_text_status(&self, url: &str, bearer: Option<&str>) -> Result<(u16, String)> {
        let headers: Vec<(&str, String)> = match bearer {
            Some(token) => vec![("Authorization", format!("Bearer {token}"))],
            None => Vec::new(),
        };
        let resp = self.send("GET", url, &headers, None)?;
        let status = resp.status_code;
        Ok((status, read_text(resp)?))
    }

    /// 发送文本请求，返回状态码与响应体（4xx/5xx 也会返回）。
    pub fn post_text(
        &self,
        url: &str,
        content_type: &str,
        body: &str,
    ) -> Result<(u16, String)> {
        let headers = [("Content-Type", content_type.to_string())];
        let resp = self.send("POST", url, &headers, Some(body))?;
        let status = resp.status_code;
        Ok((status, read_text(resp)?))
    }
}

fn read_text(mut resp: Response) -> Result<String> {
    let mut text = String::new();
    resp.read_to_string(&mut text)
        .map_err(|err| anyhow::anyhow!("读取响应内容失败：{err}"))?;
    Ok(text)
}

/// 响应头大小写不敏感查找。
pub fn header<'a>(headers: &'a HashMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Windows 使用系统自带的 WinHTTP，省掉整套第三方 TLS 与 HTTP 实现。
#[cfg(windows)]
impl HttpClient {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<Response> {
        use std::ffi::c_void;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Networking::WinHttp::{
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_ADDREQ_FLAG_ADD,
            WINHTTP_ADDREQ_FLAG_REPLACE, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER,
            WINHTTP_QUERY_RAW_HEADERS_CRLF, WINHTTP_QUERY_STATUS_CODE, WinHttpAddRequestHeaders,
            WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
            WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
        };

        fn wide(text: &str) -> Vec<u16> {
            let mut buffer: Vec<u16> = text.encode_utf16().collect();
            buffer.push(0);
            buffer
        }

        struct Handles {
            session: *mut c_void,
            connect: *mut c_void,
            request: *mut c_void,
        }

        impl Drop for Handles {
            fn drop(&mut self) {
                unsafe {
                    if !self.request.is_null() {
                        WinHttpCloseHandle(self.request);
                    }
                    if !self.connect.is_null() {
                        WinHttpCloseHandle(self.connect);
                    }
                    if !self.session.is_null() {
                        WinHttpCloseHandle(self.session);
                    }
                }
            }
        }

        struct Body {
            request: *mut c_void,
            _handles: Handles,
            done: bool,
        }

        impl Read for Body {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.done || buf.is_empty() {
                    return Ok(0);
                }
                let mut read = 0u32;
                let ok = unsafe {
                    WinHttpReadData(
                        self.request,
                        buf.as_mut_ptr() as *mut c_void,
                        buf.len() as u32,
                        &mut read,
                    )
                };
                if ok == 0 || read == 0 {
                    self.done = true;
                    return Ok(0);
                }
                Ok(read as usize)
            }
        }

        let (secure, host, port, path) = split_url(url)?;

        unsafe {
            let agent = wide(&self.user_agent);
            let session = WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                std::ptr::null(),
                std::ptr::null(),
                0,
            );
            if session.is_null() {
                bail!("无法初始化系统网络：{url}（错误码 {}）", GetLastError());
            }
            let mut handles = Handles {
                session,
                connect: std::ptr::null_mut(),
                request: std::ptr::null_mut(),
            };
            WinHttpSetTimeouts(session, 15_000, 15_000, 60_000, 600_000);

            let host_wide = wide(&host);
            let connect = WinHttpConnect(session, host_wide.as_ptr(), port, 0);
            if connect.is_null() {
                bail!("无法连接服务器：{host}（错误码 {}）", GetLastError());
            }
            handles.connect = connect;

            let verb = wide(method);
            let target = wide(&path);
            let flags = if secure { WINHTTP_FLAG_SECURE } else { 0 };
            let request = WinHttpOpenRequest(
                connect,
                verb.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                flags,
            );
            if request.is_null() {
                bail!("无法创建请求：{url}（错误码 {}）", GetLastError());
            }
            handles.request = request;

            if !headers.is_empty() {
                let mut raw = String::new();
                for (name, value) in headers {
                    raw.push_str(name);
                    raw.push_str(": ");
                    raw.push_str(value);
                    raw.push_str("\r\n");
                }
                let raw = wide(&raw);
                WinHttpAddRequestHeaders(
                    request,
                    raw.as_ptr(),
                    (raw.len() - 1) as u32,
                    WINHTTP_ADDREQ_FLAG_ADD | WINHTTP_ADDREQ_FLAG_REPLACE,
                );
            }

            let payload = body.map(|text| text.as_bytes());
            let (ptr, len) = match payload {
                Some(bytes) => (bytes.as_ptr() as *const c_void, bytes.len() as u32),
                None => (std::ptr::null(), 0),
            };
            let sent = WinHttpSendRequest(request, std::ptr::null(), 0, ptr, len, len, 0);
            if sent == 0 {
                bail!("发送请求失败：{url}（错误码 {}）", GetLastError());
            }
            if WinHttpReceiveResponse(request, std::ptr::null_mut()) == 0 {
                bail!("接收响应失败：{url}（错误码 {}）", GetLastError());
            }

            let mut status: u32 = 0;
            let mut status_len = std::mem::size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut status as *mut u32 as *mut c_void,
                &mut status_len,
                std::ptr::null_mut(),
            );

            // WinHTTP 是宽字符接口，原始响应头按 UTF-16 返回。
            let mut raw_headers = vec![0u16; 4096];
            let mut raw_len = (raw_headers.len() * 2) as u32;
            let mut parsed = HashMap::new();
            let ok = WinHttpQueryHeaders(
                request,
                WINHTTP_QUERY_RAW_HEADERS_CRLF,
                std::ptr::null(),
                raw_headers.as_mut_ptr() as *mut c_void,
                &mut raw_len,
                std::ptr::null_mut(),
            );
            if ok != 0 {
                raw_headers.truncate(raw_len as usize / 2);
                let text = String::from_utf16_lossy(&raw_headers);
                for line in text.split("\r\n").skip(1) {
                    if let Some((name, value)) = line.split_once(':') {
                        parsed.insert(name.trim().to_string(), value.trim().to_string());
                    }
                }
            }

            Ok(Response {
                status_code: status as u16,
                headers: parsed,
                reader: Box::new(Body {
                    request,
                    _handles: handles,
                    done: false,
                }),
            })
        }
    }
}

/// 其他平台使用轻量的 minreq，TLS 走系统实现。
#[cfg(not(windows))]
impl HttpClient {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &[(&str, String)],
        body: Option<&str>,
    ) -> Result<Response> {
        let mut request = if method == "POST" {
            minreq::post(url)
        } else {
            minreq::get(url)
        };
        request = request
            .with_header("User-Agent", &self.user_agent)
            .with_timeout(REQUEST_TIMEOUT_SECS as i32);
        for (name, value) in headers {
            request = request.with_header(name, value);
        }
        if let Some(body) = body {
            request = request.with_body(body);
        }
        let resp = request
            .send_lazy()
            .map_err(|err| anyhow::anyhow!("请求失败: {url}（{err}）"))?;
        Ok(Response {
            status_code: resp.status_code as u16,
            headers: resp.headers.clone(),
            reader: Box::new(resp),
        })
    }
}

/// 把 `https://host:port/path` 拆成 TLS 标记、主机、端口与路径。
#[cfg(windows)]
fn split_url(url: &str) -> Result<(bool, String, u16, String)> {
    let (secure, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        bail!("不支持的地址：{url}");
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.to_string(),
            port.parse::<u16>()
                .with_context(|| format!("地址端口不合法：{url}"))?,
        ),
        None => (
            authority.to_string(),
            if secure { 443 } else { 80 },
        ),
    };
    if host.is_empty() {
        bail!("地址缺少主机名：{url}");
    }
    Ok((secure, host, port, path.to_string()))
}

/// 单行原地刷新的极简进度显示，替代完整的进度条依赖。
#[derive(Clone)]
pub struct Progress {
    inner: Arc<ProgressInner>,
}

struct ProgressInner {
    label: Mutex<String>,
    total: AtomicU64,
    pos: AtomicU64,
    bytes: bool,
    last: Mutex<Option<Instant>>,
}

impl Progress {
    fn new(label: String, total: u64, bytes: bool) -> Self {
        Self {
            inner: Arc::new(ProgressInner {
                label: Mutex::new(label),
                total: AtomicU64::new(total),
                pos: AtomicU64::new(0),
                bytes,
                last: Mutex::new(None),
            }),
        }
    }

    /// 按字节数显示进度的下载进度。
    pub fn file(label: &str) -> Self {
        Self::new(label.to_string(), 0, true)
    }

    /// 按文件个数显示进度的批量进度。
    pub fn counter(label: &str, total: u64) -> Self {
        Self::new(label.to_string(), total, false)
    }

    pub fn set_total(&self, total: u64) {
        self.inner.total.store(total, Ordering::Relaxed);
    }

    pub fn set_label(&self, label: &str) {
        *self.inner.label.lock().unwrap() = label.to_string();
    }

    pub fn reset(&self) {
        self.inner.pos.store(0, Ordering::Relaxed);
    }

    pub fn inc(&self, amount: u64) {
        self.inner.pos.fetch_add(amount, Ordering::Relaxed);
        self.draw(false);
    }

    /// 结束显示，占用过的行用换行收尾。
    pub fn finish(&self) {
        eprint!("\r\x1b[K");
        let _ = std::io::stderr().flush();
    }

    fn draw(&self, force: bool) {
        if !force {
            let mut last = self.inner.last.lock().unwrap();
            if let Some(prev) = *last
                && prev.elapsed() < Duration::from_millis(100)
            {
                return;
            }
            *last = Some(Instant::now());
        }

        let total = self.inner.total.load(Ordering::Relaxed);
        let pos = self.inner.pos.load(Ordering::Relaxed);
        let label = self.inner.label.lock().unwrap().clone();
        let line = if self.inner.bytes {
            let percent = (pos.saturating_mul(100).checked_div(total)).unwrap_or(0).min(100);
            format!(
                "{label}  {:>3}%  {}/{}",
                percent,
                human_size(pos),
                human_size(total)
            )
        } else {
            format!("{label}  {pos}/{total}")
        };
        // 截断到终端一行的安全长度，避免折行破坏界面。
        let width = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80);
        let line = truncate_display(&line, width.saturating_sub(2));
        eprint!("\r\x1b[K  {line}");
        let _ = std::io::stderr().flush();
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn truncate_display(text: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;

    if UnicodeWidthStr::width(text) <= width {
        return text.to_string();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        use unicode_width::UnicodeWidthChar;

        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width.saturating_sub(3) {
            result.push_str("...");
            break;
        }
        result.push(ch);
        used += w;
    }
    result
}

pub fn client() -> Result<HttpClient> {
    Ok(HttpClient {
        user_agent: format!(
            "NREL/{} (Minecraft Launcher)",
            env!("CARGO_PKG_VERSION")
        ),
    })
}

pub fn get_text(client: &HttpClient, url: &str) -> Result<String> {
    client.get_text(url)
}

#[derive(Debug, Clone)]
pub struct DownloadTask {
    pub url: String,
    pub path: PathBuf,
    pub sha1: Option<String>,
    pub size: Option<u64>,
}

impl DownloadTask {
    pub fn new(url: impl Into<String>, path: PathBuf) -> Self {
        Self {
            url: url.into(),
            path,
            sha1: None,
            size: None,
        }
    }

    pub fn with_hash(mut self, sha1: Option<String>) -> Self {
        self.sha1 = sha1;
        self
    }

    pub fn with_size(mut self, size: Option<u64>) -> Self {
        self.size = size;
        self
    }
}

fn existing_is_ok(task: &DownloadTask) -> bool {
    let Ok(meta) = fs::metadata(&task.path) else {
        return false;
    };
    if let Some(size) = task.size {
        return meta.len() == size;
    }
    // 没有大小信息时，若提供了 SHA1 则校验本地文件。
    match &task.sha1 {
        Some(expected) => file_sha1(&task.path)
            .map(|actual| actual.eq_ignore_ascii_case(expected))
            .unwrap_or(false),
        None => false,
    }
}

fn file_sha1(path: &std::path::Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let read = std::io::Read::read(&mut file, &mut buf).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn download_once(
    client: &HttpClient,
    task: &DownloadTask,
    pb: Option<&Progress>,
) -> Result<()> {
    if existing_is_ok(task) {
        return Ok(());
    }

    if let Some(parent) = task.path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("无法创建目录 {}", parent.display()))?;
    }

    let resp = client.get_stream(&task.url, None)?;

    let total = header(&resp.headers, "Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .or(task.size);
    if let Some(pb) = pb {
        if let Some(total) = total {
            pb.set_total(total);
        }
        pb.reset();
        pb.set_label(&short_name(&task.path));
    }

    let tmp = temp_path(&task.path);
    let mut file = fs::File::create(&tmp)
        .with_context(|| format!("无法写入文件 {}", tmp.display()))?;
    let mut hasher = Sha1::new();
    let mut reader = resp;
    let mut buf = vec![0u8; 16 * 1024];

    loop {
        let read = reader.read(&mut buf).context("读取下载数据失败")?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
        file.write_all(&buf[..read]).context("写入文件失败")?;
        if let Some(pb) = pb {
            pb.inc(read as u64);
        }
    }
    drop(file);

    if let Some(expected) = &task.sha1 {
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = fs::remove_file(&tmp);
            bail!(
                "文件校验失败: {}（期望 {expected}，实际 {actual}）",
                task.path.display()
            );
        }
    }

    if task.path.exists() {
        let _ = fs::remove_file(&task.path);
    }
    fs::rename(&tmp, &task.path)
        .with_context(|| format!("无法保存文件 {}", task.path.display()))?;
    Ok(())
}

fn download_retry(client: &HttpClient, task: &DownloadTask, pb: Option<&Progress>) -> Result<()> {
    let mut last_err = None;
    for attempt in 0..3 {
        match download_once(client, task, pb) {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                if attempt < 2 {
                    thread::sleep(Duration::from_millis(800));
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("未知下载错误")))
}

pub fn download_one(client: &HttpClient, task: &DownloadTask, label: &str) -> Result<()> {
    if existing_is_ok(task) {
        return Ok(());
    }
    let pb = Progress::file(label);
    if let Some(size) = task.size {
        pb.set_total(size);
    }
    let result = download_retry(client, task, Some(&pb));
    pb.finish();
    result
}

/// 多线程批量下载，已有且大小一致的文件会被跳过。
pub fn download_many(client: &HttpClient, mut tasks: Vec<DownloadTask>, label: &str) -> Result<()> {
    tasks.sort_by(|a, b| a.path.cmp(&b.path));
    tasks.dedup_by(|a, b| a.path == b.path);
    if tasks.is_empty() {
        return Ok(());
    }

    let total = tasks.len() as u64;
    let pb = Progress::counter(label, total);

    let queue = Arc::new(Mutex::new(tasks.into_iter().collect::<VecDeque<_>>()));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let workers = (total as usize).clamp(1, 8);

    thread::scope(|scope| {
        for _ in 0..workers {
            let queue = Arc::clone(&queue);
            let errors = Arc::clone(&errors);
            let pb = pb.clone();
            scope.spawn(move || {
                loop {
                    let task = queue.lock().unwrap().pop_front();
                    let Some(task) = task else {
                        break;
                    };
                    if let Err(err) = download_retry(client, &task, None) {
                        errors
                            .lock()
                            .unwrap()
                            .push(format!("{}\n  {err:#}", task.url));
                    }
                    pb.inc(1);
                }
            });
        }
    });

    pb.finish();
    let errors = errors.lock().unwrap();
    if !errors.is_empty() {
        bail!(
            "有 {} 个文件下载失败：\n{}",
            errors.len(),
            errors.join("\n")
        );
    }
    Ok(())
}

fn temp_path(path: &std::path::Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    path.with_file_name(format!("{name}.part"))
}

fn short_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}
