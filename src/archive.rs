//! The archive: a plain directory tree anyone can browse without tools.
//!
//! A source tree is *replicated* into the archive under its own top-level
//! name. Point at `/home/tom/Pictures` and files land under `Pictures/`,
//! keeping the structure they had:
//!
//! ```text
//! /home/tom/Pictures/2023/hawaii/sunset.jpg   ->  Pictures/2023/hawaii/sunset.jpg
//! /mnt/jane/Pictures/2023/picnic.jpg          ->  Pictures/2023/picnic.jpg
//! /home/tom/Documents/scans/deed.jpg          ->  Documents/scans/deed.jpg
//! ```
//!
//! Two machines with a `Pictures` folder merge into one `Pictures/`, so a
//! shared `2023/` really is shared. A `Documents` folder stays separate even
//! though it holds `.jpg` scans — the folder a file lives in already records
//! what it is, far better than its extension could.
//!
//! Nothing here is content-addressed. Plug the drive into any machine and the
//! photos are just photos.

use crate::hash::{hash_file, to_hex};
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub const FORMAT_VERSION: u32 = 1;
const CONFIG_FILE: &str = "thylacine.conf";
const TMP_PREFIX: &str = ".thylacine-tmp-";

/// What happened to one file.
#[derive(Debug, PartialEq, Eq)]
pub enum Stored {
    /// Bytes were copied to this archive-relative path.
    Written(PathBuf),
    /// A file with identical content was already at this path.
    AlreadyThere(PathBuf),
}

impl Stored {
    pub fn path(&self) -> &Path {
        match self {
            Stored::Written(p) | Stored::AlreadyThere(p) => p,
        }
    }
}

#[derive(Debug)]
pub enum ArchiveError {
    Io(std::io::Error),
    NotAnArchive(PathBuf),
    AlreadyInitialised(PathBuf),
    MalformedConfig(String),
    UnsupportedVersion(u32),
    /// The source path was not beneath the source root it was scanned from.
    OutsideSourceRoot(PathBuf),
    /// Two distinct files sharing a full SHA-256 — has never happened.
    HashSuffixesExhausted(PathBuf),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchiveError::Io(_) => write!(f, "filesystem error"),
            ArchiveError::NotAnArchive(p) => write!(
                f,
                "{} is not a thylacine archive (run `init --dest` first)",
                p.display()
            ),
            ArchiveError::AlreadyInitialised(p) => {
                write!(f, "{} is already a thylacine archive", p.display())
            }
            ArchiveError::MalformedConfig(why) => write!(f, "malformed archive config: {why}"),
            ArchiveError::UnsupportedVersion(v) => write!(
                f,
                "archive format version {v} is newer than this build supports"
            ),
            ArchiveError::OutsideSourceRoot(p) => {
                write!(f, "{} is not beneath its source root", p.display())
            }
            ArchiveError::HashSuffixesExhausted(p) => {
                write!(f, "could not find a free name for {}", p.display())
            }
        }
    }
}

impl std::error::Error for ArchiveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ArchiveError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        ArchiveError::Io(e)
    }
}

pub struct Archive {
    root: PathBuf,
    id: String,
}

impl Archive {
    /// Mark a directory as an archive.
    ///
    /// The directory may already hold files — an archive assembled by hand is
    /// the expected starting point, not an edge case. `init` only writes the
    /// config; `Engine::reindex` is what learns about the existing contents.
    pub fn init(root: &Path) -> Result<Self, ArchiveError> {
        if root.join(CONFIG_FILE).exists() {
            return Err(ArchiveError::AlreadyInitialised(root.to_path_buf()));
        }

        fs::create_dir_all(root)?;
        let id = random_id()?;
        fs::write(
            root.join(CONFIG_FILE),
            format!("version = {FORMAT_VERSION}\nid = {id}\nhash = sha256\n"),
        )?;

        Ok(Archive {
            root: root.to_path_buf(),
            id,
        })
    }

    /// Refuses a directory that was never initialised, so a typo in `--dest`
    /// cannot quietly scatter 200 GB of photos somewhere unintended.
    pub fn open(root: &Path) -> Result<Self, ArchiveError> {
        let config = root.join(CONFIG_FILE);
        if !config.exists() {
            return Err(ArchiveError::NotAnArchive(root.to_path_buf()));
        }

        let text = fs::read_to_string(&config)?;
        let version: u32 = field(&text, "version")?
            .parse()
            .map_err(|_| ArchiveError::MalformedConfig("version is not a number".into()))?;
        if version > FORMAT_VERSION {
            return Err(ArchiveError::UnsupportedVersion(version));
        }

        Ok(Archive {
            root: root.to_path_buf(),
            id: field(&text, "id")?,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Where a source file wants to live, relative to the archive root.
    ///
    /// The source root's own name becomes the top-level directory, and
    /// everything below it is preserved verbatim. That is what makes two
    /// machines' `Pictures/2023/` folders merge while `Documents/` stays
    /// separate — no guessing from file extensions.
    pub fn target_path(&self, source_root: &Path, file: &Path) -> Result<PathBuf, ArchiveError> {
        let relative = file
            .strip_prefix(source_root)
            .map_err(|_| ArchiveError::OutsideSourceRoot(file.to_path_buf()))?;

        // Refuse `..` segments; a crafted path must not escape the archive.
        if relative
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(ArchiveError::OutsideSourceRoot(file.to_path_buf()));
        }

        // A root like `/` has no final component; fall back to the bare
        // relative path rather than inventing a name.
        match source_root.file_name() {
            Some(name) => Ok(PathBuf::from(name).join(relative)),
            None => Ok(relative.to_path_buf()),
        }
    }

    /// Copy a file into the archive, resolving name collisions.
    ///
    /// `hash` is the caller's already-computed digest of `source`; recomputing
    /// it here would mean reading every file twice.
    pub fn store(
        &self,
        source: &Path,
        source_root: &Path,
        hash: &[u8; 32],
    ) -> Result<Stored, ArchiveError> {
        let wanted = self.target_path(source_root, source)?;
        // One stat of the source, so collision checks can rule out
        // differently-sized files without reading them.
        let size = fs::metadata(source)?.len();

        match self.resolve_collision(&wanted, hash, size)? {
            Resolution::Occupied(path) => Ok(Stored::AlreadyThere(path)),
            Resolution::Free(path) => {
                self.copy_into_place(source, &path)?;
                Ok(Stored::Written(path))
            }
        }
    }

    /// Retrieves the relative path of every file currently in the archive directory.
    ///
    /// This is how an archive which has been prepopulated with files, or has had those files reorganized, 
    /// is indexed. 
    pub fn get_archived_filepaths(&self) -> impl Iterator<Item = Result<PathBuf, crate::scan::ScanError>> + '_ {
        let root = self.root.clone();
        crate::scan::walk(&self.root).filter_map(move |result| match result {
            Ok(entry) => {
                let name = entry.path.file_name()?.to_string_lossy().into_owned();
                if name.starts_with(TMP_PREFIX) || name == CONFIG_FILE {
                    return None;
                }
                Some(Ok(entry.path.strip_prefix(&root).ok()?.to_path_buf()))
            }
            Err(e) => Some(Err(e)),
        })
    }

    /// True if the archive holds at least one file, ignoring its own config.
    pub fn has_contents(&self) -> bool {
        self.get_archived_filepaths().next().is_some()
    }

    /// Clean up temp files left behind by an interrupted run.
    pub fn sweep_temp_files(&self) -> Result<usize, ArchiveError> {
        let mut removed = 0;
        for result in crate::scan::walk(&self.root) {
            let entry = match result {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            let is_temp = entry
                .path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with(TMP_PREFIX))
                .unwrap_or(false);
            if is_temp && fs::remove_file(&entry.path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Find a free name, or report that the wanted name already holds exactly
    /// this content.
    ///
    /// Disambiguation uses a prefix of the content hash rather than a counter,
    /// so the answer is the same on every run. A counter would produce
    /// `-1`, `-2`, `-3` depending on the order files happened to be scanned.
    fn resolve_collision(
        &self,
        wanted: &Path,
        hash: &[u8; 32],
        size: u64,
    ) -> Result<Resolution, ArchiveError> {
        let absolute = self.root.join(wanted);
        if !absolute.exists() {
            return Ok(Resolution::Free(wanted.to_path_buf()));
        }
        if same_content(&absolute, hash, size)? {
            return Ok(Resolution::Occupied(wanted.to_path_buf()));
        }

        // Same name, different bytes. Widen the hash prefix until the name is
        // either free or holds our content. Since the suffix is derived from
        // the content, this terminates.
        let hex = to_hex(hash);
        for width in [8usize, 16, 64] {
            let candidate = with_suffix(wanted, &hex[..width]);
            let absolute = self.root.join(&candidate);

            if !absolute.exists() {
                return Ok(Resolution::Free(candidate));
            }
            if same_content(&absolute, hash, size)? {
                return Ok(Resolution::Occupied(candidate));
            }
        }

        Err(ArchiveError::HashSuffixesExhausted(wanted.to_path_buf()))
    }

    /// Write to a temp name, flush, then rename.
    ///
    /// A crash mid-copy leaves a `.thylacine-tmp-*` file rather than a
    /// truncated photo sitting under a name that claims to be complete.
    fn copy_into_place(&self, source: &Path, target: &Path) -> Result<(), ArchiveError> {
        let absolute = self.root.join(target);
        let parent = absolute
            .parent()
            .ok_or_else(|| ArchiveError::MalformedConfig("target has no parent".into()))?;
        fs::create_dir_all(parent)?;

        let temp = parent.join(format!("{TMP_PREFIX}{}", std::process::id()));

        {
            let mut input = fs::File::open(source)?;
            let mut output = fs::File::create(&temp)?;
            std::io::copy(&mut input, &mut output)?;
            // Force the bytes out before the rename makes them visible.
            output.sync_all()?;
        }

        fs::rename(&temp, &absolute)?;

        // Durability of the rename itself needs the directory synced too.
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }

        Ok(())
    }
}

enum Resolution {
    Free(PathBuf),
    Occupied(PathBuf),
}

/// `IMG_1234.jpg` + `a19d4c7e` -> `IMG_1234-a19d4c7e.jpg`
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let parent = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();

    let name = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{stem}-{suffix}.{ext}"),
        None => format!("{stem}-{suffix}"),
    };

    parent.join(name)
}

/// Is the file at `path` byte-identical to content with this hash and size?
///
/// The size comparison is a cheap pre-filter: two files of different lengths
/// cannot hold the same bytes, so a mismatch settles it without opening
/// anything. Only when sizes agree is it worth reading the file to hash it.
fn same_content(path: &Path, hash: &[u8; 32], expected_size: u64) -> Result<bool, ArchiveError> {
    if fs::metadata(path)?.len() != expected_size {
        return Ok(false);
    }
    Ok(&hash_file(path)? == hash)
}

fn field(text: &str, key: &str) -> Result<String, ArchiveError> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return Ok(v.trim().to_string());
            }
        }
    }
    Err(ArchiveError::MalformedConfig(format!("missing `{key}`")))
}

fn random_id() -> Result<String, ArchiveError> {
    let mut bytes = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(to_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::hash_bytes;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("thyl-arch-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, contents: &[u8]) -> [u8; 32] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
        hash_bytes(contents)
    }

    #[test]
    fn init_leaves_existing_contents_alone() {
        let dir = temp("init-populated");
        fs::create_dir_all(dir.join("Pictures/2019")).unwrap();
        fs::write(dir.join("Pictures/2019/old.jpg"), b"already here").unwrap();

        let archive = Archive::init(&dir).unwrap();

        assert_eq!(archive.id().len(), 64);
        assert_eq!(
            fs::read(dir.join("Pictures/2019/old.jpg")).unwrap(),
            b"already here"
        );
        assert!(archive.has_contents());
    }

    #[test]
    fn a_fresh_archive_has_no_contents() {
        let archive = Archive::init(&temp("empty")).unwrap();
        assert!(!archive.has_contents(), "config file must not count");
    }

    #[test]
    fn open_refuses_a_plain_directory() {
        let dir = temp("plain");
        assert!(matches!(
            Archive::open(&dir),
            Err(ArchiveError::NotAnArchive(_))
        ));
    }

    #[test]
    fn open_recovers_the_id() {
        let dir = temp("reopen");
        let id = Archive::init(&dir).unwrap().id().to_string();
        assert_eq!(Archive::open(&dir).unwrap().id(), id);
    }

    #[test]
    fn the_source_tree_is_replicated_under_its_own_name() {
        let archive = Archive::init(&temp("replicate")).unwrap();

        let target = archive
            .target_path(
                Path::new("/home/tom/Pictures"),
                Path::new("/home/tom/Pictures/2023/hawaii/sunset.jpg"),
            )
            .unwrap();

        assert_eq!(target, PathBuf::from("Pictures/2023/hawaii/sunset.jpg"));
    }

    #[test]
    fn matching_folder_names_on_two_machines_merge() {
        let archive = Archive::init(&temp("merge")).unwrap();

        let mine = archive
            .target_path(
                Path::new("/home/tom/Pictures"),
                Path::new("/home/tom/Pictures/2023/beach.jpg"),
            )
            .unwrap();
        let hers = archive
            .target_path(
                Path::new("/mnt/jane-laptop/Pictures"),
                Path::new("/mnt/jane-laptop/Pictures/2023/picnic.jpg"),
            )
            .unwrap();

        assert_eq!(mine.parent().unwrap(), Path::new("Pictures/2023"));
        assert_eq!(hers.parent().unwrap(), Path::new("Pictures/2023"));
    }

    #[test]
    fn a_scanned_document_stays_with_documents() {
        let archive = Archive::init(&temp("scans")).unwrap();

        // The old extension-based rule would have filed this under pics/.
        let scan = archive
            .target_path(
                Path::new("/home/tom/Documents"),
                Path::new("/home/tom/Documents/deeds/house-deed.jpg"),
            )
            .unwrap();
        let photo = archive
            .target_path(
                Path::new("/home/tom/Pictures"),
                Path::new("/home/tom/Pictures/2023/house.jpg"),
            )
            .unwrap();

        assert_eq!(scan, PathBuf::from("Documents/deeds/house-deed.jpg"));
        assert_eq!(photo, PathBuf::from("Pictures/2023/house.jpg"));
    }

    #[test]
    fn trailing_slashes_do_not_change_the_target() {
        let archive = Archive::init(&temp("slash")).unwrap();

        let with = archive
            .target_path(
                Path::new("/home/tom/Pictures/"),
                Path::new("/home/tom/Pictures/a.jpg"),
            )
            .unwrap();
        let without = archive
            .target_path(
                Path::new("/home/tom/Pictures"),
                Path::new("/home/tom/Pictures/a.jpg"),
            )
            .unwrap();

        assert_eq!(with, without);
        assert_eq!(with, PathBuf::from("Pictures/a.jpg"));
    }

    #[test]
    fn parent_dir_components_are_rejected() {
        let archive = Archive::init(&temp("escape")).unwrap();
        let result =
            archive.target_path(Path::new("/src"), Path::new("/src/../../../etc/passwd"));
        assert!(matches!(result, Err(ArchiveError::OutsideSourceRoot(_))));
    }

    #[test]
    fn stores_a_file_preserving_its_structure() {
        let src = temp("store-src").join("Pictures");
        let archive = Archive::init(&temp("store-dst")).unwrap();
        let file = src.join("2023/sunset.jpg");
        let hash = write(&file, b"sunset bytes");

        let stored = archive.store(&file, &src, &hash).unwrap();

        assert_eq!(
            stored,
            Stored::Written(PathBuf::from("Pictures/2023/sunset.jpg"))
        );
        assert_eq!(
            fs::read(archive.root().join("Pictures/2023/sunset.jpg")).unwrap(),
            b"sunset bytes"
        );
    }

    #[test]
    fn storing_the_same_file_twice_is_a_no_op() {
        let src = temp("idem-src").join("Pictures");
        let archive = Archive::init(&temp("idem-dst")).unwrap();
        let path = src.join("a.jpg");
        let hash = write(&path, b"bytes");

        assert!(matches!(
            archive.store(&path, &src, &hash).unwrap(),
            Stored::Written(_)
        ));
        assert!(matches!(
            archive.store(&path, &src, &hash).unwrap(),
            Stored::AlreadyThere(_)
        ));
    }

    #[test]
    fn colliding_names_with_different_content_are_disambiguated() {
        let a_root = temp("coll-a").join("Pictures");
        let b_root = temp("coll-b").join("Pictures");
        let archive = Archive::init(&temp("coll-dst")).unwrap();

        let a = a_root.join("2023/IMG_1234.jpg");
        let b = b_root.join("2023/IMG_1234.jpg");
        let ha = write(&a, b"tom's photo");
        let hb = write(&b, b"jane's different photo");

        let first = archive.store(&a, &a_root, &ha).unwrap();
        let second = archive.store(&b, &b_root, &hb).unwrap();

        assert_eq!(first.path(), Path::new("Pictures/2023/IMG_1234.jpg"));
        assert_ne!(second.path(), first.path());

        let name = second.path().file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("IMG_1234-"), "got {name}");
        assert!(name.ends_with(".jpg"), "got {name}");

        assert_eq!(
            fs::read(archive.root().join(first.path())).unwrap(),
            b"tom's photo"
        );
        assert_eq!(
            fs::read(archive.root().join(second.path())).unwrap(),
            b"jane's different photo"
        );
    }

    #[test]
    fn disambiguation_is_deterministic() {
        let a_root = temp("det-a").join("Pictures");
        let b_root = temp("det-b").join("Pictures");
        let a = a_root.join("x.jpg");
        let b = b_root.join("x.jpg");
        let ha = write(&a, b"first");
        let hb = write(&b, b"second");

        let mut names = Vec::new();
        for run in 0..2 {
            let archive = Archive::init(&temp(&format!("det-dst-{run}"))).unwrap();
            archive.store(&a, &a_root, &ha).unwrap();
            names.push(archive.store(&b, &b_root, &hb).unwrap().path().to_path_buf());
        }
        assert_eq!(names[0], names[1], "same inputs must give the same name");
    }

    #[test]
    fn differently_sized_files_are_distinguished_without_hashing() {
        let dir = temp("size-check");
        let path = dir.join("a.jpg");
        fs::write(&path, b"some bytes").unwrap();
        let hash = hash_bytes(b"some bytes");

        assert!(same_content(&path, &hash, 10).unwrap());
        assert!(!same_content(&path, &hash, 999).unwrap());
    }

    #[test]
    fn suffix_goes_before_the_extension() {
        assert_eq!(
            with_suffix(Path::new("Pictures/2023/IMG_1234.jpg"), "a19d4c7e"),
            PathBuf::from("Pictures/2023/IMG_1234-a19d4c7e.jpg")
        );
        assert_eq!(
            with_suffix(Path::new("Documents/README"), "a19d4c7e"),
            PathBuf::from("Documents/README-a19d4c7e")
        );
    }

    #[test]
    fn no_temp_files_are_left_behind() {
        let src = temp("tmp-src").join("Pictures");
        let archive = Archive::init(&temp("tmp-dst")).unwrap();
        let path = src.join("a.jpg");
        let hash = write(&path, b"bytes");
        archive.store(&path, &src, &hash).unwrap();

        let leftovers: Vec<_> = crate::scan::walk(archive.root())
            .filter_map(Result::ok)
            .filter(|e| {
                e.path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(TMP_PREFIX)
            })
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn entries_lists_archived_files_without_the_config() {
        let src = temp("list-src").join("Pictures");
        let archive = Archive::init(&temp("list-dst")).unwrap();
        let path = src.join("2023/a.jpg");
        let hash = write(&path, b"bytes");
        archive.store(&path, &src, &hash).unwrap();

        let found: Vec<PathBuf> = archive.get_archived_filepaths().filter_map(Result::ok).collect();
        assert_eq!(found, vec![PathBuf::from("Pictures/2023/a.jpg")]);
    }

    #[test]
    fn sweep_removes_interrupted_writes() {
        let archive = Archive::init(&temp("sweep")).unwrap();
        let stray = archive.root().join(format!("{TMP_PREFIX}9999"));
        fs::write(&stray, b"half a photo").unwrap();

        assert_eq!(archive.sweep_temp_files().unwrap(), 1);
        assert!(!stray.exists());
    }
}