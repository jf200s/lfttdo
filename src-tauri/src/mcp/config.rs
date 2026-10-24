// T29：`mcp.json` 的只读配置层。
//
// 边界（简报 :9）：本文件**不**建连接、**不** spawn、**不**接调用点，也**不**产出任何
// 写文件的 API / `Serialize` / `to_json_string()` —— 写入者是 T35（管理界面）。
// `load` 的目录参数就是设置文件所在的那个目录（R293）：这里只做一次 `join(MCP_FILE)`，
// 不去找 `app_data_dir()`（那个 helper 今天只在 examples 里）。签名相对 R293 的 `&Path`
// 放宽为 `&impl AsRef<Path>`，原因见 `load` 上的冲突记录。
//
// 脱敏红线（R294）：`env`/`headers` 的值一律打码为固定 `"***"`（键名留着排查），
// 不复用 `settings.rs` 的 `mask_key` —— 那「前 4 + … + 后 4」的 8 个字符对第三方令牌
// 是真泄露。`url` 的 userinfo 也遮（R295）。`Debug` 全部手写，一处 `derive(Debug)` 都不许有。

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

/// 配置文件名。与 `SettingsStore::new(&dir)` 共用同一个目录（R293），本任务不碰那层。
pub const MCP_FILE: &str = "mcp.json";

/// 一个可用的 MCP server 定义。两臂都是**直接字段**（R291：测试逐字用
/// `ServerDef::Stdio { name, command, args, env }` 构造，`matches!(…, ServerDef::Http { .. })`
/// 判别，所以不许装箱、不许改形状）。
///
/// **没有 `#[derive(Debug)]`**（Step 3 / R294）：derive 会把 `env`/`headers` 的明文值
/// 打进任何 `{:?}`。先例：`settings.rs` 的 `SettingsError`、`openai_compat.rs` 的 `ModelConfig`。
pub enum ServerDef {
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    Http {
        name: String,
        url: String,
        headers: HashMap<String, String>,
    },
}

// —— 访问器（R291，是接口的一部分）——

impl ServerDef {
    pub fn name(&self) -> &str {
        match self {
            Self::Stdio { name, .. } | Self::Http { name, .. } => name,
        }
    }

    /// `Http` 臂没有 command ⇒ `None`（这正是 Step 1 测试靠 `s.command()` 而不是
    /// `s.command` 的原因：字段在判别臂上不成立，方法才成立）。
    pub fn command(&self) -> Option<&str> {
        match self {
            Self::Stdio { command, .. } => Some(command),
            Self::Http { .. } => None,
        }
    }

    /// 返回 `Vec<&str>` 而不是 `Vec<String>`（R291：测试的 `assert_eq!(s.args(), vec!["-y", …])`
    /// 靠的是 `Vec<&str> == Vec<&str>`）。
    pub fn args(&self) -> Vec<&str> {
        match self {
            Self::Stdio { args, .. } => args.iter().map(String::as_str).collect(),
            Self::Http { .. } => Vec::new(),
        }
    }

    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Http { url, .. } => Some(url),
            Self::Stdio { .. } => None,
        }
    }

    /// **只交键名，永远不交值**（R291）。`Http` 臂没有 `env`（它带的是 `headers`）⇒ 空。
    pub fn env_keys(&self) -> Vec<&str> {
        match self {
            Self::Stdio { env, .. } => env.keys().map(String::as_str).collect(),
            Self::Http { .. } => Vec::new(),
        }
    }
}

/// 一次读取的全部结果（R290：三字段，不是一个）。
/// 没有 `Serialize`、没有任何会回吐原值的 API；`parse_error` 只放 serde_json 的错误摘要。
pub struct McpConfig {
    pub servers: Vec<ServerDef>,
    pub skipped: Vec<Skipped>,
    pub parse_error: Option<String>,
}

/// 被跳过的条目台账。`reason` 用 `&'static str`（R290/R292：值域是固定四条，
/// 措辞必须可枚举，给台账与 T35 界面直接摆出来用）。
pub struct Skipped {
    pub name: String,
    pub reason: &'static str,
}

// —— 手写 Debug（Step 3 / R294：`#[derive(Debug)]` 一处都不许出现）——
// 先例：`settings.rs` 的 `SettingsError`、`openai_compat.rs` 的 `ModelConfig`。
// 脱敏用固定 `"***"`，不复用 `mask_key`：MCP 的 env/headers 值是用户手抄进来的第三方令牌，
// 界面没有识别需求，而「前 4 + … + 后 4」的那 8 个字符（`sk-`、`Bearer ` 都在头 4 位里）
// 是真泄露。键名留着排查。`command`/`args` **不遮**（R295 的残余风险，交给 T35 裁）。

/// 固定打码值（R294）。
const MASKED: &str = "***";

/// R295：Debug 里打 `url` 之前，若 authority 段含 `@`，把 `@` 之前整段换成 `***`
/// （`https://user:tok@mcp.example/sse` ⇒ `https://***@mcp.example/sse`）；没有 `@` 原样。
/// 只动 userinfo：host 与 path 留着，界面还认得出是哪台。残余风险按 R295 记给 T35：
/// `command`/`args` **不遮**（`npx -y --token=abc` 这类写法遮不住，遮它会把正常配置打烂）。
fn mask_url_userinfo(url: &str) -> String {
    // authority 段 = `scheme://` 之后、下一个 `/`、`?`、`#` 之前；`@` 只在这段里才算 userinfo。
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    match authority.rfind('@') {
        Some(at) => format!("{scheme}://{MASKED}{}{}", &authority[at..], tail),
        None => url.to_string(),
    }
}

/// 给 `env`/`headers` 用的脱敏展示壳：只出键名，值一律 `"***"`；键名排序 ⇒ Debug 输出确定。
struct MaskedMap<'a>(&'a HashMap<String, String>);

impl std::fmt::Debug for MaskedMap<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut keys: Vec<&String> = self.0.keys().collect();
        keys.sort();
        f.write_str("{")?;
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{k}: \"{MASKED}\"")?;
        }
        f.write_str("}")
    }
}

impl std::fmt::Debug for ServerDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // env/headers 的值打码（R294）；url 的 userinfo 打码（R295）；name/command/args 原样。
        match self {
            Self::Stdio {
                name,
                command,
                args,
                env,
            } => f
                .debug_struct("Stdio")
                .field("name", name)
                .field("command", command)
                .field("args", args)
                .field("env", &MaskedMap(env))
                .finish(),
            Self::Http { name, url, headers } => f
                .debug_struct("Http")
                .field("name", name)
                .field("url", &mask_url_userinfo(url))
                .field("headers", &MaskedMap(headers))
                .finish(),
        }
    }
}

impl std::fmt::Debug for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConfig")
            .field("servers", &self.servers)
            .field("skipped", &self.skipped)
            .field("parse_error", &self.parse_error)
            .finish()
    }
}

/// plan :743 逐字用了 `load_raw(…).to_string()`，而 `to_string` 来自 `Display` 的 blanket
/// impl ⇒ 本类型必须有 `Display`（`Produces` 里没有它，见报告的冲突记录）。
/// 实现只转发到**脱敏后的 Debug**，不存在第二条能回吐原值的路径。
impl std::fmt::Display for McpConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::fmt::Debug for Skipped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Skipped")
            .field("name", &self.name)
            .field("reason", &self.reason)
            .finish()
    }
}

/// 判别 `command`/`url` 用中间结构体，不用 `#[serde(flatten)]`（简报 §三·4：flatten +
/// untagged 会把「两个字段都没有」和「字段类型不对」揉成同一个错，毁掉 R292 的四条 reason）。
/// 未知字段**忽略**（向前兼容，与 R91 同取向；Claude/Cline/Roo 各家还有 `disabled`、
/// `alwaysAllow`、`type` 之类的键，拒了就等于读不了别人的配置）。
#[derive(Deserialize, Default)]
#[serde(default)]
struct RawServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    url: Option<String>,
    headers: Option<BTreeMap<String, String>>,
}

/// `mcpServers` 的容器**不能**直接用 `BTreeMap<String, RawServer>`：
/// `serde_json::Map` 默认虽是 BTreeMap（无 `preserve_order`），但反序列化进 map 时
/// 同名重复键会 last-wins 覆盖 ⇒ R292 的「同名重复」永远不可达（Step 2 实测红：
/// 留下来的是第二条 `"second"` 而不是第一条）。这里用自定义 `Visitor` 经 `MapAccess`
/// 逐对收集**每一个**键值对（重复键都在），再由 `load` 做一次按名字的**稳定排序**：
/// 唯一名时结果与 BTreeMap 遍历序逐字相同（简报 §三·3 钉的字典序语义不变，
/// `servers_follow_alphabetical_order_not_written_order` 仍绿），重复名时保留出现序
/// （稳定排序不动第一条 ⇒ 「留第一条」）。冲突与取舍详见 task-29 报告。
#[derive(Deserialize)]
struct RawFile {
    #[serde(
        rename = "mcpServers",
        default,
        deserialize_with = "deserialize_servers"
    )]
    servers: Vec<(String, RawServer)>,
}

fn deserialize_servers<'de, D>(deserializer: D) -> Result<Vec<(String, RawServer)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct ServersVisitor;

    impl<'de> serde::de::Visitor<'de> for ServersVisitor {
        type Value = Vec<(String, RawServer)>;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("an mcpServers object mapping names to server definitions")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut entries = Vec::new();
            while let Some(entry) = map.next_entry::<String, RawServer>()? {
                entries.push(entry);
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_map(ServersVisitor)
}

/// 读取 `dir` 下的 `mcp.json`。文件不存在 ⇒ 三条全空，**不报错、不返回 Result**（R293）。
///
/// 签名冲突记录（详见 task-29 报告）：R293 给的是 `dir: &Path`，但 plan 逐字测试写的是
/// `load(&dir)`（`dir` 是 `TempDir`），而 tempfile 3.27 的 `TempDir` 只实现 `AsRef<Path>`、
/// **没有** `Deref<Target = Path>`（dir.rs:484 实测）⇒ 二者不可能同时成立。这里放宽成
/// `&impl AsRef<Path>`：逐字测试与未来 `&Path` 调用点都原样可传，函数体仍只做一次
/// `join(MCP_FILE)`，不引入其他权力。
pub fn load(dir: &impl AsRef<Path>) -> McpConfig {
    // R293 负半边：文件**不存在** ⇒ 三条全空、不报错、不返回 Result，也不动目录里任何东西
    // （不改名、不搬走 —— 配置读这一层没有任何改文件的权力）。
    // 其他读失败（权限、路径是目录）不是简报钉过的形态：这里给空 servers + `parse_error`
    // 摘要，**不 panic**（自己的判断，报告里提请裁）。
    let content = match std::fs::read_to_string(dir.as_ref().join(MCP_FILE)) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return McpConfig {
                servers: Vec::new(),
                skipped: Vec::new(),
                parse_error: None,
            };
        }
        Err(e) => {
            return McpConfig {
                servers: Vec::new(),
                skipped: Vec::new(),
                parse_error: Some(format!("读取 {MCP_FILE} 失败: {e}")),
            };
        }
    };
    let raw = match serde_json::from_str::<RawFile>(&content) {
        Ok(raw) => raw,
        Err(e) => {
            return McpConfig {
                servers: Vec::new(),
                skipped: Vec::new(),
                parse_error: Some(e.to_string()),
            };
        }
    };
    let mut servers: Vec<ServerDef> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut entries = raw.servers;
    // 按名字做**稳定**字典序排序：与 §三·3 描述的 BTreeMap 遍历序对唯一名逐字一致，
    // 重复名时保留出现序 ⇒ 「留第一条」成立（见 RawFile 上的冲突记录）。
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, entry) in entries {
        // 条目级失败的四条 reason（R292，逐字；值域封闭，第五种形态「整体 JSON 坏」
        // 走 parse_error，不进 skipped）。判定序：名字 → 内容 → 同名。
        if name.is_empty() {
            skipped.push(Skipped {
                name,
                reason: "名字为空",
            });
            continue;
        }
        let def = if let Some(command) = entry.command {
            if command.is_empty() {
                skipped.push(Skipped {
                    name,
                    reason: "command 为空",
                });
                continue;
            }
            ServerDef::Stdio {
                name,
                command,
                args: entry.args.unwrap_or_default(),
                env: entry.env.unwrap_or_default().into_iter().collect(),
            }
        } else if let Some(url) = entry.url {
            ServerDef::Http {
                name,
                url,
                headers: entry.headers.unwrap_or_default().into_iter().collect(),
            }
        } else {
            skipped.push(Skipped {
                name,
                reason: "没有 command 也没有 url",
            });
            continue;
        };
        // 同名重复 ⇒ 留第一条、后面那条进 skipped（排序后的序是确定的，见上面的冲突记录）。
        if servers.iter().any(|s| s.name() == def.name()) {
            skipped.push(Skipped {
                name: def.name().to_string(),
                reason: "同名重复",
            });
            continue;
        }
        servers.push(def);
    }
    McpConfig {
        servers,
        skipped,
        parse_error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_a_claude_desktop_style_config() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("mcp.json"), r#"{
          "mcpServers": {
            "memory": { "command": "npx", "args": ["-y","@modelcontextprotocol/server-memory"],
                        "env": { "TOKEN": "shh" } },
            "remote": { "url": "https://mcp.example/sse", "headers": { "Authorization": "Bearer shh" } }
          }}"#).unwrap();
        let c = load(&dir);
        assert_eq!(c.servers.len(), 2);
        let s = c.servers.iter().find(|s| s.name() == "memory").unwrap();
        assert_eq!(s.command(), Some("npx"));
        assert_eq!(s.args(), vec!["-y", "@modelcontextprotocol/server-memory"]);
        assert!(matches!(
            c.servers.iter().find(|s| s.name() == "remote").unwrap(),
            ServerDef::Http { .. }
        ));
    }

    // 简报 §三·3：`serde_json::Map` 默认 BTreeMap ⇒ `servers` 顺序按名字典序，与书写序无关。
    // 夹具故意按 "zeta"、"alpha" 的书写序排，钉住这条顺序语义。
    #[test]
    fn servers_follow_alphabetical_order_not_written_order() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{ "mcpServers": { "zeta": {"command":"z"}, "alpha": {"command":"a"} } }"#,
        )
        .unwrap();
        let c = load(&dir);
        assert_eq!(
            c.servers.iter().map(|s| s.name()).collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
    }

    // —— 测试内联辅助（简报 Step 2：不是 `Produces` 的一部分，别 `pub`）——
    // `load_json` = 写进 tempdir 再 `load(&dir)`；`load_raw` 同一条路径，参数是任意字符串（含坏 JSON）。
    fn load_json(json: &str) -> McpConfig {
        load_raw(json)
    }

    fn load_raw(raw: &str) -> McpConfig {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("mcp.json"), raw).unwrap();
        load(&dir)
    }

    #[test]
    fn a_broken_entry_is_skipped_with_a_reason_not_a_crash() {
        // 无 command 也无 url / JSON 整体坏掉 / 同名重复，三种都要单独条目级失败
        let c = load_json(
            r#"{ "mcpServers": { "ok": {"command":"x"}, "bad": {}, "also": {"command":"y"} } }"#,
        );
        assert_eq!(c.servers.len(), 2, "坏的那条跳过，好的两条必须留下来");
        assert_eq!(c.skipped[0].name, "bad");
        let garbage = load_raw("{ not json");
        assert!(
            garbage.servers.is_empty() && !garbage.parse_error.is_none(),
            "整体坏掉要有明确错误，但不 panic"
        );
    }

    // —— 简报 Step 2 要求的三条补断言（plan 只写了三种形态里的两种半）——
    #[test]
    fn each_entry_failure_shape_gets_its_own_reason() {
        let no_both = load_json(r#"{ "mcpServers": { "bad": {} } }"#);
        assert_eq!(no_both.skipped[0].reason, "没有 command 也没有 url");

        let dup = load_json(
            r#"{ "mcpServers": { "d": {"command":"first"}, "d": {"command":"second"} } }"#,
        );
        assert_eq!(dup.servers.len(), 1, "同名重复：留第一条");
        assert_eq!(dup.servers[0].command(), Some("first"));
        assert_eq!(dup.skipped[0].name, "d");
        assert_eq!(dup.skipped[0].reason, "同名重复");

        let empty_cmd = load_json(r#"{ "mcpServers": { "x": {"command": ""} } }"#);
        assert_eq!(empty_cmd.skipped[0].name, "x");
        assert_eq!(empty_cmd.skipped[0].reason, "command 为空");
    }

    #[test]
    fn debug_never_prints_env_or_headers() {
        let s = ServerDef::Stdio {
            name: "x".into(),
            command: "npx".into(),
            args: vec![],
            env: HashMap::from([("TOKEN".to_string(), "sk-REAL-SECRET".to_string())]),
        };
        let d = format!("{s:?}");
        assert!(!d.contains("sk-REAL-SECRET"), "`{{:?}}` 泄密：{d}");
        assert!(
            d.contains("TOKEN") && d.contains("***"),
            "键名要留着排查，值必须打码：{d}"
        );
        assert!(
            !load_raw(r#"{"mcpServers":{"x":{"command":"npx","env":{"T":"sk-REAL-SECRET"}}}}"#)
                .to_string()
                .contains("sk-REAL-SECRET")
        );
    }

    // R295：url 的 userinfo 也遮 —— `https://user:tok@mcp.example/sse` 这种写法真实存在
    // （不少托管 MCP 就这么发凭据）。authority 段含 `@` 时把 `@` 之前整段换成 `***`，
    // host 与路径原样（界面还认得出是哪台）；不含 `@` 时整条 url 原样。
    #[test]
    fn url_userinfo_is_masked_but_host_is_not() {
        let c = load_json(
            r#"{ "mcpServers": { "hosted": {"url": "https://user:secret@mcp.example/sse"} } }"#,
        );
        let d = format!("{:?}", c.servers[0]);
        assert!(d.contains("mcp.example"), "host 不许遮：{d}");
        assert!(d.contains("***@"), "userinfo 要遮成 ***@：{d}");
        assert!(!d.contains("secret"), "`{{:?}}` 泄密：{d}");

        let plain = load_json(r#"{ "mcpServers": { "ok": {"url": "https://mcp.example/sse"} } }"#);
        let p = format!("{:?}", plain.servers[0]);
        assert!(p.contains("https://mcp.example/sse"), "无 @ 时原样：{p}");
    }

    // R293 的负半边：文件不存在 ⇒ 三条全空、不报错、不 panic。
    // 顺手钉「`load` 不写盘」：调用前后 `read_dir` 的名字序列逐字节相同
    // （防「顺手把坏文件改名搬走」——那是设置那一层的权力，配置读这一层没有）。
    #[test]
    fn absent_file_is_an_empty_config_not_an_error() {
        let dir = tempdir().unwrap();
        let names = |d: &std::path::Path| -> Vec<std::ffi::OsString> {
            let mut v: Vec<_> = std::fs::read_dir(d)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            v.sort();
            v
        };
        let before = names(dir.path());
        let c = load(&dir);
        assert!(c.servers.is_empty(), "文件不存在时 servers 必须空");
        assert!(c.skipped.is_empty(), "文件不存在时 skipped 必须空");
        assert!(
            c.parse_error.is_none(),
            "文件不存在**不是**错误：{:?}",
            c.parse_error
        );
        let after = names(dir.path());
        assert_eq!(before, after, "load 不许动目录里的任何文件");
    }
}
