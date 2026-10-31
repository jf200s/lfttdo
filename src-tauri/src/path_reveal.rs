//! Validate paths and reveal them in Explorer without invoking a shell.
use std::path::{Path, PathBuf};

pub fn validate_target(
    raw: &str,
    roots: &[PathBuf],
    allowed_dir: &Path,
) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("路径为空".to_string());
    }
    let candidate = Path::new(raw);
    if !candidate.is_absolute() {
        return Err("只接受绝对路径".to_string());
    }
    let target = candidate
        .canonicalize()
        .map_err(|_| "路径不存在或无法解析".to_string())?;
    let in_allowed = allowed_dir
        .canonicalize()
        .ok()
        .is_some_and(|dir| target.starts_with(dir));
    let in_root = roots.iter().any(|root| {
        root.canonicalize()
            .map(|root| target.starts_with(root))
            .unwrap_or(false)
    });
    if !in_allowed && !in_root {
        return Err("路径不在允许的目录内（roots 或当前会话输出目录）".to_string());
    }
    Ok(target)
}

#[cfg(windows)]
pub fn reveal(target: &Path) -> Result<(), String> {
    explorer_command(target)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开文件位置失败：{e}"))
}

#[cfg(not(windows))]
pub fn reveal(_target: &Path) -> Result<(), String> {
    Err("打开文件位置目前仅支持 Windows".to_string())
}

#[cfg(windows)]
fn explorer_command(target: &Path) -> std::process::Command {
    let mut cmd = std::process::Command::new("explorer");
    // validate_target returns a canonical path; Windows canonical paths carry the
    // `\\?\` extended-length prefix, which Explorer does not accept. Only the
    // display form goes into argv.
    let shown = crate::tools::fs_write::display_path(target);
    if target.is_dir() {
        cmd.arg(&shown);
    } else {
        cmd.arg(format!("/select,{shown}"));
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn allows_files_inside_roots_and_the_image_cache() {
        let (_guard, root) = temp_root();
        let file = root.join("notes.txt");
        std::fs::write(&file, "x").unwrap();
        let images = root.join("images");
        std::fs::create_dir(&images).unwrap();
        let cached = images.join("tool-0.png");
        std::fs::write(&cached, "x").unwrap();

        assert_eq!(
            validate_target(file.to_str().unwrap(), std::slice::from_ref(&root), &images).unwrap(),
            file.canonicalize().unwrap()
        );
        assert_eq!(
            validate_target(
                cached.to_str().unwrap(),
                std::slice::from_ref(&root),
                &images
            )
            .unwrap(),
            cached.canonicalize().unwrap()
        );
    }

    #[test]
    fn rejects_paths_outside_roots_and_cache() {
        let (_root_guard, root) = temp_root();
        let (_outside_guard, outside) = temp_root();
        let file = outside.join("secret.txt");
        std::fs::write(&file, "x").unwrap();
        let err =
            validate_target(file.to_str().unwrap(), &[root], &outside.join("images")).unwrap_err();
        assert!(err.contains("允许"), "{err}");
    }

    #[test]
    fn rejects_relative_missing_and_empty_paths() {
        let (_guard, root) = temp_root();
        for raw in ["", "   ", "notes.txt", "C:\\definitely-not-there-xyz"] {
            let err = validate_target(raw, std::slice::from_ref(&root), &root.join("images"))
                .expect_err("非法路径必须拒绝");
            assert!(!err.is_empty(), "{raw:?}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_junction_escaping_the_root_is_denied() {
        let (_dir, base) = temp_root();
        let root = base.join("root");
        std::fs::create_dir(&root).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "x").unwrap();
        let out = std::process::Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                &root.join("peek").display().to_string(),
                &outside.display().to_string(),
            ])
            .output()
            .unwrap();
        assert!(out.status.success(), "建 junction 失败");
        let target = root.join("peek").join("secret.txt");
        let err =
            validate_target(target.to_str().unwrap(), &[root], &base.join("images")).unwrap_err();
        assert!(err.contains("允许"), "junction 逃逸必须被拒：{err}");
    }

    #[cfg(windows)]
    #[test]
    fn explorer_selects_files_and_opens_directories() {
        let (_guard, root) = temp_root();
        let file = root.join("a b.txt");
        std::fs::write(&file, "x").unwrap();
        let file_cmd = explorer_command(&file.canonicalize().unwrap());
        assert!(file_cmd
            .get_program()
            .to_string_lossy()
            .ends_with("explorer"));
        let args: Vec<String> = file_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.len(), 1, "{args:?}");
        assert_eq!(
            args[0],
            format!(
                "/select,{}",
                crate::tools::fs_write::display_path(&file.canonicalize().unwrap())
            )
        );
        assert!(
            !args[0].contains(r"\\?\"),
            "不许把 verbatim 前缀交给 Explorer"
        );

        let dir_cmd = explorer_command(&root.canonicalize().unwrap());
        let dir_args: Vec<String> = dir_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(dir_args.len(), 1, "目录直接打开：{dir_args:?}");
        assert_eq!(
            PathBuf::from(&dir_args[0]),
            PathBuf::from(crate::tools::fs_write::display_path(
                &root.canonicalize().unwrap()
            ))
        );
    }
}
