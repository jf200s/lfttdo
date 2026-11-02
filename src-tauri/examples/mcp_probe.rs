//! 风险验证探针（非生产代码）。它只作为 `cargo run --example mcp_probe` 运行，
//! 不会被 `mod` 进 `src/`，因此对 `qwen-do` 应用二进制完全不可达。
//!
//! 回答 spec 第 11 节风险 #2 的三件事：
//! 1) rmcp 当前版本（3.5.1）的 client + stdio 传输到底怎么构造
//!    —— client 侧走 `rmcp::transport::TokioChildProcess`（它内部消费一个
//!    `tokio::process::Command`），server 侧才是 `rmcp::transport::stdio()`。
//! 2) Windows 上 `npx`（实为 `npx.cmd`）能否被 spawn。
//!    这里刻意用 `tokio::process::Command` 而不是 `std::process::Command`，因为
//!    rmcp 的 TokioChildProcess 收的就是 tokio 那种 Command，测出来的不对称才直接可复用。
//!    A 直接 "npx"、B `cmd /C npx`、C 先 `where npx` 解析绝对路径再 spawn，三者互不改写。
//! 3) 服务退出后 node 进程是否残留 —— Phase 2 只用 rmcp 自己的 `cancel()`（内部会
//!    `graceful_shutdown()`：先关 transport 即关子进程 stdin，等 3s，再 kill），
//!    探针**不**替它 taskkill，这样“rmcp 原生清理够不够”才是被测对象而非被探针代劳。
//!
//! 没有任何 API key 参与：`@modelcontextprotocol/server-everything` 是协议自带的一致性测试
//! 服务器，无需配置、无需密钥、讲 stdio。
use std::path::PathBuf;
use std::process::Command as StdCommand;

use rmcp::ServiceExt;
use rmcp::transport::TokioChildProcess;
use tokio::process::Command as TokioCommand;

/// 协议自带的 stdio 测试服务器（官方、无需密钥）。`stdio` 是它选择传输的命令行参数。
const PKG: &str = "@modelcontextprotocol/server-everything";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ================= Phase 1：三种 spawn 策略的“可行性” =================
    // 只测 tokio::process::Command 能否把 npx 拉起来；每种都如实打印自己的原始错误，
    // 然后由探针用 taskkill /T /F 彻底清理（把 Phase 1 的进程清干净，好让 Phase 2 的
    // 泄漏测量不被 Phase 1 的残留污染 —— 注意这里的手动清理是探针行为，不是 Plan B）。
    println!("== PHASE 1: raw spawn feasibility (tokio::process::Command) ==");

    // ---- 尝试 A：直接 spawn "npx" ----
    let mut ca = TokioCommand::new("npx");
    ca.args(["-y", PKG, "stdio"]);
    let ra = ca.spawn();
    println!("A direct npx       -> {}", describe(&ra));
    if let Ok(c) = ra {
        let pid = c.id();
        tree_kill(pid).await;
    }

    // ---- 尝试 B：cmd /C 包装 ----
    let mut cb = TokioCommand::new("cmd");
    cb.args(["/C", "npx", "-y", PKG, "stdio"]);
    let rb = cb.spawn();
    println!("B cmd /C npx       -> {}", describe(&rb));
    if let Ok(c) = rb {
        let pid = c.id();
        tree_kill(pid).await;
    }

    // ---- 尝试 C：先解析真实路径（where npx）再 spawn 绝对路径 ----
    let lines = where_npx();
    println!("C `where npx`      -> {lines:?}");
    // 优先选带 .cmd 的那条（Windows 真正可执行的 shim）；没有就退回第一条。
    let chosen: Option<PathBuf> = lines
        .iter()
        .find(|p| p.to_lowercase().ends_with(".cmd"))
        .or_else(|| lines.first())
        .map(PathBuf::from);
    println!("C resolved         -> {chosen:?}");
    if let Some(path) = chosen.clone() {
        let mut cc = TokioCommand::new(&path);
        cc.args(["-y", PKG, "stdio"]);
        let rc = cc.spawn();
        println!("C spawn(abs path)  -> {}", describe(&rc));
        if let Ok(c) = rc {
            let pid = c.id();
            tree_kill(pid).await;
        }
    }

    // 让 Phase 1 被 taskkill 掉的进程树真正消失，再进 Phase 2。
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // ================= Phase 2：真实 rmcp 握手 =================
    // 依次用 A/B/C 构造 transport，第一个能完成 initialize + list_tools 的即为赢家；
    // 关闭只用 rmcp 自己的 cancel()，不手动 taskkill —— 这是进程泄漏的被测点。
    println!("\n== PHASE 2: real rmcp handshake (serve + list_tools) ==");
    let mut candidates: Vec<(&str, Option<TokioCommand>)> = Vec::new();

    let mut ha = TokioCommand::new("npx");
    ha.args(["-y", PKG, "stdio"]);
    candidates.push(("A-direct-npx", Some(ha)));

    let mut hb = TokioCommand::new("cmd");
    hb.args(["/C", "npx", "-y", PKG, "stdio"]);
    candidates.push(("B-cmd-C", Some(hb)));

    candidates.push((
        "C-resolved",
        chosen.map(|p| {
            let mut hc = TokioCommand::new(p);
            hc.args(["-y", PKG, "stdio"]);
            hc
        }),
    ));

    // 默认顺序 A,B,C（首个成功即停）。可用 MCP_PROBE_ORDER=C-resolved,B-cmd-C 把某种
    // 策略提前，让“每种策略各自的真实结果”都能被单独量到，而不是被 A/B 短路掉
    // （取值必须是下面 push 的完整 label，写 C/B 这种短名不会命中、会静默退回默认序）。
    if let Ok(spec) = std::env::var("MCP_PROBE_ORDER") {
        let want: Vec<String> = spec.split(',').map(|t| t.trim().to_string()).collect();
        candidates
            .sort_by_key(|(label, _)| want.iter().position(|w| w == label).unwrap_or(usize::MAX));
    }
    let mut winner: Option<&str> = None;
    let mut winner_pid: Option<u32> = None;

    for (label, maybe_cmd) in candidates {
        let Some(cmd) = maybe_cmd else {
            println!("handshake {label}: skipped (no resolved path)");
            continue;
        };
        // TokioChildProcess::new 会真正 spawn；失败（如 npx 找不到）时不产生进程。
        let tc = match TokioChildProcess::new(cmd) {
            Ok(tc) => tc,
            Err(e) => {
                println!(
                    "handshake {label}: spawn ERR kind={:?} os={:?} {e}",
                    e.kind(),
                    e.raw_os_error()
                );
                continue;
            }
        };
        let pid = tc.id();
        println!("handshake {label}: child spawned pid={pid:?}");

        // serve() 消费 transport，内部完成 initialize 握手。
        let service = match ().serve(tc).await {
            Ok(s) => s,
            Err(e) => {
                println!("handshake {label}: initialize ERR {e:?}");
                continue;
            }
        };

        // 先绑定结果再 match：这样在 arm 里 move `service`（cancel/drop）不会有借用歧义。
        let list_res = service.peer().list_tools(Default::default()).await;
        match list_res {
            Ok(res) => {
                let n = res.tools.len();
                let first3: Vec<String> = res
                    .tools
                    .iter()
                    .take(3)
                    .map(|t| t.name.to_string())
                    .collect();
                println!("handshake {label}: tools={n} first3={first3:?}");
                winner = Some(label);
                winner_pid = pid;
                // 只靠 rmcp 关闭：cancel() -> RunningService::close() -> transport.close()
                // -> TokioChildProcess::graceful_shutdown()（关 stdin，等 3s，超时才 kill）。
                match service.cancel().await {
                    Ok(reason) => {
                        println!("handshake {label}: cancelled via rmcp, quit={reason:?}")
                    }
                    Err(e) => println!("handshake {label}: cancel JOIN_ERR {e:?}"),
                }
                break;
            }
            Err(e) => {
                println!("handshake {label}: list_tools ERR {e:?}");
                drop(service);
                continue;
            }
        }
    }

    // 打印赢家 pid，供外部 `tasklist` 与本探针自报的 pid 对齐做残留判定。
    println!("\nHANDSHAKE_WINNER={winner:?} WINNER_CHILD_PID={winner_pid:?}");
    println!("(note: winner child was closed ONLY via rmcp::cancel(); probe did not taskkill it)");
    Ok(())
}

/// 如实描述一次 spawn 的结果：成功给出 pid，失败给出 error kind + 原始 OS 错误码 + 文本。
fn describe(r: &Result<tokio::process::Child, std::io::Error>) -> String {
    match r {
        Ok(c) => format!("ok pid={:?}", c.id()),
        Err(e) => format!("ERR kind={:?} os={:?} {e}", e.kind(), e.raw_os_error()),
    }
}

/// 用 Windows 自带 `where` 解析 npx，返回每一行候选路径（原样）。
fn where_npx() -> Vec<String> {
    match StdCommand::new("where").arg("npx").output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect(),
        Err(e) => {
            eprintln!("`where npx` failed: {e}");
            Vec::new()
        }
    }
}

/// 彻底清理一次 Phase-1 spawn：直接 `taskkill /T /F /PID` 连整棵子进程树一起干掉，
/// 并打印 taskkill 的原始输出作为清理证据（仅用于 Phase 1 的自净，Phase 2 不走这里，
/// 以测 rmcp 原生清理）。`/T` 递归杀树是必要的：B/C 直接子进程只是 cmd.exe/npx.cmd，
/// 真正的 node 服务器是它的孙进程，单杀直接子进程（如 tokio start_kill / KillProcess）会漏。
async fn tree_kill(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let status = StdCommand::new("taskkill")
        .args(["/T", "/F", "/PID", &pid.to_string()])
        .output();
    match status {
        Ok(o) => println!(
            "  [phase1 cleanup] taskkill /T /F /PID {pid} -> {}{}",
            String::from_utf8_lossy(&o.stdout).trim(),
            String::from_utf8_lossy(&o.stderr).trim(),
        ),
        Err(e) => println!("  [phase1 cleanup] taskkill spawn failed for pid {pid}: {e}"),
    }
}
