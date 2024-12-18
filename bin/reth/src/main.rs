#![allow(missing_docs)]

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

use std::future::Future;
use clap::{Args, Parser};
use reth::{args::utils::DefaultChainSpecParser, cli::Cli};
use reth_node_builder::{
    engine_tree_config::{
        TreeConfig, DEFAULT_MEMORY_BLOCK_BUFFER_TARGET, DEFAULT_PERSISTENCE_THRESHOLD,
    },
    EngineNodeLauncher,
};
use reth_node_ethereum::{node::EthereumAddOns, EthereumNode};
use reth_provider::providers::BlockchainProvider2;

use alloy_sol_types::{sol, SolEventInterface};
use alloy_primitives::{Address, address};
use futures::StreamExt;
use tracing::info;
use reth_execution_types::Chain;
use reth_primitives::{SealedBlockWithSenders, TransactionSigned};

use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::FullNodeComponents;

sol!(CounterContract, "counter_abi.json");
use CounterContract::{CounterContractEvents};
use secp256k1::SecretKey;
use std::str::FromStr;
use web3::contract::{Contract, Options};
use web3::signing::SecretKeyRef;
use web3::transports::Http;
use web3::types::{H256, Bytes};
use web3::Error as Web3Error;

#[derive(Debug)]
pub struct StateRootContract(Contract<Http>);

impl StateRootContract {
    pub async fn new(web3: &web3::Web3<Http>, address: &str) -> Self {
        let address = web3::types::Address::from_str(&address).unwrap();
        let contract =
            Contract::from_json(web3.eth(), address, include_bytes!("../state_root_abi.json")).unwrap();
        StateRootContract(contract)
    }

    pub async fn update_state_root(&self, account: &SecretKey, state_root: Bytes) -> Result<H256, Web3Error> {
        self
            .0
            .signed_call(
                "changeStateRoot",
                state_root,
                Options {
                    gas: Some(5_000_000.into()),
                    ..Default::default()
                },
                SecretKeyRef::new(account),
            )
            .await
    }
}

/// The initialization logic of the ExEx is just an async function.
///
/// During initialization you can wait for resources you need to be up for the ExEx to function,
/// like a database connection.
async fn exex_init<Node: FullNodeComponents>(
    ctx: ExExContext<Node>,
) -> eyre::Result<impl Future<Output = eyre::Result<()>>> {
    Ok(exex(ctx))
}

/// An ExEx is just a future, which means you can implement all of it in an async function!
///
/// This ExEx just prints out whenever either a new chain of blocks being added, or a chain of
/// blocks being re-orged. After processing the chain, emits an [ExExEvent::FinishedHeight] event.
async fn exex<Node: FullNodeComponents>(mut ctx: ExExContext<Node>) -> eyre::Result<()> {
    while let Some(notification) = ctx.notifications.next().await {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                info!(committed_chain = ?new.range(), "Received commit");
                info!("Current stateRoot is [{}]", new.tip().block.header.state_root);
                notify_l1(&new).await;
            }
            ExExNotification::ChainReorged { old, new } => {
                info!(from_chain = ?old.range(), to_chain = ?new.range(), "Received reorg");
            }
            ExExNotification::ChainReverted { old } => {
                info!(reverted_chain = ?old.range(), "Received revert");
            }
        };

        if let Some(committed_chain) = notification.committed_chain() {
            ctx.events.send(ExExEvent::FinishedHeight(committed_chain.tip().num_hash()))?;
        }
    }

    Ok(())
}

/// Decode chain of blocks into a flattened list of receipt logs, filter only transactions to the
/// Counter contract [COUNTER_CONTRACT_ADDRESS] and extract [CounterContractEvents].
fn decode_chain_into_rollup_events(
    chain: &Chain,
) -> Vec<(&SealedBlockWithSenders, &TransactionSigned, CounterContractEvents)> {
    let counter_contract_address = std::env::var("COUNTER_CONTRACT_ADDRESS")
        .expect("COUNTER_CONTRACT_ADDRESS environment variable not set");
    let counter_contract_address = Address::from_str(&counter_contract_address).unwrap();
    chain
        // Get all blocks and receipts
        .blocks_and_receipts()
        // Get all receipts
        .flat_map(|(block, receipts)| {
            block
                .body
                .transactions
                .iter()
                .zip(receipts.iter().flatten())
                .map(move |(tx, receipt)| (block, tx, receipt))
        })
        // Get all logs from counter contract
        .flat_map(|(block, tx, receipt)| {
            receipt
                .logs
                .iter()
                .filter(|log| log.address == counter_contract_address)
                .map(move |log| (block, tx, log))
        })
        // Decode and filter counter events
        .filter_map(|(block, tx, log)| {
            CounterContractEvents::decode_raw_log(log.topics(), &log.data.data, true)
                .ok()
                .map(|event| (block, tx, event))
        })
        .collect()
}


async fn notify_l1(chain: &Chain) {
    let events = decode_chain_into_rollup_events(chain);

    for (_, _tx, event) in events {
        match event {
            CounterContractEvents::Incremented(..) => {
                let transport = std::env::var("L1_RPC_ADDRESS")
                    .expect("L1_RPC_ADDRESS environment variable not set");
                let transport = Http::new(&transport).unwrap();
                let web3 = web3::Web3::new(transport);
                let state_root_contract = std::env::var("STATE_ROOT_CONTRACT_ADDRESS")
                    .expect("STATE_ROOT_CONTRACT_ADDRESS environment variable not set");
                let state_root_contract = StateRootContract::new(
                    &web3,
                    &state_root_contract,
                ).await;
                let wallet = std::env::var("PREFUNDED_SECRET")
                    .expect("PREFUNDED_SECRET environment variable not set");
                let wallet = SecretKey::from_str(&wallet).unwrap();

                let tx_id = state_root_contract
                    .update_state_root(
                        &wallet, Bytes::from(chain.tip().block.header.state_root.to_vec())
                    );

                match tx_id.await {
                    Ok(id) => info!("I notifed L1 with new state root. txId = [{:#x}]", id),
                    Err(error) => info!("I failed to notify L1 with new state root. error = [{}]", error)
                }

                ()
            }
        }
    }
}


/// Parameters for configuring the engine
#[derive(Debug, Clone, Args, PartialEq, Eq)]
#[command(next_help_heading = "Engine")]
pub struct EngineArgs {
    /// Enable the engine2 experimental features on reth binary
    #[arg(long = "engine.experimental", default_value = "false")]
    pub experimental: bool,

    /// Configure persistence threshold for engine experimental.
    #[arg(long = "engine.persistence-threshold", requires = "experimental", default_value_t = DEFAULT_PERSISTENCE_THRESHOLD)]
    pub persistence_threshold: u64,

    /// Configure the target number of blocks to keep in memory.
    #[arg(long = "engine.memory-block-buffer-target", requires = "experimental", default_value_t = DEFAULT_MEMORY_BLOCK_BUFFER_TARGET)]
    pub memory_block_buffer_target: u64,
}

impl Default for EngineArgs {
    fn default() -> Self {
        Self {
            experimental: false,
            persistence_threshold: DEFAULT_PERSISTENCE_THRESHOLD,
            memory_block_buffer_target: DEFAULT_MEMORY_BLOCK_BUFFER_TARGET,
        }
    }
}

fn main() {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    if let Err(err) =
        Cli::<DefaultChainSpecParser, EngineArgs>::parse().run(|builder, engine_args| async move {
                    let engine_tree_config = TreeConfig::default()
                        .with_persistence_threshold(engine_args.persistence_threshold)
                        .with_memory_block_buffer_target(engine_args.memory_block_buffer_target);
                    let handle = builder
                        .with_types_and_provider::<EthereumNode, BlockchainProvider2<_>>()
                        .with_components(EthereumNode::components())
                        .with_add_ons::<EthereumAddOns>()
                        .install_exex("t1", exex_init)
                        .launch_with_fn(|builder| {
                            let launcher = EngineNodeLauncher::new(
                                builder.task_executor().clone(),
                                builder.config().datadir(),
                                engine_tree_config,
                            );
                            builder.launch_with(launcher)
                        })
                        .await?;
                    handle.node_exit_future.await
        })
    {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// A helper type to parse Args more easily
    #[derive(Parser)]
    struct CommandParser<T: Args> {
        #[command(flatten)]
        args: T,
    }

    #[test]
    fn test_parse_engine_args() {
        let default_args = EngineArgs::default();
        let args = CommandParser::<EngineArgs>::parse_from(["reth"]).args;
        assert_eq!(args, default_args);
    }
}
