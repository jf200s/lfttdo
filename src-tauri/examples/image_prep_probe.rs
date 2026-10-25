//! R189①：图片归一化的**真字节**探针（非生产代码）。
//!
//! 为什么要有这个文件：brief 原判据「DevTools 里确认 base64 长度 ≤5MB」在这条栈上看不见东西
//! —— `normalize_image` 是一次 Tauri IPC，`agent` 是 Tauri event，两者都不进 DevTools Network，
//! 而 `tauri.conf.json` 没有 `withGlobalTauri`（R154/R189 实测）。所以尺寸、mime、字节数只能在
//! Rust 侧量。
//!
//! 用法：`cargo run --example image_prep_probe -- <图片路径> [<更多路径> …]`
//! 每个文件打印：原始字节 / 头部声明的格式与尺寸 / 结果（mime、base64 字节、**把 base64 解回来
//! 再读一次**的宽高）或错误原话。
//!
//! 「缩放后尺寸」刻意从**返回的字节**里读，而不是从像素计算里复算：探针要证明的是「发出去的那
//! 一段到底是什么」，自己再算一遍就把被测对象换成了自己。
//!
//! 不需要 key，也不需要端点 —— 它只走 `normalize_image` 这一条纯本地路径。

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::GenericImageView;
use image::ImageDecoder;
use image::metadata::Orientation;
use lfttdo_lib::image_prep::{MAX_BASE64_BYTES, MAX_EDGE_PX, normalize_image};
use std::time::Instant;

fn main() {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        eprintln!("用法: cargo run --example image_prep_probe -- <图片路径> …");
        std::process::exit(2);
    }
    println!(
        "常量: MAX_EDGE_PX={MAX_EDGE_PX}  MAX_BASE64_BYTES={MAX_BASE64_BYTES}（入流字节闸 MAX_INPUT_BYTES 是私有的，它拦住的文件在这里表现为 Err 原话）"
    );
    println!("列: 原字节 | 头部格式 声明尺寸 | 结果 mime | base64 字节 | 解回尺寸 | 耗时");
    println!();
    for p in &paths {
        report(p);
    }
}

fn report(path: &str) {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            println!("{path}\n  读文件失败: {e}");
            return;
        }
    };
    // 头部声明尺寸只读 metadata，不解码；EXIF 方向要下一次解码才拿得到，所以这里标的是
    // 「声明」而不是「烘焙方向之后」。
    let header = image::image_dimensions(path)
        .map(|(w, h)| format!("{w}x{h}"))
        .unwrap_or_else(|_| "读不出".to_string());
    let fmt = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map(|r| {
            r.format()
                .map(|f| format!("{f:?}"))
                .unwrap_or_else(|| "认不出".to_string())
        })
        .unwrap_or_else(|_| "认不出".to_string());

    let t = Instant::now();
    let outcome = normalize_image(&bytes);
    let ms = t.elapsed().as_millis();

    println!("{path}");
    println!("  原字节 {}  头部 {fmt} 声明尺寸 {header}", bytes.len());
    match outcome {
        Ok(block) => {
            let (data, mime) = match block {
                lfttdo_lib::agent::types::ContentBlock::Image { data, mime } => {
                    (data, format!("{mime:?}"))
                }
                _ => ("(不是图片块)".to_string(), "?".to_string()),
            };
            let back = STANDARD
                .decode(&data)
                .ok()
                .and_then(|raw| {
                    let img = image::load_from_memory(&raw).ok()?;
                    let (w, h) = img.dimensions();
                    Some(format!(
                        "{w}x{h}  (编码后 {} 字节, 输出里剩的方向标签 = {})",
                        raw.len(),
                        out_orientation(&raw)
                    ))
                })
                .unwrap_or_else(|| "(base64 解不回图片)".to_string());
            println!(
                "  => mime {mime}  base64 {} 字节  解回尺寸 {back}  {ms}ms",
                data.len()
            );
        }
        Err(e) => println!("  => Err: {e}  ({ms}ms)"),
    }
    println!();
}

/// 输出字节里**还带着**的 EXIF 方向标签。R182 的口径是「方向必须在缩放与重编码之前烘焙进像素」，
/// 而 jpeg 重编码本就不携带 EXIF ⇒ 这一项应当恒为 `NoTransforms`。
///
/// 它不是多余的一列：「先旋转像素再缩放」和「先缩放再旋转像素」在宽高上长得一模一样
/// （这张 2400×1600 + Orientation=6 的夹具两种顺序都得到 1045×1568），只有这一列能把
/// 「方向被烘焙进像素」和「方向只是又被标签挂了一次」分开。
fn out_orientation(raw: &[u8]) -> String {
    image::ImageReader::new(std::io::Cursor::new(raw))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_decoder().ok())
        .map(|mut d| format!("{:?}", d.orientation().unwrap_or(Orientation::NoTransforms)))
        .unwrap_or_else(|| "?".to_string())
}
