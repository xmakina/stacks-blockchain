/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::fs;
use std::mem;
use std::collections::HashSet;
use std::collections::HashMap;
use chainstate::stacks::Error;
use chainstate::stacks::*;
use chainstate::stacks::db::{
    ClarityTx,
    StacksChainState,
    blocks::MemPoolRejection
};
use chainstate::stacks::index::TrieHash;
use chainstate::burn::BlockHeaderHash;
use chainstate::burn::db::burndb::BurnDB;
use chainstate::stacks::events::StacksTransactionReceipt;

use net::StacksMessageCodec;
use net::Error as net_error;
use net::codec::{read_next, write_next};
use vm::clarity::ClarityConnection;

use util::hash::MerkleTree;
use util::hash::Sha512Trunc256Sum;
use util::secp256k1::{MessageSignature, Secp256k1PrivateKey};

use net::StacksPublicKeyBuffer;

use chainstate::burn::*;
use chainstate::burn::operations::*;

use burnchains::BurnchainHeaderHash;
use burnchains::PrivateKey;
use burnchains::PublicKey;

use util::vrf::*;

use core::*;
use core::mempool::*;

///
///    Independent structure for building microblocks:
///       StacksBlockBuilder cannot be used, since microblocks should only be broadcasted
///       once the anchored block is mined, won sortition, and a StacksBlockBuilder will
///       not survive that long.
///
///     StacksMicroblockBuilder holds a mutable reference to the provided chainstate in the
///       new function. This is required for the `clarity_tx` -- basically, to append transactions
///       as new microblocks, the builder _needs_ to be able to keep the current clarity_tx "open" 
pub struct StacksMicroblockBuilder<'a> {
    anchor_block: BlockHeaderHash,
    anchor_block_bhh: BurnchainHeaderHash,
    anchor_block_height: u64,
    prev_microblock_header: Option<StacksMicroblockHeader>,
    header_reader: StacksChainState,
    clarity_tx: Option<ClarityTx<'a>>,
    considered: Option<HashSet<Txid>>,
    bytes_so_far: u64,
}

impl <'a> StacksMicroblockBuilder <'a> {
    pub fn new(anchor_block: BlockHeaderHash, anchor_block_bhh: BurnchainHeaderHash,
               chainstate: &'a mut StacksChainState, initial_cost: ExecutionCost, bytes_so_far: u64) -> Result<StacksMicroblockBuilder<'a>, Error> {
        let header_reader = chainstate.reopen()?;
        let mut clarity_tx = chainstate.block_begin(&anchor_block_bhh, &anchor_block,
                                                    &MINER_BLOCK_BURN_HEADER_HASH, &MINER_BLOCK_HEADER_HASH);
        let anchor_block_height = 
            StacksChainState::get_anchored_block_header_info(&header_reader.headers_db, &anchor_block_bhh, &anchor_block)?
            .ok_or(Error::NoSuchBlockError)?
            .block_height;

        clarity_tx.reset_cost(initial_cost);
        Ok(StacksMicroblockBuilder {
            anchor_block,
            anchor_block_bhh,
            anchor_block_height,
            bytes_so_far,
            clarity_tx: Some(clarity_tx),
            header_reader,
            prev_microblock_header: None,
            considered: Some(HashSet::new()),
        })
    }

    pub fn mine_next_microblock(&mut self,
                                mem_pool: &MemPoolDB,
                                miner_key: &Secp256k1PrivateKey,
                                miner_pubkey_hash: &Hash160) -> Result<StacksMicroblock, Error> {
        let mut txs_to_broadcast = vec![];

        let mut clarity_tx = self.clarity_tx.take()
            .expect("Microblock already open and processing");

        let mut considered = self.considered.take()
            .expect("Microblock already open and processing");

        let mut bytes_so_far = self.bytes_so_far;

        let result = mem_pool.iterate_candidates(
            &self.anchor_block_bhh, &self.anchor_block, self.anchor_block_height, &mut self.header_reader,
            |micro_txs| {
                for mempool_tx in micro_txs.into_iter() {
                    if mempool_tx.tx.anchor_mode != TransactionAnchorMode::OffChainOnly && mempool_tx.tx.anchor_mode != TransactionAnchorMode::Any {
                        continue;
                    }
                    if considered.contains(&mempool_tx.metadata.txid) {
                        continue;
                    } else {
                        considered.insert(mempool_tx.metadata.txid.clone());
                    }
                    if bytes_so_far + mempool_tx.metadata.len >= MAX_EPOCH_SIZE.into() {
                        return Err(Error::BlockTooBigError);
                    }
                    match StacksChainState::process_transaction(&mut clarity_tx, &mempool_tx.tx) {
                        Ok(_) => {
                            bytes_so_far += mempool_tx.metadata.len;
                            txs_to_broadcast.push(mempool_tx.tx);
                        },
                        Err(e) => {
                            match e {
                                Error::CostOverflowError(cost_before, cost_after, total_budget) => { 
                                    warn!("Transaction {} reached block cost {}; budget was {}", mempool_tx.tx.txid(), &cost_after, &total_budget);
                                    clarity_tx.reset_cost(cost_before.clone());
                                },
                                _ => {
                                    warn!("Error processing TX {}: {}", mempool_tx.tx.txid(), e);
                                }
                            }
                        }
                    }
                }
                Ok(())
            });

        self.bytes_so_far = bytes_so_far;
        self.clarity_tx.replace(clarity_tx);
        self.considered.replace(considered);

        match result {
            Ok(_) => {},
            Err(Error::BlockTooBigError) => {
                info!("Block budget reached with microblocks");
            },
            Err(e) => {
                warn!("Error producing microblock: {}", e);
                return Err(e);
            }
        }

        if txs_to_broadcast.len() == 0 {
            return Err(Error::NoTransactionsToMine)
        }

        let txid_vecs = txs_to_broadcast
            .iter()
            .map(|tx| tx.txid().as_bytes().to_vec())
            .collect();

        let merkle_tree = MerkleTree::<Sha512Trunc256Sum>::new(&txid_vecs);
        let tx_merkle_root = merkle_tree.root();
        let mut next_microblock_header =
            if let Some(ref prev_microblock) = self.prev_microblock_header {
                StacksMicroblockHeader::from_parent_unsigned(prev_microblock, &tx_merkle_root)
                    .ok_or(Error::MicroblockStreamTooLongError)?
            } else {
                // .prev_block is the hash of the parent anchored block
                StacksMicroblockHeader::first_unsigned(&self.anchor_block, &tx_merkle_root)
            };

        next_microblock_header.sign(miner_key).unwrap();
        next_microblock_header.verify(miner_pubkey_hash).unwrap();

        self.prev_microblock_header = Some(next_microblock_header.clone());

        let microblock = StacksMicroblock {
            header: next_microblock_header,
            txs: txs_to_broadcast
        };
        
        debug!("\n\nMiner: Mined microblock block {} (seq={}): {} transaction(s)\n",
               microblock.block_hash(), microblock.header.sequence, microblock.txs.len());
        Ok(microblock)
    }
}

impl <'a> Drop for StacksMicroblockBuilder<'a> {
    fn drop(&mut self) {
        self.clarity_tx.take().expect("Attempted to reclose closed microblock builder")
            .rollback_block()
    }
}

impl StacksBlockBuilder {
    fn from_parent_pubkey_hash(miner_id: usize, parent_chain_tip: &StacksHeaderInfo, total_work: &StacksWorkScore, proof: &VRFProof, pubkh: Hash160) -> StacksBlockBuilder {
        let header = StacksBlockHeader::from_parent_empty(&parent_chain_tip.anchored_header, parent_chain_tip.microblock_tail.as_ref(), total_work, proof, &pubkh);

        let mut header_bytes = vec![];
        header.consensus_serialize(&mut header_bytes).expect("FATAL: failed to serialize to vec");
        let bytes_so_far = header_bytes.len() as u64;
        
        StacksBlockBuilder {
            chain_tip: parent_chain_tip.clone(),
            header: header,
            txs: vec![],
            micro_txs: vec![],
            total_anchored_fees: 0,
            total_confirmed_streamed_fees: 0,
            total_streamed_fees: 0,
            bytes_so_far: bytes_so_far,
            anchored_done: false,
            prev_microblock_header: StacksMicroblockHeader::first_unsigned(&EMPTY_MICROBLOCK_PARENT_HASH, &Sha512Trunc256Sum([0u8; 32])),       // will be updated
            miner_privkey: StacksPrivateKey::new(),      // caller should overwrite this, or refrain from mining microblocks
            miner_payouts: None,
            miner_id: miner_id
        }
    }

    pub fn from_parent(miner_id: usize, parent_chain_tip: &StacksHeaderInfo, total_work: &StacksWorkScore, proof: &VRFProof, microblock_privkey: &StacksPrivateKey) -> StacksBlockBuilder {
        let mut pubk = StacksPublicKey::from_private(microblock_privkey);
        pubk.set_compressed(true);
        let pubkh = Hash160::from_data(&pubk.to_bytes());

        let mut builder = StacksBlockBuilder::from_parent_pubkey_hash(miner_id, parent_chain_tip, total_work, proof, pubkh);
        builder.miner_privkey = microblock_privkey.clone();
        builder
    }

    fn first_pubkey_hash(miner_id: usize, genesis_burn_header_hash: &BurnchainHeaderHash, genesis_burn_header_timestamp: u64, proof: &VRFProof, pubkh: Hash160) -> StacksBlockBuilder {
        let genesis_chain_tip = StacksHeaderInfo {
            anchored_header: StacksBlockHeader::genesis_block_header(),
            microblock_tail: None,
            block_height: 0,
            index_root: TrieHash([0u8; 32]),
            burn_header_hash: genesis_burn_header_hash.clone(),
            burn_header_timestamp: genesis_burn_header_timestamp
        };

        let mut builder = StacksBlockBuilder::from_parent_pubkey_hash(miner_id, &genesis_chain_tip, &StacksWorkScore::initial(), proof, pubkh);
        builder.header.parent_block = EMPTY_MICROBLOCK_PARENT_HASH.clone();
        builder
    }
    
    pub fn first(miner_id: usize, genesis_burn_header_hash: &BurnchainHeaderHash, genesis_burn_header_timestamp: u64, proof: &VRFProof, microblock_privkey: &StacksPrivateKey) -> StacksBlockBuilder {
        let mut pubk = StacksPublicKey::from_private(microblock_privkey);
        pubk.set_compressed(true);
        let pubkh = Hash160::from_data(&pubk.to_bytes());

        let mut builder = StacksBlockBuilder::first_pubkey_hash(miner_id, genesis_burn_header_hash, genesis_burn_header_timestamp, proof, pubkh);
        builder.miner_privkey = microblock_privkey.clone();
        builder
    }

    /// Assign the block parent
    pub fn set_parent_block(&mut self, parent_block_hash: &BlockHeaderHash) -> () {
        self.header.parent_block = parent_block_hash.clone();
    }

    /// Assign the anchored block's parent microblock (used for testing orphaning)
    pub fn set_parent_microblock(&mut self, parent_mblock_hash: &BlockHeaderHash, parent_mblock_seq: u16) -> () {
        self.header.parent_microblock = parent_mblock_hash.clone();
        self.header.parent_microblock_sequence = parent_mblock_seq;
    }

    /// Reset measured costs and fees
    pub fn reset_costs(&mut self) -> () {
        self.total_anchored_fees = 0;
        self.total_confirmed_streamed_fees = 0;
        self.total_streamed_fees = 0;
    }

    /// Append a transaction if doing so won't exceed the epoch data size.
    /// Errors out if we exceed budget, or the transaction is invalid.
    pub fn try_mine_tx(&mut self, clarity_tx: &mut ClarityTx, tx: &StacksTransaction) -> Result<(), Error> {
        let tx_len = tx.tx_len();
        self.try_mine_tx_with_len(clarity_tx, tx, tx_len)
    }

    /// Append a transaction if doing so won't exceed the epoch data size.
    /// Errors out if we exceed budget, or the transaction is invalid.
    pub fn try_mine_tx_with_len(&mut self, clarity_tx: &mut ClarityTx, tx: &StacksTransaction, tx_len: u64) -> Result<(), Error> {        
        if self.bytes_so_far + tx_len >= MAX_EPOCH_SIZE.into() {
            return Err(Error::BlockTooBigError);
        }

        if !self.anchored_done {
            // building up the anchored blocks
            if tx.anchor_mode != TransactionAnchorMode::OnChainOnly && tx.anchor_mode != TransactionAnchorMode::Any {
                return Err(Error::InvalidStacksTransaction("Invalid transaction anchor mode for anchored data".to_string()));
            }

            let (fee, _receipt) = StacksChainState::process_transaction(clarity_tx, tx)
                .map_err(|e| {
                    match e {
                        Error::CostOverflowError(cost_before, cost_after, total_budget) => { 
                            warn!("Transaction {} reached block cost {}; budget was {}", tx.txid(), &cost_after, &total_budget);
                            clarity_tx.reset_cost(cost_before);
                            Error::BlockTooBigError
                        },
                        _ => e
                    }
                })?;

            // save
            self.txs.push(tx.clone());
            self.total_anchored_fees += fee;
        }
        else {
            // building up the microblocks
            if tx.anchor_mode != TransactionAnchorMode::OffChainOnly && tx.anchor_mode != TransactionAnchorMode::Any {
                return Err(Error::InvalidStacksTransaction("Invalid transaction anchor mode for streamed data".to_string()));
            }
            
            let (fee, _receipt) = StacksChainState::process_transaction(clarity_tx, tx)
                .map_err(|e| {
                    match e {
                        Error::CostOverflowError(cost_before, cost_after, total_budget) => { 
                            warn!("Transaction {} reached block cost {}; budget was {}", tx.txid(), &cost_after, &total_budget);
                            clarity_tx.reset_cost(cost_before);
                            Error::BlockTooBigError
                        },
                        _ => e
                    }
                })?;

            // save
            self.micro_txs.push(tx.clone());
            self.total_streamed_fees += fee;
        }

        self.bytes_so_far += tx_len;
        Ok(())
    }
    
    /// Append a transaction if doing so won't exceed the epoch data size.
    /// Does not check for errors
    #[cfg(test)]
    pub fn force_mine_tx<'a>(&mut self, clarity_tx: &mut ClarityTx<'a>, tx: &StacksTransaction) -> Result<(), Error> {
        let mut tx_bytes = vec![];
        tx.consensus_serialize(&mut tx_bytes).map_err(Error::NetError)?;
        let tx_len = tx_bytes.len() as u64;
        
        if self.bytes_so_far + tx_len >= MAX_EPOCH_SIZE.into() {
            warn!("Epoch size is {} >= {}", self.bytes_so_far + tx_len, MAX_EPOCH_SIZE);
        }
        
        if !self.anchored_done {
            // save
            match StacksChainState::process_transaction(clarity_tx, tx) {
                Ok((fee, receipt)) => {
                    self.total_anchored_fees += fee;
                },
                Err(e) => {
                    warn!("Invalid transaction {} in anchored block, but forcing inclusion (error: {:?})", &tx.txid(), &e);
                }
            }

            self.txs.push(tx.clone());
        }
        else {
            match StacksChainState::process_transaction(clarity_tx, tx) {
                Ok((fee, receipt)) => {
                    self.total_streamed_fees += fee;
                },
                Err(e) => {
                    warn!("Invalid transaction {} in microblock, but forcing inclusion (error: {:?})", &tx.txid(), &e);
                }
            }
            
            self.micro_txs.push(tx.clone());
        }

        self.bytes_so_far += tx_len;
        Ok(())
    }

    /// Finish building the anchored block.
    /// TODO: expand to deny mining a block whose anchored static checks fail (and allow the caller
    /// to disable this, in order to test mining invalid blocks)
    pub fn mine_anchored_block<'a>(&mut self, clarity_tx: &mut ClarityTx<'a>) -> StacksBlock {
        assert!(!self.anchored_done);

        // add miner payments
        if let Some(ref mature_miner_rewards) = self.miner_payouts {
            // grant in order by miner, then users
            StacksChainState::process_matured_miner_rewards(clarity_tx, mature_miner_rewards)
                .expect("FATAL: failed to process miner rewards");
        }

        let txid_vecs = self.txs
            .iter()
            .map(|tx| tx.txid().as_bytes().to_vec())
            .collect();

        let merkle_tree = MerkleTree::<Sha512Trunc256Sum>::new(&txid_vecs);
        let tx_merkle_root = merkle_tree.root();
        let state_root_hash = clarity_tx.get_root_hash();

        self.header.tx_merkle_root = tx_merkle_root;
        self.header.state_index_root = state_root_hash;
        
        let block = StacksBlock {
            header: self.header.clone(),
            txs: self.txs.clone()
        };

        self.prev_microblock_header = StacksMicroblockHeader::first_unsigned(&block.block_hash(), &Sha512Trunc256Sum([0u8; 32]));

        self.prev_microblock_header.prev_block = block.block_hash();
        self.anchored_done = true;

        test_debug!("\n\nMiner {}: Mined anchored block {}, {} transactions, state root is {}\n", self.miner_id, block.block_hash(), block.txs.len(), state_root_hash);

        info!("Miner: mined anchored block {}, parent block {}", block.block_hash(), &self.header.parent_block);

        block
    }

    /// Cut the next microblock.
    pub fn mine_next_microblock<'a>(&mut self) -> Result<StacksMicroblock, Error> {
        let txid_vecs = self.micro_txs
            .iter()
            .map(|tx| tx.txid().as_bytes().to_vec())
            .collect();

        let merkle_tree = MerkleTree::<Sha512Trunc256Sum>::new(&txid_vecs);
        let tx_merkle_root = merkle_tree.root();
        let mut next_microblock_header =
            if self.prev_microblock_header.tx_merkle_root == Sha512Trunc256Sum([0u8; 32]) {
                // .prev_block is the hash of the parent anchored block
                StacksMicroblockHeader::first_unsigned(&self.prev_microblock_header.prev_block, &tx_merkle_root)
            }
            else {
                StacksMicroblockHeader::from_parent_unsigned(&self.prev_microblock_header, &tx_merkle_root)
                    .ok_or(Error::MicroblockStreamTooLongError)?
            };

        test_debug!("Sign with {}", self.miner_privkey.to_hex());

        next_microblock_header.sign(&self.miner_privkey).unwrap();
        next_microblock_header.verify(&self.header.microblock_pubkey_hash).unwrap();

        self.prev_microblock_header = next_microblock_header.clone();

        let microblock = StacksMicroblock {
            header: next_microblock_header,
            txs: self.micro_txs.clone()
        };

        self.micro_txs.clear();
        
        test_debug!("\n\nMiner {}: Mined microblock block {} (seq={}): {} transaction(s)\n", self.miner_id, microblock.block_hash(), microblock.header.sequence, microblock.txs.len());
        Ok(microblock)
    }

    /// Begin mining an epoch's transactions.
    /// NOTE: even though we don't yet know the block hash, the Clarity VM ensures that a
    /// transaction can't query information about the _current_ block (i.e. information that is not
    /// yet known).
    pub fn epoch_begin<'a>(&mut self, chainstate: &'a mut StacksChainState) -> Result<ClarityTx<'a>, Error> {
        // find matured miner rewards, so we can grant them within the Clarity DB tx.
        let matured_miner_rewards_opt = {
            let mut tx = chainstate.headers_tx_begin()?;
            StacksChainState::find_mature_miner_rewards(&mut tx, &self.chain_tip, None)?
        };

        self.miner_payouts = matured_miner_rewards_opt;
        
        // there's no way the miner can learn either the burn block hash or the stacks block hash,
        // so use a sentinel hash value for each that will never occur in practice.
        let new_burn_hash = MINER_BLOCK_BURN_HEADER_HASH.clone();
        let new_block_hash = MINER_BLOCK_HEADER_HASH.clone();

        test_debug!("\n\nMiner {} epoch begin off of {}/{}\n", self.miner_id, self.chain_tip.burn_header_hash, self.header.parent_block);

        if let Some(ref _payout) = self.miner_payouts {
            test_debug!("Miner payout to process: {:?}", _payout);
        }

        let parent_burn_header_hash = self.chain_tip.burn_header_hash.clone();
        let parent_header_hash = self.header.parent_block.clone();
        
        // apply all known parent microblocks before beginning our tenure 
        let parent_microblocks = match StacksChainState::load_staging_microblock_stream(&chainstate.blocks_db, &chainstate.blocks_path, &parent_burn_header_hash, &parent_header_hash, u16::max_value())? {
            Some(mblocks) => mblocks,
            None => vec![]
        };

        let mut tx = chainstate.block_begin(&parent_burn_header_hash, &parent_header_hash, &new_burn_hash, &new_block_hash);

        test_debug!("Miner {}: Apply {} parent microblocks", self.miner_id, parent_microblocks.len());

        if parent_microblocks.len() == 0 {
            self.set_parent_microblock(&EMPTY_MICROBLOCK_PARENT_HASH, 0);
        }
        else {
            match StacksChainState::process_microblocks_transactions(&mut tx, &parent_microblocks) {
                Ok((fees, ..)) => {
                    self.total_confirmed_streamed_fees += fees as u64;
                },
                Err((e, mblock_header_hash)) => {
                    let msg = format!("Invalid Stacks microblocks {},{} (offender {}): {:?}", parent_burn_header_hash, parent_header_hash, mblock_header_hash, &e);
                    warn!("{}", &msg);

                    return Err(Error::InvalidStacksMicroblock(msg, mblock_header_hash));
                }
            };
            let num_mblocks = parent_microblocks.len();
            let last_mblock_hdr = parent_microblocks[num_mblocks-1].header.clone();
            self.set_parent_microblock(&last_mblock_hdr.block_hash(), last_mblock_hdr.sequence);
        };
        
        test_debug!("Miner {}: Finished applying {} parent microblocks\n", self.miner_id, parent_microblocks.len());

        Ok(tx)
    }

    /// Finish up mining an epoch's transactions
    pub fn epoch_finish(self, tx: ClarityTx) -> ExecutionCost {
        let new_burn_hash = MINER_BLOCK_BURN_HEADER_HASH.clone();
        let new_block_hash = MINER_BLOCK_HEADER_HASH.clone();

        let index_block_hash = StacksBlockHeader::make_index_block_hash(&new_burn_hash, &new_block_hash);

        // clear out the block trie we just created, so the block validator logic doesn't step all
        // over it.
        //        let moved_name = format!("{}.mined", index_block_hash);

        // write out the trie...
        let consumed = tx.commit_mined_block(&index_block_hash);

        test_debug!("\n\nMiner {}: Finished mining child of {}/{}. Trie is in mined_blocks table.\n", self.miner_id, self.chain_tip.burn_header_hash, self.chain_tip.anchored_header.block_hash());

        consumed
    }
    
    /// Unconditionally build an anchored block from a list of transactions.
    /// Used when we are re-building a valid block after we exceed budget
    fn make_anchored_block_from_txs(mut builder: StacksBlockBuilder, chainstate: &mut StacksChainState, mut txs: Vec<StacksTransaction>) -> Result<StacksBlock, Error> {
        debug!("Build anchored block from {} transactions", txs.len());
        let mut epoch_tx = builder.epoch_begin(chainstate)?;
        for tx in txs.drain(..) {
            builder.try_mine_tx(&mut epoch_tx, &tx)?;
        }
        let block = builder.mine_anchored_block(&mut epoch_tx);
        builder.epoch_finish(epoch_tx);
        Ok(block)
    }

    /// Create a block builder for mining
    fn make_block_builder(stacks_parent_header: &StacksHeaderInfo,
                          proof: VRFProof,
                          total_burn: u64,
                          pubkey_hash: Hash160) -> Result<StacksBlockBuilder, Error> {

        let builder = 
            if stacks_parent_header.burn_header_hash == FIRST_BURNCHAIN_BLOCK_HASH {
                StacksBlockBuilder::first_pubkey_hash(0, &FIRST_BURNCHAIN_BLOCK_HASH, FIRST_BURNCHAIN_BLOCK_TIMESTAMP, &proof, pubkey_hash)
            }
            else {
                // building off an existing stacks block
                let new_work = StacksWorkScore {
                    burn: total_burn,
                    work: stacks_parent_header.block_height.checked_add(1).expect("FATAL: block height overflow")
                };

                StacksBlockBuilder::from_parent_pubkey_hash(0, stacks_parent_header, &new_work, &proof, pubkey_hash)
            };

        Ok(builder)
    }

    /// Given access to the mempool, mine an anchored block with no more than the given execution cost.
    ///   returns the assembled block, and the consumed execution budget.
    pub fn build_anchored_block(chainstate_handle: &StacksChainState,       // not directly used; used as a handle to open other chainstates
                                mempool: &MemPoolDB,
                                parent_stacks_header: &StacksHeaderInfo,    // Stacks header we're building off of
                                total_burn: u64,                            // the burn so far on the burnchain (i.e. from the last burnchain block)
                                proof: VRFProof,                            // proof over the burnchain's last seed
                                pubkey_hash: Hash160,
                                coinbase_tx: &StacksTransaction,
                                execution_budget: ExecutionCost) -> Result<(StacksBlock, ExecutionCost, u64), Error> {

        if let TransactionPayload::Coinbase(..) = coinbase_tx.payload {} else {
            return Err(Error::MemPoolError("Not a coinbase transaction".to_string()));
        }

        let (tip_burn_header_hash, tip_block_hash, tip_height) = (parent_stacks_header.burn_header_hash.clone(), parent_stacks_header.anchored_header.block_hash(), parent_stacks_header.block_height);

        debug!("Build anchored block off of {}/{} height {}", &tip_burn_header_hash, &tip_block_hash, tip_height); 
        
        let mut header_reader_chainstate = chainstate_handle.reopen()?;            // used for reading block headers during an epoch
        let mut chainstate = chainstate_handle.reopen_limited(execution_budget)?;  // used for processing a block up to the given limit

        let mut builder = StacksBlockBuilder::make_block_builder(parent_stacks_header, proof, total_burn, pubkey_hash)?;

        let mut epoch_tx = builder.epoch_begin(&mut chainstate)?;
        builder.try_mine_tx(&mut epoch_tx, coinbase_tx)?;

        let mut considered = HashSet::new();        // txids of all transactions we looked at
        let mut mined_origin_nonces : HashMap<StacksAddress, u64> = HashMap::new();     // map addrs of mined transaction origins to the nonces we used
        let mut mined_sponsor_nonces : HashMap<StacksAddress, u64> = HashMap::new();    // map addrs of mined transaction sponsors to the nonces we used

        let result = mempool.iterate_candidates(&tip_burn_header_hash, &tip_block_hash, tip_height, &mut header_reader_chainstate, |available_txs| {
            for txinfo in available_txs.into_iter() {
                // skip transactions early if we can
                if considered.contains(&txinfo.tx.txid()) {
                    continue;
                }
                if let Some(nonce) = mined_origin_nonces.get(&txinfo.tx.origin_address()) {
                    if *nonce >= txinfo.tx.get_origin_nonce() {
                        continue;
                    }
                }
                if let Some(sponsor_addr) = txinfo.tx.sponsor_address() {
                    if let Some(nonce) = mined_sponsor_nonces.get(&sponsor_addr) {
                        if let Some(sponsor_nonce) = txinfo.tx.get_sponsor_nonce() {
                            if *nonce >= sponsor_nonce {
                                continue;
                            }
                        }
                    }
                }

                considered.insert(txinfo.tx.txid());

                match builder.try_mine_tx_with_len(&mut epoch_tx, &txinfo.tx, txinfo.metadata.len) {
                    Ok(_) => {},
                    Err(Error::BlockTooBigError) => {
                        // done mining -- our execution budget is exceeded.
                        // Make the block from the transactions we did manage to get
                        debug!("Block budget exceeded on tx {}", &txinfo.tx.txid());
                    },
                    Err(e) => {
                        warn!("Failed to apply tx {}: {:?}", &txinfo.tx.txid(), &e);
                        continue;
                    }
                }

                mined_origin_nonces.insert(txinfo.tx.origin_address(), txinfo.tx.get_origin_nonce());
                if let (Some(sponsor_addr), Some(sponsor_nonce)) = (txinfo.tx.sponsor_address(), txinfo.tx.get_sponsor_nonce()) {
                    mined_sponsor_nonces.insert(sponsor_addr, sponsor_nonce);
                }
            }
            Ok(())
        });

        match result {
            Ok(_) => {},
            Err(e) => {
                warn!("Failure building block: {}", e);
                epoch_tx.rollback_block();
                return Err(e);
            },
        }

        // the prior do_rebuild logic wasn't necessary
        // a transaction that caused a budget exception is rolled back in process_transaction

        // save the block so we can build microblocks off of it
        let block = builder.mine_anchored_block(&mut epoch_tx);
        let size = builder.bytes_so_far;
        let consumed = builder.epoch_finish(epoch_tx);
        Ok((block, consumed, size))
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    use std::collections::HashMap;
    use std::collections::HashSet;
    use std::collections::VecDeque;
    use address::*;
    use chainstate::stacks::*;
    use chainstate::stacks::db::*;
    use chainstate::stacks::db::test::*;
    use chainstate::burn::*;
    use chainstate::burn::db::burndb::*;
    use chainstate::burn::operations::{
        LeaderBlockCommitOp,
        LeaderKeyRegisterOp,
        UserBurnSupportOp,
        BlockstackOperation,
        BlockstackOperationType
    };

    use burnchains::*;
    use burnchains::test::*;

    use util::vrf::VRFProof;

    use vm::types::*;

    use rand::Rng;
    use rand::thread_rng;
    use rand::seq::SliceRandom;

    use net::test::*;

    use util::sleep_ms;

    pub const COINBASE : u128 = 500 * 1_000_000;

    pub fn coinbase_total_at(stacks_height: u64) -> u128 {
        if stacks_height > MINER_REWARD_MATURITY + MINER_REWARD_WINDOW {
            COINBASE * ((stacks_height - MINER_REWARD_MATURITY - MINER_REWARD_WINDOW) as u128)
        }
        else {
            0
        }
    }

    pub fn path_join(dir: &str, path: &str) -> String {
        // force path to be relative
        let tail = 
            if !path.starts_with("/") {
                path.to_string()
            }
            else {
                String::from_utf8(path.as_bytes()[1..].to_vec()).unwrap()
            };

        let p = PathBuf::from(dir);
        let res = p.join(PathBuf::from(tail));
        res.to_str().unwrap().to_string()
    }

    // copy src to dest
    pub fn copy_dir(src_dir: &str, dest_dir: &str) -> Result<(), io::Error> {
        eprintln!("Copy directory {} to {}", src_dir, dest_dir);

        let mut dir_queue = VecDeque::new();
        dir_queue.push_back("/".to_string());

        while dir_queue.len() > 0 {
            let next_dir = dir_queue.pop_front().unwrap();
            let next_src_dir = path_join(&src_dir, &next_dir);
            let next_dest_dir = path_join(&dest_dir, &next_dir);

            eprintln!("mkdir {}", &next_dest_dir);
            fs::create_dir_all(&next_dest_dir)?;

            for dirent_res in fs::read_dir(&next_src_dir)? {
                let dirent = dirent_res?;
                let path = dirent.path();
                let md = fs::metadata(&path)?;
                if md.is_dir() {
                    let frontier = path_join(&next_dir, &dirent.file_name().to_str().unwrap());
                    eprintln!("push {}", &frontier);
                    dir_queue.push_back(frontier);
                }
                else {
                    let dest_path = path_join(&next_dest_dir, &dirent.file_name().to_str().unwrap());
                    eprintln!("copy {} to {}", &path.to_str().unwrap(), &dest_path);
                    fs::copy(path, dest_path)?;
                }
            }
        }
        Ok(())
    }

    // one point per round
    pub struct TestMinerTracePoint {
        pub fork_snapshots: HashMap<usize, BlockSnapshot>,  // map miner ID to snapshot
        pub stacks_blocks: HashMap<usize, StacksBlock>,     // map miner ID to stacks block 
        pub microblocks: HashMap<usize, Vec<StacksMicroblock>>,  // map miner ID to microblocks
        pub block_commits: HashMap<usize, LeaderBlockCommitOp>,  // map miner ID to block commit
        pub miner_node_map: HashMap<usize, String>,              // map miner ID to the node it worked on
    }

    impl TestMinerTracePoint {
        pub fn new() -> TestMinerTracePoint {
            TestMinerTracePoint {
                fork_snapshots: HashMap::new(),
                stacks_blocks: HashMap::new(),
                microblocks: HashMap::new(),
                block_commits: HashMap::new(),
                miner_node_map: HashMap::new()
            }
        }

        pub fn add(&mut self, miner_id: usize, node_name: String, fork_snapshot: BlockSnapshot, stacks_block: StacksBlock, microblocks: Vec<StacksMicroblock>, block_commit: LeaderBlockCommitOp) -> () {
            self.fork_snapshots.insert(miner_id, fork_snapshot);
            self.stacks_blocks.insert(miner_id, stacks_block);
            self.microblocks.insert(miner_id, microblocks);
            self.block_commits.insert(miner_id, block_commit);
            self.miner_node_map.insert(miner_id, node_name);
        }

        pub fn get_block_snapshot(&self, miner_id: usize) -> Option<BlockSnapshot> {
            self.fork_snapshots.get(&miner_id).cloned()
        }

        pub fn get_stacks_block(&self, miner_id: usize) -> Option<StacksBlock> {
            self.stacks_blocks.get(&miner_id).cloned()
        }
        
        pub fn get_microblocks(&self, miner_id: usize) -> Option<Vec<StacksMicroblock>> {
            self.microblocks.get(&miner_id).cloned()
        }

        pub fn get_block_commit(&self, miner_id: usize) -> Option<LeaderBlockCommitOp> {
            self.block_commits.get(&miner_id).cloned()
        }

        pub fn get_node_name(&self, miner_id: usize) -> Option<String> {
            self.miner_node_map.get(&miner_id).cloned()
        }

        pub fn get_miner_ids(&self) -> Vec<usize> {
            let mut miner_ids = HashSet::new();
            for miner_id in self.fork_snapshots.keys() {
                miner_ids.insert(*miner_id);
            }
            for miner_id in self.stacks_blocks.keys() {
                miner_ids.insert(*miner_id);
            }
            for miner_id in self.microblocks.keys() {
                miner_ids.insert(*miner_id);
            }
            for miner_id in self.block_commits.keys() {
                miner_ids.insert(*miner_id);
            }
            let mut ret = vec![];
            for miner_id in miner_ids.iter() {
                ret.push(*miner_id);
            }
            ret
        }
    }

    pub struct TestMinerTrace {
        pub points: Vec<TestMinerTracePoint>,
        pub burn_node: TestBurnchainNode,
        pub miners: Vec<TestMiner>,
    }

    impl TestMinerTrace {
        pub fn new(burn_node: TestBurnchainNode, miners: Vec<TestMiner>, points: Vec<TestMinerTracePoint>) -> TestMinerTrace {
            TestMinerTrace {
                points: points,
                burn_node: burn_node,
                miners: miners,
            }
        }

        /// how many blocks represented here?
        pub fn get_num_blocks(&self) -> usize {
            let mut num_blocks = 0;
            for p in self.points.iter() {
                for miner_id in p.stacks_blocks.keys() {
                    if p.stacks_blocks.get(miner_id).is_some() {
                        num_blocks += 1;
                    }
                }
            }
            num_blocks
        }
        
        /// how many sortitions represented here?
        pub fn get_num_sortitions(&self) -> usize {
            let mut num_sortitions = 0;
            for p in self.points.iter() {
                for miner_id in p.fork_snapshots.keys() {
                    if p.fork_snapshots.get(miner_id).is_some() {
                        num_sortitions += 1;
                    }
                }
            }
            num_sortitions
        }

        /// how many rounds did this trace go for?
        pub fn rounds(&self) -> usize {
            self.points.len()
        }

        /// what are the chainstate directories?
        pub fn get_test_names(&self) -> Vec<String> {
            let mut all_test_names = HashSet::new();
            for p in self.points.iter() {
                for miner_id in p.miner_node_map.keys() {
                    if let Some(ref test_name) = p.miner_node_map.get(miner_id) {
                        if !all_test_names.contains(test_name) {
                            all_test_names.insert(test_name.clone());
                        }
                    }
                }
            }
            let mut ret = vec![];
            for name in all_test_names.drain() {
                ret.push(name.to_owned());
            }
            ret
        }
    }

    pub struct TestStacksNode {
        pub chainstate: StacksChainState,
        pub prev_keys: Vec<LeaderKeyRegisterOp>,        // _all_ keys generated
        pub key_ops: HashMap<VRFPublicKey, usize>,      // map VRF public keys to their locations in the prev_keys array
        pub anchored_blocks: Vec<StacksBlock>,
        pub microblocks: Vec<Vec<StacksMicroblock>>,
        pub commit_ops: HashMap<BlockHeaderHash, usize>,
        pub test_name: String,
        forkable: bool
    }

    impl TestStacksNode {
        pub fn new(mainnet: bool, chain_id: u32, test_name: &str) -> TestStacksNode {
            let chainstate = instantiate_chainstate(mainnet, chain_id, test_name);
            TestStacksNode {
                chainstate: chainstate,
                prev_keys: vec![],
                key_ops: HashMap::new(),
                anchored_blocks: vec![],
                microblocks: vec![],
                commit_ops: HashMap::new(),
                test_name: test_name.to_string(),
                forkable: true
            }
        }
        
        pub fn open(mainnet: bool, chain_id: u32, test_name: &str) -> TestStacksNode {
            let chainstate = open_chainstate(mainnet, chain_id, test_name);
            TestStacksNode {
                chainstate: chainstate,
                prev_keys: vec![],
                key_ops: HashMap::new(),
                anchored_blocks: vec![],
                microblocks: vec![],
                commit_ops: HashMap::new(),
                test_name: test_name.to_string(),
                forkable: true,
            }
        }

        pub fn from_chainstate(chainstate: StacksChainState) -> TestStacksNode {
            TestStacksNode {
                chainstate: chainstate,
                prev_keys: vec![],
                key_ops: HashMap::new(),
                anchored_blocks: vec![],
                microblocks: vec![],
                commit_ops: HashMap::new(),
                test_name: "".to_string(),
                forkable: false
            }
        }

        // NOTE: can't do this if instatniated via from_chainstate()
        pub fn fork(&self, new_test_name: &str) -> TestStacksNode {
            if !self.forkable {
                panic!("Tried to fork an unforkable chainstate instance");
            }
            
            match fs::metadata(&chainstate_path(new_test_name)) {
                Ok(_) => {
                    fs::remove_dir_all(&chainstate_path(new_test_name)).unwrap();
                },
                Err(_) => {}
            }

            copy_dir(&chainstate_path(&self.test_name), &chainstate_path(new_test_name)).unwrap();
            let chainstate = open_chainstate(self.chainstate.mainnet, self.chainstate.chain_id, new_test_name);
            TestStacksNode {
                chainstate: chainstate,
                prev_keys: self.prev_keys.clone(),
                key_ops: self.key_ops.clone(),
                anchored_blocks: self.anchored_blocks.clone(),
                microblocks: self.microblocks.clone(),
                commit_ops: self.commit_ops.clone(),
                test_name: new_test_name.to_string(),
                forkable: true
            }
        }

        pub fn next_burn_block(burndb: &mut BurnDB, fork: &mut TestBurnchainFork) -> TestBurnchainBlock {
            let burn_block = {
                let ic = burndb.index_conn();
                fork.next_block(&ic)
            };
            burn_block
        }

        pub fn add_key_register(&mut self, block: &mut TestBurnchainBlock, miner: &mut TestMiner) -> LeaderKeyRegisterOp {
            let key_register_op = block.add_leader_key_register(miner);
            self.prev_keys.push(key_register_op.clone());
            self.key_ops.insert(key_register_op.public_key.clone(), self.prev_keys.len()-1);
            key_register_op
        }

        pub fn add_key_register_op(&mut self, op: &LeaderKeyRegisterOp) -> () {
            self.prev_keys.push(op.clone());
            self.key_ops.insert(op.public_key.clone(), self.prev_keys.len()-1);
        }

        pub fn add_block_commit(burndb: &mut BurnDB, burn_block: &mut TestBurnchainBlock, miner: &mut TestMiner, block_hash: &BlockHeaderHash, burn_amount: u64, key_op: &LeaderKeyRegisterOp, parent_block_snapshot: Option<&BlockSnapshot>) -> LeaderBlockCommitOp {
            let block_commit_op = {
                let ic = burndb.index_conn();
                let parent_snapshot = burn_block.parent_snapshot.clone();
                burn_block.add_leader_block_commit(&ic, miner, block_hash, burn_amount, key_op, Some(&parent_snapshot), parent_block_snapshot)
            };
            block_commit_op
        }

        pub fn get_last_key(&self, miner: &TestMiner) -> LeaderKeyRegisterOp {
            let last_vrf_pubkey = miner.last_VRF_public_key().unwrap();
            let idx = *self.key_ops.get(&last_vrf_pubkey).unwrap();
            self.prev_keys[idx].clone()
        }

        pub fn get_last_anchored_block(&self, miner: &TestMiner) -> Option<StacksBlock> {
            match miner.last_block_commit() {
                None => None,
                Some(block_commit_op) => match self.commit_ops.get(&block_commit_op.block_header_hash) {
                    None => None,
                    Some(idx) => Some(self.anchored_blocks[*idx].clone())
                }
            }
        }
        
        pub fn get_last_accepted_anchored_block(&self, miner: &TestMiner) -> Option<StacksBlock> {
            for bc in miner.block_commits.iter().rev() {
                if StacksChainState::has_stored_block(&self.chainstate.blocks_db, &self.chainstate.blocks_path, &bc.burn_header_hash, &bc.block_header_hash).unwrap() &&
                  !StacksChainState::is_block_orphaned(&self.chainstate.blocks_db, &bc.burn_header_hash, &bc.block_header_hash).unwrap() {
                    match self.commit_ops.get(&bc.block_header_hash) {
                        None => {
                            continue;
                        }
                        Some(idx) => {
                            return Some(self.anchored_blocks[*idx].clone());
                        }
                    }
                }
            }
            return None;
        }

        pub fn get_microblock_stream(&self, miner: &TestMiner, block_hash: &BlockHeaderHash) -> Option<Vec<StacksMicroblock>> {
            match self.commit_ops.get(block_hash) {
                None => None,
                Some(idx) => Some(self.microblocks[*idx].clone())
            }
        }

        pub fn get_anchored_block(&self, block_hash: &BlockHeaderHash) -> Option<StacksBlock> {
            match self.commit_ops.get(block_hash) {
                None => None,
                Some(idx) => Some(self.anchored_blocks[*idx].clone())
            }
        }

        pub fn get_last_winning_snapshot<'a>(ic: &BurnDBConn<'a>, fork_tip: &BlockSnapshot, miner: &TestMiner) -> Option<BlockSnapshot> {
            for commit_op in miner.block_commits.iter().rev() {
                match BurnDB::get_block_snapshot_for_winning_stacks_block(ic, &fork_tip.burn_header_hash, &commit_op.block_header_hash).unwrap() {
                    Some(sn) => {
                        return Some(sn);
                    }
                    None => {}
                }
            }
            return None;
        }

        pub fn get_miner_status<'a>(clarity_tx: &mut ClarityTx<'a>, addr: &StacksAddress) -> Option<(bool, u128)> {
            let boot_code_address = StacksAddress::from_string(&STACKS_BOOT_CODE_CONTRACT_ADDRESS.to_string()).unwrap();
            let miner_contract_id = QualifiedContractIdentifier::new(StandardPrincipalData::from(boot_code_address.clone()), ContractName::try_from(BOOT_CODE_MINER_CONTRACT_NAME.to_string()).unwrap());
            
            let miner_participant_principal = ClarityName::try_from(BOOT_CODE_MINER_REWARDS_PARTICIPANT.to_string()).unwrap();
            let miner_available_name = ClarityName::try_from(BOOT_CODE_MINER_REWARDS_AVAILABLE.to_string()).unwrap();
            let miner_authorized_name = ClarityName::try_from(BOOT_CODE_MINER_REWARDS_AUTHORIZED.to_string()).unwrap();
            
            let miner_principal = Value::Tuple(TupleData::from_data(vec![
                    (miner_participant_principal, Value::Principal(PrincipalData::Standard(StandardPrincipalData::from(addr.clone()))))])
                .expect("FATAL: failed to construct miner principal key"));

            let miner_status = clarity_tx.with_clarity_db_readonly(|db| {
                let miner_status_opt = db.fetch_entry(&miner_contract_id, BOOT_CODE_MINER_REWARDS_MAP, &miner_principal)
                    .expect("FATAL: Clarity DB Error");
                let miner_status = match miner_status_opt {
                    Value::Optional(ref optional_data) => {
                        match optional_data.data {
                            None => None,
                            Some(ref miner_status) => {
                                match **miner_status {
                                    Value::Tuple(ref tuple) => {
                                        let authorized = match tuple.get(&miner_authorized_name).expect("FATAL: no miner authorized in tuple") {
                                            Value::Bool(ref authorized) => *authorized,
                                            _ => {
                                                panic!("FATAL: miner reward data map is malformed");
                                            }
                                        };

                                        let available = match tuple.get(&miner_available_name).expect("FATAL: no miner available in tuple") {
                                            Value::UInt(ref available) => *available,
                                            _ => {
                                                panic!("FATAL: miner reward data map is malformed");
                                            }
                                        };
                                        
                                        Some((authorized, available))
                                    },
                                    ref x => {
                                        panic!("FATAL: miner status is not a tuple: {:?}", &x);
                                    }
                                }
                            }
                        }
                    },
                    ref x => {
                        panic!("FATAL: fetched miner status it not an optional: {:?}", &x);
                    }
                };
            
                miner_status
            });

            miner_status
        }

        pub fn make_tenure_commitment(&mut self, 
                                      burndb: &mut BurnDB, 
                                      burn_block: &mut TestBurnchainBlock, 
                                      miner: &mut TestMiner, 
                                      stacks_block: &StacksBlock,
                                      microblocks: &Vec<StacksMicroblock>,
                                      burn_amount: u64, 
                                      miner_key: &LeaderKeyRegisterOp, 
                                      parent_block_snapshot_opt: Option<&BlockSnapshot>) -> LeaderBlockCommitOp {

            self.anchored_blocks.push(stacks_block.clone());
            self.microblocks.push(microblocks.clone());
            
            test_debug!("Miner {}: Commit to stacks block {} (work {},{})", miner.id, stacks_block.block_hash(), stacks_block.header.total_work.burn, stacks_block.header.total_work.work);

            // send block commit for this block
            let block_commit_op = TestStacksNode::add_block_commit(burndb, burn_block, miner, &stacks_block.block_hash(), burn_amount, miner_key, parent_block_snapshot_opt);
            
            test_debug!("Miner {}: Block commit transaction builds on {},{} (parent snapshot is {:?})", miner.id, block_commit_op.parent_block_ptr, block_commit_op.parent_vtxindex, &parent_block_snapshot_opt);
            self.commit_ops.insert(block_commit_op.block_header_hash.clone(), self.anchored_blocks.len()-1);
            block_commit_op
        }

        pub fn mine_stacks_block<F>(&mut self,
                                    burndb: &mut BurnDB,
                                    miner: &mut TestMiner, 
                                    burn_block: &mut TestBurnchainBlock, 
                                    miner_key: &LeaderKeyRegisterOp, 
                                    parent_stacks_block: Option<&StacksBlock>, 
                                    burn_amount: u64,
                                    block_assembler: F) -> (StacksBlock, Vec<StacksMicroblock>, LeaderBlockCommitOp) 
        where
            F: FnOnce(StacksBlockBuilder, &mut TestMiner) -> (StacksBlock, Vec<StacksMicroblock>)
        {
            let proof = miner.make_proof(&miner_key.public_key, &burn_block.parent_snapshot.sortition_hash)
                .expect(&format!("FATAL: no private key for {}", miner_key.public_key.to_hex()));

            let (builder, parent_block_snapshot_opt) = match parent_stacks_block {
                None => {
                    // first stacks block
                    let builder = StacksBlockBuilder::first(miner.id, &burn_block.parent_snapshot.burn_header_hash, burn_block.parent_snapshot.burn_header_timestamp, &proof, &miner.next_microblock_privkey());
                    (builder, None)
                },
                Some(parent_stacks_block) => {
                    // building off an existing stacks block
                    let parent_stacks_block_snapshot = {
                        let ic = burndb.index_conn();
                        let parent_stacks_block_snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &burn_block.parent_snapshot.burn_header_hash, &parent_stacks_block.block_hash()).unwrap().unwrap();
                        let burned_last = BurnDB::get_block_burn_amount(&ic, burn_block.parent_snapshot.block_height, &burn_block.parent_snapshot.burn_header_hash).unwrap();
                        parent_stacks_block_snapshot
                    };

                    let parent_chain_tip = StacksChainState::get_anchored_block_header_info(&self.chainstate.headers_db, &parent_stacks_block_snapshot.burn_header_hash, &parent_stacks_block.header.block_hash()).unwrap().unwrap();

                    let new_work = StacksWorkScore {
                        burn: parent_stacks_block_snapshot.total_burn,
                        work: parent_stacks_block.header.total_work.work.checked_add(1).expect("FATAL: stacks block height overflow")
                    };

                    test_debug!("Work in {} {}: {},{}", burn_block.block_height, burn_block.parent_snapshot.burn_header_hash, new_work.burn, new_work.work);
                    let builder = StacksBlockBuilder::from_parent(miner.id, &parent_chain_tip, &new_work, &proof, &miner.next_microblock_privkey());
                    (builder, Some(parent_stacks_block_snapshot))
                }
            };

            test_debug!("Miner {}: Assemble stacks block from {}", miner.id, miner.origin_address().unwrap().to_string());

            let (stacks_block, microblocks) = block_assembler(builder, miner);
            let block_commit_op = self.make_tenure_commitment(burndb, burn_block, miner, &stacks_block, &microblocks, burn_amount, miner_key, parent_block_snapshot_opt.as_ref());

            (stacks_block, microblocks, block_commit_op)
        }
    }

    /// Return Some(bool) to indicate whether or not the anchored block was accepted into the queue.
    /// Return None if the block was not submitted at all.
    fn preprocess_stacks_block_data(node: &mut TestStacksNode, burn_node: &mut TestBurnchainNode, fork_snapshot: &BlockSnapshot, stacks_block: &StacksBlock, stacks_microblocks: &Vec<StacksMicroblock>, block_commit_op: &LeaderBlockCommitOp) -> Option<bool> {
        let block_hash = stacks_block.block_hash();

        let ic = burn_node.burndb.index_conn();
        let parent_block_burn_header_hash = match BurnDB::get_block_commit_parent(&ic, block_commit_op.parent_block_ptr.into(), block_commit_op.parent_vtxindex.into(), &fork_snapshot.burn_header_hash).unwrap() {
            Some(parent_commit) => parent_commit.burn_header_hash.clone(),
            None => {
                // only allowed if this is the first-ever block in the stacks fork
                assert_eq!(block_commit_op.parent_block_ptr, 0);
                assert_eq!(block_commit_op.parent_vtxindex, 0);
                assert!(stacks_block.header.is_first_mined());

                FIRST_BURNCHAIN_BLOCK_HASH.clone()
            }
        };
    
        let commit_snapshot = match BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &fork_snapshot.burn_header_hash, &block_hash).unwrap() {
            Some(sn) => sn,
            None => {
                test_debug!("Block commit did not win sorition: {:?}", block_commit_op);
                return None;
            }
        };

        // "discover" this stacks block
        test_debug!("\n\nPreprocess Stacks block {}/{} ({})", &commit_snapshot.burn_header_hash, &block_hash, StacksBlockHeader::make_index_block_hash(&commit_snapshot.burn_header_hash, &block_hash));
        let block_res = node.chainstate.preprocess_anchored_block(&ic, &commit_snapshot.burn_header_hash, commit_snapshot.burn_header_timestamp, &stacks_block, &parent_block_burn_header_hash).unwrap();

        // "discover" this stacks microblock stream
        for mblock in stacks_microblocks.iter() {
            test_debug!("Preprocess Stacks microblock {}-{} (seq {})", &block_hash, mblock.block_hash(), mblock.header.sequence);
            let mblock_res = node.chainstate.preprocess_streamed_microblock(&commit_snapshot.burn_header_hash, &stacks_block.block_hash(), mblock).unwrap();
            if !mblock_res {
                return Some(mblock_res)
            }
        }

        Some(block_res)
    }
    
    /// Verify that the stacks block's state root matches the state root in the chain state
    fn check_block_state_index_root(chainstate: &mut StacksChainState, burn_header_hash: &BurnchainHeaderHash, stacks_header: &StacksBlockHeader) -> bool {
        let index_block_hash = StacksBlockHeader::make_index_block_hash(burn_header_hash, &stacks_header.block_hash());
        let mut state_root_index = StacksChainState::open_index(&chainstate.clarity_state_index_path, Some(&StacksBlockHeader::make_index_block_hash(&MINER_BLOCK_BURN_HEADER_HASH, &MINER_BLOCK_HEADER_HASH))).unwrap();
        let state_root = state_root_index.borrow_storage_backend().read_block_root_hash(&index_block_hash).unwrap();
        state_root == stacks_header.state_index_root
    }

    /// Verify that the miner got the expected block reward
    fn check_mining_reward<'a>(clarity_tx: &mut ClarityTx<'a>, miner: &mut TestMiner, block_height: u64, prev_block_rewards: &Vec<Vec<MinerPaymentSchedule>>) -> bool {
        let mut total : u128 = 0;
        if block_height >= MINER_REWARD_MATURITY + MINER_REWARD_WINDOW {
            for (i, prev_block_reward) in prev_block_rewards.iter().enumerate() {
                if i as u64 > block_height - MINER_REWARD_MATURITY - MINER_REWARD_WINDOW {
                    break;
                }
                for recipient in prev_block_reward {
                    if recipient.address == miner.origin_address().unwrap() {
                        let reward : u128 = recipient.coinbase;     // TODO: expand to cover tx fees
                        test_debug!("Miner {} received a reward {} at block {}", &recipient.address.to_string(), reward, i);
                        total += reward;
                    }
                }
            }
        }

        let miner_status_opt = TestStacksNode::get_miner_status(clarity_tx, &miner.origin_address().unwrap());
        match miner_status_opt {
            None => {
                test_debug!("Miner {} '{}' has no mature funds in this fork", miner.id, miner.origin_address().unwrap().to_string());
                return total == 0;
            }
            Some((authorized, amount)) => {
                test_debug!("Miner {} '{}' is authorized: {}, with amount: {} in this fork", miner.id, miner.origin_address().unwrap().to_string(), authorized, amount);
                if amount != total {
                    test_debug!("Amount {} != {}", amount, total);
                    return false;
                }
                return true;
            }
        }
    }

    pub fn get_last_microblock_header(node: &TestStacksNode, miner: &TestMiner, parent_block_opt: Option<&StacksBlock>) -> Option<StacksMicroblockHeader> {
        let last_microblocks_opt = match parent_block_opt {
            Some(ref block) => node.get_microblock_stream(&miner, &block.block_hash()),
            None => None
        };

        let last_microblock_header_opt = match last_microblocks_opt {
            Some(last_microblocks) => { 
                if last_microblocks.len() == 0 {
                    None
                }
                else {
                    let l = last_microblocks.len() - 1;
                    Some(last_microblocks[l].header.clone())
                }
            },
            None => {
                None
            }
        };

        last_microblock_header_opt
    }

    fn get_all_mining_rewards(chainstate: &mut StacksChainState, tip: &StacksHeaderInfo, block_height: u64) -> Vec<Vec<MinerPaymentSchedule>> {
        let mut ret = vec![];
        let mut tx = chainstate.headers_tx_begin().unwrap();

        for i in 0..block_height {
            let block_rewards = StacksChainState::get_scheduled_block_rewards_in_fork(&mut tx, tip, i).unwrap();
            ret.push(block_rewards);
        }

        ret
    }

    /*
    // TODO: can't use this until we stop using get_simmed_block_height
    fn clarity_get_block_hash<'a>(clarity_tx: &mut ClarityTx<'a>, block_height: u64) -> Option<BlockHeaderHash> {
        let block_hash_value = clarity_tx.connection().clarity_eval_raw(&format!("(get-block-info? header-hash u{})", &block_height)).unwrap();

        match block_hash_value {
            Value::Buffer(block_hash_buff) => {
                assert_eq!(block_hash_buff.data.len(), 32);
                let mut buf = [0u8; 32];
                buf.copy_from_slice(&block_hash_buff.data[0..32]);
                Some(BlockHeaderHash(buf))
            },
            _ => {
                None
            }
        }
    }
    */

    /// Simplest end-to-end test: create 1 fork of N Stacks epochs, mined on 1 burn chain fork,
    /// all from the same miner.
    fn mine_stacks_blocks_1_fork_1_miner_1_burnchain<F, G>(test_name: &String, rounds: usize, mut block_builder: F, mut check_oracle: G) -> TestMinerTrace
    where
        F: FnMut(&mut ClarityTx, &mut StacksBlockBuilder, &mut TestMiner, usize, Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>),
        G: FnMut(&StacksBlock, &Vec<StacksMicroblock>) -> bool
    {
        let full_test_name = format!("{}-1_fork_1_miner_1_burnchain", test_name);
        let mut node = TestStacksNode::new(false, 0x80000000, &full_test_name);
        let mut burn_node = TestBurnchainNode::new();
        let mut miner_factory = TestMinerFactory::new();
        let mut miner = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 

        let first_snapshot = BurnDB::get_first_block_snapshot(burn_node.burndb.conn()).unwrap();
        let mut fork = TestBurnchainFork::new(first_snapshot.block_height, &first_snapshot.burn_header_hash, &first_snapshot.index_root, 0);
        
        let mut first_burn_block = TestStacksNode::next_burn_block(&mut burn_node.burndb, &mut fork);

        // first, register a VRF key
        node.add_key_register(&mut first_burn_block, &mut miner);

        test_debug!("Mine {} initial transactions", first_burn_block.txs.len());

        fork.append_block(first_burn_block);
        burn_node.mine_fork(&mut fork);

        let mut miner_trace = vec![];

        // next, build up some stacks blocks
        for i in 0..rounds {
            let mut burn_block = {
                let ic = burn_node.burndb.index_conn();
                fork.next_block(&ic)
            };
            
            let last_key = node.get_last_key(&miner);
            let parent_block_opt = node.get_last_accepted_anchored_block(&miner);
            let last_microblock_header = get_last_microblock_header(&node, &miner, parent_block_opt.as_ref());
            
            // next key
            node.add_key_register(&mut burn_block, &mut miner);

            let (stacks_block, microblocks, block_commit_op) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner, &mut burn_block, &last_key, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block");

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork.append_block(burn_block);
            let fork_snapshot = burn_node.mine_fork(&mut fork);

            // "discover" the stacks block and its microblocks
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block, &microblocks, &block_commit_op);

            // process all blocks
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block.block_hash(), microblocks.len());
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 1).unwrap();

            let expect_success = check_oracle(&stacks_block, &microblocks);
            if expect_success {
                // processed _this_ block
                assert_eq!(tip_info_list.len(), 1);
                let (chain_tip_opt, poison_opt) = tip_info_list[0].clone();

                assert!(chain_tip_opt.is_some());
                assert!(poison_opt.is_none());

                let (chain_tip, _) = chain_tip_opt.unwrap();

                assert_eq!(chain_tip.anchored_header.block_hash(), stacks_block.block_hash());
                assert_eq!(chain_tip.burn_header_hash, fork_snapshot.burn_header_hash);

                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &chain_tip.anchored_header));
            }

            let mut next_miner_trace = TestMinerTracePoint::new();
            next_miner_trace.add(miner.id, full_test_name.clone(), fork_snapshot, stacks_block, microblocks, block_commit_op);
            miner_trace.push(next_miner_trace);
        }

        TestMinerTrace::new(burn_node, vec![miner], miner_trace)
    }

    /// one miner begins a chain, and another miner joins it in the same fork at rounds/2.
    fn mine_stacks_blocks_1_fork_2_miners_1_burnchain<F>(test_name: &String, rounds: usize, mut miner_1_block_builder: F, mut miner_2_block_builder: F) -> TestMinerTrace 
    where
        F: FnMut(&mut ClarityTx, &mut StacksBlockBuilder, &mut TestMiner, usize, Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>)
    {

        let full_test_name = format!("{}-1_fork_2_miners_1_burnchain", test_name);
        let mut node = TestStacksNode::new(false, 0x80000000, &full_test_name);
        let mut burn_node = TestBurnchainNode::new();
        let mut miner_factory = TestMinerFactory::new();
        let mut miner_1 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 
        let mut miner_2 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 

        let mut sortition_winners = vec![];

        let first_snapshot = BurnDB::get_first_block_snapshot(burn_node.burndb.conn()).unwrap();
        let mut fork = TestBurnchainFork::new(first_snapshot.block_height, &first_snapshot.burn_header_hash, &first_snapshot.index_root, 0);
        
        let mut first_burn_block = TestStacksNode::next_burn_block(&mut burn_node.burndb, &mut fork);

        // first, register a VRF key
        node.add_key_register(&mut first_burn_block, &mut miner_1);

        test_debug!("Mine {} initial transactions", first_burn_block.txs.len());

        fork.append_block(first_burn_block);
        burn_node.mine_fork(&mut fork);
        
        let mut miner_trace = vec![];

        // next, build up some stacks blocks
        for i in 0..rounds/2 {
            let mut burn_block = {
                let ic = burn_node.burndb.index_conn();
                fork.next_block(&ic)
            };
            
            let last_key = node.get_last_key(&miner_1);
            let parent_block_opt = node.get_last_anchored_block(&miner_1);
            let last_microblock_header_opt = get_last_microblock_header(&node, &miner_1, parent_block_opt.as_ref());

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block, &mut miner_1);
            node.add_key_register(&mut burn_block, &mut miner_2);

            let (stacks_block, microblocks, block_commit_op) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block, &last_key, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block");

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork.append_block(burn_block);
            let fork_snapshot = burn_node.mine_fork(&mut fork);

            // "discover" the stacks block and its microblocks
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block, &microblocks, &block_commit_op);

            // process all blocks
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block.block_hash(), microblocks.len());
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 1).unwrap();

            // processed _this_ block
            assert_eq!(tip_info_list.len(), 1);
            let (chain_tip_opt, poison_opt) = tip_info_list[0].clone();

            assert!(chain_tip_opt.is_some());
            assert!(poison_opt.is_none());

            let (chain_tip, _) = chain_tip_opt.unwrap();

            assert_eq!(chain_tip.anchored_header.block_hash(), stacks_block.block_hash());
            assert_eq!(chain_tip.burn_header_hash, fork_snapshot.burn_header_hash);

            // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
            assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &chain_tip.anchored_header));

            sortition_winners.push(miner_1.origin_address().unwrap());

            let mut next_miner_trace = TestMinerTracePoint::new();
            next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot, stacks_block, microblocks, block_commit_op);
            miner_trace.push(next_miner_trace);
        }

        // miner 2 begins mining
        for i in rounds/2..rounds {
            let mut burn_block = {
                let ic = burn_node.burndb.index_conn();
                fork.next_block(&ic)
            };
            
            let last_key_1 = node.get_last_key(&miner_1);
            let last_key_2 = node.get_last_key(&miner_2);

            let last_winning_snapshot = {
                let first_block_height = burn_node.burndb.first_block_height;
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork.get_tip(&ic);
                BurnDB::get_last_snapshot_with_sortition(&ic, first_block_height + (i as u64) + 1, &chain_tip.burn_header_hash).expect("FATAL: no prior snapshot with sortition")
            };

            let parent_block_opt = Some(node.get_anchored_block(&last_winning_snapshot.winning_stacks_block_hash).expect("FATAL: no prior block from last winning snapshot"));

            let last_microblock_header_opt = match get_last_microblock_header(&node, &miner_1, parent_block_opt.as_ref()) {
                Some(stream) => Some(stream),
                None => get_last_microblock_header(&node, &miner_2, parent_block_opt.as_ref())
            };

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block, &mut miner_1);
            node.add_key_register(&mut burn_block, &mut miner_2);
            
            let (stacks_block_1, microblocks_1, block_commit_op_1) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block, &last_key_1, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 1 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let (stacks_block_2, microblocks_2, block_commit_op_2) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_2, &mut burn_block, &last_key_2, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 2 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_2_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork.append_block(burn_block);
            let fork_snapshot = burn_node.mine_fork(&mut fork);

            // "discover" the stacks blocks
            let res_1 = preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            let res_2 = preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_2, &microblocks_2, &block_commit_op_2);

            // exactly one stacks block will have been queued up, since sortition picks only one.
            match (res_1, res_2) {
                (Some(res), None) => {},
                (None, Some(res)) => {},
                (_, _) => assert!(false)
            }

            // process all blocks
            test_debug!("Process Stacks block {}", &fork_snapshot.winning_stacks_block_hash);
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();

            // processed exactly one block, but got back two tip-infos
            assert_eq!(tip_info_list.len(), 1);
            let (chain_tip_opt, poison_opt) = tip_info_list[0].clone();

            assert!(chain_tip_opt.is_some());
            assert!(poison_opt.is_none());

            let (chain_tip, _) = chain_tip_opt.unwrap();

            // selected block is the sortition-winning block
            assert_eq!(chain_tip.anchored_header.block_hash(), fork_snapshot.winning_stacks_block_hash);
            assert_eq!(chain_tip.burn_header_hash, fork_snapshot.burn_header_hash);
            
            let mut next_miner_trace = TestMinerTracePoint::new();
            if fork_snapshot.winning_stacks_block_hash == stacks_block_1.block_hash() {
                test_debug!("\n\nMiner 1 ({}) won sortition\n", miner_1.origin_address().unwrap().to_string());

                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_1.header));
                sortition_winners.push(miner_1.origin_address().unwrap());
            
                next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot, stacks_block_1, microblocks_1, block_commit_op_1);
            }
            else {
                test_debug!("\n\nMiner 2 ({}) won sortition\n", miner_2.origin_address().unwrap().to_string());
                
                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_2.header));
                sortition_winners.push(miner_2.origin_address().unwrap());
            
                next_miner_trace.add(miner_2.id, full_test_name.clone(), fork_snapshot, stacks_block_2, microblocks_2, block_commit_op_2);
            }

            miner_trace.push(next_miner_trace);
        }

        TestMinerTrace::new(burn_node, vec![miner_1, miner_2], miner_trace)
    }
    
    /// two miners begin working on the same stacks chain, and then the stacks chain forks
    /// (resulting in two chainstates).  The burnchain is unaffected.  One miner continues on one
    /// chainstate, and the other continues on the other chainstate.  Fork happens on rounds/2
    fn mine_stacks_blocks_2_forks_2_miners_1_burnchain<F>(test_name: &String, rounds: usize, miner_1_block_builder: F, miner_2_block_builder: F) -> TestMinerTrace 
    where
        F: FnMut(&mut ClarityTx, &mut StacksBlockBuilder, &mut TestMiner, usize, Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>)
    {
        mine_stacks_blocks_2_forks_at_height_2_miners_1_burnchain(test_name, rounds, rounds/2, miner_1_block_builder, miner_2_block_builder)
    }

    /// two miners begin working on the same stacks chain, and then the stacks chain forks
    /// (resulting in two chainstates).  The burnchain is unaffected.  One miner continues on one
    /// chainstate, and the other continues on the other chainstate.  Fork happens on fork_height
    fn mine_stacks_blocks_2_forks_at_height_2_miners_1_burnchain<F>(test_name: &String, rounds: usize, fork_height: usize, mut miner_1_block_builder: F, mut miner_2_block_builder: F) -> TestMinerTrace 
    where
        F: FnMut(&mut ClarityTx, &mut StacksBlockBuilder, &mut TestMiner, usize, Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>)
    {

        let full_test_name = format!("{}-2_forks_2_miners_1_burnchain", test_name);
        let mut node = TestStacksNode::new(false, 0x80000000, &full_test_name);
        let mut burn_node = TestBurnchainNode::new();
        let mut miner_factory = TestMinerFactory::new();
        let mut miner_1 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 
        let mut miner_2 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 

        let mut sortition_winners = vec![];

        let first_snapshot = BurnDB::get_first_block_snapshot(burn_node.burndb.conn()).unwrap();
        let mut fork = TestBurnchainFork::new(first_snapshot.block_height, &first_snapshot.burn_header_hash, &first_snapshot.index_root, 0);
        
        let mut first_burn_block = TestStacksNode::next_burn_block(&mut burn_node.burndb, &mut fork);

        // first, register a VRF key
        node.add_key_register(&mut first_burn_block, &mut miner_1);
        node.add_key_register(&mut first_burn_block, &mut miner_2);

        test_debug!("Mine {} initial transactions", first_burn_block.txs.len());

        fork.append_block(first_burn_block);
        burn_node.mine_fork(&mut fork);
        
        let mut miner_trace = vec![];
        
        // miner 1 and 2 cooperate to build a shared fork
        for i in 0..fork_height {
            let mut burn_block = {
                let ic = burn_node.burndb.index_conn();
                fork.next_block(&ic)
            };
            
            let last_key_1 = node.get_last_key(&miner_1);
            let last_key_2 = node.get_last_key(&miner_2);

            let last_winning_snapshot = {
                let first_block_height = burn_node.burndb.first_block_height;
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork.get_tip(&ic);
                BurnDB::get_last_snapshot_with_sortition(&ic, first_block_height + (i as u64) + 1, &chain_tip.burn_header_hash).expect("FATAL: no prior snapshot with sortition")
            };

            let (parent_block_opt, last_microblock_header_opt) = 
                if last_winning_snapshot.num_sortitions == 0 {
                    // this is the first block
                    (None, None)
                }
                else {
                    // this is a subsequent block
                    let parent_block_opt = Some(node.get_anchored_block(&last_winning_snapshot.winning_stacks_block_hash).expect("FATAL: no prior block from last winning snapshot"));
                    let last_microblock_header_opt = match get_last_microblock_header(&node, &miner_1, parent_block_opt.as_ref()) {
                        Some(stream) => Some(stream),
                        None => get_last_microblock_header(&node, &miner_2, parent_block_opt.as_ref())
                    };
                    (parent_block_opt, last_microblock_header_opt)
                };

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block, &mut miner_1);
            node.add_key_register(&mut burn_block, &mut miner_2);
            
            let (stacks_block_1, microblocks_1, block_commit_op_1) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block, &last_key_1, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 1 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt.as_ref());
                
                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let (stacks_block_2, microblocks_2, block_commit_op_2) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_2, &mut burn_block, &last_key_2, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 2 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_2_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork.append_block(burn_block);
            let fork_snapshot = burn_node.mine_fork(&mut fork);

            // "discover" the stacks block and its microblocks
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_2, &microblocks_2, &block_commit_op_2);

            // process all blocks
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block_1.block_hash(), microblocks_1.len());
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block_2.block_hash(), microblocks_2.len());
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();

            // processed _one_ block
            assert_eq!(tip_info_list.len(), 1);
            let (chain_tip_opt, poison_opt) = tip_info_list[0].clone();

            assert!(chain_tip_opt.is_some());
            assert!(poison_opt.is_none());

            let chain_tip = chain_tip_opt.unwrap();

            let mut next_miner_trace = TestMinerTracePoint::new();
            if fork_snapshot.winning_stacks_block_hash == stacks_block_1.block_hash() {
                test_debug!("\n\nMiner 1 ({}) won sortition\n", miner_1.origin_address().unwrap().to_string());

                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_1.header));
                sortition_winners.push(miner_1.origin_address().unwrap());
            }
            else {
                test_debug!("\n\nMiner 2 ({}) won sortition\n", miner_2.origin_address().unwrap().to_string());
                
                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_2.header));
                sortition_winners.push(miner_2.origin_address().unwrap());
            }

            // add both blocks to the miner trace, because in this test runner, there will be _two_
            // nodes that process _all_ blocks
            next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot.clone(), stacks_block_1.clone(), microblocks_1.clone(), block_commit_op_1.clone());
            next_miner_trace.add(miner_2.id, full_test_name.clone(), fork_snapshot.clone(), stacks_block_2.clone(), microblocks_2.clone(), block_commit_op_2.clone());
            miner_trace.push(next_miner_trace);
        }

        test_debug!("\n\nMiner 1 and Miner 2 now separate\n\n");

        let mut sortition_winners_1 = sortition_winners.clone();
        let mut sortition_winners_2 = sortition_winners.clone();
        let snapshot_at_fork = {
            let ic = burn_node.burndb.index_conn();
            let tip = fork.get_tip(&ic);
            tip
        };
        
        assert_eq!(snapshot_at_fork.num_sortitions, fork_height as u64);
  
        // give miner 2 its own chain state directory
        let full_test_name_2 = format!("{}.2", &full_test_name);
        let mut node_2 = node.fork(&full_test_name_2);

        // miner 1 begins working on its own fork.
        // miner 2 begins working on its own fork.
        for i in fork_height..rounds {
            let mut burn_block = {
                let ic = burn_node.burndb.index_conn();
                fork.next_block(&ic)
            };
            
            let last_key_1 = node.get_last_key(&miner_1);
            let last_key_2 = node_2.get_last_key(&miner_2);

            let mut last_winning_snapshot_1 = {
                let ic = burn_node.burndb.index_conn();
                let tip = fork.get_tip(&ic);
                match TestStacksNode::get_last_winning_snapshot(&ic, &tip, &miner_1) {
                    Some(sn) => sn,
                    None => BurnDB::get_first_block_snapshot(&ic).unwrap()
                }
            };

            let mut last_winning_snapshot_2 = {
                let ic = burn_node.burndb.index_conn();
                let tip = fork.get_tip(&ic);
                match TestStacksNode::get_last_winning_snapshot(&ic, &tip, &miner_2) {
                    Some(sn) => sn,
                    None => BurnDB::get_first_block_snapshot(&ic).unwrap()
                }
            };

            // build off of the point where the fork occurred, regardless of who won that sortition
            if last_winning_snapshot_1.num_sortitions < snapshot_at_fork.num_sortitions {
                last_winning_snapshot_1 = snapshot_at_fork.clone();
            }
            if last_winning_snapshot_2.num_sortitions < snapshot_at_fork.num_sortitions {
                last_winning_snapshot_2 = snapshot_at_fork.clone();
            }
            
            let parent_block_opt_1 = node.get_anchored_block(&last_winning_snapshot_1.winning_stacks_block_hash);
            let parent_block_opt_2 = node_2.get_anchored_block(&last_winning_snapshot_2.winning_stacks_block_hash);

            let last_microblock_header_opt_1 = get_last_microblock_header(&node, &miner_1, parent_block_opt_1.as_ref());
            let last_microblock_header_opt_2 = get_last_microblock_header(&node_2, &miner_2, parent_block_opt_2.as_ref());

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block, &mut miner_1);
            node_2.add_key_register(&mut burn_block, &mut miner_2);
            
            let (stacks_block_1, microblocks_1, block_commit_op_1) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block, &last_key_1, parent_block_opt_1.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Miner {}: Produce anchored stacks block in stacks fork 1 via {}", miner.id, miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_1.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let (stacks_block_2, microblocks_2, block_commit_op_2) = node_2.mine_stacks_block(&mut burn_node.burndb, &mut miner_2, &mut burn_block, &last_key_2, parent_block_opt_2.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Miner {}: Produce anchored stacks block in stacks fork 2 via {}", miner.id, miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name_2);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_2_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_2.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork.append_block(burn_block);
            let fork_snapshot = burn_node.mine_fork(&mut fork);

            // "discover" the stacks blocks
            let res_1 = preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            let res_2 = preprocess_stacks_block_data(&mut node_2, &mut burn_node, &fork_snapshot, &stacks_block_2, &microblocks_2, &block_commit_op_2);

            // exactly one stacks block will have been queued up, since sortition picks only one.
            match (res_1, res_2) {
                (Some(res), None) => assert!(res),
                (None, Some(res)) => assert!(res),
                (_, _) => assert!(false)
            }

            // process all blocks
            test_debug!("Process Stacks block {}", &fork_snapshot.winning_stacks_block_hash);
            let mut tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();
            let mut tip_info_list_2 = node_2.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();

            tip_info_list.append(&mut tip_info_list_2);

            // processed exactly one block, but got back two tip-infos
            assert_eq!(tip_info_list.len(), 1);
            let (chain_tip_opt, poison_opt) = tip_info_list[0].clone();

            assert!(chain_tip_opt.is_some());
            assert!(poison_opt.is_none());

            let (chain_tip, _) = chain_tip_opt.unwrap();

            // selected block is the sortition-winning block
            assert_eq!(chain_tip.anchored_header.block_hash(), fork_snapshot.winning_stacks_block_hash);
            assert_eq!(chain_tip.burn_header_hash, fork_snapshot.burn_header_hash);
            
            let mut next_miner_trace = TestMinerTracePoint::new();
            if fork_snapshot.winning_stacks_block_hash == stacks_block_1.block_hash() {
                test_debug!("\n\nMiner 1 ({}) won sortition\n", miner_1.origin_address().unwrap().to_string());

                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_1.header));
                sortition_winners_1.push(miner_1.origin_address().unwrap());
            }
            else {
                test_debug!("\n\nMiner 2 ({}) won sortition\n", miner_2.origin_address().unwrap().to_string());
                
                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node_2.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_2.header));
                sortition_winners_2.push(miner_2.origin_address().unwrap());
            }
           
            // each miner produced a block; just one of them got accepted
            next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot.clone(), stacks_block_1.clone(), microblocks_1.clone(), block_commit_op_1.clone());
            next_miner_trace.add(miner_2.id, full_test_name_2.clone(), fork_snapshot.clone(), stacks_block_2.clone(), microblocks_2.clone(), block_commit_op_2.clone());
            miner_trace.push(next_miner_trace);

            // keep chainstates in sync with one another -- each node discovers each other nodes'
            // block data.
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_2, &microblocks_2, &block_commit_op_2);
            preprocess_stacks_block_data(&mut node_2, &mut burn_node, &fork_snapshot, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            let _ = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();
            let _ = node_2.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();
        }
        
        TestMinerTrace::new(burn_node, vec![miner_1, miner_2], miner_trace)
    }
    
    /// two miners work on the same fork, and the burnchain splits them.
    /// the split happens at rounds/2
    fn mine_stacks_blocks_1_fork_2_miners_2_burnchains<F>(test_name: &String, rounds: usize, mut miner_1_block_builder: F, mut miner_2_block_builder: F) -> TestMinerTrace 
    where
        F: FnMut(&mut ClarityTx, &mut StacksBlockBuilder, &mut TestMiner, usize, Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>)
    {
        let full_test_name = format!("{}-1_fork_2_miners_2_burnchain", test_name);
        let mut burn_node = TestBurnchainNode::new();
        let mut node = TestStacksNode::new(false, 0x80000000, &full_test_name);
        let mut miner_factory = TestMinerFactory::new();
        let mut miner_1 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 
        let mut miner_2 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 

        let first_snapshot = BurnDB::get_first_block_snapshot(burn_node.burndb.conn()).unwrap();
        let mut fork_1 = TestBurnchainFork::new(first_snapshot.block_height, &first_snapshot.burn_header_hash, &first_snapshot.index_root, 0);
        
        let mut first_burn_block = TestStacksNode::next_burn_block(&mut burn_node.burndb, &mut fork_1);

        // first, register a VRF key
        node.add_key_register(&mut first_burn_block, &mut miner_1);
        node.add_key_register(&mut first_burn_block, &mut miner_2);

        test_debug!("Mine {} initial transactions", first_burn_block.txs.len());

        fork_1.append_block(first_burn_block);
        burn_node.mine_fork(&mut fork_1);
        
        let mut miner_trace = vec![];

        // next, build up some stacks blocks, cooperatively
        for i in 0..rounds/2 {
            let mut burn_block = {
                let ic = burn_node.burndb.index_conn();
                fork_1.next_block(&ic)
            };
            
            let last_key_1 = node.get_last_key(&miner_1);
            let last_key_2 = node.get_last_key(&miner_2);

            let last_winning_snapshot = {
                let first_block_height = burn_node.burndb.first_block_height;
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork_1.get_tip(&ic);
                BurnDB::get_last_snapshot_with_sortition(&ic, first_block_height + (i as u64) + 1, &chain_tip.burn_header_hash).expect("FATAL: no prior snapshot with sortition")
            };

            let (parent_block_opt, last_microblock_header_opt) = 
                if last_winning_snapshot.num_sortitions == 0 {
                    // this is the first block
                    (None, None)
                }
                else {
                    // this is a subsequent block
                    let parent_block_opt = Some(node.get_anchored_block(&last_winning_snapshot.winning_stacks_block_hash).expect("FATAL: no prior block from last winning snapshot"));
                    let last_microblock_header_opt = match get_last_microblock_header(&node, &miner_1, parent_block_opt.as_ref()) {
                        Some(stream) => Some(stream),
                        None => get_last_microblock_header(&node, &miner_2, parent_block_opt.as_ref())
                    };
                    (parent_block_opt, last_microblock_header_opt)
                };

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block, &mut miner_1);
            node.add_key_register(&mut burn_block, &mut miner_2);

            let (stacks_block_1, microblocks_1, block_commit_op_1) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block, &last_key_1, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block from miner 1");

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let (stacks_block_2, microblocks_2, block_commit_op_2) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_2, &mut burn_block, &last_key_2, parent_block_opt.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block from miner 2");

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_2_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork_1.append_block(burn_block);
            let fork_snapshot = burn_node.mine_fork(&mut fork_1);

            // "discover" the stacks block
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_2, &microblocks_2, &block_commit_op_2);

            // process all blocks
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block_1.block_hash(), microblocks_1.len());
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block_2.block_hash(), microblocks_2.len());
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();

            // processed _one_ block
            assert_eq!(tip_info_list.len(), 1);
            let (chain_tip_opt, poison_opt) = tip_info_list[0].clone();

            assert!(chain_tip_opt.is_some());
            assert!(poison_opt.is_none());

            let (chain_tip, _) = chain_tip_opt.unwrap();

            // selected block is the sortition-winning block
            assert_eq!(chain_tip.anchored_header.block_hash(), fork_snapshot.winning_stacks_block_hash);
            assert_eq!(chain_tip.burn_header_hash, fork_snapshot.burn_header_hash);
            
            let mut next_miner_trace = TestMinerTracePoint::new();
            if fork_snapshot.winning_stacks_block_hash == stacks_block_1.block_hash() {
                test_debug!("\n\nMiner 1 ({}) won sortition\n", miner_1.origin_address().unwrap().to_string());

                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_1.header));
                next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot, stacks_block_1, microblocks_1, block_commit_op_1);
            }
            else {
                test_debug!("\n\nMiner 2 ({}) won sortition\n", miner_2.origin_address().unwrap().to_string());
                
                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_2.header));
                next_miner_trace.add(miner_2.id, full_test_name.clone(), fork_snapshot, stacks_block_2, microblocks_2, block_commit_op_2);
            }
            miner_trace.push(next_miner_trace);
        }

        let mut fork_2 = fork_1.fork();

        test_debug!("\n\n\nbegin burnchain fork\n\n");

        // next, build up some stacks blocks on two separate burnchain forks.
        // send the same leader key register transactions to both forks.
        for i in rounds/2..rounds {
            let mut burn_block_1 = {
                let ic = burn_node.burndb.index_conn();
                fork_1.next_block(&ic)
            };
            let mut burn_block_2 = {
                let ic = burn_node.burndb.index_conn();
                fork_2.next_block(&ic)
            };
            
            let last_key_1 = node.get_last_key(&miner_1);
            let last_key_2 = node.get_last_key(&miner_2);

            let block_1_snapshot = {
                let first_block_height = burn_node.burndb.first_block_height;
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork_1.get_tip(&ic);
                BurnDB::get_last_snapshot_with_sortition(&ic, first_block_height + (i as u64) + 1, &chain_tip.burn_header_hash).expect("FATAL: no prior snapshot with sortition")
            };

            let block_2_snapshot = {
                let first_block_height = burn_node.burndb.first_block_height;
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork_2.get_tip(&ic);
                BurnDB::get_last_snapshot_with_sortition(&ic, first_block_height + (i as u64) + 1, &chain_tip.burn_header_hash).expect("FATAL: no prior snapshot with sortition")
            };

            let parent_block_opt_1 = node.get_anchored_block(&block_1_snapshot.winning_stacks_block_hash);
            let parent_block_opt_2 = node.get_anchored_block(&block_2_snapshot.winning_stacks_block_hash);

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block_1, &mut miner_1);
            node.add_key_register(&mut burn_block_2, &mut miner_2);

            let last_microblock_header_opt_1 = get_last_microblock_header(&node, &miner_1, parent_block_opt_1.as_ref());
            let last_microblock_header_opt_2 = get_last_microblock_header(&node, &miner_2, parent_block_opt_2.as_ref());

            let (stacks_block_1, microblocks_1, block_commit_op_1) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block_1, &last_key_1, parent_block_opt_1.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 1 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_1.as_ref());
                
                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let (stacks_block_2, microblocks_2, block_commit_op_2) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_2, &mut burn_block_2, &last_key_2, parent_block_opt_2.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 2 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_2_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_2.as_ref());
                
                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork_1.append_block(burn_block_1);
            fork_2.append_block(burn_block_2);
            let fork_snapshot_1 = burn_node.mine_fork(&mut fork_1);
            let fork_snapshot_2 = burn_node.mine_fork(&mut fork_2);
            
            assert!(fork_snapshot_1.burn_header_hash != fork_snapshot_2.burn_header_hash);
            assert!(fork_snapshot_1.consensus_hash != fork_snapshot_2.consensus_hash);

            // "discover" the stacks block
            test_debug!("preprocess fork 1 {}", stacks_block_1.block_hash());
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot_1, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            
            test_debug!("preprocess fork 2 {}", stacks_block_1.block_hash());
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot_2, &stacks_block_2, &microblocks_2, &block_commit_op_2);

            // process all blocks
            test_debug!("Process all Stacks blocks: {}, {}", &stacks_block_1.block_hash(), &stacks_block_2.block_hash());
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();

            // processed all stacks blocks -- one on each burn chain fork
            assert_eq!(tip_info_list.len(), 2);

            for (ref chain_tip_opt, ref poison_opt) in tip_info_list.iter() {
                assert!(chain_tip_opt.is_some());
                assert!(poison_opt.is_none());
            }

            // fork 1?
            let mut found_fork_1 = false;
            for (ref chain_tip_opt, ref poison_opt) in tip_info_list.iter() {
                let (chain_tip, _) = chain_tip_opt.clone().unwrap();
                if chain_tip.burn_header_hash == fork_snapshot_1.burn_header_hash {
                    found_fork_1 = true;
                    assert_eq!(chain_tip.anchored_header.block_hash(), stacks_block_1.block_hash());
            
                    // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                    assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot_1.burn_header_hash, &chain_tip.anchored_header));
                }
            }

            assert!(found_fork_1);

            let mut found_fork_2 = false;
            for (ref chain_tip_opt, ref poison_opt) in tip_info_list.iter() {
                let (chain_tip, _) = chain_tip_opt.clone().unwrap();
                if chain_tip.burn_header_hash == fork_snapshot_2.burn_header_hash {
                    found_fork_2 = true;
                    assert_eq!(chain_tip.anchored_header.block_hash(), stacks_block_2.block_hash());
                    
                    // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                    assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot_2.burn_header_hash, &chain_tip.anchored_header));
                }
            }

            assert!(found_fork_2);
            
            let mut next_miner_trace = TestMinerTracePoint::new();
            next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot_1, stacks_block_1, microblocks_1, block_commit_op_1);
            next_miner_trace.add(miner_2.id, full_test_name.clone(), fork_snapshot_2, stacks_block_2, microblocks_2, block_commit_op_2);
            miner_trace.push(next_miner_trace);
        }
        
        TestMinerTrace::new(burn_node, vec![miner_1, miner_2], miner_trace)
    }


    /// two miners begin working on separate forks, and the burnchain splits out under them,
    /// putting each one on a different fork.
    /// split happens at rounds/2
    fn mine_stacks_blocks_2_forks_2_miners_2_burnchains<F>(test_name: &String, rounds: usize, mut miner_1_block_builder: F, mut miner_2_block_builder: F) -> TestMinerTrace 
    where
        F: FnMut(&mut ClarityTx, &mut StacksBlockBuilder, &mut TestMiner, usize, Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>)
    {
        let full_test_name = format!("{}-2_forks_2_miner_2_burnchains", test_name);
        let mut burn_node = TestBurnchainNode::new();
        let mut node = TestStacksNode::new(false, 0x80000000, &full_test_name);
        let mut miner_factory = TestMinerFactory::new();
        let mut miner_1 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 
        let mut miner_2 = miner_factory.next_miner(&burn_node.burnchain, 1, 1, AddressHashMode::SerializeP2PKH); 

        let first_snapshot = BurnDB::get_first_block_snapshot(burn_node.burndb.conn()).unwrap();
        let mut fork_1 = TestBurnchainFork::new(first_snapshot.block_height, &first_snapshot.burn_header_hash, &first_snapshot.index_root, 0);
        
        let mut first_burn_block = TestStacksNode::next_burn_block(&mut burn_node.burndb, &mut fork_1);

        // first, register a VRF key
        node.add_key_register(&mut first_burn_block, &mut miner_1);
        node.add_key_register(&mut first_burn_block, &mut miner_2);

        test_debug!("Mine {} initial transactions", first_burn_block.txs.len());

        fork_1.append_block(first_burn_block);
        burn_node.mine_fork(&mut fork_1);
        
        let mut miner_trace = vec![];

        // next, build up some stacks blocks. miners cooperate
        for i in 0..rounds/2 {
            let mut burn_block = {
                let ic = burn_node.burndb.index_conn();
                fork_1.next_block(&ic)
            };
            
            let last_key_1 = node.get_last_key(&miner_1);
            let last_key_2 = node.get_last_key(&miner_2);

            let (block_1_snapshot_opt, block_2_snapshot_opt) = {
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork_1.get_tip(&ic);
                let block_1_snapshot_opt = TestStacksNode::get_last_winning_snapshot(&ic, &chain_tip, &miner_1);
                let block_2_snapshot_opt = TestStacksNode::get_last_winning_snapshot(&ic, &chain_tip, &miner_2);
                (block_1_snapshot_opt, block_2_snapshot_opt)
            };
            
            let parent_block_opt_1 = match block_1_snapshot_opt {
                Some(sn) => node.get_anchored_block(&sn.winning_stacks_block_hash),
                None => None
            };
            
            let parent_block_opt_2 = match block_2_snapshot_opt {
                Some(sn) => node.get_anchored_block(&sn.winning_stacks_block_hash),
                None => parent_block_opt_1.clone()
            };

            let last_microblock_header_opt_1 = get_last_microblock_header(&node, &miner_1, parent_block_opt_1.as_ref());
            let last_microblock_header_opt_2 = get_last_microblock_header(&node, &miner_2, parent_block_opt_2.as_ref());

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block, &mut miner_1);
            node.add_key_register(&mut burn_block, &mut miner_2);

            let (stacks_block_1, microblocks_1, block_commit_op_1) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block, &last_key_1, parent_block_opt_1.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block");

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_1.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let (stacks_block_2, microblocks_2, block_commit_op_2) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_2, &mut burn_block, &last_key_2, parent_block_opt_2.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block");

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_2_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_2.as_ref());

                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork_1.append_block(burn_block);
            let fork_snapshot = burn_node.mine_fork(&mut fork_1);

            // "discover" the stacks block
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot, &stacks_block_2, &microblocks_2, &block_commit_op_2);

            // process all blocks
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block_1.block_hash(), microblocks_1.len());
            test_debug!("Process Stacks block {} and {} microblocks", &stacks_block_2.block_hash(), microblocks_2.len());
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();

            // processed _one_ block
            assert_eq!(tip_info_list.len(), 1);
            let (chain_tip_opt, poison_opt) = tip_info_list[0].clone();

            assert!(chain_tip_opt.is_some());
            assert!(poison_opt.is_none());

            let (chain_tip, _) = chain_tip_opt.unwrap();

            // selected block is the sortition-winning block
            assert_eq!(chain_tip.anchored_header.block_hash(), fork_snapshot.winning_stacks_block_hash);
            assert_eq!(chain_tip.burn_header_hash, fork_snapshot.burn_header_hash);
            
            let mut next_miner_trace = TestMinerTracePoint::new();
            if fork_snapshot.winning_stacks_block_hash == stacks_block_1.block_hash() {
                test_debug!("\n\nMiner 1 ({}) won sortition\n", miner_1.origin_address().unwrap().to_string());

                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_1.header));
                next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot.clone(), stacks_block_1, microblocks_1, block_commit_op_1);
            }
            else {
                test_debug!("\n\nMiner 2 ({}) won sortition\n", miner_2.origin_address().unwrap().to_string());
                
                // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot.burn_header_hash, &stacks_block_2.header));
                next_miner_trace.add(miner_2.id, full_test_name.clone(), fork_snapshot, stacks_block_2, microblocks_2, block_commit_op_2);
            }

            miner_trace.push(next_miner_trace);
        }

        let mut fork_2 = fork_1.fork();

        test_debug!("\n\n\nbegin burnchain fork\n\n");

        // next, build up some stacks blocks on two separate burnchain forks.
        // send the same leader key register transactions to both forks.
        // miner 1 works on fork 1
        // miner 2 works on fork 2
        for i in rounds/2..rounds {
            let mut burn_block_1 = {
                let ic = burn_node.burndb.index_conn();
                fork_1.next_block(&ic)
            };
            let mut burn_block_2 = {
                let ic = burn_node.burndb.index_conn();
                fork_2.next_block(&ic)
            };
            
            let last_key_1 = node.get_last_key(&miner_1);
            let last_key_2 = node.get_last_key(&miner_2);
            let block_1_snapshot_opt = {
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork_1.get_tip(&ic);
                TestStacksNode::get_last_winning_snapshot(&ic, &chain_tip, &miner_1)
            };
            let block_2_snapshot_opt = {
                let ic = burn_node.burndb.index_conn();
                let chain_tip = fork_2.get_tip(&ic);
                TestStacksNode::get_last_winning_snapshot(&ic, &chain_tip, &miner_2)
            };
            
            let parent_block_opt_1 = match block_1_snapshot_opt {
                Some(sn) => node.get_anchored_block(&sn.winning_stacks_block_hash),
                None => None
            };
            
            let parent_block_opt_2 = match block_2_snapshot_opt {
                Some(sn) => node.get_anchored_block(&sn.winning_stacks_block_hash),
                None => parent_block_opt_1.clone()
            };

            // send next key (key for block i+1)
            node.add_key_register(&mut burn_block_1, &mut miner_1);
            node.add_key_register(&mut burn_block_2, &mut miner_2);

            let last_microblock_header_opt_1 = get_last_microblock_header(&node, &miner_1, parent_block_opt_1.as_ref());
            let last_microblock_header_opt_2 = get_last_microblock_header(&node, &miner_2, parent_block_opt_2.as_ref());

            let (stacks_block_1, microblocks_1, block_commit_op_1) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_1, &mut burn_block_1, &last_key_1, parent_block_opt_1.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 1 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_1_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_1.as_ref());
                
                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });
            
            let (stacks_block_2, microblocks_2, block_commit_op_2) = node.mine_stacks_block(&mut burn_node.burndb, &mut miner_2, &mut burn_block_2, &last_key_2, parent_block_opt_2.as_ref(), 1000, |mut builder, ref mut miner| {
                test_debug!("Produce anchored stacks block in stacks fork 2 via {}", miner.origin_address().unwrap().to_string());

                let mut miner_chainstate = open_chainstate(false, 0x80000000, &full_test_name);
                let all_prev_mining_rewards = get_all_mining_rewards(&mut miner_chainstate, &builder.chain_tip, builder.chain_tip.block_height);

                let mut epoch = builder.epoch_begin(&mut miner_chainstate).unwrap();
                let (stacks_block, microblocks) = miner_2_block_builder(&mut epoch, &mut builder, miner, i, last_microblock_header_opt_2.as_ref());
                
                assert!(check_mining_reward(&mut epoch, miner, builder.chain_tip.block_height, &all_prev_mining_rewards));

                builder.epoch_finish(epoch);
                (stacks_block, microblocks)
            });

            // process burn chain
            fork_1.append_block(burn_block_1);
            fork_2.append_block(burn_block_2);
            let fork_snapshot_1 = burn_node.mine_fork(&mut fork_1);
            let fork_snapshot_2 = burn_node.mine_fork(&mut fork_2);
            
            assert!(fork_snapshot_1.burn_header_hash != fork_snapshot_2.burn_header_hash);
            assert!(fork_snapshot_1.consensus_hash != fork_snapshot_2.consensus_hash);

            // "discover" the stacks block
            test_debug!("preprocess fork 1 {}", stacks_block_1.block_hash());
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot_1, &stacks_block_1, &microblocks_1, &block_commit_op_1);
            
            test_debug!("preprocess fork 2 {}", stacks_block_1.block_hash());
            preprocess_stacks_block_data(&mut node, &mut burn_node, &fork_snapshot_2, &stacks_block_2, &microblocks_2, &block_commit_op_2);

            // process all blocks
            test_debug!("Process all Stacks blocks: {}, {}", &stacks_block_1.block_hash(), &stacks_block_2.block_hash());
            let tip_info_list = node.chainstate.process_blocks(&mut burn_node.burndb, 2).unwrap();

            // processed all stacks blocks -- one on each burn chain fork
            assert_eq!(tip_info_list.len(), 2);

            for (ref chain_tip_opt, ref poison_opt) in tip_info_list.iter() {
                assert!(chain_tip_opt.is_some());
                assert!(poison_opt.is_none());
            }

            // fork 1?
            let mut found_fork_1 = false;
            for (ref chain_tip_opt, ref poison_opt) in tip_info_list.iter() {
                let (chain_tip, _) = chain_tip_opt.clone().unwrap();
                if chain_tip.burn_header_hash == fork_snapshot_1.burn_header_hash {
                    found_fork_1 = true;
                    assert_eq!(chain_tip.anchored_header.block_hash(), stacks_block_1.block_hash());
            
                    // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                    assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot_1.burn_header_hash, &chain_tip.anchored_header));
                }
            }

            assert!(found_fork_1);

            let mut found_fork_2 = false;
            for (ref chain_tip_opt, ref poison_opt) in tip_info_list.iter() {
                let (chain_tip, _) = chain_tip_opt.clone().unwrap();
                if chain_tip.burn_header_hash == fork_snapshot_2.burn_header_hash {
                    found_fork_2 = true;
                    assert_eq!(chain_tip.anchored_header.block_hash(), stacks_block_2.block_hash());
                    
                    // MARF trie exists for the block header's chain state, so we can make merkle proofs on it
                    assert!(check_block_state_index_root(&mut node.chainstate, &fork_snapshot_2.burn_header_hash, &chain_tip.anchored_header));
                }
            }

            assert!(found_fork_2);
            
            let mut next_miner_trace = TestMinerTracePoint::new();
            next_miner_trace.add(miner_1.id, full_test_name.clone(), fork_snapshot_1, stacks_block_1, microblocks_1, block_commit_op_1);
            next_miner_trace.add(miner_2.id, full_test_name.clone(), fork_snapshot_2, stacks_block_2, microblocks_2, block_commit_op_2);
            miner_trace.push(next_miner_trace);
        }
        
        TestMinerTrace::new(burn_node, vec![miner_1, miner_2], miner_trace)
    }

    /// compare two chainstates to see if they have exactly the same blocks and microblocks.
    fn assert_chainstate_blocks_eq(test_name_1: &str, test_name_2: &str) {
        let ch1 = open_chainstate(false, 0x80000000, test_name_1);
        let ch2 = open_chainstate(false, 0x80000000, test_name_2);

        // check presence of anchored blocks
        let mut all_blocks_1 = StacksChainState::list_blocks(&ch1.blocks_db).unwrap();
        let mut all_blocks_2 = StacksChainState::list_blocks(&ch2.blocks_db).unwrap();

        all_blocks_1.sort();
        all_blocks_2.sort();

        assert_eq!(all_blocks_1.len(), all_blocks_2.len());
        for i in 0..all_blocks_1.len() {
            assert_eq!(all_blocks_1[i], all_blocks_2[i]);
        }

        // check presence and ordering of microblocks
        let mut all_microblocks_1 = StacksChainState::list_microblocks(&ch1.blocks_db, &ch1.blocks_path).unwrap();
        let mut all_microblocks_2 = StacksChainState::list_microblocks(&ch2.blocks_db, &ch2.blocks_path).unwrap();

        all_microblocks_1.sort();
        all_microblocks_2.sort();

        assert_eq!(all_microblocks_1.len(), all_microblocks_2.len());
        for i in 0..all_microblocks_1.len() {
            assert_eq!(all_microblocks_1[i].0, all_microblocks_2[i].0);
            assert_eq!(all_microblocks_1[i].1, all_microblocks_2[i].1);

            assert_eq!(all_microblocks_1[i].2.len(), all_microblocks_2[i].2.len());
            for j in 0..all_microblocks_1[i].2.len() {
                assert_eq!(all_microblocks_1[i].2[j], all_microblocks_2[i].2[j]);
            }
        }

        // compare block status (staging vs confirmed) and contents
        for i in 0..all_blocks_1.len() {
            let staging_1_opt = StacksChainState::load_staging_block(&ch1.blocks_db, &ch2.blocks_path, &all_blocks_1[i].0, &all_blocks_1[i].1).unwrap();
            let staging_2_opt = StacksChainState::load_staging_block(&ch2.blocks_db, &ch2.blocks_path, &all_blocks_2[i].0, &all_blocks_2[i].1).unwrap();

            let chunk_1_opt = StacksChainState::load_block(&ch1.blocks_path, &all_blocks_1[i].0, &all_blocks_1[i].1).unwrap();
            let chunk_2_opt = StacksChainState::load_block(&ch2.blocks_path, &all_blocks_2[i].0, &all_blocks_2[i].1).unwrap();

            match (staging_1_opt, staging_2_opt) {
                (Some(staging_1), Some(staging_2)) => {
                    assert_eq!(staging_1.block_data, staging_2.block_data);
                },
                (None, None) => {},
                (_, _) => {
                    assert!(false);
                }
            }

            match (chunk_1_opt, chunk_2_opt) {
                (Some(block_1), Some(block_2)) => {
                    assert_eq!(block_1, block_2);
                },
                (None, None) => {},
                (_, _) => {
                    assert!(false);
                }
            }
        }

        for i in 0..all_microblocks_1.len() {
            if all_microblocks_1[i].2.len() == 0 {
                continue;
            }
            let chunk_1_opt = StacksChainState::load_staging_microblock_stream(&ch1.blocks_db, &ch1.blocks_path, &all_microblocks_1[i].0, &all_microblocks_1[i].1, u16::max_value()).unwrap();
            let chunk_2_opt = StacksChainState::load_staging_microblock_stream(&ch1.blocks_db, &ch2.blocks_path, &all_microblocks_2[i].0, &all_microblocks_2[i].1, u16::max_value()).unwrap();

            match (chunk_1_opt, chunk_2_opt) {
                (Some(chunk_1), Some(chunk_2)) => {
                    assert_eq!(chunk_1, chunk_2);
                },
                (None, None) => {},
                (_, _) => {
                    assert!(false);
                }
            }
            for j in 0..all_microblocks_1[i].2.len() {
                // staging status is the same
                let staging_1_opt = StacksChainState::load_staging_microblock(&ch1.blocks_db, &all_microblocks_1[i].0, &all_microblocks_1[i].1, &all_microblocks_1[i].2[j]).unwrap();
                let staging_2_opt = StacksChainState::load_staging_microblock(&ch2.blocks_db, &all_microblocks_2[i].0, &all_microblocks_2[i].1, &all_microblocks_2[i].2[j]).unwrap();

                match (staging_1_opt, staging_2_opt) {
                    (Some(staging_1), Some(staging_2)) => {
                        assert_eq!(staging_1.block_data, staging_2.block_data);
                    },
                    (None, None) => {},
                    (_, _) => {
                        assert!(false);
                    }
                }
            }
        }
    }

    /// produce all stacks blocks, but don't process them in order.  Instead, queue them all up and
    /// process them in randomized order.
    /// This works by running mine_stacks_blocks_1_fork_1_miner_1_burnchain, extracting the blocks,
    /// and then re-processing them in a different chainstate directory.
    fn miner_trace_replay_randomized(miner_trace: &mut TestMinerTrace) {
        test_debug!("\n\n");
        test_debug!("------------------------------------------------------------------------");
        test_debug!("                   Randomize and re-apply blocks");
        test_debug!("------------------------------------------------------------------------");
        test_debug!("\n\n");

        let rounds = miner_trace.rounds();
        let test_names = miner_trace.get_test_names();
        let mut nodes = HashMap::new();
        for (i, test_name) in test_names.iter().enumerate() {
            let rnd_test_name = format!("{}-replay_randomized", test_name);
            let next_node = TestStacksNode::new(false, 0x80000000, &rnd_test_name);
            nodes.insert(test_name, next_node);
        }

        let expected_num_sortitions = miner_trace.get_num_sortitions();
        let expected_num_blocks = miner_trace.get_num_blocks();
        let mut num_processed = 0;

        let mut rng = thread_rng();
        miner_trace.points.as_mut_slice().shuffle(&mut rng);

        // "discover" blocks in random order
        for point in miner_trace.points.drain(..) {
            let mut miner_ids = point.get_miner_ids();
            miner_ids.as_mut_slice().shuffle(&mut rng);

            for miner_id in miner_ids {
                let fork_snapshot_opt = point.get_block_snapshot(miner_id);
                let stacks_block_opt = point.get_stacks_block(miner_id);
                let microblocks_opt = point.get_microblocks(miner_id);
                let block_commit_op_opt = point.get_block_commit(miner_id);

                if fork_snapshot_opt.is_none() || block_commit_op_opt.is_none() {
                    // no sortition by this miner at this point in time
                    continue;
                }

                let fork_snapshot = fork_snapshot_opt.unwrap();
                let block_commit_op = block_commit_op_opt.unwrap();

                match stacks_block_opt {
                    Some(stacks_block) => {
                        let mut microblocks = microblocks_opt.unwrap_or(vec![]);

                        // "discover" the stacks block and its microblocks in all nodes
                        // TODO: randomize microblock discovery order too
                        for (node_name, mut node) in nodes.iter_mut() {

                            microblocks.as_mut_slice().shuffle(&mut rng);
                            
                            preprocess_stacks_block_data(&mut node, &mut miner_trace.burn_node, &fork_snapshot, &stacks_block, &vec![], &block_commit_op);
                            
                            if microblocks.len() > 0 {
                                for mblock in microblocks.iter() {
                                    preprocess_stacks_block_data(&mut node, &mut miner_trace.burn_node, &fork_snapshot, &stacks_block, &vec![mblock.clone()], &block_commit_op);
                                
                                    // process all the blocks we can 
                                    test_debug!("Process Stacks block {} and microblock {} {}", &stacks_block.block_hash(), mblock.block_hash(), mblock.header.sequence);
                                    let tip_info_list = node.chainstate.process_blocks(&mut miner_trace.burn_node.burndb, expected_num_blocks).unwrap();

                                    num_processed += tip_info_list.len();
                                }
                            }
                            else {
                                // process all the blocks we can 
                                test_debug!("Process Stacks block {} and {} microblocks in {}", &stacks_block.block_hash(), microblocks.len(), &node_name);
                                let tip_info_list = node.chainstate.process_blocks(&mut miner_trace.burn_node.burndb, expected_num_blocks).unwrap();

                                num_processed += tip_info_list.len();
                            }
                        }
                    },
                    None => {
                        // no block announced at this point in time
                        test_debug!("Miner {} did not produce a Stacks block for {:?} (commit {:?})", miner_id, &fork_snapshot, &block_commit_op);
                        continue;
                    }
                }
            }
        }

        // must have processed the same number of blocks in all nodes
        assert_eq!(num_processed, expected_num_blocks);

        // must have processed all blocks the same way
        for test_name in test_names.iter() {
            let rnd_test_name = format!("{}-replay_randomized", test_name);
            assert_chainstate_blocks_eq(test_name, &rnd_test_name);
        }
    }

    pub fn make_coinbase(miner: &mut TestMiner, burnchain_height: usize) -> StacksTransaction {
        // make a coinbase for this miner
        let mut tx_coinbase = StacksTransaction::new(TransactionVersion::Testnet, miner.as_transaction_auth().unwrap(), TransactionPayload::Coinbase(CoinbasePayload([(burnchain_height % 256) as u8; 32])));
        tx_coinbase.chain_id = 0x80000000;
        tx_coinbase.anchor_mode = TransactionAnchorMode::OnChainOnly;
        tx_coinbase.auth.set_origin_nonce(miner.get_nonce());

        let mut tx_signer = StacksTransactionSigner::new(&tx_coinbase);
        miner.sign_as_origin(&mut tx_signer);
        let tx_coinbase_signed = tx_signer.get_tx().unwrap();
        tx_coinbase_signed
    }

    pub fn mine_empty_anchored_block<'a>(clarity_tx: &mut ClarityTx<'a>, builder: &mut StacksBlockBuilder, miner: &mut TestMiner, burnchain_height: usize, parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {
        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);

        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);

        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();

        let stacks_block = builder.mine_anchored_block(clarity_tx);
        
        test_debug!("Produce anchored stacks block at burnchain height {} stacks height {}", burnchain_height, stacks_block.header.total_work.work);
        (stacks_block, vec![])
    }
    
    pub fn mine_empty_anchored_block_with_burn_height_pubkh<'a>(clarity_tx: &mut ClarityTx<'a>, builder: &mut StacksBlockBuilder, miner: &mut TestMiner, burnchain_height: usize, parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {
        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);

        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);

        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();

        let mut stacks_block = builder.mine_anchored_block(clarity_tx);

        let mut pubkh_bytes = [0u8; 20];
        pubkh_bytes[0..8].copy_from_slice(&burnchain_height.to_be_bytes());

        stacks_block.header.microblock_pubkey_hash = Hash160(pubkh_bytes);
        
        test_debug!("Produce anchored stacks block at burnchain height {} stacks height {} pubkeyhash {}", burnchain_height, stacks_block.header.total_work.work, &stacks_block.header.microblock_pubkey_hash);
        (stacks_block, vec![])
    }
    
    pub fn mine_empty_anchored_block_with_stacks_height_pubkh<'a>(clarity_tx: &mut ClarityTx<'a>, builder: &mut StacksBlockBuilder, miner: &mut TestMiner, burnchain_height: usize, parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {
        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);

        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);

        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();

        let mut stacks_block = builder.mine_anchored_block(clarity_tx);

        let mut pubkh_bytes = [0u8; 20];
        pubkh_bytes[0..8].copy_from_slice(&stacks_block.header.total_work.work.to_be_bytes());

        stacks_block.header.microblock_pubkey_hash = Hash160(pubkh_bytes);
        
        test_debug!("Produce anchored stacks block at burnchain height {} stacks height {} pubkeyhash {}", burnchain_height, stacks_block.header.total_work.work, &stacks_block.header.microblock_pubkey_hash);
        (stacks_block, vec![])
    }
    
    pub fn make_smart_contract(miner: &mut TestMiner, burnchain_height: usize, stacks_block_height: usize) -> StacksTransaction {
        // make a smart contract
        let contract = "
        (define-data-var bar int 0)
        (define-public (get-bar) (ok (var-get bar)))
        (define-public (set-bar (x int) (y int))
          (begin (var-set bar (/ x y)) (ok (var-get bar))))";
        
        test_debug!("Make smart contract block at hello-world-{}-{}", burnchain_height, stacks_block_height);

        let mut tx_contract = StacksTransaction::new(TransactionVersion::Testnet,
                                                     miner.as_transaction_auth().unwrap(),
                                                     TransactionPayload::new_smart_contract(&format!("hello-world-{}-{}", burnchain_height, stacks_block_height), &contract.to_string()).unwrap());

        tx_contract.chain_id = 0x80000000;
        tx_contract.auth.set_origin_nonce(miner.get_nonce());
        tx_contract.set_fee_rate(0);
        
        let mut tx_signer = StacksTransactionSigner::new(&tx_contract);
        miner.sign_as_origin(&mut tx_signer);
        let tx_contract_signed = tx_signer.get_tx().unwrap();

        tx_contract_signed
    }

    /// paired with make_smart_contract
    pub fn make_contract_call(miner: &mut TestMiner, burnchain_height: usize, stacks_block_height: usize, arg1: i128, arg2: i128) -> StacksTransaction {
        let addr = miner.origin_address().unwrap();
        let mut tx_contract_call = StacksTransaction::new(TransactionVersion::Testnet,
                                                          miner.as_transaction_auth().unwrap(),
                                                          TransactionPayload::new_contract_call(addr.clone(), &format!("hello-world-{}-{}", burnchain_height, stacks_block_height), "set-bar", vec![Value::Int(arg1), Value::Int(arg2)]).unwrap());

        tx_contract_call.chain_id = 0x80000000;
        tx_contract_call.auth.set_origin_nonce(miner.get_nonce());
        tx_contract_call.set_fee_rate(0);

        let mut tx_signer = StacksTransactionSigner::new(&tx_contract_call);
        miner.sign_as_origin(&mut tx_signer);
        let tx_contract_call_signed = tx_signer.get_tx().unwrap();
        tx_contract_call_signed
    }

    /// make a token transfer
    pub fn make_token_transfer(miner: &mut TestMiner, burnchain_height: usize, nonce: Option<u64>, recipient: &StacksAddress, amount: u64, memo: &TokenTransferMemo) -> StacksTransaction {
        let addr = miner.origin_address().unwrap();
        let mut tx_stx_transfer = StacksTransaction::new(TransactionVersion::Testnet,
                                                          miner.as_transaction_auth().unwrap(),
                                                          TransactionPayload::TokenTransfer((*recipient).clone().into(), amount, (*memo).clone()));
        
        tx_stx_transfer.chain_id = 0x80000000;
        tx_stx_transfer.auth.set_origin_nonce(nonce.unwrap_or(miner.get_nonce()));
        tx_stx_transfer.set_fee_rate(0);
        
        let mut tx_signer = StacksTransactionSigner::new(&tx_stx_transfer);
        miner.sign_as_origin(&mut tx_signer);
        let tx_stx_transfer_signed = tx_signer.get_tx().unwrap();
        tx_stx_transfer_signed
    }

    /// Mine invalid token transfers
    pub fn mine_invalid_token_transfers_block<'a>(clarity_tx: &mut ClarityTx<'a>, builder: &mut StacksBlockBuilder, miner: &mut TestMiner, burnchain_height: usize, parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {
        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);
        
        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);
        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();

        let recipient = StacksAddress::new(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, Hash160([0xff; 20]));
        let tx1 = make_token_transfer(miner, burnchain_height, Some(1), &recipient, 11111, &TokenTransferMemo([1u8; 34]));
        builder.force_mine_tx(clarity_tx, &tx1).unwrap();

        let tx2 = make_token_transfer(miner, burnchain_height, Some(2), &recipient, 22222, &TokenTransferMemo([2u8; 34]));
        builder.force_mine_tx(clarity_tx, &tx2).unwrap();
        
        let tx3 = make_token_transfer(miner, burnchain_height, Some(1), &recipient, 33333, &TokenTransferMemo([3u8; 34]));
        builder.force_mine_tx(clarity_tx, &tx3).unwrap();

        let tx4 = make_token_transfer(miner, burnchain_height, Some(2), &recipient, 44444, &TokenTransferMemo([4u8; 34]));
        builder.force_mine_tx(clarity_tx, &tx4).unwrap();

        let stacks_block = builder.mine_anchored_block(clarity_tx);

        test_debug!("Produce anchored stacks block {} with invalid token transfers at burnchain height {} stacks height {}", stacks_block.block_hash(), burnchain_height, stacks_block.header.total_work.work);
        (stacks_block, vec![])
    }
    
    /// mine a smart contract in an anchored block, and mine a contract-call in the same anchored
    /// block
    pub fn mine_smart_contract_contract_call_block<'a>(clarity_tx: &mut ClarityTx<'a>, builder: &mut StacksBlockBuilder, miner: &mut TestMiner, burnchain_height: usize, parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {
        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);

        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);
        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();

        // make a smart contract
        let tx_contract_signed = make_smart_contract(miner, burnchain_height, builder.header.total_work.work as usize);
        builder.try_mine_tx(clarity_tx, &tx_contract_signed).unwrap();

        // make a contract call 
        let tx_contract_call_signed = make_contract_call(miner, burnchain_height, builder.header.total_work.work as usize, 6, 2);
        builder.try_mine_tx(clarity_tx, &tx_contract_call_signed).unwrap();

        let stacks_block = builder.mine_anchored_block(clarity_tx);

        // TODO: test value of 'bar' in last contract(s)
        
        test_debug!("Produce anchored stacks block {} with smart contract and contract call at burnchain height {} stacks height {}", stacks_block.block_hash(), burnchain_height, stacks_block.header.total_work.work);
        (stacks_block, vec![])
    }
    
    /// mine a smart contract in an anchored block, and mine some contract-calls to it in a microblock tail
    pub fn mine_smart_contract_block_contract_call_microblock<'a>(clarity_tx: &mut ClarityTx<'a>, builder: &mut StacksBlockBuilder, miner: &mut TestMiner, burnchain_height: usize, parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {
        if burnchain_height > 0 && builder.chain_tip.anchored_header.total_work.work > 0 {
            // find previous contract in this fork
            for i in (0..burnchain_height).rev() {
                let prev_contract_id = QualifiedContractIdentifier::new(StandardPrincipalData::from(miner.origin_address().unwrap()), ContractName::try_from(format!("hello-world-{}-{}", i, builder.chain_tip.anchored_header.total_work.work).as_str()).unwrap());
                let contract = StacksChainState::get_contract(clarity_tx, &prev_contract_id).unwrap();
                if contract.is_none() {
                    continue;
                }

                let prev_bar_value = StacksChainState::get_data_var(clarity_tx, &prev_contract_id, "bar").unwrap();
                assert_eq!(prev_bar_value, Some(Value::Int(3)));
                break;
            }
        }

        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);

        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);
        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();

        // make a smart contract
        let tx_contract_signed = make_smart_contract(miner, burnchain_height, builder.header.total_work.work as usize);
        builder.try_mine_tx(clarity_tx, &tx_contract_signed).unwrap();

        let stacks_block = builder.mine_anchored_block(clarity_tx);

        let mut microblocks = vec![];
        for i in 0..3 {
            // make a contract call
            let tx_contract_call_signed = make_contract_call(miner, burnchain_height, builder.header.total_work.work as usize, 6, 2);
            builder.try_mine_tx(clarity_tx, &tx_contract_call_signed).unwrap();
            
            // put the contract-call into a microblock 
            let microblock = builder.mine_next_microblock().unwrap();
            microblocks.push(microblock);
        }

        test_debug!("Produce anchored stacks block {} with smart contract and {} microblocks with contract call at burnchain height {} stacks height {}", 
                    stacks_block.block_hash(), microblocks.len(), burnchain_height, stacks_block.header.total_work.work);

        (stacks_block, microblocks)
    }
    
    /// mine a smart contract in an anchored block, and mine a contract-call to it in a microblock.
    /// Make it so all microblocks throw a runtime exception, but confirm that they are still mined
    /// anyway.
    pub fn mine_smart_contract_block_contract_call_microblock_exception<'a>(clarity_tx: &mut ClarityTx<'a>, 
                                                                        builder: &mut StacksBlockBuilder, 
                                                                        miner: &mut TestMiner, 
                                                                        burnchain_height: usize, 
                                                                        parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {
        if burnchain_height > 0 && builder.chain_tip.anchored_header.total_work.work > 0 {
            // find previous contract in this fork
            for i in (0..burnchain_height).rev() {
                let prev_contract_id = QualifiedContractIdentifier::new(StandardPrincipalData::from(miner.origin_address().unwrap()), ContractName::try_from(format!("hello-world-{}-{}", i, builder.chain_tip.anchored_header.total_work.work).as_str()).unwrap());
                let contract = StacksChainState::get_contract(clarity_tx, &prev_contract_id).unwrap();
                if contract.is_none() {
                    continue;
                }

                test_debug!("Found contract {:?}", &prev_contract_id);
                let prev_bar_value = StacksChainState::get_data_var(clarity_tx, &prev_contract_id, "bar").unwrap();
                assert_eq!(prev_bar_value, Some(Value::Int(0)));
                break;
            }
        }

        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);

        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);
        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();

        // make a smart contract
        let tx_contract_signed = make_smart_contract(miner, burnchain_height, builder.header.total_work.work as usize);
        builder.try_mine_tx(clarity_tx, &tx_contract_signed).unwrap();

        let stacks_block = builder.mine_anchored_block(clarity_tx);

        let mut microblocks = vec![];
        for i in 0..3 {
            // make a contract call (note: triggers a divide-by-zero runtime error)
            let tx_contract_call_signed = make_contract_call(miner, burnchain_height, builder.header.total_work.work as usize, 6, 0);
            builder.try_mine_tx(clarity_tx, &tx_contract_call_signed).unwrap();
            
            // put the contract-call into a microblock 
            let microblock = builder.mine_next_microblock().unwrap();
            microblocks.push(microblock);
        }
        
        test_debug!("Produce anchored stacks block {} with smart contract and {} microblocks with contract call at burnchain height {} stacks height {}", 
                    stacks_block.block_hash(), microblocks.len(), burnchain_height, stacks_block.header.total_work.work);

        (stacks_block, microblocks)
    }

    /*
    // TODO: blocked on get-block-info's reliance on get_simmed_block_height

    /// In the first epoch, mine an anchored block followed by 100 microblocks.
    /// In all following epochs, build off of one of the microblocks.
    fn mine_smart_contract_block_contract_call_microblocks_same_stream<'a>(clarity_tx: &mut ClarityTx<'a>, 
                                                                           builder: &mut StacksBlockBuilder, 
                                                                           miner: &mut TestMiner, 
                                                                           burnchain_height: usize, 
                                                                           parent_microblock_header: Option<&StacksMicroblockHeader>) -> (StacksBlock, Vec<StacksMicroblock>) {

        let miner_account = StacksChainState::get_account(clarity_tx, &miner.origin_address().unwrap().to_account_principal());
        miner.set_nonce(miner_account.nonce);

        // make a coinbase for this miner
        let tx_coinbase_signed = make_coinbase(miner, burnchain_height);
        builder.try_mine_tx(clarity_tx, &tx_coinbase_signed).unwrap();
        
        if burnchain_height == 0 {
            // make a smart contract
            let tx_contract_signed = make_smart_contract(miner, burnchain_height, builder.header.total_work.work as usize);
            builder.try_mine_tx(clarity_tx, &tx_contract_signed).unwrap();
            
            let stacks_block = builder.mine_anchored_block(clarity_tx);

            // create the initial 20 contract calls in microblocks
            let mut stacks_microblocks = vec![];
            for i in 0..20 {
                let tx_contract_call_signed = make_contract_call(miner, burnchain_height, builder.header.total_work.work, 6, 2);
                builder.try_mine_tx(clarity_tx, &tx_contract_call_signed).unwrap();

                let microblock = builder.mine_next_microblock().unwrap();
                stacks_microblocks.push(microblock);
            }

            (stacks_block, stacks_microblocks)
        }
        else {
            // set parent at block 1
            let first_block_hash = clarity_get_block_hash(clarity_tx, 1).unwrap();
            builder.set_parent_block(&first_block_hash);
            
            let mut stacks_block = builder.mine_anchored_block(clarity_tx);

            // re-create the initial 100 contract calls in microblocks
            let mut stacks_microblocks = vec![];
            for i in 0..20 {
                let tx_contract_call_signed = make_contract_call(miner, burnchain_height, builder.header.total_work.work, 6, 2);
                builder.try_mine_tx(clarity_tx, &tx_contract_call_signed).unwrap();

                let microblock = builder.mine_next_microblock().unwrap();
                stacks_microblocks.push(microblock);
            }

            // builder.set_parent_microblock(&stacks_microblocks[burnchain_height].block_hash(), stacks_microblocks[burnchain_height].header.sequence);
            stacks_block.header.parent_microblock = stacks_microblocks[burnchain_height].block_hash();
            stacks_block.header.parent_microblock_sequence = stacks_microblocks[burnchain_height].header.sequence;

            (stacks_block, vec![])
        }
    }
    */

    #[test]
    fn mine_anchored_empty_blocks_single() {
        mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"empty-anchored-blocks".to_string(), 10, mine_empty_anchored_block, |_, _| true);
    }
    
    #[test]
    fn mine_anchored_empty_blocks_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"empty-anchored-blocks-random".to_string(), 10, mine_empty_anchored_block, |_, _| true);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_empty_blocks_multiple_miners() {
        mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"empty-anchored-blocks-multiple-miners".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
    }
    
    #[test]
    fn mine_anchored_empty_blocks_multiple_miners_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"empty-anchored-blocks-multiple-miners-random".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_empty_blocks_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"empty-anchored-blocks-stacks-fork".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
    }
    
    #[test]
    fn mine_anchored_empty_blocks_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"empty-anchored-blocks-stacks-fork-random".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_empty_blocks_burnchain_fork() {
        mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"empty-anchored-blocks-burnchain-fork".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
    }
    
    #[test]
    fn mine_anchored_empty_blocks_burnchain_fork_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"empty-anchored-blocks-burnchain-fork-random".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_empty_blocks_burnchain_fork_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"empty-anchored-blocks-burnchain-stacks-fork".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
    }
    
    #[test]
    fn mine_anchored_empty_blocks_burnchain_fork_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"empty-anchored-blocks-burnchain-stacks-fork-random".to_string(), 10, mine_empty_anchored_block, mine_empty_anchored_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_single() {
        mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"smart-contract-contract-call-anchored-blocks".to_string(), 10, mine_smart_contract_contract_call_block, |_, _| true);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_single_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"smart-contract-contract-call-anchored-blocks-random".to_string(), 10, mine_smart_contract_contract_call_block, |_, _| true);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_multiple_miners() {
        mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"smart-contract-contract-call-anchored-blocks-multiple-miners".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_multiple_miners_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"smart-contract-contract-call-anchored-blocks-multiple-miners-random".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"smart-contract-contract-call-anchored-blocks-stacks-fork".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"smart-contract-contract-call-anchored-blocks-stacks-fork-random".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_burnchain_fork() {
        mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"smart-contract-contract-call-anchored-blocks-burnchain-fork".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_burnchain_fork_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"smart-contract-contract-call-anchored-blocks-burnchain-fork-random".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_burnchain_fork_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"smart-contract-contract-call-anchored-blocks-burnchain-stacks-fork".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
    }
    
    #[test]
    fn mine_anchored_smart_contract_contract_call_blocks_burnchain_fork_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"smart-contract-contract-call-anchored-blocks-burnchain-stacks-fork-random".to_string(), 10, mine_smart_contract_contract_call_block, mine_smart_contract_contract_call_block);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_single() {
        mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"smart-contract-block-contract-call-microblock".to_string(), 10, mine_smart_contract_block_contract_call_microblock, |_, _| true);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_single_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"smart-contract-block-contract-call-microblock-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock, |_, _| true);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_multiple_miners() {
        mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-multiple-miners".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_multiple_miners_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-multiple-miners-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-stacks-fork".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
    }
    
    #[test]
    #[ignore]
    fn mine_anchored_smart_contract_block_contract_call_microblock_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-stacks-fork-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_burnchain_fork() {
        mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-burnchain-fork".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_burnchain_fork_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-burnchain-fork-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_burnchain_fork_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-burnchain-stacks-fork".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_burnchain_fork_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-burnchain-stacks-fork-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock, mine_smart_contract_block_contract_call_microblock);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_single() {
        mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"smart-contract-block-contract-call-microblock-exception".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, |_, _| true);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_single_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"smart-contract-block-contract-call-microblock-exception-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, |_, _| true);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_multiple_miners() {
        mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-exception-multiple-miners".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_multiple_miners_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-exception-multiple-miners-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-exception-stacks-fork".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
    }
    
    #[test]
    #[ignore]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"smart-contract-block-contract-call-microblock-exception-stacks-fork-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_burnchain_fork() {
        mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-exception-burnchain-fork".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_burnchain_fork_random() {
        let mut miner_trace = mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-exception-burnchain-fork-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
        miner_trace_replay_randomized(&mut miner_trace);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_burnchain_fork_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-exception-burnchain-stacks-fork".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
    }
    
    #[test]
    fn mine_anchored_smart_contract_block_contract_call_microblock_exception_burnchain_fork_stacks_fork_random() {
        let mut miner_trace = mine_stacks_blocks_2_forks_2_miners_2_burnchains(&"smart-contract-block-contract-call-microblock-exception-burnchain-stacks-fork-random".to_string(), 10, mine_smart_contract_block_contract_call_microblock_exception, mine_smart_contract_block_contract_call_microblock_exception);
        miner_trace_replay_randomized(&mut miner_trace);
    }

    #[test]
    fn mine_empty_anchored_block_deterministic_pubkeyhash_burnchain_fork() {
        mine_stacks_blocks_1_fork_2_miners_2_burnchains(&"mine_empty_anchored_block_deterministic_pubkeyhash_burnchain_fork".to_string(), 10, mine_empty_anchored_block_with_burn_height_pubkh, mine_empty_anchored_block_with_burn_height_pubkh);
    }
    
    #[test]
    fn mine_empty_anchored_block_deterministic_pubkeyhash_stacks_fork() {
        mine_stacks_blocks_2_forks_2_miners_1_burnchain(&"mine_empty_anchored_block_deterministic_pubkeyhash_stacks_fork".to_string(), 10, mine_empty_anchored_block_with_stacks_height_pubkh, mine_empty_anchored_block_with_stacks_height_pubkh);
    }
    
    #[test]
    fn mine_empty_anchored_block_deterministic_pubkeyhash_stacks_fork_at_genesis() {
        mine_stacks_blocks_2_forks_at_height_2_miners_1_burnchain(&"mine_empty_anchored_block_deterministic_pubkeyhash_stacks_fork_at_genesis".to_string(), 10, 0, mine_empty_anchored_block_with_stacks_height_pubkh, mine_empty_anchored_block_with_stacks_height_pubkh);
    }

    #[test]
    fn mine_anchored_invalid_token_transfer_blocks_single() {
        let miner_trace = mine_stacks_blocks_1_fork_1_miner_1_burnchain(&"invalid-token-transfers".to_string(), 10, mine_invalid_token_transfers_block, |_, _| false);

        let full_test_name = "invalid-token-transfers-1_fork_1_miner_1_burnchain";
        let chainstate = open_chainstate(false, 0x80000000, full_test_name);

        // each block must be orphaned
        for point in miner_trace.points.iter() {
            for (height, bc) in point.block_commits.iter() {
                assert!(StacksChainState::is_block_orphaned(&chainstate.blocks_db, &bc.burn_header_hash, &bc.block_header_hash).unwrap());
            }
        }
    }

    // TODO: merge with vm/tests/integrations.rs.
    // Distinct here because we use a different testnet ID
    pub fn make_user_contract_publish(sender: &StacksPrivateKey, nonce: u64, fee_rate: u64,
                                      contract_name: &str, contract_content: &str) -> StacksTransaction {
        let name = ContractName::from(contract_name);
        let code_body = StacksString::from_string(&contract_content.to_string()).unwrap();

        let payload = TransactionSmartContract { name, code_body };

        sign_standard_singlesig_tx(payload.into(), sender, nonce, fee_rate)
    }

    pub fn make_user_stacks_transfer(sender: &StacksPrivateKey, nonce: u64, fee_rate: u64,
                                    recipient: &PrincipalData, amount: u64) -> StacksTransaction {
        let payload = TransactionPayload::TokenTransfer(recipient.clone(), amount, TokenTransferMemo([0; 34]));
        sign_standard_singlesig_tx(payload.into(), sender, nonce, fee_rate)
    }

    pub fn make_user_coinbase(sender: &StacksPrivateKey, nonce: u64, fee_rate: u64) -> StacksTransaction {
        let payload = TransactionPayload::Coinbase(CoinbasePayload([0; 32]));
        sign_standard_singlesig_tx(payload.into(), sender, nonce, fee_rate)
    }

    pub fn sign_standard_singlesig_tx(payload: TransactionPayload,
                                      sender: &StacksPrivateKey, nonce: u64, fee_rate: u64) -> StacksTransaction {
        let mut spending_condition = TransactionSpendingCondition::new_singlesig_p2pkh(StacksPublicKey::from_private(sender))
            .expect("Failed to create p2pkh spending condition from public key.");
        spending_condition.set_nonce(nonce);
        spending_condition.set_fee_rate(fee_rate);
        let auth = TransactionAuth::Standard(spending_condition);
        let mut unsigned_tx = StacksTransaction::new(TransactionVersion::Testnet, auth, payload);

        unsigned_tx.chain_id = 0x80000000;
        unsigned_tx.post_condition_mode = TransactionPostConditionMode::Allow;

        let mut tx_signer = StacksTransactionSigner::new(&unsigned_tx);
        tx_signer.sign_origin(sender).unwrap();

        tx_signer.get_tx().unwrap()
    }

    #[test]
    fn test_build_anchored_blocks_empty() {
        let peer_config = TestPeerConfig::new("test_build_anchored_blocks_empty", 2000, 2001);
        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let num_blocks = 10;
        let first_stacks_block_height = {
            let sn = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let mut last_block : Option<StacksBlock> = None;
        for tenure_id in 0..num_blocks {
            // send transactions to the mempool
            let tip = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();

            assert_eq!(tip.block_height, first_stacks_block_height + (tenure_id as u64));
            if let Some(block) = last_block {
                assert_eq!(tip.winning_stacks_block_hash, block.block_hash());
            }

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(|ref mut miner, ref mut burndb, ref mut chainstate, vrf_proof, ref parent_opt, ref parent_microblock_header_opt| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = burndb.index_conn();
                        let snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.burn_header_hash, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.burn_header_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };
                
                let parent_header_hash = parent_tip.anchored_header.block_hash();
                let parent_tip_bhh = parent_tip.burn_header_hash.clone();

                let mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

                let coinbase_tx = make_coinbase(miner, tenure_id);

                let anchored_block = StacksBlockBuilder::build_anchored_block(chainstate, &mempool, &parent_tip, tip.total_burn, vrf_proof, Hash160([tenure_id as u8; 20]), &coinbase_tx, ExecutionCost::max_value()).unwrap();
                (anchored_block.0, vec![])
            });

            last_block = Some(stacks_block.clone());

            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);
        }
    }
    
    #[test]
    fn test_build_anchored_blocks_stx_transfers_single() {
        let privk = StacksPrivateKey::from_hex("42faca653724860da7a41bfcef7e6ba78db55146f6900de8cb2a9f760ffac70c01").unwrap();
        let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();

        let mut peer_config = TestPeerConfig::new("test_build_anchored_blocks_stx_transfers_single", 2002, 2003);
        peer_config.initial_balances = vec![
            (addr.to_account_principal(), 1000000000)
        ];

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let num_blocks = 10;
        let first_stacks_block_height = {
            let sn = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let recipient_addr_str = "ST1RFD5Q2QPK3E0F08HG9XDX7SSC7CNRS0QR0SGEV";
        let recipient = StacksAddress::from_string(recipient_addr_str).unwrap();
        let mut sender_nonce = 0;

        let mut last_block = None;
        for tenure_id in 0..num_blocks {
            // send transactions to the mempool
            let tip = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(|ref mut miner, ref mut burndb, ref mut chainstate, vrf_proof, ref parent_opt, ref parent_microblock_header_opt| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = burndb.index_conn();
                        let snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.burn_header_hash, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.burn_header_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };

                let parent_header_hash = parent_tip.anchored_header.block_hash();
                let parent_tip_bhh = parent_tip.burn_header_hash.clone();

                let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

                let coinbase_tx = make_coinbase(miner, tenure_id);

                // TODO: jude -- for some reason, this doesn't work on the first tenure.  Initial
                // balances aren't materialized if the tip is the genesis header.
                if tenure_id > 0 {
                    let stx_transfer = make_user_stacks_transfer(&privk, sender_nonce, 200, &recipient.to_account_principal(), 1);
                    sender_nonce += 1;

                    mempool.submit(&parent_tip_bhh, &parent_header_hash, stx_transfer).unwrap();
                } 
                let anchored_block = StacksBlockBuilder::build_anchored_block(chainstate, &mempool, &parent_tip, tip.total_burn, vrf_proof, Hash160([tenure_id as u8; 20]), &coinbase_tx, ExecutionCost::max_value()).unwrap();
                (anchored_block.0, vec![])
            });
            
            last_block = Some(stacks_block.clone());

            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

            if tenure_id > 0 {
                // transaction was mined
                assert_eq!(stacks_block.txs.len(), 2);
                if let TransactionPayload::TokenTransfer(ref addr, ref amount, ref memo) = stacks_block.txs[1].payload {
                    assert_eq!(*addr, recipient.to_account_principal());
                    assert_eq!(*amount, 1);
                }
                else {
                    assert!(false);
                }
            }
        }
    }

    #[test]
    fn test_build_anchored_blocks_stx_transfers_multi() {
        let mut privks = vec![];
        let mut balances = vec![];
        let num_blocks = 10;

        for _ in 0..num_blocks {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();

            privks.push(privk);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("test_build_anchored_blocks_stx_transfers_multi", 2004, 2005);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let recipient_addr_str = "ST1RFD5Q2QPK3E0F08HG9XDX7SSC7CNRS0QR0SGEV";
        let recipient = StacksAddress::from_string(recipient_addr_str).unwrap();
        let mut sender_nonce = 0;

        let mut last_block = None;
        for tenure_id in 0..num_blocks {
            // send transactions to the mempool
            let tip = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(|ref mut miner, ref mut burndb, ref mut chainstate, vrf_proof, ref parent_opt, ref parent_microblock_header_opt| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = burndb.index_conn();
                        let snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.burn_header_hash, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.burn_header_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };
                
                let parent_header_hash = parent_tip.anchored_header.block_hash();
                let parent_tip_bhh = parent_tip.burn_header_hash.clone();

                let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

                let coinbase_tx = make_coinbase(miner, tenure_id);

                // TODO: jude -- for some reason, this doesn't work on the first tenure.  Initial
                // balances aren't materialized if the tip is the genesis header.
                if tenure_id > 0 {
                    for i in 0..5 {
                        let stx_transfer = make_user_stacks_transfer(&privks[i], sender_nonce, 200, &recipient.to_account_principal(), 1);
                        mempool.submit(&parent_tip_bhh, &parent_header_hash, stx_transfer).unwrap();
                    }
                    
                    // test pagination by timestamp
                    test_debug!("Delay for 1.5s");
                    sleep_ms(1500);

                    for i in 5..10 {
                        let stx_transfer = make_user_stacks_transfer(&privks[i], sender_nonce, 200, &recipient.to_account_principal(), 1);
                        mempool.submit(&parent_tip_bhh, &parent_header_hash, stx_transfer).unwrap();
                    }
                    
                    sender_nonce += 1;
                }

                let anchored_block = StacksBlockBuilder::build_anchored_block(chainstate, &mempool, &parent_tip, tip.total_burn, vrf_proof, Hash160([tenure_id as u8; 20]), &coinbase_tx, ExecutionCost::max_value()).unwrap();
                (anchored_block.0, vec![])
            });
            
            last_block = Some(stacks_block.clone());

            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

            if tenure_id > 0 {
                // transaction was mined, even though they were staggerred by time
                assert_eq!(stacks_block.txs.len(), 11);
                for i in 1..11 {
                    if let TransactionPayload::TokenTransfer(ref addr, ref amount, ref memo) = stacks_block.txs[i].payload {
                        assert_eq!(*addr, recipient.to_account_principal());
                        assert_eq!(*amount, 1);
                    }
                    else {
                        assert!(false);
                    }
                }
            }
        }
    }
    
    #[test]
    fn test_build_anchored_blocks_skip_too_expensive() {
        let privk = StacksPrivateKey::from_hex("42faca653724860da7a41bfcef7e6ba78db55146f6900de8cb2a9f760ffac70c01").unwrap();
        let privk_extra = StacksPrivateKey::from_hex("f67c7437f948ca1834602b28595c12ac744f287a4efaf70d437042a6afed81bc01").unwrap();
        let mut privks_expensive = vec![];
        let mut initial_balances = vec![];
        let num_blocks = 10;
        for i in 0..num_blocks {
            let pk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&pk)]).unwrap().to_account_principal();

            privks_expensive.push(pk);
            initial_balances.push((addr, 10000000000));
        }


        let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();
        let addr_extra = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk_extra)]).unwrap();
        
        initial_balances.push((addr.to_account_principal(), 100000000000));
        initial_balances.push((addr_extra.to_account_principal(), 200000000000));

        let mut peer_config = TestPeerConfig::new("test_build_anchored_blocks_skip_too_expensive", 2006, 2007);
        peer_config.initial_balances = initial_balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let recipient_addr_str = "ST1RFD5Q2QPK3E0F08HG9XDX7SSC7CNRS0QR0SGEV";
        let recipient = StacksAddress::from_string(recipient_addr_str).unwrap();
        let mut sender_nonce = 0;

        let mut last_block = None;
        for tenure_id in 0..num_blocks {
            // send transactions to the mempool
            let tip = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(|ref mut miner, ref mut burndb, ref mut chainstate, vrf_proof, ref parent_opt, ref parent_microblock_header_opt| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = burndb.index_conn();
                        let snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.burn_header_hash, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.burn_header_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };
                
                let parent_header_hash = parent_tip.anchored_header.block_hash();
                let parent_tip_bhh = parent_tip.burn_header_hash.clone();
                let coinbase_tx = make_coinbase(miner, tenure_id);

                let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

                // TODO: jude -- for some reason, this doesn't work on the first tenure.  Initial
                // balances aren't materialized if the tip is the genesis header.
                if tenure_id > 0 {
                    let mut expensive_part = vec![];
                    for i in 0..100 {
                        expensive_part.push(format!("(define-data-var var-{} int 0)", i));
                    }
                    let contract = format!("{}
                    (define-data-var bar int 0)
                    (define-public (get-bar) (ok (var-get bar)))
                    (define-public (set-bar (x int) (y int))
                      (begin (var-set bar (/ x y)) (ok (var-get bar))))", expensive_part.join("\n"));

                    // fee high enough to get mined first
                    let stx_transfer = make_user_stacks_transfer(&privk, sender_nonce, (4*contract.len()) as u64, &recipient.to_account_principal(), 1);
                    mempool.submit(&parent_tip_bhh, &parent_header_hash, stx_transfer).unwrap();

                    // will never get mined
                    let contract_tx = make_user_contract_publish(&privks_expensive[tenure_id], 0, (2*contract.len()) as u64, &format!("hello-world-{}", tenure_id), &contract);

                    mempool.submit(&parent_tip_bhh, &parent_header_hash, contract_tx).unwrap();
                    
                    // will get mined last
                    let stx_transfer = make_user_stacks_transfer(&privk_extra, sender_nonce, 300, &recipient.to_account_principal(), 1);
                    mempool.submit(&parent_tip_bhh, &parent_header_hash, stx_transfer).unwrap();
                    
                    sender_nonce += 1;
                }
               
                // enough for the first stx-transfer, but not for the analysis of the smart
                // contract.
                let execution_cost = ExecutionCost {
                    write_length: 100,
                    write_count: 100,
                    read_length: 100,
                    read_count: 100,
                    runtime: 3350
                };

                let anchored_block = StacksBlockBuilder::build_anchored_block(chainstate, &mempool, &parent_tip, tip.total_burn, vrf_proof, Hash160([tenure_id as u8; 20]), &coinbase_tx, execution_cost).unwrap();
                (anchored_block.0, vec![])
            });
            
            last_block = Some(stacks_block.clone());

            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

            if tenure_id > 0 {
                // expensive transaction was not mined, but the two stx-transfers were
                assert_eq!(stacks_block.txs.len(), 3);
                for tx in stacks_block.txs.iter() {
                    match tx.payload {
                        TransactionPayload::Coinbase(..) => {},
                        TransactionPayload::TokenTransfer(ref recipient, ref amount, ref memo) => {},
                        _ => {
                            assert!(false);
                        }
                    }
                }
            }
        }
    }
    
    #[test]
    fn test_build_anchored_blocks_multiple_chaintips() {
        let mut privks = vec![];
        let mut balances = vec![];
        let num_blocks = 10;

        for _ in 0..num_blocks {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();

            privks.push(privk);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("test_build_anchored_blocks_multiple_chaintips", 2008, 2009);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let mut last_block = None;
        for tenure_id in 0..num_blocks {
            // send transactions to the mempool
            let tip = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(|ref mut miner, ref mut burndb, ref mut chainstate, vrf_proof, ref parent_opt, ref parent_microblock_header_opt| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = burndb.index_conn();
                        let snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.burn_header_hash, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.burn_header_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };
                
                let parent_header_hash = parent_tip.anchored_header.block_hash();
                let parent_tip_bhh = parent_tip.burn_header_hash.clone();
                let coinbase_tx = make_coinbase(miner, tenure_id);

                let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

                if tenure_id > 0 {
                    let contract = "
                    (define-data-var bar int 0)
                    (define-public (get-bar) (ok (var-get bar)))
                    (define-public (set-bar (x int) (y int))
                      (begin (var-set bar (/ x y)) (ok (var-get bar))))";

                    let contract_tx = make_user_contract_publish(&privks[tenure_id], 0, (2*contract.len()) as u64, &format!("hello-world-{}", tenure_id), &contract);
                    mempool.submit(&parent_tip_bhh, &parent_header_hash, contract_tx).unwrap();
                }

                let execution_cost = 
                    if tenure_id < num_blocks - 1 {
                        // doesn't allow it to get mined yet, but it'll sit in the mempool.
                        ExecutionCost {
                            write_length: 0,
                            write_count: 0,
                            read_length: 0,
                            read_count: 0,
                            runtime: 0
                        }
                    }
                    else {
                        // last block allows _everything_ to get mined
                        ExecutionCost::max_value()
                    };
                
                let anchored_block = StacksBlockBuilder::build_anchored_block(chainstate, &mempool, &parent_tip, tip.total_burn, vrf_proof, Hash160([tenure_id as u8; 20]), &coinbase_tx, execution_cost).unwrap();
                (anchored_block.0, vec![])
            });
            
            last_block = Some(stacks_block.clone());

            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);

            if tenure_id < num_blocks - 1 {
                assert_eq!(stacks_block.txs.len(), 1);
            }
            else {
                assert_eq!(stacks_block.txs.len(), num_blocks);
            }
        }
    }
    
    #[test]
    fn test_build_anchored_blocks_empty_chaintips() {
        let mut privks = vec![];
        let mut balances = vec![];
        let num_blocks = 10;

        for _ in 0..num_blocks {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();

            privks.push(privk);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("test_build_anchored_blocks_empty_chaintips", 2010, 2011);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let mut last_block = None;
        for tenure_id in 0..num_blocks {
            // send transactions to the mempool
            let tip = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(|ref mut miner, ref mut burndb, ref mut chainstate, vrf_proof, ref parent_opt, ref parent_microblock_header_opt| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = burndb.index_conn();
                        let snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.burn_header_hash, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.burn_header_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };
                
                let parent_header_hash = parent_tip.anchored_header.block_hash();
                let parent_tip_bhh = parent_tip.burn_header_hash.clone();
                let coinbase_tx = make_coinbase(miner, tenure_id);

                let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();

                let anchored_block = StacksBlockBuilder::build_anchored_block(chainstate, &mempool, &parent_tip, tip.total_burn, vrf_proof, Hash160([tenure_id as u8; 20]), &coinbase_tx, ExecutionCost::max_value()).unwrap();

                // submit a transaction for the _next_ block to pick up
                if tenure_id > 0 {
                    let contract = "
                    (define-data-var bar int 0)
                    (define-public (get-bar) (ok (var-get bar)))
                    (define-public (set-bar (x int) (y int))
                      (begin (var-set bar (/ x y)) (ok (var-get bar))))";

                    let contract_tx = make_user_contract_publish(&privks[tenure_id], 0, (2*contract.len()) as u64, &format!("hello-world-{}", tenure_id), &contract);
                    mempool.submit(&parent_tip_bhh, &parent_header_hash, contract_tx).unwrap();
                }
                
                (anchored_block.0, vec![])
            });
            
            last_block = Some(stacks_block.clone());

            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);
           
            test_debug!("\n\ncheck tenure {}: {} transactions\n", tenure_id, stacks_block.txs.len());
            
            if tenure_id > 1 {
                // two transactions after the first two tenures
                assert_eq!(stacks_block.txs.len(), 2);
            }
            else {
                assert_eq!(stacks_block.txs.len(), 1);
            }
        }
    }
    
    #[test]
    fn test_build_anchored_blocks_too_expensive_transactions() {
        let mut privks = vec![];
        let mut balances = vec![];
        let num_blocks = 3;

        for _ in 0..num_blocks {
            let privk = StacksPrivateKey::new();
            let addr = StacksAddress::from_public_keys(C32_ADDRESS_VERSION_TESTNET_SINGLESIG, &AddressHashMode::SerializeP2PKH, 1, &vec![StacksPublicKey::from_private(&privk)]).unwrap();

            privks.push(privk);
            balances.push((addr.to_account_principal(), 100000000));
        }

        let mut peer_config = TestPeerConfig::new("test_build_anchored_blocks_too_expensive_transactions", 2012, 2013);
        peer_config.initial_balances = balances;

        let mut peer = TestPeer::new(peer_config);

        let chainstate_path = peer.chainstate_path.clone();

        let first_stacks_block_height = {
            let sn = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();
            sn.block_height
        };

        let mut last_block = None;
        for tenure_id in 0..num_blocks {
            // send transactions to the mempool
            let tip = BurnDB::get_canonical_burn_chain_tip(&peer.burndb.as_ref().unwrap().conn()).unwrap();

            let (burn_ops, stacks_block, microblocks) = peer.make_tenure(|ref mut miner, ref mut burndb, ref mut chainstate, vrf_proof, ref parent_opt, ref parent_microblock_header_opt| {
                let parent_tip = match parent_opt {
                    None => {
                        StacksChainState::get_genesis_header_info(&chainstate.headers_db).unwrap()
                    }
                    Some(block) => {
                        let ic = burndb.index_conn();
                        let snapshot = BurnDB::get_block_snapshot_for_winning_stacks_block(&ic, &tip.burn_header_hash, &block.block_hash()).unwrap().unwrap();      // succeeds because we don't fork
                        StacksChainState::get_anchored_block_header_info(&chainstate.headers_db, &snapshot.burn_header_hash, &snapshot.winning_stacks_block_hash).unwrap().unwrap()
                    }
                };
                
                let parent_header_hash = parent_tip.anchored_header.block_hash();
                let parent_tip_bhh = parent_tip.burn_header_hash.clone();
                let coinbase_tx = make_coinbase(miner, tenure_id);

                let mut mempool = MemPoolDB::open(false, 0x80000000, &chainstate_path).unwrap();
                
                if tenure_id == 2 {
                    let contract = "
                    (define-data-var bar int 0)
                    (define-public (get-bar) (ok (var-get bar)))
                    (define-public (set-bar (x int) (y int))
                      (begin (var-set bar (/ x y)) (ok (var-get bar))))";

                    // should be mined once
                    let contract_tx = make_user_contract_publish(&privks[tenure_id], 0, 100000000 / 2 + 1, &format!("hello-world-{}", tenure_id), &contract);
                    let mut contract_tx_bytes = vec![];
                    contract_tx.consensus_serialize(&mut contract_tx_bytes).unwrap();
                    mempool.submit_raw(&parent_tip_bhh, &parent_header_hash, contract_tx_bytes).unwrap();

                    eprintln!("\n\ntransaction:\n{:#?}\n\n", &contract_tx);
                   
                    sleep_ms(2000);

                    // should never be mined
                    let contract_tx = make_user_contract_publish(&privks[tenure_id], 1, 100000000 / 2, &format!("hello-world-{}-2", tenure_id), &contract);
                    let mut contract_tx_bytes = vec![];
                    contract_tx.consensus_serialize(&mut contract_tx_bytes).unwrap();
                    mempool.submit_raw(&parent_tip_bhh, &parent_header_hash, contract_tx_bytes).unwrap();
                    
                    eprintln!("\n\ntransaction:\n{:#?}\n\n", &contract_tx);
                    
                    sleep_ms(2000);
                }

                let anchored_block = StacksBlockBuilder::build_anchored_block(chainstate, &mempool, &parent_tip, tip.total_burn, vrf_proof, Hash160([tenure_id as u8; 20]), &coinbase_tx, ExecutionCost::max_value()).unwrap();

                (anchored_block.0, vec![])
            });
            
            last_block = Some(stacks_block.clone());

            peer.next_burnchain_block(burn_ops.clone());
            peer.process_stacks_epoch_at_tip(&stacks_block, &microblocks);
           
            test_debug!("\n\ncheck tenure {}: {} transactions\n", tenure_id, stacks_block.txs.len());
            
            // assert_eq!(stacks_block.txs.len(), 1);
        }
    }

    // TODO: invalid block with duplicate microblock public key hash (okay between forks, but not
    // within the same fork)
    // TODO: (BLOCKED) build off of different points in the same microblock stream
    // TODO; skipped blocks
    // TODO: missing blocks
    // TODO: invalid blocks
    // TODO: no-sortition
    // TODO: burnchain forks, and we mine the same anchored stacks block in the beginnings of the two descendent
    // forks.  Verify all descendents are unique -- if A --> B and A --> C, and B --> D and C -->
    // E, and B == C, verify that it is never the case that D == E (but it is allowed that B == C
    // if the burnchain forks).
    // TODO: confirm that if A is accepted but B is rejected, then C must also be rejected even if
    // it's on a different burnchain fork.
    // TODO: confirm that we can process B and C separately, even though they're the same block
    // TODO: poison microblocks
    // TODO: verify that the Clarity MARF stores _only_ Clarity data
}
