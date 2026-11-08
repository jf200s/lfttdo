//! workflow（G24）：工作流编排工具 —— 取 ZCode /workflow 的「步骤编排 + 结果传递」
//! 精华，不做重型 DSL：一个工作流就是 workspace 下的一份 `workflows/<name>.json`
//! （name、可选 description、steps），run 时按顺序把每一步派给子代理（经
//! `ToolCtx.subagents`，与 `task` 同一条接缝），上一步的结果经 `{{步骤名}}` 注入
//! 下一步的 prompt。
//!
//! # 为什么是 Risk::Write
//!
//! 编排本身没有新增副作用面：定义是纯数据（解析即验），执行全权委托给子代理 ——
//! 但「派子代理执行任意组合动作」正是 `task` 定为 Write 的同一条理由（连续烧多轮
//! 模型请求、可能改文件），workflow 是它的 N 连发，只会更重；降成 Read 等于把一串
//! 该弹窗的调用折叠成一次静默放行。
//!
//! # 目录定位与文件名约定
//!
//! 与 undo 同一判据：write_root 优先、缺省 roots[0]。目录不存在 = 没有工作流
//! （list 给「放哪里」的提示，run 报没有这个名字）—— 定义文件由用户自己放，
//! 工具从不代写。run 用的名字是**文件名**（`workflows/<name>.json` 的 `<name>`）；
//! list 展示的是定义里的 name —— 约定两者一致，不一致时以文件名为准。
//!
//! # 步骤可见性事件（G26）
//!
//! run 循环在每步**开始前**经 `ctx.events` emit `AgentEvent::WorkflowStep
//! { status: Running }`，成功后 emit `Done`，失败（含用户取消）emit `Failed`
//! 后照旧返回错误 —— 用户在逐步报告之外，还能看到「跑到第几步」。进度是观察面
//! 不是执行面：`ctx.events` 为 `None`（headless 会话）时静默不发，工作流照跑
//! 不误；`EventSink::emit` 返回 `()`，事件出口的任何故障都不会阻塞或打断编排。
//! 前端 reducer 把它降级成通知条（与 G20 的 question 同一条路）。
use crate::agent::events::{AgentEvent, StepStatus};
use crate::agent::types::{Risk, ToolOutput};
use crate::tools::output_bound::bound_tool_output;
use crate::tools::registry::{Tool, ToolCtx, ToolError, ToolMeta};
use crate::tools::todo::usable_with_roots as base_usable;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub const NAME: &str = "workflow";

/// 复用 todo 的可用判据（roots 非空）：工作流的落点同样是 write_root / roots[0]
///（与 undo 同一取向 —— 判据偏保守没关系，两个都为空时工具内部另有明确报错）。
pub fn usable_with_roots(roots: &[PathBuf]) -> bool {
    base_usable(roots)
}

/// 单次 `{{引用}}` 注入的字符上限（按字符不按字节：字节切会碎在 UTF-8 序列中间）。
/// 上一步可能交回一大段调研报告，原样全量注入会把下一步的 prompt 顶爆 —— 超出
/// 截断并注明，保住的是「引用上一步的要点」这个用法，不是「把上一步全文复读」。
const INJECT_MAX_CHARS: usize = 8000;
/// 报告里单步结果的字符上限。报告要「逐步」可读，任何一步都不该独占输出；
/// 超长截断注明（完整内容反正已在子代理会话与 output_bound 的落盘里）。
const REPORT_STEP_MAX_CHARS: usize = 4000;

/// 扫描 prompt 里的 `{{名字}}` 引用，返回 (起始字节, 结束字节, trim 后的名字)。
/// 两条规则：
/// · `{{` 找不到闭合 `}}` 时**当字面量**（跳过这两个字节继续扫，不产出引用）——
///   模板语法残缺不该让整段 prompt 失效；
/// · 名字两侧的空白不参与匹配（`{{ 调研 }}` 与 `{{调研}}` 是同一个引用）。
/// 校验（parse_workflow）与替换（substitute）对「什么算引用」的认知都来自这一份
/// 实现，两处不会分叉。
fn scan_refs(prompt: &str) -> Vec<(usize, usize, String)> {
    let bytes = prompt.as_bytes();
    let mut refs = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            match prompt[i + 2..].find("}}") {
                Some(rel) => {
                    let end = i + 2 + rel + 2;
                    refs.push((i, end, prompt[i + 2..i + 2 + rel].trim().to_string()));
                    i = end;
                }
                None => i += 2,
            }
        } else {
            i += 1;
        }
    }
    refs
}

/// 单次注入的限幅：超长按字符边界截断并注明。
fn inject_bound(value: &str) -> String {
    if value.chars().count() <= INJECT_MAX_CHARS {
        return value.to_string();
    }
    let mut head: String = value.chars().take(INJECT_MAX_CHARS).collect();
    head.push_str("…（已截断）");
    head
}

/// 把 prompt 里的 `{{步骤名}}` 替换成该步结果（超长截断注明）。未定义的引用
/// **原样保留**：prompt 里可能本来就要出现 `{{xxx}}` 这个形状（示例、别的模板
/// 语法），猜着替换或删掉都是篡改作者的原文。无引用时透传。
pub fn substitute(prompt: &str, results: &[(String, String)]) -> String {
    let refs = scan_refs(prompt);
    if refs.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::with_capacity(prompt.len());
    let mut last = 0;
    for (start, end, name) in refs {
        out.push_str(&prompt[last..start]);
        match results.iter().find(|(n, _)| *n == name) {
            Some((_, value)) => out.push_str(&inject_bound(value)),
            None => out.push_str(&prompt[start..end]),
        }
        last = end;
    }
    out.push_str(&prompt[last..]);
    out
}

/// 一个工作流定义（解析校验后的形状）。
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowDef {
    pub name: String,
    pub description: Option<String>,
    pub steps: Vec<WorkflowStep>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowStep {
    pub name: String,
    pub prompt: String,
    pub agent: Option<String>,
}

/// JSON 的原始形状：字段全部可缺 —— 缺什么由 validate 用**中文**说清，不让 serde
/// 的英文报错直接漏给模型（错误原因会原样回到模型/用户面前）。
#[derive(Deserialize)]
struct RawWorkflow {
    name: Option<String>,
    description: Option<String>,
    steps: Option<Vec<RawStep>>,
}

#[derive(Deserialize)]
struct RawStep {
    name: Option<String>,
    prompt: Option<String>,
    agent: Option<String>,
}

/// Option<String> 的「trim 后非空」守卫：空了就给 `why` 这条中文原因。
fn non_empty(v: Option<String>, why: &str) -> Result<String, String> {
    match v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(s) => Ok(s),
        None => Err(why.to_string()),
    }
}

/// 解析并校验一份工作流定义。解析即验：任何一条不满足都是整体 Err（中文原因），
/// 不存在「带病定义」跑到一半才发现 —— 校验规则：name 非空、steps 非空、
/// step.name 唯一非空、prompt 非空、`{{引用}}` 只能指向**更早**的步骤。
/// `{{` 未闭合当字面量；指向不存在的名字不拦（substitute 会原样保留）。
pub fn parse_workflow(text: &str) -> Result<WorkflowDef, String> {
    let raw: RawWorkflow =
        serde_json::from_str(text).map_err(|e| format!("工作流 JSON 解析失败: {e}"))?;
    let name = non_empty(raw.name, "工作流 name 不能为空")?;
    let description = raw
        .description
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let raw_steps = match raw.steps {
        Some(steps) if !steps.is_empty() => steps,
        _ => return Err("工作流 steps 不能为空：至少要定义一步".to_string()),
    };
    // 第一遍：步骤名（非空 + 唯一）。名字是 `{{}}` 引用的匹配键，必须先钉稳。
    let mut all_names: Vec<String> = Vec::with_capacity(raw_steps.len());
    for (idx, rs) in raw_steps.iter().enumerate() {
        let nth = idx + 1;
        let step_name = non_empty(rs.name.clone(), &format!("第 {nth} 步的 name 不能为空"))?;
        if let Some(first) = all_names.iter().position(|x| *x == step_name) {
            return Err(format!(
                "步骤名「{step_name}」重复：步骤名必须唯一（第 {} 步与第 {nth} 步）",
                first + 1
            ));
        }
        all_names.push(step_name);
    }
    // 第二遍：prompt 非空 + 引用只许指向更早的步骤。
    let mut steps = Vec::with_capacity(raw_steps.len());
    for (idx, rs) in raw_steps.iter().enumerate() {
        let nth = idx + 1;
        let step_name = all_names[idx].clone();
        let prompt = non_empty(
            rs.prompt.clone(),
            &format!("第 {nth} 步（{step_name}）的 prompt 不能为空"),
        )?;
        for (_, _, r) in scan_refs(&prompt) {
            if r == step_name {
                return Err(format!(
                    "第 {nth} 步「{step_name}」的 prompt 引用了自己：{{{{…}}}} 只能引用更早的步骤"
                ));
            }
            // 名字在表里但不在更早的集合里 = 前向引用：顺序依赖决定了它运行时
            // 只会原样漏过去（结果还没产生），宁可解析时就拒绝。
            if all_names.contains(&r) && !all_names[..idx].contains(&r) {
                return Err(format!(
                    "第 {nth} 步「{step_name}」的 prompt 引用了后面的步骤「{r}」：只能引用更早的步骤（顺序依赖）"
                ));
            }
        }
        // agent 与 task 工具同一语义：空白视作未点名（档案表在实现方查找）。
        let agent = rs
            .agent
            .clone()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        steps.push(WorkflowStep {
            name: step_name,
            prompt,
            agent,
        });
    }
    Ok(WorkflowDef {
        name,
        description,
        steps,
    })
}

/// 工作流目录：write_root 优先、缺省 roots[0]（与 undo 同一判据）。
/// 两者皆无 = 没有可读的定义面，明确报错而不是静默当空表。
fn workflows_dir(ctx: &ToolCtx) -> Result<PathBuf, ToolError> {
    let base = ctx
        .write_root
        .clone()
        .or_else(|| ctx.roots.first().cloned())
        .ok_or_else(|| {
            ToolError::BadInput("没有可用的工作目录（roots 与 write_root 都为空）".to_string())
        })?;
    Ok(base.join("workflows"))
}

/// list 空表 / 目录不存在时的提示：说清定义放哪里、长什么样，模型或用户照着
/// 就能补上 —— 「没有工作流」是状态不是错误。
fn no_workflows_note(dir: &Path) -> String {
    format!(
        "当前没有可用的工作流。把工作流定义存成 {} 下的 <名字>.json（JSON：{{ \"name\": …, \
         \"description\": … 可选, \"steps\": [{{ \"name\": …, \"prompt\": …, \"agent\": … 可选 }}] }}），\
         再执行 list 即可看到。",
        dir.display()
    )
}

/// list 的一行：「name — description（N 步）」；没有 description 就只剩
/// 「name（N 步）」。解析失败的文件如实点出（带原因），不整表报错、也不静默消失。
fn list_line(def: &WorkflowDef) -> String {
    let n = def.steps.len();
    match &def.description {
        Some(d) => format!("{} — {}（{n} 步）", def.name, d),
        None => format!("{}（{n} 步）", def.name),
    }
}

fn list_workflows(ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
    let dir = workflows_dir(ctx)?;
    let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect(),
        // 目录不存在 = 还没放过任何定义：提示文本，不是错误。
        Err(_) => return Ok(ToolOutput::text(no_workflows_note(&dir))),
    };
    files.sort();
    if files.is_empty() {
        return Ok(ToolOutput::text(no_workflows_note(&dir)));
    }
    let mut lines = Vec::with_capacity(files.len());
    for f in files {
        let stem = f
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        match std::fs::read_to_string(&f)
            .map_err(|e| e.to_string())
            .and_then(|t| parse_workflow(&t))
        {
            Ok(def) => lines.push(list_line(&def)),
            Err(reason) => lines.push(format!("{stem} — 解析失败：{reason}")),
        }
    }
    Ok(ToolOutput::text(lines.join("\n")))
}

/// 报告的一段：「## 步骤名\n<结果>」。单步超长按字符边界截断并注明 —— 报告的
/// 职责是让模型一眼看到每步交回了什么，不是任何一步的全文复读。
fn report_section(name: &str, value: &str) -> String {
    let total = value.chars().count();
    if total <= REPORT_STEP_MAX_CHARS {
        format!("## {name}\n{value}")
    } else {
        let head: String = value.chars().take(REPORT_STEP_MAX_CHARS).collect();
        format!("## {name}\n{head}…（已截断，共 {total} 字符）")
    }
}

/// G26：发一条步骤进度事件。`ctx.events` 缺席 = headless 会话，静默不发
/// （进度是观察面不是执行面，工作流照跑不误）；`emit` 返回 `()`，出口的任何
/// 故障都不会上抛打断编排。
fn emit_step(
    ctx: &ToolCtx,
    workflow: &str,
    step: &str,
    index: u32,
    total: u32,
    status: StepStatus,
) {
    if let Some(sink) = &ctx.events {
        sink.emit(AgentEvent::WorkflowStep {
            workflow: workflow.to_string(),
            step: step.to_string(),
            index,
            total,
            status,
        });
    }
}

pub struct WorkflowTool;

impl WorkflowTool {
    /// run：按序执行每一步。子代理结果按步骤名记账，后面的步骤经 `{{步骤名}}`
    /// 引用（解析期已保证只可能引用更早的步骤，所以逐序执行一定能填上）。
    async fn run_workflow(&self, raw_name: &str, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        // fail-closed：与 task 同一取向 —— 没装配子代理运行器的会话明确报错，
        // 不许静默当空操作（注册表里注册了工具壳 ≠ 任何执行点都有 runner）。
        let runner = ctx.subagents.as_ref().ok_or_else(|| {
            ToolError::Failed("当前会话不支持工作流（没有装配子代理运行器）".to_string())
        })?;
        let name = raw_name.trim();
        if name.is_empty() {
            return Err(ToolError::BadInput(
                "run 必须填 name：要执行哪个工作流".to_string(),
            ));
        }
        // 名字直接拼进路径：带路径分隔符（或 . / ..）就是越权读任意 json，
        // 在入口拒绝 —— 工作流名是文件名，不是路径。
        if name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            return Err(ToolError::BadInput(format!(
                "工作流名不能带路径分隔符: {name}"
            )));
        }
        let dir = workflows_dir(ctx)?;
        let path = dir.join(format!("{name}.json"));
        let text = std::fs::read_to_string(&path).map_err(|_| {
            ToolError::Failed(format!(
                "没有这个工作流: {name}（工作流定义放在 {} 下的 <名字>.json）",
                dir.display()
            ))
        })?;
        let def = parse_workflow(&text).map_err(ToolError::Failed)?;
        let mut results: Vec<(String, String)> = Vec::new();
        let total = def.steps.len() as u32;
        for (idx, step) in def.steps.iter().enumerate() {
            // 步间查取消：注册表的守卫只拦「工具开始之前」，管不到多步循环的
            // 中间 —— 用户在两步之间按下停止，后续步骤不许再烧请求。
            if ctx.cancel.is_cancelled() {
                return Err(ToolError::Cancelled);
            }
            let index = (idx + 1) as u32;
            emit_step(
                ctx,
                &def.name,
                &step.name,
                index,
                total,
                StepStatus::Running,
            );
            let prompt = substitute(&step.prompt, &results);
            match runner
                .run(prompt, step.agent.clone(), ctx, &ctx.cancel)
                .await
            {
                Ok(out) => {
                    results.push((step.name.clone(), out));
                    emit_step(ctx, &def.name, &step.name, index, total, StepStatus::Done);
                }
                // 用户停止就是停止：不套「第几步」的外衣，取消语义保持原样上抛。
                // 这一步没跑完，进度上同样记 Failed（用户可见的「失败」含被停）。
                Err(ToolError::Cancelled) => {
                    emit_step(ctx, &def.name, &step.name, index, total, StepStatus::Failed);
                    return Err(ToolError::Cancelled);
                }
                Err(e) => {
                    emit_step(ctx, &def.name, &step.name, index, total, StepStatus::Failed);
                    return Err(ToolError::Failed(format!(
                        "工作流「{}」在第 {} 步「{}」停止：{e}",
                        def.name, index, step.name
                    )));
                }
            }
        }
        let report = results
            .iter()
            .map(|(n, v)| report_section(n, v))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutput::text(bound_tool_output(report, ctx)))
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn meta(&self) -> ToolMeta {
        ToolMeta {
            name: NAME.into(),
            description: "按 workflows/ 目录下的工作流定义编排多个子代理：action=list 列出\
              可用工作流；action=run（name=工作流名）按顺序执行各步，每步派一个子代理\
              （可用 agent 点名档案），上一步的结果经 {{步骤名}} 注入下一步的 prompt，\
              全部完成后返回逐步报告。定义文件的写法见 list 的提示。"
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "run"],
                        "description": "list：列出可用的工作流；run：执行 name 指定的工作流。"
                    },
                    "name": {
                        "type": "string",
                        "minLength": 1,
                        "description": "run 必填：工作流名（workflows/ 下的文件名，不带 .json）。"
                    }
                },
                "required": ["action"]
            }),
            risk: Risk::Write,
        }
    }

    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("");
        match action {
            "list" => list_workflows(ctx),
            "run" => {
                let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
                self.run_workflow(name, ctx).await
            }
            other => Err(ToolError::BadInput(format!(
                "action 必须是 list 或 run，实得 \"{other}\""
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::events::EventSink;
    use crate::tools::task::SubagentRunner;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn ctx_with(roots: Vec<PathBuf>, runner: Option<Arc<dyn SubagentRunner>>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots,
            write_root: None,
            skills_dir: None,
            skills_disabled: vec![],
            subagents: runner,
            events: None,
        }
    }

    fn text_of(out: ToolOutput) -> String {
        match out {
            ToolOutput::Text { text } => text,
            other => panic!("{other:?}"),
        }
    }

    /// 脚本化 runner：记录收到的 (prompt, agent)，按脚本逐个回结果或报错。
    /// 脚本用完再被调 = panic（夹具前提坏了要炸响，不能静默给空结果）。
    struct ScriptedRunner {
        calls: Mutex<Vec<(String, Option<String>)>>,
        script: Mutex<VecDeque<Result<String, String>>>,
        /// 首次被调就拉响取消令牌（钉「步间查取消」）；false = 不干预。
        cancel_on_first_call: bool,
    }

    impl ScriptedRunner {
        fn new(script: Vec<Result<String, String>>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                script: Mutex::new(script.into()),
                cancel_on_first_call: false,
            }
        }

        fn prompts(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(p, _)| p.clone())
                .collect()
        }

        fn agents(&self) -> Vec<Option<String>> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(_, a)| a.clone())
                .collect()
        }
    }

    #[async_trait]
    impl SubagentRunner for ScriptedRunner {
        async fn run(
            &self,
            prompt: String,
            agent: Option<String>,
            _ctx: &ToolCtx,
            cancel: &CancellationToken,
        ) -> Result<String, ToolError> {
            if self.cancel_on_first_call && self.calls.lock().unwrap().is_empty() {
                cancel.cancel();
            }
            self.calls.lock().unwrap().push((prompt, agent));
            match self
                .script
                .lock()
                .unwrap()
                .pop_front()
                .expect("脚本用完了：夹具前提坏了")
            {
                Ok(text) => Ok(text),
                Err(reason) => Err(ToolError::Failed(reason)),
            }
        }
    }

    fn write_def(dir: &Path, stem: &str, def: Value) {
        std::fs::create_dir_all(dir.join("workflows")).unwrap();
        std::fs::write(
            dir.join("workflows").join(format!("{stem}.json")),
            def.to_string(),
        )
        .unwrap();
    }

    // ---- meta / 注册钉子 ----

    /// 名字 / 风险档 / schema 的钉子：workflow 必须是 Write（弹窗），action 二选一、
    /// name 声明给模型但只在 run 时必填。
    #[test]
    fn meta_pins_name_risk_and_schema() {
        let m = WorkflowTool.meta();
        assert_eq!(m.name, "workflow");
        assert_eq!(
            m.risk,
            Risk::Write,
            "派子代理执行任意组合动作 = task 的 N 连发；降 Read 等于绕开权限门"
        );
        assert_eq!(
            m.input_schema["properties"]["action"]["enum"],
            json!(["list", "run"])
        );
        assert_eq!(m.input_schema["required"], json!(["action"]));
        assert_eq!(m.input_schema["properties"]["name"]["type"], "string");
        assert_eq!(m.input_schema["properties"]["name"]["minLength"], 1);
    }

    #[test]
    fn workflow_tool_impls_the_tool_trait_object() {
        let registry = crate::tools::registry::ToolRegistry::new();
        registry.register(Arc::new(WorkflowTool));
        assert!(registry.get(NAME).is_some());
        assert_eq!(registry.risk_of(NAME), Risk::Write);
        assert!(usable_with_roots(&[PathBuf::from("/tmp")]));
        assert!(!usable_with_roots(&[]));
    }

    // ---- parse_workflow：正例与负例 ----

    #[test]
    fn a_valid_definition_parses_with_optional_fields() {
        let def = parse_workflow(
            r#"{"name":"调研流水","description":"两步调研","steps":[
                {"name":"调研","prompt":"查 X"},
                {"name":"总结","prompt":"基于 {{ 调研 }} 总结","agent":" explore "}]}"#,
        )
        .unwrap();
        assert_eq!(def.name, "调研流水");
        assert_eq!(def.description.as_deref(), Some("两步调研"));
        assert_eq!(def.steps.len(), 2);
        assert_eq!(
            def.steps[1].agent.as_deref(),
            Some("explore"),
            "agent trim 后透传"
        );
    }

    #[test]
    fn broken_definitions_are_rejected_in_chinese() {
        // 前向引用：第 1 步引用第 2 步的名字。
        let fwd = r#"{"name":"w","steps":[
            {"name":"a","prompt":"看 {{b}}"},{"name":"b","prompt":"查"}]}"#;
        let err = parse_workflow(fwd).unwrap_err();
        assert!(
            err.contains("后面的步骤") && err.contains("b"),
            "前向引用要被拒并点名：{err}"
        );
        // 自引用。
        let self_ref = r#"{"name":"w","steps":[{"name":"a","prompt":"看 {{a}}"}]}"#;
        assert!(
            parse_workflow(self_ref).unwrap_err().contains("引用了自己"),
            "自引用要被拒"
        );
        // 重名。
        let dup = r#"{"name":"w","steps":[
            {"name":"a","prompt":"x"},{"name":"a","prompt":"y"}]}"#;
        assert!(
            parse_workflow(dup).unwrap_err().contains("重复"),
            "重名要被拒"
        );
        // 其余非空类规则（缺失与空白等价）。
        for (bad, why) in [
            (r#"{"name":"w","steps":[]}"#, "steps 不能为空"),
            (r#"{"steps":[{"name":"a","prompt":"x"}]}"#, "name 不能为空"),
            (
                r#"{"name":" ","steps":[{"name":"a","prompt":"x"}]}"#,
                "name 不能为空",
            ),
            (
                r#"{"name":"w","steps":[{"name":"a","prompt":"  "}]}"#,
                "prompt 不能为空",
            ),
            (
                r#"{"name":"w","steps":[{"name":" ","prompt":"x"}]}"#,
                "name 不能为空",
            ),
        ] {
            let err = parse_workflow(bad).unwrap_err();
            assert!(err.contains(why), "{bad} 应报「{why}」，实得 {err}");
        }
    }

    /// `{{` 未闭合当字面量：校验侧不当作引用（定义合法），替换侧原样保留。
    #[test]
    fn an_unclosed_brace_is_a_literal_not_a_reference() {
        parse_workflow(r#"{"name":"w","steps":[{"name":"a","prompt":"模板 {{x 没闭合"}]}"#)
            .expect("未闭合的 {{ 不是引用，定义应合法");
        assert_eq!(substitute("模板 {{x 没闭合", &[]), "模板 {{x 没闭合");
    }

    // ---- substitute：替换 / 截断 / 未定义透传 ----

    #[test]
    fn substitute_replaces_defined_refs_and_truncates_long_results() {
        let results = vec![
            ("第一步".to_string(), "结果一".to_string()),
            ("长步".to_string(), "字".repeat(INJECT_MAX_CHARS + 100)),
        ];
        assert_eq!(
            substitute("基于 {{第一步}} 行动", &results),
            "基于 结果一 行动"
        );
        // 名字两侧空白不参与匹配（与解析校验同一份扫描实现）。
        assert_eq!(
            substitute("基于 {{ 第一步 }} 行动", &results),
            "基于 结果一 行动"
        );
        // 未定义引用原样保留：不许猜着替换，更不许删掉。
        assert_eq!(substitute("看 {{没定义}}", &results), "看 {{没定义}}");
        // 无引用透传。
        assert_eq!(substitute("原样一句", &results), "原样一句");
        // 单次注入限幅：超出 8000 字符截断并注明，注入总量 = 上限 + 注明文案。
        let out = substitute("上一步说：{{长步}}", &results);
        assert!(out.contains("…（已截断）"), "{out}");
        assert_eq!(
            out.chars().count(),
            "上一步说：".chars().count() + INJECT_MAX_CHARS + "…（已截断）".chars().count()
        );
    }

    // ---- list ----

    #[tokio::test]
    async fn list_hints_without_a_directory_and_lists_lines_with_one() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        // 目录不存在：给「放哪里」的提示文本，不是错误。
        let hint = text_of(
            WorkflowTool
                .execute(json!({"action": "list"}), &ctx_with(roots.clone(), None))
                .await
                .unwrap(),
        );
        assert!(
            hint.contains("workflows") && hint.contains("list"),
            "{hint}"
        );
        // 空目录：同样的提示。
        std::fs::create_dir_all(tmp.path().join("workflows")).unwrap();
        let hint = text_of(
            WorkflowTool
                .execute(json!({"action": "list"}), &ctx_with(roots, None))
                .await
                .unwrap(),
        );
        assert!(hint.contains("没有可用的工作流"), "{hint}");

        // 有定义：一行一个「name — description（N 步）」。
        write_def(
            tmp.path(),
            "one",
            json!({
                "name": "调研流水",
                "description": "两步调研",
                "steps": [
                    {"name": "a", "prompt": "x"},
                    {"name": "b", "prompt": "y"}
                ]
            }),
        );
        let text = text_of(
            WorkflowTool
                .execute(
                    json!({"action": "list"}),
                    &ctx_with(vec![tmp.path().to_path_buf()], None),
                )
                .await
                .unwrap(),
        );
        assert!(text.contains("调研流水 — 两步调研（2 步）"), "{text}");
        // 坏文件如实点出（带原因），不整表报错、也不静默消失。
        std::fs::write(tmp.path().join("workflows/broken.json"), "{oops").unwrap();
        let text = text_of(
            WorkflowTool
                .execute(
                    json!({"action": "list"}),
                    &ctx_with(vec![tmp.path().to_path_buf()], None),
                )
                .await
                .unwrap(),
        );
        assert!(text.contains("broken — 解析失败"), "{text}");
        assert!(text.contains("调研流水"), "坏文件不许连坐好文件：{text}");
    }

    /// 目录定位判据与 undo 同款：write_root 优先于 roots[0]。
    #[tokio::test]
    async fn the_workflows_directory_prefers_write_root_over_roots_first() {
        let ws = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        write_def(
            out.path(),
            "w",
            json!({"name": "写目录里的", "steps": [{"name": "a", "prompt": "x"}]}),
        );
        let mut ctx = ctx_with(vec![ws.path().to_path_buf()], None);
        ctx.write_root = Some(out.path().to_path_buf());
        let text = text_of(
            WorkflowTool
                .execute(json!({"action": "list"}), &ctx)
                .await
                .unwrap(),
        );
        assert!(text.contains("写目录里的"), "write_root 优先：{text}");
    }

    // ---- run ----

    /// 三步流水：第 3 步的 prompt 引用 `{{第1步}}{{第2步}}` —— stub 收到的第三段
    /// prompt 必须带着前两步的结果；报告逐段带步骤名与结果；agent 按步透传。
    #[tokio::test]
    async fn a_three_step_pipeline_passes_results_forward_and_reports_every_step() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            tmp.path(),
            "p",
            json!({
                "name": "三步流水",
                "steps": [
                    {"name": "第1步", "prompt": "查 A"},
                    {"name": "第2步", "prompt": "查 B", "agent": "explore"},
                    {"name": "第3步", "prompt": "汇总：{{第1步}} 与 {{第2步}}"}
                ]
            }),
        );
        let runner = Arc::new(ScriptedRunner::new(vec![
            Ok("结果一".into()),
            Ok("结果二".into()),
            Ok("汇总完成".into()),
        ]));
        let ctx = ctx_with(vec![tmp.path().to_path_buf()], Some(runner.clone()));
        let out = WorkflowTool
            .execute(json!({"action": "run", "name": "p"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            runner.prompts(),
            vec![
                "查 A".to_string(),
                "查 B".to_string(),
                "汇总：结果一 与 结果二".to_string(),
            ],
            "第三步的 prompt 要带上前两步的替换结果"
        );
        assert_eq!(
            runner.agents(),
            vec![None, Some("explore".to_string()), None],
            "agent 按步透传，未点名的步是 None"
        );
        let text = text_of(out);
        for part in [
            "## 第1步",
            "结果一",
            "## 第2步",
            "结果二",
            "## 第3步",
            "汇总完成",
        ] {
            assert!(text.contains(part), "报告缺「{part}」：{text}");
        }
    }

    #[test]
    fn a_long_step_result_is_truncated_in_the_report() {
        let long = "字".repeat(REPORT_STEP_MAX_CHARS + 50);
        let section = report_section("步", &long);
        assert!(section.contains("…（已截断，共 "), "{section}");
        assert!(
            section.chars().count() < REPORT_STEP_MAX_CHARS + 100,
            "单段不许远超上限：{}",
            section.chars().count()
        );
        assert_eq!(report_section("步", "短"), "## 步\n短");
    }

    /// 某步 Err → 立即停：错误注明停在第几步（点名步骤名），后续步骤不再派。
    #[tokio::test]
    async fn a_failing_step_stops_the_workflow_and_names_the_step() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            tmp.path(),
            "p",
            json!({
                "name": "会炸的流水",
                "steps": [
                    {"name": "第一步", "prompt": "查 A"},
                    {"name": "第二步", "prompt": "查 B"},
                    {"name": "第三步", "prompt": "{{第一步}}"}
                ]
            }),
        );
        let runner = Arc::new(ScriptedRunner::new(vec![
            Ok("结果一".into()),
            Err("子代理炸了".into()),
        ]));
        let ctx = ctx_with(vec![tmp.path().to_path_buf()], Some(runner.clone()));
        let err = WorkflowTool
            .execute(json!({"action": "run", "name": "p"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m)
                if m.contains("第 2 步") && m.contains("第二步") && m.contains("子代理炸了")),
            "错误要点名停在第几步与原因：{err}"
        );
        assert_eq!(runner.prompts().len(), 2, "第二步失败后不许再派第三步");
    }

    // ---- G26：步骤进度事件 ----

    /// 记录型事件出口（与 gapcheck 的 VecSink 同形状；自含一份）。
    #[derive(Default)]
    struct EventLog(Mutex<Vec<AgentEvent>>);

    impl EventSink for EventLog {
        fn emit(&self, e: AgentEvent) {
            self.0.lock().unwrap().push(e);
        }
    }

    impl EventLog {
        fn events(&self) -> Vec<AgentEvent> {
            self.0.lock().unwrap().clone()
        }

        /// 发出过的 WorkflowStep 事件 `(workflow, step, index, total, status)`。
        fn steps(&self) -> Vec<(String, String, u32, u32, StepStatus)> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    AgentEvent::WorkflowStep {
                        workflow,
                        step,
                        index,
                        total,
                        status,
                    } => Some((workflow.clone(), step.clone(), *index, *total, *status)),
                    _ => None,
                })
                .collect()
        }
    }

    /// 三步流水：每步开始前 Running、成功后 Done，按步序交错成 6 条（3×Running+Done），
    /// index 1 起始、total 恒为步总数；sink 里只有这一种事件（进度是这条工具唯一的出站）。
    #[tokio::test]
    async fn every_step_emits_running_then_done_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            tmp.path(),
            "p",
            json!({
                "name": "三步流水",
                "steps": [
                    {"name": "第1步", "prompt": "查 A"},
                    {"name": "第2步", "prompt": "查 B"},
                    {"name": "第3步", "prompt": "汇总"}
                ]
            }),
        );
        let runner = Arc::new(ScriptedRunner::new(vec![
            Ok("一".into()),
            Ok("二".into()),
            Ok("三".into()),
        ]));
        let sink = Arc::new(EventLog::default());
        let mut ctx = ctx_with(vec![tmp.path().to_path_buf()], Some(runner));
        ctx.events = Some(sink.clone());
        WorkflowTool
            .execute(json!({"action": "run", "name": "p"}), &ctx)
            .await
            .unwrap();
        assert_eq!(
            sink.steps(),
            vec![
                ("三步流水".into(), "第1步".into(), 1, 3, StepStatus::Running),
                ("三步流水".into(), "第1步".into(), 1, 3, StepStatus::Done),
                ("三步流水".into(), "第2步".into(), 2, 3, StepStatus::Running),
                ("三步流水".into(), "第2步".into(), 2, 3, StepStatus::Done),
                ("三步流水".into(), "第3步".into(), 3, 3, StepStatus::Running),
                ("三步流水".into(), "第3步".into(), 3, 3, StepStatus::Done),
            ],
            "每步一对 Running/Done，序号 1 起始"
        );
        assert_eq!(
            sink.events().len(),
            6,
            "工具不再发别的（进度是唯一出站事件）"
        );
    }

    /// 失败路径：失败那步先 Running 再 Failed，错误结果照旧返回（进度不打断语义）；
    /// 成功的前一步仍是 Running+Done。
    #[tokio::test]
    async fn a_failing_step_emits_running_then_failed() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            tmp.path(),
            "p",
            json!({
                "name": "会炸的流水",
                "steps": [
                    {"name": "第一步", "prompt": "查 A"},
                    {"name": "第二步", "prompt": "查 B"}
                ]
            }),
        );
        let runner = Arc::new(ScriptedRunner::new(vec![
            Ok("结果一".into()),
            Err("子代理炸了".into()),
        ]));
        let sink = Arc::new(EventLog::default());
        let mut ctx = ctx_with(vec![tmp.path().to_path_buf()], Some(runner));
        ctx.events = Some(sink.clone());
        WorkflowTool
            .execute(json!({"action": "run", "name": "p"}), &ctx)
            .await
            .expect_err("失败路径照旧返回错误");
        assert_eq!(
            sink.steps(),
            vec![
                (
                    "会炸的流水".into(),
                    "第一步".into(),
                    1,
                    2,
                    StepStatus::Running
                ),
                ("会炸的流水".into(), "第一步".into(), 1, 2, StepStatus::Done),
                (
                    "会炸的流水".into(),
                    "第二步".into(),
                    2,
                    2,
                    StepStatus::Running
                ),
                (
                    "会炸的流水".into(),
                    "第二步".into(),
                    2,
                    2,
                    StepStatus::Failed
                ),
            ],
            "失败那步记 Failed，之后的步骤不再发事件"
        );
    }

    /// 没装配 runner ⇒ 明确报错（fail-closed），与 task 同一句取向。
    #[tokio::test]
    async fn run_without_a_runner_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            tmp.path(),
            "p",
            json!({"name": "w", "steps": [{"name": "a", "prompt": "x"}]}),
        );
        let err = WorkflowTool
            .execute(
                json!({"action": "run", "name": "p"}),
                &ctx_with(vec![tmp.path().to_path_buf()], None),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(s) if s.contains("没有装配子代理运行器")),
            "{err}"
        );
    }

    /// 步间查取消：第一步跑完时令牌已取消 ⇒ 第二步开跑前拦截，runner 只被调一次。
    #[tokio::test]
    async fn a_cancel_between_steps_stops_before_the_next_subagent() {
        let tmp = tempfile::tempdir().unwrap();
        write_def(
            tmp.path(),
            "p",
            json!({
                "name": "w",
                "steps": [
                    {"name": "第一步", "prompt": "查 A"},
                    {"name": "第二步", "prompt": "{{第一步}}"}
                ]
            }),
        );
        let mut runner = ScriptedRunner::new(vec![Ok("一".into()), Ok("二".into())]);
        runner.cancel_on_first_call = true;
        let runner = Arc::new(runner);
        let ctx = ctx_with(vec![tmp.path().to_path_buf()], Some(runner.clone()));
        let err = WorkflowTool
            .execute(json!({"action": "run", "name": "p"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolError::Cancelled),
            "取消保持原样上抛：{err:?}"
        );
        assert_eq!(
            runner.prompts().len(),
            1,
            "第一步之后已取消，第二步不得开跑"
        );
    }

    /// 入参不合法（action 不认识 / run 缺 name / 名字带路径）在碰 runner 之前就被拒；
    /// 名字带路径分隔符是越权读任意 json 的形状，必须拦下。
    #[tokio::test]
    async fn malformed_input_is_rejected_before_any_subagent_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let runner = Arc::new(ScriptedRunner::new(vec![]));
        let ctx = ctx_with(vec![tmp.path().to_path_buf()], Some(runner.clone()));
        for bad in [
            json!({}),
            json!({"action": "nope"}),
            json!({"action": "run"}),
            json!({"action": "run", "name": "  "}),
            json!({"action": "run", "name": "../escape"}),
            json!({"action": "run", "name": "a\\b"}),
        ] {
            WorkflowTool
                .execute(bad.clone(), &ctx)
                .await
                .expect_err("入参不合法必须报错");
        }
        assert!(
            runner.prompts().is_empty(),
            "校验失败不许惊动 runner：{:?}",
            runner.prompts()
        );
        // 名字存在但不带路径：run 找不到定义文件时报「没有这个工作流」。
        let err = WorkflowTool
            .execute(json!({"action": "run", "name": "missing"}), &ctx)
            .await
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::Failed(m) if m.contains("没有这个工作流: missing")),
            "{err}"
        );
    }
}
