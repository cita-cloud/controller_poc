// Copyright Rivtower Technologies LLC.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::auth::Authentication;
use crate::error::Error;
use crate::node_manager::ChainStatus;
use crate::pool::Pool;
use crate::util::*;
use crate::utxo_set::{SystemConfig, LOCK_ID_BLOCK_INTERVAL, LOCK_ID_VALIDATORS};
use crate::GenesisBlock;
use cita_cloud_proto::blockchain::raw_transaction::Tx;
use cita_cloud_proto::blockchain::{
    Block, BlockHeader, RawTransaction, RawTransactions,
};
use cita_cloud_proto::common::{
    proposal_enum::Proposal, BftProposal, ConsensusConfiguration, Hash, ProposalEnum,
};
use futures::future;
use futures::future::FutureExt;
use log::{info, warn};
use prost::Message;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const FORCE_IN_SYNC: u64 = 6;

#[derive(PartialEq)]
pub enum ChainStep {
    SyncStep,
    OnlineStep,
}

#[allow(dead_code)]
pub struct Chain {
    block_number: u64,
    block_hash: Vec<u8>,
    block_delay_number: u32,
    // hashmap for each index
    // key of hashmap is block_hash
    // value of hashmap is (block, proof)
    #[allow(clippy::type_complexity)]
    fork_tree: Vec<HashMap<Vec<u8>, Block>>,
    main_chain: Vec<Vec<u8>>,
    main_chain_tx_hash: Vec<Vec<u8>>,
    candidate_block: Option<(u64, Vec<u8>, Block)>,
    pool: Arc<RwLock<Pool>>,
    // todo auth set in controller not chain
    auth: Arc<RwLock<Authentication>>,
    genesis: GenesisBlock,
    key_id: u64,
    node_address: Vec<u8>,
}

impl Chain {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_delay_number: u32,
        current_block_number: u64,
        current_block_hash: Vec<u8>,
        pool: Arc<RwLock<Pool>>,
        auth: Arc<RwLock<Authentication>>,
        genesis: GenesisBlock,
        key_id: u64,
        node_address: Vec<u8>,
    ) -> Self {
        let fork_tree_size = (block_delay_number * 2 + 2) as usize;
        let mut fork_tree = Vec::with_capacity(fork_tree_size);
        for _ in 0..=fork_tree_size {
            fork_tree.push(HashMap::new());
        }

        Chain {
            block_number: current_block_number,
            block_hash: current_block_hash,
            block_delay_number,
            fork_tree,
            main_chain: Vec::new(),
            main_chain_tx_hash: Vec::new(),
            candidate_block: None,
            pool,
            auth,
            genesis,
            key_id,
            node_address,
        }
    }

    pub async fn init(&self, init_block_number: u64) {
        if init_block_number == 0 {
            info!("finalize genesis block");
            self.finalize_block(self.genesis.genesis_block(), self.block_hash.clone())
                .await;
        }
    }

    pub async fn init_auth(&self, init_block_number: u64) {
        let mut auth = self.auth.write().await;
        auth.init(init_block_number).await;
    }

    pub fn get_genesis_block(&self) -> Block {
        self.genesis.genesis_block()
    }

    pub fn get_block_number(&self, is_pending: bool) -> u64 {
        if is_pending {
            self.block_number + self.main_chain.len() as u64
        } else {
            self.block_number
        }
    }

    pub async fn extract_proposal_info(&self, h: u64) -> Option<(Vec<u8>, Vec<u8>)> {
        let pre_h = h - self.block_delay_number as u64 - 1;
        let key = pre_h.to_be_bytes().to_vec();

        let state_root = {
            let ret = load_data(6, key.clone()).await;
            if ret.is_err() {
                warn!("get_proposal get state_root failed");
                return None;
            }
            ret.unwrap()
        };

        let proof = {
            let ret = get_compact_block(pre_h).await;
            if ret.is_none() {
                warn!("get_proposal get proof failed");
                return None;
            }
            ret.unwrap().1
        };

        Some((state_root, proof))
    }

    pub async fn get_proposal(&self) -> Result<(u64, Vec<u8>), Error> {
        if let Some((h, _, block)) = self.candidate_block.clone() {
            return Ok((h, self.assemble_proposal(block, h).await?));
        }
        Err(Error::NoCandidate)
    }

    pub async fn assemble_proposal(&self, mut block: Block, height: u64) -> Result<Vec<u8>, Error> {
        block.proof = Vec::new();
        if let Some((pre_state_root, pre_proof)) = self.extract_proposal_info(height).await {
            let proposal = ProposalEnum {
                proposal: Some(Proposal::BftProposal(BftProposal {
                    proposal: Some(block),
                    pre_state_root,
                    pre_proof,
                })),
            };

            let mut proposal_bytes = Vec::new();
            proposal
                .encode(&mut proposal_bytes)
                .map_err(|_| Error::EncodeError(format!("encode proposal error")))?;

            Ok(proposal_bytes)
        } else {
            Err(Error::NoEarlyStatus)
        }
    }

    pub async fn add_remote_proposal(
        &mut self,
        block_hash: &[u8],
        block: Block,
    ) -> Result<bool, Error> {
        let header = block.header.clone().unwrap();
        let block_height = header.height;
        if block_height <= self.block_number {
            return Err(Error::ProposalTooLow(block_height, self.block_number));
        }

        if block_height - self.block_number > (self.block_delay_number * 2 + 2) as u64 {
            return Err(Error::ProposalTooHigh(block_height, self.block_number));
        }

        if !self.fork_tree[(block_height - self.block_number - 1) as usize].contains_key(block_hash)
        {
            self.fork_tree[(block_height - self.block_number - 1) as usize]
                .insert(block_hash.to_vec(), block);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn is_candidate(&self, block_hash: &[u8]) -> bool {
        if let Some((_, hash, _)) = &self.candidate_block {
            hash == block_hash
        } else {
            false
        }
    }

    pub async fn add_proposal(&mut self, global_status: &ChainStatus) -> Result<(), Error> {
        if self.candidate_block.is_some()
            || self.next_step(global_status).await == ChainStep::SyncStep
        {
            Ok(())
        } else {
            info!("main_chain_tx_hash len {}", self.main_chain_tx_hash.len());

            let (tx_hash_list, tx_list) = {
                let mut pool = self.pool.write().await;
                info!("tx poll len {}", pool.len());
                pool.package(self.block_number + 1)
            };

            let mut data = Vec::new();
            for hash in tx_hash_list.iter() {
                data.extend_from_slice(hash);
            }
            let transactions_root = hash_data(&data);

            let prevhash = if self.main_chain.is_empty() {
                self.block_hash.clone()
            } else {
                self.main_chain.last().unwrap().to_owned()
            };
            let height = self.block_number + self.main_chain.len() as u64 + 1;

            info!("proposal {} prevhash 0x{}", height, hex::encode(&prevhash));
            let header = BlockHeader {
                prevhash,
                timestamp: unix_now(),
                height,
                transactions_root,
                proposer: self.node_address.clone(),
            };

            let full_block = Block {
                version: 0,
                header: Some(header.clone()),
                body: Some(RawTransactions { body: tx_list }),
                proof: vec![],
            };

            let mut block_header_bytes = Vec::new();
            header
                .encode(&mut block_header_bytes)
                .expect("encode block header failed");

            let block_hash = hash_data(&block_header_bytes);

            info!(
                "proposal {} block_hash 0x{}",
                height,
                hex::encode(&block_hash)
            );

            self.candidate_block = Some((height, block_hash.clone(), full_block.clone()));
            self.fork_tree[self.main_chain.len()].insert(block_hash, full_block);

            Ok(())
        }
    }

    pub fn clear_proposal(&mut self) {
        self.candidate_block = None;
    }

    pub async fn check_proposal(&self, h: u64, proposal: ProposalEnum) -> Result<bool, Error> {
        if h <= self.block_number {
            return Err(Error::ProposalTooLow(h, self.block_number));
        }

        if h > self.block_number + self.block_delay_number as u64 + 1 {
            return Err(Error::ProposalTooHigh(h, self.block_number));
        }

        match proposal.proposal {
            Some(Proposal::BftProposal(bft_proposal)) => {
                let pre_h = h - self.block_delay_number as u64 - 1;
                let key = pre_h.to_be_bytes().to_vec();

                let state_root = load_data(6, key)
                    .await
                    .map_err(Error::InternalError)
                    .unwrap();

                let proof = get_compact_block(pre_h).await.unwrap().1;

                if bft_proposal.pre_state_root == state_root && bft_proposal.pre_proof == proof {
                    Ok(true)
                } else {
                    warn!("check_proposal failed!\nproposal_state_root {}\nstate_root {}\nproposal_proof {}\nproof {}",
                          hex::encode(&bft_proposal.pre_state_root),
                          hex::encode(&state_root),
                          hex::encode(&bft_proposal.pre_proof),
                          hex::encode(&proof),
                    );
                    Err(Error::ProposalCheckError)
                }
            }
            None => Err(Error::NoneProposal),
        }
    }

    async fn finalize_block(&self, full_block: Block, block_hash: Vec<u8>) {
        use tokio::task::JoinHandle;

        // current task handles
        let mut handles = Vec::<JoinHandle<()>>::new();
        let do_store = |region, key, value| {
            tokio::spawn(store_data(region, key, value).map(|res| match res {
                Ok(true) => (),
                Ok(false) => panic!("Store data failed: storage return false"),
                Err(e) => panic!("Store data failed: {}", e),
            }))
        };

        let compact_block = full_to_compact(full_block.clone());
        let compact_block_body = compact_block.body.unwrap();
        let tx_hash_list = compact_block_body.tx_hashes.clone();

        let block_header = full_block.header.clone().unwrap();
        let block_height = block_header.height;
        let key = block_height.to_be_bytes().to_vec();

        // region 5 : block_height - proof
        // store_data(self.storage_port, 5, key.clone(), proof.to_owned())
        //    .await
        //    .expect("store proof failed");

        // region 4 : block_height - block hash
        handles.push(do_store(4, key.clone(), block_hash.clone()));

        // region 8 : block hash - block_height
        handles.push(do_store(8, block_hash.clone(), key.clone()));

        if !check_block_exists(block_height) {
            // region 3: block_height - block body
            let mut block_body_bytes = Vec::new();
            compact_block_body
                .encode(&mut block_body_bytes)
                .expect("encode block body failed");
            // store_data(self.storage_port, 3, key.clone(), block_body_bytes)
            //    .await
            //    .expect("store_data failed");

            // region 2: block_height - block header
            let mut block_header_bytes = Vec::new();
            block_header
                .encode(&mut block_header_bytes)
                .expect("encode block header failed");
            // store_data(self.storage_port, 2, key.clone(), block_header_bytes)
            //    .await
            //    .expect("store_data failed");

            // store block with proof in sync folder.
            let proof = full_block.proof.clone();
            handles.push(tokio::spawn(async move {
                write_block(
                    block_height,
                    block_header_bytes.as_slice(),
                    block_body_bytes.as_slice(),
                    &proof,
                )
                .await;
            }));
        }

        // region 1: tx_hash - tx
        if let Some(raw_txs) = full_block.body.clone() {
            let store_handles = raw_txs
                .body
                .iter()
                .enumerate()
                .filter_map(|(tx_index, raw_tx)| {
                    let tx_hash = match &raw_tx.tx {
                        Some(Tx::UtxoTx(utxo_tx)) => &utxo_tx.transaction_hash,
                        Some(Tx::NormalTx(normal_tx)) => &normal_tx.transaction_hash,
                        None => return None,
                    }
                    .clone();

                    let mut tx_bytes = Vec::new();
                    raw_tx.encode(&mut tx_bytes).unwrap();

                    let h = tokio::spawn(async move {
                        let jobs = vec![
                            write_tx(&tx_hash, &tx_bytes).boxed(),
                            store_tx_info(&tx_hash, block_height, tx_index).boxed(),
                        ];
                        future::join_all(jobs).await;
                    });
                    Some(h)
                });
            handles.extend(store_handles);

            for raw_tx in raw_txs.body.into_iter() {
                if let Some(Tx::UtxoTx(utxo_tx)) = raw_tx.tx.clone() {
                    if {
                        let mut auth = self.auth.write().await;
                        auth.update_system_config(&utxo_tx)
                    } {
                        // if sys_config changed, store utxo tx hash into global region
                        let lock_id = utxo_tx.transaction.unwrap().lock_id;
                        let key = lock_id.to_be_bytes().to_vec();
                        store_data(0, key, utxo_tx.transaction_hash.clone())
                            .await
                            .expect("store_data failed");

                        if lock_id == LOCK_ID_VALIDATORS || lock_id == LOCK_ID_BLOCK_INTERVAL {
                            let sys_config = {
                                let auth = self.auth.read().await;
                                auth.get_system_config()
                            };
                            reconfigure(ConsensusConfiguration {
                                height: block_height,
                                block_interval: sys_config.block_interval,
                                validators: sys_config.validators,
                            })
                            .await
                            .expect("reconfigure failed");
                        }
                    }
                };
            }
        }

        // exec block
        // if exec_block after consensus, we should ignore the error, because all node will have same error.
        // if exec_block before consensus, we shouldn't ignore, because it means that block is invalid.
        // TODO: get length of hash from kms
        let executed_block_hash = exec_block(full_block)
            .await
            .unwrap_or_else(|_| vec![0u8; 32]);
        // region 6 : block_height - executed_block_hash
        handles.push(do_store(6, key.clone(), executed_block_hash));

        // this must be before update pool
        {
            let mut auth = self.auth.write().await;
            auth.insert_tx_hash(block_height, tx_hash_list.clone());
        }
        // update pool
        {
            let mut pool = self.pool.write().await;
            pool.update(&tx_hash_list);
        }

        info!(
            "finalize_block: {}, block_hash: 0x{}",
            block_height,
            hex::encode(&block_hash)
        );

        // region 0: 0 - current height; 1 - current hash
        handles.push(do_store(0, 0u64.to_be_bytes().to_vec(), key));
        handles.push(do_store(0, 1u64.to_be_bytes().to_vec(), block_hash));

        let bg_results = future::join_all(handles).await;
        if bg_results.into_iter().any(|res| res.is_err()) {
            panic!("finalizing block failed. Background task error");
        }
    }

    pub async fn commit_block(
        &mut self,
        height: u64,
        proposal: &[u8],
        proof: &[u8],
    ) -> Result<(ConsensusConfiguration, ChainStatus), Error> {
        if height <= self.block_number {
            return Err(Error::ProposalTooLow(height, self.block_number));
        }

        if height > self.block_number + self.block_delay_number as u64 + 1 {
            return Err(Error::ProposalTooHigh(height, self.block_number));
        }

        let bft_proposal = match ProposalEnum::decode(proposal)
            .map_err(|_| Error::DecodeError(format!("decode ProposalEnum failed")))?
            .proposal
        {
            Some(Proposal::BftProposal(bft_proposal)) => Ok(bft_proposal),
            None => Err(Error::ExpectError(format!("no proposal found"))),
        }?;

        for (index, _map) in self.fork_tree.iter_mut().enumerate() {
            // make sure the block in fork_tree
            if let Some(mut full_block) = bft_proposal.proposal {
                // store proof
                full_block.proof = proof.to_vec();
                let compact_block = full_to_compact(full_block.clone());

                // try to backwards found a candidate_chain
                let mut candidate_chain = Vec::new();
                let mut candidate_chain_tx_hash = Vec::new();

                candidate_chain.push(get_block_hash(full_block.header.as_ref())?);
                candidate_chain_tx_hash.extend_from_slice(&compact_block.body.unwrap().tx_hashes);

                let mut prev_hash = full_block.header.clone().unwrap().prevhash;
                for i in 0..index {
                    let map = self.fork_tree.get(index - i - 1).unwrap();
                    if let Some(prev_full_block) = map.get(&prev_hash) {
                        if prev_full_block.proof.is_empty() {
                            warn!("candidate_chain has no proof");
                            return Err(Error::ExpectError(
                                "candidate_chain has no proof".to_string(),
                            ));
                        }
                        let prev_compact_block = full_to_compact(prev_full_block.to_owned());
                        candidate_chain.push(prev_hash.clone());
                        for hash in prev_compact_block.to_owned().body.unwrap().tx_hashes {
                            if candidate_chain_tx_hash.contains(&hash) {
                                // candidate_chain has dup tx, so failed
                                warn!("candidate_chain has dup tx");
                                return Err(Error::ExpectError(
                                    "candidate_chain has dup tx".to_string(),
                                ));
                            }
                        }
                        candidate_chain_tx_hash.extend_from_slice(
                            &prev_compact_block.to_owned().body.unwrap().tx_hashes,
                        );
                        prev_hash = prev_full_block.to_owned().header.unwrap().prevhash;
                    } else {
                        // candidate_chain interrupted, so failed
                        warn!("candidate_chain interrupted");
                        return Err(Error::ExpectError(
                            "candidate_chain interrupted".to_string(),
                        ));
                    }
                }

                if prev_hash != self.block_hash {
                    warn!("candidate_chain can't fit finalized block");
                    // break this invalid chain
                    let blk_hash = candidate_chain.last().unwrap();
                    self.fork_tree.get_mut(0).unwrap().remove(blk_hash);
                    return Err(Error::ExpectError(
                        "candidate_chain can't fit finalized block".to_string(),
                    ));
                }

                // if candidate_chain longer than original main_chain
                if candidate_chain.len() > self.main_chain.len() {
                    // replace the main_chain
                    candidate_chain.reverse();
                    self.main_chain = candidate_chain;
                    self.main_chain_tx_hash = candidate_chain_tx_hash;
                    print_main_chain(&self.main_chain, self.block_number);
                    // check if any block has been finalized
                    if self.main_chain.len() > self.block_delay_number as usize {
                        let finalized_blocks_number =
                            self.main_chain.len() - self.block_delay_number as usize;
                        let new_main_chain = self.main_chain.split_off(finalized_blocks_number);
                        let mut finalized_tx_hash_list = Vec::new();
                        // save finalized blocks / txs / current height / current hash
                        for (_index, block_hash) in self.main_chain.iter().enumerate() {
                            // get block
                            // let block = self.fork_tree[index].get(block_hash).unwrap().to_owned();
                            let block = full_block.clone();
                            self.finalize_block(block.clone(), block_hash.to_owned())
                                .await;
                            let block_body = full_to_compact(block).body.unwrap();
                            finalized_tx_hash_list
                                .extend_from_slice(block_body.tx_hashes.as_slice());
                        }
                        self.block_number += finalized_blocks_number as u64;
                        self.block_hash = self.main_chain[finalized_blocks_number - 1].to_owned();

                        // let sys_config = self.get_system_config().await;
                        // let csf = ChainStatusWithFlag {
                        //     status: ChainStatus {
                        //         version: sys_config.version,
                        //         chain_id: sys_config.chain_id,
                        //         height: self.block_number,
                        //         hash: Some(Hash {
                        //             hash: self.block_hash.clone(),
                        //         }),
                        //         address: None,
                        //     },
                        //     broadcast_or_not: true,
                        // };
                        // self.task_sender.send(EventTask::UpdateStatus(csf)).unwrap();

                        self.main_chain = new_main_chain;
                        // update main_chain_tx_hash
                        self.main_chain_tx_hash = self
                            .main_chain_tx_hash
                            .iter()
                            .cloned()
                            .filter(|hash| !finalized_tx_hash_list.contains(hash))
                            .collect();
                        let new_fork_tree = self.fork_tree.split_off(finalized_blocks_number);
                        self.fork_tree = new_fork_tree;
                        self.fork_tree
                            .resize(self.block_delay_number as usize * 2 + 2, HashMap::new());
                    }
                    // candidate_block need update
                    self.clear_proposal();

                    let config = self.get_system_config().await;

                    return Ok((
                        ConsensusConfiguration {
                            height,
                            block_interval: config.block_interval,
                            validators: config.validators,
                        },
                        ChainStatus {
                            version: config.version,
                            chain_id: config.chain_id,
                            height,
                            hash: Some(Hash {
                                hash: self.block_hash.clone(),
                            }),
                            address: None,
                        },
                    ));
                }
                break;
            }
        }

        Err(Error::NoForkTree)
    }

    pub async fn process_block(
        &mut self,
        block: Block,
    ) -> Result<(ConsensusConfiguration, ChainStatus), Error> {
        let block_hash = get_block_hash(block.header.as_ref())?;
        let header = block.header.clone().unwrap();
        let height = header.height;

        if height <= self.block_number {
            return Err(Error::ProposalTooLow(height, self.block_number));
        }

        if height > self.block_number + self.block_delay_number as u64 + 1 {
            return Err(Error::ProposalTooHigh(height, self.block_number));
        }

        if &header.prevhash != &self.block_hash {
            warn!(
                "prev_hash of block({}) is not equal with self block hash",
                height
            );
            return Err(Error::BlockCheckError);
        }

        let proposal_bytes = self.assemble_proposal(block.clone(), height).await?;

        check_block(height, proposal_bytes, block.proof.clone())
            .await
            .map_or_else(
                |e| Err(Error::InternalError(e)),
                |res| {
                    if !res {
                        Err(Error::ConsensusProposalCheckError)
                    } else {
                        Ok(())
                    }
                },
            )?;

        self.check_transactions(block.body.clone().ok_or(Error::NoneBlockBody)?)
            .await?;

        self.finalize_block(block, get_block_hash(Some(&header))?)
            .await;

        self.block_number = height;
        self.block_hash = block_hash;

        let config = self.get_system_config().await;

        Ok((
            ConsensusConfiguration {
                height,
                block_interval: config.block_interval,
                validators: config.validators,
            },
            ChainStatus {
                version: config.version,
                chain_id: config.chain_id,
                height,
                hash: Some(Hash {
                    hash: self.block_hash.clone(),
                }),
                address: None,
            },
        ))
    }

    pub async fn get_system_config(&self) -> SystemConfig {
        let rd = self.auth.read().await;
        rd.get_system_config()
    }

    pub async fn next_step(&self, global_status: &ChainStatus) -> ChainStep {
        if global_status.height > self.block_number
            && (self.fork_tree[0].is_empty()
                || global_status.height >= self.block_number + FORCE_IN_SYNC)
        {
            log::debug!("in sync mod");
            ChainStep::SyncStep
        } else {
            log::debug!("in online mod");
            ChainStep::OnlineStep
        }
    }

    pub fn check_dup_tx(&self, tx_hash: &Vec<u8>) -> bool {
        self.main_chain_tx_hash.contains(tx_hash)
    }

    // todo use &RawTransactions
    pub async fn check_transactions(&self, raw_txs: RawTransactions) -> Result<(), Error> {
        let auth = self.auth.read().await;

        // todo not do clone
        for raw_tx in raw_txs.body.clone() {
            let tx_hash = auth
                .check_raw_tx(raw_tx)
                .await
                .map_err(|e| Error::ExpectError(e))?;

            if self.check_dup_tx(&tx_hash.clone()) {
                return Err(Error::DupTransaction(tx_hash));
            }
        }
        Ok(())
    }

    pub async fn chain_get_tx(&self, tx_hash: &[u8]) -> Option<RawTransaction> {
        if let Some(raw_tx) = {
            let rd = self.pool.read().await;
            rd.pool_get_tx(tx_hash)
        } {
            return Some(raw_tx);
        } else {
            db_get_tx(tx_hash).await
        }
    }

    pub async fn clear_candidate(&mut self) {
        self.fork_tree[0].clear();
        self.candidate_block = None;
    }
}
