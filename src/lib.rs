//! resmon 核心库（任务监控）：进程采样、分组判定、格式化。
//!
//! GUI（`src/main.rs`）与诊断工具（`src/bin/resmon-dump.rs`）共用这里的逻辑，
//! 保证「看到的」和「查证的」是同一套代码。

#![allow(non_snake_case)]
#![allow(non_camel_case_types)]
// 循环变量同时索引多个并行数组（GROUP_NAMES / cur / cpu_pct），按序号遍历更直观。
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;

use windows_sys::core::PWSTR;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HWND};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
    TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Environment::GetEnvironmentVariableW;
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows_sys::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::SystemInformation::GetTickCount64;
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetCurrentProcessId, GetProcessTimes, OpenProcess,
    QueryFullProcessImageNameW, TerminateProcess, ALL_PROCESSOR_GROUPS,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_READ,
};

// ---------------------------------------------------------------- 分组常量

/// Codex / Claude / Kimi CLI 固定在最上面；鹊桥网关放在倒二（任务监控之上）。
/// 这些分组没有进程时不显示（`main.rs` 的 `view()` 跳过空行）。
pub const G_CODEX: usize = 0;
pub const G_CLAUDE: usize = 1;
pub const G_KIMI: usize = 2;
pub const G_WORKBUDDY: usize = 3;
pub const G_OPENCODE: usize = 4;
pub const G_DSH: usize = 5;
pub const G_PI: usize = 6;
pub const G_MAGPIE: usize = 7;
pub const G_ZCODE: usize = 8;
pub const G_AGENT: usize = 9;
pub const G_ARGENT: usize = 10;
pub const G_QUEQIAO: usize = 11;
pub const G_OMP: usize = 12;
/// 挂件自己。放在最后，始终显示，方便直接看到监视器的开销。
pub const G_RESMON: usize = 13;
pub const G_COUNT: usize = 14;

pub const GROUP_NAMES: [&str; G_COUNT] = [
    "Codex",
    "Claude",
    "Kimi CLI",
    "WorkBuddy",
    "OpenCode",
    "DSH",
    "Pi",
    "Magpie",
    "ZCode",
    "代理管理",
    "Argent",
    "鹊桥网关",
    "omp",
    "任务监控",
];

/// 其它程序按可执行名聚合，超过任一阈值全部列出（不截断）。
pub const OTHER_MEM_BYTES: u64 = 100 * 1024 * 1024;
pub const OTHER_CPU_PCT: f64 = 5.0;

// ---------------------------------------------------------------- 格式化

pub fn fmt_mem(bytes: u64) -> String {
    let mb = bytes as f64 / 1048576.0;
    if mb >= 1024.0 {
        format!("{:.2} GB", mb / 1024.0)
    } else if mb >= 100.0 {
        format!("{:.0} MB", mb)
    } else {
        format!("{:.1} MB", mb)
    }
}

pub fn fmt_pct(p: f64) -> String {
    format!("{:.1}%", p)
}

pub fn lower(v: &str) -> String {
    v.to_lowercase()
}

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- 根路径

#[derive(Clone, Default)]
pub struct Roots {
    pub wb: [String; 2],
    pub dsh: [String; 2],
    pub zcode: [String; 2],
    pub agent: [String; 2],
}

impl Roots {
    /// 从环境变量与约定路径推出各组的根目录。
    ///
    /// 源码树路径不写死：用 `WORKBUDDY_ROOT` / `DSH_HOME` / `ZCODE_ROOT` /
    /// `AGENT_MANAGER_ROOT` 覆盖，缺省只保留用户目录下的安装位置。
    pub fn detect() -> Roots {
        let prof = lower(&env_var("USERPROFILE"));
        let lad = lower(&env_var("LOCALAPPDATA"));
        Roots {
            wb: [
                env_root("WORKBUDDY_ROOT"),
                format!("{}\\.workbuddy\\", prof),
            ],
            dsh: [env_root("DSH_HOME"), format!("{}\\.dsh\\", prof)],
            zcode: [
                env_root("ZCODE_ROOT"),
                format!("{}\\programs\\zcode\\", lad),
            ],
            agent: [
                env_root("AGENT_MANAGER_ROOT"),
                format!("{}\\programs\\agentmanager\\", lad),
            ],
        }
    }
}

/// 读一个可选的根目录环境变量；统一成小写 + 结尾反斜杠，空值返回空串。
fn env_root(name: &str) -> String {
    let v = env_var(name);
    let v = v.trim();
    if v.is_empty() {
        return String::new();
    }
    let mut out = lower(v).replace('/', "\\");
    if !out.ends_with('\\') {
        out.push('\\');
    }
    out
}

fn env_var(name: &str) -> String {
    unsafe {
        let n = wide(name);
        let mut buf = vec![0u16; 1024];
        let len = GetEnvironmentVariableW(n.as_ptr(), buf.as_mut_ptr(), buf.len() as u32);
        if len == 0 || len as usize >= buf.len() {
            return String::new();
        }
        buf.truncate(len as usize);
        String::from_utf16_lossy(&buf)
    }
}

/// 是否是 Windows 系统进程。
///
/// 判据有两层：
/// 1. 核心系统进程名（System / csrss / lsass / svchost …）；
/// 2. 可执行文件位于 `%WINDIR%`（`C:\Windows\`）下——explorer、dwm、
///    `SystemApps` 里的壳组件都在这里。
///
/// 界面上这些行照常显示，只是不给「结束」入口；即使误调，
/// `kill_group()` / `kill_extra()` 也会跳过。
pub fn is_system_process(pid: u32, low_name: &str, low_path: &str) -> bool {
    if pid <= 4 {
        return true;
    }

    const SYSTEM_NAMES: &[&str] = &[
        "system",
        "idle",
        "registry",
        "memory compression",
        "secure system",
        "smss.exe",
        "csrss.exe",
        "wininit.exe",
        "winlogon.exe",
        "services.exe",
        "lsass.exe",
        "lsm.exe",
        "svchost.exe",
        "dwm.exe",
        "fontdrvhost.exe",
        "spoolsv.exe",
        "audiodg.exe",
        "sihost.exe",
        "taskhostw.exe",
        "wudfhost.exe",
        "msmpeng.exe",
        "nissrv.exe",
        "securityhealthservice.exe",
        "securityhealthsystray.exe",
    ];

    if SYSTEM_NAMES.contains(&low_name) {
        return true;
    }

    static WINDIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let windir = WINDIR.get_or_init(|| {
        let w = lower(&env_var("WINDIR"));
        if w.is_empty() {
            "c:\\windows\\".to_string()
        } else {
            format!("{}\\", w.trim_end_matches('\\'))
        }
    });

    !low_path.is_empty() && low_path.starts_with(windir.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_process_detection() {
        assert!(is_system_process(4, "system", ""));
        assert!(is_system_process(800, "csrss.exe", r"c:\windows\system32\csrss.exe"));
        assert!(is_system_process(900, "explorer.exe", r"c:\windows\explorer.exe"));
        assert!(!is_system_process(
            1000,
            "msedge.exe",
            r"c:\program files (x86)\microsoft\edge\application\msedge.exe"
        ));
        assert!(!is_system_process(
            1100,
            "python.exe",
            r"c:\users\x\appdata\local\programs\python\python313\python.exe"
        ));
    }

    #[test]
    fn codex_and_claude_are_pinned_groups() {
        let roots = Roots::default();
        let codex = classify(&roots, "", "codex.exe", "").expect("codex");
        assert_eq!(codex.group, G_CODEX);
        let claude = classify(&roots, "", "claude.exe", "").expect("claude");
        assert_eq!(claude.group, G_CLAUDE);
        let kimi = classify(&roots, "", "kimi.exe", "").expect("kimi");
        assert_eq!(kimi.group, G_KIMI);

        // npm 版的 node 启动器。
        let node_codex = classify(
            &roots,
            r"d:\nodejs\node.exe",
            "node.exe",
            r#""d:\nodejs\node.exe" c:\users\x\appdata\roaming\npm\node_modules\@openai\codex\bin\codex.js"#,
        )
        .expect("node codex");
        assert_eq!(node_codex.group, G_CODEX);

        let node_claude = classify(
            &roots,
            r"d:\nodejs\node.exe",
            "node.exe",
            r#""d:\nodejs\node.exe" c:\users\x\appdata\roaming\npm\node_modules\@anthropic-ai\claude-code\cli.js"#,
        )
        .expect("node claude");
        assert_eq!(node_claude.group, G_CLAUDE);
    }

    #[test]
    fn omp_processes_are_pinned() {
        let roots = Roots::default();

        // 主进程 omp.exe。
        let omp = classify(&roots, r"c:\users\x\.bun\bin\omp.exe", "omp.exe", "").expect("omp");
        assert_eq!(omp.group, G_OMP);
        assert_eq!(omp.rule, "name:omp*");

        // bun worker 靠命令行识别（真机是反斜杠路径）。
        let worker = classify(
            &roots,
            r"c:\users\x\.bun\bin\bun.exe",
            "bun.exe",
            r"c:\users\x\.bun\bin\bun.exe c:\users\x\.bun\install\global\node_modules\@oh-my-pi\pi-coding-agent\dist\cli.js __omp_worker_daemon_broker",
        )
        .expect("omp bun worker");
        assert_eq!(worker.group, G_OMP);
        assert_eq!(worker.rule, "cmd:bun+omp-worker");

        // 其它 bun 进程不归 omp。
        let other = classify(&roots, r"c:\bunx\bun.exe", "bun.exe", r"bun run dev.js");
        assert!(other.is_none() || other.as_ref().map(|m| m.group) != Some(G_OMP));
    }

    fn proc(pid: u32, parent: u32, name: &str, path: &str, cmd: &str) -> ProcInfo {
        ProcInfo {
            pid,
            parent_pid: parent,
            name: name.to_string(),
            path: path.to_string(),
            cmd: cmd.to_string(),
            group: None,
            rule: "-",
            mem_bytes: 0,
            priv_bytes: 0,
            cpu_ticks: 0,
        }
    }

    #[test]
    fn webview_host_flag_parsing() {
        assert_eq!(
            webview_host_flag(
                r#""c:\x\msedgewebview2.exe" --embedded-browser-webview=1 --webview-exe-name=SearchHost.exe --user-data-dir="c:\y""#,
            ),
            Some("searchhost.exe".to_string())
        );
        assert_eq!(webview_host_flag("--type=renderer"), None);
    }

    #[test]
    fn webview_follows_system_host_and_user_host() {
        let procs = vec![
            proc(
                100,
                0,
                "SearchHost.exe",
                r"c:\windows\systemapps\microsoftwindows.client.cbs_cw5n1h2txyewy\searchhost.exe",
                "-ServerName:CortanaUI.AppX",
            ),
            proc(
                101,
                100,
                "msedgewebview2.exe",
                r"c:\program files (x86)\microsoft\edgewebview\application\157.0.4341.46\msedgewebview2.exe",
                "--embedded-browser-webview=1 --webview-exe-name=SearchHost.exe",
            ),
            proc(
                102,
                101,
                "msedgewebview2.exe",
                r"c:\program files (x86)\microsoft\edgewebview\application\157.0.4341.46\msedgewebview2.exe",
                "--type=renderer",
            ),
            proc(
                200,
                0,
                "magpie.exe",
                r"c:\users\x\appdata\local\programs\magpie\magpie.exe",
                "",
            ),
            proc(
                201,
                200,
                "msedgewebview2.exe",
                r"c:\program files (x86)\microsoft\edgewebview\application\157.0.4341.46\msedgewebview2.exe",
                "--embedded-browser-webview=1 --webview-exe-name=magpie.exe",
            ),
        ];

        let ids = identities(&procs);

        // 系统宿主的 WebView2 主进程与 renderer：整组隐藏。
        assert!(ids[1].hidden && ids[1].system);
        assert!(ids[2].hidden && ids[2].system);

        // 普通宿主：WebView2 并入宿主行（extra_key = 宿主名）。
        assert!(!ids[4].hidden);
        assert_eq!(ids[4].extra_key.as_deref(), Some("magpie.exe"));
        assert_eq!(ids[4].display, "magpie.exe");

        // 宿主自身不变。
        assert!(!ids[0].hidden);
        assert!(ids[0].system);
        assert!(!ids[3].hidden);
        assert_eq!(ids[3].extra_key.as_deref(), Some("magpie.exe"));
    }
}

// ---------------------------------------------------------------- 分类

/// 归类结果：分组序号 + 命中的规则说明（用于诊断输出）。
pub struct Match {
    pub group: usize,
    pub rule: &'static str,
}

/// 把一个进程归类到某一组。路径/名称/命令行均为小写。
///
/// 判定顺序：
/// 0. 鹊桥网关（要在路径规则之前，源码树里的 exe 会命中 agent-manager 根）
/// 1. 路径前缀（WorkBuddy 根 → DSH 根 → ZCode 根 → Agent Manager 根）
/// 2. 进程名前缀
/// 3. 命令行特征（仅对 node / msedgewebview2 / cmd / bun 读命令行）
pub fn classify(
    roots: &Roots,
    low_path: &str,
    low_name: &str,
    low_cmd: &str,
) -> Option<Match> {
    // 鹊桥网关的守护进程与面板：queqiao-gateway.exe / queqiao-gateway-panel.exe
    if low_name.starts_with("queqiao-gateway") {
        return Some(Match { group: G_QUEQIAO, rule: "name:queqiao-gateway*" });
    }
    // Argent：~/.argent 下的 CLI、桌面端与后台辅助进程。
    if low_path.contains(".argent") {
        return Some(Match { group: G_ARGENT, rule: "path:.argent-root" });
    }
    if low_name.starts_with("argent") {
        return Some(Match { group: G_ARGENT, rule: "name:argent*" });
    }
    for r in &roots.wb {
        if !r.is_empty() && low_path.starts_with(r.as_str()) {
            return Some(Match { group: G_WORKBUDDY, rule: "path:workbuddy-root" });
        }
    }
    for r in &roots.dsh {
        if !r.is_empty() && low_path.starts_with(r.as_str()) {
            return Some(Match { group: G_DSH, rule: "path:dsh-root" });
        }
    }
    for r in &roots.zcode {
        if !r.is_empty() && low_path.starts_with(r.as_str()) {
            return Some(Match { group: G_ZCODE, rule: "path:zcode-root" });
        }
    }
    for r in &roots.agent {
        if !r.is_empty() && low_path.starts_with(r.as_str()) {
            return Some(Match { group: G_AGENT, rule: "path:agent-manager-root" });
        }
    }

    if low_name.starts_with("workbuddy") {
        return Some(Match { group: G_WORKBUDDY, rule: "name:workbuddy*" });
    }
    if low_name.starts_with("deepseek harness") {
        return Some(Match { group: G_DSH, rule: "name:deepseek harness*" });
    }
    if low_name.starts_with("opencode") {
        return Some(Match { group: G_OPENCODE, rule: "name:opencode*" });
    }
    if low_name.starts_with("magpie") {
        return Some(Match { group: G_MAGPIE, rule: "name:magpie*" });
    }
    // ZCode 桌面端（Electron 主进程/子进程）与它自带的 runtime-host。
    if low_name.starts_with("zcode") {
        return Some(Match { group: G_ZCODE, rule: "name:zcode*" });
    }
    // Agent Manager 的各版本 exe（agent-manager / -app / -egui / -web）。
    if low_name.starts_with("agent-manager") {
        return Some(Match { group: G_AGENT, rule: "name:agent-manager*" });
    }
    // Claude Code 原生可执行文件。
    if low_name.starts_with("kimi") {
        return Some(Match { group: G_KIMI, rule: "name:kimi*" });
    }
    if low_name.starts_with("claude") {
        return Some(Match { group: G_CLAUDE, rule: "name:claude*" });
    }
    // Codex CLI 与它的 app-server 守护进程（都叫 codex.exe）。
    if low_name.starts_with("codex") {
        return Some(Match { group: G_CODEX, rule: "name:codex*" });
    }
    // 挂件自己。用精确名，避免把 resmon-dump 也算进来。
    if low_name == "resmon.exe" {
        return Some(Match { group: G_RESMON, rule: "name:resmon.exe" });
    }

    // omp 主进程（~/.bun/bin/omp.exe 等，名字以 omp 开头）。
    if low_name.starts_with("omp") {
        return Some(Match { group: G_OMP, rule: "name:omp*" });
    }
    // omp 的 bun worker：bun.exe 跑 @oh-my-pi 的 pi-coding-agent（路径可能是 / 或 \）。
    if low_name == "bun.exe" && low_cmd.contains("@oh-my-pi") {
        return Some(Match { group: G_OMP, rule: "cmd:bun+omp-worker" });
    }

    // Magpie 的界面跑在 WebView2 里，命令行带 magpie.exe。
    if low_name == "msedgewebview2.exe" && low_cmd.contains("magpie.exe") {
        return Some(Match { group: G_MAGPIE, rule: "cmd:msedgewebview2+magpie.exe" });
    }
    if low_name == "cmd.exe" && low_cmd.contains("start-serve.cmd") {
        return Some(Match { group: G_MAGPIE, rule: "cmd:cmd+start-serve.cmd" });
    }

    // Pi 跑在 node.exe 上，靠命令行区分。
    if low_name == "node.exe"
        && (low_cmd.contains("pi-launcher") || low_cmd.contains("\\.pi\\agent\\"))
    {
        return Some(Match { group: G_PI, rule: "cmd:node+pi" });
    }

    // npm 安装的 Codex / Claude Code 也走 node.exe。
    if low_name == "node.exe"
        && (low_cmd.contains("@openai\\codex")
            || low_cmd.contains("@openai/codex")
            || low_cmd.contains("codex.js"))
    {
        return Some(Match { group: G_CODEX, rule: "cmd:node+codex" });
    }
    if low_name == "node.exe"
        && (low_cmd.contains("@anthropic-ai\\claude-code")
            || low_cmd.contains("@anthropic-ai/claude-code")
            || low_cmd.contains("claude-code"))
    {
        return Some(Match { group: G_CLAUDE, rule: "cmd:node+claude-code" });
    }
    if low_name == "node.exe"
        && (low_cmd.contains("@moonshot-ai\\kimi-code")
            || low_cmd.contains("@moonshot-ai/kimi-code")
            || low_cmd.contains("kimi-code"))
    {
        return Some(Match { group: G_KIMI, rule: "cmd:node+kimi-code" });
    }

    // kimi 有时以受保护进程运行，QueryFullProcessImageName 返回乱码，靠命令行识别。
    if low_cmd.contains(".kimi-code\\bin\\kimi.exe")
        || low_cmd.contains("@moonshot-ai\\kimi-code")
        || low_cmd.contains("@moonshot-ai/kimi-code")
    {
        return Some(Match { group: G_KIMI, rule: "cmd:kimi" });
    }

    None
}

// ---------------------------------------------------------------- 命令行

#[repr(C)]
struct PbiX {
    _r1: *mut c_void,
    peb_base_address: *mut c_void,
    _r2: [*mut c_void; 2],
    _pid: usize,
    _r3: *mut c_void,
}

#[repr(C)]
struct UnicodeStrX {
    length: u16,
    maximum_length: u16,
    buffer: PWSTR,
}

type NtQueryInformationProcessFn =
    unsafe extern "system" fn(HWND, u32, *mut c_void, u32, *mut u32) -> i32;

/// 通过 PEB 读命令行（x64 偏移）。
///
/// # Safety
/// `h` 必须是有效的进程句柄。
pub unsafe fn read_cmd_line(h: HWND) -> Option<String> {
    unsafe {
        static FN: std::sync::OnceLock<Option<NtQueryInformationProcessFn>> =
            std::sync::OnceLock::new();
        let f = *FN.get_or_init(|| {
            let ntdll = GetModuleHandleW(wide("ntdll.dll").as_ptr());
            if ntdll.is_null() {
                return None;
            }
            let p = GetProcAddress(ntdll, b"NtQueryInformationProcess\0".as_ptr());
            p.map(|f| {
                std::mem::transmute::<
                    windows_sys::Win32::Foundation::FARPROC,
                    NtQueryInformationProcessFn,
                >(Some(f))
            })
        });
        let f = f?;

        let mut pbi: PbiX = std::mem::zeroed();
        let mut ret: u32 = 0;
        if f(h, 0, &mut pbi as *mut _ as *mut c_void, size_of::<PbiX>() as u32, &mut ret) < 0 {
            return None;
        }
        if pbi.peb_base_address.is_null() {
            return None;
        }

        let mut params: *mut c_void = ptr::null_mut();
        if ReadProcessMemory(
            h,
            (pbi.peb_base_address as usize + 0x20) as *const c_void,
            &mut params as *mut _ as *mut c_void,
            size_of::<*mut c_void>(),
            ptr::null_mut(),
        ) == 0
            || params.is_null()
        {
            return None;
        }

        let mut us: UnicodeStrX = std::mem::zeroed();
        if ReadProcessMemory(
            h,
            (params as usize + 0x70) as *const c_void,
            &mut us as *mut _ as *mut c_void,
            size_of::<UnicodeStrX>(),
            ptr::null_mut(),
        ) == 0
            || us.buffer.is_null()
            || us.length < 2
        {
            return None;
        }

        let len = std::cmp::min(us.length as usize, 32768);
        let mut buf = vec![0u16; len / 2];
        let mut read: usize = 0;
        if ReadProcessMemory(
            h,
            us.buffer as *const c_void,
            buf.as_mut_ptr() as *mut c_void,
            len,
            &mut read,
        ) == 0
            || read != len
        {
            return None;
        }
        let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf16_lossy(&buf[..n]))
    }
}

// ---------------------------------------------------------------- 采样

#[derive(Clone, Copy, Default)]
pub struct GroupSample {
    pub mem_bytes: u64,
    pub priv_bytes: u64,
    pub cpu_ticks: u64,
    pub count: u32,
}

#[derive(Clone, Default)]
pub struct OtherAgg {
    pub display_name: String,
    pub exe: String,
    pub mem_bytes: u64,
    pub cpu_ticks: u64,
    pub count: u32,
    /// 只要聚合里有一个进程属于系统进程，就整行不允许结束。
    pub system: bool,
}

pub struct ExtraRow {
    pub name: String,
    pub exe: String,
    pub mem_bytes: u64,
    pub cpu_pct: f64,
    pub count: u32,
    pub system: bool,
}

/// 单个进程的采集结果。
pub struct ProcInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub name: String,
    pub path: String,
    pub cmd: String,
    pub group: Option<usize>,
    pub rule: &'static str,
    pub mem_bytes: u64,
    pub priv_bytes: u64,
    pub cpu_ticks: u64,
}

/// 采集全部进程并判定归属。
pub fn collect(roots: &Roots) -> Vec<ProcInfo> {
    let mut out: Vec<ProcInfo> = Vec::new();

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return out;
        }
        let mut pe: PROCESSENTRY32W = std::mem::zeroed();
        pe.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut pe) != 0 {
            loop {
                let pid = pe.th32ProcessID;
                if pid != 0 && pid != 4 {
                    let mut h = OpenProcess(
                        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
                        0,
                        pid,
                    );
                    if h.is_null() {
                        h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                    }
                    if !h.is_null() {
                        let mut path_buf = vec![0u16; 1024];
                        let mut sz = path_buf.len() as u32;
                        let low_path;
                        let low_name;
                        let disp_name;

                        if QueryFullProcessImageNameW(h, 0, path_buf.as_mut_ptr(), &mut sz) != 0 {
                            let full = String::from_utf16_lossy(&path_buf[..sz as usize]);
                            low_path = lower(&full);
                            disp_name = match full.rfind('\\') {
                                Some(i) => full[i + 1..].to_string(),
                                None => full.clone(),
                            };
                            low_name = lower(&disp_name);
                        } else {
                            let mut name_buf = [0u16; 260];
                            let n = pe.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
                            name_buf[..n].copy_from_slice(&pe.szExeFile[..n]);
                            disp_name = String::from_utf16_lossy(&name_buf[..n]);
                            low_path = String::new();
                            low_name = lower(&disp_name);
                        }

                        let mut cmd = String::new();
                        let mut m = classify(roots, &low_path, &low_name, "");
                        // 这几类进程，以及受保护进程（名字可能乱码），需要读命令行兜底。
                        // bun.exe 是 omp worker 的载体，必须读命令行才能归组。
                        if m.is_none()
                            && (low_name == "node.exe"
                                || low_name == "msedgewebview2.exe"
                                || low_name == "cmd.exe"
                                || low_name == "bun.exe"
                                || low_name.chars().any(|c| !c.is_ascii()))
                        {
                            if let Some(c) = read_cmd_line(h) {
                                cmd = c;
                                m = classify(roots, &low_path, &low_name, &lower(&cmd));
                            }
                        }

                        let mut mem: u64 = 0;
                        let mut priv_bytes: u64 = 0;
                        let mut ticks: u64 = 0;

                        let mut pmc: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
                        pmc.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
                        if GetProcessMemoryInfo(
                            h,
                            &mut pmc as *mut _ as *mut PROCESS_MEMORY_COUNTERS,
                            size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
                        ) != 0
                        {
                            mem = pmc.WorkingSetSize as u64;
                            priv_bytes = pmc.PrivateUsage as u64;
                        }

                        let mut ct = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
                        let mut et = ct;
                        let mut kt = ct;
                        let mut ut = ct;
                        if GetProcessTimes(h, &mut ct, &mut et, &mut kt, &mut ut) != 0 {
                            let k = ((kt.dwHighDateTime as u64) << 32) | kt.dwLowDateTime as u64;
                            let u = ((ut.dwHighDateTime as u64) << 32) | ut.dwLowDateTime as u64;
                            ticks = k + u;
                        }

                        let (group, rule) = match m {
                            Some(mm) => (Some(mm.group), mm.rule),
                            None => (None, "-"),
                        };

                        out.push(ProcInfo {
                            pid,
                            parent_pid: pe.th32ParentProcessID,
                            name: disp_name,
                            path: low_path,
                            cmd,
                            group,
                            rule,
                            mem_bytes: mem,
                            priv_bytes,
                            cpu_ticks: ticks,
                        });

                        CloseHandle(h);
                    }
                }

                if Process32NextW(snap, &mut pe) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }

    out
}

// ---------------------------------------------------------------- WebView2 宿主

const WEBVIEW_EXE: &str = concat!("msedgewebview2", ".exe");

fn is_webview2(low_name: &str) -> bool {
    low_name == WEBVIEW_EXE
}

/// 从 WebView2 命令行里取宿主 exe 名（Chromium 会带 `--webview-exe-name=xxx`）。
fn webview_host_flag(cmd: &str) -> Option<String> {
    const KEY: &str = "--webview-exe-name=";
    let low = lower(cmd);
    let start = low.find(KEY)? + KEY.len();
    let rest = &low[start..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"')
        .unwrap_or(rest.len());
    let name = rest[..end].trim_matches('"');
    (!name.is_empty()).then(|| name.to_string())
}

#[derive(Clone)]
struct HostInfo {
    /// 宿主 exe 名（小写），也是并入「其它程序」时使用的聚合键。
    name: String,
    /// 宿主显示名；宿主进程不在时回退到命令行里的名字。
    display: String,
    group: Option<usize>,
    system: bool,
}

/// 每个进程的最终归属。WebView2 不再按自己的 exe 名单独成行，而是跟随宿主。
struct Identity {
    group: Option<usize>,
    extra_key: Option<String>,
    display: String,
    system: bool,
    /// 系统宿主下的 WebView2：整组隐藏，不占行、不计入合计。
    hidden: bool,
}

fn webview_hosts(procs: &[ProcInfo]) -> Vec<Option<HostInfo>> {
    let mut out: Vec<Option<HostInfo>> = vec![None; procs.len()];
    let low_names: Vec<String> = procs.iter().map(|p| lower(&p.name)).collect();

    let by_pid: HashMap<u32, usize> = procs
        .iter()
        .enumerate()
        .map(|(i, p)| (p.pid, i))
        .collect();
    let mut by_name: HashMap<String, usize> = HashMap::new();
    for (i, name) in low_names.iter().enumerate() {
        by_name.entry(name.clone()).or_insert(i);
    }

    for (i, p) in procs.iter().enumerate() {
        if !is_webview2(&low_names[i]) {
            continue;
        }

        let flag = webview_host_flag(&p.cmd);
        let flag_low = flag.as_ref().map(|s| lower(s));

        // 沿父链找最近的、不是 msedgewebview2 的祖先。
        // crashpad-handler / gpu / renderer 等子进程没有 --webview-exe-name，
        // 也能通过这里归到同一个宿主。
        let mut ancestor = None;
        let mut cursor = p.parent_pid;
        for _ in 0..32 {
            let Some(&j) = by_pid.get(&cursor) else { break };
            if !is_webview2(&low_names[j]) {
                ancestor = Some(j);
                break;
            }
            let parent = procs[j].parent_pid;
            if parent == 0 || parent == cursor {
                break;
            }
            cursor = parent;
        }

        let host_idx = match &flag_low {
            Some(f) => by_name
                .get(f)
                .copied()
                .or_else(|| ancestor.filter(|&j| low_names[j] == *f)),
            None => ancestor,
        };

        out[i] = match host_idx {
            Some(j) => {
                let h = &procs[j];
                Some(HostInfo {
                    name: low_names[j].clone(),
                    display: h.name.clone(),
                    group: h.group,
                    system: is_system_process(h.pid, &low_names[j], &h.path),
                })
            }
            None => flag.map(|f| HostInfo {
                name: lower(&f),
                display: f,
                group: None,
                system: false,
            }),
        };
    }

    out
}

/// 把每个进程折算成「该记到哪一组 / 哪一行」。
/// - 普通进程：沿用 classify 的结果。
/// - WebView2：跟随宿主。系统宿主 → hidden；普通宿主 → 并入宿主所在组或宿主行。
fn identities(procs: &[ProcInfo]) -> Vec<Identity> {
    let hosts = webview_hosts(procs);
    let low_names: Vec<String> = procs.iter().map(|p| lower(&p.name)).collect();

    procs
        .iter()
        .enumerate()
        .map(|(i, p)| {
            if is_webview2(&low_names[i]) {
                if let Some(host) = &hosts[i] {
                    if host.system {
                        return Identity {
                            group: None,
                            extra_key: None,
                            display: host.display.clone(),
                            system: true,
                            hidden: true,
                        };
                    }
                    if let Some(group) = host.group {
                        return Identity {
                            group: Some(group),
                            extra_key: None,
                            display: host.display.clone(),
                            system: false,
                            hidden: false,
                        };
                    }
                    return Identity {
                        group: None,
                        extra_key: Some(host.name.clone()),
                        display: host.display.clone(),
                        system: false,
                        hidden: false,
                    };
                }
            }

            Identity {
                group: p.group,
                extra_key: p.group.is_none().then(|| low_names[i].clone()),
                display: p.name.clone(),
                system: is_system_process(p.pid, &low_names[i], &p.path),
                hidden: false,
            }
        })
        .collect()
}

/// 强行结束某一组的全部进程，返回 `(成功数, 失败数)`。
///
/// 用与界面完全相同的 `collect()` + `identities()` 来圈定进程，
/// 保证「看到的」就是要结束的。ResMon 自己永远不会被结束。
pub fn kill_group(roots: &Roots, group: usize) -> (usize, usize) {
    if group == G_RESMON {
        return (0, 0);
    }

    let self_pid = unsafe { GetCurrentProcessId() };
    let procs = collect(roots);
    let ids = identities(&procs);
    let mut killed = 0;
    let mut failed = 0;

    for (p, id) in procs.iter().zip(&ids) {
        if id.hidden {
            continue;
        }
        if id.group != Some(group) {
            continue;
        }
        if p.pid == self_pid || is_system_process(p.pid, &lower(&p.name), &p.path) {
            continue;
        }

        let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, p.pid) };
        if h.is_null() {
            failed += 1;
            continue;
        }

        if unsafe { TerminateProcess(h, 1) } != 0 {
            killed += 1;
        } else {
            failed += 1;
        }
        unsafe { CloseHandle(h) };
    }

    (killed, failed)
}

/// 结束「其它程序」里按可执行名聚合出来的一行，比如 `msedge.exe`。
/// WebView2 行按宿主名聚合，宿主与它的全部 WebView2 子进程会一起结束。
/// 系统进程、ResMon 自己、以及已经归入固定分组的进程都会被跳过。
pub fn kill_extra(roots: &Roots, exe: &str) -> (usize, usize) {
    let self_pid = unsafe { GetCurrentProcessId() };
    let procs = collect(roots);
    let ids = identities(&procs);
    let mut killed = 0;
    let mut failed = 0;

    for (p, id) in procs.iter().zip(&ids) {
        if id.hidden {
            continue;
        }
        if id.group.is_some() {
            continue;
        }
        let key = match &id.extra_key {
            Some(k) => k.clone(),
            None => lower(&p.name),
        };
        if !key.eq_ignore_ascii_case(exe) {
            continue;
        }
        if p.pid == self_pid || is_system_process(p.pid, &lower(&p.name), &p.path) {
            continue;
        }

        let h = unsafe { OpenProcess(PROCESS_TERMINATE, 0, p.pid) };
        if h.is_null() {
            failed += 1;
            continue;
        }

        if unsafe { TerminateProcess(h, 1) } != 0 {
            killed += 1;
        } else {
            failed += 1;
        }
        unsafe { CloseHandle(h) };
    }

    (killed, failed)
}

/// 带历史状态的采样器：负责 CPU 增量与「其它程序」阈值筛选。
pub struct Sampler {
    pub roots: Roots,
    pub cur: [GroupSample; G_COUNT],
    pub cpu_pct: [f64; G_COUNT],
    pub extras: Vec<ExtraRow>,
    prev_cpu: [u64; G_COUNT],
    prev_tick: u64,
    prev_other: HashMap<String, OtherAgg>,
}

impl Default for Sampler {
    fn default() -> Self {
        Sampler::new()
    }
}

impl Sampler {
    pub fn new() -> Sampler {
        Sampler {
            roots: Roots::detect(),
            cur: Default::default(),
            cpu_pct: [0.0; G_COUNT],
            extras: Vec::new(),
            prev_cpu: [0; G_COUNT],
            prev_tick: 0,
            prev_other: HashMap::new(),
        }
    }

    /// 采一次样，返回本轮所有进程明细（诊断用）。
    pub fn sample(&mut self) -> Vec<ProcInfo> {
        let procs = collect(&self.roots);
        let ids = identities(&procs);

        let mut cur: [GroupSample; G_COUNT] = Default::default();
        let mut others: HashMap<String, OtherAgg> = HashMap::new();

        for (p, id) in procs.iter().zip(&ids) {
            // 系统宿主下的 WebView2：不显示、不计数。
            if id.hidden {
                continue;
            }
            match id.group {
                Some(g) => {
                    cur[g].count += 1;
                    cur[g].mem_bytes += p.mem_bytes;
                    cur[g].priv_bytes += p.priv_bytes;
                    cur[g].cpu_ticks += p.cpu_ticks;
                }
                None => {
                    let key = id
                        .extra_key
                        .clone()
                        .unwrap_or_else(|| lower(&p.name));
                    let e = others.entry(key.clone()).or_default();
                    if e.display_name.is_empty() {
                        e.display_name = id.display.clone();
                    }
                    if e.exe.is_empty() {
                        e.exe = key;
                    }
                    e.system |= id.system;
                    e.mem_bytes += p.mem_bytes;
                    e.cpu_ticks += p.cpu_ticks;
                    e.count += 1;
                }
            }
        }

        let now = unsafe { GetTickCount64() };
        let dt = if self.prev_tick != 0 && now > self.prev_tick {
            now - self.prev_tick
        } else {
            0
        };
        let mut cores = unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) };
        if cores == 0 {
            cores = 1;
        }

        let pct_of = |ticks: u64, prev: u64| -> f64 {
            if self.prev_tick == 0 || dt == 0 || ticks <= prev {
                0.0
            } else {
                (ticks - prev) as f64 / (dt as f64 * 10000.0 * cores as f64) * 100.0
            }
        };

        for i in 0..G_COUNT {
            self.cpu_pct[i] = pct_of(cur[i].cpu_ticks, self.prev_cpu[i]);
            self.prev_cpu[i] = cur[i].cpu_ticks;
            self.cur[i] = cur[i];
        }

        // 其它程序：超过内存或 CPU 阈值才显示。新出现的可执行名以自身累计量作为
        // 基准，避免历史 CPU 累计值让它在第一次刷新时看起来很忙。
        let mut extras: Vec<ExtraRow> = Vec::new();
        for (key, oa) in others.iter() {
            let prev = self.prev_other.get(key).map(|v| v.cpu_ticks).unwrap_or(oa.cpu_ticks);
            let pct = pct_of(oa.cpu_ticks, prev);

            if oa.mem_bytes > OTHER_MEM_BYTES || pct > OTHER_CPU_PCT {
                let mut name = oa.display_name.clone();
                if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") {
                    name.truncate(name.len() - 4);
                }
                extras.push(ExtraRow {
                    name,
                    exe: oa.exe.clone(),
                    mem_bytes: oa.mem_bytes,
                    cpu_pct: pct,
                    count: oa.count,
                    system: oa.system,
                });
            }
        }

        extras.sort_by(|a, b| {
            let sa = (a.mem_bytes as f64 / OTHER_MEM_BYTES as f64).max(a.cpu_pct / OTHER_CPU_PCT);
            let sb = (b.mem_bytes as f64 / OTHER_MEM_BYTES as f64).max(b.cpu_pct / OTHER_CPU_PCT);
            if sa != sb {
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                b.mem_bytes.cmp(&a.mem_bytes)
            }
        });

        self.extras = extras;
        self.prev_other = others;
        self.prev_tick = now;

        procs
    }
}
