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

use crate::util::{verify_tx_hash, verify_tx_signature, genesis_block_hash, load_data};
use cita_ng_proto::controller::raw_transaction::Tx::{NormalTx, UtxoTx};
use cita_ng_proto::controller::{raw_transaction::Tx, RawTransaction};
use cita_ng_proto::blockchain::{CompactBlockBody, Transaction, UtxoTransaction};
use prost::Message;
use std::collections::HashMap;

pub const BLOCKLIMIT: u64 = 100;

#[derive(Clone)]
pub struct Authentication {
    kms_port: String,
    storage_port: String,
    history_hashes: HashMap<u64, Vec<Vec<u8>>>,
    current_block_number: u64,
}

impl Authentication {
    pub fn new(kms_port: String, storage_port: String) -> Self {
        Authentication {
            kms_port,
            storage_port,
            history_hashes: HashMap::new(),
            current_block_number: 0,
        }
    }

    pub async fn init(&mut self, init_block_number: u64) {
        let begin_block_number = if init_block_number >= BLOCKLIMIT {
            init_block_number - BLOCKLIMIT + 1
        } else {
            1u64
        };

        for h in begin_block_number..(init_block_number + 1) {
            // region 3: block_height - block body
            let block_body_bytes = load_data(self.storage_port.clone(), 3, h.to_be_bytes().to_vec()).await.unwrap();
            let block_body = CompactBlockBody::decode(block_body_bytes.as_slice()).unwrap();
            self.history_hashes.insert(h, block_body.tx_hashes);
        }
        self.current_block_number = init_block_number;
    }

    pub fn insert_tx_hash(&mut self, h: u64, hash_list: Vec<Vec<u8>>) {
        self.history_hashes.insert(h, hash_list);
        if h >= BLOCKLIMIT {
            self.history_hashes.remove(&(h - BLOCKLIMIT));
        }
        if h > self.current_block_number {
            self.current_block_number = h;
        }
    }

    fn check_tx_hash(&self, tx_hash: &Vec<u8>) -> Result<(), String> {
        for (_h, hash_list) in self.history_hashes.iter() {
            if hash_list.contains(tx_hash) {
                return Err("dup".to_owned());
            }
        }
        Ok(())
    }

    fn check_transaction(&self, tx: &Transaction) -> Result<(), String> {
        // todo version and chain_id need utxo_set
        if tx.version != 0 {
            return Err("Invalid version".to_owned());
        }
        if tx.nonce.len() > 128 {
            return Err("Invalid nonce".to_owned());
        }
        if tx.valid_until_block <= self.current_block_number || tx.valid_until_block > (self.current_block_number + BLOCKLIMIT) {
            return Err("Invalid valid_until_block".to_owned());
        }
        if tx.value.len() != 32 {
            return Err("Invalid value".to_owned());
        }
        if tx.chain_id.len() != 32 || tx.chain_id != vec![0u8; 32] {
            return Err("Invalid chain_id".to_owned());
        }
        Ok(())
    }

    pub async fn check_raw_tx(&self, raw_tx: RawTransaction) -> Result<Vec<u8>, String> {
        if let Some(tx) = raw_tx.tx {
            match tx {
                NormalTx(normal_tx) => {
                    if normal_tx.witness.is_none() {
                        return Err("witness is none".to_owned());
                    }

                    let witness = normal_tx.witness.unwrap();
                    let signature = witness.signature;
                    let sender = witness.sender;

                    let mut tx_bytes: Vec<u8> = Vec::new();
                    if let Some(tx) = normal_tx.transaction {
                        self.check_transaction(&tx)?;
                        let ret = tx.encode(&mut tx_bytes);
                        if ret.is_err() {
                            return Err("encode tx failed".to_owned());
                        }
                    } else {
                        return Err("tx is none".to_owned());
                    }

                    let tx_hash = normal_tx.transaction_hash;

                    self.check_tx_hash(&tx_hash)?;

                    if let Ok(is_ok) =
                        verify_tx_hash(self.kms_port.clone(), tx_hash.clone(), tx_bytes).await
                    {
                        if !is_ok {
                            return Err("Invalid tx_hash".to_owned());
                        }
                    }

                    if let Ok(address) =
                        verify_tx_signature(self.kms_port.clone(), tx_hash.clone(), signature).await
                    {
                        if address == sender {
                            Ok(tx_hash)
                        } else {
                            Err("Invalid sender".to_owned())
                        }
                    } else {
                        Err("kms recover signature failed".to_owned())
                    }
                }
                UtxoTx(utxo_tx) => {
                    // todo check transaction
                    let witnesses = utxo_tx.witnesses;

                    let mut tx_bytes: Vec<u8> = Vec::new();
                    if let Some(tx) = utxo_tx.transaction {
                        let ret = tx.encode(&mut tx_bytes);
                        if ret.is_err() {
                            return Err("encode utxo tx failed".to_owned());
                        }
                    } else {
                        return Err("utxo tx is none".to_owned());
                    }

                    let tx_hash = utxo_tx.transaction_hash;
                    if let Ok(is_ok) =
                        verify_tx_hash(self.kms_port.clone(), tx_hash.clone(), tx_bytes).await
                    {
                        if !is_ok {
                            return Err("Invalid utxo tx hash".to_owned());
                        }
                    }

                    for (i, w) in witnesses.into_iter().enumerate() {
                        let signature = w.signature;
                        let sender = w.sender;

                        if let Ok(address) =
                            verify_tx_signature(self.kms_port.clone(), tx_hash.clone(), signature)
                                .await
                        {
                            if address != sender {
                                let err_str = format!("Invalid sender index: {}", i);
                                return Err(err_str);
                            }
                        } else {
                            let err_str = format!("kms recover signature failed index: {}", i);
                            return Err(err_str);
                        }
                    }
                    Ok(tx_hash)
                }
            }
        } else {
            Err("Invalid raw tx".to_owned())
        }
    }
}
