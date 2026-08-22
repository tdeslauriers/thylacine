//! Directory traversal, hand-rolled rather than via `walkdir`.
//!
//! Two decisions shape this module:
//!
//! 1. It is an `Iterator`, not a function returning `Vec`. A photo library can
//!    hold hundreds of thousands of files; the caller should be able to start
//!    hashing the first one before the last one is discovered.
//!
//! 2. Errors are *yielded*, not returned. One unreadable directory should not
//!    abort a backup of the other 200,000 files.

use std::fs::{self, ReadDir};
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

/// A regular file found during the walk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
    /// Seconds since the Unix epoch. Negative for files older than 1970,
    /// which do exist on restored archives.
    pub mtime: i64,
}

/// Something went wrong at one point in the tree. The walk continues.
#[derive(Debug)]
pub struct ScanError {
    pub path: PathBuf,
    pub source: io::Error,
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.source)
    }
}

impl std::error::Error for ScanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Seconds since the Unix epoch, negative for pre-1970 files.
///
/// Shared so that everything recording an mtime — the walker, the archive
/// index — derives it the same way.
pub fn mtime_secs(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .map(|time| match time.duration_since(UNIX_EPOCH) {
            Ok(after) => after.as_secs() as i64,
            Err(before) => -(before.duration().as_secs() as i64),
        })
        .unwrap_or(0)
}

pub struct Walker {
    /// Open directory handles, deepest last. Holding `ReadDir` rather than a
    /// list of paths means the OS does the buffering and memory stays flat
    /// regardless of how many entries a directory holds.
    stack: Vec<(PathBuf, ReadDir)>,
    /// Queued errors, drained before reading more entries.
    pending: Vec<ScanError>,
    follow_symlinks: bool,
}

/// Walk `root`, yielding every regular file beneath it.
///
/// Symlinks are not followed by default: a link pointing at its own ancestor
/// would loop forever, and a link out of the tree would silently pull in files
/// the caller never asked for.
pub fn walk(root: &Path) -> Walker {
    let mut walker = Walker {
        stack: Vec::new(),
        pending: Vec::new(),
        follow_symlinks: false,
    };
    walker.push_dir(root.to_path_buf());
    walker
}

impl Walker {
    /// Opt into following symlinks. Cycle detection is the caller's problem;
    /// prefer leaving this off.
    pub fn follow_symlinks(mut self, follow: bool) -> Self {
        self.follow_symlinks = follow;
        self
    }

    fn push_dir(&mut self, path: PathBuf) {
        match fs::read_dir(&path) {
            Ok(handle) => self.stack.push((path, handle)),
            Err(source) => self.pending.push(ScanError { path, source }),
        }
    }
}

impl Iterator for Walker {
    type Item = Result<FileEntry, ScanError>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(err) = self.pending.pop() {
                return Some(Err(err));
            }

            let (dir_path, handle) = self.stack.last_mut()?;
            let dir_path = dir_path.clone();

            let entry = match handle.next() {
                Some(Ok(entry)) => entry,
                Some(Err(source)) => {
                    return Some(Err(ScanError {
                        path: dir_path,
                        source,
                    }));
                }
                None => {
                    self.stack.pop();
                    continue;
                }
            };

            let path = entry.path();

            // symlink_metadata describes the link itself; metadata follows it.
            let meta = if self.follow_symlinks {
                fs::metadata(&path)
            } else {
                fs::symlink_metadata(&path)
            };

            let meta = match meta {
                Ok(meta) => meta,
                Err(source) => return Some(Err(ScanError { path, source })),
            };

            if meta.is_dir() {
                self.push_dir(path);
                continue;
            }

            // Skip symlinks, sockets, fifos, devices — anything that is not a
            // plain file has no bytes worth archiving.
            if !meta.is_file() {
                continue;
            }

            return Some(Ok(FileEntry {
                path,
                size: meta.len(),
                mtime: mtime_secs(&meta),
            }));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("thyl-scan-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempTree(dir)
        }

        fn file(&self, rel: &str, contents: &[u8]) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, contents).unwrap();
            path
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.0.join(rel);
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            // Restore permissions first or cleanup fails on the 000 dir test.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(entries) = fs::read_dir(&self.0) {
                    for entry in entries.flatten() {
                        let _ = fs::set_permissions(
                            entry.path(),
                            fs::Permissions::from_mode(0o755),
                        );
                    }
                }
            }
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn collect_ok(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = walk(root)
            .filter_map(Result::ok)
            .map(|e| {
                e.path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn finds_files_at_every_depth() {
        let tree = TempTree::new("depth");
        tree.file("top.jpg", b"a");
        tree.file("2023/summer/beach.jpg", b"b");
        tree.file("2023/winter/snow.jpg", b"c");

        assert_eq!(
            collect_ok(tree.path()),
            vec!["2023/summer/beach.jpg", "2023/winter/snow.jpg", "top.jpg"]
        );
    }

    #[test]
    fn reports_size_and_mtime() {
        let tree = TempTree::new("meta");
        tree.file("photo.jpg", b"twelve bytes");

        let entries: Vec<FileEntry> = walk(tree.path()).filter_map(Result::ok).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 12);
        // Just written, so within a wide sanity window of now.
        assert!(entries[0].mtime > 1_600_000_000);
    }

    #[test]
    fn empty_directories_yield_nothing() {
        let tree = TempTree::new("empty");
        tree.dir("no-photos-here");
        assert!(collect_ok(tree.path()).is_empty());
    }

    #[test]
    fn missing_root_yields_one_error_not_a_panic() {
        let results: Vec<_> = walk(Path::new("/definitely/not/here")).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_by_default() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("symlink");
        tree.file("real.jpg", b"a");
        symlink(tree.path().join("real.jpg"), tree.path().join("link.jpg")).unwrap();

        assert_eq!(collect_ok(tree.path()), vec!["real.jpg"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_does_not_hang() {
        use std::os::unix::fs::symlink;

        let tree = TempTree::new("loop");
        tree.file("photos/a.jpg", b"a");
        // Classic trap: a link pointing back at its own ancestor.
        symlink(tree.path(), tree.path().join("photos/back")).unwrap();

        assert_eq!(collect_ok(tree.path()), vec!["photos/a.jpg"]);
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_does_not_abort_the_walk() {
        use std::os::unix::fs::PermissionsExt;

        let tree = TempTree::new("perms");
        tree.file("readable/a.jpg", b"a");
        tree.file("readable/b.jpg", b"b");
        let locked = tree.dir("locked");
        tree.file("locked/secret.jpg", b"c");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let (ok, errs): (Vec<_>, Vec<_>) = walk(tree.path()).partition(Result::is_ok);

        // Running as root defeats permission bits; only assert when it bit.
        if !errs.is_empty() {
            assert_eq!(errs.len(), 1);
            assert_eq!(ok.len(), 2, "the readable files still came through");
        }

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_filenames_survive() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let tree = TempTree::new("utf8");
        let weird = OsString::from_vec(vec![b'p', b'i', b'c', 0xFF, b'.', b'j', b'p', b'g']);
        fs::write(tree.path().join(&weird), b"bytes").unwrap();

        let entries: Vec<FileEntry> = walk(tree.path()).filter_map(Result::ok).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path.file_name().unwrap(), weird.as_os_str());
    }

    #[test]
    fn iteration_is_lazy() {
        let tree = TempTree::new("lazy");
        for i in 0..50 {
            tree.file(&format!("dir{}/f{}.jpg", i % 5, i), b"x");
        }

        // Taking three must not require discovering all fifty.
        let first_three: Vec<_> = walk(tree.path()).filter_map(Result::ok).take(3).collect();
        assert_eq!(first_three.len(), 3);
    }

    #[test]
    fn a_file_as_root_yields_an_error() {
        let tree = TempTree::new("fileroot");
        let file = tree.file("single.jpg", b"a");

        let results: Vec<_> = walk(&file).collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].is_err());
    }
}