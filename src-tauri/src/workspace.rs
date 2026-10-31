//! Resolve the per-session output directory: workspace first, default storage otherwise.
use std::path::{Path, PathBuf};

pub fn resolve_storage_root(
    configured: &str,
    data_dir: &Path,
    documents: Option<&Path>,
) -> Result<PathBuf, String> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let root = PathBuf::from(configured);
        if !root.is_absolute() {
            return Err("默认储存位置必须是绝对路径".to_string());
        }
        return prepare_dir(root);
    }
    let primary = documents.unwrap_or(data_dir).join("LFTTDO");
    match prepare_dir(primary) {
        Ok(root) => Ok(root),
        Err(primary_error) if documents.is_some() => prepare_dir(data_dir.join("LFTTDO"))
            .map_err(|fallback| format!("{primary_error}；备用位置也失败：{fallback}")),
        Err(error) => Err(error),
    }
}

fn prepare_dir(root: PathBuf) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("创建默认储存位置失败：{}: {e}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("默认储存位置不是目录：{}", root.display()));
    }
    root.canonicalize()
        .map_err(|e| format!("默认储存位置无法解析：{}: {e}", root.display()))
}

pub fn session_output_dir(
    workspace: Option<&str>,
    storage_root: &Path,
    session_id: &str,
) -> Result<PathBuf, String> {
    if let Some(raw) = workspace {
        let path = PathBuf::from(raw.trim());
        if !path.is_absolute() {
            return Err("工作文件夹必须是绝对路径".to_string());
        }
        let canonical = path
            .canonicalize()
            .map_err(|e| format!("工作文件夹无法解析：{}: {e}", path.display()))?;
        if !canonical.is_dir() {
            return Err(format!("工作文件夹不是目录：{}", canonical.display()));
        }
        return Ok(canonical);
    }
    if session_id.is_empty()
        || session_id.contains(['/', '\\'])
        || session_id.contains("..")
        || session_id == "."
        || Path::new(session_id).is_absolute()
    {
        return Err("会话 id 非法".to_string());
    }
    let storage = storage_root
        .canonicalize()
        .map_err(|e| format!("默认储存位置无法解析：{}: {e}", storage_root.display()))?;
    let dir = storage.join(session_id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("创建会话输出目录失败：{}: {e}", dir.display()))?;
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("会话输出目录无法解析：{}: {e}", dir.display()))?;
    if !dir.starts_with(&storage) {
        return Err(format!("会话输出目录越界：{}", dir.display()));
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_storage_wins_and_gets_created() {
        let d = tempfile::tempdir().unwrap();
        let configured = d.path().join("my-storage");
        let root = resolve_storage_root(
            configured.to_str().unwrap(),
            d.path(),
            Some(&d.path().join("Documents")),
        )
        .unwrap();
        assert!(root.is_dir());
        assert_eq!(root, configured.canonicalize().unwrap());
    }

    #[test]
    fn empty_storage_uses_documents_lfttdo() {
        let d = tempfile::tempdir().unwrap();
        let docs = d.path().join("Documents");
        let root = resolve_storage_root("", d.path(), Some(&docs)).unwrap();
        assert_eq!(root, docs.join("LFTTDO").canonicalize().unwrap());
        assert!(root.is_dir());
    }

    #[test]
    fn empty_storage_falls_back_to_data_dir_when_documents_is_unknown() {
        let d = tempfile::tempdir().unwrap();
        let root = resolve_storage_root("", d.path(), None).unwrap();
        assert_eq!(root, d.path().join("LFTTDO").canonicalize().unwrap());
    }

    #[test]
    fn relative_storage_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let err = resolve_storage_root("relative/storage", d.path(), None).unwrap_err();
        assert!(err.contains("绝对路径"), "{err}");
    }

    #[test]
    fn workspace_overrides_default_and_must_exist() {
        let d = tempfile::tempdir().unwrap();
        let storage = d.path().join("storage");
        std::fs::create_dir(&storage).unwrap();
        let workspace = d.path().join("work");
        std::fs::create_dir(&workspace).unwrap();

        let out =
            session_output_dir(Some(workspace.to_str().unwrap()), &storage, "session-1").unwrap();
        assert_eq!(out, workspace.canonicalize().unwrap());

        let missing = d.path().join("gone");
        let err = session_output_dir(Some(missing.to_str().unwrap()), &storage, "s").unwrap_err();
        assert!(err.contains("无法解析"), "{err}");
    }

    #[test]
    fn default_output_is_a_per_session_subdirectory() {
        let d = tempfile::tempdir().unwrap();
        let storage = d.path().join("storage");
        std::fs::create_dir(&storage).unwrap();
        let a = session_output_dir(None, &storage, "session-a").unwrap();
        let b = session_output_dir(None, &storage, "session-b").unwrap();
        assert_eq!(a, storage.join("session-a").canonicalize().unwrap());
        assert_eq!(b, storage.join("session-b").canonicalize().unwrap());
        assert_ne!(a, b, "不同会话默认目录必须隔离");
        assert!(a.is_dir() && b.is_dir());
    }

    #[test]
    fn a_hostile_session_id_cannot_escape_the_storage_root() {
        let d = tempfile::tempdir().unwrap();
        let storage = d.path().join("storage");
        std::fs::create_dir(&storage).unwrap();
        for bad in ["", r"C:\Windows", "a/b", r"a\b", "..", "a..b", "."] {
            let err = session_output_dir(None, &storage, bad).expect_err("恶意 id 必须拒绝");
            assert!(!err.is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn relative_workspace_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        let storage = d.path().join("storage");
        std::fs::create_dir(&storage).unwrap();
        let err = session_output_dir(Some("work"), &storage, "s").unwrap_err();
        assert!(err.contains("绝对路径"), "{err}");
    }
}
