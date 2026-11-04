//! 离线语音引擎探针：报告模型文件状态（存在/尺寸）、Windows 下列出 cpal 输入设备、
//! 模型就绪时加载 ort 模型（首次 ~1s），给了 wav 再跑一次识别。
//! **不联网、不采麦克风**，可以安全地反复运行。
//!
//! 运行：`cargo run --example speech_probe -- [模型目录] [测试wav]`。
//! Windows 缺省目录是 `%APPDATA%/com.qwa.do/models/sense-voice-zh-en-ja-ko-yue-2024-07-17`
//! （与 ipc.rs 的 app_data_dir 同一条路径）；其他平台请显式传模型目录。
//! 识别管线平台无关（ort 推理 + 自研前处理/解码），Linux 上也能跑模型/推理探针。

use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().map(PathBuf::from).unwrap_or_else(|| {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".into());
        PathBuf::from(appdata)
            .join("com.qwa.do")
            .join("models")
            .join(lfttdo_lib::voice::download::MODEL_DIR_NAME)
    });
    let wav = args.next().map(PathBuf::from);
    println!("模型目录: {}", dir.display());
    for name in ["tokens.txt", "model.int8.onnx"] {
        match std::fs::metadata(dir.join(name)) {
            Ok(m) => println!("  {name}: 存在，{} 字节", m.len()),
            Err(_) => println!("  {name}: 不存在"),
        }
    }
    let status = lfttdo_lib::voice::download::status_of(&dir);
    println!("模型状态: {}", status.state);

    #[cfg(windows)]
    match lfttdo_lib::voice::input_devices() {
        Ok(names) if names.is_empty() => println!("输入设备: 无（没有麦克风）"),
        Ok(names) => {
            println!("输入设备: {} 个", names.len());
            for name in names {
                println!("  - {name}");
            }
        }
        Err(e) => println!("输入设备枚举失败: {e}"),
    }

    if status.state != "ready" {
        println!("模型加载: 跳过（模型未就绪）");
        return;
    }
    let t = std::time::Instant::now();
    let model = match lfttdo_lib::voice::asr::cached(&dir) {
        Ok(m) => m,
        Err(e) => {
            println!("模型加载失败: {e}");
            return;
        }
    };
    println!("模型加载: 成功，耗时 {:.1}s", t.elapsed().as_secs_f64());

    let Some(wav) = wav else {
        println!("识别探针: 跳过（未提供测试 wav）");
        return;
    };
    let mut reader = match hound::WavReader::open(&wav) {
        Ok(r) => r,
        Err(e) => {
            println!("无法打开 wav {}：{e}", wav.display());
            return;
        }
    };
    let spec = reader.spec();
    let max = 2i64.pow(spec.bits_per_sample as u32 - 1) as f32;
    let samples: Vec<f32> = reader
        .samples::<i32>()
        .step_by(spec.channels as usize)
        .filter_map(|s| s.ok())
        .map(|s| s as f32 / max)
        .collect();
    let t = std::time::Instant::now();
    match model.recognize(&samples, spec.sample_rate, true) {
        Ok(text) => println!(
            "识别: 「{text}」（{:.2}s 音频，耗时 {:.2}s）",
            samples.len() as f64 / spec.sample_rate as f64,
            t.elapsed().as_secs_f64()
        ),
        Err(e) => println!("识别失败: {e}"),
    }
}
