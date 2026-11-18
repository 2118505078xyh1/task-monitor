# 任务监控 — 桌面资源小挂件（原 ResMon）

Rust + [iced](https://iced.rs) 编写的迷你桌面挂件，显示以下程序的内存（工作集）与 CPU 占用（没运行的行不显示）：

- **Codex**（品牌色）
- **Claude**（品牌色）
- **Kimi CLI**
- WorkBuddy
- OpenCode
- DSH（DeepSeek Harness，含 our-free-model 引擎）
- Pi（node agent）
- Magpie（含 magpie-bun）
- ZCode（桌面端主进程 + 子进程 + zcode-runtime-host）
- 代理管理（各版本 exe）
- Argent
- 鹊桥网关（守护进程 + 面板合并计数）
- omp（oh-my-pi 主进程 + bun worker）
- 任务监控自己（始终显示，方便直接看到挂件的开销）

每 2 秒刷新一次。当前版本实测自身约 **20 MB 工作集 / 6–7 MB 私有内存**。

**固定分组里没有运行进程的行直接不显示**（任务监控自己除外），窗口高度随可见行数收缩，
不会留空行。除上述固定分组外，其他**非系统**程序会按可执行文件名分组实时检测：
**内存超过 100 MB 或 CPU 超过 5%** 时全部追加显示，不截断、不设行数上限。
系统进程不可结束，直接不进入列表（不占行、不计入合计）；列表里的每一行
都可以右键结束。放不下时只滚动“其它程序”区域——可见的固定分组和底部合计始终可见；
窗口最高不超过桌面工作区（自动避开任务栏）。
底部“合计”只统计界面上可见的行（可见固定组 + 非系统其它程序）。
系统进程的判定是“核心系统进程名或位于 `%WINDIR%` 下”；Windows Terminal、
Phone Link、Widgets、Omen 等 Store 应用仍算普通程序——它们可以被结束，
超过阈值时照常显示。

**WebView2 跟随宿主**：`msedgewebview2.exe` 不是独立程序，任务监控通过
`--webview-exe-name=` 参数和父进程链找到真正的宿主——
宿主是系统进程（如 Windows 搜索的 SearchHost）时整组隐藏；
宿主是普通程序（如 Magpie）时，WebView2 的进程数与内存并入宿主所在组，
结束宿主时子进程一起结束。不会出现“杀掉又回来”的空壳行。

## 使用

运行：

```
%LOCALAPPDATA%\Programs\ResMon\resmon.exe
```

安装目录与 exe 名保留 `resmon`（进程标识、互斥体、ini 都绑在它上面），
桌面快捷方式与界面标题是「任务监控」。

- 左键拖动可移动位置（自动记住；存的是底边，窗口变高时向上长，且始终限制在
  桌面工作区内，不会被任务栏或屏幕边缘遮住）
- 右键菜单：复制数据 / 总在最前 / 退出
- 在任意**正在运行**的行上右键 → 菜单顶部多出「结束 XXX（N）」，点它再确认一次；
  确认框会显示进程数和该行占用的总内存：
  - 固定分组（Codex / Claude / WorkBuddy / OpenCode / …）
  - 「其它程序」里的聚合行（msedge、OmenCommandCenter …）
  - 不提供的：没在运行的行（count=0）、任务监控自己
  - 系统进程不会出现在列表里，`kill_group()` / `kill_extra()` 也会再次跳过
    系统进程，误调也杀不掉
  结束操作走与统计完全相同的 `collect()` + `classify()`，不会误伤别的进程；
  任务监控自己永远不会被结束。
- 也可在挂件上按 `Ctrl+C` 复制当前数据（制表符分隔，可直接粘到表格或文本里）

## 构建

需要 Rust 工具链（<https://rustup.rs>），`cargo` 在 PATH 上即可：

```
build.bat
```

流程：`cargo build --release` → 产物复制到 `%LOCALAPPDATA%\Programs\ResMon\resmon.exe`。

release 配置已开 `opt-level="z"` + LTO + `panic="abort"` + strip，
单个 exe 约 2.3 MB，静态链接 CRT，不依赖任何运行时。

> ⚠️ 不要把 exe 放在带 Low 完整性标签的目录下运行：进程会继承低权限，
> 读不到普通（Medium）进程的数据，导致统计偏少。
> 构建时请先退出正在运行的挂件。

## 目录结构

| 文件 | 说明 |
| --- | --- |
| `src/lib.rs` | 核心库：进程采样、分类判定、阈值筛选、结束进程、格式化 |
| `src/main.rs` | GUI：iced 应用、透明圆角面板、ini 持久化、右键菜单、剪贴板 |
| `src/bin/resmon-dump.rs` | 诊断工具：打印每个进程的归属与命中规则 |
| `build.bat` | 构建 + 部署 |

GUI 与诊断工具共用 `src/lib.rs` 里的 `classify()`，
所以「看到的」和「查证的」永远是同一套代码。

## 诊断工具

```
resmon-dump.exe              # 全部进程的归属明细
resmon-dump.exe --group Pi   # 只看某个组
resmon-dump.exe --unmatched  # 只看未归组的进程
resmon-dump.exe --rules      # 规则命中统计
resmon-dump.exe --raw        # pid\t组号\t组名（差分测试用）
```

> 同样必须在 Medium 完整性目录下跑，否则读不到普通进程。

## 差分测试

2026-11-12 重写时曾用原 C++ 版的 `Classify()` 逐字拷贝建一个参照程序，
与 Rust 版对同一批进程逐条比对，95 个共同进程**分组判定 0 处不一致**。
验证完成后已按要求把 C++ 参照程序与差分脚本一并删除。

现在验证靠 `resmon-dump` 直接看判定依据（`rule=` 字段）。

## 分组规则

路径类规则不写死安装盘符：源码树/自定义安装位置用
`WORKBUDDY_ROOT`、`DSH_HOME`、`ZCODE_ROOT`、`AGENT_MANAGER_ROOT` 覆盖，
缺省只看 `%LOCALAPPDATA%\Programs\...` 与用户目录。

| 分组 | 匹配方式 | 规则名 |
|------|----------|--------|
| Codex | 进程名以 `codex` 开头，或 `node.exe` 命令行含 `@openai\codex` / `@openai/codex` / `codex.js` | `name:codex*` / `cmd:node+codex` |
| Claude | 进程名以 `claude` 开头，或 `node.exe` 命令行含 `@anthropic-ai\claude-code` / `claude-code` | `name:claude*` / `cmd:node+claude-code` |
| Kimi CLI | 进程名以 `kimi` 开头；或 `node.exe` 命令行含 `@moonshot-ai\kimi-code` / `kimi-code`；或命令行含 `.kimi-code\bin\kimi.exe` | `name:kimi*` / `cmd:node+kimi-code` / `cmd:kimi` |
| WorkBuddy | 路径以 `%WORKBUDDY_ROOT%` 或 `%USERPROFILE%\.workbuddy\` 开头，或进程名以 WorkBuddy 开头 | `path:workbuddy-root` / `name:workbuddy*` |
| OpenCode | 进程名以 `opencode` 开头（含 opencode-cli） | `name:opencode*` |
| DSH | 路径以 `%DSH_HOME%` 或 `%USERPROFILE%\.dsh\` 开头，或进程名以 DeepSeek Harness 开头 | `path:dsh-root` / `name:deepseek harness*` |
| Magpie | 进程名以 `magpie` 开头（客户端与 `serve` 都是 magpie.exe）；另有 `msedgewebview2.exe`（命令行含 `magpie.exe`）和运行 `start-serve.cmd` 的 cmd 窗口 | `name:magpie*` / `cmd:msedgewebview2+magpie.exe` / `cmd:cmd+start-serve.cmd` |
| Pi | `node.exe` 且命令行包含 `.pi\agent\` 或 `pi-launcher` | `cmd:node+pi` |
| ZCode | 路径以 `%ZCODE_ROOT%`（默认 `%LOCALAPPDATA%\Programs\ZCode\`）开头，或进程名以 `zcode` 开头（含 `ZCode.exe`、`zcode-runtime-host.exe`） | `path:zcode-root` / `name:zcode*` |
| 代理管理 | 路径以 `%AGENT_MANAGER_ROOT%`（默认 `%LOCALAPPDATA%\Programs\AgentManager\`）开头，或进程名以 `agent-manager` 开头（含 `agent-manager.exe` / `-app` / `-egui` / `-web`） | `path:agent-manager-root` / `name:agent-manager*` |
| Argent | 路径含 `.argent`（`~/.argent` 下的 CLI、桌面端与辅助进程）；或进程名以 `argent` 开头 | `path:.argent-root` / `name:argent*` |
| 鹊桥网关 | 进程名以 `queqiao-gateway` 开头（守护进程 + 面板合并计数） | `name:queqiao-gateway*` |
| omp | 进程名以 `omp` 开头（`~/.bun/bin/omp.exe` 等）；或 `bun.exe` 命令行含 `@oh-my-pi/pi-coding-agent`（worker） | `name:omp*` / `cmd:bun+omp-worker` |
| 任务监控 | 进程名精确等于 `resmon.exe`（`resmon-dump` 不会被算进来） | `name:resmon.exe` |

未归入以上固定分组的进程按可执行文件名聚合；其中**系统进程直接过滤掉**，
其余超过阈值的以较浅的颜色追加在固定行下方，并计入底部“合计”。
排序按 `max(内存/100MB, CPU/5%)` 从高到低。

内存取工作集（Working Set），CPU 按系统逻辑处理器总数归一化。

## 实现要点

- **为什么不用 wgpu**：iced 默认带 `wgpu` 渲染器，实测最小窗口就要
  260 MB 工作集（Vulkan 后端更夸张），对资源监视器完全不可接受。
  现在固定使用 `tiny-skia`（CPU 渲染）+ `crisp`，同样一个窗口
  **20 MB 工作集 / 6 MB 私有内存**；界面 1–2 秒才重绘一次，CPU 渲染绰绰有余。
- **透明圆角**：`transparent: true` + `decorations: false`，面板本身是带
  14 px 圆角的半透明容器；tiny-skia + softbuffer 在本机可直接透出桌面。
- **命令行读取**：`node.exe` / `bun.exe` / `msedgewebview2.exe` / `cmd.exe` 需要靠命令行区分归属，
  通过 `NtQueryInformationProcess` 拿 PEB，读 x64 的 `ProcessParameters/CommandLine`
  （偏移 `PEB+0x20` → `RTL_USER_PROCESS_PARAMETERS+0x70`）。
- **无控制台窗口**：`main.rs` 顶部 `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]`
  —— Rust 默认是控制台子系统（PE subsystem=3），不加这行每次启动会多一个黑窗口。
  `resmon-dump` 是命令行工具，故意保留终端。
- **单实例**：`Local\ResMonWidget_7c1f` 互斥体。
- **窗口尺寸**：行数变化时保持**底边**锚定，向上生长；ini 存 `bottom` 而不是 `y`，
  否则每次重启都会因“先按基础高度开窗、再长高”而向上漂一截。
  高度不超过工作区 − 12 px；只有“其它程序”溢出时才给该区域套 `scrollable`
  并预留 10 px 滚动条槽，固定行与合计行不参与滚动。
- **位置约束**：启动载入和每次变高都把窗口 x / 底边限制在主屏工作区（扣掉任务栏）内，
  不会被拖到屏幕外或压住任务栏。
- **右键命中**：每一行用 `mouse_area::on_right_press` 直接携带该行的
  `KillTarget`，不再把鼠标坐标反算成行号——滚动后也不会点错行。
- **结束进程**：固定分组走 `kill_group()`，「其它程序」行走 `kill_extra()`，
  两者都重新枚举 + 分类，只结束选中的组/可执行名；UI 先确认一次，
  确认后才 `TerminateProcess`，并跳过系统进程和 `resmon.exe` 自己。
- **剪贴板**：走 iced 的 `clipboard::write`，不再手写 `GlobalAlloc`。