//! 可选的 Phira 在线功能。
//!
//! 本地回放 / 谱面流程不依赖这里；只有用户主动登录或下载在线谱面时才会发请求。
//! 登录只保存刷新令牌，密码不会落盘。谱面下载到系统数据目录，随后仍走现有的
//! 本地谱面加载与校验流程。

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    fs,
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};

const API_URL: &str = "https://phira.5wyxi.com";
const MAX_CHART_BYTES: u64 = 256 << 20;

const MESSAGE_DELISTED: &str = "谱面已下架";
const MESSAGE_SERVER: &str = "无法连接到服务器";
const MESSAGE_NETWORK: &str = "无法连接到服务器，请检查网络设置";

/// 登录后的账号信息。`token` 用于当前会话，`refresh_token` 用于下次启动后续期。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: i32,
    pub name: String,
    /// 当前会话的访问令牌，只在内存中使用，不写入账号文件。
    #[serde(skip)]
    pub token: String,
    /// 刷新令牌；下次启动时用它换取新的访问令牌。
    pub refresh_token: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginResp {
    id: i32,
    token: String,
    refresh_token: String,
}

#[derive(Deserialize)]
struct UserResp {
    id: i32,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChartResp {
    id: i32,
    name: String,
    level: String,
    file: String,
}

/// 后台任务消息。
pub enum Event {
    Status(String),
    Login(Result<Account, String>),
    Download {
        result: Result<PathBuf, String>,
        account: Option<Account>,
        session_expired: bool,
    },
}

/// 后台登录。密码只在本次请求中使用，不会写入磁盘。
pub fn spawn_login(email: String, password: String) -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = login(&email, &password).map_err(|err| format!("{err:#}"));
        let _ = tx.send(Event::Login(result));
    });
    rx
}

/// 后台下载谱面。返回的账号可能已通过刷新令牌续期，UI 应重新保存。
pub fn spawn_download(id: i32, account: Option<Account>) -> Receiver<Event> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let had_account = account.is_some();
        let mut account = account;
        let result = download_chart(id, &mut account, &tx).map_err(|err| format!("{err:#}"));
        let session_expired = had_account && account.is_none();
        let _ = tx.send(Event::Download {
            result,
            account,
            session_expired,
        });
    });
    rx
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(180))
        .user_agent(concat!("ppr-render/", env!("CARGO_PKG_VERSION")))
        .build()
}

fn login(email: &str, password: &str) -> Result<Account> {
    let agent = agent();
    let body = json!({
        "email": email,
        "password": password,
        "cancelDeleteRequest": false,
        "clientVersion": concat!("ppr-render/", env!("CARGO_PKG_VERSION")),
        "clientFlavor": "none",
    });
    let resp = agent
        .post(&format!("{API_URL}/login"))
        .send_json(body)
        .map_err(request_error)?;
    let login: LoginResp = resp.into_json().context("登录响应格式无效")?;
    let user = fetch_me(&agent, &login.token).context("登录成功，但无法读取账号信息")?;
    Ok(Account {
        id: user.id,
        name: user.name,
        token: login.token,
        refresh_token: login.refresh_token,
    })
}

struct RefreshError {
    /// 刷新令牌本身被服务器拒绝；为 false 时只是网络或格式错误，不应删除本地账号。
    invalid: bool,
    /// 可以直接展示给用户的网络 / 服务器错误。
    friendly: Option<&'static str>,
    error: anyhow::Error,
}

impl RefreshError {
    fn other(error: anyhow::Error) -> Self {
        Self {
            invalid: false,
            friendly: None,
            error,
        }
    }

    fn from_ureq(err: ureq::Error) -> Self {
        let friendly = match &err {
            ureq::Error::Status(code, _) if *code >= 500 => Some(MESSAGE_SERVER),
            ureq::Error::Transport(_) => Some(MESSAGE_NETWORK),
            _ => None,
        };
        Self {
            invalid: false,
            friendly,
            error: request_error(err),
        }
    }
}

fn refresh(agent: &ureq::Agent, account: &Account) -> Result<Account, RefreshError> {
    let body = json!({
        "refreshToken": account.refresh_token,
        "cancelDeleteRequest": false,
        "clientVersion": concat!("ppr-render/", env!("CARGO_PKG_VERSION")),
        "clientFlavor": "none",
    });
    let resp = match agent.post(&format!("{API_URL}/login")).send_json(body) {
        Ok(resp) => resp,
        Err(ureq::Error::Status(401, _)) => {
            return Err(RefreshError {
                invalid: true,
                friendly: None,
                error: anyhow!("刷新登录状态失败（HTTP 401）"),
            });
        }
        Err(err) => return Err(RefreshError::from_ureq(err)),
    };
    let login: LoginResp = resp.into_json().map_err(|err| {
        RefreshError::other(anyhow::Error::new(err).context("刷新登录响应格式无效"))
    })?;
    Ok(Account {
        id: login.id,
        name: account.name.clone(),
        token: login.token,
        refresh_token: login.refresh_token,
    })
}

fn fetch_me(agent: &ureq::Agent, token: &str) -> Result<UserResp> {
    let resp = agent
        .get(&format!("{API_URL}/me"))
        .set("Accept", "application/json")
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(request_error)?;
    resp.into_json().context("账号信息格式无效")
}

/// 获取 JSON 接口；遇到 401 时自动用刷新令牌续期一次。
fn get_api(
    agent: &ureq::Agent,
    path: &str,
    account: &mut Option<Account>,
) -> Result<ureq::Response> {
    let mut tried_refresh = false;
    loop {
        let mut req = agent
            .get(&format!("{API_URL}{path}"))
            .set("Accept", "application/json");
        if let Some(account) = account.as_ref() {
            req = req.set("Authorization", &format!("Bearer {}", account.token));
        }
        match req.call() {
            Ok(resp) => return Ok(resp),
            Err(ureq::Error::Status(401, _)) if !tried_refresh && account.is_some() => {
                let current = account.as_ref().expect("checked above").clone();
                match refresh(agent, &current) {
                    Ok(updated) => {
                        *account = Some(updated);
                        tried_refresh = true;
                    }
                    Err(err) => {
                        if err.invalid {
                            *account = None;
                        }
                        if let Some(message) = err.friendly {
                            return Err(anyhow!(message));
                        }
                        let context = if err.invalid {
                            "登录状态已失效，请重新登录"
                        } else {
                            "刷新登录状态失败"
                        };
                        return Err(err.error.context(context));
                    }
                }
            }
            Err(err) => return Err(download_request_error(err)),
        }
    }
}

fn download_chart(id: i32, account: &mut Option<Account>, tx: &Sender<Event>) -> Result<PathBuf> {
    if id <= 0 {
        bail!("谱面 ID 必须是正整数");
    }
    let agent = agent();
    // 账号文件里不保存访问令牌；从磁盘恢复账号后先用刷新令牌续期一次。
    if account.as_ref().is_some_and(|it| it.token.is_empty()) {
        let current = account.as_ref().expect("checked above").clone();
        match refresh(&agent, &current) {
            Ok(updated) => *account = Some(updated),
            Err(err) if err.invalid => {
                // 登录过期不应妨碍公开谱面下载；后续请求会以未登录身份继续。
                *account = None;
                status(tx, "登录状态已失效，将以未登录身份下载");
            }
            Err(err) => {
                if let Some(message) = err.friendly {
                    return Err(anyhow!(message));
                }
                return Err(err.error.context("刷新登录状态失败"));
            }
        }
    }
    status(tx, "正在获取谱面信息…");
    let resp = get_api(&agent, &format!("/chart/{id}"), account)?;
    let chart: ChartResp = resp.into_json().context("谱面信息格式无效")?;
    if chart.id != id {
        bail!("服务器返回了其他谱面（ID {}）", chart.id);
    }
    if !chart.file.starts_with("http://") && !chart.file.starts_with("https://") {
        bail!("谱面下载地址无效");
    }

    status(tx, &format!("正在下载「{}」…", chart.name));
    let mut req = agent
        .get(&chart.file)
        .set("Accept", "application/octet-stream");
    // 只向 Phira 官方域名发送令牌；CDN 跳转地址通常不需要，也不应泄露账号凭据。
    if chart.file.starts_with(API_URL) {
        if let Some(account) = account.as_ref() {
            req = req.set("Authorization", &format!("Bearer {}", account.token));
        }
    }
    let resp = req.call().map_err(download_request_error)?;
    let total = resp
        .header("Content-Length")
        .and_then(|it| it.parse::<u64>().ok());
    if total.is_some_and(|total| total > MAX_CHART_BYTES) {
        bail!("谱面压缩包过大（超过 256 MiB）");
    }
    let mut reader = resp.into_reader();
    let mut bytes = Vec::with_capacity(total.unwrap_or(0).min(MAX_CHART_BYTES) as usize);
    let mut buffer = [0u8; 64 * 1024];
    let mut last_report = Instant::now();
    loop {
        let read = reader.read(&mut buffer).context("读取谱面压缩包失败")?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() as u64 > MAX_CHART_BYTES {
            bail!("谱面压缩包过大（超过 256 MiB）");
        }
        if last_report.elapsed() >= Duration::from_millis(120) {
            let message = match total {
                Some(total) if total > 0 => format!(
                    "正在下载「{}」… {:.0}%",
                    chart.name,
                    bytes.len() as f64 / total as f64 * 100.
                ),
                _ => format!("正在下载「{}」… {} KiB", chart.name, bytes.len() / 1024),
            };
            status(tx, &message);
            last_report = Instant::now();
        }
    }
    if bytes.is_empty() {
        bail!("下载到的谱面压缩包为空");
    }

    status(tx, "正在解压谱面…");
    let root = charts_dir()?;
    fs::create_dir_all(&root).with_context(|| format!("无法创建 {}", root.display()))?;
    let target = root.join(id.to_string());
    let temp = tempfile::Builder::new()
        .prefix(".download-")
        .tempdir_in(&root)
        .context("无法创建临时目录")?;
    let dir = prpr::dir::Dir::new(temp.path())?;
    prpr::ext::unzip_into(Cursor::new(&bytes), &dir, false)
        .with_context(|| format!("无法解压谱面「{}」", chart.name))?;
    if !is_chart_dir(temp.path()) {
        bail!("下载的压缩包中没有 info.yml / info.txt / info.csv");
    }

    if target.exists() {
        if target.is_dir() {
            fs::remove_dir_all(&target)
        } else {
            fs::remove_file(&target)
        }
        .with_context(|| format!("无法替换旧谱面 {}", target.display()))?;
    }
    let temp_path = temp.keep();
    fs::rename(&temp_path, &target)
        .with_context(|| format!("无法保存谱面到 {}", target.display()))?;
    status(
        tx,
        &format!("已下载「{}」 {}", chart.name, level_suffix(&chart.level)),
    );
    Ok(target)
}

fn level_suffix(level: &str) -> String {
    if level.is_empty() {
        String::new()
    } else {
        format!("（{level}）")
    }
}

fn status(tx: &Sender<Event>, message: &str) {
    let _ = tx.send(Event::Status(message.to_owned()));
}

fn request_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let text = resp.into_string().unwrap_or_default();
            let detail = serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .and_then(|json| {
                    json.get("error")
                        .and_then(|it| it.as_str())
                        .map(str::to_owned)
                })
                .filter(|it| !it.is_empty())
                .unwrap_or(text);
            if detail.is_empty() {
                anyhow!("Phira 请求失败（HTTP {code}）")
            } else {
                anyhow!("Phira 请求失败（HTTP {code}）：{detail}")
            }
        }
        ureq::Error::Transport(err) => anyhow!("网络请求失败：{err}"),
    }
}

/// 下载谱面时的错误分类：把 404 / 5xx / 断网转换成明确的用户提示。
fn download_request_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => match status_message(code) {
            Some(message) => anyhow!(message),
            None => request_error(ureq::Error::Status(code, resp)),
        },
        ureq::Error::Transport(_) => anyhow!(MESSAGE_NETWORK),
    }
}

fn status_message(code: u16) -> Option<&'static str> {
    match code {
        404 | 410 => Some(MESSAGE_DELISTED),
        code if code >= 500 => Some(MESSAGE_SERVER),
        _ => None,
    }
}

fn is_chart_dir(path: &Path) -> bool {
    path.join("info.yml").is_file()
        || path.join("info.txt").is_file()
        || path.join("info.csv").is_file()
}

/// 在线下载与账号文件的根目录；可用 `PPR_RENDER_DATA_DIR` 覆盖。
pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PPR_RENDER_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from);
    let base = match std::env::consts::OS {
        "windows" => std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("APPDATA"))
            .map(PathBuf::from)
            .or_else(|| home.clone())
            .context("无法确定 Windows 数据目录")?,
        "macos" => home
            .clone()
            .context("无法确定用户目录")?
            .join("Library/Application Support"),
        _ => std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| home.clone().map(|home| home.join(".local/share")))
            .context("无法确定数据目录")?,
    };
    Ok(base.join("ppr-render"))
}

/// 在线谱面的缓存目录。目录本身可能尚未创建。
pub fn charts_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("charts"))
}

fn account_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("account.json"))
}

pub fn load_account() -> Option<Account> {
    let path = account_path().ok()?;
    let data = fs::read(&path).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn save_account(account: &Account) -> Result<()> {
    let path = account_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("无法创建 {}", parent.display()))?;
    }
    let data = serde_json::to_vec_pretty(account)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .with_context(|| format!("无法写入 {}", path.display()))?;
    file.write_all(&data)?;
    file.sync_all().ok();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
    }
    Ok(())
}

pub fn clear_account() {
    if let Ok(path) = account_path() {
        if let Err(err) = fs::remove_file(&path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                eprintln!("无法删除账号文件 {}：{err}", path.display());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_data_dir_is_stable() {
        let dir = charts_dir().unwrap();
        assert!(dir.ends_with("charts"));
    }

    #[test]
    fn download_error_messages_are_classified() {
        assert_eq!(status_message(404), Some("谱面已下架"));
        assert_eq!(status_message(410), Some("谱面已下架"));
        assert_eq!(status_message(500), Some("无法连接到服务器"));
        assert_eq!(status_message(503), Some("无法连接到服务器"));
        assert_eq!(status_message(400), None);
        assert_eq!(MESSAGE_NETWORK, "无法连接到服务器，请检查网络设置");
    }

    #[test]
    fn account_file_does_not_store_access_token() {
        let account = Account {
            id: 1,
            name: "tester".into(),
            token: "access-secret".into(),
            refresh_token: "refresh-secret".into(),
        };
        let json = serde_json::to_string(&account).unwrap();
        assert!(!json.contains("access-secret"));
        assert!(json.contains("refresh-secret"));

        let restored: Account = serde_json::from_str(&json).unwrap();
        assert!(restored.token.is_empty());
        assert_eq!(restored.refresh_token, "refresh-secret");
    }

    #[test]
    #[ignore = "需要网络；手动运行以验证 Phira 在线下载"]
    fn download_public_chart() {
        let (tx, rx) = mpsc::channel();
        let mut account = None;
        let path = download_chart(51121, &mut account, &tx).unwrap();
        assert!(is_chart_dir(&path));
        let mut events = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, Event::Status(_)) {
                events += 1;
            }
        }
        assert!(events >= 2);
        println!("downloaded to {}", path.display());
    }

    #[test]
    #[ignore = "需要网络；验证登录过期后仍能下载公开谱面"]
    fn download_public_chart_with_expired_login() {
        let (tx, _rx) = mpsc::channel();
        let mut account = Some(Account {
            id: 0,
            name: "expired".into(),
            token: String::new(),
            refresh_token: "invalid-refresh-token".into(),
        });
        let path = download_chart(51121, &mut account, &tx).unwrap();
        assert!(is_chart_dir(&path));
        assert!(account.is_none());
        println!("downloaded without login to {}", path.display());
    }
}
