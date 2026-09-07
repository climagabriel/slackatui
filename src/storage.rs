//! Check the requested archive root without creating or changing storage.

use std::io;
use std::path::{Path, PathBuf};

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn failure(path: &Path, directory: bool, error: io::Error) -> String {
    let target = quote(path);
    let mut message = format!("cannot use {}: {error}", path.display());
    if !matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
    ) {
        message
            .push_str("\nInspect this path or select another archive directory with --root DIR.");
        return message;
    }
    message.push_str("\nRun these commands to grant your user access, then retry:");
    if directory {
        message.push_str(&format!("\n  sudo mkdir --parents -- {target}"));
    }
    message.push_str(&format!(
        "\n  sudo chown -- \"$(id --user):$(id --group)\" {target}\n  sudo chmod -- {} {target}",
        if directory { "u+rwx" } else { "u+rw" }
    ));
    message
}

pub fn resolve_root(requested: &Path) -> Result<PathBuf, String> {
    let root = std::path::absolute(requested).map_err(|error| error.to_string())?;
    for entry in std::fs::read_dir(&root).map_err(|error| failure(&root, true, error))? {
        entry.map_err(|error| failure(&root, true, error))?;
    }
    for set in ["full", "dms"] {
        let directory = root.join(set);
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(failure(&directory, true, error)),
        };
        for entry in entries {
            let path = entry
                .map_err(|error| failure(&directory, true, error))?
                .path();
            if path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .starts_with('.')
            {
                continue;
            }
            if !std::fs::metadata(&path)
                .map_err(|error| failure(&path, true, error))?
                .is_dir()
            {
                continue;
            }
            let database = path.join("slackdump.sqlite");
            match std::fs::File::open(&database) {
                Ok(_) => (),
                Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                Err(error) => return Err(failure(&database, false, error)),
            }
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "slack-tui-storage-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn missing_root_prints_commands_without_creating_it() {
        let temp = Scratch::new();
        let root = temp.0.join("missing/archives");
        let message = resolve_root(&root).unwrap_err();
        assert!(message.contains(&format!("sudo mkdir --parents -- {}", quote(&root))));
        assert!(message.contains("sudo chown -- \"$(id --user):$(id --group)\""));
        assert!(message.contains("sudo chmod -- u+rwx"));
        assert!(!temp.0.join("missing").exists());
    }

    #[test]
    fn readable_root_preserves_contents_and_supports_empty_corpus() {
        let temp = Scratch::new();
        std::fs::write(temp.0.join("keep"), b"original").unwrap();
        assert_eq!(resolve_root(&temp.0).unwrap(), temp.0);
        assert!(crate::archive::Corpus::open(&temp.0, 30.0, None)
            .unwrap()
            .convs
            .is_empty());
        assert_eq!(std::fs::read(temp.0.join("keep")).unwrap(), b"original");
    }

    #[test]
    fn occupied_path_is_not_replaced() {
        let temp = Scratch::new();
        let root = temp.0.join("file");
        std::fs::write(&root, b"original").unwrap();
        let message = resolve_root(&root).unwrap_err();
        assert!(!message.contains("sudo mkdir"));
        assert_eq!(std::fs::read(root).unwrap(), b"original");
    }

    #[test]
    fn unreadable_database_names_exact_target_without_changing_it() {
        let temp = Scratch::new();
        let archive = temp.0.join("full/test");
        std::fs::create_dir_all(&archive).unwrap();
        let db = archive.join("slackdump.sqlite");
        std::fs::write(&db, b"original").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0)).unwrap();
        if std::fs::File::open(&db).is_err() {
            let message = resolve_root(&temp.0).unwrap_err();
            assert!(message.contains(&format!("sudo chmod -- u+rw {}", quote(&db))));
            assert!(!message.contains("sudo mkdir"));
            assert_eq!(
                std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
                0
            );
        }
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(db).unwrap(), b"original");
    }

    #[test]
    fn unreadable_directory_exits_without_changing_permissions() {
        let temp = Scratch::new();
        std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0)).unwrap();
        if std::fs::read_dir(&temp.0).is_err() {
            let message = resolve_root(&temp.0).unwrap_err();
            assert!(message.contains(&format!("sudo chmod -- u+rwx {}", quote(&temp.0))));
            assert_eq!(
                std::fs::metadata(&temp.0).unwrap().permissions().mode() & 0o777,
                0
            );
        }
        std::fs::set_permissions(&temp.0, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn command_paths_are_shell_quoted() {
        assert_eq!(
            quote(Path::new("/tmp/a'b $(touch x)")),
            "'/tmp/a'\\''b $(touch x)'"
        );
    }
}
