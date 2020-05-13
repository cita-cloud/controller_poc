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
use crate::chain::Chain;
use crate::pool::Pool;
use crate::util::{broadcast_message, genesis_block, load_data};
use cita_ng_proto::blockchain::{
    BlockHeader, CompactBlock, CompactBlockBody, UnverifiedUtxoTransaction, UtxoTransaction,
    Witness,
};
use cita_ng_proto::common::Hash;
use cita_ng_proto::controller::raw_transaction::Tx::{NormalTx, UtxoTx};
use cita_ng_proto::controller::{raw_transaction::Tx, RawTransaction};
use cita_ng_proto::network::NetworkMsg;
use futures_util::future::TryFutureExt;
use log::{info, warn};
use prost::Message;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct Controller {
    consensus_port: String,
    network_port: String,
    storage_port: String,
    kms_port: String,
    executor_port: String,
    block_delay_number: u32,
    auth: Arc<RwLock<Authentication>>,
    pool: Arc<RwLock<Pool>>,
    chain: Arc<RwLock<Chain>>,
}

impl Controller {
    pub fn new(
        consensus_port: String,
        network_port: String,
        storage_port: String,
        kms_port: String,
        executor_port: String,
        block_delay_number: u32,
        current_block_number: u64,
        current_block_hash: Vec<u8>,
    ) -> Self {
        let auth = Arc::new(RwLock::new(Authentication::new(kms_port.clone(), storage_port.clone())));
        let pool = Arc::new(RwLock::new(Pool::new(500)));
        let chain = Arc::new(RwLock::new(Chain::new(
            storage_port.clone(),
            network_port.clone(),
            kms_port.clone(),
            executor_port.clone(),
            block_delay_number,
            current_block_number,
            current_block_hash,
            pool.clone(),
            auth.clone(),
        )));
        Controller {
            consensus_port,
            network_port,
            storage_port,
            kms_port,
            executor_port,
            block_delay_number,
            auth,
            pool,
            chain,
        }
    }

    pub async fn init(&self, init_block_number: u64) {
        {
            let mut chain = self.chain.write().await;
            chain.add_proposal().await
        }
        {
            let mut auth = self.auth.write().await;
            auth.init(init_block_number).await;
        }
    }

    pub async fn rpc_get_block_number(&self, is_pending: bool) -> Result<u64, String> {
        let chain = self.chain.read().await;
        let block_number = chain.get_block_number(is_pending);
        Ok(block_number)
    }

    pub async fn rpc_send_raw_transaction(
        &self,
        raw_tx: RawTransaction,
    ) -> Result<Vec<u8>, String> {
        let tx_hash = {
            let auth = self.auth.read().await;
            auth.check_raw_tx(raw_tx.clone()).await?
        };

        let mut pool = self.pool.write().await;
        let is_ok = pool.enqueue(raw_tx.clone(), tx_hash.clone());
        if is_ok {
            let mut raw_tx_bytes: Vec<u8> = Vec::new();
            let _ = raw_tx.encode(&mut raw_tx_bytes);
            let msg = NetworkMsg {
                module: "controller".to_owned(),
                r#type: "raw_tx".to_owned(),
                origin: 0,
                msg: raw_tx_bytes,
            };
            let _ = broadcast_message(self.network_port.clone(), msg).await;
            Ok(tx_hash)
        } else {
            Err("dup".to_owned())
        }
    }

    pub async fn rpc_get_block_by_hash(&self, hash: Vec<u8>) -> Result<CompactBlock, String> {
        Ok(genesis_block())
    }

    async fn get_block_hash(&self, block_number: u64) -> Result<Vec<u8>, String> {
        Ok(vec![])
    }

    pub async fn rpc_get_block_by_number(&self, block_number: u64) -> Result<CompactBlock, String> {
        let chain = self.chain.read().await;
        let ret = chain.get_block_by_number(block_number).await;
        if ret.is_none() {
            Err("can't find block by number".to_owned())
        } else {
            Ok(ret.unwrap())
        }
    }

    pub async fn rpc_get_transaction(&self, tx_hash: Vec<u8>) -> Result<RawTransaction, String> {
        let pool = self.pool.read().await;
        let ret = pool.get_tx(&tx_hash);
        if let Some(raw_tx) = ret {
            Ok(raw_tx)
        } else {
            let ret = load_data(self.storage_port.clone(), 1, tx_hash).await;
            if let Ok(raw_tx_bytes) = ret {
                let ret = RawTransaction::decode(raw_tx_bytes.as_slice());
                if ret.is_err() {
                    Err("decode failed".to_owned())
                } else {
                    let raw_tx = ret.unwrap();
                    Ok(raw_tx)
                }
            } else {
                Err("can't get transaction".to_owned())
            }
        }
    }

    pub async fn chain_get_proposal(&self) -> Result<Vec<u8>, String> {
        let chain = self.chain.read().await;
        if let Some(proposal) = chain.get_candidate_block_hash() {
            return Ok(proposal);
        } else {
            Err("get proposal error".to_owned())
        }
    }

    pub async fn chain_check_proposal(&self, proposal: &[u8]) -> Result<bool, String> {
        let chain = self.chain.read().await;
        let ret = chain.check_proposal(proposal).await;
        Ok(ret)
    }

    pub async fn chain_commit_block(&self, proposal: &[u8]) -> Result<(), String> {
        let mut chain = self.chain.write().await;
        chain.commit_block(proposal).await;
        Ok(())
    }

    pub async fn process_network_msg(&self, msg: NetworkMsg) -> Result<(), String> {
        match msg.r#type.as_str() {
            "raw_tx" => {
                let raw_tx_bytes = msg.msg;
                if let Ok(raw_tx) = RawTransaction::decode(raw_tx_bytes.as_slice()) {
                    self.rpc_send_raw_transaction(raw_tx).await.map(|_| ())
                } else {
                    Err("Decode raw transaction failed".to_owned())
                }
            }
            "block" => {
                info!("get block from network");
                let block_bytes = msg.msg;
                if let Ok(block) = CompactBlock::decode(block_bytes.as_slice()) {
                    if let Some(block_body) = block.clone().body {
                        let tx_hash_list = block_body.tx_hashes;
                        {
                            let pool = self.pool.read().await;
                            for hash in tx_hash_list.iter() {
                                if pool.get_tx(hash).is_none() {
                                    warn!("block is invalid");
                                    return Err("block is invalid".to_owned());
                                }
                            }
                        }
                        {
                            info!("add block");
                            let mut chain = self.chain.write().await;
                            chain.add_block(block).await;
                        }
                        Ok(())
                    } else {
                        warn!("block body is empty");
                        Err("block body is empty".to_owned())
                    }
                } else {
                    warn!("Decode block failed");
                    Err("Decode block failed".to_owned())
                }
            }
            _ => {
                panic!("unknown network message");
            }
        }
    }
}
