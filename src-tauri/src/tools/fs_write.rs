//! Shared helpers for write tools: UTF-8/EOL checks and atomic replacement.
use crate::tools::registry::ToolError;
use std::io::Write;
use std::path::Path;

pub(crate) const MAX_EXISTING_BYTES: u64 = 4 * 1024 * 1024;

pub(crate) fn existing_text(path: &Path) -> Result<Option<String>, ToolError> {
    let meta = match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ToolError::Failed(format!(
                "无法读取目标状态: {}: {e}",
                path.display()
            )));
        }
        Ok(m) => m,
    };
    if meta.is_dir() {
        return Err(ToolError::Failed(format!(
            "目标是目录，不是文件: {}",
            path.display()
        )));
    }
    if !meta.file_type().is_file() {
        return Err(ToolError::Failed(format!(
            "目标不是普通文件: {}",
            path.display()
        )));
    }
    if meta.len() > MAX_EXISTING_BYTES {
        return Err(ToolError::Failed(format!(
            "目标文件超过 {} 字节上限，拒绝整文件读写: {}",
            MAX_EXISTING_BYTES,
            path.display()
        )));
    }
    let bytes = std::fs::read(path)
        .map_err(|e| ToolError::Failed(format!("读取目标失败: {}: {e}", path.display())))?;
    if bytes.contains(&0) {
        return Err(ToolError::Failed(format!(
            "目标疑似二进制（含 NUL 字节），拒绝写入: {}",
            path.display()
        )));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| {
        ToolError::Failed(format!(
            "目标不是 UTF-8 文本（可能是 GBK 等编码），拒绝写入且不静默转码: {}",
            path.display()
        ))
    })
}

pub(crate) fn display_path(path: &Path) -> String {
    let shown = path.display().to_string();
    if let Some(rest) = shown.strip_prefix("\\\\?\\UNC\\") {
        return format!("\\\\{rest}");
    }
    shown.strip_prefix("\\\\?\\").unwrap_or(&shown).to_string()
}

pub(crate) fn is_crlf(text: &str) -> bool {
    text.contains("\r\n")
}

pub(crate) fn has_mixed_eol(text: &str) -> bool {
    let bytes = text.as_bytes();
    let (mut saw_crlf, mut saw_lf) = (false, false);
    for (i, b) in bytes.iter().enumerate() {
        if *b != b'\n' {
            continue;
        }
        if i > 0 && bytes[i - 1] == b'\r' {
            saw_crlf = true;
        } else {
            saw_lf = true;
        }
        if saw_crlf && saw_lf {
            return true;
        }
    }
    false
}

pub(crate) fn to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

pub(crate) fn to_crlf(text: &str) -> String {
    to_lf(text).replace('\n', "\r\n")
}

pub(crate) fn atomic_write(path: &Path, text: &str) -> Result<(), ToolError> {
    let dir = path
        .parent()
        .ok_or_else(|| ToolError::Failed(format!("目标没有父目录: {}", path.display())))?;
    let temp = dir.join(format!(".lfttdo-write-{}.tmp", uuid::Uuid::new_v4()));
    let write_result = (|| -> std::io::Result<()> {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    if let Err(e) = write_result {
        std::fs::remove_file(&temp).ok();
        return Err(ToolError::Failed(format!(
            "写入临时文件失败: {}: {e}",
            path.display()
        )));
    }
    if let Err(e) = std::fs::rename(&temp, path) {
        std::fs::remove_file(&temp).ok();
        return Err(ToolError::Failed(format!(
            "替换目标文件失败: {}: {e}",
            path.display()
        )));
    }
    Ok(())
}
