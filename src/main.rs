//! 任务监控（原 ResMon）— 桌面资源小挂件（GUI，iced 0.14）
//!
//! 采样与分组逻辑在 `resmon` 库里，与诊断工具共用。
//! 界面用 iced 保留模式重写：透明无边框窗口 + 自绘圆角面板，
//! 空闲时不重绘，只有 1 秒时钟和 2 秒采样会唤醒。

// GUI 程序：不分配控制台窗口
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::ffi::c_void;
use std::time::{Duration, Instant};

use iced::widget::{
    button, column, container, mouse_area, row, rule, scrollable, stack, text, Space,
};
use iced::window;
use iced::{
    Alignment, Color, Element, Font, Length, Point, Size, Subscription, Task, Theme,
};

use resmon::{
    fmt_mem, fmt_pct, wide, Sampler, GROUP_NAMES, G_CLAUDE, G_CODEX, G_COUNT, G_KIMI,
    G_RESMON,
};

use windows_sys::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, RECT, SYSTEMTIME,
};
use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::System::WindowsProgramming::{
    GetPrivateProfileIntW, WritePrivateProfileStringW,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETWORKAREA,
};

// ---------------------------------------------------------------- 尺寸 / 颜色

const WIDTH: f32 = 220.0;
const ROW_H: f32 = 24.0;

/// 内容区左右边距。四列宽度加上三个间隔必须**正好**等于内容宽：
/// iced 的 Row 在有剩余空间时会把 Fixed 子项也摊开，列就会跑到边框外面，
/// 所以这里不留任何余量。
const PAD_X: f32 = 10.0;
const PAD_Y: f32 = 5.0;
/// 三段间隔：任务监控→进程 5、进程→内存 7（最大）、内存→CPU 2。
const GAP_NAME_PROC: f32 = 5.0;
const GAP_PROC_MEM: f32 = 7.0;
const GAP_MEM_CPU: f32 = 2.0;
const GAPS: f32 = GAP_NAME_PROC + GAP_PROC_MEM + GAP_MEM_CPU;
const CONTENT_W: f32 = WIDTH - PAD_X * 2.0;
const COL_NAME: f32 = 80.0;
/// 「进程」只有 1–3 位数字，单独给一列窄的，离「任务监控」列更近。
const COL_PROC: f32 = 20.0;
/// 内存要放下「19.2 MB」这类带小数的体积；CPU 最长「100.0%」，比内存窄。
const COL_MEM: f32 = 46.0;
const COL_CPU: f32 = CONTENT_W - COL_NAME - COL_PROC - COL_MEM - GAPS;

const TITLE_H: f32 = 26.0;
const HEAD_H: f32 = 18.0;
const TOTAL_H: f32 = 26.0;
/// 「其它程序」与上面固定分组之间的分隔区（4 + 1 + 4）。
const SECTION_GAP: f32 = 9.0;

/// 没有任何内容可省略时的窗口高度：全部固定分组 + 没有其它程序。
/// `view()` 与 `window_height()` 都按“可见行数”累计，改行渲染逻辑时两边同步。
const FULL_PINNED_H: f32 = PAD_Y
    + TITLE_H
    + 6.0
    + HEAD_H
    + 6.0
    + G_COUNT as f32 * ROW_H
    + 6.0
    + 1.0
    + 4.0
    + TOTAL_H
    + PAD_Y;

/// 每个固定分组行的基准高度（BASE_H 去掉一个分组行）：
/// 启动开窗、ini 底边换算、位置钳制都用它兜底，保证窗口至少装得下
/// 标题 + 表头 + 合计 + 一行。
const BASE_H: f32 = FULL_PINNED_H - ROW_H;

/// 非内容部分的高度：标题、表头、分隔与合计。
const CHROME_H: f32 =
    PAD_Y + TITLE_H + 6.0 + HEAD_H + 6.0 + 6.0 + 1.0 + 4.0 + TOTAL_H + PAD_Y;

/// count>0 的固定分组行数（任务监控自己永远显示）。
fn pinned_rows(app: &App) -> f32 {
    (0..G_COUNT)
        .filter(|&i| i == G_RESMON || app.sampler.cur[i].count > 0)
        .count() as f32
        * ROW_H
}

/// 窗口高度 = 非内容部分 + 可见固定行 + 其它程序行；不超过工作区。
fn window_height(app: &App) -> f32 {
    let extras = shown_extras(app);
    let desired = CHROME_H
        + pinned_rows(app)
        + if extras > 0 { SECTION_GAP } else { 0.0 }
        + extras as f32 * ROW_H;
    desired.min(max_window_height())
}

const REFRESH: Duration = Duration::from_secs(2);
const TICK: Duration = Duration::from_secs(1);

const C_BG: Color = Color::from_rgb8(0, 0, 0);
const C_TEXT: Color = Color::from_rgb8(231, 232, 236);
const C_DIM: Color = Color::from_rgb8(132, 134, 143);
const C_FAINT: Color = Color::from_rgb8(96, 98, 107);
const C_LINE: Color = Color::from_rgb8(56, 57, 64);
const C_GOOD: Color = Color::from_rgb8(98, 202, 144);
const C_WARN: Color = Color::from_rgb8(236, 172, 86);
const C_BAD: Color = Color::from_rgb8(240, 104, 104);
const C_ALT: Color = Color::from_rgb8(199, 200, 208);

/// 常驻两个 Agent 的标识色：Codex 冷蓝，Claude 暖橙。
const C_CODEX: Color = Color::from_rgb8(126, 180, 255);
const C_CLAUDE: Color = Color::from_rgb8(228, 148, 104);
const C_KIMI: Color = Color::from_rgb8(213, 126, 204);

const C_MENU_BG: Color = Color::from_rgb8(38, 38, 44);
const C_MENU_HOVER: Color = Color::from_rgb8(58, 58, 68);

/// 面板背景留一点点透明，和桌面融在一起。
/// 面板背景不透明：纯黑 #000000，避免底下桌面颜色透上来。
const BG_ALPHA: f32 = 1.0;

// ---------------------------------------------------------------- 字体

fn ui_font() -> Font {
    Font::with_name("Microsoft YaHei UI")
}

fn semibold() -> Font {
    Font {
        weight: iced::font::Weight::Semibold,
        ..ui_font()
    }
}

/// 等宽数字格。列宽相同、右对齐，数字的右边严格等距，最右一列贴着右边框。
fn num_cell<'a>(
    value: impl Into<String>,
    size: f32,
    color: Color,
    font: Font,
    width: f32,
) -> Element<'a, Msg> {
    container(
        text(value.into())
            .size(size)
            .font(font)
            .color(color)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .width(Length::Fixed(width))
    .align_x(iced::alignment::Horizontal::Right)
    .clip(true)
    .into()
}

/// 左对齐的名字格：固定宽度并裁剪，长名字不会压到「进程」列。
fn name_cell<'a>(
    value: impl Into<String>,
    size: f32,
    color: Color,
    font: Font,
) -> Element<'a, Msg> {
    container(
        text(value.into())
            .size(size)
            .font(font)
            .color(color)
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .width(Length::Fixed(COL_NAME))
    .clip(true)
    .into()
}

// ---------------------------------------------------------------- 消息

#[derive(Debug, Clone)]
enum Msg {
    /// 1 秒一次：检查提示条是否过期，顺手把拖窗口后的位置写盘。
    Tick,
    /// 2 秒一次：重新采样。
    Sample,
    Window(window::Id, window::Event),
    Key(iced::keyboard::Event),
    /// 左键按在面板上：空白处拖动窗口；菜单开着时先关菜单。
    BodyPress,
    /// 右键：打开/关闭菜单（空白处）。
    RightPress,
    /// 右键落在某一行上：菜单里多出「结束」。
    RightOnRow(KillTarget),
    CloseMenu,
    ToggleTopmost,
    Copy,
    /// 菜单里的「结束 <组>…」：先问一次。
    AskKill,
    CancelKill,
    ConfirmKill,
    Quit,
}

// ---------------------------------------------------------------- 状态

struct Toast {
    text: String,
    color: Color,
    until: Instant,
}

/// 右键命中的可结束对象。固定分组用组号；「其它程序」用可执行名
/// （列表会按占用重排，存组号会指错行）。
#[derive(Clone, Debug)]
enum KillTarget {
    Group(usize),
    Extra {
        exe: String,
        label: String,
        count: u32,
        mem_bytes: u64,
    },
}

struct App {
    sampler: Sampler,
    topmost: bool,
    /// 右键菜单开着。
    menu_open: bool,
    /// 右键落在可结束的行上时，记录目标；菜单里多出「结束」。
    kill_target: Option<KillTarget>,
    /// 「结束」的二次确认页。
    confirm_kill: bool,
    toast: Option<Toast>,
    window_id: Option<window::Id>,
    pos: Point,
    size: Size,
    pos_dirty: bool,
    extras_n: usize,
}

impl App {
    fn new(ini: Ini) -> App {
        App {
            sampler: Sampler::new(),
            topmost: ini.topmost,
            menu_open: false,
            kill_target: None,
            confirm_kill: false,
            toast: None,
            window_id: None,
            pos: ini.pos,
            size: Size::new(WIDTH, BASE_H),
            pos_dirty: false,
            extras_n: 0,
        }
    }

    fn toast(&self) -> Option<&Toast> {
        self.toast
            .as_ref()
            .filter(|toast| Instant::now() < toast.until)
    }
}

impl KillTarget {
    fn label(&self, _app: &App) -> String {
        match self {
            KillTarget::Group(group) => GROUP_NAMES[*group].to_string(),
            KillTarget::Extra { label, .. } => label.clone(),
        }
    }

    fn count(&self, app: &App) -> u32 {
        match self {
            KillTarget::Group(group) => app.sampler.cur[*group].count,
            KillTarget::Extra { count, .. } => *count,
        }
    }

    fn mem(&self, app: &App) -> u64 {
        match self {
            KillTarget::Group(group) => app.sampler.cur[*group].mem_bytes,
            KillTarget::Extra { mem_bytes, .. } => *mem_bytes,
        }
    }
}

// ---------------------------------------------------------------- 窗口高度

/// 主屏工作区（逻辑像素，扣掉任务栏），返回 (x, y, 宽, 高)。
fn work_area() -> (f32, f32, f32, f32) {
    unsafe {
        let mut wa: RECT = std::mem::zeroed();
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut wa as *mut RECT as *mut c_void,
            0,
        ) == 0
        {
            return (0.0, 0.0, 1280.0, 900.0);
        }
        let s = scale();
        (
            wa.left as f32 / s,
            wa.top as f32 / s,
            (wa.right - wa.left) as f32 / s,
            (wa.bottom - wa.top) as f32 / s,
        )
    }
}

/// 主屏工作区高度（逻辑像素，扣掉任务栏）。
fn work_area_height() -> f32 {
    work_area().3
}

/// 窗口最高不超过工作区，留 12px 余量。
fn max_window_height() -> f32 {
    (work_area_height() - 12.0).max(BASE_H)
}

/// 界面上真正显示的「其它程序」行数：系统进程不可结束，直接不展示。
fn shown_extras(app: &App) -> usize {
    app.sampler.extras.iter().filter(|e| !e.system).count()
}

/// 「其它程序」放不下时返回滚动区高度；放得下返回 None（不套 scrollable，
/// 少一层事件与裁剪）。可见固定行与合计行始终可见。
fn extras_viewport(app: &App) -> Option<f32> {
    let visible = shown_extras(app);
    if visible == 0 {
        return None;
    }
    let full = visible as f32 * ROW_H;
    let available = (app.size.height - CHROME_H - pinned_rows(app) - SECTION_GAP).max(ROW_H);
    (full > available + 0.5).then_some(available)
}

/// 改高度时保持底边不动：窗口往上长。
fn resize_keep_bottom(app: &mut App, new_h: f32) -> Task<Msg> {
    let old_h = app.size.height;
    if (new_h - old_h).abs() < 0.5 {
        return Task::none();
    }

    let mut new_pos = Point::new(app.pos.x, app.pos.y + old_h - new_h);
    // 往上长时别把标题拖出屏幕顶部，也别让底边压到任务栏。
    let (wx, wy, ww, wh) = work_area();
    new_pos.x = new_pos.x.clamp(wx, (wx + ww - WIDTH - 8.0).max(wx));
    let top_limit = wy;
    let bottom_limit = (wy + wh - new_h - 8.0).max(wy);
    new_pos.y = new_pos.y.clamp(top_limit, bottom_limit);
    app.size = Size::new(WIDTH, new_h);
    app.pos = new_pos;

    match app.window_id {
        Some(id) => Task::batch([
            window::resize(id, app.size),
            window::move_to(id, new_pos),
        ]),
        None => Task::none(),
    }
}

// ---------------------------------------------------------------- ini

#[derive(Clone)]
struct Ini {
    pos: Point,
    topmost: bool,
}

fn exe_dir() -> String {
    unsafe {
        let mut buf = vec![0u16; 1024];
        let n = GetModuleFileNameW(std::ptr::null_mut(), buf.as_mut_ptr(), buf.len() as u32);
        let full = String::from_utf16_lossy(&buf[..n as usize]);
        match full.rfind('\\') {
            Some(i) => full[..i].to_string(),
            None => ".".to_string(),
        }
    }
}

fn ini_file() -> String {
    format!("{}\\{}", exe_dir(), "resmon.ini")
}

/// 读 ini 的字符串值。
///
/// 不用 `GetPrivateProfileStringW`：那会把 UTF-8 文件按 ANSI 解，中文标签变乱码。
/// 这里整文件按 UTF-8 读，只取 `[section]` 下的 `key=value`。
fn ini_string(section: &str, key: &str, fallback: &str) -> String {
    let Ok(text) = std::fs::read_to_string(ini_file()) else {
        return fallback.to_string();
    };
    let mut cur = "";
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') {
            cur = &t[1..t.len() - 1];
            continue;
        }
        if cur != section {
            continue;
        }
        if let Some((k, v)) = t.split_once('=') {
            if k.trim() == key {
                return v.trim().to_string();
            }
        }
    }
    fallback.to_string()
}

fn scale() -> f32 {
    unsafe {
        let dpi = GetDpiForSystem();
        if dpi == 0 {
            1.0
        } else {
            dpi as f32 / 96.0
        }
    }
}

fn clamp_pos(p: Point) -> Point {
    let (wx, wy, ww, wh) = work_area();
    let x = p.x.clamp(wx, (wx + ww - WIDTH - 8.0).max(wx));
    let y = p.y.clamp(wy, (wy + wh - BASE_H - 8.0).max(wy));
    Point::new(x, y)
}

fn default_pos() -> Point {
    let (wx, wy, ww, wh) = work_area();
    Point::new(
        wx + ww - WIDTH - 24.0,
        wy + wh - BASE_H - 24.0,
    )
}

fn load_ini() -> Ini {
    let file = ini_file();
    unsafe {
        if GetFileAttributesW(wide(&file).as_ptr()) == INVALID_FILE_ATTRIBUTES {
            return Ini {
                pos: default_pos(),
                topmost: true,
            };
        }

        let path = wide(&file);
        let section = wide("window");
        let read = |key: &str, fallback: i32| {
            GetPrivateProfileIntW(
                section.as_ptr(),
                wide(key).as_ptr(),
                fallback,
                path.as_ptr(),
            )
        };

        let x = read("x", -100000);
        let bottom = read("bottom", -100000);
        let topmost = read("topmost", 1) != 0;
        // ui3=1 表示坐标是 iced 的逻辑像素，且 y 存的是窗口底边。
        // 存底边而不是顶边：窗口因 extras 变高时会向上长，顶边会变，底边不会。
        let logical = read("ui3", 0) != 0;

        let pos = if logical && x > -32000 && bottom > -32000 {
            clamp_pos(Point::new(x as f32, bottom as f32 - BASE_H))
        } else {
            default_pos()
        };

        Ini { pos, topmost }
    }
}

fn save_ini(app: &App) {
    unsafe {
        let file = wide(&ini_file());
        let section = wide("window");

        let write = |key: &str, value: String| {
            WritePrivateProfileStringW(
                section.as_ptr(),
                wide(key).as_ptr(),
                wide(&value).as_ptr(),
                file.as_ptr(),
            );
        };

        write("x", (app.pos.x.round() as i32).to_string());
        write(
            "bottom",
            (app.pos.y + app.size.height).round().to_string(),
        );
        write("topmost", if app.topmost { "1" } else { "0" }.to_string());
        write("ui3", "1".to_string());
        // 旧版键留着也无害，但不再读。
    }
}

// ---------------------------------------------------------------- 快照 / 复制

fn snapshot(app: &App) -> String {
    let mut out = String::new();

    unsafe {
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        out.push_str(&format!(
            "任务监控  {:04}-{:02}-{:02} {:02}:{:02}:{:02}\r\n\r\n",
            t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
        ));
    }

    out.push_str("任务监控\t进程\t内存\tCPU\r\n");

    let mut total_mem: u64 = 0;
    let mut total_cpu: f64 = 0.0;
    let mut total_n: u32 = 0;

    // 与界面一致：零占用的固定分组不导出（任务监控自己除外）。
    for i in 0..G_COUNT {
        if i != G_RESMON && app.sampler.cur[i].count == 0 {
            continue;
        }
        out.push_str(GROUP_NAMES[i]);
        out.push('\t');
        out.push_str(&app.sampler.cur[i].count.to_string());
        out.push('\t');
        out.push_str(&if app.sampler.cur[i].count > 0 {
            fmt_mem(app.sampler.cur[i].mem_bytes)
        } else {
            "—".to_string()
        });
        out.push('\t');
        out.push_str(&if app.sampler.cur[i].count > 0 {
            fmt_pct(app.sampler.cpu_pct[i])
        } else {
            "—".to_string()
        });
        out.push_str("\r\n");

        total_mem += app.sampler.cur[i].mem_bytes;
        total_cpu += app.sampler.cpu_pct[i];
        total_n += app.sampler.cur[i].count;
    }

    for e in app.sampler.extras.iter().filter(|e| !e.system) {
        out.push_str(&e.name);
        out.push('\t');
        out.push_str(&e.count.to_string());
        out.push('\t');
        out.push_str(&fmt_mem(e.mem_bytes));
        out.push('\t');
        out.push_str(&fmt_pct(e.cpu_pct));
        out.push_str("\r\n");

        total_mem += e.mem_bytes;
        total_cpu += e.cpu_pct;
        total_n += e.count;
    }

    out.push_str("合计\t");
    out.push_str(&total_n.to_string());
    out.push('\t');
    out.push_str(&fmt_mem(total_mem));
    out.push('\t');
    out.push_str(&fmt_pct(total_cpu));
    out.push_str("\r\n");

    out
}

fn copy_snapshot(app: &mut App) -> Task<Msg> {
    app.menu_open = false;
    app.confirm_kill = false;
    app.toast = Some(Toast {
        text: "已复制".to_string(),
        color: C_GOOD,
        until: Instant::now() + Duration::from_millis(1500),
    });
    iced::clipboard::write(snapshot(app))
}

fn close_window(app: &App) -> Task<Msg> {
    match app.window_id {
        Some(id) => window::close(id),
        None => iced::exit(),
    }
}

// ---------------------------------------------------------------- update

fn update(app: &mut App, msg: Msg) -> Task<Msg> {
    match msg {
        Msg::Tick => {
            if let Some(toast) = &app.toast {
                if Instant::now() >= toast.until {
                    app.toast = None;
                }
            }
            if app.pos_dirty {
                save_ini(app);
                app.pos_dirty = false;
            }
            Task::none()
        }

        Msg::Sample => {
            app.sampler.sample();
            let n = shown_extras(app);
            if n != app.extras_n {
                app.extras_n = n;
                return resize_keep_bottom(app, window_height(app));
            }
            Task::none()
        }

        Msg::Window(id, event) => match event {
            window::Event::Opened { position, size } => {
                app.window_id = Some(id);
                if let Some(pos) = position {
                    app.pos = pos;
                }
                app.size = size;
                // 启动时的第一次采样可能早于窗口创建，那时改不了尺寸；
                // 窗口一就绪就按 extras 补一次高度，并把底边钉住。
                resize_keep_bottom(app, window_height(app))
            }
            window::Event::Moved(pos) => {
                app.pos = pos;
                app.pos_dirty = true;
                Task::none()
            }
            window::Event::Resized(size) => {
                app.size = size;
                Task::none()
            }
            window::Event::CloseRequested => {
                save_ini(app);
                close_window(app)
            }
            window::Event::Closed => iced::exit(),
            _ => Task::none(),
        },

        Msg::Key(event) => {
            if let iced::keyboard::Event::KeyPressed {
                key, modifiers, ..
            } = event
            {
                if modifiers.control() {
                    if let iced::keyboard::Key::Character(c) = key.as_ref() {
                        if c.eq_ignore_ascii_case("c") {
                            return copy_snapshot(app);
                        }
                    }
                }
            }
            Task::none()
        }

        Msg::BodyPress => {
            // 菜单开着时左键交给菜单层处理：
            // 点按钮 → 按钮自己的消息；点空白 → close_catcher 的 CloseMenu。
            // 这里再关一次会把「结束」的确认页一起关掉。
            if app.menu_open {
                return Task::none();
            }
            match app.window_id {
                Some(id) => window::drag(id),
                None => Task::none(),
            }
        }

        Msg::RightPress => {
            if app.menu_open {
                app.menu_open = false;
                app.confirm_kill = false;
                app.kill_target = None;
                return Task::none();
            }

            app.menu_open = true;
            app.confirm_kill = false;
            // 空白处右键：只有通用菜单，没有「结束」。
            app.kill_target = None;
            Task::none()
        }

        Msg::RightOnRow(target) => {
            app.menu_open = true;
            app.confirm_kill = false;
            app.kill_target = Some(target);
            Task::none()
        }

        Msg::CloseMenu => {
            app.menu_open = false;
            app.confirm_kill = false;
            app.kill_target = None;
            Task::none()
        }

        Msg::ToggleTopmost => {
            app.topmost = !app.topmost;
            app.menu_open = false;
            app.confirm_kill = false;
            app.kill_target = None;
            save_ini(app);
            let level = if app.topmost {
                window::Level::AlwaysOnTop
            } else {
                window::Level::Normal
            };
            match app.window_id {
                Some(id) => window::set_level(id, level),
                None => Task::none(),
            }
        }

        Msg::Copy => copy_snapshot(app),

        Msg::AskKill => {
            if app.kill_target.is_some() {
                app.confirm_kill = true;
            }
            Task::none()
        }

        Msg::CancelKill => {
            app.confirm_kill = false;
            Task::none()
        }

        Msg::ConfirmKill => {
            let Some(target) = app.kill_target.clone() else {
                app.menu_open = false;
                app.confirm_kill = false;
                return Task::none();
            };

            let (ok, failed) = match &target {
                KillTarget::Group(group) => {
                    resmon::kill_group(&app.sampler.roots, *group)
                }
                KillTarget::Extra { exe, .. } => {
                    resmon::kill_extra(&app.sampler.roots, exe)
                }
            };
            let (text, color) = if failed == 0 {
                (format!("已结束 {ok} 个进程"), C_GOOD)
            } else if ok == 0 {
                (format!("结束失败（{failed} 个）"), C_BAD)
            } else {
                (format!("{ok} 个结束，{failed} 个失败"), C_WARN)
            };

            app.menu_open = false;
            app.confirm_kill = false;
            app.kill_target = None;
            app.toast = Some(Toast {
                text,
                color,
                until: Instant::now() + Duration::from_millis(2500),
            });

            // 进程刚被杀掉，等下一次 Tick 前先刷一遍列表。
            Task::done(Msg::Sample)
        }

        Msg::Quit => {
            save_ini(app);
            close_window(app)
        }
    }
}

fn subscription(_app: &App) -> Subscription<Msg> {
    Subscription::batch([
        iced::time::every(TICK).map(|_| Msg::Tick),
        iced::time::every(REFRESH).map(|_| Msg::Sample),
        window::events().map(|(id, event)| Msg::Window(id, event)),
        iced::keyboard::listen().map(Msg::Key),
    ])
}

// ---------------------------------------------------------------- 样式

fn panel_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Color { a: BG_ALPHA, ..C_BG }.into()),
        // 不要外框：1px 灰边在圆角处会露成灰角，纯黑面板直接铺满窗口。
        border: iced::Border {
            radius: 12.0.into(),
            ..iced::Border::default()
        },
        ..container::Style::default()
    }
}

fn menu_panel_style(_theme: &Theme) -> container::Style {
    container::Style {
        background: Some(C_MENU_BG.into()),
        border: iced::Border {
            color: C_LINE,
            width: 1.0,
            radius: 7.0.into(),
        },
        ..container::Style::default()
    }
}

fn menu_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => {
            Some(C_MENU_HOVER.into())
        }
        _ => None,
    };

    button::Style {
        background,
        text_color: C_TEXT,
        border: iced::Border::default(),
        shadow: iced::Shadow::default(),
        snap: true,
    }
}

fn separator() -> Element<'static, Msg> {
    rule::horizontal(1.0)
        .style(|_| rule::Style {
            color: C_LINE,
            radius: 0.0.into(),
            fill_mode: rule::FillMode::Full,
            snap: true,
        })
        .into()
}

// ---------------------------------------------------------------- view

fn title_row(app: &App) -> Element<'_, Msg> {
    let mut title = row![
        // 品牌色竖条（与图标的绿色呼应）
        container(Space::new().width(Length::Fixed(3.0)).height(Length::Fixed(15.0)))
            .style(|_: &Theme| container::Style {
                background: Some(C_GOOD.into()),
                border: iced::Border {
                    radius: 2.0.into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            }),
        Space::new().width(Length::Fixed(7.0)),
        text("任务监控").size(14.0).font(semibold()).color(C_TEXT),
        Space::new().width(Length::Fixed(8.0)),
    ]
    .width(Length::Fill)
    .height(Length::Fixed(TITLE_H))
    .align_y(Alignment::Center);

    // 倒计时整句与标题同字号，用暖色突出，不再拆成大小两截。
    if let Some((label, state)) = countdown() {
        title = match state {
            Countdown::Days(days) => {
                title.push(text(format!("距{label} {days} 天")).size(14.0).font(semibold()).color(C_WARN))
            }
            Countdown::Today => {
                title.push(text(format!("{label}就是今天")).size(14.0).font(semibold()).color(C_WARN))
            }
            Countdown::Over => title.push(text(format!("{label}已结束")).size(14.0).color(C_DIM)),
        };
    }

    // 右上角只放操作提示和关闭按钮，不再显示时间。
    title = title.push(Space::new().width(Length::Fill));
    if let Some(toast) = app.toast() {
        title = title.push(text(toast.text.clone()).size(10.5).color(toast.color));
        title = title.push(Space::new().width(Length::Fixed(6.0)));
    }
    title = title.push(close_button());

    title.into()
}

enum Countdown {
    Days(i64),
    Today,
    Over,
}

/// 倒计时来自仓库外的 `resmon.ini`：`[countdown] date=YYYY-MM-DD label=xxx`。
/// 没配置就不显示——代码里不放任何个人日期。
fn countdown() -> Option<(String, Countdown)> {
    let date = ini_string("countdown", "date", "");
    let (y, m, d) = parse_date(date.trim())?;
    let label = {
        let l = ini_string("countdown", "label", "");
        if l.trim().is_empty() {
            "倒计时".to_string()
        } else {
            l.trim().to_string()
        }
    };
    unsafe {
        let mut t: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut t);
        let today = days_from_civil(t.wYear as i32, t.wMonth, t.wDay);
        let days = days_from_civil(y, m, d) - today;
        let state = if days > 0 {
            Countdown::Days(days)
        } else if days == 0 {
            Countdown::Today
        } else {
            Countdown::Over
        };
        Some((label, state))
    }
}

/// `YYYY-MM-DD` → `(年, 月, 日)`；格式不对返回 None。
fn parse_date(s: &str) -> Option<(i32, u16, u16)> {
    let mut parts = s.split('-');
    let y = parts.next()?.parse().ok()?;
    let m = parts.next()?.parse().ok()?;
    let d = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some((y, m, d))
}

/// 公历日期 → 1970-01-01 起的天数（Howard Hinnant 的算法）。
fn days_from_civil(y: i32, m: u16, d: u16) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let y = y as i64;
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn close_button() -> Element<'static, Msg> {
    button(
        container(text("×").size(20.0).color(C_TEXT))
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Center),
    )
    .width(Length::Fixed(26.0))
    .height(Length::Fixed(26.0))
    .padding(0)
    .style(close_button_style)
    .on_press(Msg::Quit)
    .into()
}

fn close_button_style(_theme: &Theme, status: button::Status) -> button::Style {
    // 平时不铺底色，让 × 直接落在纯黑标题栏上；只有悬停/按下才给一点反馈。
    let (background, text_color) = match status {
        button::Status::Hovered => (Some(Color { a: 0.12, ..C_TEXT }.into()), C_TEXT),
        button::Status::Pressed => (Some(Color { a: 0.20, ..C_TEXT }.into()), C_TEXT),
        _ => (None, C_TEXT),
    };

    button::Style {
        background,
        text_color,
        border: iced::Border {
            radius: 6.0.into(),
            ..iced::Border::default()
        },
        ..button::Style::default()
    }
}

fn header_row() -> Element<'static, Msg> {
    row![
        name_cell("任务监控", 10.5, C_FAINT, ui_font()),
        Space::new().width(Length::Fixed(GAP_NAME_PROC)),
        num_cell("进程", 10.5, C_FAINT, ui_font(), COL_PROC),
        Space::new().width(Length::Fixed(GAP_PROC_MEM)),
        num_cell("内存", 10.5, C_FAINT, ui_font(), COL_MEM),
        Space::new().width(Length::Fixed(GAP_MEM_CPU)),
        num_cell("CPU", 10.5, C_FAINT, ui_font(), COL_CPU),
    ]
    .width(Length::Fixed(CONTENT_W))
    .height(Length::Fixed(HEAD_H))
    .spacing(0.0)
    .align_y(Alignment::Center)
    .into()
}

fn data_row(
    name: String,
    name_color: Color,
    name_font: Font,
    count: String,
    mem: String,
    mem_color: Color,
    cpu: String,
    cpu_color: Color,
    font: Font,
) -> Element<'static, Msg> {
    row![
        name_cell(name, 12.5, name_color, name_font),
        Space::new().width(Length::Fixed(GAP_NAME_PROC)),
        num_cell(count, 10.5, C_DIM, font, COL_PROC),
        Space::new().width(Length::Fixed(GAP_PROC_MEM)),
        num_cell(mem, 12.0, mem_color, font, COL_MEM),
        Space::new().width(Length::Fixed(GAP_MEM_CPU)),
        num_cell(cpu, 12.0, cpu_color, font, COL_CPU),
    ]
    .width(Length::Fixed(CONTENT_W))
    .height(Length::Fixed(ROW_H))
    .spacing(0.0)
    .align_y(Alignment::Center)
    .into()
}

/// 隐藏滚动条外观：需要滚动时用滚轮，轨道不再压住右侧数字。
fn extras_scroll_style(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut style = scrollable::default(theme, status);
    style.vertical_rail.background = None;
    style.vertical_rail.border = iced::Border::default();
    style.vertical_rail.scroller.background = Color::TRANSPARENT.into();
    style.vertical_rail.scroller.border = iced::Border::default();
    style
}

fn cpu_color(p: f64, active: bool) -> Color {
    if !active {
        C_DIM
    } else if p >= 25.0 {
        C_BAD
    } else if p >= 8.0 {
        C_WARN
    } else if p >= 0.05 {
        C_TEXT
    } else {
        C_DIM
    }
}

fn group_row(app: &App, i: usize) -> Element<'static, Msg> {
    let s = &app.sampler.cur[i];
    let active = s.count > 0;
    let cpu = if active {
        fmt_pct(app.sampler.cpu_pct[i])
    } else {
        "—".to_string()
    };
    let mem = if active {
        fmt_mem(s.mem_bytes)
    } else {
        "—".to_string()
    };

    data_row(
        GROUP_NAMES[i].to_string(),
        C_TEXT,
        ui_font(),
        s.count.to_string(),
        mem,
        C_TEXT,
        cpu,
        cpu_color(app.sampler.cpu_pct[i], active),
        ui_font(),
    )
}

/// Codex / Claude 的常驻行：带一层品牌色底，永远排在最上面。
fn pinned_group_row(app: &App, i: usize, accent: Color) -> Element<'static, Msg> {
    let s = &app.sampler.cur[i];
    let active = s.count > 0;
    let cpu = if active {
        fmt_pct(app.sampler.cpu_pct[i])
    } else {
        "—".to_string()
    };
    let mem = if active {
        fmt_mem(s.mem_bytes)
    } else {
        "—".to_string()
    };

    let inner = data_row(
        GROUP_NAMES[i].to_string(),
        accent,
        semibold(),
        s.count.to_string(),
        mem,
        C_TEXT,
        cpu,
        cpu_color(app.sampler.cpu_pct[i], active),
        ui_font(),
    );

    // 不再给常驻行铺底色：只保留品牌色名字，表格看起来更干净。
    inner
}

/// 给可结束的行挂右键菜单；系统进程 / 没运行的行传 None，右键落到空白菜单。
fn killable<'a>(row: Element<'a, Msg>, target: Option<KillTarget>) -> Element<'a, Msg> {
    match target {
        Some(target) => mouse_area(row)
            .on_right_press(Msg::RightOnRow(target))
            .into(),
        None => row,
    }
}

fn extra_row(e: &resmon::ExtraRow) -> Element<'static, Msg> {
    data_row(
        truncate(&e.name, 22),
        C_ALT,
        ui_font(),
        e.count.to_string(),
        fmt_mem(e.mem_bytes),
        C_TEXT,
        fmt_pct(e.cpu_pct),
        cpu_color(e.cpu_pct, true),
        ui_font(),
    )
}

fn total_row(app: &App) -> Element<'static, Msg> {
    let mut total_mem: u64 = 0;
    let mut total_cpu: f64 = 0.0;
    let mut total_n: u32 = 0;

    // 合计只统计界面上可见的行：count>0 的固定分组 + 非系统其它程序。
    for i in 0..G_COUNT {
        if i != G_RESMON && app.sampler.cur[i].count == 0 {
            continue;
        }
        total_mem += app.sampler.cur[i].mem_bytes;
        total_cpu += app.sampler.cpu_pct[i];
        total_n += app.sampler.cur[i].count;
    }
    for e in app.sampler.extras.iter().filter(|e| !e.system) {
        total_mem += e.mem_bytes;
        total_cpu += e.cpu_pct;
        total_n += e.count;
    }

    let cells = data_row(
        "合计".to_string(),
        C_TEXT,
        semibold(),
        total_n.to_string(),
        fmt_mem(total_mem),
        C_GOOD,
        fmt_pct(total_cpu),
        C_TEXT,
        semibold(),
    );

    container(cells)
        .width(Length::Fixed(CONTENT_W))
        .height(Length::Fixed(TOTAL_H))
        .align_y(Alignment::Center)
        .into()
}

fn menu_item(label: String, color: Color, message: Msg) -> Element<'static, Msg> {
    button(
        container(text(label).size(12.0).color(color))
            .width(Length::Fill)
            .align_x(Alignment::Start),
    )
    .width(Length::Fill)
    .height(Length::Fixed(24.0))
    .padding([0.0, 8.0])
    .style(menu_button_style)
    .on_press(message)
    .into()
}

fn positioned(menu: Element<'static, Msg>) -> Element<'static, Msg> {
    container(menu)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding {
            top: 34.0,
            right: 10.0,
            bottom: 0.0,
            left: 0.0,
        })
        .align_x(Alignment::End)
        .align_y(Alignment::Start)
        .into()
}

fn main_menu(app: &App) -> Element<'static, Msg> {
    let mut items: Vec<Element<'static, Msg>> = Vec::new();

    if let Some(target) = &app.kill_target {
        let n = target.count(app);
        if n > 0 {
            items.push(menu_item(
                format!("结束 {}（{n}）", truncate(&target.label(app), 14)),
                C_BAD,
                Msg::AskKill,
            ));
            items.push(Space::new().height(Length::Fixed(2.0)).into());
            items.push(separator());
            items.push(Space::new().height(Length::Fixed(2.0)).into());
        }
    }

    let check = if app.topmost { "✓ " } else { "   " };
    items.push(menu_item(
        format!("{check}总在最前"),
        C_TEXT,
        Msg::ToggleTopmost,
    ));
    items.push(Space::new().height(Length::Fixed(2.0)).into());
    items.push(separator());
    items.push(Space::new().height(Length::Fixed(2.0)).into());
    items.push(menu_item(
        "复制数据   Ctrl+C".to_string(),
        C_TEXT,
        Msg::Copy,
    ));

    container(column(items).width(Length::Fill).spacing(1.0))
        .width(Length::Fixed(176.0))
        .padding(4.0)
        .style(menu_panel_style)
        .into()
}

fn confirm_menu(app: &App) -> Element<'static, Msg> {
    let Some(target) = &app.kill_target else {
        return main_menu(app);
    };
    let n = target.count(app);
    let mem = target.mem(app);
    let label = truncate(&target.label(app), 18);

    let confirm = container(
        column![
            text(format!("结束 {label}？"))
                .size(12.0)
                .font(semibold())
                .color(C_TEXT),
            Space::new().height(Length::Fixed(4.0)),
            text(format!(
                "共 {n} 个进程 · {}，未保存的数据会丢失。",
                fmt_mem(mem)
            ))
            .size(11.0)
            .color(C_DIM)
            .width(Length::Fill),
            Space::new().height(Length::Fixed(6.0)),
            row![
                button(text("取消").size(12.0).color(C_TEXT))
                    .width(Length::Fill)
                    .height(Length::Fixed(24.0))
                    .padding([0.0, 8.0])
                    .style(menu_button_style)
                    .on_press(Msg::CancelKill),
                button(text("确认结束").size(12.0).color(C_BAD))
                    .width(Length::Fill)
                    .height(Length::Fixed(24.0))
                    .padding([0.0, 8.0])
                    .style(menu_button_style)
                    .on_press(Msg::ConfirmKill),
            ]
            .spacing(4.0),
        ]
        .width(Length::Fill)
        .spacing(0.0),
    )
    .width(Length::Fixed(206.0))
    .padding(8.0)
    .style(menu_panel_style);

    confirm.into()
}

fn menu_overlay(app: &App) -> Element<'static, Msg> {
    if app.confirm_kill {
        positioned(confirm_menu(app))
    } else {
        positioned(main_menu(app))
    }
}

fn view(app: &App) -> Element<'_, Msg> {
    let mut body = column![
        title_row(app),
        Space::new().height(Length::Fixed(6.0)),
        header_row(),
        Space::new().height(Length::Fixed(6.0)),
    ]
    .width(Length::Fill);

    for i in 0..G_COUNT {
        // 零占用的固定分组不渲染（任务监控自己除外），窗口高度随之收缩。
        if i != G_RESMON && app.sampler.cur[i].count == 0 {
            continue;
        }
        let row = match i {
            G_CODEX => pinned_group_row(app, i, C_CODEX),
            G_CLAUDE => pinned_group_row(app, i, C_CLAUDE),
            G_KIMI => pinned_group_row(app, i, C_KIMI),
            _ => group_row(app, i),
        };
        let target = if i != G_RESMON && app.sampler.cur[i].count > 0 {
            Some(KillTarget::Group(i))
        } else {
            None
        };
        body = body.push(killable(row, target));
    }

    let shown = shown_extras(app);
    if shown > 0 {
        body = body.push(Space::new().height(Length::Fixed(4.0)));
        body = body.push(separator());
        body = body.push(Space::new().height(Length::Fixed(4.0)));

        let mut list = column![].width(Length::Fixed(CONTENT_W));
        for e in app.sampler.extras.iter().filter(|e| !e.system) {
            let target = (e.count > 0).then(|| KillTarget::Extra {
                exe: e.exe.clone(),
                label: e.name.clone(),
                count: e.count,
                mem_bytes: e.mem_bytes,
            });
            list = list.push(killable(extra_row(e), target));
        }

        // 滚动条隐藏：窗口高度已经按屏幕上限算好，需要滚的时候滚轮可用，
        // 但不再让一条竖轨压住 CPU 数字。
        let extras: Element<'_, Msg> = match extras_viewport(app) {
            Some(h) => scrollable(list)
                .width(Length::Fixed(CONTENT_W))
                .height(Length::Fixed(h))
                .style(extras_scroll_style)
                .into(),
            None => list.into(),
        };
        body = body.push(extras);
    }

    body = body.push(Space::new().height(Length::Fixed(6.0)));
    body = body.push(separator());
    body = body.push(Space::new().height(Length::Fixed(4.0)));
    body = body.push(total_row(app));
    body = body.push(Space::new().height(Length::Fixed(PAD_Y)));

    // 面板铺满整个窗口：窗口尺寸和内容高度按同一组常量计算，
    // 不会出现底部留一大块空、或右边露出桌面的情况。
    let panel = container(body)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(iced::Padding {
            top: PAD_Y,
            right: PAD_X,
            bottom: 0.0,
            left: PAD_X,
        })
        .style(panel_style);

    let base: Element<'_, Msg> = mouse_area(panel)
        .on_press(Msg::BodyPress)
        .on_right_press(Msg::RightPress)
        .into();

    if !app.menu_open {
        return base;
    }

    // 菜单三层：面板 / 全屏“点空白关闭”捕获层 / 菜单本体。
    // 中间层保证点菜单外面任何位置都能关掉菜单，又不会挡住菜单按钮。
    let close_catcher: Element<'_, Msg> = mouse_area(
        Space::new()
            .width(Length::Fill)
            .height(Length::Fill),
    )
    .on_press(Msg::CloseMenu)
    .into();

    stack([
        base,
        close_catcher,
        menu_overlay(app),
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

// ---------------------------------------------------------------- 入口

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn main() -> iced::Result {
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // 单实例：第二次双击直接退出。
        let mtx = CreateMutexW(
            std::ptr::null(),
            0,
            wide("Local\\ResMonWidget_7c1f").as_ptr(),
        );
        if !mtx.is_null() && GetLastError() == ERROR_ALREADY_EXISTS {
            return Ok(());
        }
    }

    let ini = load_ini();
    let win = window::Settings {
        size: Size::new(WIDTH, BASE_H),
        position: window::Position::Specific(ini.pos),
        resizable: false,
        decorations: false,
        transparent: true,
        level: if ini.topmost {
            window::Level::AlwaysOnTop
        } else {
            window::Level::Normal
        },
        exit_on_close_request: false,
        platform_specific: window::settings::PlatformSpecific {
            skip_taskbar: true,
            ..Default::default()
        },
        ..window::Settings::default()
    };

    iced::application(
        move || (App::new(ini.clone()), Task::done(Msg::Sample)),
        update,
        view,
    )
        .title("任务监控")
        .window(win)
        .theme(|_: &App| {
            // 透明主题：窗口清屏色带 0 alpha，圆角外的像素不会留下灰色方块。
            Theme::custom(
                "任务监控",
                iced::theme::Palette {
                    background: Color::TRANSPARENT,
                    text: C_TEXT,
                    primary: C_GOOD,
                    success: C_GOOD,
                    warning: C_WARN,
                    danger: C_BAD,
                },
            )
        })
        .default_font(ui_font())
        .subscription(subscription)
        .run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_from_civil_epoch_and_span() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(
            days_from_civil(2026, 1, 31) - days_from_civil(2026, 1, 1),
            30
        );
        assert!(parse_date("2026-12-19").is_some());
        assert!(parse_date("2026-13-01").is_none());
        assert!(parse_date("not-a-date").is_none());
    }
}
