//! Non-destructive archive directory initialization and per-user fallback.

use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

fn prepare(root: &Path) -> io::Result<()> {
    // create_dir_all accepts existing directories without replacing their contents.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root)?;
    for entry in std::fs::read_dir(root)? {
        entry?;
    }
    for set in ["full", "dms"] {
        match std::fs::read_dir(root.join(set)) {
            Ok(entries) => {
                for entry in entries {
                    let path = entry?.path();
                    if path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .starts_with('.')
                    {
                        continue;
                    }
                    if !std::fs::metadata(&path)?.is_dir() {
                        continue;
                    }
                    // Opening existing databases read-only detects permission failures
                    // without creating, truncating, or changing any archive.
                    match std::fs::File::open(path.join("slackdump.sqlite")) {
                        Ok(_) => (),
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                        Err(error) => return Err(error),
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn select_root(requested: &Path, fallback: &Path) -> Result<PathBuf, String> {
    match prepare(requested) {
        Ok(()) => Ok(requested.to_path_buf()),
        Err(error) => {
            if requested == fallback {
                return Err(format!("cannot use {}: {error}", requested.display()));
            }
            prepare(fallback).map_err(|fallback_error| {
                format!(
                    "cannot use {}: {error}; fallback {}: {fallback_error}",
                    requested.display(),
                    fallback.display()
                )
            })?;
            eprintln!(
                "slack-tui: cannot use {}: {error}; using {}",
                requested.display(),
                fallback.display()
            );
            Ok(fallback.to_path_buf())
        }
    }
}

pub fn resolve_root(requested: &Path) -> Result<PathBuf, String> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        });
    match cache {
        Some(cache) => select_root(requested, &cache.join("slackdumps")),
        None => prepare(requested)
            .map(|()| requested.to_path_buf())
            .map_err(|error| {
                format!(
                    "cannot use {}: {error}; set HOME or XDG_CACHE_HOME for a fallback",
                    requested.display()
                )
            }),
    }
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
    fn missing_root_starts_with_empty_corpus_and_preserves_existing_files() {
        let temp = Scratch::new();
        let root = temp.0.join("nested/archives");
        let fallback = temp.0.join("fallback");
        assert_eq!(select_root(&root, &fallback).unwrap(), root);
        let sentinel = root.join("keep");
        std::fs::write(&sentinel, b"preserve me").unwrap();
        std::fs::create_dir(root.join("full")).unwrap();
        std::fs::write(root.join("full/README"), b"archive notes").unwrap();
        assert_eq!(select_root(&root, &fallback).unwrap(), root);
        assert_eq!(std::fs::read(sentinel).unwrap(), b"preserve me");
        let corpus = crate::archive::Corpus::open(&root, 30.0, None).unwrap();
        assert!(corpus.convs.is_empty());
        assert_eq!(corpus.root, root);
        assert!(!fallback.exists());
    }

    #[test]
    fn occupied_path_falls_back_without_overwriting_either_location() {
        let temp = Scratch::new();
        let root = temp.0.join("occupied");
        let fallback = temp.0.join("fallback");
        std::fs::write(&root, b"original").unwrap();
        assert_eq!(select_root(&root, &fallback).unwrap(), fallback);
        std::fs::write(fallback.join("keep"), b"fallback data").unwrap();
        assert_eq!(select_root(&root, &fallback).unwrap(), fallback);
        assert_eq!(std::fs::read(root).unwrap(), b"original");
        assert_eq!(
            std::fs::read(fallback.join("keep")).unwrap(),
            b"fallback data"
        );
    }

    #[test]
    fn unreadable_archive_falls_back_without_changing_permissions() {
        let temp = Scratch::new();
        let root = temp.0.join("archives");
        let archive = root.join("full/test");
        let fallback = temp.0.join("fallback");
        std::fs::create_dir_all(&archive).unwrap();
        let db = archive.join("slackdump.sqlite");
        std::fs::write(&db, b"existing database").unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0)).unwrap();
        // Root can bypass mode bits; the permission regression needs an ordinary user.
        if std::fs::File::open(&db).is_err() {
            assert_eq!(select_root(&root, &fallback).unwrap(), fallback);
            assert_eq!(
                std::fs::metadata(&db).unwrap().permissions().mode() & 0o777,
                0
            );
        }
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(std::fs::read(db).unwrap(), b"existing database");
    }

    #[test]
    fn unreadable_directory_falls_back_without_replacing_it() {
        let temp = Scratch::new();
        let root = temp.0.join("archives");
        let fallback = temp.0.join("fallback");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("keep"), b"original").unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0)).unwrap();
        if std::fs::read_dir(&root).is_err() {
            assert_eq!(select_root(&root, &fallback).unwrap(), fallback);
            assert_eq!(
                std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
                0
            );
        }
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(std::fs::read(root.join("keep")).unwrap(), b"original");
    }
}
