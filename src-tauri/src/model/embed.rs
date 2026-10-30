// 嵌入抽象(T6,spec §3.1):长期记忆向量的**唯一**来源接口与生产实现。
//
// 本模块与 `provider.rs` 同一条 Plan A 约束:不得 `use tauri`,且核心形状可脱离
// 网络单测 —— 请求体/响应解析拆成纯函数,真端点行为由 trait 假实现覆盖
// (ipc.rs 的测试夹具),本文件**不写**任何发真包的测试。
//
// 裁定(偏离 spec §3.2 的 sqlite-vec 路线,记入 ledger):向量方案是
// `memories.embedding BLOB` + Rust 侧余弦,本模块只负责「文本 → f32 数组」这一段;
// 编码与检索归 `store::memory`(那边的文件头写着为什么不上 sqlite-vec)。
//
// 错误进 `String`,**不套** `ModelError` 四分类:那条分类是为「用户看着报错去查什么」
// 设计的(R28),而嵌入链的失败一律降级为纯关键词召回(裁定 4)、只进 warn 日志,
// 没有用户盯着它挑分类 —— 但错误文本必须带状态码,401(key)/404(端点没有
// /embeddings)/429(限流)在日志里得能一眼分开。
use super::openai_compat::normalize_base_url;
use async_trait::async_trait;

/// 嵌入提供者的最小接口。**输出顺序 = 输入顺序**:调用方按下标把向量对回记忆行,
/// 返回乱序或缺条 = 错位的向量比没有向量更糟(检索会「语义正确地」召回错误的记忆)。
/// 失败整批报错 —— 部分成功没有意义,降级是整批一起降(全有向量或全没向量,
/// 检索侧才不用处理「同一批里一半有一半没有」的第三态)。
#[async_trait]
pub trait EmbedProvider: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

/// 生产实现:OpenAI 兼容端点的非流式 `POST {base}/embeddings`,请求体
/// `{"model": …, "input": [...]}`,Authorization 复用聊天的 api_key —— 同一家端点、
/// 同一个凭据,隐私边界与聊天一致不扩大(spec §3.1 的取舍原文)。
pub struct EmbedEndpoint {
    url: String,
    model: String,
    api_key: String,
    http: reqwest::Client,
}

impl EmbedEndpoint {
    /// `base_url` 复用聊天的 `normalize_base_url`(R27):用户粘贴的是控制台给的那一行
    /// (`…/v1/chat/completions`),聊天侧已经把它收敛成 `<origin><path>/v1`,嵌入侧
    /// 用第二种归一就会让「聊天能通、嵌入 404」成为常态。空 base_url 是构造失败
    /// (调用方 send_message 已有前置守卫,这里是同一判据的最后一道)。
    pub fn new(base_url: &str, model: &str, api_key: &str) -> Result<Self, String> {
        let base = normalize_base_url(base_url);
        if base.is_empty() {
            return Err("base_url 为空，无法构造嵌入端点".into());
        }
        Ok(Self {
            url: embeddings_url(&base),
            model: model.to_string(),
            api_key: api_key.to_string(),
            http: reqwest::Client::new(),
        })
    }

    /// 诊断用(测试取证请求 URL),不是热路径。
    #[cfg(test)]
    fn url(&self) -> &str {
        &self.url
    }
}

/// `{base}/embeddings`。base 已经是 normalize 过的 `<origin><path>/v1` 形态,
/// 这里只做字面拼接 —— 端点段的后缀判定归 normalize 一处所有,不在这里再判第二遍。
fn embeddings_url(base: &str) -> String {
    format!("{base}/embeddings")
}

/// 请求体。OpenAI 兼容形状:`{"model": …, "input": [...]}`,input 就是字符串数组
/// (OpenAI 规范同时收 token 数组与字符串数组,兼容端点普遍只保证前者之外的后者)。
fn request_body(model: &str, texts: &[String]) -> serde_json::Value {
    serde_json::json!({ "model": model, "input": texts })
}

/// 响应解析:取 `data[].embedding`,按 `index` 排序后收集。
///
/// `expected` 是输入条数:返回条数对不上必须报错 —— 上层按下标把向量对回记忆行,
/// 少一条就是后面全部错位,错位的向量比降级(没向量)危险得多。
/// `index` 缺失的条目按数组位置计(规范要求有,个别兼容实现省略;省略时顺序即
/// 响应顺序,是唯一可用的次序);向量里的非数值是**错误**不是 0 —— 0 向量在余弦里
/// 恰好是「语义无关」的合法值,静默填 0 会把一条坏响应伪装成一次成功的降级。
fn parse_embeddings_response(raw: &str, expected: usize) -> Result<Vec<Vec<f32>>, String> {
    let v: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| format!("嵌入响应不是合法 JSON: {e}"))?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| "嵌入响应缺少 data 数组".to_string())?;
    let mut items: Vec<(i64, Vec<f32>)> = data
        .iter()
        .enumerate()
        .map(|(pos, item)| {
            let index = item.get("index").and_then(|i| i.as_i64()).unwrap_or(pos as i64);
            let arr = item
                .get("embedding")
                .and_then(|e| e.as_array())
                .ok_or_else(|| format!("嵌入响应第 {index} 条缺少 embedding 数组"))?;
            let vec = arr
                .iter()
                .map(|x| {
                    x.as_f64()
                        .map(|n| n as f32)
                        .ok_or_else(|| format!("嵌入响应第 {index} 条的向量含非数值"))
                })
                .collect::<Result<Vec<f32>, String>>()?;
            Ok((index, vec))
        })
        .collect::<Result<_, String>>()?;
    items.sort_by_key(|(i, _)| *i);
    let out: Vec<Vec<f32>> = items.into_iter().map(|(_, v)| v).collect();
    if out.len() != expected {
        return Err(format!(
            "嵌入端点返回 {} 条向量,请求 {} 条 —— 条数不符,按整批失败降级",
            out.len(),
            expected
        ));
    }
    Ok(out)
}

/// 日志里的响应体截断:错误响应可能是整页 HTML,一行 warn 不该背两 KB。
/// 按**字符**截(中文日志按字节切会踩 UTF-8 边界)。
fn truncate_for_log(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars).collect();
    format!("{head}…(共 {} 字符)", s.chars().count())
}

#[async_trait]
impl EmbedProvider for EmbedEndpoint {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        // 空批直接回空:不发网络请求。「压缩段零新增记忆」的正常路径不该付一次
        // HTTP 的价,而调用方也没有义务记住这条守卫。
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut req = self.http.post(&self.url).json(&request_body(&self.model, texts));
        // key 为空(本机 ollama 形态,resolve_api_key 对回环放行空 key)不发
        // Authorization:头里带一个空 Bearer 反而可能被严格端点拒掉。
        if !self.api_key.is_empty() {
            req = req.bearer_auth(&self.api_key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("嵌入请求发送失败: {e}"))?;
        let status = resp.status();
        let raw = resp
            .text()
            .await
            .map_err(|e| format!("嵌入响应读取失败(状态 {status}): {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "嵌入端点返回 {status}: {}",
                truncate_for_log(&raw, 300)
            ));
        }
        parse_embeddings_response(&raw, texts.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用户粘贴完整端点行(与聊天侧同一粘贴习惯)→ normalize 摘掉端点段再拼
    /// `/embeddings`。拼出 `…/v1/chat/completions/embeddings` 就是 404 的另一种拼法。
    #[test]
    fn the_embeddings_url_is_the_normalized_base_plus_the_embeddings_segment() {
        let e = EmbedEndpoint::new("https://x.test/v1/chat/completions", "m", "k").unwrap();
        assert_eq!(
            e.url(),
            "https://x.test/v1/embeddings",
            "normalize 先收敛成 <origin>/v1,再拼 embeddings 段"
        );
        // 空白 base 是构造失败:不产出「请求一定会打到空 URL」的半成品。
        assert!(EmbedEndpoint::new("   ", "m", "k").is_err());
    }

    #[test]
    fn the_request_body_carries_model_and_input_in_the_openai_shape() {
        let body = request_body(
            "text-embedding-v3",
            &["用户住在北京".to_string(), "用户养了一只猫".to_string()],
        );
        assert_eq!(body["model"], "text-embedding-v3");
        assert_eq!(
            body["input"],
            serde_json::json!(["用户住在北京", "用户养了一只猫"]),
            "input 是字符串数组,顺序即调用方给定的顺序"
        );
    }

    #[test]
    fn the_response_parses_data_embeddings_in_index_order() {
        // index 乱序到达(流式网关常见):必须按 index 归位,输出顺序 = 输入顺序。
        let raw = r#"{"data":[
            {"index":1,"embedding":[0.5,0.25]},
            {"index":0,"embedding":[1.0,0.0]}
        ]}"#;
        let out = parse_embeddings_response(raw, 2).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], vec![1.0f32, 0.0]);
        assert_eq!(out[1], vec![0.5f32, 0.25]);
        // 缺 index 的条目按数组位置计:两个都缺 = 顺序即响应顺序。
        let raw = r#"{"data":[{"embedding":[7.0]},{"embedding":[8.0]}]}"#;
        let out = parse_embeddings_response(raw, 2).unwrap();
        assert_eq!(out, vec![vec![7.0f32], vec![8.0f32]]);
    }

    #[test]
    fn a_malformed_or_count_mismatched_embedding_response_is_rejected_not_zero_filled() {
        // 每一种坏形状都必须报错:静默填 0 会把坏响应伪装成「语义无关」的合法向量,
        // 那是检索里最阴险的一种脏数据。
        assert!(parse_embeddings_response("not json", 1).is_err(), "非 JSON");
        assert!(
            parse_embeddings_response(r#"{"data":[]}"#, 1).is_err(),
            "条数不符(0 ≠ 1)"
        );
        assert!(
            parse_embeddings_response(r#"{"items":[]}"#, 1).is_err(),
            "缺 data 数组"
        );
        assert!(
            parse_embeddings_response(r#"{"data":[{"index":0}]}"#, 1).is_err(),
            "缺 embedding 字段"
        );
        assert!(
            parse_embeddings_response(r#"{"data":[{"index":0,"embedding":["a"]}]}"#, 1).is_err(),
            "向量含非数值"
        );
        assert!(
            parse_embeddings_response(
                r#"{"data":[{"index":0,"embedding":[1.0]},{"index":1,"embedding":[2.0]}]}"#,
                3
            )
            .is_err(),
            "条数不符(2 ≠ 3):上层按下标对行,少一条 = 后面全错位"
        );
    }

    /// 空批不发网络请求(实现合同):这一支在离线测试里就能钉住 —— 构造不连网,
    /// 空输入直接返回,任何「先发请求再判空」的写法在这里会以连接失败的形式红。
    #[tokio::test]
    async fn an_empty_batch_returns_empty_without_touching_the_network() {
        let e = EmbedEndpoint::new("https://x.test/v1", "m", "k").unwrap();
        assert_eq!(e.embed(&[]).await.unwrap(), Vec::<Vec<f32>>::new());
    }
}
