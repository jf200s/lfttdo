//! T31 的真机探针（非生产代码）。findings 的读数就是它的 stdout。
//!
//! 它只回答**单元测试答不了**的问题，且每条都标明白它能证明到哪：
//! 1. 真 `npx` 走 `spawn_command` 会不会被解析成绝对路径的 `.cmd`（R305）——单测用的是假表，
//!    这一条是「本机 PATH 上真的长得像假表」；
//! 2. 真握手能不能数到工具、耗时多少（plan :887 的 `tools=N` 与握手耗时）；
//! 3. **直接子进程的 PID 与映像名**——T34「真 node 是孙进程」的前提（plan :887 点名要这条）；
//! 4. `CREATE_NO_WINDOW` 的取证尝试：`tasklist /V` 的 Window Title 列。**这条有天花板**：
//!    本探针跑在控制台里，子进程本来就会继承父控制台；两行读数相同**不能**证明标志没生效，
//!    只能说明「在控制台宿主里这种取证分辨不出」——真判据在 T37 的真窗口验收。
//! 5. `node.exe` 的在场计数（前后各一次，同时点之间给 T34 一个基线数）。
//!
//! 用法（仓库根）：
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --example mcp_bridge_probe
//! cargo run --manifest-path src-tauri/Cargo.toml --example mcp_bridge_probe -- --server server-everything
//! ```
//! 会用到 npm（`npx -y` 首次要下载包）——这是**允许的网络行为**：它是 example，不是 test，
//! `src/` 里的单测一条都不碰网络（R305）。
use std::collections::HashMap;
use std::process::Command as StdCommand;
use std::time::Instant;

use qwen_do_lib::mcp::bridge::{
    CONNECT_TIMEOUT_SECS, McpBridge, resolve_program_in_path, spawn_command,
};
use qwen_do_lib::mcp::config::ServerDef;

#[tokio::main]
async fn main() {
    let pkg = parse_server_arg();
    println!("PKG={pkg}");
    println!("CONNECT_TIMEOUT_SECS={CONNECT_TIMEOUT_SECS}");

    let mut args: Vec<String> = vec![pkg.clone()];
    if pkg.ends_with("server-everything") {
        args.push("stdio".into());
    }
    let def = ServerDef::Stdio {
        name: "probe".into(),
        command: "npx".into(),
        args,
        env: HashMap::new(),
    };

    // ① 选型（纯函数）：这里用的是**真**解析器，不是假表。
    match spawn_command(&def, &resolve_program_in_path) {
        Ok(plan) => {
            println!("PLAN program={:?}", plan.program());
            println!("PLAN args={:?}", plan.args());
            println!(
                "PLAN creation_flags=0x{:08x} create_no_window={}",
                plan.creation_flags(),
                plan.creation_flags() & 0x0800_0000 == 0x0800_0000
            );
        }
        Err(e) => {
            println!("PLAN_ERR={e}");
            println!("VERDICT=选型就失败了，后面的连接不会发生");
            return;
        }
    }

    // ② CREATE_NO_WINDOW 的取证尝试（见文件头第 4 条的天花板）。
    window_title_experiment();

    // ③ 真连接。
    let node_before = node_count();
    println!("NODE_BEFORE={node_before}");
    let bridge = McpBridge::new();
    let started = Instant::now();
    let out = bridge.connect_all(&[def]).await;
    let ms = started.elapsed().as_millis();
    println!("MS={ms}");
    match &out[0].1 {
        Ok(tools) => {
            println!("RESULT=ok");
            println!("TOOLS={tools}");
            println!("TOOL_NAMES={:?}", bridge.tool_names("probe"));
        }
        Err(reason) => {
            println!("RESULT=err");
            println!("REASON={reason}");
            println!("VERDICT=连接失败（findings 要原样贴上面这行，并标 未实测）");
        }
    }
    println!("STATUS={:?}", bridge.status_of("probe"));

    // ④ 直接子进程的 PID 与映像名 —— T34 的前提。
    let pids = bridge.pids();
    println!("PIDS={pids:?}");
    for (name, pid) in &pids {
        let line = tasklist(&["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        println!("TASKLIST[{name} {pid}]={line}");
        let verbose = tasklist(&["/V", "/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
        println!("TASKLIST_V[{name} {pid}]={verbose}");
    }

    // ⑤ 关闭 + 残留计数。`shutdown` 只走 rmcp 的 cancel（R309），进程树回收是 T34。
    bridge.shutdown().await;
    println!("STATUS_AFTER_SHUTDOWN={:?}", bridge.status_of("probe"));
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    println!("NODE_AFTER={}", node_count());
}

fn parse_server_arg() -> String {
    let mut argv = std::env::args().skip(1);
    let mut server = "server-memory".to_string();
    while let Some(arg) = argv.next() {
        if arg == "--server"
            && let Some(v) = argv.next()
        {
            server = v;
        }
    }
    if server.contains('/') {
        return server;
    }
    format!("@modelcontextprotocol/{server}")
}

/// 同一个 `cmd` 子进程，带/不带 `CREATE_NO_WINDOW` 各起一次，比较 `tasklist /V` 的 Window Title。
/// `ping -n 6` 大约 5 秒，够 `tasklist` 采样。
fn window_title_experiment() {
    for (label, flags) in [("without", 0u32), ("with", 0x0800_0000u32)] {
        let mut cmd = StdCommand::new("cmd");
        // argv 逐段传（R311 的同一纪律）：`>` 重定向是 shell 语法，这里不拼字符串，
        // 改成把子进程的 stdout/stderr 接到 null。
        cmd.args(["/c", "ping", "-n", "6", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(flags);
        }
        match cmd.spawn() {
            Ok(mut child) => {
                let pid = child.id();
                std::thread::sleep(std::time::Duration::from_millis(400));
                let line = tasklist(&["/V", "/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"]);
                println!("WINDOW_TITLE[{label} flags=0x{flags:08x}]={line}");
                let _ = child.kill();
                let _ = child.wait();
            }
            Err(e) => println!("WINDOW_TITLE[{label}] spawn 失败：{e}"),
        }
    }
    println!(
        "WINDOW_TITLE_NOTE=两行相同不能证明标志没生效：探针跑在控制台里，子进程本来就继承父控制台；真判据在 T37 真窗口验收"
    );
}

/// `tasklist` 的输出是 GBK；映像名（如 `cmd.exe`）是 ASCII，`from_utf8_lossy` 对这条判据无损。
fn tasklist(args: &[&str]) -> String {
    let out = StdCommand::new("tasklist").args(args).output();
    match out {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if text.is_empty() {
                String::from_utf8_lossy(&o.stderr).trim().to_string()
            } else {
                text
            }
        }
        Err(e) => format!("tasklist 起不来：{e}"),
    }
}

fn node_count() -> usize {
    let text = tasklist(&["/FI", "IMAGENAME eq node.exe", "/FO", "CSV", "/NH"]);
    text.lines()
        .filter(|line| line.to_ascii_lowercase().contains("node.exe"))
        .count()
}
