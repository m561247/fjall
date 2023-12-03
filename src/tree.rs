use crate::{
    block_cache::BlockCache,
    compaction::{worker::start_compaction_thread, CompactionStrategy},
    entry::{OccupiedEntry, VacantEntry},
    id::generate_segment_id,
    journal::{shard::JournalShard, Journal},
    levels::Levels,
    memtable::MemTable,
    prefix::Prefix,
    range::{MemTableGuard, Range},
    segment::{self, meta::Metadata, Segment},
    tree_inner::TreeInner,
    value::SeqNo,
    Batch, Config, Value,
};
use std::{
    collections::HashMap,
    ops::RangeBounds,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, AtomicU64},
        Arc, RwLock, RwLockWriteGuard,
    },
};
use std_semaphore::Semaphore;

pub struct CompareAndSwapError {
    /// The value currently in the tree that caused the CAS error
    pub prev: Option<Vec<u8>>,

    /// The value that was proposed
    pub next: Option<Vec<u8>>,
}

pub type CompareAndSwapResult = Result<(), CompareAndSwapError>;

/// A log-structured merge tree (LSM-tree/LSMT)
///
/// The tree is internally synchronized (Send + Sync), so it does not need to be wrapped in a lock nor an Arc.
///
/// To share the tree between threads, use `Arc::clone(&tree)` or `tree.clone()`.
#[doc(alias = "keyspace")]
#[doc(alias = "table")]
#[derive(Clone)]
pub struct Tree(Arc<TreeInner>);

impl std::ops::Deref for Tree {
    type Target = Arc<TreeInner>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn ignore_tombstone_value(item: Value) -> Option<Value> {
    if item.is_tombstone {
        None
    } else {
        Some(item)
    }
}

impl Tree {
    /// Opens the tree at the given folder.
    ///
    /// Will create a new tree if the folder is not in use
    /// or recover a previous state if it exists.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Tree::open(Config::new(folder))?;
    /// // Same as
    /// # let folder = tempfile::tempdir()?;
    /// let tree = Config::new(folder).open()?;
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn open(config: Config) -> crate::Result<Self> {
        if config.path.join(".lsm").exists() {
            Self::recover(config)
        } else {
            Self::create_new(config)
        }
    }

    /// Gets the given key’s corresponding entry in the map for in-place manipulation.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Tree::open(Config::new(folder))?;
    ///
    /// let value = tree.entry("a")?.or_insert("abc")?;
    /// assert_eq!("abc".as_bytes(), &value);
    ///
    /// let value = tree.entry("a")?.or_insert("def")?;
    /// assert_eq!("abc".as_bytes(), &value);
    ///
    /// let value = tree.entry("a")?.and_update(|_| "def")?.or_insert("abc")?;
    /// assert_eq!("def".as_bytes(), &value);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn entry<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<crate::entry::Entry> {
        let key = key.as_ref();
        let item = self.get_internal_entry(key, true)?;

        Ok(match item {
            Some(item) => crate::entry::Entry::Occupied(OccupiedEntry {
                tree: self.clone(),
                key: key.to_vec(),
                value: item.value,
            }),
            None => crate::entry::Entry::Vacant(VacantEntry {
                tree: self.clone(),
                key: key.to_vec(),
            }),
        })
    }

    /// Initializes a new, atomic write batch.
    ///
    /// Call [`Batch::commit`] to commit the batch to the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// let mut batch = tree.batch();
    /// batch.insert("a", "hello");
    /// batch.insert("b", "hello2");
    /// batch.insert("c", "hello3");
    /// batch.remove("idontlikeu");
    ///
    /// batch.commit()?;
    ///
    /// assert_eq!(3, tree.len()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    #[must_use]
    pub fn batch(&self) -> Batch {
        Batch::new(self.clone())
    }

    /// Returns `true` if there are some segments that are being compacted.
    #[doc(hidden)]
    #[must_use]
    pub fn is_compacting(&self) -> bool {
        let levels = self.levels.read().expect("lock is poisoned");
        levels.is_compacting()
    }

    /// Counts the amount of segments currently in the tree.
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.levels.read().expect("lock is poisoned").len()
    }
    /// Sums the disk space usage of the tree (segments + journals).
    #[must_use]
    pub fn disk_space(&self) -> u64 {
        let segment_size = self
            .levels
            .read()
            .expect("lock is poisoned")
            .get_all_segments()
            .values()
            .map(|x| x.metadata.file_size)
            .sum::<u64>();

        let memtable_size = u64::from(
            self.active_journal_size_bytes
                .load(std::sync::atomic::Ordering::Relaxed),
        );

        segment_size + memtable_size
    }

    /// Returns the folder path used by the tree.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        self.config.path.clone()
    }

    /// Scans the entire Tree, returning the amount of items.
    ///
    /// # Examples
    ///
    /// ```
    /// # use lsm_tree::Error as TreeError;
    /// # use lsm_tree::{Tree, Config};
    /// #
    /// # let folder = tempfile::tempdir()?;
    /// # let mut tree = Config::new(folder).open()?;
    /// #
    /// assert_eq!(tree.len()?, 0);
    /// tree.insert("1", "abc")?;
    /// tree.insert("3", "abc")?;
    /// tree.insert("5", "abc")?;
    /// assert_eq!(tree.len()?, 3);
    ///
    /// # Ok::<(), TreeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    #[deprecated(
        note = "len() isn't deprecated per se, however it performs a full tree scan and should be avoided"
    )]
    pub fn len(&self) -> crate::Result<usize> {
        Ok(self.iter()?.into_iter().filter(Result::is_ok).count())
    }

    /// Returns `true` if the tree is empty.
    ///
    /// This operation has O(1) complexity.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// assert!(tree.is_empty()?);
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// assert!(!tree.is_empty()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn is_empty(&self) -> crate::Result<bool> {
        self.first_key_value().map(|x| x.is_none())
    }

    /// Creates a new tree in a folder.
    ///
    /// # Errors
    ///
    /// - Will return `Err` if an IO error occurs
    /// - Will fail, if the folder already occupied
    fn create_new(config: Config) -> crate::Result<Self> {
        // Setup folders
        std::fs::create_dir_all(&config.path)?;
        std::fs::create_dir_all(config.path.join("segments"))?;
        std::fs::create_dir_all(config.path.join("journals"))?;

        let marker = config.path.join(".lsm");
        assert!(!marker.try_exists()?);

        // fsync .lsm marker
        let file = std::fs::File::create(marker)?;
        file.sync_all()?;

        let first_journal_path = config.path.join("journals").join(generate_segment_id());
        let levels = Levels::create_new(config.levels, config.path.join("levels.json"))?;

        let block_cache = Arc::new(BlockCache::new(config.block_cache_capacity as usize));

        let flush_threads = config.flush_threads.into();

        let inner = TreeInner {
            config,
            journal: Journal::create_new(first_journal_path)?,
            active_memtable: Arc::new(RwLock::new(MemTable::default())),
            immutable_memtables: Arc::default(),
            block_cache,
            lsn: AtomicU64::new(0),
            levels: Arc::new(RwLock::new(levels)),
            flush_semaphore: Arc::new(Semaphore::new(flush_threads)),
            compaction_semaphore: Arc::new(Semaphore::new(4)), // TODO: config
            active_journal_size_bytes: AtomicU32::default(),
        };

        // fsync folder
        let folder = std::fs::File::open(&inner.config.path)?;
        folder.sync_all()?;

        // fsync folder
        let folder = std::fs::File::open(inner.config.path.join("journals"))?;
        folder.sync_all()?;

        Ok(Self(Arc::new(inner)))
    }

    fn recover_segments<P: AsRef<Path>>(
        folder: &P,
        block_cache: &Arc<BlockCache>,
    ) -> crate::Result<HashMap<String, Arc<Segment>>> {
        let folder = folder.as_ref();

        // NOTE: First we load the level manifest without any
        // segments just to get the IDs
        // Then we recover the segments and build the actual level manifest
        let levels = Levels::recover(&folder.join("levels.json"), HashMap::new())?;
        let segment_ids_to_recover = levels.list_ids();

        let mut segments = HashMap::new();

        for dirent in std::fs::read_dir(folder.join("segments"))? {
            let dirent = dirent?;
            let path = dirent.path();

            assert!(path.is_dir());

            let segment_id = dirent
                .file_name()
                .to_str()
                .expect("invalid segment folder name")
                .to_owned();
            log::debug!("Recovering segment from {}", path.display());

            if segment_ids_to_recover.contains(&segment_id) {
                let segment = Segment::recover(&path, Arc::clone(block_cache))?;
                segments.insert(segment.metadata.id.clone(), Arc::new(segment));
                log::debug!("Recovered segment from {}", path.display());
            } else {
                log::info!("Deleting unfinished segment: {}", path.to_string_lossy());
                std::fs::remove_dir_all(path)?;
            }
        }

        if segments.len() < segment_ids_to_recover.len() {
            log::error!("Expected segments : {segment_ids_to_recover:?}");
            log::error!(
                "Recovered segments: {:?}",
                segments.keys().collect::<Vec<_>>()
            );

            panic!("Some segments were not recovered")
        }

        Ok(segments)
    }

    fn recover_active_journal(config: &Config) -> crate::Result<Option<(Journal, MemTable)>> {
        // Load previous levels manifest
        // Add all flushed segments to it, then recover properly
        let mut levels = Levels::recover(&config.path.join("levels.json"), HashMap::new())?;

        let mut active_journal = None;

        for dirent in std::fs::read_dir(config.path.join("journals"))? {
            let dirent = dirent?;
            let journal_path = dirent.path();

            assert!(journal_path.is_dir());

            // TODO: replace fs extra with Journal::disk_space
            let journal_size = fs_extra::dir::get_size(&journal_path)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "fs_extra error"))?;

            if journal_size == 0 {
                std::fs::remove_dir_all(&journal_path)?;
                continue;
            }

            if !journal_path.join(".flush").exists() {
                // TODO: handle this
                assert!(active_journal.is_none(), "Second active journal found :(");

                if journal_size < config.max_memtable_size.into() {
                    log::info!("Setting {} as active journal", journal_path.display());

                    let (recovered_journal, memtable) = Journal::recover(journal_path.clone())?;
                    active_journal = Some((recovered_journal, memtable));

                    continue;
                }

                log::info!(
                    "Flushing active journal because it is too large: {}",
                    dirent.path().to_string_lossy()
                );
                // Journal is too large to be continued to be used
                // Just flush it
            }

            log::info!(
                "Flushing orphaned journal {} to segment",
                dirent.path().to_string_lossy()
            );

            // TODO: optimize this

            let (recovered_journal, memtable) = Journal::recover(journal_path.clone())?;
            log::trace!("Recovered old journal");
            drop(recovered_journal);

            let segment_id = dirent
                .file_name()
                .to_str()
                .expect("invalid journal folder name")
                .to_string();
            let segment_folder = config.path.join("segments").join(&segment_id);

            if !levels.contains_id(&segment_id) {
                // The level manifest does not contain the segment
                // If the segment is maybe half written, clean it up here
                // and then write it
                if segment_folder.exists() {
                    std::fs::remove_dir_all(&segment_folder)?;
                }

                let mut segment_writer = segment::writer::Writer::new(segment::writer::Options {
                    path: segment_folder.clone(),
                    evict_tombstones: false,
                    block_size: config.block_size,
                })?;

                for (key, value) in memtable.items {
                    segment_writer.write(Value::from((key, value)))?;
                }

                segment_writer.finish()?;

                if segment_writer.item_count > 0 {
                    let metadata = Metadata::from_writer(segment_id, segment_writer);
                    metadata.write_to_file()?;

                    log::info!("Written segment from orphaned journal: {:?}", metadata.id);

                    levels.add_id(metadata.id);
                    levels.write_to_disk()?;
                }
            }

            std::fs::remove_dir_all(journal_path)?;
        }

        Ok(active_journal)
    }

    /// Tries to recover a tree from a folder.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    fn recover(config: Config) -> crate::Result<Self> {
        log::info!("Recovering tree from {}", config.path.display());

        let start = std::time::Instant::now();

        log::info!("Restoring journal");
        let active_journal = Self::recover_active_journal(&config)?;

        log::info!("Restoring memtable");

        let (journal, memtable) = if let Some(active_journal) = active_journal {
            active_journal
        } else {
            let next_journal_path = config.path.join("journals").join(generate_segment_id());
            (Journal::create_new(next_journal_path)?, MemTable::default())
        };

        // TODO: optimize this... do on journal load...
        let lsn = memtable
            .items
            .iter()
            .map(|x| {
                let key = x.key();
                key.seqno
            })
            .max()
            .unwrap_or(0);

        // Load segments
        log::info!("Restoring segments");

        let block_cache = Arc::new(BlockCache::new(config.block_cache_capacity as usize));
        let segments = Self::recover_segments(&config.path, &block_cache)?;

        // Check if a segment has a higher seqno and then take it
        let lsn = lsn.max(
            segments
                .values()
                .map(|x| x.metadata.seqnos.1)
                .max()
                .unwrap_or(0),
        );

        // Finalize Tree
        log::debug!("Loading level manifest");

        let mut levels = Levels::recover(&config.path.join("levels.json"), segments)?;
        levels.sort_levels();

        let compaction_threads = 4; // TODO: config
        let flush_threads = config.flush_threads.into();

        let inner = TreeInner {
            config,
            journal,
            active_memtable: Arc::new(RwLock::new(memtable)),
            immutable_memtables: Arc::default(),
            block_cache,
            lsn: AtomicU64::new(lsn),
            levels: Arc::new(RwLock::new(levels)),
            flush_semaphore: Arc::new(Semaphore::new(flush_threads)),
            compaction_semaphore: Arc::new(Semaphore::new(compaction_threads)),
            active_journal_size_bytes: AtomicU32::default(),
        };

        let tree = Self(Arc::new(inner));

        log::debug!("Starting {compaction_threads} compaction threads");
        for _ in 0..compaction_threads {
            start_compaction_thread(&tree);
        }

        log::info!("Tree loaded in {}s", start.elapsed().as_secs_f32());

        Ok(tree)
    }

    fn append_entry(
        &self,
        mut shard: RwLockWriteGuard<'_, JournalShard>,
        value: Value,
    ) -> crate::Result<()> {
        let size = shard.write(&value)?;
        drop(shard);

        let memtable_lock = self.active_memtable.read().expect("lock is poisoned");
        memtable_lock.insert(value);

        let memtable_size = self
            .active_journal_size_bytes
            .fetch_add(size as u32, std::sync::atomic::Ordering::Relaxed);

        drop(memtable_lock);

        if memtable_size > self.config.max_memtable_size {
            log::debug!("Memtable reached threshold size");
            crate::flush::start(self)?;
        }

        Ok(())
    }

    /// Inserts a key-value pair into the tree.
    ///
    /// If the key already exists, the item will be overwritten.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("a", nanoid::nanoid!())?;
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn insert<K: Into<Vec<u8>>, V: Into<Vec<u8>>>(
        &self,
        key: K,
        value: V,
    ) -> crate::Result<()> {
        let shard = self.journal.lock_shard();

        let value = Value::new(
            key,
            value,
            false,
            self.lsn.fetch_add(1, std::sync::atomic::Ordering::AcqRel),
        );

        self.append_entry(shard, value)?;

        Ok(())
    }

    /// Deletes an item from the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("a", "abc")?;
    ///
    /// let item = tree.get("a")?.expect("should have item");
    /// assert_eq!("abc".as_bytes(), item);
    ///
    /// tree.remove("a")?;
    ///
    /// let item = tree.get("a")?;
    /// assert_eq!(None, item);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn remove<K: Into<Vec<u8>>>(&self, key: K) -> crate::Result<()> {
        let shard = self.journal.lock_shard();

        let value = Value::new(
            key,
            vec![],
            true,
            self.lsn.fetch_add(1, std::sync::atomic::Ordering::AcqRel),
        );

        self.append_entry(shard, value)?;

        Ok(())
    }

    /// Removes the item and returns its value if it was previously in the tree.
    ///
    /// This is less efficient than just deleting because it needs to do a read before deleting.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// let item = tree.remove_entry("a")?;
    /// assert_eq!(None, item);
    ///
    /// tree.insert("a", "abc")?;
    ///
    /// let item = tree.remove_entry("a")?.expect("should have removed item");
    /// assert_eq!("abc".as_bytes(), item);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn remove_entry<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<Option<Vec<u8>>> {
        let key = key.as_ref();

        Ok(if let Some(item) = self.get(key)? {
            self.remove(key)?;
            Some(item)
        } else {
            None
        })
    }

    /// Returns `true` if the tree contains the specified key.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// assert!(!tree.contains_key("a")?);
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// assert!(tree.contains_key("a")?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn contains_key<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<bool> {
        self.get(key).map(|x| x.is_some())
    }

    #[allow(clippy::iter_not_returning_iterator)]
    /// Returns an iterator that scans through the entire Tree.
    ///
    /// Avoid using this function, or limit it as otherwise it may scan a lot of items.
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn iter(&self) -> crate::Result<Range<'_>> {
        self.range::<Vec<u8>, _>(..)
    }

    /// Returns an iterator over a range of items.
    ///
    /// Avoid using full or unbounded ranges as they may scan a lot of items (unless limited).
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// assert_eq!(1, tree.range("a"..="z")?.into_iter().count());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn range<K: AsRef<[u8]>, R: RangeBounds<K>>(&self, range: R) -> crate::Result<Range<'_>> {
        use std::ops::Bound::{self, Excluded, Included, Unbounded};

        let lo: Bound<Vec<u8>> = match range.start_bound() {
            Included(x) => Included(x.as_ref().into()),
            Excluded(x) => Excluded(x.as_ref().into()),
            Unbounded => Unbounded,
        };

        let hi: Bound<Vec<u8>> = match range.end_bound() {
            Included(x) => Included(x.as_ref().into()),
            Excluded(x) => Excluded(x.as_ref().into()),
            Unbounded => Unbounded,
        };

        let bounds: (Bound<Vec<u8>>, Bound<Vec<u8>>) = (lo, hi);

        let lock = self.levels.read().expect("lock is poisoned");

        let segment_info = lock
            .get_all_segments()
            .values()
            .filter(|x| x.check_key_range_overlap(&bounds))
            .cloned()
            .collect::<Vec<_>>();

        Ok(Range::new(
            crate::range::MemTableGuard {
                active: self.active_memtable.read().expect("lock is poisoned"),
                immutable: self.immutable_memtables.read().expect("lock is poisoned"),
            },
            bounds,
            segment_info,
        ))
    }

    /// Returns an iterator over a prefixed set of items.
    ///
    /// Avoid using an empty prefix as it may scan a lot of items (unless limited).
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// tree.insert("a", nanoid::nanoid!())?;
    /// tree.insert("ab", nanoid::nanoid!())?;
    /// tree.insert("abc", nanoid::nanoid!())?;
    /// assert_eq!(2, tree.prefix("ab")?.into_iter().count());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn prefix<K: Into<Vec<u8>>>(&self, prefix: K) -> crate::Result<Prefix<'_>> {
        use std::ops::Bound::{self};

        let prefix = prefix.into();

        let lock = self.levels.read().expect("lock is poisoned");

        let bounds: (Bound<Vec<u8>>, Bound<Vec<u8>>) =
            (Bound::Included(prefix.clone()), std::ops::Bound::Unbounded);

        let segment_info = lock
            .get_all_segments()
            .values()
            .filter(|x| x.check_key_range_overlap(&bounds))
            .cloned()
            .collect();

        Ok(Prefix::new(
            MemTableGuard {
                active: self.active_memtable.read().expect("lock is poisoned"),
                immutable: self.immutable_memtables.read().expect("lock is poisoned"),
            },
            prefix,
            segment_info,
        ))
    }

    /// Returns the first key-value pair in the tree.
    /// The key in this pair is the minimum key in the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # use lsm_tree::Error as TreeError;
    /// # use lsm_tree::{Tree, Config};
    /// #
    /// # let folder = tempfile::tempdir()?;
    /// # let mut tree = Config::new(folder).open()?;
    /// #
    /// tree.insert("1", "abc")?;
    /// tree.insert("3", "abc")?;
    /// tree.insert("5", "abc")?;
    ///
    /// let (key, _) = tree.first_key_value()?.expect("item should exist");
    /// assert_eq!(key, "1".as_bytes());
    ///
    /// # Ok::<(), TreeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn first_key_value(&self) -> crate::Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.iter()?.into_iter().next().transpose()
    }

    /// Returns the last key-value pair in the tree.
    /// The key in this pair is the maximum key in the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # use lsm_tree::Error as TreeError;
    /// # use lsm_tree::{Tree, Config};
    /// #
    /// # let folder = tempfile::tempdir()?;
    /// # let mut tree = Config::new(folder).open()?;
    /// #
    /// tree.insert("1", "abc")?;
    /// tree.insert("3", "abc")?;
    /// tree.insert("5", "abc")?;
    /// #
    /// let (key, _) = tree.last_key_value()?.expect("item should exist");
    /// assert_eq!(key, "5".as_bytes());
    ///
    /// # Ok::<(), TreeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn last_key_value(&self) -> crate::Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.iter()?.into_iter().next_back().transpose()
    }

    #[doc(hidden)]
    pub fn get_internal_entry<K: AsRef<[u8]>>(
        &self,
        key: K,
        evict_tombstone: bool,
    ) -> crate::Result<Option<Value>> {
        let memtable_lock = self.active_memtable.read().expect("lock is poisoned");

        if let Some(item) = memtable_lock.get(&key) {
            if evict_tombstone {
                return Ok(ignore_tombstone_value(item));
            }
            return Ok(Some(item));
        };
        drop(memtable_lock);

        // Now look in immutable memtables
        let memtable_lock = self.immutable_memtables.read().expect("lock is poisoned");
        for (_, memtable) in memtable_lock.iter().rev() {
            if let Some(item) = memtable.get(&key) {
                if evict_tombstone {
                    return Ok(ignore_tombstone_value(item));
                }
                return Ok(Some(item));
            }
        }
        drop(memtable_lock);

        // Now look in segments... this may involve disk I/O
        let segment_lock = self.levels.read().expect("lock is poisoned");
        let segments = &segment_lock.get_all_segments_flattened();

        for segment in segments {
            if let Some(item) = segment.get(&key)? {
                if evict_tombstone {
                    return Ok(ignore_tombstone_value(item));
                }
                return Ok(Some(item));
            }
        }

        Ok(None)
    }

    /// Retrieves an item from the tree.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("a", "my_value")?;
    ///
    /// let item = tree.get("a")?;
    /// assert_eq!(Some("my_value".as_bytes().to_vec()), item);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn get<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<Option<Vec<u8>>> {
        Ok(self.get_internal_entry(key, true)?.map(|x| x.value))
    }

    pub(crate) fn increment_lsn(&self) -> SeqNo {
        self.lsn.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
    }

    /// Compare-and-swap an entry
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn compare_and_swap<K: AsRef<[u8]>>(
        &self,
        key: K,
        expected: Option<&Vec<u8>>,
        next: Option<&Vec<u8>>,
    ) -> crate::Result<CompareAndSwapResult> {
        let key = key.as_ref();

        match self.get(key)? {
            Some(current_value) => {
                match expected {
                    Some(expected_value) => {
                        // We expected Some and got Some
                        // Check if the value is as expected
                        if current_value != *expected_value {
                            return Ok(Err(CompareAndSwapError {
                                prev: Some(current_value),
                                next: next.cloned(),
                            }));
                        }

                        // Set or delete the object now
                        match next {
                            Some(next_value) => {
                                self.insert(key, next_value.clone())?;
                                Ok(Ok(()))
                            }
                            None => {
                                self.remove(key)?;
                                Ok(Ok(()))
                            }
                        }
                    }
                    None => {
                        // We expected Some but got None
                        // CAS error!
                        Ok(Err(CompareAndSwapError {
                            prev: None,
                            next: next.cloned(),
                        }))
                    }
                }
            }
            None => match expected {
                Some(_) => {
                    // We expected Some but got None
                    // CAS error!
                    Ok(Err(CompareAndSwapError {
                        prev: None,
                        next: next.cloned(),
                    }))
                }
                None => match next {
                    // We expected None and got None
                    // Set or delete the object now
                    Some(next_value) => {
                        self.insert(key, next_value.clone())?;
                        Ok(Ok(()))
                    }
                    None => {
                        self.remove(key)?;
                        Ok(Ok(()))
                    }
                },
            },
        }
    }

    /// Atomically fetches and updates an item if it exists.
    ///
    /// Returns the previous value if the item exists.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("key", "a")?;
    ///
    /// let prev = tree.fetch_update("key", |_| Some("b"))?.expect("item should exist");
    /// assert_eq!("a".as_bytes(), prev);
    ///
    /// let item = tree.get("key")?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), item);
    ///
    /// let prev = tree.fetch_update("key", |_| None::<String>)?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), prev);
    ///
    /// assert!(tree.is_empty()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn fetch_update<K: AsRef<[u8]>, V: Into<Vec<u8>>, F: Fn(Option<&Vec<u8>>) -> Option<V>>(
        &self,
        key: K,
        f: F,
    ) -> crate::Result<Option<Vec<u8>>> {
        let key = key.as_ref();

        let mut fetched = self.get(key)?;

        loop {
            let expected = fetched.as_ref();
            let next = f(expected).map(Into::into);

            match self.compare_and_swap(key, expected, next.as_ref())? {
                Ok(_) => return Ok(fetched),
                Err(err) => {
                    fetched = err.prev;
                }
            }
        }
    }

    /// Atomically fetches and updates an item if it exists.
    ///
    /// Returns the updated value if the item exists.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?;
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder).open()?;
    /// tree.insert("key", "a")?;
    ///
    /// let prev = tree.update_fetch("key", |_| Some("b"))?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), prev);
    ///
    /// let item = tree.get("key")?.expect("item should exist");
    /// assert_eq!("b".as_bytes(), item);
    ///
    /// let prev = tree.update_fetch("key", |_| None::<String>)?;
    /// assert_eq!(None, prev);
    ///
    /// assert!(tree.is_empty()?);
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn update_fetch<K: AsRef<[u8]>, V: Into<Vec<u8>>, F: Fn(Option<&Vec<u8>>) -> Option<V>>(
        &self,
        key: K,
        f: F,
    ) -> crate::Result<Option<Vec<u8>>> {
        let key = key.as_ref();

        let mut fetched = self.get(key)?;

        loop {
            let expected = fetched.as_ref();
            let next = f(expected).map(Into::into);

            match self.compare_and_swap(key, expected, next.as_ref())? {
                Ok(_) => return Ok(next),
                Err(err) => {
                    fetched = err.prev;
                }
            }
        }
    }

    /// Force-starts a memtable flush thread.
    #[doc(hidden)]
    pub fn force_memtable_flush(
        &self,
    ) -> crate::Result<std::thread::JoinHandle<crate::Result<()>>> {
        crate::flush::start(self)
    }

    /// Force-starts a memtable flush thread and waits until its completely done.
    #[doc(hidden)]
    pub fn wait_for_memtable_flush(&self) -> crate::Result<()> {
        let flush_thread = self.force_memtable_flush()?;
        flush_thread.join().expect("should join")
    }

    /// Performs major compaction.
    #[doc(hidden)]
    #[must_use]
    pub fn do_major_compaction(&self) -> std::thread::JoinHandle<crate::Result<()>> {
        log::info!("Starting major compaction thread");
        let tree = self.clone();

        std::thread::spawn(move || {
            let levels = tree.levels.write().expect("lock is poisoned");
            let compactor = crate::compaction::major::Strategy::default();

            let choice = compactor.choose(&levels);

            if let crate::compaction::Choice::DoCompact(payload) = choice {
                crate::compaction::worker::do_compaction(&tree, &payload, levels)?;
            }
            Ok(())
        })
    }

    /// Flushes the journal to disk, making sure all written data
    /// is persisted and crash-safe.
    ///
    /// # Examples
    ///
    /// ```
    /// # let folder = tempfile::tempdir()?.into_path();
    /// use lsm_tree::{Config, Tree};
    ///
    /// let tree = Config::new(folder.clone()).open()?;
    /// tree.insert("a", nanoid::nanoid!())?;
    /// tree.flush()?;
    ///
    /// let tree = Config::new(folder).open()?;
    ///
    /// let item = tree.get("a")?;
    /// assert!(item.is_some());
    /// #
    /// # Ok::<(), lsm_tree::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Will return `Err` if an IO error occurs.
    pub fn flush(&self) -> crate::Result<()> {
        self.journal.flush()?;
        Ok(())
    }
}
