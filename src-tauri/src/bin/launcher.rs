// LFTTDO 便携启动器（R335）：一个无窗口 exe 负责整条生命周期。
//
// 它做四件事，全部只依赖标准库 + serde_json：
//   1) 把人设.txt 注入 LFTTDO\data\settings.json 的 system_prompt；
//   2) 若 127.0.0.1:11435 没有服务，就用 U 盘的 ollama.exe 起一个（模型目录指向 U 盘）；
//   3) 启动 LFTTDO\lfttdo.exe 并等它退出；
//   4) 主窗口退出后，杀掉**本次启动的** Ollama 进程树，然后自己退出。
//
// 为什么不是批处理：双击批处理会留一个黑窗口，关它不等于关应用；做成 windows 子系统 exe
// 之后，用户只看得到主窗口，关掉主窗口这条链自然收尾。
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const OLLAMA_PORT: u16 = 11435;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn main() {
    // 启动器就放在 aidox 根目录（与 LFTTDO\、ollama\、ollama-models\、人设.txt 同级）
    let Some(root) = exe_dir() else { return };
    let app = root.join("LFTTDO").join("lfttdo.exe");
    if !app.is_file() {
        return;
    }
    inject_persona(&root);
    let started = ensure_ollama(&root);
    let Ok(mut app_child) = Command::new(&app).spawn() else {
        stop_ollama(started);
        return;
    };
    let _ = app_child.wait();
    stop_ollama(started);
}

fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok()?.parent().map(Path::to_path_buf)
}

fn port_open() -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], OLLAMA_PORT));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

/// 起 U 盘的 Ollama；已经有人在 11435 上服务时返回 None（那种进程不归我们关）。
fn ensure_ollama(root: &Path) -> Option<Child> {
    if port_open() {
        return None;
    }
    // Ollama 与模型目录在 aidox 的上一级（E:\glmai\ollama*），与应用目录同级但不在其内
    let base = root.parent().map(Path::to_path_buf).unwrap_or_else(|| root.to_path_buf());
    let exe = base.join("ollama").join("ollama.exe");
    if !exe.is_file() {
        return None;
    }
    let mut cmd = Command::new(&exe);
    cmd.arg("serve")
        .env("OLLAMA_MODELS", base.join("ollama-models"))
        .env("OLLAMA_HOST", format!("127.0.0.1:{OLLAMA_PORT}"))
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn().ok()
}

fn stop_ollama(child: Option<Child>) {
    let Some(mut child) = child else { return };
    let pid = child.id();
    let mut kill = Command::new("taskkill");
    kill.args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        kill.creation_flags(CREATE_NO_WINDOW);
    }
    // /T 连模型 runner 一起收；失败再退回直接 kill，保证不会把 Ollama 留在内存里。
    let _ = kill.status();
    let _ = child.kill();
    let _ = child.wait();
}

fn inject_persona(root: &Path) {
    let persona = root.join("人设.txt");
    let settings = root.join("LFTTDO").join("data").join("settings.json");
    let Ok(text) = std::fs::read_to_string(&persona) else {
        return;
    };
    let Ok(raw) = std::fs::read_to_string(&settings) else {
        return;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return;
    };
    let Some(map) = value.as_object_mut() else { return };
    map.insert(
        "system_prompt".to_string(),
        serde_json::Value::String(text.trim().replace("\r\n", "\n")),
    );
    if let Ok(out) = serde_json::to_string_pretty(&value) {
        let _ = std::fs::write(&settings, out + "\n");
    }
}
