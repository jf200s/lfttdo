//! Task 18：图片归一化。
//!
//! 一条命令进来的字节要变成模型能收的一块 `ContentBlock::Image`，这里负责全部规则：
//! 按**内容**（不是扩展名）判格式 → 把 EXIF 方向烘焙进像素 → 等比缩到最长边
//! [`MAX_EDGE_PX`] → webp/gif 收窄成 jpeg（gif 取首帧）→ 逐级降质直到 base64 后
//! 不超过 [`MAX_BASE64_BYTES`]。三条闸各自有各自的错误分支，不许互相顶替（R185）。

use crate::agent::types::{ContentBlock, ImageMime};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::ImageFormat;
use image::metadata::Orientation;
use image::{DynamicImage, GenericImageView, ImageDecoder, ImageReader};
use std::io::Cursor;

pub const MAX_EDGE_PX: u32 = 1568;
pub const MAX_BASE64_BYTES: usize = 5_000_000;
/// R185：入流字节上限（编码后的文件大小，不是像素数）。
const MAX_INPUT_BYTES: usize = 40_000_000;
/// R183：解码缓冲的分配预算，取值照 `image::Limits::default()` 的 `max_alloc`，
/// 也就是 `ImageReader::decode()` 里那道 `limits.reserve(total_bytes)` 的等价物。
const MAX_DECODE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;
/// 像素总数上限（与分配预算互补）：一张纯色 10000×10000 的 PNG 编码后可能不到 1MB，
/// `total_bytes` 也只有 300MB（低于 512MB 分配预算），但解码出来仍是一张 1 亿像素的图。
/// 缩放发生在解码**之后**，所以像素闸只能在这里、解码**之前**。
const MAX_PIXELS: u64 = 40_000_000;
/// jpeg 首发质量。png 首发不吃它（png 编码器没有质量旋钮，走 `encode_png`，F6-N1）；
/// 两支各自只编这一次，超线才进阶梯。
const FIRST_JPEG_QUALITY: u8 = 90;
/// PNG 超限回落成 JPEG 时的第一级（PNG 自己没有质量旋钮）。
const PNG_FALLBACK_JPEG_QUALITY: u8 = 85;
/// 降质阶梯。R184 实测：1568² 的纯噪声在 q=85 是 5.18 MB base64（刚好超），
/// q=75 是 4.11 MB（已经过线）—— 阶梯走到第二级必然回落成功。
const QUALITY_LADDER: [u8; 4] = [75, 60, 45, 30];

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("图片无法识别为图片（可能是伪装扩展名或已损坏）")]
    Unreadable,
    #[error("图片降质到最低后仍超过 5MB（当前 {actual} 字节），请改用更小的截图")]
    TooLargeAfterCompression { actual: usize },
    #[error("不支持的图片格式，仅接受 png / jpeg / webp / gif")]
    Unsupported,
    /// F5（R185）：解码已经成功、**编码器**自己拒收（边长超上限、颜色型不支持等）。
    /// 这一支从前被 `map_err` 折成 [`Self::Unreadable`]，等于对着用户说「你的文件认不出来」，
    /// 而它的文件当然认得出来 —— 顶替原因就是 R185 不许的那件事。
    #[error("图片转码失败（已读出像素，是编码器拒收，不是文件认不出来）")]
    EncodeFailed,
    #[error("图片太大（当前 {actual} 字节），请先裁剪或缩小后再发送")]
    InputTooLarge { actual: usize },
    #[error("图片像素过大（{width}×{height}，上限 4000 万像素），请先缩小后再发送")]
    TooManyPixels { width: u32, height: u32 },
}

/// 生产入口：常量就是界面承诺的那两个数。
pub fn normalize_image(bytes: &[u8]) -> Result<ContentBlock, ImageError> {
    normalize_with(bytes, MAX_EDGE_PX, MAX_BASE64_BYTES)
}

/// 上限可注入的实现（R184①）。**不是**给前端用的第三种接口 —— IPC 那一层只调
/// [`normalize_image`]，注入的两个参数只是让「降质到最低仍然超线」这条在生产常量下
/// 今天不可达的分支（R184）能在测试里被真的执行一遍。
pub fn normalize_with(
    bytes: &[u8],
    max_edge_px: u32,
    max_base64_bytes: usize,
) -> Result<ContentBlock, ImageError> {
    // R185：这道闸管的是体积，不是格式，所以它不能说「不支持的图片格式」。
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ImageError::InputTooLarge {
            actual: bytes.len(),
        });
    }

    // 按内容判格式：用户改名成 `.png` 的 webp 不该被误判。
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ImageError::Unreadable)?;
    let fmt = reader.format().ok_or(ImageError::Unreadable)?;
    let keep_png = matches!(fmt, ImageFormat::Png);
    if !(keep_png
        || matches!(
            fmt,
            ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
        ))
    {
        return Err(ImageError::Unsupported);
    }

    // R182：`ImageReader::decode()` 内部只做 make_decoder + set_limits + from_decoder
    // （image_reader_type.rs:311-321），**从不读 orientation**；重编码成 JPEG 又会抹掉 EXIF。
    // 所以方向只能在这里、在同一次解码里拿到，并在缩放与重编码之前烘焙进像素
    // （配方照 dynimage.rs:1145-1152 的原样）。`format()` 是 `&self` 借用，借完之后
    // `into_decoder()` 才能夺回所有权 —— 顺序写对就能编译。
    let mut decoder = reader.into_decoder().map_err(|_| ImageError::Unreadable)?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    // R183：换成 decoder-first 会把 `decode()` 自带的 `limits.reserve(decoder.total_bytes())`
    // 悄悄弄丢（`into_decoder()` 只做 make_decoder + set_limits，实测 :219-224 没有 reserve），
    // 而 `Limits::default()` 宽高不限、只卡 512 MiB 分配 —— 那道 reserve 就是「小文件、巨尺寸」
    // 炸弹唯一的闸。上面的 `MAX_INPUT_BYTES` 管的是入流大小，管不了 `total_bytes`，补回它：
    // 调用点在 `from_decoder` 之前，判定本身单独立成 `within_decode_budget`，原因见那条测试的注释。
    if !within_decode_budget(decoder.total_bytes()) {
        return Err(ImageError::Unreadable);
    }
    // 分配预算之后再过像素闸：超高像素但 total_bytes 不到 512MB 的图（纯色大图）
    // 会走到这里；缩放发生在解码之后，拦不住一次 1 亿像素的解码。
    let (pw, ph) = decoder.dimensions();
    if u64::from(pw) * u64::from(ph) > MAX_PIXELS {
        return Err(ImageError::TooManyPixels {
            width: pw,
            height: ph,
        });
    }
    let mut img = DynamicImage::from_decoder(decoder).map_err(|_| ImageError::Unreadable)?;
    img.apply_orientation(orientation); // NoTransforms 是 no-op，不必加 if

    let (w, h) = img.dimensions();
    // 缩放系数是「目标边长 / 当前最长边」再夹到 1.0：本来就不超限的图一律不动。
    // brief:88 写的是反的（`w.max(h) as f32 / max_edge_px as f32`），那条式子对超限的图
    // 得到 >1 再被 `.min(1.0)` 夹成 1.0 ⇒ 大图不缩；对小图得到 <1 ⇒ 反而把小图缩成邮票
    // （`downscales_when_long_edge_exceeds_limit` 与 `leaves_small_images_untouched_dimensions`
    // 两条同时红，实测见 findings）。这里按接口声明「等比缩到最长边 <= MAX_EDGE_PX」修正。
    let scale = (max_edge_px as f32 / w.max(h) as f32).min(1.0);
    let img = if scale < 1.0 {
        img.thumbnail_exact(
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
        )
    } else {
        img
    };

    // R137（正确性缺陷，不是风格）：`ImageMime` 只有 Png|Jpeg 且 `rename_all="lowercase"`，
    // mime 若在回落分支里不跟着改，返回的就是「data 是 JPEG 字节、声明是 png」，模型端按 png
    // 解 jpeg 会拒收或静默出脏图。所以回落必须**同时**改 mime，声明与字节永远同源。
    let mut mime = if keep_png {
        ImageMime::Png
    } else {
        ImageMime::Jpeg
    };
    // F6-N1（行为一字未变）：png 首发从前也接着 `FIRST_JPEG_QUALITY` 这个参数，而 png 编码器
    // 根本没有质量旋钮 —— 那个值进去就是个被忽略的摆设，名字却把它挂在 png 的调用上。
    // 现在拆成两个函数，png 这条路在**签名上**就没有质量参数可吃。
    // mime 与编码器仍由同一个 `keep_png` 决定 ⇒ R137 的「声明与字节同源」没有变两份真相。
    let mut encoded = if keep_png {
        encode_png(&img)?
    } else {
        encode_jpeg(&img, FIRST_JPEG_QUALITY)?
    };

    if base64_len(encoded.len()) > max_base64_bytes {
        if keep_png {
            // PNG 没有质量旋钮：只能换成 JPEG 再来 —— 换的那一刻 mime 必须跟着变（R137）。
            mime = ImageMime::Jpeg;
            encoded = encode_jpeg(&img, PNG_FALLBACK_JPEG_QUALITY)?;
        }
        for q in QUALITY_LADDER {
            if base64_len(encoded.len()) <= max_base64_bytes {
                break;
            }
            encoded = encode_jpeg(&img, q)?;
        }
    }

    let actual = base64_len(encoded.len());
    if actual > max_base64_bytes {
        return Err(ImageError::TooLargeAfterCompression { actual });
    }
    Ok(ContentBlock::Image {
        data: STANDARD.encode(&encoded),
        mime,
    })
}

/// jpeg 编码。质量旋钮只在这一支存在，所以它是唯一收 `quality` 的函数（F6-N1）。
///
/// R136（原写法编译不过）：内存编码只有 `write_with_encoder`，`Vec<u8>` 实现 `Write`
/// 但不实现 `Seek`，所以必须 `Cursor` 包一层再 `into_inner()`。
fn encode_jpeg(img: &DynamicImage, quality: u8) -> Result<Vec<u8>, ImageError> {
    // 显式 encoder，质量阶梯要用 `new_with_quality`（实测 codecs/jpeg/encoder.rs:398）。
    // `write_with_encoder` 的约束 `P: PixelWithColorType` 对 `Rgb<u8>` 成立（buffer.rs:1125）。
    let mut buf = Vec::new();
    img.to_rgb8()
        .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(
            &mut buf, quality,
        ))
        // F5：编码器拒收（边长/颜色型）不是输入问题，不许顶替成 Unreadable。
        .map_err(|_| ImageError::EncodeFailed)?;
    Ok(buf)
}

/// png 编码：签名里没有质量参数可吃（F6-N1）。
///
/// PNG 必须留在这条路上、不许 `to_rgb8()`：否则带 alpha 的截图会被抹成黑底。
/// 与 jpeg 那一支同理由用 `Cursor<Vec<u8>>` —— `write_to` 要求 `W: Write + Seek`
/// （dynimage.rs:1371）。
fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, ImageError> {
    let mut cur = Cursor::new(Vec::new());
    img.write_to(&mut cur, ImageFormat::Png)
        .map_err(|_| ImageError::EncodeFailed)?;
    Ok(cur.into_inner())
}

/// R183 那道闸的判定本身。取值照 `image::Limits::default().max_alloc`（limits.rs:49-56），
/// 也就是它替掉的那一行 `limits.reserve(decoder.total_bytes())` 的等价物。
///
/// 为什么要把一个 `<=` 单独成函数：炸弹夹具**证伪不了调用点**（实测见
/// `rejects_a_small_png_that_claims_huge_dimensions` 的注释），所以这里至少把边界值本身钉住，
/// 改动这个取值必须有人红。
fn within_decode_budget(total_bytes: u64) -> bool {
    total_bytes <= MAX_DECODE_ALLOC_BYTES
}

/// `STANDARD`（带 padding、不换行）编码后的字节数，不用真的编一遍就知道会不会超线。
fn base64_len(raw: usize) -> usize {
    raw.div_ceil(3) * 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use image::metadata::Orientation;
    use image::{
        ExtendedColorType, GenericImageView, ImageDecoder, ImageEncoder, ImageReader,
        codecs::png::PngEncoder,
    };

    /// 纯色 PNG（`new_rgb8` 是全 0 = 黑，压得动，用来测尺寸与尺寸之外的分支）。
    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = image::DynamicImage::new_rgb8(w, h).into_rgb8();
        let mut buf = Vec::new();
        PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }

    /// 逐像素独立随机 RGB 的 PNG：对 PNG 是零压缩（控制器实测 ratio 1.000），对 JPEG
    /// 是「降质也压不动」的最坏形态。R184 的裁定靠它 + 可注入上限。
    fn noise_png(w: u32, h: u32) -> Vec<u8> {
        let mut rng = rand::rng();
        let img = image::RgbImage::from_fn(w, h, |_, _| {
            image::Rgb([
                rand::Rng::random(&mut rng),
                rand::Rng::random(&mut rng),
                rand::Rng::random(&mut rng),
            ])
        });
        let mut buf = Vec::new();
        PngEncoder::new(&mut buf)
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
            .unwrap();
        buf
    }

    fn base64_of(blk: &ContentBlock) -> String {
        match blk {
            ContentBlock::Image { data, .. } => data.clone(),
            other => panic!("归一化必须返回 Image 块，得到 mime {}", mime_of(other)),
        }
    }

    /// 失败信息只报声明，不把整张 base64 打进 panic 输出（一次失败刷出 90KB 没人看得完，
    /// 而要看的就是「声明是什么」那一小块）。
    fn mime_of(blk: &ContentBlock) -> String {
        match blk {
            ContentBlock::Image { mime, .. } => format!("{mime:?}"),
            ContentBlock::Text { .. } => "不是 Image 块（Text）".to_string(),
            other => format!("不是 Image 块（{other:?}）"),
        }
    }

    fn raw_of(blk: &ContentBlock) -> Vec<u8> {
        STANDARD.decode(base64_of(blk)).unwrap()
    }

    /// PNG 的 chunk：长度 + 类型 + 数据 + CRC32（CRC 覆盖类型与数据）。
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for b in bytes {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut over = kind.to_vec();
        over.extend_from_slice(data);
        out.extend_from_slice(&crc32(&over).to_be_bytes());
        out
    }

    /// R183 的解压炸弹夹具：IHDR 声称 `w`×`h`、IDAT 只有一字节的 zlib 流。
    /// 文件极小（几十字节），但 `decoder.total_bytes()` 是天文数字。
    fn bomb_png(w: u32, h: u32) -> Vec<u8> {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        // bit depth 8 / color type 2 (RGB) / compression 0 / filter 0 / interlace 0
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
        // 一个合法但只解码出 1 个字节的 zlib 流：远不够填首行，更够填 40000 行
        out.extend_from_slice(&png_chunk(
            b"IDAT",
            &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x01, 0x00, 0x01],
        ));
        out.extend_from_slice(&png_chunk(b"IEND", &[]));
        out
    }

    /// R182 的夹具：`image` 的 JPEG **编码器不写 EXIF**，所以方向段只能手工插。
    /// APP1 里放一个最小的 TIFF（小端、一条 IFD entry：tag 0x0112 / SHORT / count 1 /
    /// value = `orientation`）。图像本身是「上半亮、下半暗」的横带，方向被烘焙进像素后
    /// 亮带会变成竖带 —— 宽高互换 + 亮带换轴，两件事一起才叫真的应用了方向。
    fn jpeg_with_orientation(orientation: u16, w: u32, h: u32) -> Vec<u8> {
        let img = image::RgbImage::from_fn(w, h, |_, y| {
            if y < h / 2 {
                image::Rgb([255, 255, 255])
            } else {
                image::Rgb([0, 0, 0])
            }
        });
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 90)
            .write_image(img.as_raw(), w, h, ExtendedColorType::Rgb8)
            .unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "夹具前提：JPEG 以 SOI 开头");

        let mut tiff: Vec<u8> = vec![b'I', b'I', 42, 0, 8, 0, 0, 0];
        tiff.extend_from_slice(&1u16.to_le_bytes()); // entry 数
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // tag = Orientation
        tiff.extend_from_slice(&3u16.to_le_bytes()); // type = SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count
        tiff.extend_from_slice(&orientation.to_le_bytes()); // value
        tiff.extend_from_slice(&0u16.to_le_bytes()); // padding（value 占 4 字节）
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 无

        let mut app1: Vec<u8> = vec![0xFF, 0xE1];
        // 段长度字段含它自己那 2 字节，不含标记本身
        let seg_len = (2 + 6 + tiff.len()) as u16;
        app1.extend_from_slice(&seg_len.to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);

        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    /// R183 的第二半：闸的**取值**本身。
    /// 为什么需要这条：上面那条炸弹用例的 `Err(Unreadable)` **证伪不了调用点** ——
    /// 本机实测把 `within_decode_budget` 的调用删掉后，同一条夹具仍然 0.00s 通过，因为
    /// 4.8 GB 的解码缓冲在 Windows 上是**惰性提交的虚拟内存**（`vec![0u8; 4_800_000_000]`
    /// 225µs 就返回成功），而截断的 IDAT 让 png 在填满缓冲之前就报
    /// `Decoding(NoMoreImageData)`（对照：有 reserve 时报 `Limits(InsufficientMemory)`，34.7µs）。
    /// 两支都被 `map_err` 收成同一个 `Unreadable`，所以错误类型分不出来。这道闸真正的价值在
    /// 「数据够长」的完整炸弹上（那时才会真的提交 4.8 GB），那种文件没法在单测里造，
    /// 于是这里直接把边界钉住：改这个取值必须有人红。
    #[test]
    fn the_decode_budget_matches_the_reserve_it_replaces() {
        // 与它替掉的那行 `limits.reserve(...)` 同取值 = `image::Limits::default().max_alloc`
        assert_eq!(MAX_DECODE_ALLOC_BYTES, 512 * 1024 * 1024);
        // 生产路径最大的一张（1568² RGB8）必须在线内，否则谁都发不出去
        assert!(within_decode_budget(1568 * 1568 * 3));
        // 炸弹夹具那张必须在线外
        assert!(!within_decode_budget(40_000 * 40_000 * 3));
    }

    #[test]
    fn downscales_when_long_edge_exceeds_limit() {
        let blk = normalize_image(&png(4000, 1000)).unwrap();
        let back = image::load_from_memory(&raw_of(&blk)).unwrap();
        assert!(back.width() <= MAX_EDGE_PX, "宽 {} 超限", back.width());
        assert!(back.height() <= MAX_EDGE_PX, "高 {} 超限", back.height());
    }

    #[test]
    fn leaves_small_images_untouched_dimensions() {
        let blk = normalize_image(&png(200, 120)).unwrap();
        let back = image::load_from_memory(&raw_of(&blk)).unwrap();
        assert_eq!((back.width(), back.height()), (200, 120));
    }

    #[test]
    fn converts_webp_and_gif_to_jpeg_first_frame() {
        // GIF：两帧才叫得出「首帧」。第一帧红、第二帧蓝，归一化只许交回首帧。
        let red = image::RgbaImage::from_pixel(40, 40, image::Rgba([255, 0, 0, 255]));
        let blue = image::RgbaImage::from_pixel(40, 40, image::Rgba([0, 0, 255, 255]));
        let mut gif_buf = Vec::new();
        {
            let mut enc = image::codecs::gif::GifEncoder::new(&mut gif_buf);
            enc.encode_frames(vec![image::Frame::new(red), image::Frame::new(blue)])
                .unwrap();
        }
        let blk = normalize_image(&gif_buf).unwrap();
        // 内部只允许 Png | Jpeg，其余格式在归一化处收窄
        assert!(
            matches!(
                blk,
                ContentBlock::Image {
                    mime: ImageMime::Jpeg,
                    ..
                }
            ),
            "gif 必须收成 jpeg，得到 mime {}",
            mime_of(&blk)
        );
        let back = image::load_from_memory(&raw_of(&blk)).unwrap();
        assert_eq!((back.width(), back.height()), (40, 40));
        let p = back.get_pixel(20, 20).0;
        assert!(p[0] > 180 && p[2] < 80, "必须是首帧（红），得到 {p:?}");

        // WebP：同样收成 jpeg（`ImageMime` 里根本没有 webp 这一支）
        let mut webp_buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(32, 16)
            .write_to(&mut webp_buf, image::ImageFormat::WebP)
            .unwrap();
        let blk = normalize_image(webp_buf.get_ref()).unwrap();
        assert!(
            matches!(
                blk,
                ContentBlock::Image {
                    mime: ImageMime::Jpeg,
                    ..
                }
            ),
            "webp 必须收成 jpeg，得到 mime {}",
            mime_of(&blk)
        );
        let back = image::load_from_memory(&raw_of(&blk)).unwrap();
        assert_eq!((back.width(), back.height()), (32, 16));
    }

    #[test]
    fn reports_unreadable_for_bytes_that_carry_no_image_magic() {
        // R185 之后这一支只管「连魔数都认不出来」：`with_guessed_format` 给 None ⇒ Unreadable。
        // 测试名原样承自 brief 的 `..._unsupported_...`（F6-N3）：真 `Unsupported` 另有专测
        // （上面 `reports_unsupported_for_a_real_but_unaccepted_format`），这里断言的一直是
        // `Unreadable`，所以改的是名字，不是断言。
        let err = normalize_image(b"this is definitely not an image").unwrap_err();
        assert!(matches!(err, ImageError::Unreadable), "得到 {err:?}");
        assert!(err.to_string().contains("无法识别为图片"));
    }

    /// R185 的另一支：格式认得出来（BMP 魔数），但它不在本任务接受的四种格式里。
    /// 这一支的文案必须是「不支持的图片格式」，而不是把体积问题说成格式问题。
    #[test]
    fn reports_unsupported_for_a_real_but_unaccepted_format() {
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0u8; 60]);
        let err = normalize_image(&bmp).unwrap_err();
        assert!(
            matches!(err, ImageError::Unsupported),
            "bmp 该说不支持，得到 {err:?}"
        );
        assert!(err.to_string().contains("不支持的图片格式"));
    }

    /// F5（R185 的第四支）：**编码器**拒收不许被说成「无法识别为图片」。
    /// 构造法：JPEG 的边长上限是 65535（实测 image-0.25.10 `codecs/jpeg/encoder.rs:357`
    /// 「width and height must be >= 1 and <= 65535」），把一张 65536×1 的图直接交给
    /// `encode` —— 能走到这一步说明解码早就成功了，失败只能是编码器自己拒的。
    #[test]
    fn an_encoder_rejection_is_not_blamed_on_the_input() {
        let img = image::DynamicImage::new_rgb8(65536, 1);
        let err = encode_jpeg(&img, FIRST_JPEG_QUALITY)
            .expect_err("夹具前提：超过 JPEG 边长上限，编码器必须报错");
        assert!(
            matches!(err, ImageError::EncodeFailed),
            "这一支必须是「编码器拒收」，得到 {err:?}"
        );
        assert!(
            !err.to_string().contains("无法识别"),
            "编码器故障被说成「输入不是图片」：{err}"
        );
    }

    /// R185 的第一支：40MB 那道前置闸管的是**入流字节数**，格式完全受支持也一样拦。
    /// 以前它报「不支持的图片格式」——那句话在用户脸上是错的，会把人引去换格式。
    #[test]
    fn reports_input_too_large_for_a_huge_but_supported_file() {
        let mut bytes = png(16, 16);
        bytes.resize(MAX_INPUT_BYTES + 1, 0);
        let err = normalize_image(&bytes).unwrap_err();
        assert!(
            matches!(err, ImageError::InputTooLarge { actual } if actual == MAX_INPUT_BYTES + 1),
            "超 40MB 该说太大，得到 {err:?}"
        );
        assert!(err.to_string().contains(&(MAX_INPUT_BYTES + 1).to_string()));
        // 边界的另一侧：一张小 png 不许被这道闸误伤
        assert!(
            normalize_image(&png(16, 16)).is_ok(),
            "卡在线内的小图必须走完整流程"
        );
    }

    /// R184①②：生产常量下这条路**今天不可达**（见 R184 的阶梯表），所以把上限做成可注入
    /// 参数、用小上限把这条分支真的执行一遍。生产常量一个都没改。
    #[test]
    fn reports_too_large_when_still_over_cap_after_quality_steps() {
        let err = normalize_with(&noise_png(300, 300), 300, 2_000).unwrap_err();
        let actual = match err {
            ImageError::TooLargeAfterCompression { actual } => actual,
            other => panic!("必须指名 TooLargeAfterCompression 这个原因，得到 {other:?}"),
        };
        assert!(actual > 2_000, "报出来的数字得真的超过上限，得到 {actual}");
        assert!(
            err.to_string().contains(&actual.to_string()),
            "文案里的数字必须与实际字节数一致，实际文案：{err}"
        );
    }

    /// 产物要能直接塞进 JSON 字段：**带 padding、不换行**的 STANDARD base64。
    /// 夹具取 64×64：实测 jpeg raw = 325（325 % 3 == 1 ⇒ 结尾必有两个 `=`）。
    /// 原本用的 300×200 那档 raw = 1692 恰好是 3 的倍数，`STANDARD_NO_PAD` 也能满足
    /// `len % 4 == 0` —— 那条断言在这套夹具下**证伪不了**（实测 mutation 是绿的），
    /// 所以换成需要 padding 的尺寸，并把「夹具确实需要 padding」本身钉住，
    /// 免得将来换夹具又悄悄失去牙齿而没人察觉。
    #[test]
    fn produces_valid_base64_without_newlines() {
        let blk = normalize_image(&png(64, 64)).unwrap();
        let data = base64_of(&blk);
        assert!(
            !data.contains('\n') && !data.contains('\r'),
            "base64 里不许有换行"
        );
        assert_eq!(data.len() % 4, 0, "带 padding 的 base64 长度必是 4 的倍数");
        assert!(data.ends_with('='), "该有 padding，实际长度 {}", data.len());
        // 夹具前提放在最后：它不是被测行为，是「上面三条还有没有牙齿」的自检。
        assert_eq!(
            raw_of(&blk).len() % 3,
            1,
            "夹具前提：字节数必须需要 padding，否则这三条没有牙齿"
        );
    }

    /// R137 的守门测试：**声明必须与字节同源**。
    /// PNG 超限回落 JPEG 后，`mime` 若不跟着改，线上就是「声明 png、实为 jpeg」，
    /// 模型端按 png 解 jpeg 会拒收或静默出脏图。所以这里不许只 `matches!` 查 mime，
    /// 必须**读回字节的 magic**：JPEG 是 `FF D8`，PNG 是 `89 50 4E 47`。
    /// 触发回落的办法是 R184① 的可注入上限（小 `max_base64_bytes`），夹具因此很小很快。
    #[test]
    fn mime_matches_the_actual_bytes_after_png_fallback() {
        // 回落那一侧：png 压不过小上限 ⇒ 改 JPEG，mime 必须同时改。
        // 上限 100_000 是按本机实测挑的：300×300 噪声 png_b64=360,520、q=85=194,932、
        // q=60=120,520、q=45=97,508 ⇒ 阶梯走到 q=45 才过线，回落分支与阶梯都被真执行。
        let blk = normalize_with(&noise_png(300, 300), 300, 100_000).unwrap();
        let raw = raw_of(&blk);
        match &blk {
            ContentBlock::Image {
                mime: ImageMime::Jpeg,
                ..
            } => {
                assert_eq!(&raw[..2], &[0xFF, 0xD8], "声明 jpeg 但字节不是 jpeg")
            }
            other => panic!("回落之后该交出 jpeg 块，得到 mime {}", mime_of(other)),
        }
        // 没回落那一侧：小图原样留 png，声明与字节也得对得上（match 的另一支）
        let blk = normalize_image(&png(200, 120)).unwrap();
        let raw = raw_of(&blk);
        match &blk {
            ContentBlock::Image {
                mime: ImageMime::Png,
                ..
            } => {
                assert_eq!(
                    &raw[..4],
                    &[0x89, 0x50, 0x4E, 0x47],
                    "声明 png 但字节不是 png"
                )
            }
            other => panic!("小 png 该原样留 png，得到 mime {}", mime_of(other)),
        }
    }

    /// R184③：正相反的一条 —— 「1568 缩放 + 阶梯本身就是那道闸」。最难压的图（逐像素噪声）
    /// 走**生产上限**必须是 Ok。将来谁把 MAX_EDGE_PX 放宽、或把阶梯砍到只剩 q=85，
    /// `TooLargeAfterCompression` 会突然开始 reachable，这条会先红。
    ///
    /// 注一：R184③ 原文的夹具是 `noise_png(4000, 4000)`，但本机实测 4000² 噪声 png =
    /// 48,004,000 字节（噪声对 PNG 是零压缩），会先撞上 R185 那道 40 MB 入流闸而只能得到
    /// `InputTooLarge`，**永远不会 Ok**，所以夹具退到 2000×2000（长边仍 > 1568 ⇒ 仍走缩放）。
    /// 注二：控制器表格里 1568² 纯噪声 q=85 = 5,179,040（超）/ q=75 = 4,106,664（过线）是
    /// **不经重采样**的那一档；从 2000 缩下来的图被 Lanczos3 抹平了一层，q=85 就已经过线。
    /// 所以这里两档都钉：缩放档（含尺寸断言）与原生 1568 档（真的走到阶梯第二级）。
    #[test]
    fn the_downscale_is_what_keeps_the_worst_case_under_the_cap() {
        let blk = normalize_with(&noise_png(2000, 2000), MAX_EDGE_PX, MAX_BASE64_BYTES)
            .expect("长边超限的噪声图必须被缩到能发出去");
        let raw = raw_of(&blk);
        let back = image::load_from_memory(&raw).unwrap();
        assert_eq!(
            (back.width(), back.height()),
            (1568, 1568),
            "长边必须被缩到上限"
        );
        assert!(
            raw.len() <= MAX_BASE64_BYTES,
            "交出去的字节 {} 超过 {MAX_BASE64_BYTES}",
            raw.len()
        );

        // 原生 1568² 噪声：png_b64 = 7.38 MB ⇒ 回落 q=85 = 5,179,040 仍然超 ⇒
        // 必须靠阶梯的下一级（q=75 = 4,106,664）过线。阶梯被砍短就红在这里。
        let blk = normalize_with(&noise_png(1568, 1568), MAX_EDGE_PX, MAX_BASE64_BYTES)
            .expect("1568² 纯噪声也必须能发出去（阶梯第二级）");
        assert!(
            matches!(
                blk,
                ContentBlock::Image {
                    mime: ImageMime::Jpeg,
                    ..
                }
            ),
            "png 压不动 ⇒ 必须回落 jpeg，得到 mime {}",
            mime_of(&blk)
        );
        let raw = raw_of(&blk);
        assert_eq!(&raw[..2], &[0xFF, 0xD8]);
        assert!(
            raw.len() <= MAX_BASE64_BYTES,
            "交出去的字节 {} 超过 {MAX_BASE64_BYTES}",
            raw.len()
        );
    }

    /// R183：decoder-first 会把 `decode()` 自带的 `limits.reserve(total_bytes)` 那道闸弄丢，
    /// 必须显式补回。夹具是「小文件、巨尺寸」：IHDR 声称 40000×40000（`total_bytes` = 4.8 GB），
    /// 文件只有几十字节。
    ///
    /// 诚实说明：这条**证伪不了调用点** —— 把 `within_decode_budget` 的调用删掉它照样 0.00s 通过
    /// （4.8 GB 缓冲在 Windows 上是惰性提交的虚拟内存，225µs 就返回，而截断的 IDAT 让 png 先报
    /// `Decoding(NoMoreImageData)`；对照有 reserve 时报 `Limits(InsufficientMemory)`，34.7µs。
    /// 两支都被 `map_err` 收成同一个 `Unreadable`，错误类型分不出来）。取值本身由
    /// [`the_decode_budget_matches_the_reserve_it_replaces`] 钉住。这条留着是因为：谁把 `map_err`
    /// 改成 panic、或把这条路径整个重构掉，这里会响。
    #[test]
    // 夹具前提二的条件虽是编译期常量，但它是夹具护栏：MAX_DECODE_ALLOC_BYTES 或夹具尺寸
    // 一旦改动必须当场红；clippy 的 const 块化会改变失败模式，删除则失去护栏，故保留原样。
    #[allow(clippy::assertions_on_constants)]
    fn rejects_a_small_png_that_claims_huge_dimensions() {
        let bytes = bomb_png(40_000, 40_000);
        // 夹具前提一：这是个小文件，40MB 那道入流闸管不到它 —— 拦住它的只能是分配预算
        assert!(
            bytes.len() < 1_000,
            "夹具前提：炸弹文件应当极小，实际 {} 字节",
            bytes.len()
        );
        // 夹具前提二：它声称的解码缓冲确实超过闸的取值
        assert!(
            40_000u64 * 40_000 * 3 > MAX_DECODE_ALLOC_BYTES,
            "夹具前提：尺寸必须大到越过分配预算"
        );
        // 夹具前提三：IHDR/CRC 是真的能被 png 解析（否则这条测的是解析失败，不是闸）
        let probe = ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .expect("夹具前提：IHDR 必须能被解码器读出来");
        assert_eq!(probe.dimensions(), (40_000, 40_000));

        let err = normalize_image(&bytes).unwrap_err();
        assert!(
            matches!(err, ImageError::Unreadable),
            "炸弹必须给出明确错误而不是放行/panic，得到 {err:?}"
        );
    }

    /// 像素闸：10000×10000 的纯色 PNG 可以很小，`total_bytes` 也只有 300MB
    /// （低于 512MB 分配预算），但解码仍是一次 1 亿像素的分配 —— 必须在解码之前拒。
    #[test]
    fn rejects_a_high_pixel_count_image_before_decoding_it() {
        let bytes = bomb_png(10_000, 10_000);
        // 前提从夹具本身量出来，而不是写一条常量比较：这张图必须**低于**分配预算，
        // 否则测到的是另一道闸。
        let probe = ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert!(
            probe.total_bytes() < MAX_DECODE_ALLOC_BYTES,
            "夹具前提：total_bytes={} 必须低于分配预算",
            probe.total_bytes()
        );
        let err = normalize_image(&bytes).unwrap_err();
        assert!(
            matches!(
                err,
                ImageError::TooManyPixels {
                    width: 10_000,
                    height: 10_000
                }
            ),
            "必须给出明确的像素过大错误，得到 {err:?}"
        );
    }

    /// R182：手机照片的方向必须在归一化内部、缩放与重编码之前烘焙进像素。
    /// 判据是出来的 jpeg **宽高互换**且亮带换轴；`image` 的 JPEG 编码器不写 EXIF，
    /// 所以夹具是自己手插 APP1/EXIF 的 —— 先拿解码器验夹具本身能被认出方向，
    /// 认不出来就是夹具假了，这条会在这里红，而不是在后面假装绿。
    #[test]
    fn bakes_exif_orientation_into_pixels_before_reencoding() {
        let (w, h) = (64u32, 32u32);
        let bytes = jpeg_with_orientation(6, w, h); // 6 = Rotate90
        let mut probe = ImageReader::new(std::io::Cursor::new(&bytes))
            .with_guessed_format()
            .unwrap()
            .into_decoder()
            .unwrap();
        assert_eq!(
            probe.orientation().unwrap(),
            Orientation::Rotate90,
            "夹具前提：手造的 EXIF 方向必须真能被解码器读出来"
        );

        let blk = normalize_image(&bytes).unwrap();
        let back = image::load_from_memory(&raw_of(&blk)).unwrap();
        assert_eq!(
            (back.width(), back.height()),
            (h, w),
            "方向 6 必须让宽高互换"
        );

        // 亮带从横变竖：没应用方向时，左右两半一样亮（差 0）；应用后一亮一暗。
        let mid_x = back.width() / 2;
        let (mut left, mut nl) = (0u64, 0u64);
        let (mut right, mut nr) = (0u64, 0u64);
        for (x, _, p) in back.pixels() {
            let v = u64::from(p.0[0]);
            if x < mid_x {
                left += v;
                nl += 1;
            } else {
                right += v;
                nr += 1;
            }
        }
        let (lm, rm) = (left as f64 / nl as f64, right as f64 / nr as f64);
        assert!(
            (lm - rm).abs() > 60.0,
            "方向没被烘焙进像素：亮带还是横的（左均 {lm:.1} 右均 {rm:.1}）"
        );
    }
}
