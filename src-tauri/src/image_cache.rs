//! Tool image cache: event images are written under the session output directory.
use crate::agent::events::AgentEvent;
use crate::agent::types::ImageMime;
use base64::Engine as _;
use std::path::{Path, PathBuf};

pub const CACHE_SUBDIR: &str = "images";

pub fn save(
    cache_dir: &Path,
    call_id: &str,
    index: usize,
    data_b64: &str,
    mime: &ImageMime,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(cache_dir)
        .map_err(|e| format!("创建图片缓存目录失败：{}: {e}", cache_dir.display()))?;
    let ext = match mime {
        ImageMime::Png => "png",
        ImageMime::Jpeg => "jpeg",
    };
    let name = format!(
        "{}-{index}-{:016x}.{ext}",
        sanitize_call_id(call_id),
        fnv1a(data_b64.as_bytes())
    );
    let path = cache_dir.join(name);
    if path.is_file() {
        return Ok(path);
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .map_err(|e| format!("图片 base64 解码失败：{e}"))?;
    let temp = cache_dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap().to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let written = std::fs::write(&temp, &bytes).and_then(|_| std::fs::rename(&temp, &path));
    if let Err(e) = written {
        std::fs::remove_file(&temp).ok();
        return Err(format!("写入图片缓存失败：{}: {e}", path.display()));
    }
    Ok(path)
}

pub fn enrich_images(
    call_id: &str,
    images: &mut [crate::agent::types::ToolImage],
    cache_dir: &Path,
) {
    for (index, image) in images.iter_mut().enumerate() {
        if image.path.is_some() {
            continue;
        }
        match save(cache_dir, call_id, index, &image.data, &image.mime) {
            Ok(path) => {
                image.path = Some(crate::tools::fs_write::display_path(&path));
            }
            Err(e) => tracing::warn!("工具图片缓存失败（界面将不显示路径）：{e}"),
        }
    }
}

pub fn enrich_event(event: &mut AgentEvent, cache_dir: &Path) {
    let AgentEvent::ToolResult {
        call_id, images, ..
    } = event
    else {
        return;
    };
    enrich_images(call_id, images, cache_dir);
}

fn sanitize_call_id(call_id: &str) -> String {
    let stem: String = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    if stem.is_empty() {
        "tool".to_string()
    } else {
        stem
    }
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::types::ToolImage;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn image(data: &str) -> ToolImage {
        ToolImage {
            data: data.into(),
            mime: ImageMime::Png,
            path: None,
        }
    }

    #[test]
    fn saves_and_reuses_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let first = save(dir.path(), "call-1", 0, &b64(b"hello"), &ImageMime::Png).unwrap();
        assert!(first.is_file());
        assert_eq!(first.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&first).unwrap(), b"hello");

        let second = save(dir.path(), "call-1", 0, &b64(b"hello"), &ImageMime::Png).unwrap();
        assert_eq!(first, second, "同内容必须复用同一个文件");
    }

    #[test]
    fn different_bytes_for_the_same_call_get_distinct_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = save(dir.path(), "call-1", 0, &b64(b"one"), &ImageMime::Png).unwrap();
        let b = save(dir.path(), "call-1", 0, &b64(b"two"), &ImageMime::Png).unwrap();
        assert_ne!(a, b, "call id 相同但内容不同，不能互相覆盖");
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        assert_eq!(std::fs::read(&b).unwrap(), b"two");
    }

    #[test]
    fn sanitizes_the_call_id_and_uses_the_jpeg_extension() {
        let dir = tempfile::tempdir().unwrap();
        let path = save(dir.path(), "a/b:c", 3, &b64(b"x"), &ImageMime::Jpeg).unwrap();
        let name = path.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("a_b_c-3-"), "{name}");
        assert!(name.ends_with(".jpeg"), "{name}");
        assert!(
            !path.to_string_lossy().contains("a/b"),
            "目录分隔符不许进文件名"
        );
    }

    #[test]
    fn rejects_invalid_base64() {
        let dir = tempfile::tempdir().unwrap();
        let err = save(dir.path(), "call-1", 0, "not base64!!", &ImageMime::Png).unwrap_err();
        assert!(err.contains("base64"), "{err}");
    }

    #[test]
    fn enrich_sets_paths_and_ignores_the_broken_images() {
        let dir = tempfile::tempdir().unwrap();
        let mut event = AgentEvent::ToolResult {
            call_id: "c1".into(),
            is_error: false,
            ms: 1,
            preview: "[图片 png]".into(),
            images: vec![image(&b64(b"ok")), image("bad!!")],
        };
        enrich_event(&mut event, dir.path());
        let AgentEvent::ToolResult { images, .. } = event else {
            panic!("必须是 tool_result");
        };
        let path = images[0].path.as_deref().expect("合法图片必须补路径");
        assert!(Path::new(path).is_file(), "{path}");
        assert_eq!(std::fs::read(path).unwrap(), b"ok");
        assert!(images[1].path.is_none(), "坏图只留 None，不许凭空造路径");
        assert_eq!(images[1].data, "bad!!", "base64 本体不许被改");
    }

    #[test]
    fn enrich_is_a_noop_for_non_tool_events() {
        let dir = tempfile::tempdir().unwrap();
        let mut event = AgentEvent::Usage {
            input_tokens: 1,
            output_tokens: 2,
        };
        enrich_event(&mut event, dir.path());
        assert!(matches!(event, AgentEvent::Usage { .. }));
    }
}
