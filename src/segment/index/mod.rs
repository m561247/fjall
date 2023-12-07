pub mod writer;

use crate::block_cache::BlockCache;
use crate::descriptor_table::FileDescriptorTable;
use crate::disk_block::DiskBlock;
use crate::disk_block_index::{DiskBlockIndex, DiskBlockReference};
use crate::file::{BLOCKS_FILE, TOP_LEVEL_INDEX_FILE};
use crate::serde::{Deserializable, Serializable};
use crate::value::UserKey;
use crate::version::Version;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::sync::Arc;

/// Points to a block on file
///
/// # Disk representation
///
/// \[offset; 8 bytes] - \[size; 4 byte] - \[key length; 2 bytes] - \[key; N bytes]
#[derive(Clone, Debug)]
pub struct IndexEntry {
    /// Position of block in file
    pub offset: u64,

    /// Size of block in bytes
    pub size: u32,

    /// Key of first item in block
    pub start_key: UserKey,
}

impl Serializable for IndexEntry {
    fn serialize<W: Write>(&self, writer: &mut W) -> Result<(), crate::SerializeError> {
        writer.write_u64::<BigEndian>(self.offset)?;
        writer.write_u32::<BigEndian>(self.size)?;

        // NOTE: Truncation is okay and actually needed
        #[allow(clippy::cast_possible_truncation)]
        writer.write_u16::<BigEndian>(self.start_key.len() as u16)?;

        writer.write_all(&self.start_key)?;

        Ok(())
    }
}

impl Deserializable for IndexEntry {
    fn deserialize<R: Read>(reader: &mut R) -> Result<Self, crate::DeserializeError>
    where
        Self: Sized,
    {
        let offset = reader.read_u64::<BigEndian>()?;
        let size = reader.read_u32::<BigEndian>()?;

        let key_len = reader.read_u16::<BigEndian>()?;

        let mut key = vec![0; key_len.into()];
        reader.read_exact(&mut key)?;

        Ok(Self {
            offset,
            size,
            start_key: Arc::from(key),
        })
    }
}

pub type IndexBlock = DiskBlock<IndexEntry>;

pub struct IndexBlockIndex(Arc<BlockCache>);

impl IndexBlockIndex {
    pub fn insert(&self, segment_id: String, key: UserKey, value: Arc<IndexBlock>) {
        self.0.insert_index_block(segment_id, key, value);
    }

    pub fn get(&self, segment_id: String, key: &[u8]) -> Option<Arc<IndexBlock>> {
        self.0.get_index_block(segment_id, key)
    }
}

/// In-memory index that translates item keys to block refs.
///
/// See <https://rocksdb.org/blog/2017/05/12/partitioned-index-filter.html>
pub struct MetaIndex {
    descriptor_table: Arc<FileDescriptorTable>,

    /// Segment ID
    segment_id: String,

    /// Level-0 index ("fence pointers"). Is read-only and always fully loaded.
    ///
    /// This index points to index blocks inside the level-1 index.
    index: DiskBlockIndex,

    /// Level-1 index. This index is only partially loaded into memory, decreasing memory usage, compared to a fully loaded one.
    ///
    /// However to find a disk block, one layer of indirection is required:
    ///
    /// To find a reference to a segment block, first the level-0 index needs to be checked,
    /// then the corresponding index block needs to be loaded, which contains the wanted disk block reference.
    blocks: IndexBlockIndex,
}

impl IndexBlock {
    pub(crate) fn get_previous_block_info(&self, key: &[u8]) -> Option<&IndexEntry> {
        self.items.iter().rev().find(|x| &*x.start_key < key)
    }

    pub(crate) fn get_next_block_info(&self, key: &[u8]) -> Option<&IndexEntry> {
        self.items.iter().find(|x| &*x.start_key > key)
    }

    /// Finds the block that contains a key
    pub(crate) fn get_lower_bound_block_info(&self, key: &[u8]) -> Option<&IndexEntry> {
        self.items.iter().rev().find(|x| &*x.start_key <= key)
    }
}

impl MetaIndex {
    pub fn get_prefix_upper_bound(&self, key: &[u8]) -> crate::Result<Option<IndexEntry>> {
        let Some((block_key, block_ref)) = self.index.get_prefix_upper_bound(key) else {
            return Ok(None);
        };

        let index_block = self.load_index_block(block_key, block_ref)?;
        Ok(index_block.items.first().cloned())
    }

    pub fn get_upper_bound_block_info(&self, key: &[u8]) -> crate::Result<Option<IndexEntry>> {
        let Some((block_key, block_ref)) = self.index.get_lower_bound_block_info(key) else {
            return Ok(None);
        };

        let index_block = self.load_index_block(block_key, block_ref)?;

        let next_block = index_block.get_next_block_info(key);

        match next_block {
            Some(block) => Ok(Some(block).cloned()),
            None => {
                // The upper bound block is not in the same index block as the key, so load next index block
                let Some((block_key, block_ref)) = self.index.get_next_block_key(key) else {
                    return Ok(None);
                };

                Ok(Some(IndexEntry {
                    offset: block_ref.offset,
                    size: block_ref.size,
                    start_key: block_key.to_vec().into(),
                }))
            }
        }
    }

    /// Gets the reference to a disk block that should contain the given item
    pub fn get_lower_bound_block_info(&self, key: &[u8]) -> crate::Result<Option<IndexEntry>> {
        let Some((block_key, block_ref)) = self.index.get_lower_bound_block_info(key) else {
            return Ok(None);
        };

        let index_block = self.load_index_block(block_key, block_ref)?;
        Ok(index_block.get_lower_bound_block_info(key).cloned())
    }

    /// Returns the previous index block's key, if it exists, or None
    pub fn get_previous_block_key(&self, key: &[u8]) -> crate::Result<Option<IndexEntry>> {
        let Some((first_block_key, first_block_ref)) = self.index.get_lower_bound_block_info(key)
        else {
            return Ok(None);
        };

        let index_block = self.load_index_block(first_block_key, first_block_ref)?;

        let maybe_prev = index_block.get_previous_block_info(key);

        match maybe_prev {
            Some(item) => Ok(Some(item).cloned()),
            None => {
                let Some((prev_block_key, prev_block_ref)) =
                    self.index.get_previous_block_key(first_block_key)
                else {
                    return Ok(None);
                };

                let index_block = self.load_index_block(prev_block_key, prev_block_ref)?;

                Ok(index_block.items.last().cloned())
            }
        }
    }

    /// Returns the next index block's key, if it exists, or None
    pub fn get_next_block_key(&self, key: &[u8]) -> crate::Result<Option<IndexEntry>> {
        let Some((first_block_key, first_block_ref)) = self.index.get_lower_bound_block_info(key)
        else {
            return Ok(None);
        };

        let index_block = self.load_index_block(first_block_key, first_block_ref)?;

        let maybe_next = index_block.get_next_block_info(key);

        match maybe_next {
            Some(item) => Ok(Some(item).cloned()),
            None => {
                let Some((next_block_key, next_block_ref)) =
                    self.index.get_next_block_key(first_block_key)
                else {
                    return Ok(None);
                };

                let index_block = self.load_index_block(next_block_key, next_block_ref)?;

                Ok(index_block.items.first().cloned())
            }
        }
    }

    /// Returns the first block's key
    pub fn get_first_block_key(&self) -> crate::Result<IndexEntry> {
        let (block_key, block_ref) = self.index.get_first_block_key();
        let index_block = self.load_index_block(block_key, block_ref)?;

        Ok(index_block
            .items
            .first()
            .expect("block should not be empty")
            .clone())
    }

    /// Returns the last block's key
    pub fn get_last_block_key(&self) -> crate::Result<IndexEntry> {
        let (block_key, block_ref) = self.index.get_last_block_key();
        let index_block = self.load_index_block(block_key, block_ref)?;

        Ok(index_block
            .items
            .last()
            .expect("block should not be empty")
            .clone())
    }

    /// Load an index block from disk
    fn load_index_block(
        &self,
        block_key: &[u8],
        block_ref: &DiskBlockReference,
    ) -> crate::Result<Arc<DiskBlock<IndexEntry>>> {
        match self.blocks.get(self.segment_id.clone(), block_key) {
            Some(block) => {
                // Cache hit: Copy from block

                Ok(block)
            }
            None => {
                // Cache miss: load from disk

                let mut file_reader = self.descriptor_table.access();

                let block = IndexBlock::from_file_compressed(
                    &mut *file_reader,
                    block_ref.offset,
                    block_ref.size,
                )?;

                drop(file_reader);

                let block = Arc::new(block);

                self.blocks.insert(
                    self.segment_id.clone(),
                    block_key.into(),
                    Arc::clone(&block),
                );

                Ok(block)
            }
        }
    }

    pub fn get_latest<K: AsRef<[u8]>>(&self, key: K) -> crate::Result<Option<IndexEntry>> {
        let key = key.as_ref();

        let Some((block_key, index_block_ref)) = self.index.get_lower_bound_block_info(key) else {
            return Ok(None);
        };

        let index_block = self.load_index_block(block_key, index_block_ref)?;

        Ok(index_block.get_lower_bound_block_info(key).cloned())
    }

    // TODO: use this instead of from_file after writing Segment somehow...
    pub fn from_items(
        segment_id: String,
        descriptor_table: Arc<FileDescriptorTable>,
        items: Vec<IndexEntry>,
        block_cache: Arc<BlockCache>,
    ) -> crate::Result<Self> {
        let mut tree = BTreeMap::new();

        for item in items {
            tree.insert(
                item.start_key,
                DiskBlockReference {
                    offset: item.offset,
                    size: item.size,
                },
            );
        }

        Ok(Self {
            descriptor_table,
            segment_id,
            index: DiskBlockIndex::new(tree),
            blocks: IndexBlockIndex(Arc::clone(&block_cache)),
        })
    }

    /// Only used for tests
    #[allow(dead_code)]
    pub(crate) fn new(segment_id: String, block_cache: Arc<BlockCache>) -> crate::Result<Self> {
        let index_block_index = IndexBlockIndex(Arc::clone(&block_cache));

        Ok(Self {
            // NOTE: It's just a test
            #[allow(clippy::expect_used)]
            descriptor_table: Arc::new(
                FileDescriptorTable::new("Cargo.toml").expect("should open"),
            ),
            segment_id,
            blocks: index_block_index,
            index: DiskBlockIndex::new(BTreeMap::default()),
        })
    }

    pub fn from_file<P: AsRef<Path>>(
        segment_id: String,
        descriptor_table: Arc<FileDescriptorTable>,
        path: P,
        block_cache: Arc<BlockCache>,
    ) -> crate::Result<Self> {
        log::debug!("Reading block index from {}", path.as_ref().display());

        // TODO: change to debug asserts
        assert!(
            path.as_ref().exists(),
            "{} missing",
            path.as_ref().display()
        );
        assert!(
            path.as_ref().join(TOP_LEVEL_INDEX_FILE).exists(),
            "{} missing",
            path.as_ref().display()
        );
        assert!(
            path.as_ref().join(BLOCKS_FILE).exists(),
            "{} missing",
            path.as_ref().display()
        );

        let version_size = Version::len().into();
        let file_size = std::fs::metadata(path.as_ref().join(TOP_LEVEL_INDEX_FILE))?.len();

        let index = IndexBlock::from_file_compressed(
            &mut BufReader::new(File::open(path.as_ref().join(TOP_LEVEL_INDEX_FILE))?), // TODO:
            version_size,
            (file_size - version_size) as u32,
        )?;

        if !index.check_crc(index.crc)? {
            return Err(crate::Error::CrcCheck);
        }

        debug_assert!(!index.items.is_empty());

        Self::from_items(segment_id, descriptor_table, index.items, block_cache)
    }
}
