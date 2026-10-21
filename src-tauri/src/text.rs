//! 字符边界上的截断。四个调用点共用这一份实现（R77 的精神：同一件事只许有一个真相）：
//! `agent::types::ToolOutput::preview`（事件流预览）、`store::db`（会话标题）、
//! `model::assistant_acc`（解析失败时留在消息里的原始片段）、
//! `model::openai_compat`（进错误信息的响应正文）。
//!
//! 它们共用的是「怎么切」。标记文案分两组：预览与标题同一口径（[`truncate_display`]），
//! 两处日志片段要声明丢弃了多少或「正文已截断」，各自用 [`head_chars`] 自己拼 ——
//! 把四处的文案也合并成第五个真相反而是错的。

/// 按**字符**取前 `max` 个，返回 `(head, dropped)`。
///
/// 为什么不是 `&s[..max]`：这里切的都是模型输出、网页正文、用户贴进来的中文，
/// 中日韩一个字符占 3 字节，按字节下标切片会在非 UTF-8 边界上 panic ——
/// 而那正好发生在「内容太大所以要截」的分支上，是最不该 panic 的时刻。
///
/// `dropped == 0` 表示**没有**截断，调用方据此决定要不要加标记；边界取等号时
/// （恰好 `max` 个字符）`dropped` 也是 0，所以短文本不会被凭空加上省略号。
/// 长度按字符计而不是字节计：标记里写「丢弃 N 个字符」要能对上用户的直觉。
pub fn head_chars(s: &str, max: usize) -> (String, usize) {
    let mut it = s.chars();
    let head: String = it.by_ref().take(max).collect();
    (head, it.count())
}

/// [`head_chars`] 再补一个省略号 —— 「事件流预览」（`agent::types::ToolOutput::preview`）
/// 和「会话标题」（`store::db`）的口径完全相同，所以合并成这一个函数；
/// 日志片段那两处要报丢弃字数或声明「正文已截断」，仍各自用 [`head_chars`] 自己拼。
pub fn truncate_display(s: &str, max: usize) -> String {
    let (mut head, dropped) = head_chars(s, max);
    if dropped > 0 {
        head.push('…');
    }
    head
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_within_the_limit_comes_back_unchanged_and_unmarked() {
        let (head, dropped) = head_chars("今天天气不错", 200);
        assert_eq!(head, "今天天气不错");
        assert_eq!(dropped, 0, "没截断却报数，调用方就会给短文本加省略号");
    }

    #[test]
    fn the_exact_boundary_is_not_truncation() {
        // 恰好 `max` 个字符是最容易写错的一档：`>=` 与 `>` 之差会让每条刚好等长的
        // 标题都带一个多余的省略号。
        let (head, dropped) = head_chars("abcd", 4);
        assert_eq!(head, "abcd");
        assert_eq!(dropped, 0);
    }

    #[test]
    fn over_the_limit_it_reports_how_many_characters_were_dropped() {
        let src = "甲".repeat(10) + "乙丙丁";
        let (head, dropped) = head_chars(&src, 10);
        assert_eq!(head, "甲".repeat(10));
        assert_eq!(dropped, 3);
    }

    #[test]
    fn counting_is_by_characters_so_multibyte_never_panics() {
        // 每个汉字 3 字节：按字节下标切 `&s[..6]` 会 panic（UTF-8 非边界）。
        // 这条同时钉住「`dropped` 是字符数」——按字节算的话 4 个汉字会报成 12。
        let (head, dropped) = head_chars("你好世界再见", 4);
        assert_eq!(head, "你好世界");
        assert_eq!(dropped, 2);
    }

    #[test]
    fn a_zero_limit_keeps_nothing_and_blames_everything() {
        // `max = 0`  reachable：常量写错、或将来把上限交给配置。不许 panic，
        // 也不许把 dropped 报成 0（那等于「没截断」，标记就丢了）。
        let (head, dropped) = head_chars("abc", 0);
        assert_eq!(head, "");
        assert_eq!(dropped, 3);
    }

    #[test]
    fn emoji_are_counted_as_one_character_each() {
        // grapheme 集群（如带修饰符的旗帜、ZWJ 家庭）在这里仍按 `char` 计，
        // 与四处调用点的口径一致：宁可标记里的数字偏大，也不引入 `unicode-segmentation`。
        let (head, dropped) = head_chars("🙂🙂🙂", 2);
        assert_eq!(head, "🙂🙂");
        assert_eq!(dropped, 1);
    }
}
