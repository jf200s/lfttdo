//! 非 Windows 目标的占位实现。
//!
//! 这个模块的存在只为两件事：让 `cargo test` / `cargo clippy --all-targets` 在 Linux
//! 上能编译整棵 crate，以及让误在非 Windows 上调用语音命令时拿到一条明确的中文错误
//! （而不是一个链接失败或静默的 `Ok`）。它不模拟任何录音行为。

use std::path::Path;

const UNSUPPORTED: &str = "语音输入仅支持 Windows。";

/// 与 Windows 实现相同的句柄形状：非 Windows 上永远构造不出来（`start` 只返回 `Err`）。
pub struct VoiceSession;

pub fn start(_model_dir: &Path) -> Result<VoiceSession, String> {
    Err(UNSUPPORTED.into())
}

impl VoiceSession {
    pub fn stop(self) -> Result<String, String> {
        Err(UNSUPPORTED.into())
    }

    pub fn cancel(self) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
}
