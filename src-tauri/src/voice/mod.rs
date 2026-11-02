//! Phase 1：Windows 内置离线语音输入（SAPI 5）的后端。
//!
//! 三条设计约束：
//! 1. **不依赖 tauri** —— `lib.rs` 的硬约束是「`src/` 下只有 `ipc.rs` 允许 `use tauri`」，
//!    本模块只给 `ipc` 提供 `start` / `VoiceSession::{stop,cancel}` / `probe`。
//! 2. **COM 只在专用线程里** —— `sapi` 的 `start()` 起一条 `std::thread`，所有
//!    `CoInitializeEx` / 接口创建 / 事件轮询 / 接口释放都发生在那条线程上；
//!    `VoiceSession` 只持 `Sender<Command>` 与 `JoinHandle`，不把 COM 对象带出线程。
//!    `probe()` 同样在自己的线程里初始化 MTA COM，不依赖调用方的 COM 状态。
//! 3. **非 Windows 编译 stub** —— `cargo test` / `cargo clippy --all-targets` 在 Linux
//!    上全程不碰麦克风或 COM。

#[cfg(windows)]
mod sapi;
#[cfg(not(windows))]
mod stub;

#[cfg(windows)]
pub use sapi::{VoiceSession, probe, start};
#[cfg(not(windows))]
pub use stub::{VoiceSession, probe, start};

/// 一个 SAPI 识别器 token 的只读摘要（`examples/speech_probe.rs` 打印用）。
#[derive(Debug, Clone)]
pub struct RecognizerInfo {
    pub index: u32,
    pub token_id: String,
    pub description: String,
    /// token 的 `Language` 属性原值。SAPI 以十六进制 LCID 存储（如 `"804"` = 0x804）。
    pub language: String,
}

/// `probe()` 的结果。
#[derive(Debug, Clone)]
pub struct ProbeReport {
    /// 用户默认 locale 名（如 `zh-CN`）；取不到时为 `None`。
    pub default_language: Option<String>,
    pub recognizers: Vec<RecognizerInfo>,
    /// 用 zh-CN（LCID 0x804）创建 `SpInprocRecognizer` + `LoadDictation` 的结果。
    /// 不启动麦克风采集。
    pub zh_cn_dictation: Result<(), String>,
}

/// 语音链路上的错误来自哪一段 —— 决定同一个 HRESULT 该说「没有麦克风」还是「缺语言包」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureStage {
    Recognizer,
    Microphone,
}

/// 麦克风权限被拒绝。
pub const E_ACCESSDENIED: i32 = 0x8007_0005u32 as i32;
/// 找不到 token / 设备 / 语言包（按 `FailureStage` 解释）。
/// `SPERR_ENGINE_BUSY` / `SPERR_AUDIO_BUSY` 等没有可靠来源的错误码不在这里猜：
/// 猜错会把「格式不支持」说成「设备被占用」。未知 HRESULT 一律走带原始码的通用文案。
pub const SPERR_NOT_FOUND: i32 = 0x8004_503Au32 as i32;

/// 把一段最终识别文本拼进累积文本。
///
/// 中英混排的规则只有一条：**两侧都是 ASCII 字母数字时补一个空格**，其余情况直接相接。
/// 于是 `"hello"` + `"world"` → `"hello world"`，`"中文"` + `"测试"` → `"中文测试"`，
/// `"hello"` + `"中文"` → `"hello中文"`。片段先 `trim`，空片段直接忽略。
/// 纯函数（不碰 COM / IO），`cargo test` 里直接测。
pub fn append_transcript(acc: &mut String, piece: &str) {
    let piece = piece.trim();
    if piece.is_empty() {
        return;
    }
    if let (Some(prev), Some(next)) = (acc.chars().last(), piece.chars().next())
        && prev.is_ascii_alphanumeric()
        && next.is_ascii_alphanumeric()
    {
        acc.push(' ');
    }
    acc.push_str(piece);
}

/// SAPI token 的 `Language` 属性是十六进制字符串（`"804"` = LCID 0x804）。
/// 解析失败（空串、非十六进制）给 `None`，调用方跳过该 token。
pub fn parse_language_attribute(value: &str) -> Option<u32> {
    u32::from_str_radix(value.trim(), 16).ok()
}

/// 把 HRESULT 映射成可行动的中文。
///
/// `stage` 是必要的：`SPERR_NOT_FOUND`（0x8004503A）在录音设备段意味着「没有麦克风」，
/// 在识别器段意味着「缺语言包」，一句话说不清两件事。未知 HRESULT 一律带 `0x%08X`，
/// 方便用户/日志定位。
pub fn describe_failure(stage: FailureStage, code: i32) -> String {
    let known = |message: &str| format!("{message}（HRESULT 0x{code:08X}）");
    if code == E_ACCESSDENIED {
        return known(
            "麦克风权限被拒绝：请在 Windows 设置 → 隐私和安全性 → 麦克风中，允许桌面应用访问麦克风",
        );
    }
    match stage {
        FailureStage::Microphone if code == SPERR_NOT_FOUND => known(
            "没有检测到可用的麦克风（无默认录音设备）。请连接麦克风，并在系统声音设置中把它设为默认输入设备",
        ),
        FailureStage::Recognizer if code == SPERR_NOT_FOUND => known(
            "系统缺少所选语言的语音识别语言包。请在 Windows 设置 → 时间和语言 → 语言和区域中安装该语言的“语音识别”组件",
        ),
        _ => format!(
            "语音识别失败（HRESULT 0x{code:08X}）：请稍后重试；若持续失败，请检查系统语音组件是否完整"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_joins_ascii_words_with_a_space_and_cjk_without_one() {
        let mut acc = String::new();
        append_transcript(&mut acc, "  hello ");
        assert_eq!(acc, "hello");
        append_transcript(&mut acc, "world");
        assert_eq!(acc, "hello world");
        append_transcript(&mut acc, "中文");
        assert_eq!(acc, "hello world中文");
        append_transcript(&mut acc, "测试");
        assert_eq!(acc, "hello world中文测试");
        append_transcript(&mut acc, "");
        append_transcript(&mut acc, "   ");
        assert_eq!(acc, "hello world中文测试", "空片段不许留下分隔符");
    }

    #[test]
    fn transcript_starts_without_a_leading_space() {
        let mut acc = String::new();
        append_transcript(&mut acc, "abc");
        assert_eq!(acc, "abc");
    }

    #[test]
    fn transcript_separates_two_ascii_fragments_only() {
        let mut acc = String::from("abc123");
        append_transcript(&mut acc, "456def");
        assert_eq!(acc, "abc123 456def");
    }

    #[test]
    fn language_attribute_is_hex_lcid() {
        assert_eq!(parse_language_attribute("804"), Some(0x0804));
        assert_eq!(parse_language_attribute(" 409 "), Some(0x0409));
        assert_eq!(parse_language_attribute(""), None);
        assert_eq!(parse_language_attribute("zh-CN"), None);
    }

    #[test]
    fn every_known_hresult_maps_to_actionable_chinese() {
        let denied = describe_failure(FailureStage::Recognizer, E_ACCESSDENIED);
        assert!(denied.contains("权限"), "{denied}");
        assert!(denied.contains("0x80070005"), "{denied}");

        let no_mic = describe_failure(FailureStage::Microphone, SPERR_NOT_FOUND);
        assert!(no_mic.contains("麦克风"), "{no_mic}");

        let no_language = describe_failure(FailureStage::Recognizer, SPERR_NOT_FOUND);
        assert!(no_language.contains("语言包"), "{no_language}");
    }

    #[test]
    fn unknown_hresult_is_reported_with_its_hex_code() {
        let message = describe_failure(FailureStage::Recognizer, 0x8000_4005u32 as i32);
        assert!(message.contains("0x80004005"), "{message}");
    }
}
