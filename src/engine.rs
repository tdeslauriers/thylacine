use crate::archive::{Archive, Stored};
use crate::cache::{Cache, Entry};
use crate::hash::hash_file;
use crate::scan;
use std::error::Error;
use std::path::{Path, PathBuf};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Metadata matched the cache; the file was never opened.
    pub skipped_unchanged: u64,
    /// Hashed, but the content was already in the archive under some name.
    pub duplicates: u64,
    /// Bytes copied into the archive.
    pub copied: u64,
    pub bytes_copied: u64,
    pub errors: u64,
}

pub struct Engine {
    archive: Archive,
    cache: Cache,
    sources: Vec<PathBuf>,
}

impl Engine {
    pub fn new(archive: Archive, cache: Cache, sources: Vec<PathBuf>) -> Self {
        Engine {
            archive,
            cache,
            sources,
        }
    }

    pub fn run(&mut self) -> Result<Stats, Box<dyn Error>> {
        let swept = self.archive.sweep_temp_files()?;
        if swept > 0 {
            eprintln!("cleaned up {swept} temp file(s) from an interrupted run");
        }

        // An archive assembled by hand, or one whose cache was lost, holds
        // files the index knows nothing about. Copying into it without
        // learning them first would duplicate every one. Adopt them instead.
        if self.cache.archived_count()? == 0 && self.archive.has_contents() {
            eprintln!("indexing existing archive contents...");
            let found = self.reindex()?;
            eprintln!("  adopted {found} file(s) already in the archive");
        }

        let mut stats = Stats::default();

        for source_root in &self.sources {
            // One transaction per source root. Without it SQLite commits per
            // statement, paying a durability round-trip for every file.
            let tx = self.cache.transaction()?;

            for result in scan::walk(source_root) {
                match result {
                    Ok(entry) => {
                        match Self::handle(&self.archive, &self.cache, source_root, &entry) {
                            Ok(outcome) => stats.record(outcome, entry.size),
                            Err(err) => {
                                eprintln!("  {}: {err}", entry.path.display());
                                stats.errors += 1;
                            }
                        }
                    }
                    Err(err) => {
                        eprintln!("  {err}");
                        stats.errors += 1;
                    }
                }
            }

            tx.commit()?;
        }

        Ok(stats)
    }

    /// The per-file decision, in cost order: cheapest check first.
    fn handle(
        archive: &Archive,
        cache: &Cache,
        source_root: &Path,
        entry: &scan::FileEntry,
    ) -> Result<Outcome, Box<dyn Error>> {
        let key = entry.path.to_string_lossy().into_owned();
        let size = entry.size as i64;

        // 1. Unchanged since last run? Never open the file.
        if let Some(known) = cache.lookup_source(&key)? {
            if known.metadata_matches(size, entry.mtime) {
                return Ok(Outcome::Unchanged);
            }
        }

        // 2. Read it once, hash it once.
        let digest = hash_file(&entry.path)?;

        let record = Entry {
            path: key,
            size,
            mtime: entry.mtime,
            hash: digest.to_vec(),
        };

        // 3. Do we already hold these bytes, anywhere in the archive under any
        //    name? Update the source table and exit.
        if cache.find_by_hash(&digest)?.is_some() {
            cache.upsert_source(&record)?;
            return Ok(Outcome::Duplicate);
        }

        // 4. Copy it in, resolving any name collision.
        let stored = archive.store(&entry.path, source_root, &digest)?;
        let archived_path = stored.path().to_string_lossy().into_owned();

        // The archived row must describe the file *in the archive*, not the
        // source it came from. A plain copy does not inherit the source's
        // mtime, so reading it back is the only way to get this right — and
        // getting it wrong would make a future integrity check compare the
        // archive against a timestamp it never had.
        let archived_meta = std::fs::metadata(archive.root().join(stored.path()))?;

        cache.record_archived(&Entry {
            path: archived_path,
            size: archived_meta.len() as i64,
            mtime: scan::mtime_secs(&archived_meta),
            hash: digest.to_vec(),
        })?;
        cache.upsert_source(&record)?;

        Ok(match stored {
            Stored::Written(_) => Outcome::Copied,
            Stored::AlreadyThere(_) => Outcome::Duplicate,
        })
    }

    /// Rebuild the archive index by walking the archive and re-hashing.
    ///
    /// This is the recovery path after a lost cache, and also how files that
    /// were dragged into the archive by hand become known.
    pub fn reindex(&mut self) -> Result<u64, Box<dyn Error>> {
        let tx = self.cache.transaction()?;
        self.cache.clear_archived()?;

        let mut count = 0;
        let entries: Vec<PathBuf> = self.archive.entries().filter_map(Result::ok).collect();

        for relative in entries {
            let absolute = self.archive.root().join(&relative);
            let meta = std::fs::metadata(&absolute)?;
            let digest = hash_file(&absolute)?;

            self.cache.record_archived(&Entry {
                path: relative.to_string_lossy().into_owned(),
                size: meta.len() as i64,
                mtime: scan::mtime_secs(&meta),
                hash: digest.to_vec(),
            })?;
            count += 1;
        }

        tx.commit()?;
        Ok(count)
    }

    pub fn archive(&self) -> &Archive {
        &self.archive
    }

    #[cfg(test)]
    fn cache_for_test(&self) -> &Cache {
        &self.cache
    }
}

enum Outcome {
    Unchanged,
    Duplicate,
    Copied,
}

impl Stats {
    fn record(&mut self, outcome: Outcome, size: u64) {
        match outcome {
            Outcome::Unchanged => self.skipped_unchanged += 1,
            Outcome::Duplicate => self.duplicates += 1,
            Outcome::Copied => {
                self.copied += 1;
                self.bytes_copied += size;
            }
        }
    }

    pub fn total_seen(&self) -> u64 {
        self.skipped_unchanged + self.duplicates + self.copied
    }
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} files seen: {} copied ({} bytes), {} already archived, {} unchanged, {} errors",
            self.total_seen(),
            self.copied,
            self.bytes_copied,
            self.duplicates,
            self.skipped_unchanged,
            self.errors
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        _dir: PathBuf,
        source_a: PathBuf,
        source_b: PathBuf,
        dest: PathBuf,
    }

    fn fixture(name: &str) -> Fixture {
        let dir =
            std::env::temp_dir().join(format!("thyl-eng-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // Source roots are named so their final component is meaningful:
        // that name becomes the archive's top-level directory.
        let (source_a, source_b, dest) = (
            dir.join("machine-a").join("Pictures"),
            dir.join("machine-b").join("Pictures"),
            dir.join("archive"),
        );
        for p in [&source_a, &source_b, &dest] {
            fs::create_dir_all(p).unwrap();
        }
        Fixture {
            _dir: dir,
            source_a,
            source_b,
            dest,
        }
    }

    fn put(root: &Path, rel: &str, contents: &[u8]) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn engine(f: &Fixture, sources: Vec<PathBuf>) -> Engine {
        let archive = match Archive::open(&f.dest) {
            Ok(a) => a,
            Err(_) => Archive::init(&f.dest).unwrap(),
        };
        let cache = Cache::in_memory(archive.id()).unwrap();
        Engine::new(archive, cache, sources)
    }

    #[test]
    fn the_source_tree_is_replicated_verbatim() {
        let f = fixture("replicate");
        put(&f.source_a, "2023/sunset.jpg", b"photo");
        put(&f.source_a, "clips/trip.mp4", b"video");
        put(&f.source_a, "scans/deed.jpg", b"a scanned document");
        put(&f.source_a, "misc/notes.xyz", b"unknown");

        let stats = engine(&f, vec![f.source_a.clone()]).run().unwrap();

        assert_eq!(stats.copied, 4);
        assert!(f.dest.join("Pictures/2023/sunset.jpg").exists());
        assert!(f.dest.join("Pictures/clips/trip.mp4").exists());
        // The scan keeps its folder; no extension rule moves it elsewhere.
        assert!(f.dest.join("Pictures/scans/deed.jpg").exists());
        assert!(f.dest.join("Pictures/misc/notes.xyz").exists());
    }

    #[test]
    fn different_source_folders_stay_separate() {
        let f = fixture("separate");
        let docs = f._dir.join("machine-a").join("Documents");
        put(&f.source_a, "2023/house.jpg", b"a photo of the house");
        put(&docs, "deeds/house.jpg", b"a scan of the deed");

        let stats = engine(&f, vec![f.source_a.clone(), docs.clone()])
            .run()
            .unwrap();

        assert_eq!(stats.copied, 2);
        assert!(f.dest.join("Pictures/2023/house.jpg").exists());
        assert!(f.dest.join("Documents/deeds/house.jpg").exists());
    }

    #[test]
    fn an_existing_archive_is_adopted_not_duplicated() {
        let f = fixture("adopt");
        put(&f.source_a, "2023/sunset.jpg", b"sunset bytes");

        // The archive already holds this photo, filed by hand somewhere else.
        let archive = Archive::init(&f.dest).unwrap();
        let existing = f.dest.join("Pictures/hawaii-trip/sunset.jpg");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, b"sunset bytes").unwrap();

        let cache = Cache::in_memory(archive.id()).unwrap();
        let stats = Engine::new(archive, cache, vec![f.source_a.clone()])
            .run()
            .unwrap();

        assert_eq!(stats.copied, 0, "already present, under a hand-made name");
        assert_eq!(stats.duplicates, 1);
        assert!(!f.dest.join("Pictures/2023/sunset.jpg").exists());
    }

    #[test]
    fn two_machines_merge_into_one_year_directory() {
        let f = fixture("merge");
        put(&f.source_a, "2023/beach.jpg", b"tom at the beach");
        put(&f.source_b, "2023/picnic.jpg", b"jane at the picnic");

        let stats = engine(&f, vec![f.source_a.clone(), f.source_b.clone()])
            .run()
            .unwrap();

        assert_eq!(stats.copied, 2);
        assert!(f.dest.join("Pictures/2023/beach.jpg").exists());
        assert!(f.dest.join("Pictures/2023/picnic.jpg").exists());
    }

    #[test]
    fn identical_content_on_two_machines_is_stored_once() {
        let f = fixture("dedup");
        put(&f.source_a, "2023/shared.jpg", b"the same photo");
        // Different name, different directory, same bytes.
        put(&f.source_b, "backup/copy-of-shared.jpg", b"the same photo");

        let stats = engine(&f, vec![f.source_a.clone(), f.source_b.clone()])
            .run()
            .unwrap();

        assert_eq!(stats.copied, 1);
        assert_eq!(stats.duplicates, 1);
        assert!(!f.dest.join("Pictures/backup/copy-of-shared.jpg").exists());
    }

    #[test]
    fn same_name_different_photos_both_survive() {
        let f = fixture("collide");
        put(&f.source_a, "2023/IMG_1234.jpg", b"tom's photo");
        put(&f.source_b, "2023/IMG_1234.jpg", b"jane's photo");

        let stats = engine(&f, vec![f.source_a.clone(), f.source_b.clone()])
            .run()
            .unwrap();

        assert_eq!(stats.copied, 2);

        let names: Vec<String> = fs::read_dir(f.dest.join("Pictures/2023"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "both photos kept: {names:?}");
    }

    #[test]
    fn a_second_run_copies_nothing() {
        let f = fixture("rerun");
        put(&f.source_a, "2023/a.jpg", b"photo one");
        put(&f.source_a, "2023/b.jpg", b"photo two");

        let archive = Archive::init(&f.dest).unwrap();
        let cache = Cache::in_memory(archive.id()).unwrap();
        let mut eng = Engine::new(archive, cache, vec![f.source_a.clone()]);

        let first = eng.run().unwrap();
        assert_eq!(first.copied, 2);

        let second = eng.run().unwrap();
        assert_eq!(second.copied, 0);
        assert_eq!(second.skipped_unchanged, 2, "metadata cache should hit");
    }

    #[test]
    fn a_changed_document_is_archived_alongside_the_old_one() {
        let f = fixture("changed");
        put(&f.source_a, "budget.xlsx", b"version one");

        let archive = Archive::init(&f.dest).unwrap();
        let cache = Cache::in_memory(archive.id()).unwrap();
        let mut eng = Engine::new(archive, cache, vec![f.source_a.clone()]);
        eng.run().unwrap();

        // Edit it. New bytes, new mtime.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        put(&f.source_a, "budget.xlsx", b"version two, edited");

        let stats = eng.run().unwrap();
        assert_eq!(stats.copied, 1, "the edit should be archived");

        let names: Vec<String> = fs::read_dir(f.dest.join("Pictures"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "both versions kept: {names:?}");
    }

    #[test]
    fn a_moved_source_file_is_not_recopied() {
        let f = fixture("moved");
        put(&f.source_a, "2023/vacation.jpg", b"beach photo");

        let archive = Archive::init(&f.dest).unwrap();
        let cache = Cache::in_memory(archive.id()).unwrap();
        let mut eng = Engine::new(archive, cache, vec![f.source_a.clone()]);
        eng.run().unwrap();

        // User reorganises: same photo, new folder and name.
        fs::remove_file(f.source_a.join("2023/vacation.jpg")).unwrap();
        put(&f.source_a, "2023-hawaii/day-one.jpg", b"beach photo");

        let stats = eng.run().unwrap();
        assert_eq!(stats.copied, 0);
        assert_eq!(stats.duplicates, 1, "recognised by hash, not path");
    }

    #[test]
    fn reindex_rebuilds_the_archive_index_from_disk() {
        let f = fixture("reindex");
        put(&f.source_a, "2023/a.jpg", b"photo");

        let archive = Archive::init(&f.dest).unwrap();
        let cache = Cache::in_memory(archive.id()).unwrap();
        let mut eng = Engine::new(archive, cache, vec![f.source_a.clone()]);
        eng.run().unwrap();

        // Simulate losing the cache entirely.
        let archive = Archive::open(&f.dest).unwrap();
        let fresh = Cache::in_memory(archive.id()).unwrap();
        let mut eng = Engine::new(archive, fresh, vec![f.source_a.clone()]);

        assert_eq!(eng.reindex().unwrap(), 1);

        // With the index rebuilt, the source file is recognised as a duplicate
        // rather than copied a second time.
        let stats = eng.run().unwrap();
        assert_eq!(stats.copied, 0);
        assert_eq!(stats.duplicates, 1);
    }

    #[test]
    fn archived_rows_describe_the_archived_file_not_the_source() {
        let f = fixture("archived-meta");
        put(&f.source_a, "2023/a.jpg", b"photo");

        // Backdate the source so its mtime cannot coincide with the copy's.
        let old = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(1_000_000_000);
        let src_file = fs::File::options()
            .write(true)
            .open(f.source_a.join("2023/a.jpg"))
            .unwrap();
        src_file.set_modified(old).unwrap();
        drop(src_file);

        let archive = Archive::init(&f.dest).unwrap();
        let cache = Cache::in_memory(archive.id()).unwrap();
        let mut eng = Engine::new(archive, cache, vec![f.source_a.clone()]);
        eng.run().unwrap();

        let on_disk = fs::metadata(f.dest.join("Pictures/2023/a.jpg")).unwrap();
        let recorded = eng.cache_for_test().archived_entry("Pictures/2023/a.jpg").unwrap().unwrap();

        assert_eq!(recorded.mtime, scan::mtime_secs(&on_disk));
        assert_ne!(recorded.mtime, 1_000_000_000, "source mtime must not leak in");
    }

    #[test]
    fn reindex_records_the_same_metadata_as_a_backup() {
        let f = fixture("reindex-agrees");
        put(&f.source_a, "2023/a.jpg", b"photo");

        let archive = Archive::init(&f.dest).unwrap();
        let cache = Cache::in_memory(archive.id()).unwrap();
        let mut eng = Engine::new(archive, cache, vec![f.source_a.clone()]);
        eng.run().unwrap();

        let from_backup = eng.cache_for_test().archived_entry("Pictures/2023/a.jpg").unwrap().unwrap();
        eng.reindex().unwrap();
        let from_reindex = eng.cache_for_test().archived_entry("Pictures/2023/a.jpg").unwrap().unwrap();

        assert_eq!(from_backup.size, from_reindex.size);
        assert_eq!(from_backup.mtime, from_reindex.mtime);
        assert_eq!(from_backup.hash, from_reindex.hash);
    }

    #[test]
    fn an_unreadable_file_does_not_stop_the_run() {
        let f = fixture("errors");
        put(&f.source_a, "good.jpg", b"fine");
        put(&f.source_a, "bad.jpg", b"also fine");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                f.source_a.join("bad.jpg"),
                fs::Permissions::from_mode(0o000),
            )
            .unwrap();
        }

        let stats = engine(&f, vec![f.source_a.clone()]).run().unwrap();

        // Root ignores permission bits, so only assert when it actually bit.
        if stats.errors > 0 {
            assert_eq!(stats.copied, 1, "the readable file still got through");
        } else {
            assert_eq!(stats.copied, 2);
        }
    }
}