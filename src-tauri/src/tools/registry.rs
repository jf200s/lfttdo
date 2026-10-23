// 工具层：`Tool` 抽象 + 注册表 + 执行上下文。
//
// Plan A 的生产工具表是**空的**（plan:1854：`echo` 仅测试与循环验证用、不注册进生产），
// 于是 spec 5.1「MVP 内置工具全为 Read，因此全程零弹窗」为真；Plan B 只需 `register`
// 一个 `fetch_url`，本文件的机制一行不改。
//
// 风险的唯一真相源是 `risk_of`（= 注册表里那份 `ToolMeta.risk`）。`ToolDef` 里没有 risk
// 字段，模型看不到也改不了自己的权限级别；未知工具按 `Risk::Exec`（最严的一档）处理，
// 未知即 Read 会让「没注册的工具」静默自动放行。
use crate::agent::types::{Risk, ToolOutput};
use crate::model::provider::ToolDef;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

#[derive(Error, Debug)]
pub enum ToolError {
    #[error("工具 {name} 未注册")]
    NotFound { name: String },
    #[error("入参不符合 schema: {0}")]
    BadInput(String),
    #[error("工具执行失败: {0}")]
    Failed(String),
    #[error("已取消")]
    Cancelled,
}

#[derive(Clone)]
pub struct ToolCtx {
    pub http: reqwest::Client,
    pub cancel: CancellationToken,
    pub roots: Vec<PathBuf>,
}

impl ToolCtx {
    /// 路径穿越防线。四条规则，每条都有测试钉着（R47）：
    /// 1. **只接受绝对路径**：相对路径的含义随进程 cwd 漂移，不能拿去和 root 比。
    /// 2. **canonicalize 的是候选路径的父目录**，再把最后一段原样接回去。于是
    ///    `/tmp/../etc/passwd` 会先在文件系统上把 `..` 走掉（落地 `/etc`）再比 root，
    ///    而不是拿组件序列 `[/, tmp, .., etc, passwd]` 去 `starts_with("/tmp")` ——
    ///    后者正是「brief 版只 canonicalize root」会放行穿越的原因。同时「已存在目录下的
    ///    新文件」仍可创建（整条路径 canonicalize 会把这种误判成失败）。
    /// 3. **末段是 `.` 或 `..` 直接拒**：把 `.` 接回去等于自己再造一次穿越。
    ///    （`Path::components()` 会把 `.` 归一化掉、`file_name()` 对 `..` 返回 None，
    ///    所以这条必须按词法看最后一段，不能只靠那两个 API。）
    /// 4. **任何 canonicalize 失败都返回 `BadInput`**，绝不 `unwrap_or_else(原始值)`：
    ///    root 表里有一条解析不了就整体拒绝（失败关闭），拿一把假尺子比等于没有检查。
    ///
    /// 符号链接 / Windows junction 的逃逸：`canonicalize()` 会解析 reparse point，
    /// 父目录里任何一层软链都在比对前被展开，因此落到 root 外的会被拒。
    /// **Task 25 已实测**（R275：junction 标准用户可建，`mklink /J`）：逃逸与
    /// 「root 本身是链接不得过度收紧」两头由本模块 tests 的
    /// `a_junction_inside_the_root_pointing_outside_is_denied` 与
    /// `a_root_that_is_itself_a_junction_is_not_over_tightened` 钉住。**文件级符号链接**
    /// 需开发者模式、本机建不了（实测报「没有足够的权限」）⇒ 该子形态**未实测**；
    /// 而「最后一段自己是指针」本就归 canonicalize 父目录的设计管不到，由
    /// `tools/read_file.rs` 在读之前把整条路径二次 canonicalize、交回本函数复验补上。
    pub fn resolve_within_roots(&self, raw: &str) -> Result<PathBuf, ToolError> {
        // 规则 4：先整体校验 root 表。
        let mut roots: Vec<PathBuf> = Vec::with_capacity(self.roots.len());
        for r in &self.roots {
            roots.push(r.canonicalize().map_err(|e| {
                ToolError::BadInput(format!("root 无法解析: {}: {e}", r.display()))
            })?);
        }

        // 规则 3：词法上的最后一段。
        match raw.split(['/', '\\']).next_back() {
            Some(".") | Some("..") => {
                return Err(ToolError::BadInput(format!(
                    "路径最后一段不能是 . 或 ..: {raw}"
                )));
            }
            _ => {}
        }

        // 规则 1。
        let p = Path::new(raw);
        if !p.is_absolute() {
            return Err(ToolError::BadInput(format!("只接受绝对路径: {raw}")));
        }

        // 规则 2：canonicalize 父目录后接回最后一段。
        let file_name = p
            .file_name()
            .ok_or_else(|| ToolError::BadInput(format!("路径没有可落盘的最后一段: {raw}")))?;
        let parent = p
            .parent()
            .ok_or_else(|| ToolError::BadInput(format!("路径没有父目录: {raw}")))?;
        let parent_c = parent.canonicalize().map_err(|e| {
            ToolError::BadInput(format!("父目录无法解析: {}: {e}", parent.display()))
        })?;
        let resolved = parent_c.join(file_name);

        // 规则 1/2 的比对：两边都是 canonical 形态（Windows 上是 `\\?\C:\…`，
        // 盘符大小写与前缀形式都由 canonicalize 统一掉），`starts_with` 才是逐组件比。
        if roots.iter().any(|r| resolved.starts_with(r)) {
            return Ok(resolved);
        }
        Err(ToolError::BadInput(format!("路径越界: {raw}")))
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn meta(&self) -> ToolMeta;
    async fn execute(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutput, ToolError>;
}

pub struct ToolMeta {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub risk: Risk,
}

/// 同名后注册者覆盖先注册者（MCP 工具撞名时这是有意行为，`defs()` 不会出双份）。
#[derive(Default)]
pub struct ToolRegistry {
    tools: Arc<RwLock<BTreeMap<String, Arc<dyn Tool>>>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, t: Arc<dyn Tool>) {
        crate::lock::write(&self.tools).insert(t.meta().name.clone(), t);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        crate::lock::read(&self.tools).get(name).cloned()
    }

    /// 未知工具按 `Risk::Exec`：这是唯一安全的默认值，因为 `Exec` 在权限门里必定弹窗，
    /// 而 `Read` 会被静默自动放行（MCP 工具名形如 `mcp__server__tool`，Plan A 不注册）。
    pub fn risk_of(&self, name: &str) -> Risk {
        self.get(name).map(|t| t.meta().risk).unwrap_or(Risk::Exec)
    }

    /// 交给模型的声明：只有 (name, description, input_schema) 三项 —— 类型 `ToolDef`
    /// 里没有 risk 字段，所以风险等级不可能经模型之手。按名字排序，请求体可复现。
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .read()
            .unwrap()
            .values()
            .map(|t| {
                let m = t.meta();
                ToolDef {
                    name: m.name,
                    description: m.description,
                    input_schema: m.input_schema,
                }
            })
            .collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        input: Value,
        ctx: &ToolCtx,
    ) -> Result<ToolOutput, ToolError> {
        let t = self
            .get(name)
            .ok_or_else(|| ToolError::NotFound { name: name.into() })?;
        // 取消守卫放在这里（不是只放在各工具里）：「按了停止 yet 又跑了一个新工具」是
        // Task 10 循环里最容易漏的一条，注册表拦一次就够（R53 有测试钉着）。
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        t.execute(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::{Risk, ToolOutput};
    use crate::tools::echo::{EchoTool, RiskOverrideTool};
    use serde_json::json;
    use std::path::{Component, Path, PathBuf};
    use tokio_util::sync::CancellationToken;

    fn tmp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("qwen-do-t9-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).expect("建临时目录失败");
        p
    }

    fn ctx_with_roots(roots: Vec<PathBuf>) -> ToolCtx {
        ToolCtx {
            http: reqwest::Client::new(),
            cancel: CancellationToken::new(),
            roots,
        }
    }

    fn drive_of(p: &Path) -> Option<String> {
        p.components().find_map(|c| match c {
            Component::Prefix(x) => Some(x.as_os_str().to_string_lossy().to_lowercase()),
            _ => None,
        })
    }

    // ---- 注册 / 查表 / 执行 ----
    #[tokio::test]
    async fn registered_tool_executes_and_returns_output() {
        let r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        let out = r
            .execute("echo", json!({"text": "hi"}), &ctx_with_roots(vec![]))
            .await
            .unwrap();
        assert!(matches!(out, ToolOutput::Text { text } if text == "hi"));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_not_panic() {
        let r = ToolRegistry::new();
        let err = r
            .execute("nope", json!({}), &ctx_with_roots(vec![]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("nope"), "{err}");
    }

    #[tokio::test]
    async fn unknown_tool_risk_defaults_to_exec_not_read() {
        // 未知即 Read 会让未注册工具静默自动放行——危险的默认值
        let r = ToolRegistry::new();
        assert_eq!(r.risk_of("mcp__x__y"), Risk::Exec);
    }

    #[tokio::test]
    async fn defs_exclude_risk_and_include_schema() {
        let r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        let d = r.defs();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].name, "echo");
        // R53：这个名字里写着 include_schema 的测试必须真的断言 schema。
        assert_eq!(
            d[0].input_schema,
            json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
            "发给模型的 schema 必须是 echo 声明的那一份"
        );
        assert!(!d[0].description.is_empty());
        // risk 不在 ToolDef 的字段里（编译期事实：那个结构体就没有 risk 字段），
        // 所以模型侧拿不到风险、也不可能有「模型自称 Read」这条通路；风险只经 risk_of 出。
        assert_eq!(r.risk_of("echo"), Risk::Read);
    }

    #[tokio::test]
    async fn duplicate_registration_replaces_not_duplicates() {
        let r = ToolRegistry::new();
        r.register(Arc::new(EchoTool));
        r.register(Arc::new(RiskOverrideTool(Risk::Write)));
        r.register(Arc::new(EchoTool));
        assert_eq!(r.defs().len(), 2, "同名替换、异名共存");
        let names: Vec<String> = r.defs().into_iter().map(|d| d.name).collect();
        assert!(names.contains(&"echo".to_string()) && names.contains(&"guarded".to_string()));
        r.register(Arc::new(EchoTool));
        assert_eq!(r.defs().len(), 2, "重复注册 echo 不得变出第三条");
    }

    // ---- R53：execute 的取消守卫 ----
    /// 一个**自己不看取消**的工具。用它而不是 echo 来钉这条守卫：echo 内部也查
    /// `is_cancelled`，拿 echo 测的话把注册表的守卫删掉测试照样绿（等于没防）。
    struct IgnoreCancelTool;

    #[async_trait::async_trait]
    impl Tool for IgnoreCancelTool {
        fn meta(&self) -> ToolMeta {
            ToolMeta {
                name: "naive".into(),
                description: "从不检查取消的测试工具".into(),
                input_schema: json!({"type": "object"}),
                risk: Risk::Exec,
            }
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &ToolCtx,
        ) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text("照常跑完"))
        }
    }

    #[tokio::test]
    async fn execute_on_an_already_cancelled_ctx_returns_cancelled() {
        let r = ToolRegistry::new();
        r.register(Arc::new(IgnoreCancelTool));
        let ctx = ctx_with_roots(vec![]);
        ctx.cancel.cancel();
        let err = r.execute("naive", json!({}), &ctx).await.unwrap_err();
        assert!(
            matches!(err, ToolError::Cancelled),
            "取消后的 ctx 必须走注册表的守卫，实际 {err:?}"
        );
    }

    // ---- R54：测试专用的「把风险抬高」工具（Task 10 的 denied_tool 测试要用） ----
    #[tokio::test]
    async fn risk_override_tool_reports_the_risk_the_struct_says() {
        let r = ToolRegistry::new();
        r.register(Arc::new(RiskOverrideTool(Risk::Write)));
        assert_eq!(r.risk_of("guarded"), Risk::Write, "风险必须来自结构体字段");
        // 执行逻辑复用 echo
        let out = r
            .execute("guarded", json!({"text": "x"}), &ctx_with_roots(vec![]))
            .await
            .unwrap();
        assert!(matches!(out, ToolOutput::Text { text } if text == "x"));
        // 换个风险值就是另一个风险 —— 这就是「测试里造一个 Ask 工具」的唯一手段
        let r2 = ToolRegistry::new();
        r2.register(Arc::new(RiskOverrideTool(Risk::Exec)));
        assert_eq!(r2.risk_of("guarded"), Risk::Exec);
    }

    // ---- R47：路径穿越防线 ----
    #[tokio::test]
    async fn existing_file_inside_the_root_is_accepted() {
        let root = tmp_dir("in");
        let file = root.join("note.txt");
        std::fs::write(&file, b"hi").unwrap();
        let got = ctx_with_roots(vec![root.clone()])
            .resolve_within_roots(file.to_str().unwrap())
            .unwrap();
        assert!(got.starts_with(root.canonicalize().unwrap()), "{got:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn nonexistent_file_under_an_existing_root_is_accepted() {
        // Plan B 的 write_file 要能新建文件：所以只能 canonicalize 父目录，
        // 整条路径 canonicalize 会把这条误判成「路径不存在」。
        let root = tmp_dir("new");
        let cand = root.join("brand-new.txt");
        assert!(!cand.exists());
        let got = ctx_with_roots(vec![root.clone()])
            .resolve_within_roots(cand.to_str().unwrap())
            .unwrap();
        assert!(got.starts_with(root.canonicalize().unwrap()), "{got:?}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn dot_dot_traversal_is_rejected() {
        let root = tmp_dir("trav");
        // 词法上仍以 root 开头，组件序列却已经走出 root：
        // 只 canonicalize root 的实现（brief 版）在这里是**放行**的（R47）。
        let evil = root.join("..").join("sibling-secret.txt");
        let raw = evil.to_str().unwrap().to_string();
        assert!(
            Path::new(&raw).starts_with(&root),
            "夹具前提：这条路径必须词法上看着像在 root 里"
        );
        let err = ctx_with_roots(vec![root.clone()])
            .resolve_within_roots(&raw)
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("越界")),
            "实际 {err:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn final_component_dot_or_dotdot_is_rejected() {
        let root = tmp_dir("last");
        let ctx = ctx_with_roots(vec![root.clone()]);
        for tail in [".", ".."] {
            let raw = root.join(tail);
            let err = ctx.resolve_within_roots(raw.to_str().unwrap()).unwrap_err();
            assert!(
                matches!(err, ToolError::BadInput(_)),
                "末段 {tail:?} 必须拒，实际 {err:?}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn relative_path_is_rejected() {
        let root = tmp_dir("rel");
        let ctx = ctx_with_roots(vec![root.clone()]);
        for raw in ["relative/x.txt", "", "x.txt"] {
            let err = ctx.resolve_within_roots(raw).unwrap_err();
            assert!(
                matches!(&err, ToolError::BadInput(m) if m.contains("绝对路径")),
                "相对路径 {raw:?} 必须拒，实际 {err:?}"
            );
        }
        // `.` 与 `..` 也是「相对」的，但它们先撞上末段规则（各有明确报错）
        for raw in [".", ".."] {
            let err = ctx.resolve_within_roots(raw).unwrap_err();
            assert!(
                matches!(&err, ToolError::BadInput(_)),
                "{raw:?} 必须拒，实际 {err:?}"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn cross_drive_absolute_path_is_rejected() {
        let root = tmp_dir("drive"); // temp 在 C:
        let here = std::env::current_dir().unwrap(); // cargo 跑在 D: 的工作区里
        let (root_c, here_c) = (root.canonicalize().unwrap(), here.canonicalize().unwrap());
        // 夹具前提：本测试要求临时目录与 cwd 在不同盘符（实测本机 C: 与 D:，见下方 SKIP
        // 分支）。单盘符机器上前提不成立 —— 那是**环境**不满足而不是实现有 bug，
        // 所以跳过而不是判红（reviewer Important：跨机器不该产生假失败）。
        // 内置 harness 没有 skip 语义，所以这里 println；要用 `-- --show-output` 看。
        if drive_of(&root_c) == drive_of(&here_c) {
            println!("SKIP cross_drive: root 与 cwd 同盘符，跨盘符前提不成立");
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        let cand = here.join("qwen-do-other-drive.txt");
        let err = ctx_with_roots(vec![root.clone()])
            .resolve_within_roots(cand.to_str().unwrap())
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("越界")),
            "跨盘符绝对路径必须拒，实际 {err:?}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn a_root_that_cannot_be_canonicalized_errors_instead_of_degrading() {
        // brief 版对 root 用 `canonicalize().unwrap_or_else(|_| r.clone())`：
        // root 不存在时静默降级成原始字符串，于是「越界检查」比的是一把假尺子。
        let missing =
            std::env::temp_dir().join(format!("qwen-do-t9-nope-{}", uuid::Uuid::new_v4()));
        assert!(!missing.exists(), "夹具前提：这个 root 必须不存在");
        let cand = missing.join("x.txt");
        let err = ctx_with_roots(vec![missing.clone()])
            .resolve_within_roots(cand.to_str().unwrap())
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("root")),
            "root 解析失败必须报错，实际 {err:?}"
        );
    }

    #[tokio::test]
    async fn no_roots_denies_everything() {
        let ctx = ctx_with_roots(vec![]);
        let dir = tmp_dir("noroot");
        let abs = dir.join("x.txt");
        let err = ctx.resolve_within_roots(abs.to_str().unwrap()).unwrap_err();
        std::fs::remove_dir_all(&dir).ok();
        assert!(matches!(err, ToolError::BadInput(_)), "{err:?}");
    }

    // ---- Task 25：把 `resolve_within_roots` 文档里那句「未实测」变成实测（junction 两头都钉） ----

    /// R275：文件符号链接要开发者模式（本机实测建不了），但目录 junction 标准用户
    /// 可建且同为 reparse point，canonicalize 对它走同一条展开路径。
    /// `mklink` 是 cmd 内建：参数走数组、绝不拼命令串。
    #[cfg(windows)]
    fn make_junction(link: &Path, target_dir: &Path) {
        let out = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &link.display().to_string(),
                &target_dir.display().to_string(),
            ])
            .output()
            .expect("启动 cmd.exe 建 junction 失败");
        assert!(
            out.status.success(),
            "建 junction 失败 {} -> {}: {}",
            link.display(),
            target_dir.display(),
            String::from_utf8_lossy(&out.stdout)
        );
    }

    /// 判据（brief Step 1）：**canonicalize 之后的候选路径**必须 starts_with
    /// **canonicalize 之后的 root**。root 自己也在 :60-65 被 canonicalize（fail-closed
    /// 循环，Task 25 台账核实：brief「改为对 root 也 canonicalize」一句是 stale，
    /// 现状已满足）——所以 root 是 junction 时不会被误杀（见正向那条）。
    #[cfg(windows)]
    #[tokio::test]
    async fn a_junction_inside_the_root_pointing_outside_is_denied() {
        let dir = tmp_dir("junction-out");
        let root = dir.join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"do-not-read").unwrap();
        make_junction(&root.join("link_dir"), &outside);

        let cand = root.join("link_dir").join("secret.txt");
        let err = ctx_with_roots(vec![root.clone()])
            .resolve_within_roots(cand.to_str().unwrap())
            .unwrap_err();
        assert!(
            matches!(&err, ToolError::BadInput(m) if m.contains("越界")),
            "root 内指向 root 外的 junction 必须在父目录 canonicalize 时被展开并拒，实得 {err:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 过度收紧同样是缺陷（同一条 brief 判据的另一头）：root 本身是 junction 时，
    /// 它「里面」的正常文件必须放行 —— 两侧比的都是 canonical 形态的同一把尺子。
    #[cfg(windows)]
    #[tokio::test]
    async fn a_root_that_is_itself_a_junction_is_not_over_tightened() {
        let dir = tmp_dir("junction-root");
        let target = dir.join("target");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("note.txt"), b"hi").unwrap();
        let jroot = dir.join("jroot");
        make_junction(&jroot, &target);

        let cand = jroot.join("note.txt");
        let got = ctx_with_roots(vec![jroot.clone()])
            .resolve_within_roots(cand.to_str().unwrap())
            .unwrap();
        assert!(
            got.starts_with(target.canonicalize().unwrap()),
            "root 是 junction 时正常路径必须解析成功，实得 {got:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 同上（与 gate 那条一对）：Task 13 要把注册表塞进 `AppState`，
    /// `Send + Sync` 必须由编译器现在就管住，而不是到装配那天才报。
    #[test]
    fn registry_tool_and_ctx_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ToolRegistry>();
        assert_send_sync::<ToolCtx>();
        assert_send_sync::<Arc<dyn Tool>>();
        assert_send_sync::<EchoTool>();
    }
}
