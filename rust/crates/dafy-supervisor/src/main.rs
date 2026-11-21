//! 大肥鱼守护进程（Rust 版 keep_alive.js）
//!
//! 行为对齐现有 Node 脚本：
//! - 先探测端口，缺哪个起哪个，不重复拉起；
//! - bridge 起来后再等 TTS 预热（最长 90s）；
//! - 每 30 秒巡检一次，掉线就重启；
//! - 子进程隐藏窗口、独立运行，守护进程退出后仍保留（与旧行为一致）。
//!
//! 新增：
//! - 单实例互斥量，防重复守护；
//! - 重启退避与熔断（10 分钟内重启 5 次后暂停 5 分钟）；
//! - 结构化日志落盘 rust/logs/；
//! - `--once` 只巡检一次并输出状态，不拉起任何进程。

use dafy_core::{logger::Logger, net, process, single::SingleInstance};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeKind {
    /// 返回体里 `"ok":true`
    JsonOk,
    /// 任何 HTTP 响应（含 401/404）都算活着
    AnyHttp,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Runtime {
    Node,
    Python,
    /// 同目录下的 Rust 可执行文件，不需要脚本参数
    Rust,
}

struct Service {
    name: &'static str,
    port: u16,
    probe: ProbeKind,
    health_path: &'static str,
    /// 拉起后等待就绪的最长秒数
    wait_secs: u64,
    runtime: Runtime,
    /// Rust 可执行文件路径（Runtime::Rust 时使用）
    bin: Option<PathBuf>,
    script: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
}

#[derive(Default)]
struct RunState {
    pid: Option<u32>,
    restarts: Vec<Instant>,
    blocked_until: Option<Instant>,
    /// 脚本不存在：不再反复尝试，只提示一次
    missing: bool,
    last_error: String,
}

struct Supervisor {
    services: Vec<Service>,
    states: Vec<Mutex<RunState>>,
    logger: Arc<Logger>,
    node: PathBuf,
    python: PathBuf,
}

impl Supervisor {
    fn new(root: &Path, logger: Arc<Logger>) -> Self {
        let services = build_services(root);
        let states = services.iter().map(|_| Mutex::new(RunState::default())).collect();
        Self {
            services,
            states,
            logger,
            node: resolve_node(root),
            python: resolve_python(root),
        }
    }

    async fn healthy(&self, svc: &Service) -> bool {
        match svc.probe {
            ProbeKind::JsonOk => {
                net::json_ok(svc.port, svc.health_path, Duration::from_millis(1200)).await
            }
            ProbeKind::AnyHttp => {
                net::http_get(svc.port, svc.health_path, Duration::from_millis(1200))
                    .await
                    .is_some()
            }
        }
    }

    /// 启动服务；受退避/熔断约束。返回是否真的发起了启动。
    async fn ensure_started(&self, idx: usize) -> bool {
        let svc = &self.services[idx];
        let mut st = self.states[idx].lock().await;

        if st.missing {
            return false;
        }
        if svc.runtime == Runtime::Rust {
            if !svc.bin.as_ref().map(|p| p.is_file()).unwrap_or(false) {
                st.missing = true;
                self.logger
                    .error(format!("{} 找不到可执行文件（已跳过）", svc.name));
                return false;
            }
        } else if !svc.script.is_file() {
            st.missing = true;
            self.logger
                .error(format!("{} 找不到脚本：{}（已跳过）", svc.name, svc.script.display()));
            return false;
        }

        let now = Instant::now();
        if let Some(until) = st.blocked_until {
            if now < until {
                return false;
            }
            st.blocked_until = None;
            st.restarts.clear();
        }
        st.restarts
            .retain(|t| now.saturating_duration_since(*t) < Duration::from_secs(600));
        if st.restarts.len() >= 5 {
            st.blocked_until = Some(now + Duration::from_secs(300));
            self.logger.warn(format!(
                "{} 10 分钟内已重启 5 次，熔断 5 分钟（避免无限重启）",
                svc.name
            ));
            return false;
        }

        let program = match svc.runtime {
            Runtime::Node => self.node.clone(),
            Runtime::Python => self.python.clone(),
            Runtime::Rust => svc.bin.clone().unwrap_or_else(|| self.node.clone()),
        };
        let mut args = Vec::with_capacity(svc.args.len() + 1);
        if svc.runtime != Runtime::Rust {
            args.push(svc.script.to_string_lossy().to_string());
        }
        args.extend(svc.args.iter().cloned());

        let mut envs: Vec<(String, String)> = Vec::new();
        if svc.runtime == Runtime::Python {
            envs.push(("PYTHONUNBUFFERED".into(), "1".into()));
            envs.push(("PYTHONIOENCODING".into(), "utf-8".into()));
            envs.push(("PYTHONUTF8".into(), "1".into()));
            envs.push(("CUDA_VISIBLE_DEVICES".into(), String::new()));
            envs.push(("HF_ENDPOINT".into(), "https://hf-mirror.com".into()));
        }

        match process::spawn_hidden(
            process::SpawnSpec {
                program: &program,
                args: &args,
                cwd: &svc.cwd,
                envs: &envs,
                tag: svc.name,
            },
            self.logger.clone(),
        ) {
            Ok(pid) => {
                st.pid = Some(pid);
                st.restarts.push(now);
                st.last_error.clear();
                self.logger.info(format!(
                    "启动 {}（端口 {}，pid {}）",
                    svc.name, svc.port, pid
                ));
                true
            }
            Err(e) => {
                st.last_error = e.to_string();
                if svc.runtime == Runtime::Node && program == Path::new("node") {
                    self.logger.error(format!(
                        "{} 启动失败：{}。请安装 Node 或设置 DAFY_NODE 指向 node.exe",
                        svc.name, e
                    ));
                } else {
                    self.logger.error(format!("{} 启动失败：{}", svc.name, e));
                }
                false
            }
        }
    }

    /// 等待服务就绪；已就绪则立即返回。
    async fn wait_ready(&self, idx: usize, secs: u64) -> bool {
        let svc = &self.services[idx];
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if self.healthy(svc).await {
                self.logger.info(format!("{} 已就绪（端口 {}）", svc.name, svc.port));
                return true;
            }
            if Instant::now() >= deadline {
                self.logger
                    .warn(format!("{} 在 {}s 内仍未就绪，交给巡检继续重试", svc.name, secs));
                return false;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn probe_all(&self) -> Vec<bool> {
        let mut out = Vec::with_capacity(self.services.len());
        for svc in &self.services {
            out.push(self.healthy(svc).await);
        }
        out
    }

    async fn print_status(&self, up: &[bool]) {
        self.logger.info("=== 服务状态 ===");
        for (i, svc) in self.services.iter().enumerate() {
            let pid = self.states[i].lock().await.pid;
            let mark = if up[i] { "✅ 运行中" } else { "❌ 挂了" };
            let pid_text = pid.map(|p| format!(" pid={p}")).unwrap_or_default();
            self.logger.info(format!(
                "{:<10} {}: {}{}",
                svc.name, svc.port, mark, pid_text
            ));
        }
    }

    /// 与旧脚本一致：按顺序补齐服务，bridge 先起，TTS 最后等。
    async fn bootstrap(&self) {
        let up = self.probe_all().await;
        self.print_status(&up).await;

        let idx = |name: &str| self.services.iter().position(|s| s.name == name).unwrap();

        if !up[idx("bridge")] {
            if self.ensure_started(idx("bridge")).await {
                self.wait_ready(idx("bridge"), self.services[idx("bridge")].wait_secs).await;
            }
        } else {
            self.logger.info("bridge 已在运行");
        }

        if !up[idx("tts")] {
            self.ensure_started(idx("tts")).await;
        }
        if !up[idx("edge-tts")] {
            self.ensure_started(idx("edge-tts")).await;
        }
        // 旧脚本会阻塞等 TTS 预热（首次约 55s，上限 90s）
        self.wait_ready(idx("tts"), self.services[idx("tts")].wait_secs).await;

        for name in ["ofm", "pc", "video"] {
            if !self.probe_all().await[idx(name)] {
                self.ensure_started(idx(name)).await;
            }
        }

        let up = self.probe_all().await;
        self.print_status(&up).await;
        self.logger.info("守护已启动，每 30 秒巡检一次；关闭本窗口不影响子服务");
    }

    async fn watchdog_pass(&self) {
        let up = self.probe_all().await;
        for (i, svc) in self.services.iter().enumerate() {
            if !up[i] {
                self.logger.warn(format!("[watchdog] {} 掉线，尝试重启", svc.name));
                self.ensure_started(i).await;
            }
        }
    }
}

fn build_services(root: &Path) -> Vec<Service> {
    let qqbot = root.join("qqbot");
    let tts = root.join("tts");
    let pc = root.join("pc-server");
    let video = root.join("video-server");
    let ofm = root
        .parent()
        .map(|p| p.join("dsh-our-free-model"))
        .unwrap_or_else(|| root.join("dsh-our-free-model"));

    // 同名 Rust 可执行文件存在时优先用 Rust 版；缺失自动回退旧 Node 实现（可随时回滚）
    let rust_bin = |name: &str| -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join(name)))
            .filter(|p| p.is_file())
    };
    let pc_bin = rust_bin("dafy-pc.exe");
    let bridge_bin = rust_bin("dafy-bridge.exe");
    let bridge_script = qqbot.join("bridge").join("bridge.js");

    vec![
        Service {
            name: "bridge",
            port: 18902,
            probe: ProbeKind::JsonOk,
            health_path: "/api/health",
            wait_secs: 20,
            runtime: if bridge_bin.is_some() {
                Runtime::Rust
            } else {
                Runtime::Node
            },
            bin: bridge_bin,
            script: bridge_script,
            args: vec![],
            cwd: qqbot.clone(),
        },
        Service {
            name: "tts",
            port: 18903,
            probe: ProbeKind::JsonOk,
            health_path: "/health",
            wait_secs: 90,
            runtime: Runtime::Python,
            bin: None,
            script: tts.join("tts_server.py"),
            args: vec!["--port".into(), "18903".into(), "--preload".into()],
            cwd: tts.clone(),
        },
        Service {
            name: "edge-tts",
            port: 18904,
            probe: ProbeKind::JsonOk,
            health_path: "/health",
            wait_secs: 20,
            runtime: Runtime::Python,
            bin: None,
            script: tts.join("edge_tts_server.py"),
            args: vec!["--port".into(), "18904".into()],
            cwd: tts.clone(),
        },
        Service {
            name: "ofm",
            port: 18900,
            probe: ProbeKind::AnyHttp,
            health_path: "/v1/models",
            wait_secs: 30,
            runtime: Runtime::Node,
            bin: None,
            script: ofm.join("packages").join("standalone").join("cli.mjs"),
            args: vec!["--port".into(), "18900".into()],
            cwd: ofm,
        },
        Service {
            name: "pc",
            port: 18907,
            probe: ProbeKind::AnyHttp,
            health_path: "/api/ping",
            wait_secs: 15,
            runtime: if pc_bin.is_some() {
                Runtime::Rust
            } else {
                Runtime::Node
            },
            bin: pc_bin,
            script: pc.join("server.js"),
            args: vec![],
            cwd: pc,
        },
        Service {
            name: "video",
            port: 18905,
            probe: ProbeKind::AnyHttp,
            health_path: "/api/ping",
            wait_secs: 30,
            runtime: Runtime::Node,
            bin: None,
            script: video.join("server.js"),
            args: vec![],
            cwd: video,
        },
    ]
}

fn resolve_node(root: &Path) -> PathBuf {
    if let Ok(v) = std::env::var("DAFY_NODE") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return p;
        }
    }
    let bundled = root.join("node").join("node.exe");
    if bundled.is_file() {
        return bundled;
    }
    PathBuf::from("node")
}

fn resolve_python(root: &Path) -> PathBuf {
    if let Ok(v) = std::env::var("DAFY_PYTHON") {
        let p = PathBuf::from(v);
        if p.is_file() {
            return p;
        }
    }
    let bundled = root.join("python").join("python.exe");
    if bundled.is_file() {
        return bundled;
    }
    if let Some(parent) = root.parent() {
        let venv = parent
            .join("agent-lightning")
            .join(".venv")
            .join("Scripts")
            .join("python.exe");
        if venv.is_file() {
            return venv;
        }
    }
    PathBuf::from("python")
}

struct Args {
    once: bool,
    kill_children: bool,
}

fn parse_args() -> Args {
    let mut once = false;
    let mut kill_children = false;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--once" => once = true,
            "--kill-children" => kill_children = true,
            "-h" | "--help" => {
                println!("用法: dafy-supervisor [--once] [--kill-children]");
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Args { once, kill_children }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let args = parse_args();
    let root = dafy_core::project_root();

    let logger = if args.once {
        Arc::new(Logger::console_only("supervisor"))
    } else {
        match Logger::init(root.join("rust").join("logs"), "supervisor") {
            Ok(l) => Arc::new(l),
            Err(e) => {
                eprintln!("初始化日志失败: {e}");
                std::process::exit(1);
            }
        }
    };

    // --once 只读巡检，允许在和守护进程同时运行时执行
    let _guard = if args.once {
        None
    } else {
        match SingleInstance::acquire("Local\\dafy-supervisor-v1") {
            Ok(Some(g)) => Some(g),
            Ok(None) => {
                logger.error("已有守护进程在运行，本次退出（避免重复拉起服务）");
                std::process::exit(3);
            }
            Err(e) => {
                logger.error(format!("单实例检查失败，继续启动: {e}"));
                None
            }
        }
    };

    logger.info(format!("项目目录: {}", root.display()));
    let sup = Arc::new(Supervisor::new(&root, logger.clone()));

    if args.once {
        let up = sup.probe_all().await;
        sup.print_status(&up).await;
        let all_ok = up.iter().all(|v| *v);
        std::process::exit(if all_ok { 0 } else { 2 });
    }

    sup.bootstrap().await;

    let mut interval = tokio::time::interval(Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await; // 第一次 tick 立即返回，跳过，避免刚启动就重复巡检

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                logger.info("收到退出信号，守护进程结束（子服务按现有约定继续运行）");
                if args.kill_children {
                    for (i, svc) in sup.services.iter().enumerate() {
                        if let Some(pid) = sup.states[i].lock().await.pid {
                            logger.info(format!("结束 {}（pid {}）", svc.name, pid));
                            process::kill_tree(pid);
                        }
                    }
                }
                break;
            }
            _ = interval.tick() => {
                sup.watchdog_pass().await;
            }
        }
    }
}
