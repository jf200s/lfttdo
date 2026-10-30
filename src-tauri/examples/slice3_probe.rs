//! 切片 3 的原始帧探针（非生产代码，Task 28）。findings 贴的就是它的 stdout。
//!
//! 为什么要有这个文件（plan 没写）：T28 的判据是「事件数 = 0」「声明表里有没有 `read_file`」
//! 「拒绝文案点名哪个地址」，而界面只看得见这些事实**投影之后**的样子（DOM 卡片）。
//! 先例是 `fetch_probe.rs` / `stream_probe.rs` / `image_prep_probe.rs`：三个都靠
//! 「不带 `--live` 一个字节都不发出去」这条纪律住在 `examples/` 里，判据是**原始输出**而不是推断。
//!
//! 走的是**生产装配**，不是探针自己那套：`ipc::build_production_registry()` +
//! `ipc::run_turn(TurnDeps{…})`（`ipc.rs:379`，一项 tauri 类型都没有）。探针自己
//! 再拼一遍会话等于造第二条构造路径 —— 那正是 T27 评审 Minor-3 警告的形状。
//! roots 交的是 `Settings.roots` 那份**原表**（和 `ipc.rs:569` 逐字同一件事），
//! 于是「每轮现取 + 使用点裁剪」（R276/R284）在真机上被走到，而不是被绕过。
//!
//! SQLite 一律开在**临时库**上：用户那份 `%APPDATA%\com.qwa.do\qwen-do.db` 是真历史，
//! 探针往里写一行就是污染。
//!
//! 用法（都在仓库根）：
//! ```text
//! cargo run --manifest-path src-tauri/Cargo.toml --example slice3_probe
//! cargo run --manifest-path src-tauri/Cargo.toml --example slice3_probe -- --live --turn "今天天气怎么样"
//! cargo run --manifest-path src-tauri/Cargo.toml --example slice3_probe -- --live --turn "读 D:\slice3-notes\a.txt 第三行" --roots D:\slice3-notes
//! cargo run --manifest-path src-tauri/Cargo.toml --example slice3_probe -- --live --direct read_file --input {"path":"C:\\Windows\\win.ini"} --roots D:\\slice3-notes
//! cargo run --manifest-path src-tauri/Cargo.toml --example slice3_probe -- --live --token-delta
//! ```
//! 设置读**真文件**（只读，探针绝不 `save()`）；base_url 非回环时探针直接停 ——
//! 凭据管理器不是探针该碰的东西（R164）。

use futures::StreamExt;
use lfttdo_lib::agent::context::ContextBuilder;
use lfttdo_lib::agent::events::{AgentEvent, EventSink};
use lfttdo_lib::agent::types::{ContentBlock, Message};
use lfttdo_lib::ipc::{MemoryTurnCfg, TurnDeps, build_production_registry, run_turn};
use lfttdo_lib::model::openai_compat::{OpenAiCompat, is_loopback_endpoint};
use lfttdo_lib::model::provider::{ChatRequest, ModelDelta, ModelError, ModelProvider};
use lfttdo_lib::permission::gate::PermissionGate;
use lfttdo_lib::settings::{SettingsStore, effective_roots};
use lfttdo_lib::store::db::Db;
use lfttdo_lib::tools::registry::{ToolCtx, ToolRegistry};

use serde_json::{json, value::Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio_util::sync::CancellationToken;

/// 把每个事件原样打成一行 JSON 的输出槽。`emit` 是同步的（`events.rs:100`），
/// 所以这里只做 `println!` —— 它自己带 stdout 锁，行不会被切开。
struct JsonSink {
    t0: Instant,
    seen: Mutex<Vec<String>>,
}

impl JsonSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            t0: Instant::now(),
            seen: Mutex::new(Vec::new()),
        })
    }
    /// 零弹窗判据的**唯一**读数：`permission_request` 出现了几个。
    fn count_of(&self, tag: &str) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.as_str() == tag)
            .count()
    }
}

impl EventSink for JsonSink {
    fn emit(&self, e: AgentEvent) {
        let tag = serde_json::to_value(&e)
            .ok()
            .and_then(|v| v.get("t").and_then(|t| t.as_str()).map(String::from))
            .unwrap_or_else(|| "?".into());
        self.seen.lock().unwrap().push(tag.clone());
        let ms = self.t0.elapsed().as_millis();
        match serde_json::to_string(&e) {
            Ok(body) => println!("{{\"seq_ms\":{ms},\"tag\":{tag:?},\"event\":{body}}}"),
            Err(err) => println!("{{\"seq_ms\":{ms},\"tag\":{tag:?},\"serialize_error\":{err:?}}}"),
        }
    }
}

/// 记录「这一轮到底把哪些工具声明交给了模型」的 provider 装饰器。
/// R286 的判据（空 roots ⇒ 声明表里没有 `read_file`）住在 `session.rs` 的 `request()` 里，
/// 事件流看不见它；在**唯一的生产出口**包一层，读到的就是真发出去的那份请求，
/// 而不是探针自己按同一个条件重算一遍（那样测的是探针，不是生产）。
struct Recording {
    inner: Arc<dyn ModelProvider>,
}

impl ModelProvider for Recording {
    fn stream<'a>(
        &'a self,
        req: ChatRequest,
    ) -> std::pin::Pin<Box<dyn futures::Stream<Item = Result<ModelDelta, ModelError>> + Send + 'a>>
    {
        let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
        let decl_bytes: usize = req
            .tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description, "input_schema": t.input_schema})
                    .to_string()
                    .len()
            })
            .sum();
        println!(
            "{}",
            json!({"t": "request", "messages": req.messages.len(), "tools": names, "tools_json_bytes": decl_bytes, "temperature": req.temperature})
        );
        self.inner.stream(req)
    }
}

fn app_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("QWEN_APP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let roaming = std::env::var("APPDATA").expect("APPDATA 没设，探针找不到设置文件");
    PathBuf::from(roaming).join("com.qwa.do")
}

/// 非回环 ⇒ 停。理由写在 `ipc.rs:506` 那条闸里：没有 key 的公网端点不许被探针顺手打出去，
/// 而探针也不该去读凭据管理器（那里躺着用户真 key，findings 要原样贴 stdout）。
fn api_key_for(base_url: &str) -> String {
    assert!(
        is_loopback_endpoint(base_url),
        "探针只跑回环端点（本机 Ollama）。非回环需要真凭据，而探针不碰凭据管理器 —— 见 R164。实得 {base_url}"
    );
    String::new()
}

fn parse_roots_flag(args: &[String]) -> Vec<PathBuf> {
    args.iter()
        .position(|a| a == "--roots")
        .and_then(|i| args.get(i + 1))
        .map(|one| vec![PathBuf::from(one)])
        .unwrap_or_default()
}

async fn turn_mode(args: &[String], prompt: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dir = app_data_dir();
    let store = SettingsStore::new(&dir);
    let mut settings = store.load()?;
    let extra = parse_roots_flag(args);
    if !extra.is_empty() {
        // 模拟「用户在设置文件里写了这条 root」，但**不改文件**（只改内存里那份），
        // 因为真改文件是界面侧的验证（走 `tauri dev` 那一路），探针不许写用户的设置。
        settings.roots.extend(extra);
    }
    println!(
        "{}",
        json!({"t": "setup", "base_url": settings.base_url, "model": settings.model, "persisted_roots": settings.roots, "effective_roots": effective_roots(&settings.roots)})
    );

    let db_path = std::env::temp_dir().join(format!(
        "slice3-probe-{}.db",
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    ));
    let db = Arc::new(Db::open(&db_path)?);
    let sid = db.create_session("切片 3 探针")?;

    let api_key = api_key_for(&settings.base_url);
    let model_cfg = settings.to_model_config(&api_key);
    let temperature = model_cfg.temperature;
    let model: Arc<dyn ModelProvider> = Arc::new(Recording {
        inner: Arc::new(OpenAiCompat::new(model_cfg)?),
    });
    // T6:嵌入客户端与生产装配(send_message)同一条线 —— source=="endpoint" 才构造,
    // 失败 warn 后降 None(检索退化为纯关键词)。key 在此处可用,构造即持走明文。
    let embed: Option<Arc<dyn lfttdo_lib::model::embed::EmbedProvider>> =
        if settings.memory_embedding_source == "endpoint" {
            match lfttdo_lib::model::embed::EmbedEndpoint::new(
                &settings.base_url,
                &settings.memory_embedding_model,
                &api_key,
            ) {
                Ok(e) => Some(Arc::new(e)),
                Err(reason) => {
                    eprintln!("{{\"t\":\"embed_off\",\"reason\":{reason:?}}}");
                    None
                }
            }
        } else {
            None
        };
    let sink = JsonSink::new();

    let reason = run_turn(
        TurnDeps {
            db: db.clone(),
            registry: build_production_registry(db.clone()),
            gate: Arc::new(PermissionGate::new()),
            model,
            sink: sink.clone(),
            context: settings.to_context_config(),
            temperature,
            // T5：记忆四项与生产装配(send_message)同一条线 —— 从本轮那份 Settings 现取。
            // T6 两键同线:embedding_endpoint 是 source=="endpoint" 的现算形态。
            memory: MemoryTurnCfg {
                enabled: settings.memory_enabled,
                recall_k: settings.memory_recall_k as usize,
                note_token_cap: settings.memory_note_token_cap as usize,
                profile_char_cap: settings.memory_profile_char_cap as usize,
                // T10/T12:待办提醒窗口与衰减半衰期都从本轮 Settings 现取(与生产装配同一条线)。
                prospective_window_hours: settings.memory_prospective_window_hours,
                fact_half_life_days: settings.memory_decay_half_life_days as f64,
                embedding_endpoint: settings.memory_embedding_source == "endpoint",
                embedding_model: settings.memory_embedding_model.clone(),
            },
            embed,
            // 探针只跑一轮、单线程:一把空锁足够(并发语义不是探针要验的事)。
            extract_locks: Default::default(),
            roots: settings.roots.clone(),
            skills_dir: None,
            skills_disabled: Vec::new(),
            cancel: Arc::new(Mutex::new(None)),
        },
        &sid,
        vec![ContentBlock::Text {
            text: prompt.to_string(),
        }],
    )
    .await
    .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    println!(
        "{}",
        json!({"t": "summary", "stop_reason": format!("{reason:?}"), "permission_request_events": sink.count_of("permission_request"), "tool_call_events": sink.count_of("tool_call"), "tool_result_events": sink.count_of("tool_result"), "db_path": db_path})
    );
    drop(db);
    let _ = std::fs::remove_file(&db_path);
    Ok(())
}

/// 不经模型、直接叫工具：SSRF 文案与越界拒绝的**逐字**读数在这里最干净
/// （模型会改写文案，那是要另测的另一件事 —— `--turn` 那条测的就是它）。
/// 探针自己的工具表:内存库让 `memory_search` 能注册(生产装配的一致性),
/// 但查不到任何东西 —— 探针不碰用户的真实记忆库。
fn probe_registry() -> Arc<ToolRegistry> {
    let db = Arc::new(Db::open(std::path::Path::new(":memory:")).expect("内存库"));
    build_production_registry(db)
}

async fn direct_mode(
    name: &str,
    input: Value,
    roots: Vec<PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let registry = probe_registry();
    let tool = registry.get(name).ok_or_else(|| {
        format!(
            "生产工具表里没有 {name}（实得 {:?}）",
            registry
                .defs()
                .iter()
                .map(|d| d.name.clone())
                .collect::<Vec<_>>()
        )
    })?;
    let ctx = ToolCtx {
        http: reqwest::Client::new(),
        cancel: CancellationToken::new(),
        roots: effective_roots(&roots),
        skills_dir: None,
        skills_disabled: Vec::new(),
    };
    let outcome = tool.execute(input.clone(), &ctx).await;
    println!(
        "{}",
        json!({"t": "direct", "tool": name, "risk": format!("{:?}", registry.risk_of(name)), "input": input, "declared": registry.defs().iter().any(|d| d.name == name), "outcome": format!("{outcome:?}")})
    );
    Ok(())
}

/// T27 评审 Important-2 的那格空数：**同一批消息**，带工具声明 vs 不带，真实 `prompt_tokens` 差多少。
/// 两次都走生产 provider（`OpenAiCompat`），所以量的是端点自己怎么数，不是我们的估算。
/// 估算单独给：`ContextBuilder::build(同一份历史)` 的 `est_input_tokens`，它**不含**工具声明
/// （`session.rs:316` 就是这么写的），于是比值就是「估少了多少」。
async fn token_delta_mode() -> Result<(), Box<dyn std::error::Error>> {
    let dir = app_data_dir();
    let settings = SettingsStore::new(&dir).load()?;
    let api_key = api_key_for(&settings.base_url);
    let model_cfg = settings.to_model_config(&api_key);
    let provider = OpenAiCompat::new(model_cfg)?;
    let registry = probe_registry();

    let history = vec![Message::user(vec![ContentBlock::Text {
        text: "请把下面这句话原样复述一遍：切片 3 令牌测量。".to_string(),
    }])];
    let built = ContextBuilder::new(settings.to_context_config()).build(&history);
    let messages = built.messages.clone();

    let mut out = Vec::new();
    // 三条臂：`no_tools` / 只声明 `fetch_url` / 两条都声明。中间那条是为了拿到**单把工具**的
    // 边际代价 —— T29–T37 要往声明表里加 MCP 工具，拍板依据是「一把多少 token」，不是「一共多少」。
    // 声明字节用 `Recording` 那一条**同一个形状**（`name`/`description`/`input_schema`），
    // 这样本节里的字节数能和请求帧里的 `tools_json_bytes` 直接对上；它是**代理计数**，
    // 不是 OpenAI 线上体（线上还要包一层 `{"type":"function","function":{…}}`）。
    let all = registry.defs();
    let only_fetch: Vec<_> = all
        .iter()
        .filter(|d| d.name == "fetch_url")
        .cloned()
        .collect();
    let arms: [(&str, Vec<_>); 3] = [
        ("no_tools", Vec::new()),
        ("fetch_only", only_fetch),
        ("with_tools", all),
    ];
    for (label, tools) in arms {
        let decl_bytes: usize = tools
            .iter()
            .map(|t| {
                json!({"name": t.name, "description": t.description, "input_schema": t.input_schema})
                    .to_string()
                    .len()
            })
            .sum();
        let req = ChatRequest {
            messages: messages.clone(),
            tools,
            temperature: Some(0.0),
        };
        let mut stream = provider.stream(req);
        let mut usage = json!({"input_tokens": null, "output_tokens": null});
        while let Some(delta) = stream.next().await {
            if let Ok(ModelDelta::Usage {
                input_tokens,
                output_tokens,
            }) = delta
            {
                usage = json!({"input_tokens": input_tokens, "output_tokens": output_tokens});
            }
        }
        out.push(json!({"arms": label, "usage": usage, "decl_json_bytes": decl_bytes}));
    }
    println!(
        "{}",
        json!({"t": "token_delta", "arms": out, "est_input_tokens_without_tools": built.est_input_tokens})
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let live = args.iter().any(|a| a == "--live");

    if !live {
        // 一条字节都不发出去：只报「带 --live 时会做什么」，和 fetch_probe 同一套纪律。
        let dir = app_data_dir();
        let settings = SettingsStore::new(&dir).load()?;
        println!(
            "{}",
            json!({"t": "plan", "live": false, "settings_dir": dir, "base_url": settings.base_url, "model": settings.model, "persisted_roots": settings.roots, "registry_tools": probe_registry().defs().iter().map(|d| d.name.clone()).collect::<Vec<_>>(), "modes": ["--turn <话>", "--direct <tool> --input <json> [--roots <dir>]", "--token-delta"], "note": "加 --live 才会真发请求；本探针只读设置，绝不 save()"})
        );
        return Ok(());
    }

    if let Some(i) = args.iter().position(|a| a == "--turn") {
        let prompt = args.get(i + 1).cloned().unwrap_or_default();
        if prompt.trim().is_empty() {
            return Err("--turn 后面要给模型说的话".into());
        }
        return turn_mode(&args, &prompt).await;
    }
    if let Some(i) = args.iter().position(|a| a == "--direct") {
        let name = args.get(i + 1).cloned().unwrap_or_default();
        let input = args
            .iter()
            .position(|a| a == "--input")
            .and_then(|j| args.get(j + 1))
            .map(|raw| serde_json::from_str::<Value>(raw))
            .transpose()?
            .unwrap_or_else(|| json!({}));
        return direct_mode(&name, input, parse_roots_flag(&args)).await;
    }
    if args.iter().any(|a| a == "--token-delta") {
        return token_delta_mode().await;
    }
    Err("带 --live 时要选 --turn / --direct / --token-delta 之一".into())
}
