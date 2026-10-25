//! T34 的真机探针（非生产代码）。
//!
//! plan :1241 要判的是**三层树**：`cmd.exe → node.exe → ping.exe`。单测
//! （`mcp::process`）只证到「直接子进程被内核带走」；这里把 node 与它自己 spawn 的 ping
//! 也数进来 —— 真 npx 的形态就是「真服务器是孙进程」。
//!
//! 用法（仓库根）：
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --example mcp_kill_probe
//! ```
//! 判据：`KILL_ALL_OK in <n> ms`；若 ping 还活着，探针打印每个 pid 的在场状态并以非零码退出。
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use lfttdo_lib::mcp::process::create_kill_job;

fn main() {
    let job = match create_kill_job() {
        Ok(job) => job,
        Err(e) => {
            println!("JOB_ERR={e}");
            std::process::exit(2);
        }
    };

    // node 内联脚本：再 spawn 一个长命 ping，并把两个 pid 打到 stdout。
    let script = "const {spawn}=require('child_process');\
                  const p=spawn('ping',['-t','127.0.0.1'],{stdio:'ignore'});\
                  console.log('NODE_PID='+process.pid+' PING_PID='+p.pid);\
                  setInterval(()=>{},1000);";
    let mut cmd = Command::new("cmd");
    cmd.args(["/c", "node", "-e", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            println!("SPAWN_ERR={e}");
            std::process::exit(2);
        }
    };
    let cmd_pid = child.id();
    job.assign(cmd_pid).expect("把 cmd 放进 job");
    println!("CMD_PID={cmd_pid}");

    // 读第一行拿 node / ping 的 pid。
    let stdout = child.stdout.take().expect("stdout 被 pipe 了");
    let mut line = String::new();
    if BufReader::new(stdout).read_line(&mut line).is_err() || !line.contains("NODE_PID=") {
        println!("HELLO_ERR=没读到 node 的自报 pid：{line:?}");
        std::process::exit(2);
    }
    let (node_pid, ping_pid) = parse_pids(&line).expect("解析 NODE_PID / PING_PID");
    println!("NODE_PID={node_pid}");
    println!("PING_PID={ping_pid}");

    let pids = [cmd_pid, node_pid, ping_pid];
    for pid in pids {
        println!("ALIVE_BEFORE[{pid}]={}", alive(pid));
    }

    let dropped_at = Instant::now();
    drop(job);

    // 每 50ms 轮询一次，全部消失即成功；3 秒还不齐就是失败（plan :1247 的例外清单）。
    while dropped_at.elapsed() < Duration::from_secs(3) {
        if pids.iter().all(|pid| !alive(*pid)) {
            println!("KILL_ALL_OK in {} ms", dropped_at.elapsed().as_millis());
            let _ = child.kill();
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    for pid in pids {
        println!("ALIVE_AFTER[{pid}]={}", alive(pid));
    }
    println!(
        "KILL_ALL_FAILED：drop(job) 之后仍有进程在世（findings 不许写成「基本可用」，见 plan :1247）"
    );
    let _ = child.kill();
    std::process::exit(1);
}

fn parse_pids(line: &str) -> Option<(u32, u32)> {
    let mut node = None;
    let mut ping = None;
    for token in line.split_whitespace() {
        if let Some(v) = token.strip_prefix("NODE_PID=") {
            node = v.parse().ok();
        }
        if let Some(v) = token.strip_prefix("PING_PID=") {
            ping = v.parse().ok();
        }
    }
    Some((node?, ping?))
}

/// `tasklist` 输出是 GBK，但 PID 是 ASCII；取不到时按「还活着」处理（失败关闭）。
fn alive(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()),
        Err(_) => true,
    }
}
