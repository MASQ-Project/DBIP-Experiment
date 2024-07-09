// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

pub mod blockchain_interface_web3;
pub mod data_structures;
pub mod lower_level_interface;
pub mod test_utils;

use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
use crate::blockchain::blockchain_interface::data_structures::errors::{
    BlockchainAgentBuildError, BlockchainError, PayableTransactionError, ResultForReceipt,
};
use crate::blockchain::blockchain_interface::data_structures::RetrievedBlockchainTransactions;
use crate::blockchain::blockchain_interface::lower_level_interface::LowBlockchainInt;
use crate::sub_lib::wallet::Wallet;
use core::panic;
use ethereum_types::U256;
use futures::Future;
use masq_lib::blockchains::chains::Chain;
use web3::contract::{Contract};
use web3::transports::{Batch, Http};
use web3::types::{Address, BlockNumber, H256};
use web3::{Web3};

// TODO: GH-744: Fix this trait - before submitting this code for review.
// Create some tools for each blockchain and pass these tool in every function of this trait.
// Example Web3 tools for Web3 based blockchains.
pub trait BlockchainInterface {
    fn contract_address(&self) -> Address;

    fn get_chain(&self) -> Chain;

    fn get_contract(&self) -> Contract<Http>;

    fn get_web3(&self) -> Web3<Http>;

    fn get_web3_batch(&self) -> Web3<Batch<Http>>;

    fn get_transport(&self) -> Http;

    fn lower_interface(&self) -> Box<dyn LowBlockchainInt>;

    fn retrieve_transactions(
        &self,
        start_block: BlockNumber,
        fallback_start_block_number: u64,
        recipient: Address,
    ) -> Box<dyn Future<Item = RetrievedBlockchainTransactions, Error = BlockchainError>>;

    fn build_blockchain_agent(
        &self,
        consuming_wallet: Wallet,
    ) -> Box<dyn Future<Item = Box<dyn BlockchainAgent>, Error = BlockchainAgentBuildError>>;

    fn get_transaction_receipt(&self, hash: H256) -> ResultForReceipt;

    as_any_ref_in_trait!();
}
