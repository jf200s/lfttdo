//! experiment（G25）：实验循环工具 —— autoresearch 的「基准 → keep/discard + git
//! 回滚 + 账本」精华移植。模型用它对工作区跑「改 → 测 → 留/弃」的自优化循环：
//!
//! · `experiment_run`：用平台 shell（unix `sh -c` / windows `cmd /C`）在工作目录
//!   跑基准命令，从输出按行解析 `METRIC 名=值`，可复跑（1..=5）取**中位数**；
//! · `experiment_keep`：把当前工作树变更 `git commit`（`experiment:` 前缀 +
//!   `Result:` 摘要），要求本实例刚跑过 run 且工作树（除 `.lfttdo`）确有变更；
//! · `experiment_discard`：工作树回滚到 HEAD（checkout + reset + clean，
//!   `.lfttdo` 账本保全）—— 与 autoresearch 插件 rollbackWorkingTree 同一语义。
//!
//! # 为什么是 Risk::Exec
//!
//! 它同时干两件最重的事：跑**任意命令**（与 run_command 同级的能力）+ 直接改
//! git 状态（keep 提交、discard 回滚工作树）。Exec 档每次调用都弹窗 —— 实验循环
//! 会在无人值守的意图下连续改文件，用户必须在每一步都看得见、拦得住。
//!
//! # 账本（.lfttdo/experiments.jsonl）
//!
//! 每次 action 各追加一行 JSON（run/keep/discard），复跑的每一次结果**全记录**
//! （中位数可从原始值重算）。账本是观察面不是授权面：写入失败只记 warn 不阻断
//! 主交付（命令结果与 git 状态照常返回）；且它**永不进 git** —— keep 的
//! `git add` 用 `:(exclude).lfttdo` 排除，discard 的 clean 用 `-e .lfttdo` 保全。
//!
//! # 同一会话内的顺序纪律
//!
//! keep 依赖**工具实例**上的 `last_run`（最近一次 run 解析到的指标）。实例随生产
//! 注册表存活于整个应用会话，所以纪律是「本实例最近一次 run」而不是「紧邻上一
//! 次调用」—— 中间再插一次 run 会覆盖 last_run，keep 保留的永远是最近一次的
//! 结果。跨实例（重建注册表）last_run 归零，keep 明确报错让模型先跑再留。
use crate::agent::events::AgentEvent;
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::tools::todo::{APP_STATE_DIR, usable_with_roots as base_usable};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub const NAME: &str = "experiment";

/// 复用 todo 的可用判据（roots 非空）：实验的落点同样是 write_root / roots[0]
///（与 undo/workflow 同一取向 —— 两个都为空时工具内部另有明确报错）。
pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    base_usable(roots)
}

/// 基准命令单次执行的超时。MVP 固定 120s：扩展点是给 run 加 `timeout_ms` 入参并
/// 钳到 600s 上限（autoresearch 的默认档），这里先不做 —— 调用方是交互式模型
/// 循环，单次实验占住会话太久不如让使用者把长基准拆成多条命令。
const RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// 命令合并输出的字节上限（超长截断保尾部可用部分）： runaway 输出不该吃满内存。
const OUTPUT_CAP_BYTES: usize = 512 * 1024;
/// 返回给模型的输出尾长度（按字符）：判读命令结果通常只看结尾（错误摘要在最后）。
const OUTPUT_TAIL_CHARS: usize = 2000;
/// 单次复跑的上限（schema 同步钉 1..=5）：实验循环的价值在「多轮小步」，
/// 单次调用跑太多遍既烧时间也让中位数失去诊断意义。
const MAX_REPEAT: u64 = 5;

/// 实例状态：最近一次 run 解析出的 (primary, metrics)。有 METRIC 才算「跑出了
/// 结果」—— 没解析到 METRIC 的 run 不设置它，keep 会明确要求先跑出指标。
#[derive(Clone, Debug)]
struct LastRun {
    primary: String,
    metrics: Vec<(String, f64)>,
}

pub struct ExperimentTool {
    last_run: Mutex<Option<LastRun>>,
}

impl ExperimentTool {
    pub fn new() -> Self {
        Self {
            last_run: Mutex::new(None),
        }
    }
}

impl Default for ExperimentTool {
    fn default() -> Self {
        Self::new()
    }
}

// ---- 纯函数：METRIC 解析与中位数 ----

/// 从一行输出解析 `METRIC 名=值`（`^METRIC\s+(\w+)=(-?[\d.]+)$`，行尾空白容忍）。
/// 手写而不用 regex crate：模式逐字固定、每行只跑一次，手写分支比正则引擎便宜，
/// 且每个拒收形状都能在测试里点名。返回 None = 该行不是指标行，安静跳过。
fn parse_metric_line(line: &str) -> Option<(String, f64)> {
    let line = line.trim_end();
    let rest = line.strip_prefix("METRIC")?;
    // \s+ 至少一个空白（"METRICtime=5" 不是指标行）。
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let (name, value) = rest.trim_start().split_once('=')?;
    // \w+（ASCII 字母数字下划线；中文指标名不支持 —— 键要进 JSON 与 git 摘要）。
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // -?[\d.]+：先验字符类再 parse —— parse 单独会放行 "1e5"/"inf" 这类
    // 字符类之外的形状，那不是本格式承诺的数字。
    let body = value.strip_prefix('-').unwrap_or(value);
    if body.is_empty() || !body.chars().all(|c| c.is_ascii_digit() || c == '.') {
        return None;
    }
    let parsed = value.parse::<f64>().ok()?;
    Some((name.to_string(), parsed))
}

/// 从整段输出解析全部指标行，**按出现顺序**收集；同名以最后一次为准
/// （基准命令通常会话内多次打印同一指标，最后一句是终值）。
fn parse_metrics(output: &str) -> Vec<(String, f64)> {
    let mut metrics: Vec<(String, f64)> = Vec::new();
    for line in output.lines() {
        if let Some((name, value)) = parse_metric_line(line) {
            match metrics.iter_mut().find(|(n, _)| *n == name) {
                Some(slot) => slot.1 = value,
                None => metrics.push((name, value)),
            }
        }
    }
    metrics
}

/// 中位数：升序排序后取中间 —— 奇数取正中，偶数取中间两数的平均（标准定义）。
fn median_of(values: &mut [f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = values.len() / 2;
    Some(if values.len() % 2 == 1 {
        values[mid]
    } else {
        (values[mid - 1] + values[mid]) / 2.0
    })
}

/// 尾部截取（按字符不按字节，字节切会碎在 UTF-8 序列中间）。
fn tail_chars(s: &str, max: usize) -> &str {
    let total = s.chars().count();
    if total <= max {
        return s;
    }
    let start = s
        .char_indices()
        .nth(total - max)
        .map(|(i, _)| i)
        .unwrap_or(0);
    &s[start..]
}

// ---- 基础设施：目录、账本、进程 ----

/// 基目录：write_root 优先，缺省 roots[0]；都没有 = 没有可实验的落点（BadInput）。
fn base_dir(ctx: &ToolCtx) -> Result<PathBuf, ToolError> {
    ctx.write_root
        .clone()
        .or_else(|| ctx.roots.first().cloned())
        .ok_or_else(|| {
            ToolError::BadInput("没有可用的工作目录（roots 与 write_root 都为空）".to_string())
        })
}

fn ledger_path(base: &Path) -> PathBuf {
    base.join(APP_STATE_DIR).join("experiments.jsonl")
}

/// 账本追加一行。写失败**不阻断主流程**：账本是观察面，命令结果与 git 状态才是
/// 主交付 —— 磁盘满/权限问题只 warn，不让一次成功的实验被记账失败判死。
fn append_ledger(base: &Path, entry: &Value) {
    let path = ledger_path(base);
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        std::fs::create_dir_all(path.parent().expect("账本路径必有父目录"))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(f, "{entry}")
    };
    if let Err(e) = write() {
        tracing::warn!("实验账本写入失败（不阻断）: {} : {e}", path.display());
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn metrics_json(metrics: &[(String, f64)]) -> String {
    serde_json::to_string(&metrics_object(metrics)).unwrap_or_else(|_| "{}".to_string())
}

/// 指标的 JSON 对象形状（账本里 metrics 字段用真对象，机器可读）。
fn metrics_object(metrics: &[(String, f64)]) -> Value {
    let map: serde_json::Map<String, Value> =
        metrics.iter().map(|(n, v)| (n.clone(), json!(v))).collect();
    Value::Object(map)
}

/// 字节帽截断（超长部分丢弃；接缝处最多一个替换符，无害）。
fn cap_bytes(s: String, cap: usize) -> String {
    if s.len() <= cap {
        return s;
    }
    String::from_utf8_lossy(&s.as_bytes()[..cap]).into_owned()
}

/// 平台 shell：unix `sh -c` / windows `cmd /C`。命令文本原样交给 shell ——
/// 与 run_command 同一信任模型（Exec 档弹窗是唯一的门）。
fn shell() -> (&'static str, &'static str) {
    #[cfg(unix)]
    {
        ("sh", "-c")
    }
    #[cfg(windows)]
    {
        ("cmd", "/C")
    }
}

/// 512 KiB 字节帽内追加（超长部分继续读但不存 —— 读必须排空，子进程才不会
/// 因管道写满而卡死）。
async fn drain_into<R: AsyncReadExt + Unpin>(mut r: R, buf: &Mutex<Vec<u8>>, cap: usize) {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let mut b = buf.lock().unwrap();
                if b.len() < cap {
                    let take = (cap - b.len()).min(n);
                    b.extend_from_slice(&chunk[..take]);
                }
            }
        }
    }
}

/// 跑一条基准命令：平台 shell、cwd = base、stdout+stderr 并发读取合并（保真
/// 交错顺序，任一管道单独排空都会在对方写满时死锁）、总帽 512 KiB、超时杀进程。
/// 返回 (退出码是否成功, 合并输出)。命令**自身失败不是工具错误** —— 退出码与
/// 输出原样交回模型（实验循环里「测试挂了」正是要判读的信息）。
async fn run_command(command: &str, base: &Path) -> Result<(bool, String), ToolError> {
    let (program, flag) = shell();
    let work = async {
        let mut child = Command::new(program)
            .args([flag, command])
            .current_dir(base)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // 超时路径靠 drop 杀进程：future 被 timeout 掐掉时子进程必须跟着死。
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| ToolError::Failed(format!("命令启动失败: {e}")))?;
        let out = child.stdout.take().expect("stdout 已 piped");
        let err = child.stderr.take().expect("stderr 已 piped");
        let merged = Mutex::new(Vec::new());
        tokio::join!(
            drain_into(out, &merged, OUTPUT_CAP_BYTES),
            drain_into(err, &merged, OUTPUT_CAP_BYTES),
        );
        let status = child
            .wait()
            .await
            .map_err(|e| ToolError::Failed(format!("等待命令退出失败: {e}")))?;
        let text = String::from_utf8_lossy(&merged.into_inner().unwrap()).into_owned();
        Ok((status.success(), text))
    };
    match tokio::time::timeout(RUN_TIMEOUT, work).await {
        Ok(res) => res,
        Err(_) => Err(ToolError::Failed(format!(
            "命令超时（超过 {}s 已终止）：拆小基准或缩短命令再试",
            RUN_TIMEOUT.as_secs()
        ))),
    }
}

/// 一次 git 调用：cwd = base，stdout 帽 512 KiB；非零退出 → Failed 带 stderr 尾。
async fn git(base: &Path, args: &[&str]) -> Result<String, ToolError> {
    let out = Command::new("git")
        .args(args)
        .current_dir(base)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| ToolError::Failed(format!("git 启动失败（本机没有 git？）: {e}")))?;
    if !out.status.success() {
        let stderr = cap_bytes(
            String::from_utf8_lossy(&out.stderr).into_owned(),
            OUTPUT_CAP_BYTES,
        );
        return Err(ToolError::Failed(format!(
            "git {} 失败: {}",
            args.first().unwrap_or(&""),
            tail_chars(stderr.trim(), 500)
        )));
    }
    Ok(cap_bytes(
        String::from_utf8_lossy(&out.stdout).into_owned(),
        OUTPUT_CAP_BYTES,
    ))
}

/// porcelain 一行是否指向 .lfttdo（账本永不参与 keep 的变更判定）。
/// porcelain v1 每行前两格是状态码 + 空格，路径从第 4 字节起；`.lfttdo` 是
/// ASCII 名不会被 git 加引号，前缀判断安全。
fn is_lfttdo_line(line: &str) -> bool {
    match line.get(3..) {
        Some(p) => p == ".lfttdo" || p.starts_with(".lfttdo/"),
        None => false,
    }
}

// ---- 三个 action ----

impl ExperimentTool {
    async fn run_action(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let command = input
            .get("command")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if command.is_empty() {
            return Err(ToolError::BadInput(
                "experiment_run 必填 command：要执行的基准命令".to_string(),
            ));
        }
        let repeat = match input.get("repeat") {
            None | Some(Value::Null) => 1,
            Some(v) => {
                let n = v
                    .as_u64()
                    .ok_or_else(|| ToolError::BadInput("repeat 必须是 1..=5 的整数".to_string()))?;
                if !(1..=MAX_REPEAT).contains(&n) {
                    return Err(ToolError::BadInput(format!(
                        "repeat 必须是 1..={MAX_REPEAT} 的整数，实得 {n}"
                    )));
                }
                n
            }
        };
        let base = base_dir(ctx)?;

        // 逐次复跑：每次的原始结果各记一行账本（复跑结果全记录，中位数可重算）。
        let mut all_runs: Vec<Vec<(String, f64)>> = Vec::new();
        let mut last_output = String::new();
        let mut last_code_ok = true;
        for i in 1..=repeat {
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let (ok, output) = run_command(command, &base).await?;
            let metrics = parse_metrics(&output);
            let mut entry = json!({
                "ts": now_ms(),
                "action": "run",
                "command": command,
                "repeat": repeat,
            });
            if !metrics.is_empty() {
                entry["metrics"] = metrics_object(&metrics);
                entry["primary"] = json!(metrics[0].0);
            }
            append_ledger(&base, &entry);
            // G27：这次复跑完成了，把当次指标发给用户（实验复跑 i/N 的可见性）。
            // `ctx.events` 缺席 = headless 会话，静默不发 —— 进度是观察面不是执行面，
            // 实验循环照跑不误；`emit` 返回 `()`，出口故障不会上抛打断复跑。
            if let Some(sink) = &ctx.events {
                sink.emit(AgentEvent::ExperimentRun {
                    command_tail: tail_chars(command, 60).to_string(),
                    repeat: i as u32,
                    of: repeat as u32,
                    metrics: if metrics.is_empty() {
                        serde_json::Value::Null
                    } else {
                        metrics_object(&metrics)
                    },
                });
            }
            all_runs.push(metrics);
            last_output = output;
            last_code_ok = ok;
        }

        // 跨复跑的中位数：指标名取并集（首次出现序），每个名对各次的原始值取中位。
        // primary 以首个指标名为准（模型报告与 git 摘要都只挂这一个名字）。
        let mut names: Vec<String> = Vec::new();
        for run in &all_runs {
            for (n, _) in run {
                if !names.contains(n) {
                    names.push(n.clone());
                }
            }
        }
        let mut median_rows: Vec<(String, f64, Vec<f64>)> = Vec::new();
        for name in &names {
            let mut values: Vec<f64> = all_runs
                .iter()
                .filter_map(|run| run.iter().find(|(n, _)| n == name).map(|(_, v)| *v))
                .collect();
            let raw = values.clone(); // 展示用原始序（median_of 会原地排序）
            if let Some(med) = median_of(&mut values) {
                median_rows.push((name.clone(), med, raw));
            }
        }
        let last_metrics = all_runs.last().cloned().unwrap_or_default();
        // 「跑出了结果」的判据：至少解析到一个 METRIC —— 没有指标的 run 没有
        // 可保留的东西，keep 会明确要求先跑出指标。
        if let Some((primary, _, _)) = median_rows.first() {
            let metrics = last_metrics.clone();
            *self.last_run.lock().unwrap() = Some(LastRun {
                primary: primary.clone(),
                metrics,
            });
        }

        // 报告：最后一次的 metrics + 中位数说明 + 输出尾 2000 字符。
        let mut report = format!(
            "命令退出码: {}（{} 次执行）\n",
            if last_code_ok { "0" } else { "非零" },
            all_runs.len()
        );
        if median_rows.is_empty() {
            report.push_str("没有解析到 METRIC 行（格式：METRIC 名=值）。\n");
        } else {
            let primary = median_rows[0].0.clone();
            report.push_str(&format!(
                "METRIC 汇总（{repeat} 次取中位数，primary = {primary}）：\n"
            ));
            for (name, med, raw) in &median_rows {
                let raws = raw
                    .iter()
                    .map(|v| v.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                report.push_str(&format!("- {name} = {med}（原始值: {raws}）\n"));
            }
            report.push_str(&format!(
                "metrics（最后一次）: {}\n",
                metrics_json(&last_metrics)
            ));
        }
        let total = last_output.chars().count();
        report.push_str(&format!(
            "--- 输出尾 {OUTPUT_TAIL_CHARS} 字符（共 {total} 字符）---\n"
        ));
        report.push_str(tail_chars(&last_output, OUTPUT_TAIL_CHARS));
        Ok(ToolOutput::text(report))
    }

    async fn keep_action(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if description.is_empty() {
            return Err(ToolError::BadInput(
                "experiment_keep 必填 description：这次保留改了什么、为什么".to_string(),
            ));
        }
        let base = base_dir(ctx)?;
        // 顺序纪律：keep 保留的是本实例最近一次 run 的结果（见文件头）。
        let Some(last) = self.last_run.lock().unwrap().clone() else {
            return Err(ToolError::Failed(
                "还没有可保留的实验结果：先用 experiment_run 跑出 METRIC，再 keep".to_string(),
            ));
        };
        git(&base, &["rev-parse", "--is-inside-work-tree"])
            .await
            .map_err(|_| ToolError::Failed("当前工作目录不是 git 仓库".to_string()))?;
        // 工作树变更检查（.lfttdo 账本除外）：没有变更就没有可保留的东西 ——
        // 空提交只会污染历史，让「哪次实验改了什么」变得不可读。
        let status = git(&base, &["status", "--porcelain"]).await?;
        if !status.lines().any(|l| !is_lfttdo_line(l)) {
            return Err(ToolError::Failed(
                "没有可保留的变更（工作树除 .lfttdo 外没有差异）".to_string(),
            ));
        }
        // add 用 pathspec 排除 .lfttdo：账本记录实验过程，不是工作区产物，
        // 混进实验提交会让回滚（checkout HEAD -- .）把历史也抹掉。
        git(&base, &["add", "-A", "--", ".", ":(exclude).lfttdo"]).await?;
        let result_line = format!("{} {}", last.primary, metrics_json(&last.metrics));
        let message = format!("experiment: {description}\n\nResult: {result_line}");
        git(&base, &["commit", "-m", &message]).await?;
        let hash = git(&base, &["rev-parse", "--short", "HEAD"])
            .await?
            .trim()
            .to_string();
        append_ledger(
            &base,
            &json!({
                "ts": now_ms(),
                "action": "keep",
                "note": description,
                "primary": last.primary,
                "metrics": metrics_object(&last.metrics),
                "commit": hash,
            }),
        );
        Ok(ToolOutput::text(format!(
            "已保留为 commit {hash}: experiment: {description}\nResult: {result_line}"
        )))
    }

    async fn discard_action(&self, input: &Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        if reason.is_empty() {
            return Err(ToolError::BadInput(
                "experiment_discard 必填 reason：为什么放弃这次变更".to_string(),
            ));
        }
        let base = base_dir(ctx)?;
        git(&base, &["rev-parse", "--is-inside-work-tree"])
            .await
            .map_err(|_| ToolError::Failed("当前工作目录不是 git 仓库".to_string()))?;
        // 回滚三连与 autoresearch 插件 rollbackWorkingTree 同一语义：
        // checkout 恢复已跟踪文件、reset 清空暂存区、clean 删掉未跟踪的新产物；
        // 两步 clean 排除都指向 .lfttdo —— 账本是实验的起因证据，回滚不销毁它。
        git(&base, &["checkout", "HEAD", "--", "."]).await?;
        git(&base, &["reset", "-q"]).await?;
        git(&base, &["clean", "-fd", "-e", ".lfttdo", "-e", ".lfttdo/"]).await?;
        append_ledger(
            &base,
            &json!({"ts": now_ms(), "action": "discard", "note": reason}),
        );
        Ok(ToolOutput::text(
            "已回滚：工作树恢复到 HEAD，.lfttdo 账本保留",
        ))
    }
}

#[async_trait]
impl Tool for ExperimentTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.into(),
            description: "实验循环（改 → 测 → 留/弃的自优化）：action=experiment_run \
              （command 必填，repeat 1..=5 可选）在工作目录用平台 shell 跑基准命令，\
              从输出解析 METRIC 名=值 行，多次复跑取中位数；action=experiment_keep \
              （description 必填）把当前变更 git 提交（experiment: 前缀），\
              要求本实例刚跑过 run 且工作树有变更；action=experiment_discard \
              （reason 必填）把工作树回滚到 HEAD（.lfttdo 账本保全）。\
              每次调用都记账到 .lfttdo/experiments.jsonl。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["experiment_run", "experiment_keep", "experiment_discard"],
                        "description": "run：跑基准命令；keep：保留当前变更；discard：回滚当前变更。"
                    },
                    "command": {
                        "type": "string",
                        "minLength": 1,
                        "description": "experiment_run 必填：基准命令（在 shell 里跑，输出里用 METRIC 名=值 报指标）。"
                    },
                    "repeat": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 5,
                        "description": "experiment_run 可选：复跑次数（默认 1，最多 5），各指标取中位数。"
                    },
                    "description": {
                        "type": "string",
                        "minLength": 1,
                        "description": "experiment_keep 必填：这次保留改了什么、为什么保留。"
                    },
                    "reason": {
                        "type": "string",
                        "minLength": 1,
                        "description": "experiment_discard 必填：为什么放弃这次变更。"
                    }
                },
                "required": ["action"]
            }),
            risk: Risk::Exec,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        match action {
            "experiment_run" => self.run_action(&input, ctx).await,
            "experiment_keep" => self.keep_action(&input, ctx).await,
            "experiment_discard" => self.discard_action(&input, ctx).await,
            other => Err(ToolError::BadInput(format!(
                "action 必须是 experiment_run / experiment_keep / experiment_discard 之一，实得 \"{other}\""
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::EventSink;
    use std::process::Command as StdCommand;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    fn ctx_with(roots: Vec<PathBuf>, write_root: Option<PathBuf>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots,
            write_root,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: None,
            events: None,
        }
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        }
    }

    /// 测试里同步跑 git（setup 与取证都用它）：失败即 panic（夹具前提坏了要炸响）。
    /// git 是跨平台的；`GIT_CONFIG_{GLOBAL,SYSTEM}=/dev/null` 在 Git for Windows
    /// 上同样生效（它把 /dev/null 映射到 NUL）—— Windows CI 上验证。
    #[cfg(any(unix, windows))]
    fn git_sync(dir: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git 启动失败");
        assert!(
            out.status.success(),
            "git {:?} 失败: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// 临时 git 仓库：init + 本仓库身份（不碰全局配置）+ 空 init commit。
    /// keep 的 commit 身份来自仓库级 config，CI 环境没有全局身份也能跑。
    #[cfg(any(unix, windows))]
    fn temp_git_repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git_sync(&repo, &["init"]);
        git_sync(&repo, &["config", "user.email", "g25@example.com"]);
        git_sync(&repo, &["config", "user.name", "G25"]);
        git_sync(&repo, &["commit", "--allow-empty", "-m", "init"]);
        (dir, repo)
    }

    fn read_ledger(repo: &Path) -> Vec<Value> {
        std::fs::read_to_string(repo.join(".lfttdo/experiments.jsonl"))
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).expect("账本每行都必须是合法 JSON"))
            .collect()
    }

    /// 记录型事件出口（与 gapcheck 的 VecSink 同形状；自含一份）。
    #[derive(Default)]
    struct EventLog(Mutex<Vec<AgentEvent>>);

    impl EventSink for EventLog {
        fn emit(&self, e: AgentEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    impl EventLog {
        /// 发出过的 ExperimentRun 事件 `(command_tail, repeat, of, metrics)`。
        fn runs(&self) -> Vec<(String, u32, u32, Value)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::ExperimentRun {
                        command_tail,
                        repeat,
                        of,
                        metrics,
                    } => Some((command_tail.clone(), *repeat, *of, metrics.clone())),
                    _ => None,
                })
                .collect()
        }
    }

    // ---- meta / 注册钉子 ----

    #[test]
    fn meta_pins_name_risk_and_schema() {
        let m = ExperimentTool::new().meta();
        assert_eq!(m.name, "experiment");
        assert_eq!(
            m.risk,
            Risk::Exec,
            "跑任意命令 + 直接改 git 状态：Exec 档每次调用都弹窗"
        );
        assert_eq!(
            m.input_schema["properties"]["action"]["enum"],
            json!(["experiment_run", "experiment_keep", "experiment_discard"])
        );
        assert_eq!(m.input_schema["required"], json!(["action"]));
        for field in ["command", "description", "reason"] {
            assert_eq!(m.input_schema["properties"][field]["minLength"], 1);
        }
        assert_eq!(m.input_schema["properties"]["repeat"]["minimum"], 1);
        assert_eq!(m.input_schema["properties"]["repeat"]["maximum"], 5);
    }

    #[test]
    fn experiment_tool_impls_the_tool_trait_object() {
        let registry = crate::tools::registry::ToolRegistry::new();
        registry.register(Arc::new(ExperimentTool::new()));
        assert!(registry.get(NAME).is_some());
        assert_eq!(registry.risk_of(NAME), Risk::Exec);
        assert!(usable_with_roots(&[PathBuf::from("/tmp")]));
        assert!(!usable_with_roots(&[]));
    }

    // ---- 纯函数：METRIC 解析 ----

    #[test]
    fn metric_lines_parse_names_and_values() {
        assert_eq!(
            parse_metrics("METRIC time=5\n"),
            vec![("time".to_string(), 5.0)]
        );
        // 多行 + 多名 + 浮点 + 负数。
        assert_eq!(
            parse_metrics("noise\nMETRIC time=12.5\nMETRIC delta=-3\nMETRIC acc=0.85\n"),
            vec![
                ("time".to_string(), 12.5),
                ("delta".to_string(), -3.0),
                ("acc".to_string(), 0.85),
            ]
        );
        // 行尾空白容忍（空格与 tab）。
        assert_eq!(
            parse_metrics("METRIC time=5  \t\n"),
            vec![("time".to_string(), 5.0)]
        );
        // 同名以最后一次为准（顺序保留首次出现位）。
        assert_eq!(
            parse_metrics("METRIC a=1\nMETRIC b=2\nMETRIC a=9\n"),
            vec![("a".to_string(), 9.0), ("b".to_string(), 2.0)]
        );
    }

    #[test]
    fn non_metric_lines_are_skipped_silently() {
        for bad in [
            "",                   // 空
            "metric time=5",      // 小写
            "METRICtime=5",       // 缺空白
            "METRIC time =5",     // 名字带尾空白（\w+ 不匹配）
            "METRIC time=abc",    // 值不是数字
            "METRIC time=1e5",    // 科学计数不在 -?[\d.] 字符类里
            "METRIC time=",       // 空值
            "METRIC =5",          // 空名
            "前缀 METRIC time=5", // 不在行首
        ] {
            assert!(parse_metrics(bad).is_empty(), "{bad:?} 不该解析出指标");
        }
    }

    // ---- 纯函数：中位数 ----

    #[test]
    fn median_takes_the_middle_for_odd_and_even_and_single() {
        assert_eq!(median_of(&mut [3.0, 1.0, 2.0]), Some(2.0), "奇数取正中");
        assert_eq!(
            median_of(&mut [4.0, 1.0, 3.0, 2.0]),
            Some(2.5),
            "偶数取中间两数平均"
        );
        assert_eq!(median_of(&mut [5.0]), Some(5.0), "单值即自身");
        assert_eq!(median_of(&mut []), None, "空表无中位数");
        assert_eq!(median_of(&mut [7.0, 7.0]), Some(7.0));
    }

    // ---- 入参校验（无进程依赖）----

    #[tokio::test]
    async fn no_base_dir_is_a_bad_input_for_every_action() {
        let tool = ExperimentTool::new();
        let ctx = ctx_with(vec![], None);
        for input in [
            json!({"action": "experiment_run", "command": "echo hi"}),
            json!({"action": "experiment_keep", "description": "x"}),
            json!({"action": "experiment_discard", "reason": "y"}),
        ] {
            let err = tool.execute(input, &ctx).await.unwrap_err();
            assert!(
                matches!(&err, ToolError::BadInput(m) if m.contains("没有可用的工作目录")),
                "{err}"
            );
        }
    }

    #[tokio::test]
    async fn malformed_input_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(vec![tmp.path().to_path_buf()], None);
        let tool = ExperimentTool::new();
        for (input, why) in [
            (json!({}), "action"),
            (json!({"action": "nope"}), "action"),
            (json!({"action": "experiment_run"}), "必填 command"),
            (
                json!({"action": "experiment_run", "command": "  "}),
                "必填 command",
            ),
            (
                json!({"action": "experiment_run", "command": "echo", "repeat": 0}),
                "1..=5",
            ),
            (
                json!({"action": "experiment_run", "command": "echo", "repeat": 6}),
                "1..=5",
            ),
            (json!({"action": "experiment_keep"}), "必填 description"),
            (json!({"action": "experiment_discard"}), "必填 reason"),
        ] {
            let err = tool.execute(input, &ctx).await.unwrap_err();
            assert!(err.to_string().contains(why), "应报「{why}」，实得 {err}");
        }
    }

    // ---- 进程用例（G28 起 Windows CI 覆盖）：git 跨平台，命令语法分平台 ——
    // unix 用 sh（单引号 / $n），windows 用 cmd（echo 不带引号）/ powershell（计数器）。
    // 命令语法 unix 专属（sh 特性）的用例保持 #[cfg(unix)] 并注明；逻辑跨平台的
    // 按 any(unix, windows) 或同形状 #[cfg(windows)] 镜像（Windows CI 上验证）。

    /// run：临时 git 仓库里 echo METRIC —— 指标解析正确、账本落 1 行。
    #[cfg(unix)]
    #[tokio::test]
    async fn run_parses_metrics_and_records_the_ledger() {
        let (_dir, repo) = temp_git_repo();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        let out = tool
            .execute(
                json!({"action": "experiment_run", "command": "echo 'METRIC time=5'"}),
                &ctx,
            )
            .await
            .unwrap();
        let text = text_of(out);
        assert!(text.contains("time = 5"), "{text}");
        assert!(text.contains("METRIC time=5"), "输出尾要带命令原文：{text}");
        let ledger = read_ledger(&repo);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["action"], "run");
        assert_eq!(ledger[0]["command"], "echo 'METRIC time=5'");
        assert_eq!(
            ledger[0]["metrics"],
            json!({"time": 5.0}),
            "账本里 metrics 是真对象"
        );
        assert_eq!(ledger[0]["primary"], "time");
    }

    /// Windows 镜像（与 unix 版同形状；Windows CI 上验证）：cmd 的 echo 不需要也
    /// 不该带单引号（引号会原样进输出），其余断言逐条对应。
    #[cfg(windows)]
    #[tokio::test]
    async fn run_parses_metrics_and_records_the_ledger() {
        let (_dir, repo) = temp_git_repo();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        let out = tool
            .execute(
                json!({"action": "experiment_run", "command": "echo METRIC time=5"}),
                &ctx,
            )
            .await
            .unwrap();
        let text = text_of(out);
        assert!(text.contains("time = 5"), "{text}");
        assert!(text.contains("METRIC time=5"), "输出尾要带命令原文：{text}");
        let ledger = read_ledger(&repo);
        assert_eq!(ledger.len(), 1);
        assert_eq!(ledger[0]["action"], "run");
        assert_eq!(ledger[0]["command"], "echo METRIC time=5");
        assert_eq!(
            ledger[0]["metrics"],
            json!({"time": 5.0}),
            "账本里 metrics 是真对象"
        );
        assert_eq!(ledger[0]["primary"], "time");
    }

    /// Windows 计数器命令（PowerShell 一行，经 cmd /C 原样透传）：每次执行自增并
    /// 打印 —— cmd 没有 sh 的 `$()` 算术，纯 cmd 写法要靠 `call echo %%n%%` 的
    /// 展开陷阱，晦涩易碎；windows-latest 必有 powershell.exe。Windows CI 上验证。
    #[cfg(windows)]
    const COUNTER_CMD: &str = "powershell -NoProfile -Command \"$n=0; \
         if(Test-Path c.txt){$n=[int](Get-Content c.txt | Select-Object -First 1)}; \
         $n=$n+1; Set-Content c.txt $n; Write-Output ('METRIC time='+$n)\"";

    /// 复跑取中位数：三次分别产出 1/2/3，中位数 2；每次复跑各记一行账本。
    #[cfg(unix)]
    #[tokio::test]
    async fn repeat_runs_record_each_iteration_and_report_the_median() {
        let (_dir, repo) = temp_git_repo();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        // 计数器命令：每次执行自增并打印（跨复跑可区分原始值）。
        let cmd = "n=$(cat c.txt 2>/dev/null || echo 0); n=$((n+1)); echo $n > c.txt; echo \"METRIC time=$n\"";
        let out = tool
            .execute(
                json!({"action": "experiment_run", "command": cmd, "repeat": 3}),
                &ctx,
            )
            .await
            .unwrap();
        let text = text_of(out);
        assert!(text.contains("3 次取中位数"), "{text}");
        assert!(text.contains("time = 2"), "中位数(1,2,3) = 2：{text}");
        assert!(
            text.contains("原始值: 1, 2, 3"),
            "中位数说明要带各次原始值：{text}"
        );
        let ledger = read_ledger(&repo);
        assert_eq!(ledger.len(), 3, "复跑结果全记录进账本：{}", ledger.len());
        for line in &ledger {
            assert_eq!(line["action"], "run");
            assert_eq!(line["repeat"], 3);
        }
    }

    /// Windows 镜像（与 unix 版同形状；Windows CI 上验证）：计数器换成 PowerShell
    /// 一行，中位数/账本断言逐条对应。
    #[cfg(windows)]
    #[tokio::test]
    async fn repeat_runs_record_each_iteration_and_report_the_median() {
        let (_dir, repo) = temp_git_repo();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        let out = tool
            .execute(
                json!({"action": "experiment_run", "command": COUNTER_CMD, "repeat": 3}),
                &ctx,
            )
            .await
            .unwrap();
        let text = text_of(out);
        assert!(text.contains("3 次取中位数"), "{text}");
        assert!(text.contains("time = 2"), "中位数(1,2,3) = 2：{text}");
        assert!(
            text.contains("原始值: 1, 2, 3"),
            "中位数说明要带各次原始值：{text}"
        );
        let ledger = read_ledger(&repo);
        assert_eq!(ledger.len(), 3, "复跑结果全记录进账本：{}", ledger.len());
        for line in &ledger {
            assert_eq!(line["action"], "run");
            assert_eq!(line["repeat"], 3);
        }
    }

    /// G27：repeat 循环每次复跑完成后发一条 ExperimentRun —— 按序 1/3、2/3、3/3，
    /// metrics 是**当次**解析的对象（不是中位数、也不是最后一次的值）；
    /// command_tail 是命令末 60 字符（超长截尾）。
    #[cfg(unix)]
    #[tokio::test]
    async fn repeat_runs_emit_one_experiment_run_event_per_iteration_in_order() {
        let (_dir, repo) = temp_git_repo();
        let sink = Arc::new(EventLog::default());
        let mut ctx = ctx_with(vec![repo.clone()], None);
        ctx.events = Some(sink.clone());
        let tool = ExperimentTool::new();
        // 计数器命令：每次执行自增并打印（跨复跑可区分当次指标）。
        let cmd = "n=$(cat c.txt 2>/dev/null || echo 0); n=$((n+1)); echo $n > c.txt; echo \"METRIC time=$n\"";
        tool.execute(
            json!({"action": "experiment_run", "command": cmd, "repeat": 3}),
            &ctx,
        )
        .await
        .unwrap();
        let runs = sink.runs();
        assert_eq!(
            runs,
            vec![
                (runs[0].0.clone(), 1, 3, json!({"time": 1.0}),),
                (runs[1].0.clone(), 2, 3, json!({"time": 2.0})),
                (runs[2].0.clone(), 3, 3, json!({"time": 3.0})),
            ],
            "每次复跑一条事件，序号 1 起始、按序推进，metrics 是当次值：{runs:?}"
        );
        for (tail, _, _, _) in &runs {
            assert_eq!(
                tail.chars().count(),
                60,
                "command_tail 恒为 60 字符：{tail}"
            );
            assert!(
                tail.ends_with("echo \"METRIC time=$n\""),
                "截尾保的是命令末段：{tail}"
            );
        }
    }

    /// Windows 镜像（与 unix 版同形状；Windows CI 上验证）：计数器换成 PowerShell
    /// 一行；command_tail 断言改为与命令常量的末 60 字符逐字相等（按字符截取的
    /// 判据在两个平台必须是同一条 —— tail_chars 的实现只有一份）。
    #[cfg(windows)]
    #[tokio::test]
    async fn repeat_runs_emit_one_experiment_run_event_per_iteration_in_order() {
        let (_dir, repo) = temp_git_repo();
        let sink = Arc::new(EventLog::default());
        let mut ctx = ctx_with(vec![repo.clone()], None);
        ctx.events = Some(sink.clone());
        let tool = ExperimentTool::new();
        tool.execute(
            json!({"action": "experiment_run", "command": COUNTER_CMD, "repeat": 3}),
            &ctx,
        )
        .await
        .unwrap();
        let runs = sink.runs();
        assert_eq!(
            runs,
            vec![
                (runs[0].0.clone(), 1, 3, json!({"time": 1.0})),
                (runs[1].0.clone(), 2, 3, json!({"time": 2.0})),
                (runs[2].0.clone(), 3, 3, json!({"time": 3.0})),
            ],
            "每次复跑一条事件，序号 1 起始、按序推进，metrics 是当次值：{runs:?}"
        );
        for (tail, _, _, _) in &runs {
            assert_eq!(
                tail.chars().count(),
                60,
                "command_tail 恒为 60 字符：{tail}"
            );
            assert_eq!(
                tail.as_str(),
                tail_chars(COUNTER_CMD, 60),
                "截尾保的是命令末 60 字符：{tail}"
            );
        }
    }

    /// G27 的两个边界：没解析到 METRIC 的复跑 metrics 是 `null`（不是 `{}` 也不是
    /// 缺键）；命令不足 60 字符时 command_tail 原样透传（不垫空格）。
    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_run_without_metrics_emits_null_and_a_short_command_stays_whole() {
        let (_dir, repo) = temp_git_repo();
        let sink = Arc::new(EventLog::default());
        let mut ctx = ctx_with(vec![repo.clone()], None);
        ctx.events = Some(sink.clone());
        let tool = ExperimentTool::new();
        tool.execute(
            json!({"action": "experiment_run", "command": "echo hi"}),
            &ctx,
        )
        .await
        .unwrap();
        assert_eq!(
            sink.runs(),
            vec![("echo hi".to_string(), 1, 1, Value::Null)],
            "无指标 → null，短命令原样：{:?}",
            sink.runs()
        );
    }

    /// 非 git 目录：run 照常（不需要 git），keep/discard 明确报「不是 git 仓库」。
    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn keep_and_discard_fail_loudly_outside_a_git_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx_with(vec![tmp.path().to_path_buf()], None);
        let tool = ExperimentTool::new();
        // 先跑出 METRIC（last_run 在位），keep 的报错才轮得到 git 检查。
        tool.execute(
            json!({"action": "experiment_run", "command": "echo METRIC time=1"}),
            &ctx,
        )
        .await
        .unwrap();
        let err = tool
            .execute(
                json!({"action": "experiment_keep", "description": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("不是 git 仓库")),
            "{err}"
        );
        let err = tool
            .execute(json!({"action": "experiment_discard", "reason": "y"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("不是 git 仓库")),
            "{err}"
        );
    }

    /// keep 的顺序纪律：本实例没跑过 run ⇒ 明确报错，不碰 git。
    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn keep_without_a_run_asks_for_one() {
        let (_dir, repo) = temp_git_repo();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        let err = tool
            .execute(
                json!({"action": "experiment_keep", "description": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("先用 experiment_run")),
            "{err}"
        );
    }

    /// keep 无变更：干净仓库里（账本自身被 porcelain 过滤）⇒「没有可保留的变更」。
    /// 这条同时钉住「.lfttdo 不算变更」—— 账本刚写过一行，porcelain 非空但被排除。
    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn keep_without_changes_is_rejected_and_lfttdo_does_not_count() {
        let (_dir, repo) = temp_git_repo();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        tool.execute(
            json!({"action": "experiment_run", "command": "echo METRIC time=5"}),
            &ctx,
        )
        .await
        .unwrap();
        // 夹具前提：此刻 porcelain 非空（有 .lfttdo/），排除后为空。
        let status = git_sync(&repo, &["status", "--porcelain"]);
        assert!(
            status.lines().any(|l| l.contains(".lfttdo")),
            "夹具前提：账本应在 porcelain 里出现：{status}"
        );
        let err = tool
            .execute(
                json!({"action": "experiment_keep", "description": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("没有可保留的变更")),
            "{err}"
        );
    }

    /// 完整循环：写文件 → run → keep（commit 出现、账本 2 行）→ 改坏 + 新增未跟踪
    /// 文件 → run → discard → 文件字节级恢复、未跟踪产物被清、账本保全。
    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn the_full_loop_keeps_commits_and_discard_restores_bytes() {
        let (_dir, repo) = temp_git_repo();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        let a = repo.join("a.txt");

        // ① 改：写 a.txt → run（METRIC time=5）→ keep。
        std::fs::write(&a, "v1\n").unwrap();
        tool.execute(
            json!({"action": "experiment_run", "command": "echo METRIC time=5"}),
            &ctx,
        )
        .await
        .unwrap();
        let keep_out = text_of(
            tool.execute(
                json!({"action": "experiment_keep", "description": "v1 基线"}),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert!(keep_out.contains("已保留为 commit"), "{keep_out}");
        assert_eq!(
            git_sync(&repo, &["rev-list", "--count", "HEAD"]).trim(),
            "2",
            "初始 commit + keep 的 experiment commit"
        );
        assert!(
            git_sync(&repo, &["log", "--format=%s", "-1"])
                .trim()
                .starts_with("experiment:"),
            "提交主题要带 experiment: 前缀"
        );
        let hash = git_sync(&repo, &["rev-parse", "--short", "HEAD"])
            .trim()
            .to_string();
        assert_eq!(read_ledger(&repo).len(), 2, "run + keep 各一行");

        // ② 测坏结果：改坏 a.txt + 造一个未跟踪新文件 → run（METRIC time=9）。
        std::fs::write(&a, "broken\r\nwith\tCRLF").unwrap();
        std::fs::write(repo.join("extra.txt"), "junk").unwrap();
        let run_out = text_of(
            tool.execute(
                json!({"action": "experiment_run", "command": "echo METRIC time=9"}),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert!(run_out.contains("time = 9"), "{run_out}");

        // ③ 弃：discard → a.txt 字节级恢复、未跟踪产物被清、账本保全。
        let discard_out = text_of(
            tool.execute(
                json!({"action": "experiment_discard", "reason": "回归了"}),
                &ctx,
            )
            .await
            .unwrap(),
        );
        assert!(discard_out.contains("已回滚"), "{discard_out}");
        assert_eq!(
            std::fs::read(&a).unwrap(),
            b"v1\n".to_vec(),
            "discard 后 a.txt 必须字节级等于 keep 时的内容"
        );
        assert!(
            !repo.join("extra.txt").exists(),
            "未跟踪新文件要被 clean 掉"
        );
        assert!(
            repo.join(".lfttdo/experiments.jsonl").exists(),
            "账本不参与回滚（clean 排除）"
        );
        // 账本 4 行，action 序列 run→keep→run→discard；keep 行带 commit 短哈希。
        let ledger = read_ledger(&repo);
        let actions: Vec<&str> = ledger
            .iter()
            .map(|l| l["action"].as_str().expect("action 是字符串"))
            .collect();
        assert_eq!(actions, vec!["run", "keep", "run", "discard"]);
        assert_eq!(
            ledger[1]["commit"],
            json!(hash),
            "keep 行要记 commit 短哈希"
        );
        assert_eq!(ledger[3]["note"], "回归了", "discard 行要记原因");
    }

    /// 账本写失败不阻断：.lfttdo 被一个同名**文件**占住时，追加必然失败，
    /// 但 run 的命令结果照常返回（观察面让位于主交付）。
    #[cfg(any(unix, windows))]
    #[tokio::test]
    async fn a_ledger_write_failure_does_not_block_the_run() {
        let (_dir, repo) = temp_git_repo();
        std::fs::write(repo.join(".lfttdo"), "不是目录").unwrap();
        let ctx = ctx_with(vec![repo.clone()], None);
        let tool = ExperimentTool::new();
        let out = tool
            .execute(
                json!({"action": "experiment_run", "command": "echo METRIC time=5"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(text_of(out).contains("time = 5"), "命令结果照常返回");
    }
}
