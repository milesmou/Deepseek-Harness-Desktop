//! DeepSeek Harness Desktop —— Tauri 桌面壳。
//!
//! 启动流程:
//! 1. 打开本地 loading 页;
//! 2. 检查 `http://127.0.0.1:<port>` 是否已有 Harness 服务在运行;
//! 3. 没有则按候选命令顺序拉起服务(可用环境变量 DSH_WEB_COMMAND 自定义);
//! 4. 端口就绪后把窗口导航到 Harness Web GUI;
//! 5. 应用退出时,杀掉由本应用拉起的服务进程(保留预先存在的服务)。

#[cfg(not(feature = "bootstrap"))]
compile_error!("工程仅保留网络引导版,必须启用 bootstrap feature");

#[cfg(all(
    feature = "bootstrap",
    not(all(target_os = "windows", target_arch = "x86_64"))
))]
compile_error!("bootstrap 发布模式目前仅支持 Windows x64");

use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tauri::{AppHandle, Manager, RunEvent, Url, WebviewWindow};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

#[cfg(feature = "bootstrap")]
use std::net::ToSocketAddrs;

#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// 让子进程不弹出控制台窗口(Windows)。
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const DEFAULT_PORT: u16 = 3080;
/// 单个候选命令的启动等待上限;`npx` 首次运行需要下载包,给足时间。
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(400);
const HTTP_PROBE_TIMEOUT: Duration = Duration::from_millis(800);
const MAX_PROBE_BYTES: u64 = 256 * 1024;
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
#[cfg(feature = "bootstrap")]
const BOOTSTRAP_NODE_VERSION: &str = "24.16.0";
#[cfg(feature = "bootstrap")]
const BOOTSTRAP_NODE_ARCHIVE: &str = "node-v24.16.0-win-x64";
#[cfg(feature = "bootstrap")]
const BOOTSTRAP_NODE_OFFICIAL_URL: &str =
    "https://nodejs.org/download/release/v24.16.0/node-v24.16.0-win-x64.zip";
#[cfg(feature = "bootstrap")]
const BOOTSTRAP_NODE_CHINA_URL: &str =
    "https://npmmirror.com/mirrors/node/v24.16.0/node-v24.16.0-win-x64.zip";
#[cfg(feature = "bootstrap")]
const BOOTSTRAP_NODE_SHA256: &str =
    "edaca9bd58ec8e92037dac4e877d52f6b8f430b81c18b57e264b4e2fb111cd56";
#[cfg(feature = "bootstrap")]
const NPM_OFFICIAL_REGISTRY: &str = "https://registry.npmjs.org/";
#[cfg(feature = "bootstrap")]
const NPM_CHINA_REGISTRY: &str = "https://registry.npmmirror.com/";
#[cfg(feature = "bootstrap")]
const BOOTSTRAP_NODE_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = $env:DSH_BOOTSTRAP_ROOT
$destination = $env:DSH_BOOTSTRAP_NODE_DIR
$temporary = Join-Path $root ("node-install-" + $PID)
$zip = Join-Path $temporary 'node.zip'
New-Item -ItemType Directory -Force -Path $root | Out-Null
if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
  $downloaded = $false
  $lastError = $null
  foreach ($url in @($env:DSH_BOOTSTRAP_NODE_PRIMARY_URL, $env:DSH_BOOTSTRAP_NODE_SECONDARY_URL)) {
    try {
      if (Test-Path -LiteralPath $zip) { Remove-Item -LiteralPath $zip -Force }
      Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip
      $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
      if ($actual -ne $env:DSH_BOOTSTRAP_NODE_SHA256) {
        throw "Node archive SHA-256 mismatch from ${url}: $actual"
      }
      $downloaded = $true
      break
    } catch {
      $lastError = $_
    }
  }
  if (-not $downloaded) {
    throw "All Node download sources failed. Last error: $lastError"
  }
  Expand-Archive -LiteralPath $zip -DestinationPath $temporary
  $source = Join-Path $temporary $env:DSH_BOOTSTRAP_NODE_ARCHIVE
  if (-not (Test-Path -LiteralPath (Join-Path $source 'node.exe'))) {
    throw 'Downloaded Node archive is incomplete'
  }
  if (Test-Path -LiteralPath $destination) {
    Remove-Item -LiteralPath $destination -Recurse -Force
  }
  Move-Item -LiteralPath $source -Destination $destination
} finally {
  if (Test-Path -LiteralPath $temporary) { Remove-Item -LiteralPath $temporary -Recurse -Force }
}
"#;

/// 由本应用拉起的 Harness 服务进程。
struct ServerProcess {
    child: Mutex<Option<Child>>,
    shutting_down: AtomicBool,
}

fn port_open(port: u16) -> bool {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("valid loopback address");
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

fn response_is_harness(response: &str) -> bool {
    let status_ok = response
        .lines()
        .next()
        .is_some_and(|line| line.starts_with("HTTP/1.1 200") || line.starts_with("HTTP/1.0 200"));
    status_ok && response.contains("window.__DSH_BOOT__")
}

/// 不只检查端口，还确认首页是 Harness 服务端注入后的启动页。
fn harness_ready(port: u16) -> bool {
    let addr: SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .expect("valid loopback address");
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, HTTP_PROBE_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(HTTP_PROBE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(HTTP_PROBE_TIMEOUT));
    let request = format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: text/html\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut bytes = Vec::new();
    if stream
        .take(MAX_PROBE_BYTES)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return false;
    }
    response_is_harness(&String::from_utf8_lossy(&bytes))
}

/// 打开诊断日志(应用数据目录/dsh-server.log),失败时静默降级。
fn open_log(handle: &AppHandle) -> Option<File> {
    let dir = handle.path().app_data_dir().ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("dsh-server.log");
    if path
        .metadata()
        .map(|m| m.len() >= MAX_LOG_BYTES)
        .unwrap_or(false)
    {
        let rotated = dir.join("dsh-server.log.1");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(&path, rotated);
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn log_line(log: &Option<File>, msg: &str) {
    let Some(mut f) = log.as_ref().and_then(|f| f.try_clone().ok()) else {
        return;
    };
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(f, "[{secs}] {msg}");
}

/// 子进程 stdio:stdout/stderr 写诊断日志,stdin 置空。
fn child_stdio(log: &Option<File>) -> (Stdio, Stdio, Stdio) {
    match log {
        Some(f) => (
            Stdio::null(),
            f.try_clone()
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
            f.try_clone()
                .map(Stdio::from)
                .unwrap_or_else(|_| Stdio::null()),
        ),
        None => (Stdio::null(), Stdio::null(), Stdio::null()),
    }
}

fn set_loading_progress(window: &WebviewWindow, progress: u8, message: &str) {
    let escaped = serde_json::to_string(message).unwrap_or_default();
    let _ = window.eval(format!(
        "window.__setProgress && window.__setProgress({progress}, {escaped})"
    ));
}

fn set_version_title(window: &WebviewWindow, dsh_version: Option<&str>) {
    let title = match dsh_version.filter(|version| !version.is_empty() && *version != "unbundled") {
        Some(version) => format!("DeepSeek Harness {version}"),
        None => "DeepSeek Harness".to_string(),
    };
    let _ = window.set_title(&title);
}

#[cfg(feature = "bootstrap")]
fn projected_setup_progress(start: u8, end: u8, elapsed_seconds: u64) -> u8 {
    let span = u64::from(end.saturating_sub(start));
    let advanced =
        span.saturating_sub(span.saturating_mul(15) / elapsed_seconds.saturating_add(15));
    start
        .saturating_add(u8::try_from(advanced).unwrap_or(u8::MAX))
        .min(end.saturating_sub(1))
}

#[cfg(feature = "bootstrap")]
#[derive(Clone, Copy)]
struct SourceChoice {
    primary_name: &'static str,
    primary_url: &'static str,
    secondary_name: &'static str,
    secondary_url: &'static str,
}

#[cfg(feature = "bootstrap")]
impl SourceChoice {
    fn swapped(self) -> Self {
        Self {
            primary_name: self.secondary_name,
            primary_url: self.secondary_url,
            secondary_name: self.primary_name,
            secondary_url: self.primary_url,
        }
    }
}

#[cfg(feature = "bootstrap")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DshVersionDecision {
    Install,
    Update,
    Current,
    UseExistingOffline,
}

#[cfg(feature = "bootstrap")]
fn decide_dsh_version(
    has_existing: bool,
    installed: Option<&str>,
    latest: Option<&str>,
) -> DshVersionDecision {
    if !has_existing {
        return DshVersionDecision::Install;
    }
    match (installed, latest) {
        (Some(installed), Some(latest)) if installed == latest => DshVersionDecision::Current,
        (_, Some(_)) => DshVersionDecision::Update,
        (_, None) => DshVersionDecision::UseExistingOffline,
    }
}

#[cfg(feature = "bootstrap")]
fn https_host(url: &str) -> Option<&str> {
    url.strip_prefix("https://")?
        .split('/')
        .next()
        .filter(|host| !host.is_empty())
}

#[cfg(feature = "bootstrap")]
fn measure_source_latency(url: &str) -> Option<Duration> {
    let host = https_host(url)?;
    let started = Instant::now();
    let addresses = (host, 443).to_socket_addrs().ok()?;
    for address in addresses {
        if TcpStream::connect_timeout(&address, Duration::from_millis(1800)).is_ok() {
            return Some(started.elapsed());
        }
    }
    None
}

#[cfg(feature = "bootstrap")]
fn prefer_china_source(china: Option<Duration>, official: Option<Duration>) -> bool {
    match (china, official) {
        (Some(china), Some(official)) => china <= official,
        (Some(_), None) => true,
        _ => false,
    }
}

#[cfg(feature = "bootstrap")]
fn latency_text(latency: Option<Duration>) -> String {
    latency
        .map(|value| format!("{} ms", value.as_millis()))
        .unwrap_or_else(|| "不可用".to_string())
}

#[cfg(feature = "bootstrap")]
fn select_fastest_source(
    window: &WebviewWindow,
    progress: u8,
    kind: &str,
    china_url: &'static str,
    official_url: &'static str,
    log: &Option<File>,
) -> SourceChoice {
    set_loading_progress(
        window,
        progress,
        &format!("正在测试 {kind} 国内源和官网源延迟…"),
    );
    let (china_latency, official_latency) = thread::scope(|scope| {
        let china = scope.spawn(|| measure_source_latency(china_url));
        let official = scope.spawn(|| measure_source_latency(official_url));
        (
            china.join().unwrap_or(None),
            official.join().unwrap_or(None),
        )
    });
    let china_first = prefer_china_source(china_latency, official_latency);
    let (primary_name, primary_url, secondary_name, secondary_url) = if china_first {
        ("中国大陆源", china_url, "官网源", official_url)
    } else {
        ("官网源", official_url, "中国大陆源", china_url)
    };
    let summary = format!(
        "{kind} 源测速:中国大陆 {}，官网 {}；选择 {primary_name}",
        latency_text(china_latency),
        latency_text(official_latency)
    );
    log_line(log, &summary);
    set_loading_progress(window, progress.saturating_add(2), &summary);
    SourceChoice {
        primary_name,
        primary_url,
        secondary_name,
        secondary_url,
    }
}

#[cfg(feature = "bootstrap")]
fn command_works(program: &str, argument: &str) -> bool {
    let mut command = Command::new(program);
    command.arg(argument);
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// 运行下载/npm 等准备命令。准备进程也纳入统一生命周期管理，退出应用时会终止进程树。
#[cfg(feature = "bootstrap")]
fn run_managed_setup(
    handle: &AppHandle,
    window: &WebviewWindow,
    command: &mut Command,
    label: &str,
    progress_start: u8,
    progress_end: u8,
    log: &Option<File>,
) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
    let (stdin, stdout, stderr) = child_stdio(log);
    command.stdin(stdin).stdout(stdout).stderr(stderr);
    let child = command
        .spawn()
        .map_err(|e| format!("无法启动{label}: {e}"))?;
    register_child(handle, child)?;
    let started = Instant::now();
    let mut last_reported_second = u64::MAX;
    set_loading_progress(window, progress_start, &format!("{label}…"));

    loop {
        let state = handle.state::<ServerProcess>();
        if state.shutting_down.load(Ordering::SeqCst) {
            stop_managed_child(handle);
            return Err("应用正在退出".to_string());
        }
        let status = {
            let mut guard = state
                .child
                .lock()
                .map_err(|_| "无法锁定准备进程状态".to_string())?;
            let Some(child) = guard.as_mut() else {
                return Err(format!("{label}进程意外丢失"));
            };
            match child.try_wait() {
                Ok(Some(status)) => {
                    guard.take();
                    Some(status)
                }
                Ok(None) => None,
                Err(error) => {
                    drop(guard);
                    stop_managed_child(handle);
                    return Err(format!("无法查询{label}进程状态: {error}"));
                }
            }
        };
        if let Some(status) = status {
            let result = status
                .success()
                .then_some(())
                .ok_or_else(|| format!("{label}失败: {status}"));
            if result.is_ok() {
                set_loading_progress(window, progress_end, &format!("{label}完成"));
            }
            return result;
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed != last_reported_second {
            last_reported_second = elapsed;
            let progress = projected_setup_progress(progress_start, progress_end, elapsed);
            set_loading_progress(window, progress, &format!("{label}… 已用时 {elapsed} 秒"));
        }
        thread::sleep(Duration::from_millis(150));
    }
}

/// 运行短暂的版本查询命令并捕获标准输出，同时保留退出取消和进度反馈。
#[cfg(feature = "bootstrap")]
fn run_managed_capture(
    handle: &AppHandle,
    window: &WebviewWindow,
    command: &mut Command,
    label: &str,
    progress_start: u8,
    progress_end: u8,
    log: &Option<File>,
) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
    command.stdin(Stdio::null()).stdout(Stdio::piped());
    command.stderr(match log {
        Some(file) => file
            .try_clone()
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null()),
        None => Stdio::null(),
    });
    let child = command
        .spawn()
        .map_err(|e| format!("无法启动{label}: {e}"))?;
    register_child(handle, child)?;
    let started = Instant::now();
    let mut last_reported_second = u64::MAX;
    set_loading_progress(window, progress_start, &format!("{label}…"));

    loop {
        let state = handle.state::<ServerProcess>();
        if state.shutting_down.load(Ordering::SeqCst) {
            stop_managed_child(handle);
            return Err("应用正在退出".to_string());
        }
        let finished = {
            let mut guard = state
                .child
                .lock()
                .map_err(|_| "无法锁定版本查询进程状态".to_string())?;
            let Some(child) = guard.as_mut() else {
                return Err(format!("{label}进程意外丢失"));
            };
            match child.try_wait() {
                Ok(Some(status)) => guard.take().map(|child| (child, status)),
                Ok(None) => None,
                Err(error) => {
                    drop(guard);
                    stop_managed_child(handle);
                    return Err(format!("无法查询{label}进程状态: {error}"));
                }
            }
        };
        if let Some((mut child, status)) = finished {
            let mut output = String::new();
            if let Some(mut stdout) = child.stdout.take() {
                stdout
                    .read_to_string(&mut output)
                    .map_err(|e| format!("无法读取{label}结果: {e}"))?;
            }
            if !status.success() {
                return Err(format!("{label}失败: {status}"));
            }
            set_loading_progress(window, progress_end, &format!("{label}完成"));
            return Ok(output.trim().to_string());
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed != last_reported_second {
            last_reported_second = elapsed;
            let progress = projected_setup_progress(progress_start, progress_end, elapsed);
            set_loading_progress(window, progress, &format!("{label}… 已用时 {elapsed} 秒"));
        }
        thread::sleep(Duration::from_millis(150));
    }
}

/// 通过 `cmd /C` 执行命令串,不弹控制台窗口。
fn spawn_cmdline(cmdline: &str, log: &Option<File>) -> std::io::Result<Child> {
    let mut cmd = Command::new("cmd");
    cmd.args(["/C", cmdline]);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let (i, o, e) = child_stdio(log);
    cmd.stdin(i).stdout(o).stderr(e);
    cmd.spawn()
}

/// 结束整个进程树(Windows 用 taskkill /T;其他平台直接 kill)。
fn kill_tree(child: &mut Child) {
    #[cfg(target_os = "windows")]
    {
        let mut k = Command::new("taskkill");
        k.args(["/PID", &child.id().to_string(), "/T", "/F"]);
        k.creation_flags(CREATE_NO_WINDOW);
        let _ = k.status();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(feature = "bootstrap")]
fn prepend_node_to_path(command: &mut Command, node: &std::path::Path) -> Result<(), String> {
    let Some(node_dir) = node.parent().filter(|path| !path.as_os_str().is_empty()) else {
        return Ok(());
    };
    let mut paths = vec![node_dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    let joined = std::env::join_paths(paths).map_err(|e| format!("无法构造 Node PATH: {e}"))?;
    command.env("PATH", joined);
    Ok(())
}

/// 返回可配套使用的 node 与 npm。系统环境不完整时下载并校验官方便携版 Node。
#[cfg(feature = "bootstrap")]
fn ensure_bootstrap_node(
    handle: &AppHandle,
    window: &WebviewWindow,
    log: &Option<File>,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    if command_works("node", "--version") && command_works("npm.cmd", "--version") {
        log_line(log, "检测到系统 Node 与 npm,直接使用");
        set_loading_progress(window, 20, "已检测到系统 Node 与 npm");
        return Ok(("node".into(), "npm.cmd".into()));
    }

    let root = handle
        .path()
        .app_data_dir()
        .map_err(|e| format!("无法定位应用数据目录: {e}"))?
        .join("bootstrap");
    let node_dir = root.join(format!("node-v{BOOTSTRAP_NODE_VERSION}-win-x64"));
    let node = node_dir.join("node.exe");
    let npm = node_dir.join("npm.cmd");
    if node.is_file() && npm.is_file() {
        log_line(log, &format!("使用已安装的便携 Node: {}", node.display()));
        set_loading_progress(window, 45, "便携 Node 环境已就绪");
        return Ok((node, npm));
    }

    let sources = select_fastest_source(
        window,
        8,
        "Node 下载",
        BOOTSTRAP_NODE_CHINA_URL,
        BOOTSTRAP_NODE_OFFICIAL_URL,
        log,
    );
    log_line(
        log,
        &format!(
            "Node 下载顺序:{} -> {}",
            sources.primary_name, sources.secondary_name
        ),
    );
    set_loading_progress(
        window,
        12,
        &format!(
            "未检测到完整 Node 环境,正通过{}安装 Node {BOOTSTRAP_NODE_VERSION}…",
            sources.primary_name
        ),
    );
    log_line(log, &format!("下载并安装 Node {BOOTSTRAP_NODE_VERSION}"));
    std::fs::create_dir_all(&root).map_err(|e| format!("无法创建引导目录: {e}"))?;
    let mut command = Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            BOOTSTRAP_NODE_SCRIPT,
        ])
        .env("DSH_BOOTSTRAP_ROOT", &root)
        .env("DSH_BOOTSTRAP_NODE_DIR", &node_dir)
        .env("DSH_BOOTSTRAP_NODE_PRIMARY_URL", sources.primary_url)
        .env("DSH_BOOTSTRAP_NODE_SECONDARY_URL", sources.secondary_url)
        .env("DSH_BOOTSTRAP_NODE_SHA256", BOOTSTRAP_NODE_SHA256)
        .env("DSH_BOOTSTRAP_NODE_ARCHIVE", BOOTSTRAP_NODE_ARCHIVE);
    run_managed_setup(handle, window, &mut command, "Node 下载与安装", 12, 45, log)?;
    if !node.is_file() || !npm.is_file() {
        return Err("Node 安装完成但 node.exe 或 npm.cmd 缺失".to_string());
    }
    Ok((node, npm))
}

fn read_npm_package_version(package_json: &std::path::Path) -> Option<String> {
    let bytes = std::fs::read(package_json).ok()?;
    let package: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    package.get("version")?.as_str().map(ToOwned::to_owned)
}

#[cfg(feature = "bootstrap")]
fn parse_npm_view_version(output: &str) -> Result<String, String> {
    serde_json::from_str::<String>(output)
        .or_else(|_| {
            let value = output.trim().trim_matches('"');
            (!value.is_empty()).then(|| value.to_string()).ok_or(())
        })
        .map_err(|_| format!("npm 返回了无效版本: {output}"))
}

#[cfg(feature = "bootstrap")]
fn has_dsh_runtime(root: &std::path::Path) -> bool {
    root.join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js")
        .is_file()
}

#[cfg(feature = "bootstrap")]
fn select_npx_dsh_root(npx_root: &std::path::Path) -> std::path::PathBuf {
    let preferred = npx_root.join("deepseek-harness-desktop");
    if has_dsh_runtime(&preferred) {
        return preferred;
    }
    std::fs::read_dir(npx_root)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| {
            let path = entry.path();
            has_dsh_runtime(&path).then(|| {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH);
                (modified, path)
            })
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
        .unwrap_or(preferred)
}

/// 网络引导版:确保 Node 可用,用 npm 安装/更新 dsh,再启动 Web 服务。
#[cfg(feature = "bootstrap")]
fn spawn_bootstrap_server(
    handle: &AppHandle,
    window: &WebviewWindow,
    port: u16,
    log: &Option<File>,
) -> Result<(Child, String), String> {
    let (node, npm) = ensure_bootstrap_node(handle, window, log)?;
    let npx_root = handle
        .path()
        .local_data_dir()
        .map_err(|e| format!("无法定位本地应用数据目录: {e}"))?
        .join("npm-cache")
        .join("_npx");
    std::fs::create_dir_all(&npx_root).map_err(|e| format!("无法创建 npm _npx 目录: {e}"))?;
    let root = select_npx_dsh_root(&npx_root);
    std::fs::create_dir_all(&root).map_err(|e| format!("无法创建 dsh 目录: {e}"))?;
    log_line(log, &format!("dsh npm 目录: {}", root.display()));
    let bin = root
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    let package_json = root
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    let had_existing = bin.is_file();
    let installed_version = read_npm_package_version(&package_json);
    let mut npm_sources = select_fastest_source(
        window,
        46,
        "npm registry",
        NPM_CHINA_REGISTRY,
        NPM_OFFICIAL_REGISTRY,
        log,
    );

    let make_version_query = |registry: &str| -> Result<Command, String> {
        let mut command = Command::new(&npm);
        command
            .args([
                "view",
                "@deepseek-ai/dsh",
                "version",
                "--json",
                "--loglevel=error",
            ])
            .args(["--registry", registry])
            .env("NPM_CONFIG_REGISTRY", registry);
        prepend_node_to_path(&mut command, &node)?;
        Ok(command)
    };

    let primary_query_label = format!("dsh 版本检查（{}）", npm_sources.primary_name);
    let mut primary_query = make_version_query(npm_sources.primary_url)?;
    let primary_result = run_managed_capture(
        handle,
        window,
        &mut primary_query,
        &primary_query_label,
        50,
        57,
        log,
    )
    .and_then(|output| parse_npm_view_version(&output));
    let latest_version = match primary_result {
        Ok(version) => Some(version),
        Err(primary_error) => {
            log_line(
                log,
                &format!(
                    "{}版本查询失败:{primary_error};切换到{}",
                    npm_sources.primary_name, npm_sources.secondary_name
                ),
            );
            let secondary_query_label = format!("dsh 版本检查（{}）", npm_sources.secondary_name);
            let mut secondary_query = make_version_query(npm_sources.secondary_url)?;
            match run_managed_capture(
                handle,
                window,
                &mut secondary_query,
                &secondary_query_label,
                54,
                59,
                log,
            )
            .and_then(|output| parse_npm_view_version(&output))
            {
                Ok(version) => {
                    npm_sources = npm_sources.swapped();
                    Some(version)
                }
                Err(secondary_error) => {
                    log_line(
                        log,
                        &format!(
                            "两个 npm 源均无法查询版本;{}: {primary_error};{}: {secondary_error}",
                            npm_sources.primary_name, npm_sources.secondary_name
                        ),
                    );
                    None
                }
            }
        }
    };
    log_line(
        log,
        &format!(
            "dsh 版本:本地={},最新={}",
            installed_version.as_deref().unwrap_or("未安装"),
            latest_version.as_deref().unwrap_or("查询失败")
        ),
    );
    let version_decision = decide_dsh_version(
        had_existing,
        installed_version.as_deref(),
        latest_version.as_deref(),
    );

    let make_install = |registry: &str| -> Result<Command, String> {
        let mut command = Command::new(&npm);
        command
            .args(["install", "--prefix"])
            .arg(&root)
            .args([
                "@deepseek-ai/dsh@latest",
                "--omit=dev",
                "--no-audit",
                "--no-fund",
                "--loglevel=warn",
            ])
            .env("NPM_CONFIG_REGISTRY", registry);
        prepend_node_to_path(&mut command, &node)?;
        Ok(command)
    };

    let final_result = match version_decision {
        DshVersionDecision::Current => {
            let version = installed_version.as_deref().unwrap_or("未知");
            log_line(log, &format!("dsh {version} 已是最新版,跳过 npm install"));
            set_loading_progress(
                window,
                90,
                &format!("DeepSeek Harness {version} 已是最新版"),
            );
            Ok(())
        }
        DshVersionDecision::UseExistingOffline => {
            let version = installed_version.as_deref().unwrap_or("未知");
            log_line(log, &format!("无法查询更新,直接使用本地 dsh {version}"));
            set_loading_progress(
                window,
                90,
                &format!("无法检查更新,正在使用本地版本 {version}…"),
            );
            Ok(())
        }
        DshVersionDecision::Install | DshVersionDecision::Update => {
            let installing = version_decision == DshVersionDecision::Install;
            let action = if installing { "安装" } else { "更新" };
            set_loading_progress(
                window,
                60,
                &format!(
                    "正在通过{}{action} DeepSeek Harness…",
                    npm_sources.primary_name
                ),
            );
            log_line(
                log,
                &format!(
                    "使用 npm {action} @deepseek-ai/dsh@latest（{} -> {}）",
                    installed_version.as_deref().unwrap_or("未安装"),
                    latest_version.as_deref().unwrap_or("最新版"),
                ),
            );
            let primary_label = format!("dsh {action}（{}）", npm_sources.primary_name);
            let mut install = make_install(npm_sources.primary_url)?;
            let install_result =
                run_managed_setup(handle, window, &mut install, &primary_label, 60, 88, log);
            if let Err(primary_error) = install_result {
                log_line(
                    log,
                    &format!(
                        "{}失败:{primary_error};切换到{}",
                        npm_sources.primary_name, npm_sources.secondary_name
                    ),
                );
                set_loading_progress(
                    window,
                    72,
                    &format!(
                        "{}失败,正在切换{}重试…",
                        npm_sources.primary_name, npm_sources.secondary_name
                    ),
                );
                let secondary_label = format!("dsh {action}（{}）", npm_sources.secondary_name);
                let mut retry = make_install(npm_sources.secondary_url)?;
                run_managed_setup(handle, window, &mut retry, &secondary_label, 72, 90, log)
                    .map_err(|secondary_error| {
                        format!(
                            "两个 npm 源均失败；{}: {primary_error}；{}: {secondary_error}",
                            npm_sources.primary_name, npm_sources.secondary_name
                        )
                    })
            } else {
                set_loading_progress(window, 90, &format!("dsh {action}完成"));
                Ok(())
            }
        }
    };
    if let Err(error) = final_result {
        if !had_existing || !bin.is_file() {
            return Err(error);
        }
        log_line(log, &format!("dsh 更新失败,继续使用已安装版本: {error}"));
        set_loading_progress(window, 90, "两个源更新均失败,正在使用已安装的版本…");
    }
    if !bin.is_file() {
        return Err("npm 完成后未找到 @deepseek-ai/dsh/lib/bin.js".to_string());
    }
    let active_version = read_npm_package_version(&package_json);
    set_version_title(window, active_version.as_deref());

    let mut command = Command::new(&node);
    command
        .arg(&bin)
        .args(["web", "--port", &port.to_string()])
        .current_dir(&root);
    prepend_node_to_path(&mut command, &node)?;
    #[cfg(target_os = "windows")]
    command.creation_flags(CREATE_NO_WINDOW);
    let (stdin, stdout, stderr) = child_stdio(log);
    command.stdin(stdin).stdout(stdout).stderr(stderr);
    let child = command
        .spawn()
        .map_err(|e| format!("无法启动网络安装的 dsh: {e}"))?;
    set_loading_progress(window, 94, "DeepSeek Harness 已就绪,正在启动服务…");

    Ok((
        child,
        format!("{} {} web --port {port}", node.display(), bin.display()),
    ))
}

fn register_child(handle: &AppHandle, mut child: Child) -> Result<(), String> {
    let state = handle.state::<ServerProcess>();
    if state.shutting_down.load(Ordering::SeqCst) {
        kill_tree(&mut child);
        return Err("应用正在退出".to_string());
    }
    let mut guard = state
        .child
        .lock()
        .map_err(|_| "无法锁定子进程状态".to_string())?;
    if state.shutting_down.load(Ordering::SeqCst) {
        drop(guard);
        kill_tree(&mut child);
        return Err("应用正在退出".to_string());
    }
    if guard.is_some() {
        drop(guard);
        kill_tree(&mut child);
        return Err("已有受管 Harness 子进程".to_string());
    }
    *guard = Some(child);
    Ok(())
}

fn stop_managed_child(handle: &AppHandle) {
    let child = handle
        .try_state::<ServerProcess>()
        .and_then(|state| state.child.lock().ok()?.take());
    if let Some(mut child) = child {
        kill_tree(&mut child);
    }
}

fn wait_for_harness(handle: &AppHandle, port: u16, timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let state = handle.state::<ServerProcess>();
        if state.shutting_down.load(Ordering::SeqCst) {
            return Err("应用正在退出".to_string());
        }
        {
            let mut guard = state
                .child
                .lock()
                .map_err(|_| "无法锁定子进程状态".to_string())?;
            let Some(child) = guard.as_mut() else {
                return Err("受管 Harness 子进程不存在".to_string());
            };
            if let Some(status) = child
                .try_wait()
                .map_err(|e| format!("无法查询 Harness 子进程状态: {e}"))?
            {
                guard.take();
                return Err(format!("Harness 子进程提前退出: {status}"));
            }
        }
        if harness_ready(port) {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(format!("端口 {port} 未出现有效的 Harness Web 服务"))
}

fn launch_and_wait(
    handle: &AppHandle,
    child: Child,
    description: &str,
    port: u16,
    log: &Option<File>,
) -> Result<(), String> {
    register_child(handle, child)?;
    log_line(
        log,
        &format!(
            "等待 Harness 服务 {port} 就绪(最多 {} 秒)…",
            STARTUP_TIMEOUT.as_secs()
        ),
    );
    match wait_for_harness(handle, port, STARTUP_TIMEOUT) {
        Ok(()) => Ok(()),
        Err(reason) => {
            stop_managed_child(handle);
            Err(format!("{description}: {reason}"))
        }
    }
}

fn start_server(
    handle: &AppHandle,
    window: &WebviewWindow,
    port: u16,
    log: &Option<File>,
) -> Result<(), String> {
    // 1) 环境变量 DSH_WEB_COMMAND:最高优先级
    if let Ok(custom) = std::env::var("DSH_WEB_COMMAND") {
        let custom = custom.trim().to_string();
        if !custom.is_empty() {
            log_line(log, &format!("尝试自定义命令: {custom}"));
            match spawn_cmdline(&custom, log) {
                Ok(child) => return launch_and_wait(handle, child, &custom, port, log),
                Err(e) => return Err(format!("自定义命令启动失败: {custom} ({e})")),
            }
        }
    }

    // 2) 网络引导:安装 Node(若缺失),检查 dsh 版本并按需安装或更新。
    let (child, what) = spawn_bootstrap_server(handle, window, port, log)?;
    launch_and_wait(handle, child, &what, port, log)
}

fn report_startup_error(
    handle: &AppHandle,
    window: &WebviewWindow,
    port: u16,
    reason: &str,
    log: &Option<File>,
) {
    log_line(log, &format!("启动失败: {reason}"));
    let msg = format!(
        "无法启动 DeepSeek Harness 服务(端口 {port} 未就绪)。\n\n{reason}\n\n\
         请确认运行环境和网络可用，也可设置环境变量 DSH_WEB_COMMAND 指向自定义启动命令后重新打开应用。"
    );
    let escaped = serde_json::to_string(&msg).unwrap_or_default();
    let _ = window.eval(format!(
        "window.__showError && window.__showError({escaped})"
    ));
    handle
        .dialog()
        .message(msg)
        .title("DeepSeek Harness Desktop")
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let port: u16 = std::env::var("DSH_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let app_url: Url = format!("http://127.0.0.1:{port}/")
        .parse()
        .expect("valid loopback app url");

    let app = tauri::Builder::default()
        // 必须最先注册，避免两个实例并发安装或争抢同一端口。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.show();
                let _ = window.unminimize();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(ServerProcess {
            child: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
        })
        .setup(move |app| {
            let handle: AppHandle = app.handle().clone();
            let Some(window) = app.get_webview_window("main") else {
                return Ok(());
            };
            set_version_title(&window, None);
            let log = open_log(&handle);
            log_line(&log, &format!("启动检查:目标端口 {port}"));

            // 启动检查放在独立线程里,避免阻塞主线程(loading 页可以正常渲染)。
            thread::spawn(move || {
                set_loading_progress(&window, 5, "正在检查 DeepSeek Harness 运行环境…");
                if harness_ready(port) {
                    // 已有服务在跑(例如本机已开的 GUI),直接接入。
                    log_line(&log, &format!("端口 {port} 已有 Harness 服务,直接接入"));
                    set_loading_progress(&window, 100, "已连接到 DeepSeek Harness");
                    let _ = window.navigate(app_url.clone());
                    return;
                }
                if port_open(port) {
                    report_startup_error(
                        &handle,
                        &window,
                        port,
                        &format!("端口 {port} 已被非 Harness 服务占用"),
                        &log,
                    );
                    return;
                }

                set_loading_progress(&window, 8, "正在准备 DeepSeek Harness 运行环境…");
                match start_server(&handle, &window, port, &log) {
                    Ok(()) => {
                        log_line(&log, &format!("服务已就绪,导航到 {}", app_url));
                        set_loading_progress(&window, 100, "DeepSeek Harness 已启动");
                        let _ = window.navigate(app_url.clone());
                    }
                    Err(reason) => report_startup_error(&handle, &window, port, &reason, &log),
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        if matches!(event, RunEvent::Exit | RunEvent::ExitRequested { .. }) {
            // 退出时回收本应用拉起的服务进程。
            if let Some(state) = app_handle.try_state::<ServerProcess>() {
                state.shutting_down.store(true, Ordering::SeqCst);
            }
            stop_managed_child(app_handle);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::response_is_harness;

    #[test]
    fn accepts_harness_boot_page() {
        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
                        <script>window.__DSH_BOOT__ = {}</script>";
        assert!(response_is_harness(response));
    }

    #[test]
    fn rejects_unrelated_service_on_same_port() {
        let response = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<h1>Other app</h1>";
        assert!(!response_is_harness(response));
    }

    #[test]
    fn rejects_error_page_with_marker_text() {
        let response = "HTTP/1.1 503 Service Unavailable\r\n\r\nwindow.__DSH_BOOT__";
        assert!(!response_is_harness(response));
    }

    #[cfg(feature = "bootstrap")]
    #[test]
    fn bootstrap_node_download_is_pinned_and_checksummed() {
        use super::{
            BOOTSTRAP_NODE_CHINA_URL, BOOTSTRAP_NODE_OFFICIAL_URL, BOOTSTRAP_NODE_SHA256,
            BOOTSTRAP_NODE_VERSION,
        };

        assert!(BOOTSTRAP_NODE_OFFICIAL_URL.contains(BOOTSTRAP_NODE_VERSION));
        assert!(BOOTSTRAP_NODE_CHINA_URL.contains(BOOTSTRAP_NODE_VERSION));
        assert_eq!(BOOTSTRAP_NODE_SHA256.len(), 64);
        assert!(BOOTSTRAP_NODE_SHA256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit()));
    }

    #[cfg(feature = "bootstrap")]
    #[test]
    fn setup_progress_moves_without_reaching_stage_end_early() {
        use super::projected_setup_progress;

        assert_eq!(projected_setup_progress(10, 45, 0), 10);
        assert!(projected_setup_progress(10, 45, 30) > 10);
        assert_eq!(projected_setup_progress(10, 45, u64::MAX), 44);
    }

    #[cfg(feature = "bootstrap")]
    #[test]
    fn source_selection_prefers_available_lower_latency_source() {
        use super::{https_host, prefer_china_source};
        use std::time::Duration;

        assert!(prefer_china_source(
            Some(Duration::from_millis(20)),
            Some(Duration::from_millis(100))
        ));
        assert!(!prefer_china_source(
            Some(Duration::from_millis(100)),
            Some(Duration::from_millis(20))
        ));
        assert!(prefer_china_source(Some(Duration::from_millis(20)), None));
        assert!(!prefer_china_source(None, None));
        assert_eq!(
            https_host("https://registry.npmmirror.com/example"),
            Some("registry.npmmirror.com")
        );
    }

    #[cfg(feature = "bootstrap")]
    #[test]
    fn dsh_version_check_only_installs_when_needed() {
        use super::{
            decide_dsh_version, parse_npm_view_version, select_npx_dsh_root, DshVersionDecision,
        };

        assert_eq!(
            decide_dsh_version(false, None, Some("1.2.3")),
            DshVersionDecision::Install
        );
        assert_eq!(
            decide_dsh_version(true, Some("1.2.3"), Some("1.2.3")),
            DshVersionDecision::Current
        );
        assert_eq!(
            decide_dsh_version(true, Some("1.2.2"), Some("1.2.3")),
            DshVersionDecision::Update
        );
        assert_eq!(
            decide_dsh_version(true, Some("1.2.3"), None),
            DshVersionDecision::UseExistingOffline
        );
        assert_eq!(parse_npm_view_version("\"1.2.3\"").unwrap(), "1.2.3");

        let missing_root = std::env::temp_dir().join("dsh-npx-selection-missing");
        assert_eq!(
            select_npx_dsh_root(&missing_root),
            missing_root.join("deepseek-harness-desktop")
        );
    }
}
