// Copyright 2018 The Grin Developers
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Implements storage primitives required by the chain

use std::sync::{Arc, RwLock};

use croaring::Bitmap;
use lru_cache::LruCache;

use util::secp::pedersen::Commitment;

use core::consensus::TargetError;
use core::core::hash::{Hash, Hashed};
use core::core::target::Difficulty;
use core::core::{Block, BlockHeader};
use grin_store::{self, option_to_not_found, to_key, u64_to_key, Error};
use types::{BlockMarker, BlockSums, ChainStore, Tip};

const STORE_SUBPATH: &'static str = "chain";

const BLOCK_HEADER_PREFIX: u8 = 'h' as u8;
const BLOCK_PREFIX: u8 = 'b' as u8;
const HEAD_PREFIX: u8 = 'H' as u8;
const HEADER_HEAD_PREFIX: u8 = 'I' as u8;
const SYNC_HEAD_PREFIX: u8 = 's' as u8;
const HEADER_HEIGHT_PREFIX: u8 = '8' as u8;
const COMMIT_POS_PREFIX: u8 = 'c' as u8;
const BLOCK_MARKER_PREFIX: u8 = 'm' as u8;
const BLOCK_SUMS_PREFIX: u8 = 'M' as u8;
const BLOCK_INPUT_BITMAP_PREFIX: u8 = 'B' as u8;

/// An implementation of the ChainStore trait backed by a simple key-value
/// store.
pub struct ChainKVStore {
	db: grin_store::Store,
	header_cache: Arc<RwLock<LruCache<Hash, BlockHeader>>>,
	block_input_bitmap_cache: Arc<RwLock<LruCache<Hash, Vec<u8>>>>,
}

impl ChainKVStore {
	/// Create new chain store
	pub fn new(root_path: String) -> Result<ChainKVStore, Error> {
		let db = grin_store::Store::open(format!("{}/{}", root_path, STORE_SUBPATH).as_str())?;
		Ok(ChainKVStore {
			db,
			header_cache: Arc::new(RwLock::new(LruCache::new(1_000))),
			block_input_bitmap_cache: Arc::new(RwLock::new(LruCache::new(1_000))),
		})
	}

	fn get_block_header_db(&self, h: &Hash) -> Result<BlockHeader, Error> {
		option_to_not_found(
			self.db
				.get_ser(&to_key(BLOCK_HEADER_PREFIX, &mut h.to_vec())),
		)
	}

	fn build_block_input_bitmap(&self, block: &Block) -> Result<Bitmap, Error> {
		let bitmap = block
			.inputs
			.iter()
			.filter_map(|x| self.get_output_pos(&x.commitment()).ok())
			.map(|x| x as u32)
			.collect();
		Ok(bitmap)
	}

	// Get the block input bitmap from the db or build the bitmap from
	// the full block from the db (if the block is found).
	fn get_block_input_bitmap_db(&self, bh: &Hash) -> Result<Bitmap, Error> {
		if let Ok(Some(bytes)) = self.db
			.get(&to_key(BLOCK_INPUT_BITMAP_PREFIX, &mut bh.to_vec()))
		{
			Ok(Bitmap::deserialize(&bytes))
		} else {
			match self.get_block(bh) {
				Ok(block) => {
					let bitmap = self.save_block_input_bitmap(&block)?;
					Ok(bitmap)
				}
				Err(e) => Err(e),
			}
		}
	}
}

impl ChainStore for ChainKVStore {
	fn head(&self) -> Result<Tip, Error> {
		option_to_not_found(self.db.get_ser(&vec![HEAD_PREFIX]))
	}

	fn head_header(&self) -> Result<BlockHeader, Error> {
		self.get_block_header(&self.head()?.last_block_h)
	}

	fn save_head(&self, t: &Tip) -> Result<(), Error> {
		self.db
			.batch()
			.put_ser(&vec![HEAD_PREFIX], t)?
			.put_ser(&vec![HEADER_HEAD_PREFIX], t)?
			.write()
	}

	fn save_body_head(&self, t: &Tip) -> Result<(), Error> {
		self.db.put_ser(&vec![HEAD_PREFIX], t)
	}

	fn get_header_head(&self) -> Result<Tip, Error> {
		option_to_not_found(self.db.get_ser(&vec![HEADER_HEAD_PREFIX]))
	}

	fn save_header_head(&self, t: &Tip) -> Result<(), Error> {
		self.db.put_ser(&vec![HEADER_HEAD_PREFIX], t)
	}

	fn get_sync_head(&self) -> Result<Tip, Error> {
		option_to_not_found(self.db.get_ser(&vec![SYNC_HEAD_PREFIX]))
	}

	fn save_sync_head(&self, t: &Tip) -> Result<(), Error> {
		self.db.put_ser(&vec![SYNC_HEAD_PREFIX], t)
	}

	fn init_head(&self) -> Result<(), Error> {
		if self.get_header_head().is_err() {
			let tip = self.head()?;
			self.save_header_head(&tip)?;
		}
		let header_tip = self.get_header_head()?;
		self.save_sync_head(&header_tip)
	}

	// Reset both header_head and sync_head to the current head of the body chain
	fn reset_head(&self) -> Result<(), Error> {
		let tip = self.head()?;
		self.save_header_head(&tip)?;
		self.save_sync_head(&tip)
	}

	fn get_block(&self, h: &Hash) -> Result<Block, Error> {
		option_to_not_found(self.db.get_ser(&to_key(BLOCK_PREFIX, &mut h.to_vec())))
	}

	fn block_exists(&self, h: &Hash) -> Result<bool, Error> {
		self.db.exists(&to_key(BLOCK_PREFIX, &mut h.to_vec()))
	}

	fn get_block_header(&self, h: &Hash) -> Result<BlockHeader, Error> {
		{
			let mut header_cache = self.header_cache.write().unwrap();

			// cache hit - return the value from the cache
			if let Some(header) = header_cache.get_mut(h) {
				return Ok(header.clone());
			}
		}

		// cache miss - get it from db and cache it for next time (if we found one in
		// db)
		let res = self.get_block_header_db(h);
		if let Ok(header) = res {
			let mut header_cache = self.header_cache.write().unwrap();
			header_cache.insert(*h, header.clone());
			return Ok(header);
		}
		res
	}

	/// Save the block and its header
	fn save_block(&self, b: &Block) -> Result<(), Error> {
		let batch = self.db
			.batch()
			.put_ser(&to_key(BLOCK_PREFIX, &mut b.hash().to_vec())[..], b)?
			.put_ser(
				&to_key(BLOCK_HEADER_PREFIX, &mut b.hash().to_vec())[..],
				&b.header,
			)?;
		batch.write()
	}

	/// Delete a full block. Does not delete any record associated with a block
	/// header.
	fn delete_block(&self, bh: &Hash) -> Result<(), Error> {
		self.db.delete(&to_key(BLOCK_PREFIX, &mut bh.to_vec())[..])
	}

	// We are on the current chain if -
	// * the header by height index matches the header, and
	// * we are not ahead of the current head
	fn is_on_current_chain(&self, header: &BlockHeader) -> Result<(), Error> {
		let head = self.head()?;

		// check we are not out ahead of the current head
		if header.height > head.height {
			return Err(Error::NotFoundErr);
		}

		let header_at_height = self.get_header_by_height(header.height)?;
		if header.hash() == header_at_height.hash() {
			Ok(())
		} else {
			Err(Error::NotFoundErr)
		}
	}

	fn save_block_header(&self, bh: &BlockHeader) -> Result<(), Error> {
		let hash = bh.hash();
		self.db
			.put_ser(&to_key(BLOCK_HEADER_PREFIX, &mut hash.to_vec())[..], bh)?;

		// Write the block_header to the cache also.
		{
			let mut header_cache = self.header_cache.write().unwrap();
			header_cache.insert(hash, bh.clone());
		}

		Ok(())
	}

	fn get_header_by_height(&self, height: u64) -> Result<BlockHeader, Error> {
		option_to_not_found(self.db.get_ser(&u64_to_key(HEADER_HEIGHT_PREFIX, height)))
			.and_then(|hash| self.get_block_header(&hash))
	}

	fn save_header_height(&self, bh: &BlockHeader) -> Result<(), Error> {
		self.db
			.put_ser(&u64_to_key(HEADER_HEIGHT_PREFIX, bh.height), &bh.hash())
	}

	fn delete_header_by_height(&self, height: u64) -> Result<(), Error> {
		self.db.delete(&u64_to_key(HEADER_HEIGHT_PREFIX, height))
	}

	fn save_output_pos(&self, commit: &Commitment, pos: u64) -> Result<(), Error> {
		self.db.put_ser(
			&to_key(COMMIT_POS_PREFIX, &mut commit.as_ref().to_vec())[..],
			&pos,
		)
	}

	fn get_output_pos(&self, commit: &Commitment) -> Result<u64, Error> {
		option_to_not_found(
			self.db
				.get_ser(&to_key(COMMIT_POS_PREFIX, &mut commit.as_ref().to_vec())),
		)
	}

	fn delete_output_pos(&self, commit: &[u8]) -> Result<(), Error> {
		self.db
			.delete(&to_key(COMMIT_POS_PREFIX, &mut commit.to_vec()))
	}

	fn save_block_marker(&self, bh: &Hash, marker: &BlockMarker) -> Result<(), Error> {
		self.db
			.put_ser(&to_key(BLOCK_MARKER_PREFIX, &mut bh.to_vec())[..], &marker)
	}

	fn get_block_marker(&self, bh: &Hash) -> Result<BlockMarker, Error> {
		option_to_not_found(
			self.db
				.get_ser(&to_key(BLOCK_MARKER_PREFIX, &mut bh.to_vec())),
		)
	}

	fn delete_block_marker(&self, bh: &Hash) -> Result<(), Error> {
		self.db
			.delete(&to_key(BLOCK_MARKER_PREFIX, &mut bh.to_vec()))
	}

	fn save_block_sums(&self, bh: &Hash, marker: &BlockSums) -> Result<(), Error> {
		self.db
			.put_ser(&to_key(BLOCK_SUMS_PREFIX, &mut bh.to_vec())[..], &marker)
	}

	fn get_block_sums(&self, bh: &Hash) -> Result<BlockSums, Error> {
		option_to_not_found(
			self.db
				.get_ser(&to_key(BLOCK_SUMS_PREFIX, &mut bh.to_vec())),
		)
	}

	fn delete_block_sums(&self, bh: &Hash) -> Result<(), Error> {
		self.db.delete(&to_key(BLOCK_SUMS_PREFIX, &mut bh.to_vec()))
	}

	fn get_block_input_bitmap(&self, bh: &Hash) -> Result<Bitmap, Error> {
		{
			let mut cache = self.block_input_bitmap_cache.write().unwrap();

			// cache hit - return the value from the cache
			if let Some(bytes) = cache.get_mut(bh) {
				return Ok(Bitmap::deserialize(&bytes));
			}
		}

		// cache miss - get it from db and cache it for next time
		// if we found one in db
		let res = self.get_block_input_bitmap_db(bh);
		if let Ok(bitmap) = res {
			let mut cache = self.block_input_bitmap_cache.write().unwrap();
			cache.insert(*bh, bitmap.serialize());
			return Ok(bitmap);
		}
		res
	}

	fn save_block_input_bitmap(&self, block: &Block) -> Result<Bitmap, Error> {
		let hash = block.hash();
		let bitmap = self.build_block_input_bitmap(block)?;
		self.db.put(
			&to_key(BLOCK_INPUT_BITMAP_PREFIX, &mut hash.to_vec())[..],
			bitmap.serialize(),
		)?;
		{
			let mut cache = self.block_input_bitmap_cache.write().unwrap();
			cache.insert(hash, bitmap.serialize());
		}
		Ok(bitmap)
	}

	fn delete_block_input_bitmap(&self, bh: &Hash) -> Result<(), Error> {
		self.db
			.delete(&to_key(BLOCK_INPUT_BITMAP_PREFIX, &mut bh.to_vec()))
	}

	/// Maintain consistency of the "header_by_height" index by traversing back
	/// through the current chain and updating "header_by_height" until we reach
	/// a block_header that is consistent with its height (everything prior to
	/// this will be consistent).
	/// We need to handle the case where we have no index entry for a given
	/// height to account for the case where we just switched to a new fork and
	/// the height jumped beyond current chain height.
	fn setup_height(&self, header: &BlockHeader, old_tip: &Tip) -> Result<(), Error> {
		// remove headers ahead if we backtracked
		for n in header.height..old_tip.height {
			self.delete_header_by_height(n + 1)?;
		}
		self.build_by_height_index(header, false)
	}

	fn build_by_height_index(&self, header: &BlockHeader, force: bool) -> Result<(), Error> {
		self.save_header_height(&header)?;

		if header.height > 0 {
			let mut prev_header = self.get_block_header(&header.previous)?;
			while prev_header.height > 0 {
				if !force {
					if let Ok(_) = self.is_on_current_chain(&prev_header) {
						break;
					}
				}
				self.save_header_height(&prev_header)?;

				prev_header = self.get_block_header(&prev_header.previous)?;
			}
		}
		Ok(())
	}
}

/// An iterator on blocks, from latest to earliest, specialized to return
/// information pertaining to block difficulty calculation (timestamp and
/// previous difficulties). Mostly used by the consensus next difficulty
/// calculation.
pub struct DifficultyIter {
	start: Hash,
	store: Arc<ChainStore>,

	// maintain state for both the "next" header in this iteration
	// and its previous header in the chain ("next next" in the iteration)
	// so we effectively read-ahead as we iterate through the chain back
	// toward the genesis block (while maintaining current state)
	header: Option<BlockHeader>,
	prev_header: Option<BlockHeader>,
}

impl DifficultyIter {
	/// Build a new iterator using the provided chain store and starting from
	/// the provided block hash.
	pub fn from(start: Hash, store: Arc<ChainStore>) -> DifficultyIter {
		DifficultyIter {
			start: start,
			store: store,
			header: None,
			prev_header: None,
		}
	}
}

impl Iterator for DifficultyIter {
	type Item = Result<(u64, Difficulty), TargetError>;

	fn next(&mut self) -> Option<Self::Item> {
		// Get both header and previous_header if this is the initial iteration.
		// Otherwise move prev_header to header and get the next prev_header.
		self.header = if self.header.is_none() {
			self.store.get_block_header(&self.start).ok()
		} else {
			self.prev_header.clone()
		};

		// If we have a header we can do this iteration.
		// Otherwise we are done.
		if let Some(header) = self.header.clone() {
			self.prev_header = self.store.get_block_header(&header.previous).ok();

			let prev_difficulty = self.prev_header
				.clone()
				.map_or(Difficulty::zero(), |x| x.total_difficulty);
			let difficulty = header.total_difficulty - prev_difficulty;

			Some(Ok((header.timestamp.to_timespec().sec as u64, difficulty)))
		} else {
			return None;
		}
	}
}
