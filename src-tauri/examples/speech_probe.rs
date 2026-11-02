//! Phase 0 探测：枚举 Windows SAPI 识别器、系统默认语言，并用 zh-CN 试建识别器 +
//! `LoadDictation`。**不启动麦克风采集**，可以安全地在任意机器上反复运行。
//!
//! 运行：`cargo run --example speech_probe`（Windows）。
//! 非 Windows 目标只打印提示，保证 `cargo clippy --all-targets` 在 Linux 上能过。

#[cfg(windows)]
fn main() {
    match lfttdo_lib::voice::probe() {
        Ok(report) => {
            println!(
                "系统默认语言: {}",
                report.default_language.as_deref().unwrap_or("（未知）")
            );
            println!("识别器 token 数量: {}", report.recognizers.len());
            for recognizer in &report.recognizers {
                let lcid = lfttdo_lib::voice::parse_language_attribute(&recognizer.language)
                    .map(|value| format!("0x{value:04X}"))
                    .unwrap_or_else(|| "（无法解析）".to_string());
                println!(
                    "#{} | token={} | {} | Language={} (LCID {})",
                    recognizer.index,
                    recognizer.token_id,
                    recognizer.description,
                    recognizer.language,
                    lcid
                );
            }
            match &report.zh_cn_dictation {
                Ok(()) => println!("zh-CN（LCID 0x804）识别器创建 + LoadDictation: 成功"),
                Err(error) => println!("zh-CN（LCID 0x804）识别器创建 + LoadDictation: {error}"),
            }
        }
        Err(error) => eprintln!("语音探测失败: {error}"),
    }
}

#[cfg(not(windows))]
fn main() {
    println!("语音探测仅支持 Windows。");
}
