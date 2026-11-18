//! resmon-dump — 分类诊断工具
//!
//! 打印每个进程被归到哪一组、命中了哪条规则，以及未归组的进程明细。
//! 用的是 GUI 完全相同的 `classify` / `collect` 代码。
//!
//! 用法：
//!   resmon-dump            打印全部分组明细
//!   resmon-dump --group Pi   只看某个组（大小写不敏感）
//!   resmon-dump --unmatched   只看未归组的进程
//!   resmon-dump --rules      只打印规则命中统计
//!   resmon-dump --raw       输出 "pid\tgroup\tname"（差分测试用，与 classify_ref.exe 同格式）

use resmon::{collect, Roots, GROUP_NAMES, G_COUNT, OTHER_MEM_BYTES};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let unmatched = args.iter().any(|a| a == "--unmatched");
    let raw = args.iter().any(|a| a == "--raw");
    let rules_only = args.iter().any(|a| a == "--rules");

    let want_group: Option<String> = args
        .iter()
        .position(|a| a == "--group")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.to_lowercase());

    let roots = Roots::detect();
    let procs = collect(&roots);

    if raw {
        // 与 classify_ref.cpp 完全同格式：pid \t 组号 \t 组名
        for p in &procs {
            let g = match p.group {
                Some(i) => i as i32,
                None => -1,
            };
            let name = match p.group {
                Some(i) => GROUP_NAMES[i],
                None => "-",
            };
            println!("{}\t{}\t{}", p.pid, g, name);
        }
        return;
    }

    // 规则命中统计
    let mut rule_counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    let mut group_counts = [0usize; G_COUNT];
    let mut group_mem = [0u64; G_COUNT];
    for p in &procs {
        if let Some(g) = p.group {
            group_counts[g] += 1;
            group_mem[g] += p.mem_bytes;
            *rule_counts.entry(p.rule).or_default() += 1;
        }
    }

    if rules_only {
        println!("=== 规则命中统计 ===");
        for (r, c) in &rule_counts {
            println!("  {r:<32} {c}");
        }
        println!();
        println!("=== 分组汇总 ===");
        for i in 0..G_COUNT {
            println!(
                "  {:<12} {:>3} 进程  {:>10}",
                GROUP_NAMES[i],
                group_counts[i],
                resmon::fmt_mem(group_mem[i])
            );
        }
        return;
    }

    println!("=== 根目录配置 ===");
    println!("  workbuddy: {:?}", roots.wb);
    println!("  dsh:       {:?}", roots.dsh);
    println!("  zcode:     {:?}", roots.zcode);
    println!("  agent:     {:?}", roots.agent);
    println!();

    println!("=== 分组汇总 ===");
    for i in 0..G_COUNT {
        println!(
            "  {:<12} {:>3} 进程  {:>10}",
            GROUP_NAMES[i],
            group_counts[i],
            resmon::fmt_mem(group_mem[i])
        );
    }
    println!();

    println!("=== 规则命中统计 ===");
    let mut rules: Vec<_> = rule_counts.into_iter().collect();
    rules.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (r, c) in rules {
        println!("  {r:<32} {c}");
    }
    println!();

    if let Some(want) = &want_group {
        let idx = GROUP_NAMES
            .iter()
            .position(|g| g.to_lowercase() == *want)
            .unwrap_or_else(|| {
                eprintln!("未知分组: {want}");
                std::process::exit(1);
            });
        println!("=== 分组 {} 明细 ===", GROUP_NAMES[idx]);
        for p in procs.iter().filter(|p| p.group == Some(idx)) {
            println!(
                "  pid={:<7} {:<28} {:>10}  rule={}",
                p.pid,
                truncate(&p.name, 28),
                resmon::fmt_mem(p.mem_bytes),
                p.rule
            );
            println!("      path: {}", p.path);
            if !p.cmd.is_empty() {
                println!("      cmd:  {}", truncate(&p.cmd, 160));
            }
        }
        return;
    }

    if unmatched {
        println!("=== 未归组进程（内存 > {} 的会显示在 GUI 里）===", resmon::fmt_mem(OTHER_MEM_BYTES));
        let mut v: Vec<_> = procs.iter().filter(|p| p.group.is_none()).collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.mem_bytes));
        for p in v {
            println!(
                "  pid={:<7} {:<28} {:>10}{}",
                p.pid,
                truncate(&p.name, 28),
                resmon::fmt_mem(p.mem_bytes),
                if p.mem_bytes > OTHER_MEM_BYTES { "  <== 会显示" } else { "" }
            );
            if !p.cmd.is_empty() {
                println!("      cmd:  {}", truncate(&p.cmd, 160));
            }
        }
        return;
    }

    // 默认：全部进程的归属
    println!("=== 全部进程归属 ===");
    let mut v: Vec<_> = procs.iter().collect();
    v.sort_by_key(|p| (p.group.is_none(), std::cmp::Reverse(p.mem_bytes)));
    for p in v {
        let g = match p.group {
            Some(i) => format!("{:<10}", GROUP_NAMES[i]),
            None => format!("{:<10}", "(其它)"),
        };
        println!(
            "  {g} pid={:<7} {:<26} {:>10}  rule={}",
            p.pid,
            truncate(&p.name, 26),
            resmon::fmt_mem(p.mem_bytes),
            p.rule
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}
