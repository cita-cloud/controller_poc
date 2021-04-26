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

mod auth;
mod chain;
mod config;
mod controller;
mod genesis;
mod panic_hook;
mod pool;
mod sync;
mod util;
mod utxo_set;

use crate::panic_hook::set_panic_handler;
use clap::Clap;
use git_version::git_version;
use log::{debug, info, warn};

const GIT_VERSION: &str = git_version!(
    args = ["--tags", "--always", "--dirty=-modified"],
    fallback = "unknown"
);
const GIT_HOMEPAGE: &str = "https://github.com/cita-cloud/controller_poc";

/// This doc string acts as a help message when the user runs '--help'
/// as do all doc strings on fields
#[derive(Clap)]
#[clap(version = "0.1.0", author = "Rivtower Technologies.")]
struct Opts {
    #[clap(subcommand)]
    subcmd: SubCommand,
}

#[derive(Clap)]
enum SubCommand {
    /// print information from git
    #[clap(name = "git")]
    GitInfo,
    /// run this service
    #[clap(name = "run")]
    Run(RunOpts),
}

/// A subcommand for run
#[derive(Clap)]
struct RunOpts {
    /// Sets grpc port of this service.
    #[clap(short = 'p', long = "port", default_value = "50004")]
    grpc_port: String,
}

fn main() {
    ::std::env::set_var("RUST_BACKTRACE", "full");
    set_panic_handler();

    let opts: Opts = Opts::parse();

    // You can handle information about subcommands by requesting their matches by name
    // (as below), requesting just the name used, or both at the same time
    match opts.subcmd {
        SubCommand::GitInfo => {
            println!("git version: {}", GIT_VERSION);
            println!("homepage: {}", GIT_HOMEPAGE);
        }
        SubCommand::Run(opts) => {
            // init log4rs
            log4rs::init_file("controller-log4rs.yaml", Default::default()).unwrap();
            info!("grpc port of this service: {}", opts.grpc_port);
            let _ = run(opts);
        }
    }
}

use cita_cloud_proto::network::network_service_client::NetworkServiceClient;
use cita_cloud_proto::network::RegisterInfo;

async fn register_network_msg_handler(
    network_port: u16,
    port: String,
) -> Result<bool, Box<dyn std::error::Error>> {
    let network_addr = format!("http://127.0.0.1:{}", network_port);
    let mut client = NetworkServiceClient::connect(network_addr).await?;

    let request = Request::new(RegisterInfo {
        module_name: "controller".to_owned(),
        hostname: "127.0.0.1".to_owned(),
        port,
    });

    let response = client.register_network_msg_handler(request).await?;

    Ok(response.into_inner().is_success)
}

use cita_cloud_proto::blockchain::CompactBlock;
use cita_cloud_proto::common::{Empty, Hash, Proposal, ProposalWithProof, SimpleResponse};
use cita_cloud_proto::controller::SystemConfig as ProtoSystemConfig;
use cita_cloud_proto::controller::{
    rpc_service_server::RpcService, rpc_service_server::RpcServiceServer, BlockNumber, Flag,
    PeerCount, RawTransaction, SoftwareVersion, TransactionIndex,
};
use tonic::{transport::Server, Request, Response, Status};

// grpc server of RPC
pub struct RPCServer {
    controller: Controller,
}

impl RPCServer {
    fn new(controller: Controller) -> Self {
        RPCServer { controller }
    }
}

#[tonic::async_trait]
impl RpcService for RPCServer {
    async fn get_block_number(
        &self,
        request: Request<Flag>,
    ) -> Result<Response<BlockNumber>, Status> {
        debug!("get_block_number request: {:?}", request);

        let flag = request.into_inner();
        self.controller
            .rpc_get_block_number(flag.flag)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |block_number| {
                    let reply = Response::new(BlockNumber { block_number });
                    Ok(reply)
                },
            )
    }
    async fn send_raw_transaction(
        &self,
        request: Request<RawTransaction>,
    ) -> Result<Response<Hash>, Status> {
        debug!("send_raw_transaction request: {:?}", request);

        let raw_tx = request.into_inner();

        self.controller
            .rpc_send_raw_transaction(raw_tx)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |tx_hash| {
                    let reply = Response::new(Hash { hash: tx_hash });
                    Ok(reply)
                },
            )
    }
    async fn get_block_by_hash(
        &self,
        request: Request<Hash>,
    ) -> Result<Response<CompactBlock>, Status> {
        debug!("send_raw_transaction request: {:?}", request);

        let hash = request.into_inner();

        self.controller
            .rpc_get_block_by_hash(hash.hash)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |block| {
                    let reply = Response::new(block);
                    Ok(reply)
                },
            )
    }
    async fn get_block_by_number(
        &self,
        request: Request<BlockNumber>,
    ) -> Result<tonic::Response<CompactBlock>, Status> {
        debug!("get_block_by_number request: {:?}", request);

        let block_number = request.into_inner();

        self.controller
            .rpc_get_block_by_number(block_number.block_number)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |block| {
                    let reply = Response::new(block);
                    Ok(reply)
                },
            )
    }
    async fn get_transaction(
        &self,
        request: Request<Hash>,
    ) -> Result<tonic::Response<RawTransaction>, Status> {
        debug!("get_block_by_number request: {:?}", request);

        let hash = request.into_inner();

        self.controller
            .rpc_get_transaction(hash.hash)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |raw_tx| {
                    let reply = Response::new(raw_tx);
                    Ok(reply)
                },
            )
    }
    async fn get_system_config(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<ProtoSystemConfig>, Status> {
        debug!("get_system_config request: {:?}", request);

        self.controller.rpc_get_system_config().await.map_or_else(
            |e| Err(Status::internal(e)),
            |sys_config| {
                let reply = Response::new(ProtoSystemConfig {
                    version: sys_config.version,
                    chain_id: sys_config.chain_id,
                    admin: sys_config.admin,
                    block_interval: sys_config.block_interval,
                    validators: sys_config.validators,
                    emergency_brake: sys_config.emergency_brake,
                    version_pre_hash: sys_config
                        .utxo_tx_hashes
                        .get(&LOCK_ID_VERSION)
                        .unwrap()
                        .to_owned(),
                    chain_id_pre_hash: sys_config
                        .utxo_tx_hashes
                        .get(&LOCK_ID_CHAIN_ID)
                        .unwrap()
                        .to_owned(),
                    admin_pre_hash: sys_config
                        .utxo_tx_hashes
                        .get(&LOCK_ID_ADMIN)
                        .unwrap()
                        .to_owned(),
                    block_interval_pre_hash: sys_config
                        .utxo_tx_hashes
                        .get(&LOCK_ID_BLOCK_INTERVAL)
                        .unwrap()
                        .to_owned(),
                    validators_pre_hash: sys_config
                        .utxo_tx_hashes
                        .get(&LOCK_ID_VALIDATORS)
                        .unwrap()
                        .to_owned(),
                    emergency_brake_pre_hash: sys_config
                        .utxo_tx_hashes
                        .get(&LOCK_ID_EMERGENCY_BRAKE)
                        .unwrap()
                        .to_owned(),
                });
                Ok(reply)
            },
        )
    }

    async fn get_version(
        &self,
        request: Request<Empty>,
    ) -> Result<Response<SoftwareVersion>, Status> {
        debug!("get_version request: {:?}", request);
        let reply = Response::new(SoftwareVersion {
            version: "4.0.0".to_owned(),
        });
        Ok(reply)
    }

    async fn get_block_hash(
        &self,
        request: Request<BlockNumber>,
    ) -> Result<Response<Hash>, Status> {
        debug!("get_block_hash request: {:?}", request);

        let block_number = request.into_inner();

        self.controller
            .rpc_get_block_hash(block_number.block_number)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |block_hash| {
                    let reply = Response::new(Hash { hash: block_hash });
                    Ok(reply)
                },
            )
    }

    async fn get_transaction_block_number(
        &self,
        request: Request<Hash>,
    ) -> Result<Response<BlockNumber>, Status> {
        debug!("get_transaction_block_number request: {:?}", request);

        let tx_hash = request.into_inner();

        self.controller
            .rpc_get_tx_block_number(tx_hash.hash)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |block_number| {
                    let reply = Response::new(BlockNumber { block_number });
                    Ok(reply)
                },
            )
    }

    async fn get_transaction_index(
        &self,
        request: Request<Hash>,
    ) -> Result<Response<TransactionIndex>, Status> {
        debug!("get_transaction_index request: {:?}", request);

        let tx_hash = request.into_inner();

        self.controller
            .rpc_get_tx_index(tx_hash.hash)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |tx_index| {
                    let reply = Response::new(TransactionIndex { tx_index });
                    Ok(reply)
                },
            )
    }

    async fn get_peer_count(&self, request: Request<Empty>) -> Result<Response<PeerCount>, Status> {
        debug!("get_peer_count request: {:?}", request);

        self.controller.rpc_get_peer_count().await.map_or_else(
            |e| Err(Status::internal(e)),
            |peer_count| {
                let reply = Response::new(PeerCount { peer_count });
                Ok(reply)
            },
        )
    }
}

use cita_cloud_proto::controller::{
    consensus2_controller_service_server::Consensus2ControllerService,
    consensus2_controller_service_server::Consensus2ControllerServiceServer,
};

//grpc server for Consensus2ControllerService
pub struct Consensus2ControllerServer {
    controller: Controller,
}

impl Consensus2ControllerServer {
    fn new(controller: Controller) -> Self {
        Consensus2ControllerServer { controller }
    }
}

#[tonic::async_trait]
impl Consensus2ControllerService for Consensus2ControllerServer {
    async fn get_proposal(&self, request: Request<Empty>) -> Result<Response<Proposal>, Status> {
        debug!("get_proposal request: {:?}", request);

        self.controller.chain_get_proposal().await.map_or_else(
            |e| Err(Status::internal(e)),
            |(height, data)| {
                let reply = Response::new(Proposal { height, data });
                Ok(reply)
            },
        )
    }
    async fn check_proposal(
        &self,
        request: Request<Proposal>,
    ) -> Result<Response<SimpleResponse>, Status> {
        debug!("check_proposal request: {:?}", request);

        let proposal = request.into_inner();

        let height = proposal.height;
        let data = proposal.data;

        self.controller
            .chain_check_proposal(height, &data)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |is_ok| {
                    let reply = Response::new(SimpleResponse { is_success: is_ok });
                    Ok(reply)
                },
            )
    }
    async fn commit_block(
        &self,
        request: Request<ProposalWithProof>,
    ) -> Result<Response<Empty>, Status> {
        debug!("commit_block request: {:?}", request);

        let proposal_with_proof = request.into_inner();
        let proposal = proposal_with_proof.proposal.unwrap();
        let height = proposal.height;
        let data = proposal.data;
        let proof = proposal_with_proof.proof;

        self.controller
            .chain_commit_block(height, &data, &proof)
            .await
            .map_or_else(
                |e| Err(Status::internal(e)),
                |_| Ok(Response::new(Empty {})),
            )
    }
}

use cita_cloud_proto::network::{
    network_msg_handler_service_server::NetworkMsgHandlerService,
    network_msg_handler_service_server::NetworkMsgHandlerServiceServer, NetworkMsg,
};

// grpc server of network msg handler
pub struct ControllerNetworkMsgHandlerServer {
    controller: Controller,
}

impl ControllerNetworkMsgHandlerServer {
    fn new(controller: Controller) -> Self {
        ControllerNetworkMsgHandlerServer { controller }
    }
}

#[tonic::async_trait]
impl NetworkMsgHandlerService for ControllerNetworkMsgHandlerServer {
    async fn process_network_msg(
        &self,
        request: Request<NetworkMsg>,
    ) -> Result<Response<SimpleResponse>, Status> {
        debug!("process_network_msg request: {:?}", request);

        let msg = request.into_inner();
        if msg.module != "controller" {
            Err(Status::invalid_argument("wrong module"))
        } else {
            self.controller.process_network_msg(msg).await.map_or_else(
                |e| Err(Status::internal(e)),
                |_| {
                    let reply = SimpleResponse { is_success: true };
                    Ok(Response::new(reply))
                },
            )
        }
    }
}

use crate::config::ControllerConfig;
use crate::controller::Controller;
use crate::sync::Notifier;
use crate::util::{hash_data, load_data, load_data_maybe_empty, reconfigure};
use crate::utxo_set::{
    SystemConfigFile, LOCK_ID_ADMIN, LOCK_ID_BLOCK_INTERVAL, LOCK_ID_BUTTON, LOCK_ID_CHAIN_ID,
    LOCK_ID_VALIDATORS, LOCK_ID_VERSION, LOCK_ID_EMERGENCY_BRAKE,
};
use cita_cloud_proto::controller::raw_transaction::Tx::UtxoTx;
use genesis::GenesisBlock;
use prost::Message;
use std::fs;
use std::sync::Arc;
use std::time::Duration;
use tokio::time;

#[tokio::main]
async fn run(opts: RunOpts) -> Result<(), Box<dyn std::error::Error>> {
    // read consensus-config.toml
    let buffer = fs::read_to_string("controller-config.toml")
        .unwrap_or_else(|err| panic!("Error while loading config: [{}]", err));
    let config = ControllerConfig::new(&buffer);

    let network_port = config.network_port;
    let consensus_port = config.consensus_port;
    let storage_port = config.storage_port;
    let kms_port = config.kms_port;
    let executor_port = config.executor_port;
    let block_delay_number = config.block_delay_number;

    let grpc_port_clone = opts.grpc_port.clone();
    let mut interval = time::interval(Duration::from_secs(3));
    loop {
        interval.tick().await;
        // register endpoint
        {
            let ret = register_network_msg_handler(network_port, grpc_port_clone.clone()).await;
            if ret.is_ok() && ret.unwrap() {
                info!("register network msg handler success!");
                break;
            }
        }
        warn!("register network msg handler failed! Retrying");
    }

    let mut interval = time::interval(Duration::from_secs(3));
    loop {
        interval.tick().await;
        // register endpoint
        {
            let ret = hash_data(kms_port, vec![0u8; 32]).await;
            if ret.is_ok() {
                info!("kms is ready!");
                break;
            }
        }
        warn!("kms not ready! Retrying");
    }

    // load key_id
    let buffer = fs::read_to_string("key_id")
        .unwrap_or_else(|err| panic!("Error while loading key_id: [{}]", err));
    let key_id = u64::from_str_radix(&buffer, 10)
        .unwrap_or_else(|err| panic!("Error while parsing key_id: [{}]", err));
    info!("key_id: {}", key_id);

    // load node_address
    let buffer = fs::read_to_string("node_address")
        .unwrap_or_else(|err| panic!("Error while loading node_address: [{}]", err));
    // skip 0x prefix
    let node_address = hex::decode(&buffer[2..])
        .unwrap_or_else(|err| panic!("Error while parsing node_address: [{}]", err));
    info!("node_address: {:?}", buffer);

    // load genesis.toml
    let buffer = fs::read_to_string("genesis.toml")
        .unwrap_or_else(|err| panic!("Error while loading genesis.toml: [{}]", err));
    let genesis = GenesisBlock::new(&buffer);
    let current_block_number;
    let current_block_hash;
    let mut interval = time::interval(Duration::from_secs(3));
    loop {
        interval.tick().await;
        {
            let ret = load_data_maybe_empty(storage_port, 0, 0u64.to_be_bytes().to_vec()).await;
            if let Ok(current_block_number_bytes) = ret {
                info!("get current block number success!");
                if current_block_number_bytes.is_empty() {
                    info!("this is a new chain!");
                    current_block_number = 0u64;
                    current_block_hash = genesis.genesis_block_hash(kms_port).await;
                } else {
                    info!("this is an old chain!");
                    let mut bytes: [u8; 8] = [0; 8];
                    bytes[..8].clone_from_slice(&current_block_number_bytes[..8]);
                    current_block_number = u64::from_be_bytes(bytes);
                    current_block_hash = load_data(storage_port, 0, 1u64.to_be_bytes().to_vec())
                        .await
                        .unwrap();
                }
                break;
            }
        }
        warn!("get current block number failed! Retrying");
    }
    info!("current block number: {}", current_block_number);
    info!("current block hash: {:?}", current_block_hash);

    // load initial sys_config
    let buffer = fs::read_to_string("init_sys_config.toml")
        .unwrap_or_else(|err| panic!("Error while loading init_sys_config.toml: [{}]", err));

    let mut sys_config = SystemConfigFile::new(&buffer).to_system_config();
    if current_block_number != 0 {
        for id in LOCK_ID_VERSION..LOCK_ID_BUTTON {
            let key = id.to_be_bytes().to_vec();
            // region 0 global
            let tx_hash = load_data_maybe_empty(storage_port, 0, key).await.unwrap();
            if tx_hash.is_empty() {
                continue;
            }
            // region 1: tx_hash - tx
            let raw_tx_bytes = load_data(storage_port, 1, tx_hash).await.unwrap();
            let raw_tx = RawTransaction::decode(raw_tx_bytes.as_slice()).unwrap();
            let tx = raw_tx.tx.unwrap();
            if let UtxoTx(utxo_tx) = tx {
                sys_config.update(&utxo_tx, true);
            } else {
                panic!("tx is not utxo_tx");
            }
        }
    }
    info!("sys_config: {:?}", sys_config);

    // send configuration to consensus
    let sys_config_clone = sys_config.clone();
    tokio::spawn(async move {
        let mut interval = time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            // reconfigure consensus
            {
                info!("reconfigure consensus!");
                let ret = reconfigure(
                    consensus_port,
                    current_block_number,
                    sys_config_clone.clone(),
                )
                .await;
                if ret.is_ok() && ret.unwrap() {
                    info!("reconfigure success!");
                    break;
                }
            }
        }
    });

    let controller = Controller::new(
        consensus_port,
        network_port,
        storage_port,
        kms_port,
        executor_port,
        block_delay_number,
        current_block_number,
        current_block_hash,
        sys_config,
        genesis,
        Arc::new(Notifier::new(".".to_string(), "txs".to_string())),
        Arc::new(Notifier::new(".".to_string(), "proposals".to_string())),
        Arc::new(Notifier::new(".".to_string(), "blocks".to_string())),
        key_id,
        node_address,
    );

    controller.init(current_block_number).await;

    let addr_str = format!("0.0.0.0:{}", opts.grpc_port);
    let addr = addr_str.parse()?;

    info!("start grpc server!");
    Server::builder()
        .add_service(RpcServiceServer::new(RPCServer::new(controller.clone())))
        .add_service(Consensus2ControllerServiceServer::new(
            Consensus2ControllerServer::new(controller.clone()),
        ))
        .add_service(NetworkMsgHandlerServiceServer::new(
            ControllerNetworkMsgHandlerServer::new(controller),
        ))
        .serve(addr)
        .await?;

    Ok(())
}
