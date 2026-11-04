//! 语音输入后端：离线引擎（ort + SenseVoice-Small int8，自研前处理/解码）。
//!
//! 三条设计约束：
//! 1. **不依赖 tauri** —— `lib.rs` 的硬约束是「`src/` 下只有 `ipc.rs` 允许 `use tauri`」，
//!    本模块只给 `ipc` 提供 `start` / `VoiceSession::{stop,cancel}` 与模型下载器；
//!    进度事件经调用方传入的 `Fn(VoiceModelStatus)` 闭包发布，闭包由 `ipc.rs` 装配。
//! 2. **采集与识别只在专用线程里** —— `engine` 的 `start()` 起一条 `std::thread`，
//!    cpal 流只在那条线程上创建/使用/释放；识别走进程级缓存的 ort 模型
//!    （`asr` 模块，线程安全）；`VoiceSession` 只持 `Sender<Command>` 与
//!    `JoinHandle`（形状与 SAPI 时代一致）。
//! 3. **非 Windows 编译 stub** —— 采集引擎与 cpal 挂在 `cfg(windows)` 依赖段下，
//!    `cargo test` / `cargo clippy --all-targets` 在 Linux 上全程不碰麦克风；
//!    下载器与 `asr` 识别管线平台无关，Linux 可测。

pub mod asr;
pub mod download;
#[cfg(windows)]
mod engine;
#[cfg(not(windows))]
mod stub;

#[cfg(windows)]
pub use engine::{VoiceSession, input_devices, probe_recognizer, start};
#[cfg(not(windows))]
pub use stub::{VoiceSession, start};

use serde::Serialize;

/// 语音模型的状态快照（契约：Rust serde 字段名 == TS `VoiceModelStatus`）。
/// 经 `"voice-model"` 事件通道发布（通道名与 `app.emit` 都只住在 `ipc.rs`）。
#[derive(Debug, Clone, Serialize)]
pub struct VoiceModelStatus {
    /// `"missing" | "downloading" | "ready" | "error"`。
    pub state: String,
    /// 0..=100；非 downloading 状态一律 0。
    pub percent: u32,
    /// 错误详情；无错误时为空串。
    pub message: String,
}

/// 把一段最终识别文本拼进累积文本。
///
/// 中英混排的规则只有一条：**两侧都是 ASCII 字母数字时补一个空格**，其余情况直接相接。
/// 于是 `"hello"` + `"world"` → `"hello world"`，`"中文"` + `"测试"` → `"中文测试"`，
/// `"hello"` + `"中文"` → `"hello中文"`。片段先 `trim`，空片段直接忽略。
/// 纯函数（不碰 IO），`cargo test` 里直接测。
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
}
