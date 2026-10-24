// T30：`mcp__{server}__{tool}` 的工具名命名层。
//
// 边界：本文件是**叶子**——不 `use` 任何其他模块（不碰 `super::config`、不碰 registry），
// 不产生任何调用点；第一个消费者是 T31 的 stdio 桥。
// 不引入任何哈希依赖（`fnv`/`siphasher`/`twox-hash` 都不行）：`fnv1a64` 手写，
// Cargo.lock 干净度与 <15MB 那条线都盯着这里。

/// 兼容端点对 function name 的**长度上限**（不是本项目的偏好数）：超过它的名字整轮请求一起被拒。
pub const NAME_LIMIT: usize = 64;

/// 兜底形状里 `server` 段保留的 char 数（T31 要把这三个常量写进连接日志的解析，故 `pub`）。
pub const SERVER_KEEP: usize = 8;
/// 兜底形状里 `tool` 段保留的 char 数。
pub const TOOL_KEEP: usize = 24;
/// 兜底形状里哈希十六进制读数取几位（高位在前）。
pub const HASH_HEX: usize = 7;

/// FNV-1a 64 的 offset basis。
const FNV64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64 的 prime。
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64：按定义式逐**字节**（octet）算，`hash = (hash XOR octet) * prime`，模 2^64。
///
/// 为什么不用 `std::collections::hash_map::DefaultHasher`：官方文档明说其算法不稳定、
/// 跨版本可能变；用它做工具名，等于升级 rustc 之后所有历史会话里的 tool_call 名字全对不上。
/// 本函数的输出是跨版本常量，写死在测试向量里（见 `fnv1a64_matches_published_vectors`）。
pub fn fnv1a64(s: &str) -> u64 {
    let mut hash = FNV64_OFFSET_BASIS;
    for octet in s.as_bytes() {
        hash ^= *octet as u64;
        hash = hash.wrapping_mul(FNV64_PRIME);
    }
    hash
}

/// 兼容端点对 function name 的字符集：`^[a-zA-Z0-9_-]{1,64}$`。
/// 这个集合**不是**本项目的偏好（R300）：非法字符不是那把工具单独红，而是**整轮请求一起 400**。
fn is_allowed(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// 取前 `n` 个 **char**（绝不按字节切；非 ASCII 名字按字节切会在多字节边界上 panic）。
fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// 把不在 `[A-Za-z0-9_-]` 里的字符逐个换成 `_`。**按 char 处理，绝不按字节切**（R301），
/// 所以一个非 ASCII 字符正好换成一个 `_`（不是 UTF-8 字节数那种 3 个 `_`）。
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if is_allowed(c) { c } else { '_' })
        .collect()
}

/// 兜底形状：`mcp__` + `server` 前 [`SERVER_KEEP`] 个 char + `__` + `tool` 前 [`TOOL_KEEP`] 个 char，
/// 再接 `#` + `fnv1a64(原始全名)` 十六进制的前 [`HASH_HEX`] 位。
/// 恒长 `5 + 8 + 2 + 24 + 1 + 7 = 47`（`mcp__` 是 5 个字符，不是 plan :808 算的 8+1）⇒
/// 给 `-N` 序号留 64 − 47 = 17 个字符的余量。
///
/// **为什么分隔符是 `#`**：plan :808 与简报 R300 第 4 条都把形状钉成 `…#{7 位 hex}`，本轮照钉。
/// 注意 `#` 本身**不在** `^[a-zA-Z0-9_-]{1,64}$` 那个字符集里 ⇒ 兜底名对严格校验的端点仍可能 400；
/// 这一层的承诺只有「永不 panic + 前缀段永不产生非法字符 + 恒在预算内」，
/// 端点 400 之后怎么提示用户归 T31/T37（简报 R301 要求明写的残余风险）。
fn fallback(raw: &str, server: &str, tool: &str) -> String {
    // 哈希对象永远是**原始全名**（`raw`），不是截断后的前缀、也不是洗过的名字：
    // 两条只差非法字符的名字洗完是同一个串，只有哈希能把它们分开。
    // `{:016x}` 先补零成 16 位再 `take` ⇒ 拿到的是**高位在前**的 7 个 hex；
    // 顺序反了（先 `{:x}` 再补零）会让「前 7 位」变成「低位的 7 位」，稳定性判据在某些输入上会漂。
    let hex = format!("{:016x}", fnv1a64(raw));
    let tag: String = hex.chars().take(HASH_HEX).collect();
    // 前缀段同样过一遍字符替换（R300 第 4 条）：`#` 是唯一的非 `[A-Za-z0-9_-]` 字符。
    let head = sanitize(&format!(
        "mcp__{}__{}",
        take_chars(server, SERVER_KEEP),
        take_chars(tool, TOOL_KEEP)
    ));
    format!("{head}#{tag}")
}

/// 把 (server, tool) 拼成喂给模型端的工具名。
///
/// 四条分支（R300）：拼原始全名 ⇒ 洗非法字符 ⇒ 「没超预算**且**一个字符都没动过」才直接返回 ⇒
/// 否则走 [`fallback`]。
///
/// 本层保证的是「**永不 panic + 永不产生 `[A-Za-z0-9_-]` 之外的前缀字符 + 恒在预算内**」。
/// 「兼容端点接不接受非 ASCII / `#` 的工具名」不是本层能保证的（残余风险见 [`fallback`] 的注释），
/// 端点 400 之后怎么提示用户归 T31/T37。
pub fn namespaced(server: &str, tool: &str) -> String {
    let raw = format!("mcp__{server}__{tool}");
    let sanitized = sanitize(&raw);
    // 「够短」不够，还得「一个字符都没替换过」：`a.b` 与 `a_b` 洗完是同一个串，
    // 直接返回会让它们在模型端撞成一把工具。
    if raw.chars().count() <= NAME_LIMIT && sanitized == raw {
        return sanitized;
    }
    fallback(&raw, server, tool)
}

/// 挂序号：后缀永远是 `-{index}`；若「base + 后缀」超过 [`NAME_LIMIT`]，**从 base 的尾部截**给后缀让位
/// （R298）。序号始终挂在整个名字的**尾部**，不挂在截断后的 base 上。按 char 截，不按字节。
fn with_suffix(base: &str, index: usize) -> String {
    let suffix = format!("-{index}");
    let budget = NAME_LIMIT.saturating_sub(suffix.chars().count());
    format!("{}{suffix}", take_chars(base, budget))
}

/// 同名冲突时按**喂入顺序**挂序号：第一次 ⇒ 原样返回并登记；第 N 次同名 ⇒ 挂 `-2`、`-3`……
/// （序号从 2 起，因为第一次没后缀；不许用 `-1` 表示第一次，那等于把「没冲突」伪装成「有冲突的第 1 号」。）
///
/// **有前提的确定性**（R302）：输出依赖调用方的喂入顺序，所以调用方必须按**稳定序**喂
/// —— T29 的 `servers` 已按名字典序，T31 的 registry 容器是 `BTreeMap` ⇒ 同一序。
/// 这里**不**做任何「排序/改名来消除顺序依赖」的小动作：那是把冲突藏起来。
pub fn dedup(name: &str, taken: &mut std::collections::HashSet<String>) -> String {
    if taken.insert(name.to_string()) {
        return name.to_string();
    }
    let mut index = 2;
    loop {
        let candidate = with_suffix(name, index);
        if taken.insert(candidate.clone()) {
            return candidate;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn short_names_pass_through_and_long_names_get_a_stable_short_hash() {
        assert_eq!(namespaced("web", "search"), "mcp__web__search");
        let server = "a-very-long-community-server-name-that-goes-on";
        let tool = "get_the_current_weather_forecast_for_a_location";
        let n = namespaced(server, tool);
        assert!(
            n.len() <= NAME_LIMIT,
            "超 64 会被兼容端点拒：{n}({})",
            n.len()
        );
        assert_eq!(
            n,
            namespaced(server, tool),
            "同一输入必须永远得到同一名字（重启后要能认出来）"
        );
        assert_ne!(
            n,
            namespaced(server, &format!("{tool}2")),
            "只换工具名末字符时不许撞名"
        );
    }

    #[test]
    fn collisions_are_disambiguated_in_order() {
        // 截断前缀相同的两条长名字，必须拿到不同结果且后缀是 -2、-3
        let a = namespaced(
            "srv",
            "tool_name_that_is_extremely_long_aaaaaaaaaaaaaaaaaaaaaaaa",
        );
        let b = namespaced(
            "srv",
            "tool_name_that_is_extremely_long_aaaaaaaaaaaaaaaaaaaaaabbb",
        );
        assert_ne!(a, b);
        let mut taken = HashSet::new();
        let mut out = vec![];
        for raw in [a.clone(), a.clone(), b.clone()] {
            out.push(dedup(&raw, &mut taken));
        }
        assert_eq!(out[1], format!("{a}-2"), "第二次同名要挂序号");
        assert!(
            out.iter().all(|n| n.len() <= NAME_LIMIT),
            "挂序号也不许突破上限：{out:?}"
        );
    }

    #[test]
    fn dedup_truncates_the_base_when_the_suffix_would_overflow() {
        // R298 的第二形态：base 本身已经顶着 NAME_LIMIT 时，序号要从 base 的尾部挤空间出来，
        // 而不是把总长推过上限（上面那条 verbatim 测试走的是「整名字尾直接挂 -2」那一支）。
        let base = "x".repeat(NAME_LIMIT);
        let mut taken = HashSet::new();
        assert_eq!(dedup(&base, &mut taken), base, "第一次不挂后缀");
        let second = dedup(&base, &mut taken);
        assert!(
            second.chars().count() <= NAME_LIMIT,
            "挤不下就要从 base 尾部截给后缀让位：{second}({})",
            second.chars().count()
        );
        assert_eq!(
            second,
            format!("{}-2", "x".repeat(NAME_LIMIT - 2)),
            "截到刚好让位，序号仍挂在整个名字的尾部"
        );
        assert_eq!(
            dedup(&base, &mut taken).chars().count(),
            NAME_LIMIT,
            "第三次也不许超预算"
        );
    }

    #[test]
    fn fnv1a64_matches_published_vectors() {
        // 空串 ⇒ 没进循环，直接是 offset basis。
        assert_eq!(fnv1a64(""), 0xcbf2_9ce4_8422_2325);
        // **plan :787 那个 `0xa4_32_d8_43_6a_df_8b_a6` 是错的**（R299）。本轮用两份独立实现
        // （Python 大整数 + Node BigInt，都按定义式）与 Rust 三方对齐后钉死下面的值；
        // 红证据见 task-30-report.md 第 2 节。
        assert_eq!(fnv1a64("hello"), 0xa430_d846_80aa_bd0b);
        // 公开测试向量（isthe.com/chongo 的 FNV 参考实现）：
        assert_eq!(fnv1a64("a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64("foobar"), 0x8594_4171_f739_67e8);
    }

    /// 形状的共用断言：兜底那支是「`[A-Za-z0-9_-]` 前缀段 + 唯一一个 `#` + [`HASH_HEX`] 位小写 hex」，
    /// 直通那支是纯 `[A-Za-z0-9_-]`。
    fn assert_allowed_shape(name: &str, ctx: &str) {
        let head = match name.split_once('#') {
            Some((head, tag)) => {
                assert_eq!(
                    name.matches('#').count(),
                    1,
                    "只许一个分隔符 `#`：{ctx} {name}"
                );
                assert_eq!(
                    tag.chars().count(),
                    HASH_HEX,
                    "hash 段恒 {HASH_HEX} 位：{ctx} {name}"
                );
                assert!(
                    tag.chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                    "hash 段是 7 位小写 hex、不带 `0x`：{ctx} {name}"
                );
                head
            }
            None => name,
        };
        assert!(
            head.chars().all(is_allowed),
            "前缀段只许 [A-Za-z0-9_-]：{ctx} {name}"
        );
    }

    #[test]
    fn illegal_characters_and_non_ascii_names_are_safe() {
        // ① 名字够短**但**发生过替换 ⇒ 必须走兜底那支（R300 第 3 条：只要动过字符就不许直接返回，
        // 因为 `a.b` 与 `a_b` 洗之后是同一个串，只有哈希能把它们分开）。
        let short_dotted = namespaced("srv", "server.tool:name");
        assert!(
            short_dotted.contains('#'),
            "发生过替换还直接返回，`a.b` 与 `a_b` 就会撞名：{short_dotted}"
        );
        assert!(
            !short_dotted.contains('.') && !short_dotted.contains(':'),
            "结果里不许有非法字符：{short_dotted}"
        );
        assert_allowed_shape(&short_dotted, "① 带 . 和 : 的名字");
        assert!(
            !short_dotted.ends_with('#'),
            "结果以 `#` + {HASH_HEX} 位 hex 收尾，不许以 `#` 结尾：{short_dotted}"
        );

        // ② 非 ASCII（用户真会把服务起名成中文）⇒ 不 panic、只含合法字符、在预算内。
        let cjk = namespaced("我的服务器", "获取天气");
        assert!(
            cjk.chars().count() <= NAME_LIMIT,
            "CJK 名字也要在预算内：{cjk}"
        );
        assert_allowed_shape(&cjk, "② CJK 名字");

        // ③ 两条只在**非法字符**上不同的长名字：截断后的前缀段一样、洗完整名也一样，
        //    只有「哈希对象 = 原始全名」能把它们分开（这一条就是控制器那个头条变异的靶子）。
        let with_dot = "a_tool_name_that_is_so_long_it_overflows_the_limit_with.dots";
        let with_underscore = "a_tool_name_that_is_so_long_it_overflows_the_limit_with_dots";
        let dotted = namespaced("srv", with_dot);
        let underlined = namespaced("srv", with_underscore);
        assert_ne!(
            dotted, underlined,
            "哈希必须打在原始全名上，否则只靠非法字符区分的两条名字会撞"
        );
        assert_allowed_shape(&dotted, "③-a 只在非法字符上不同（点号）");
        assert_allowed_shape(&underlined, "③-b 只在非法字符上不同（下划线）");

        // ④ 恒长断言：server/tool 都超过各自的保留位数时，兜底结果恒 47 个 char。
        let fat_server = "s".repeat(SERVER_KEEP + 40);
        let fat_tool = "t".repeat(TOOL_KEEP + 60);
        let n = namespaced(&fat_server, &fat_tool);
        assert_eq!(
            n.chars().count(),
            5 + SERVER_KEEP + 2 + TOOL_KEEP + 1 + HASH_HEX,
            "兜底形状恒长（= 47，见 R300 对 plan :808 那串算术的纠正）：{n}"
        );
        assert_eq!(
            n.chars().count(),
            47,
            "把 47 这个数钉死，注释和代码不许各说各话：{n}"
        );
        assert_allowed_shape(&n, "④ 恒长形状");
    }

    #[test]
    fn dedup_is_deterministic_for_a_stable_input_order() {
        // R302：dedup 的输出依赖喂入顺序 ⇒ 这是「有前提的确定性」。前提（稳定序）由调用方保证
        // —— T29 的 servers 已按名字典序、T31 的 registry 是 BTreeMap ⇒ 同一序。
        // 这里钉的是「同一序喂两次 ⇒ 结果逐字相同」，顺带钉序号从 2 起（第一次不挂后缀）。
        let feed = |name: &'static str| {
            let mut taken = HashSet::new();
            let mut out = vec![];
            for _ in 0..3 {
                out.push(dedup(name, &mut taken));
            }
            out
        };
        let first = feed("mcp__web__search");
        let second = feed("mcp__web__search");
        assert_eq!(
            first, second,
            "同一序两次调用必须逐字相同：{first:?} vs {second:?}"
        );
        assert_eq!(
            first,
            vec![
                "mcp__web__search".to_string(),
                "mcp__web__search-2".to_string(),
                "mcp__web__search-3".to_string()
            ],
            "第一次不挂后缀、第二次 `-2`、第三次 `-3`（不许用 `-1` 表示第一次）"
        );
        assert_eq!(
            dedup("x", &mut HashSet::new()),
            "x",
            "第一次见 ⇒ 原样返回并登记"
        );

        // 混合喂入也只看重复次数，不看名字之间互相的关系；顺序变了结果就变（这一条钉的是
        // 「本层不藏顺序依赖」，不是让调用方去猜）。
        let mut taken = HashSet::new();
        assert_eq!(dedup("a_name", &mut taken), "a_name");
        assert_eq!(dedup("other", &mut taken), "other");
        assert_eq!(dedup("a_name", &mut taken), "a_name-2");
    }

    #[test]
    fn hash_tag_is_the_high_bits_of_the_padded_hex() {
        // 机器事实 3：先 `{:016x}` 补零、再 `take(HASH_HEX)` ⇒ 拿到的必须是**高位在前**的 7 个 hex。
        // 常数由第二份实现（Python，逐字节按定义式，见 task-30-report.md 第 3 节）独立算出：
        //   fnv1a64("mcp__a-very-long…__get_the_current_weather_forecast_for_a_location")
        //     = 0x39a9f58269fca488 ⇒ 高 7 位 `39a9f58`、低 7 位 `9fca488`（两者不同 ⇒ 能分辨写反的 bug）
        //   fnv1a64(… "__get_the_current_weather_forecast_for_a_locationjaa")
        //     = 0x062893d1c86112ec ⇒ 高位是 0，专门用来钉「补零在前」：
        //        不补零的 `format!("{:x}", …)` 再 take(7) 会给出 `62893d1`，与正确值差一位。
        let long = "a-very-long-community-server-name-that-goes-on";
        let n = namespaced(long, "get_the_current_weather_forecast_for_a_location");
        assert_eq!(
            n,
            namespaced(long, "get_the_current_weather_forecast_for_a_location"),
            "同一输入两次要逐字相同"
        );
        assert!(n.ends_with("#39a9f58"), "要高位在前那 7 位：{n}");
        assert!(!n.ends_with("#9fca488"), "低位那 7 位是顺序写反的形态：{n}");

        let padded = namespaced(long, "get_the_current_weather_forecast_for_a_locationjaa");
        assert!(
            padded.ends_with("#062893d"),
            "高位为 0 时也必须补足 {HASH_HEX} 位：{padded}"
        );
        assert!(
            !padded.ends_with("#62893d1"),
            "不补零就 take 的写法会给出这个：{padded}"
        );
        assert_eq!(padded.chars().count(), 47, "补零那颗也是恒长 47：{padded}");
    }
}
