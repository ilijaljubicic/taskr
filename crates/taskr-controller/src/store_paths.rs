//! Controller-local paths for TASKR durable state and launch configuration.

use std::path::{Path, PathBuf};

pub(crate) const DEFAULT_STORE_DIR_NAME: &str = ".taskr";

pub(crate) fn default_store_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "HOME is not set; pass --store-path explicitly".to_owned())?;
    default_store_path_for_home(&home)
}

fn default_store_path_for_home(home: &Path) -> Result<PathBuf, String> {
    let path = home.join(DEFAULT_STORE_DIR_NAME);
    if !path.exists() && home.join(".mmux/mmux.db").is_file() {
        return Err("Existing legacy MMUX store found. Use an already converted Taskr store or pass --store-path explicitly; refusing to create an empty replacement.".into());
    }
    Ok(path)
}

pub(crate) fn resolve_store_path(path: Option<&Path>) -> Result<PathBuf, String> {
    match path {
        Some(path) => expand_tilde_path(path),
        None => default_store_path(),
    }
}

pub(crate) fn expand_tilde_path(path: &Path) -> Result<PathBuf, String> {
    let text = path.to_string_lossy();
    if text == "~" {
        return std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set; cannot expand '~'".to_owned());
    }
    if let Some(rest) = text.strip_prefix("~/") {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| "HOME is not set; cannot expand '~/'".to_owned())?;
        return Ok(home.join(rest));
    }
    Ok(path.to_path_buf())
}

pub(crate) fn ensure_store_dir(path: &Path) -> Result<(), String> {
    if path.join("mmux.db").exists() && !path.join("taskr.db").exists() {
        return Err(format!("Legacy MMUX database found in '{}'. Use an already converted Taskr store containing taskr.db or select a separate store directory; refusing to create an empty Taskr store.", path.display()));
    }
    std::fs::create_dir_all(path).map_err(|error| {
        format!(
            "failed to create store path '{}': {}",
            path.display(),
            error
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(
            |error| {
                format!(
                    "failed to set store path permissions '{}': {}",
                    path.display(),
                    error
                )
            },
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "{}-{}-{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ))
    }

    #[test]
    fn expands_tilde_paths_with_home() {
        std::env::set_var("HOME", "/home/test-user");
        assert_eq!(
            expand_tilde_path(Path::new("~")).unwrap(),
            PathBuf::from("/home/test-user")
        );
        assert_eq!(
            expand_tilde_path(Path::new("~/work/taskr")).unwrap(),
            PathBuf::from("/home/test-user/work/taskr")
        );
        assert_eq!(
            expand_tilde_path(Path::new("/abs/path")).unwrap(),
            PathBuf::from("/abs/path")
        );
    }

    #[test]
    fn resolve_store_path_defaults_to_home_dot_taskr() {
        std::env::set_var("HOME", "/home/test-user");
        assert_eq!(
            resolve_store_path(None).unwrap(),
            PathBuf::from("/home/test-user/.taskr")
        );
        assert_eq!(
            resolve_store_path(Some(Path::new("~/store"))).unwrap(),
            PathBuf::from("/home/test-user/store")
        );
    }

    #[test]
    fn ensure_store_dir_creates_private_directory() {
        let dir = unique_temp_path("taskr-store-paths-store");
        ensure_store_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_store_is_rejected_without_creating_empty_taskr_database() {
        let home = unique_temp_path("taskr-legacy-home");
        let legacy = home.join(".mmux");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("mmux.db"), b"existing store").unwrap();
        assert!(default_store_path_for_home(&home)
            .unwrap_err()
            .contains("already converted Taskr store"));
        assert!(ensure_store_dir(&legacy).unwrap_err().contains("refusing"));
        assert!(!legacy.join("taskr.db").exists());
        assert!(!home.join(".taskr").exists());
        std::fs::create_dir(home.join(".taskr")).unwrap();
        assert_eq!(
            default_store_path_for_home(&home).unwrap(),
            home.join(".taskr")
        );
        std::fs::remove_dir_all(home).unwrap();
    }
}
