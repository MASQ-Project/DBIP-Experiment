// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

mod batch_payable_tools;
pub mod lower_level_interface_web3;
mod test_utils;

use crate::accountant::db_access_objects::pending_payable_dao::PendingPayable;
use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
use crate::blockchain::blockchain_interface::data_structures::errors::BlockchainError::QueryFailed;
use crate::blockchain::blockchain_interface::data_structures::errors::{
    BlockchainError, PayableTransactionError,
};
use crate::blockchain::blockchain_interface::data_structures::BlockchainTransaction;
use crate::blockchain::blockchain_interface::lower_level_interface::LowBlockchainInt;
use crate::blockchain::blockchain_interface::RetrievedBlockchainTransactions;
use crate::blockchain::blockchain_interface::{BlockchainAgentBuildError, BlockchainInterface};
use crate::db_config::persistent_configuration::PersistentConfiguration;
use crate::sub_lib::wallet::Wallet;
use futures::{Future, future, Stream};
use indoc::indoc;
use masq_lib::blockchains::chains::Chain;
use masq_lib::logger::Logger;
use std::convert::{From, TryInto};
use std::fmt::Debug;
use std::rc::Rc;
use futures::future::err;
use libc::addrinfo;
use web3::contract::{Contract, Options};
use web3::transports::{Batch, EventLoopHandle, Http};
use web3::types::{Address, BlockNumber, Log, TransactionReceipt, H256, U256, FilterBuilder};
use web3::{BatchTransport, Error as Web3Error, Web3};
use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::agent_web3::BlockchainAgentWeb3;
use crate::blockchain::blockchain_interface::blockchain_interface_web3::lower_level_interface_web3::LowBlockchainIntWeb3;
use crate::blockchain::blockchain_interface_utils::{get_service_fee_balance, get_transaction_fee_balance, get_transaction_id, request_block_number, create_blockchain_agent_web3, BlockchainAgentFutureResult, get_gas_price};
use crate::sub_lib::blockchain_bridge::ConsumingWalletBalances;

const CONTRACT_ABI: &str = indoc!(
    r#"[{
    "constant":true,
    "inputs":[{"name":"owner","type":"address"}],
    "name":"balanceOf",
    "outputs":[{"name":"","type":"uint256"}],
    "payable":false,
    "stateMutability":"view",
    "type":"function"
    },{
    "constant":false,
    "inputs":[{"name":"to","type":"address"},{"name":"value","type":"uint256"}],
    "name":"transfer",
    "outputs":[{"name":"","type":"bool"}],
    "payable":false,
    "stateMutability":"nonpayable",
    "type":"function"
    }]"#
);

pub const TRANSACTION_LITERAL: H256 = H256([
    0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37, 0x8d, 0xaa,
    0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d, 0xf5, 0x23, 0xb3, 0xef,
]);

pub const TRANSFER_METHOD_ID: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

pub const REQUESTS_IN_PARALLEL: usize = 1;

pub const BLOCKCHAIN_SERVICE_URL_NOT_SPECIFIED: &str =
    "To avoid being delinquency-banned, you should \
restart the Node with a value for blockchain-service-url";

pub type BlockchainResult<T> = Result<T, BlockchainError>;
pub type ResultForBalance = BlockchainResult<web3::types::U256>;
pub type ResultForBothBalances = BlockchainResult<(web3::types::U256, web3::types::U256)>;
pub type ResultForNonce = BlockchainResult<web3::types::U256>;
pub type ResultForReceipt = BlockchainResult<Option<TransactionReceipt>>;

pub struct BlockchainInterfaceNull {
    logger: Logger,
}

pub struct BlockchainInterfaceWeb3 {
    logger: Logger,
    chain: Chain,
    gas_limit_const_part: u64,
    // This must not be dropped for Web3 requests to be completed
    _event_loop_handle: EventLoopHandle,
    transport: Http,
    // lower_interface // TODO: GH-744 Add this back here....
}

pub const GWEI: U256 = U256([1_000_000_000u64, 0, 0, 0]);

pub fn to_wei(gwub: u64) -> U256 {
    let subgwei = U256::from(gwub);
    subgwei.full_mul(GWEI).try_into().expect("Internal Error")
}

impl BlockchainInterface for BlockchainInterfaceWeb3 {
    fn contract_address(&self) -> Address {
        self.chain.rec().contract
    }

    fn get_chain(&self) -> Chain {
        self.chain
    }

    fn get_contract(&self) -> Contract<Http> {
        Contract::from_json(
            self.get_web3().eth(),
            self.chain.rec().contract,
            CONTRACT_ABI.as_bytes(),
        )
        .expect("Unable to initialize contract.")
    }

    fn get_web3(&self) -> Web3<Http> {
        Web3::new(self.transport.clone())
    }

    fn get_web3_batch(&self) -> Web3<Batch<Http>> {
        let transport = self.transport.clone();
        Web3::new(Batch::new(transport))
    }

    fn get_transport(&self) -> Http {
        self.transport.clone()
    }

    fn retrieve_transactions(
        &self,
        start_block: BlockNumber,
        end_block: BlockNumber,
        recipient: &Wallet,
    ) -> Box<dyn Future<Item = RetrievedBlockchainTransactions, Error = BlockchainError>> {
        debug!(
            self.logger,
            "Retrieving transactions from start block: {:?} to end block: {:?} for: {} chain_id: {} contract: {:#x}",
            start_block,
            end_block,
            recipient,
            self.chain.rec().num_chain_id,
            self.contract_address()
        );
        let filter = FilterBuilder::default()
            .address(vec![self.contract_address()])
            .from_block(start_block)
            .to_block(end_block)
            .topics(
                Some(vec![TRANSACTION_LITERAL]),
                None,
                Some(vec![recipient.address().into()]),
                None,
            )
            .build();

        let web3 = self.get_web3();
        let web3_batch = self.get_web3_batch();
        let log_request = web3_batch.eth().logs(filter);
        let logger = self.logger.clone();
        let logger2 = self.logger.clone();

        // web3.eth().logs()
        // TODO: GH-744: Look into why submit batch is being called, can we remove this.
        // web3_batch.eth().logs should be able to be called from just web3.
        return Box::new(
            web3_batch
                .transport()
                .submit_batch()
                .map_err(|e| BlockchainError::QueryFailed(e.to_string()) )
                .then(move |_| {
                    request_block_number(web3, start_block, end_block, logger).then(
                        move |response_block_number| {
                            let response_block_number =
                                response_block_number.unwrap_or_else(|_| {
                                    panic!("This Future always returns successfully");
                                });
                            log_request.then(move |logs| {
                                debug!(logger2, "Transaction retrieval completed: {:?}", logs);
                                future::result::<RetrievedBlockchainTransactions, BlockchainError>(
                                    match logs {
                                        Ok(logs) => {
                                            let logs_len = logs.len();
                                            if logs.iter().any(|log| {
                                                log.topics.len() < 2 || log.data.0.len() > 32
                                            }) {
                                                warning!(
                                                    logger2,
                                                    "Invalid response from blockchain server: {:?}",
                                                    logs
                                                );
                                                Err(BlockchainError::InvalidResponse)
                                            } else {
                                                let transactions: Vec<BlockchainTransaction> = Self::extract_transactions_from_logs(logs);
                                                debug!(
                                                    logger2,
                                                    "Retrieved transactions: {:?}", transactions
                                                );
                                                if transactions.is_empty()
                                                    && logs_len != transactions.len()
                                                {
                                                    warning!(logger2,"Retrieving transactions: logs: {}, transactions: {}",logs_len,transactions.len())
                                                }

                                                // Get the largest transaction block number, unless there are no
                                                // transactions, in which case use end_block, unless get_latest_block()
                                                // was not successful.
                                                let transaction_max_block_number = Self::find_largest_transaction_block_number(
                                                        response_block_number,
                                                        &transactions,
                                                    );
                                                debug!(
                                                    logger2,
                                                    "Discovered transaction max block nbr: {}",
                                                    transaction_max_block_number
                                                );

                                                Ok(RetrievedBlockchainTransactions {
                                                    new_start_block: 1u64 + transaction_max_block_number,
                                                    transactions,
                                                })
                                            }
                                        }
                                        Err(e) => Err(BlockchainError::QueryFailed(e.to_string())),
                                    },
                                )
                            })
                        },
                    )
                }),
        );
    }

    fn build_blockchain_agent(
        &self,
        consuming_wallet: &Wallet,
    ) -> Box<dyn Future<Item = Box<dyn BlockchainAgent>, Error = BlockchainAgentBuildError>> {
        let web3 = self.get_web3();
        let contract = self.get_contract();
        let gas_limit_const_part = self.gas_limit_const_part.clone();
        let wallet_address = consuming_wallet.address();
        let consuming_wallet_clone_1 = consuming_wallet.clone();
        let consuming_wallet_clone_2 = consuming_wallet.clone();
        let consuming_wallet_clone_3 = consuming_wallet.clone();
        let consuming_wallet_clone_4 = consuming_wallet.clone();

        Box::new(
            get_gas_price(web3.clone())
                .map_err(|e| {
                    BlockchainAgentBuildError::GasPrice(e.clone())
                })
                .and_then(move |gas_price_wei| {
                get_transaction_fee_balance(web3.clone(), wallet_address)
                    .map_err(move |e| {
                        BlockchainAgentBuildError::TransactionFeeBalance(
                            consuming_wallet_clone_1,
                            e.clone(),
                        )
                    })
                    .and_then(move |transaction_fee_balance| {
                        get_service_fee_balance(contract, wallet_address)
                            .map_err(move |e| {
                                BlockchainAgentBuildError::ServiceFeeBalance(
                                    consuming_wallet_clone_2,
                                    e.clone(),
                                )
                            })
                            .and_then(move |masq_token_balance| {
                                get_transaction_id(web3, wallet_address)
                                    .map_err(move |e| {
                                        BlockchainAgentBuildError::TransactionID(
                                            consuming_wallet_clone_3,
                                            e.clone(),
                                        )
                                    })
                                    .and_then(move |pending_transaction_id| {
                                        let blockchain_agent_future_result =
                                            BlockchainAgentFutureResult {
                                                gas_price_wei,
                                                transaction_fee_balance,
                                                masq_token_balance,
                                                pending_transaction_id,
                                            };
                                        Ok(create_blockchain_agent_web3(
                                            gas_limit_const_part,
                                            blockchain_agent_future_result,
                                            consuming_wallet_clone_4,
                                        ))
                                    })
                            })
                    })
            }),
        )
    }

    fn get_service_fee_balance(
        // TODO: GH-744 - This has been migrated to Blockchain_interface_utils
        &self,
        wallet_address: Address,
    ) -> Box<dyn Future<Item = U256, Error = BlockchainError>> {
        // Box::new(

        todo!("This is to be Deleted - code migrated to Blockchain_interface_utils")
        // self.get_contract()
        //     .query("balanceOf", wallet_address, None, Options::default(), None)
        //     .map_err(move |e| {
        //         BlockchainError::QueryFailed(format!("{:?} for wallet {}", e, wallet_address))
        //     }),
        // )
    }

    fn get_transaction_fee_balance(
        // TODO: GH-744 - This has been migrated to Blockchain_interface_utils
        &self,
        wallet: &Wallet,
    ) -> Box<dyn Future<Item = U256, Error = BlockchainError>> {
        todo!("This is to be Deleted - code migrated to Blockchain_interface_utils")
        // Box::new(
        //     // self.get_web3()
        //     //     .eth()
        //     //     .balance(wallet.address(), None)
        //     //     .map_err(|e| QueryFailed(e.to_string())),
        // )
    }

    fn get_token_balance(
        // TODO: GH-744 - This has been migrated to Blockchain_interface_utils
        &self,
        wallet: &Wallet,
    ) -> Box<dyn Future<Item = U256, Error = BlockchainError>> {
        todo!("Code migrated to Blockchain_interface_utils");
        Box::new(
            self.get_contract()
                .query(
                    "balanceOf",
                    wallet.address(),
                    None,
                    Options::default(),
                    None,
                )
                .map_err(|e| BlockchainError::QueryFailed(e.to_string())),
        )
    }

    fn get_transaction_count(
        &self,
        wallet: &Wallet,
    ) -> Box<dyn Future<Item = U256, Error = BlockchainError>> {
        Box::new(
            self.get_web3()
                .eth()
                .transaction_count(wallet.address(), Some(BlockNumber::Pending))
                .map_err(|e| BlockchainError::QueryFailed(e.to_string())),
        )
    }

    fn get_transaction_receipt(&self, hash: H256) -> ResultForReceipt {
        self.get_web3()
            .eth()
            .transaction_receipt(hash)
            .map_err(|e| BlockchainError::QueryFailed(e.to_string()))
            .wait()
    }

    fn lower_interface(&self) -> &dyn LowBlockchainInt {
        todo!("GH-744: Need to remove lower_interface");
    }
}

pub type HashAndAmountResult = Result<Vec<(H256, u128)>, PayableTransactionError>;
pub type HashesAndAmounts = Vec<(H256, u128)>;

#[derive(Debug, Clone, PartialEq, Eq, Copy)]
pub struct HashAndAmount {
    pub hash: H256,
    pub amount: u128,
}

impl BlockchainInterfaceWeb3 {
    pub fn new(transport: Http, event_loop_handle: EventLoopHandle, chain: Chain) -> Self {
        // let web3 = Web3::new(transport.clone());
        // let web3 = Rc::new(Web3::new(transport.clone()));
        // let web3_batch = Rc::new(Web3::new(Batch::new(transport.clone())));
        // let contract =
        //     Contract::from_json(web3.eth(), chain.rec().contract, CONTRACT_ABI.as_bytes())
        //         .expect("Unable to initialize contract.");
        // let lower_level_blockchain_interface = Box::new(LowBlockchainIntWeb3::new(
        //     Rc::clone(&web3),
        //     Rc::clone(&web3_batch),
        //     contract,
        // ));
        let gas_limit_const_part = Self::web3_gas_limit_const_part(chain);

        Self {
            logger: Logger::new("BlockchainInterface"),
            chain,
            gas_limit_const_part,
            _event_loop_handle: event_loop_handle,
            // lower_interface: lower_level_blockchain_interface,
            transport,
            // web3,
            // contract,
        }
    }

    fn web3_gas_limit_const_part(chain: Chain) -> u64 {
        match chain {
            Chain::EthMainnet | Chain::EthRopsten | Chain::Dev => 55_000,
            Chain::PolyMainnet | Chain::PolyMumbai => 70_000,
        }
    }

    fn extract_transactions_from_logs(logs: Vec<Log>) -> Vec<BlockchainTransaction> {
        logs.iter()
            .filter_map(|log: &Log| match log.block_number {
                None => None,
                Some(block_number) => {
                    let wei_amount = U256::from(log.data.0.as_slice()).as_u128();
                    Some(BlockchainTransaction {
                        block_number: block_number.as_u64(),
                        from: Wallet::from(log.topics[1]),
                        wei_amount,
                    })
                }
            })
            .collect()
    }

    fn find_largest_transaction_block_number(
        response_block_number: u64,
        transactions: &[BlockchainTransaction],
    ) -> u64 {
        if transactions.is_empty() {
            response_block_number
        } else {
            transactions
                .iter()
                .fold(response_block_number, |a, b| a.max(b.block_number))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::agent_web3::WEB3_MAXIMAL_GAS_LIMIT_MARGIN;
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::test_utils::BlockchainAgentMock;
    use crate::blockchain::bip32::Bip32EncryptionKeyProvider;
    use crate::blockchain::blockchain_interface::blockchain_interface_web3::{
        BlockchainInterfaceWeb3, CONTRACT_ABI, REQUESTS_IN_PARALLEL, TRANSACTION_LITERAL,
        TRANSFER_METHOD_ID,
    };
    use crate::blockchain::blockchain_interface::data_structures::BlockchainTransaction;
    use crate::blockchain::blockchain_interface::test_utils::LowBlockchainIntMock;
    use crate::blockchain::blockchain_interface::{
        BlockchainAgentBuildError, BlockchainError, BlockchainInterface,
        RetrievedBlockchainTransactions,
    };
    use crate::blockchain::test_utils::{
        all_chains, make_blockchain_interface_web3, make_fake_event_loop_handle, make_tx_hash,
        TestTransport,
    };
    use crate::db_config::persistent_configuration::PersistentConfigError;
    use crate::sub_lib::blockchain_bridge::ConsumingWalletBalances;
    use crate::sub_lib::wallet::Wallet;
    use crate::test_utils::http_test_server::TestServer;
    use crate::test_utils::persistent_configuration_mock::PersistentConfigurationMock;
    use crate::test_utils::{assert_string_contains, make_paying_wallet};
    use crate::test_utils::{make_wallet, TestRawTransaction};
    use ethereum_types::U64;
    use ethsign_crypto::Keccak256;
    use futures::Future;
    use indoc::indoc;
    use masq_lib::blockchains::chains::Chain;
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use masq_lib::test_utils::utils::TEST_DEFAULT_CHAIN;
    use masq_lib::utils::find_free_port;
    use serde_derive::Deserialize;
    use serde_json::Value;
    use std::net::Ipv4Addr;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use web3::transports::Http;
    use web3::types::{
        BlockNumber, Bytes, TransactionParameters, TransactionReceipt, H2048, H256, U256,
    };
    use masq_lib::test_utils::mock_blockchain_client_server::MBCSBuilder;

    #[test]
    fn constants_are_correct() {
        let contract_abi_expected: &str = indoc!(
            r#"[{
            "constant":true,
            "inputs":[{"name":"owner","type":"address"}],
            "name":"balanceOf",
            "outputs":[{"name":"","type":"uint256"}],
            "payable":false,
            "stateMutability":"view",
            "type":"function"
            },{
            "constant":false,
            "inputs":[{"name":"to","type":"address"},{"name":"value","type":"uint256"}],
            "name":"transfer",
            "outputs":[{"name":"","type":"bool"}],
            "payable":false,
            "stateMutability":"nonpayable",
            "type":"function"
            }]"#
        );
        let transaction_literal_expected: H256 = H256 {
            0: [
                0xdd, 0xf2, 0x52, 0xad, 0x1b, 0xe2, 0xc8, 0x9b, 0x69, 0xc2, 0xb0, 0x68, 0xfc, 0x37,
                0x8d, 0xaa, 0x95, 0x2b, 0xa7, 0xf1, 0x63, 0xc4, 0xa1, 0x16, 0x28, 0xf5, 0x5a, 0x4d,
                0xf5, 0x23, 0xb3, 0xef,
            ],
        };
        assert_eq!(CONTRACT_ABI, contract_abi_expected);
        assert_eq!(TRANSACTION_LITERAL, transaction_literal_expected);
        assert_eq!(TRANSFER_METHOD_ID, [0xa9, 0x05, 0x9c, 0xbb]);
        assert_eq!(REQUESTS_IN_PARALLEL, 1);
    }

    #[test]
    fn blockchain_interface_web3_can_return_contract() {
        all_chains().iter().for_each(|chain| {
            let mut subject = make_blockchain_interface_web3(None);
            subject.chain = *chain;

            assert_eq!(subject.contract_address(), chain.rec().contract)
        })
    }

    #[test]
    fn blockchain_interface_web3_retrieves_transactions() {
        let to = "0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc";
        let port = find_free_port();
        #[rustfmt::skip]
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .raw_response(
                r#"{
                "jsonrpc":"2.0",
                "id":3,
                "result":[
                    {
                        "address":"0xcd6c588e005032dd882cd43bf53a32129be81302",
                        "blockHash":"0x1a24b9169cbaec3f6effa1f600b70c7ab9e8e86db44062b49132a4415d26732a",
                        "blockNumber":"0x4be663",
                        "data":"0x0000000000000000000000000000000000000000000000000010000000000000",
                        "logIndex":"0x0",
                        "removed":false,
                        "topics":[
                            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                            "0x0000000000000000000000003ab28ecedea6cdb6feed398e93ae8c7b316b1182",
                            "0x000000000000000000000000adc1853c7859369639eb414b6342b36288fe6092"
                        ],
                        "transactionHash":"0x955cec6ac4f832911ab894ce16aa22c3003f46deff3f7165b32700d2f5ff0681",
                        "transactionIndex":"0x0"
                    },
                    {
                        "address":"0xcd6c588e005032dd882cd43bf53a32129be81302",
                        "blockHash":"0x1a24b9169cbaec3f6effa1f600b70c7ab9e8e86db44062b49132a4415d26732b",
                        "blockNumber":"0x4be662",
                        "data":"0x0000000000000000000000000000000000000000000000000010000000000000",
                        "logIndex":"0x0",
                        "removed":false,
                        "topics":[
                            "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                            "0x0000000000000000000000003f69f9efd4f2592fd70be8c32ecd9dce71c472fc",
                            "0x000000000000000000000000adc1853c7859369639eb414b6342b36288fe6092"
                        ],
                        "transactionHash":"0x955cec6ac4f832911ab894ce16aa22c3003f46deff3f7165b32700d2f5ff0680",
                        "transactionIndex":"0x0"
                    }
                ]
            }"#.to_string()
            )
            .end_batch()
            .response("0x178def".to_string(), 2)
            .start();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let chain = TEST_DEFAULT_CHAIN;
        let subject = BlockchainInterfaceWeb3::new(transport, event_loop_handle, chain);
        let end_block_nbr = 1024u64;

        let result = subject
            .retrieve_transactions(
                BlockNumber::Number(42u64.into()),
                BlockNumber::Number(end_block_nbr.into()),
                &Wallet::from_str(&to).unwrap(),
            )
            .wait()
            .unwrap();

        assert_eq!(
            result,
            RetrievedBlockchainTransactions {
                new_start_block: 0x4be664,
                transactions: vec![
                    BlockchainTransaction {
                        block_number: 0x4be663,
                        from: Wallet::from_str("0x3ab28ecedea6cdb6feed398e93ae8c7b316b1182")
                            .unwrap(),
                        wei_amount: 4_503_599_627_370_496u128,
                    },
                    BlockchainTransaction {
                        block_number: 0x4be662,
                        from: Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc")
                            .unwrap(),
                        wei_amount: 4_503_599_627_370_496u128,
                    },
                ]
            }
        )
    }

    #[test]
    fn get_transaction_count_works() {
        let port = find_free_port();
        let wallet = make_paying_wallet(b"test_wallet");
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x1".to_string(), 2)
            .start();

        let subject = make_blockchain_interface_web3(Some(port));

        let result = subject.get_transaction_count(&wallet).wait();
        assert_eq!(result, Ok(1.into()));
    }

    #[test]
    fn get_transaction_count_gets_error() {
        let port = find_free_port();
        let wallet = make_paying_wallet(b"test_wallet");
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("trash".to_string(), 2)
            .start();

        let subject = make_blockchain_interface_web3(Some(port));

        let result = subject.get_transaction_count(&wallet).wait();
        assert_eq!(
            result,
            Err(QueryFailed(
                "Decoder error: Error(\"0x prefix is missing\", line: 0, column: 0)".to_string()
            ))
        );
    }

    #[test]
    fn blockchain_interface_web3_handles_no_retrieved_transactions() {
        let to_wallet = make_paying_wallet(b"test_wallet");
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .raw_response(r#"{"jsonrpc":"2.0","id":3,"result":[]}"#.to_string())
            .end_batch()
            .response("0x178def".to_string(), 2)
            .start();
        let subject = make_blockchain_interface_web3(Some(port));
        let end_block_nbr = 1024u64;

        let result = subject
            .retrieve_transactions(
                BlockNumber::Number(42u64.into()),
                BlockNumber::Number(end_block_nbr.into()),
                &to_wallet,
            )
            .wait();

        assert_eq!(
            result,
            Ok(RetrievedBlockchainTransactions {
                new_start_block: 1543664,
                transactions: vec![]
            })
        );
    }

    #[test]
    #[should_panic(expected = "No address for an uninitialized wallet!")]
    fn blockchain_interface_web3_retrieve_transactions_returns_an_error_if_the_to_address_is_invalid(
    ) {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let chain = TEST_DEFAULT_CHAIN;
        let subject = BlockchainInterfaceWeb3::new(transport, event_loop_handle, chain);

        let result = subject
            .retrieve_transactions(
                BlockNumber::Number(42u64.into()),
                BlockNumber::Latest,
                &Wallet::new("0x3f69f9efd4f2592fd70beecd9dce71c472fc"),
            )
            .wait();

        assert_eq!(
            result.expect_err("Expected an Err, got Ok"),
            BlockchainError::InvalidAddress
        );
    }

    #[test]
    fn blockchain_interface_web3_retrieve_transactions_returns_an_error_if_a_response_with_too_few_topics_is_returned(
    ) {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .raw_response(r#"{"jsonrpc":"2.0","id":3,"result":[{"address":"0xcd6c588e005032dd882cd43bf53a32129be81302","blockHash":"0x1a24b9169cbaec3f6effa1f600b70c7ab9e8e86db44062b49132a4415d26732a","blockNumber":"0x4be663","data":"0x0000000000000000000000000000000000000000000000056bc75e2d63100000","logIndex":"0x0","removed":false,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"],"transactionHash":"0x955cec6ac4f832911ab894ce16aa22c3003f46deff3f7165b32700d2f5ff0681","transactionIndex":"0x0"}]}"#.to_string())
            .end_batch()
            .start();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let chain = TEST_DEFAULT_CHAIN;
        let subject = BlockchainInterfaceWeb3::new(transport, event_loop_handle, chain);

        let result = subject
            .retrieve_transactions(
                BlockNumber::Number(42u64.into()),
                BlockNumber::Latest,
                &Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap(),
            )
            .wait();

        assert_eq!(
            result.expect_err("Expected an Err, got Ok"),
            BlockchainError::InvalidResponse
        );
    }

    #[test]
    fn blockchain_interface_web3_retrieve_transactions_returns_an_error_if_a_response_with_data_that_is_too_long_is_returned(
    ) {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .raw_response(r#"{"jsonrpc":"2.0","id":3,"result":[{"address":"0xcd6c588e005032dd882cd43bf53a32129be81302","blockHash":"0x1a24b9169cbaec3f6effa1f600b70c7ab9e8e86db44062b49132a4415d26732a","blockNumber":"0x4be663","data":"0x0000000000000000000000000000000000000000000000056bc75e2d6310000001","logIndex":"0x0","removed":false,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef","0x0000000000000000000000003f69f9efd4f2592fd70be8c32ecd9dce71c472fc","0x000000000000000000000000adc1853c7859369639eb414b6342b36288fe6092"],"transactionHash":"0x955cec6ac4f832911ab894ce16aa22c3003f46deff3f7165b32700d2f5ff0681","transactionIndex":"0x0"}]}"#.to_string())
            .end_batch()
            .start();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let chain = TEST_DEFAULT_CHAIN;
        let subject = BlockchainInterfaceWeb3::new(transport, event_loop_handle, chain);

        let result = subject
            .retrieve_transactions(
                BlockNumber::Number(42u64.into()),
                BlockNumber::Latest,
                &Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap(),
            )
            .wait();

        assert_eq!(result, Err(BlockchainError::InvalidResponse));
    }

    #[test]
    fn blockchain_interface_web3_retrieve_transactions_ignores_transaction_logs_that_have_no_block_number(
    ) {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .raw_response(r#"{"jsonrpc":"2.0","id":2,"result":[{"address":"0xcd6c588e005032dd882cd43bf53a32129be81302","blockHash":"0x1a24b9169cbaec3f6effa1f600b70c7ab9e8e86db44062b49132a4415d26732a","data":"0x0000000000000000000000000000000000000000000000000010000000000000","logIndex":"0x0","removed":false,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef","0x0000000000000000000000003f69f9efd4f2592fd70be8c32ecd9dce71c472fc","0x000000000000000000000000adc1853c7859369639eb414b6342b36288fe6092"],"transactionHash":"0x955cec6ac4f832911ab894ce16aa22c3003f46deff3f7165b32700d2f5ff0681","transactionIndex":"0x0"}]}"#.to_string())
            .end_batch()
            .response("0x178def", 1)
            .start();
        init_test_logging();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();

        let end_block_nbr = 1024u64;
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);

        let result = subject
            .retrieve_transactions(
                BlockNumber::Number(42u64.into()),
                BlockNumber::Number(end_block_nbr.into()),
                &Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap(),
            )
            .wait();

        assert_eq!(
            result,
            Ok(RetrievedBlockchainTransactions {
                new_start_block: 1543664,
                transactions: vec![]
            })
        );
        let test_log_handler = TestLogHandler::new();
        test_log_handler.exists_log_containing(
            "WARN: BlockchainInterface: Retrieving transactions: logs: 1, transactions: 0",
        );
    }

    #[test]
    fn blockchain_interface_non_clandestine_retrieve_transactions_uses_block_number_latest_as_fallback_start_block_plus_one(
    ) {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .raw_response(r#"{"jsonrpc":"2.0","id":2,"result":[{"address":"0xcd6c588e005032dd882cd43bf53a32129be81302","blockHash":"0x1a24b9169cbaec3f6effa1f600b70c7ab9e8e86db44062b49132a4415d26732a","data":"0x0000000000000000000000000000000000000000000000000010000000000000","logIndex":"0x0","removed":false,"topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef","0x0000000000000000000000003f69f9efd4f2592fd70be8c32ecd9dce71c472fc","0x000000000000000000000000adc1853c7859369639eb414b6342b36288fe6092"],"transactionHash":"0x955cec6ac4f832911ab894ce16aa22c3003f46deff3f7165b32700d2f5ff0681","transactionIndex":"0x0"}]}"#.to_string())
            .end_batch()
            .start();
        let subject = make_blockchain_interface_web3(Some(port));
        let start_block = BlockNumber::Number(42u64.into());

        let result = subject
            .retrieve_transactions(
                start_block,
                BlockNumber::Latest,
                &Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap(),
            )
            .wait();

        let expected_fallback_start_block =
            if let BlockNumber::Number(start_block_nbr) = start_block {
                start_block_nbr.as_u64() + 1u64
            } else {
                panic!("start_block of Latest, Earliest, and Pending are not supported!")
            };

        assert_eq!(
            result,
            Ok(RetrievedBlockchainTransactions {
                new_start_block: 1 + expected_fallback_start_block,
                transactions: vec![]
            })
        );
    }

    #[test]
    fn blockchain_interface_web3_can_build_blockchain_agent() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x3B9ACA00".to_string(), 0)
            .response("0xFFF0".to_string(), 0)
            .response("0x000000000000000000000000000000000000000000000000000000000000FFFF".to_string(), 0)
            .response("0x23".to_string(), 1)
            .start();
        let chain = Chain::PolyMainnet;
        let wallet = make_wallet("abc");
        let subject = make_blockchain_interface_web3(Some(port));
        let transaction_fee_balance = U256::from(65_520);
        let masq_balance = U256::from(65_535);
        let transaction_id = U256::from(35);

        let result = subject
            .build_blockchain_agent(&wallet)
            .wait()
            .unwrap();


        let expected_gas_price_gwei = 1;
        assert_eq!(result.consuming_wallet(), &wallet);
        assert_eq!(result.pending_transaction_id(), transaction_id);
        assert_eq!(
            result.consuming_wallet_balances(),
            ConsumingWalletBalances {
                transaction_fee_balance_in_minor_units: transaction_fee_balance,
                masq_token_balance_in_minor_units: masq_balance
            }
        );
        assert_eq!(result.agreed_fee_per_computation_unit(), expected_gas_price_gwei);
        let expected_fee_estimation = (3
            * (BlockchainInterfaceWeb3::web3_gas_limit_const_part(chain)
                + WEB3_MAXIMAL_GAS_LIMIT_MARGIN)
            * expected_gas_price_gwei) as u128;
        assert_eq!(
            result.estimated_transaction_fee_total(3),
            expected_fee_estimation
        )
    }

    #[test]
    fn build_of_the_blockchain_agent_fails_on_fetching_gas_price() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port).start();
        let chain = Chain::PolyMumbai;
        let wallet = make_wallet("abc");
        let subject = make_blockchain_interface_web3(Some(port));

        let err = subject.build_blockchain_agent(&wallet).wait().err().unwrap();

        let expected_err = BlockchainAgentBuildError::GasPrice(
            QueryFailed("Transport error: Error(IncompleteMessage)".to_string()),
        );
        assert_eq!(err, expected_err)
    }

    fn build_of_the_blockchain_agent_fails_on_blockchain_interface_error<F>(
        port: u16,
        expected_err_factory: F,
    ) where
        F: FnOnce(&Wallet) -> BlockchainAgentBuildError,
    {
        let chain = Chain::EthMainnet;
        let wallet = make_wallet("bcd");
        let mut subject = make_blockchain_interface_web3(Some(port));
        // TODO: GH-744: Come back to this
        // subject.lower_interface = Box::new(lower_blockchain_interface);

        let result = subject
            .build_blockchain_agent(&wallet)
            .wait();

        let err = match result {
            Err(e) => e,
            _ => panic!("we expected Err() but got Ok()"),
        };
        let expected_err = expected_err_factory(&wallet);
        assert_eq!(err, expected_err)
    }

    #[test]
    fn build_of_the_blockchain_agent_fails_on_transaction_fee_balance() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x3B9ACA00".to_string(), 0)
            .start();
        let expected_err_factory = |wallet: &Wallet| {
            BlockchainAgentBuildError::TransactionFeeBalance(
                wallet.clone(),
                BlockchainError::QueryFailed("Transport error: Error(IncompleteMessage)".to_string())
            )
        };

        build_of_the_blockchain_agent_fails_on_blockchain_interface_error(
            port,
            expected_err_factory,
        );
    }

    #[test]
    fn build_of_the_blockchain_agent_fails_on_masq_balance() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x3B9ACA00".to_string(), 0)
            .response("0xFFF0".to_string(), 0)
            .start();
        let expected_err_factory = |wallet: &Wallet| {
            BlockchainAgentBuildError::ServiceFeeBalance(
                wallet.clone(),
                BlockchainError::QueryFailed("Api error: Transport error: Error(IncompleteMessage)".to_string())
            )
        };

        build_of_the_blockchain_agent_fails_on_blockchain_interface_error(
            port,
            expected_err_factory,
        );
    }

    #[test]
    fn build_of_the_blockchain_agent_fails_on_transaction_id() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x3B9ACA00".to_string(), 0)
            .response("0xFFF0".to_string(), 0)
            .response("0x000000000000000000000000000000000000000000000000000000000000FFFF".to_string(), 0)
            .start();

        let expected_err_factory = |wallet: &Wallet| {
            BlockchainAgentBuildError::TransactionID(
                wallet.clone(),
                BlockchainError::QueryFailed("Transport error: Error(IncompleteMessage) for wallet 0x0000…6364".to_string())
            )
        };

        build_of_the_blockchain_agent_fails_on_blockchain_interface_error(
            port,
            expected_err_factory,
        );
    }

    // TODO: GH-744 - We had removed this test, but master has some changes, so its been brought back
    // #[test]
    // fn blockchain_interface_web3_can_transfer_tokens_in_batch() {
    //     //exercising also the layer of web3 functions, but the transport layer is mocked
    //     init_test_logging();
    //     let send_batch_params_arc = Arc::new(Mutex::new(vec![]));
    //     //we compute the hashes ourselves during the batch preparation and so we don't care about
    //     //the same ones coming back with the response; we use the returned OKs as indicators of success only.
    //     //Any eventual rpc errors brought back are processed as well...
    //     let expected_batch_responses = vec![
    //         Ok(json!("...unnecessarily important hash...")),
    //         Err(web3::Error::Rpc(RPCError {
    //             code: ErrorCode::ServerError(114),
    //             message: "server being busy".to_string(),
    //             data: None,
    //         })),
    //         Ok(json!("...unnecessarily important hash...")),
    //     ];
    //     let transport = TestTransport::default()
    //         .send_batch_params(&send_batch_params_arc)
    //         .send_batch_result(expected_batch_responses);
    //     let (accountant, _, accountant_recording_arc) = make_recorder();
    //     let actor_addr = accountant.start();
    //     let fingerprint_recipient = recipient!(actor_addr, PendingPayableFingerprintSeeds);
    //     let logger = Logger::new("sending_batch_payments");
    //     let chain = TEST_DEFAULT_CHAIN;
    //     let mut subject =
    //         BlockchainInterfaceWeb3::new(transport.clone(), make_fake_event_loop_handle(), chain);
    //     subject.logger = logger;
    //     let amount_1 = gwei_to_wei(900_000_000_u64);
    //     let account_1 = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
    //         make_wallet("w123"),
    //         amount_1,
    //         None,
    //     );
    //     let amount_2 = 123_456_789;
    //     let account_2 = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
    //         make_wallet("w555"),
    //         amount_2,
    //         None,
    //     );
    //     let amount_3 = gwei_to_wei(33_355_666_u64);
    //     let account_3 = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
    //         make_wallet("w987"),
    //         amount_3,
    //         None,
    //     );
    //     let accounts_to_process = vec![account_1, account_2, account_3];
    //     let consuming_wallet = make_paying_wallet(b"gdasgsa");
    //     let agent = make_initialized_agent(120, consuming_wallet, U256::from(6));
    //     let test_timestamp_before = SystemTime::now();
    //
    //     let result = subject
    //         .send_batch_of_payables(agent, &fingerprint_recipient, &accounts_to_process)
    //         .unwrap();
    //
    //     let test_timestamp_after = SystemTime::now();
    //     let system = System::new("can transfer tokens test");
    //     System::current().stop();
    //     assert_eq!(system.run(), 0);
    //     let send_batch_params = send_batch_params_arc.lock().unwrap();
    //     assert_eq!(
    //         *send_batch_params,
    //         vec![vec![
    //             (
    //                 1,
    //                 Call::MethodCall(MethodCall {
    //                     jsonrpc: Some(V2),
    //                     method: "eth_sendRawTransaction".to_string(),
    //                     params: Params::Array(vec![Value::String("0xf8a906851bf08eb00082db6894384dec25e03f94931767ce4c3556168468ba24c380b844a9059cbb000\
    //     00000000000000000000000000000000000000000000000000000773132330000000000000000000000000000000000000000000000000c7d713b49da00002aa060b9f375c06f56\
    //     41951606643d76ef999d32ae02f6b6cd62c9275ebdaa36a390a0199c3d8644c428efd5e0e0698c031172ac6873037d90dcca36a1fbf2e67960ff".to_string())]),
    //                     id: Id::Num(1)
    //                 })
    //             ),
    //             (
    //                 2,
    //                 Call::MethodCall(MethodCall {
    //                     jsonrpc: Some(V2),
    //                     method: "eth_sendRawTransaction".to_string(),
    //                     params: Params::Array(vec![Value::String("0xf8a907851bf08eb00082dae894384dec25e03f94931767ce4c3556168468ba24c380b844a9059cbb000\
    //     000000000000000000000000000000000000000000000000000007735353500000000000000000000000000000000000000000000000000000000075bcd1529a00e61352bb2ac9b\
    //     32b411206250f219b35cdc85db679f3e2416daac4f730a12f1a02c2ad62759d86942f3af2b8915ecfbaa58268010e00d32c18a49a9fc3b9bd20a".to_string())]),
    //                     id: Id::Num(1)
    //                 })
    //             ),
    //             (
    //                 3,
    //                 Call::MethodCall(MethodCall {
    //                     jsonrpc: Some(V2),
    //                     method: "eth_sendRawTransaction".to_string(),
    //                     params: Params::Array(vec![Value::String("0xf8a908851bf08eb00082db6894384dec25e03f94931767ce4c3556168468ba24c380b844a9059cbb000\
    //     0000000000000000000000000000000000000000000000000000077393837000000000000000000000000000000000000000000000000007680cd2f2d34002aa02d300cc8ba7b63\
    //     b0147727c824a54a7db9ec083273be52a32bdca72657a3e310a042a17224b35e7036d84976a23fbe8b1a488b2bcabed1e4a2b0b03f0c9bbc38e9".to_string())]),
    //                     id: Id::Num(1)
    //                 })
    //             )
    //         ]]
    //     );
    //     let check_expected_successful_request = |expected_hash: H256, idx: usize| {
    //         let pending_payable = match &result[idx]{
    //             Ok(pp) => pp,
    //             Err(RpcPayablesFailure { rpc_error, recipient_wallet: recipient, hash }) => panic!(
    //                 "we expected correct pending payable but got one with rpc_error: {:?} and hash: {} for recipient: {}",
    //                 rpc_error, hash, recipient
    //             ),
    //         };
    //         let hash = pending_payable.hash;
    //         assert_eq!(hash, expected_hash)
    //     };
    //     //first successful request
    //     let expected_hash_1 =
    //         H256::from_str("26e5e0cec02023e40faff67e88e3cf48a98574b5f9fdafc03ef42cad96dae1c1")
    //             .unwrap();
    //     check_expected_successful_request(expected_hash_1, 0);
    //     //failing request
    //     let pending_payable_fallible_2 = &result[1];
    //     let (rpc_error, recipient_2, hash_2) = match pending_payable_fallible_2 {
    //         Ok(pp) => panic!(
    //             "we expected failing pending payable but got a good one: {:?}",
    //             pp
    //         ),
    //         Err(RpcPayablesFailure {
    //             rpc_error,
    //             recipient_wallet: recipient,
    //             hash,
    //         }) => (rpc_error, recipient, hash),
    //     };
    //     assert_eq!(
    //         rpc_error,
    //         &web3::Error::Rpc(RPCError {
    //             code: ErrorCode::ServerError(114),
    //             message: "server being busy".to_string(),
    //             data: None
    //         })
    //     );
    //     let expected_hash_2 =
    //         H256::from_str("57e7c9a5f6af1ab3363e323d59c2c9d1144bbb1a7c2065eeb6696d4e302e67f2")
    //             .unwrap();
    //     assert_eq!(hash_2, &expected_hash_2);
    //     assert_eq!(recipient_2, &make_wallet("w555"));
    //     //second_succeeding_request
    //     let expected_hash_3 =
    //         H256::from_str("a472e3b81bc167140a217447d9701e9ed2b65252f1428f7779acc3710a9ede44")
    //             .unwrap();
    //     check_expected_successful_request(expected_hash_3, 2);
    //     let accountant_recording = accountant_recording_arc.lock().unwrap();
    //     assert_eq!(accountant_recording.len(), 1);
    //     let initiate_fingerprints_msg =
    //         accountant_recording.get_record::<PendingPayableFingerprintSeeds>(0);
    //     let actual_common_timestamp = initiate_fingerprints_msg.batch_wide_timestamp;
    //     assert!(
    //         test_timestamp_before <= actual_common_timestamp
    //             && actual_common_timestamp <= test_timestamp_after
    //     );
    //     assert_eq!(
    //         initiate_fingerprints_msg,
    //         &PendingPayableFingerprintSeeds {
    //             batch_wide_timestamp: actual_common_timestamp,
    //             hashes_and_balances: vec![
    //                 (expected_hash_1, gwei_to_wei(900_000_000_u64)),
    //                 (expected_hash_2, 123_456_789),
    //                 (expected_hash_3, gwei_to_wei(33_355_666_u64))
    //             ]
    //         }
    //     );
    //     let log_handler = TestLogHandler::new();
    //     log_handler.exists_log_containing("DEBUG: sending_batch_payments: \
    //     Common attributes of payables to be transacted: sender wallet: 0x5c361ba8d82fcf0e5538b2a823e9d457a2296725, contract: \
    //       0x384dec25e03f94931767ce4c3556168468ba24c3, chain_id: 3, gas_price: 120");
    //     log_handler.exists_log_containing(
    //         "DEBUG: sending_batch_payments: Preparing payment of 900,000,000,000,000,000 wei \
    //     to 0x0000000000000000000000000000000077313233 with nonce 6",
    //     );
    //     log_handler.exists_log_containing(
    //         "DEBUG: sending_batch_payments: Preparing payment of 123,456,789 wei \
    //     to 0x0000000000000000000000000000000077353535 with nonce 7",
    //     );
    //     log_handler.exists_log_containing(
    //         "DEBUG: sending_batch_payments: Preparing payment of 33,355,666,000,000,000 wei \
    //     to 0x0000000000000000000000000000000077393837 with nonce 8",
    //     );
    //     log_handler.exists_log_containing(
    //         "INFO: sending_batch_payments: Paying to creditors...\n\
    //     Transactions in the batch:\n\
    //     \n\
    //     gas price:                                   120 gwei\n\
    //     chain:                                       ropsten\n\
    //     \n\
    //     [wallet address]                             [payment in wei]\n\
    //     0x0000000000000000000000000000000077313233   900,000,000,000,000,000\n\
    //     0x0000000000000000000000000000000077353535   123,456,789\n\
    //     0x0000000000000000000000000000000077393837   33,355,666,000,000,000\n",
    //     );
    // }

    // TODO: GH-744: This had batch payable tools, come back to this later
    // #[test]
    // fn send_payables_within_batch_components_are_used_together_properly() {
    //     let sign_transaction_params_arc = Arc::new(Mutex::new(vec![]));
    //     let append_transaction_to_batch_params_arc = Arc::new(Mutex::new(vec![]));
    //     let new_payable_fingerprint_params_arc = Arc::new(Mutex::new(vec![]));
    //     let submit_batch_params_arc: Arc<Mutex<Vec<Web3<Batch<TestTransport>>>>> =
    //         Arc::new(Mutex::new(vec![]));
    //     let reference_counter_arc = Arc::new(());
    //     let (accountant, _, accountant_recording_arc) = make_recorder();
    //     let initiate_fingerprints_recipient = accountant.start().recipient();
    //     let consuming_wallet_secret = b"consuming_wallet_0123456789abcde";
    //     let secret_key =
    //         (&Bip32EncryptionKeyProvider::from_raw_secret(consuming_wallet_secret).unwrap()).into();
    //     let batch_wide_timestamp_expected = SystemTime::now();
    //     let transport = TestTransport::default().initiate_reference_counter(&reference_counter_arc);
    //     let chain = Chain::EthMainnet;
    //     let mut subject =
    //         BlockchainInterfaceWeb3::new(transport, make_fake_event_loop_handle(), chain);
    //     let first_transaction_params_expected = TransactionParameters {
    //         nonce: Some(U256::from(4)),
    //         to: Some(subject.contract_address()),
    //         gas: U256::from(56_552),
    //         gas_price: Some(U256::from(123000000000_u64)),
    //         value: U256::from(0),
    //         data: Bytes(vec![
    //             169, 5, 156, 187, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    //             99, 114, 101, 100, 105, 116, 111, 114, 51, 50, 49, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    //             0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 77, 149, 149, 231, 24,
    //         ]),
    //         chain_id: Some(chain.rec().num_chain_id),
    //     };
    //     let first_signed_transaction = subject
    //         .web3
    //         .accounts()
    //         .sign_transaction(first_transaction_params_expected.clone(), &secret_key)
    //         .wait()
    //         .unwrap();
    //     let second_transaction_params_expected = TransactionParameters {
    //         nonce: Some(U256::from(5)),
    //         to: Some(subject.contract_address()),
    //         gas: U256::from(56_552),
    //         gas_price: Some(U256::from(123000000000_u64)),
    //         value: U256::from(0),
    //         data: Bytes(vec![
    //             169, 5, 156, 187, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    //             99, 114, 101, 100, 105, 116, 111, 114, 49, 50, 51, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    //             0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 156, 231, 56, 4,
    //         ]),
    //         chain_id: Some(chain.rec().num_chain_id),
    //     };
    //     let second_signed_transaction = subject
    //         .web3
    //         .accounts()
    //         .sign_transaction(second_transaction_params_expected.clone(), &secret_key)
    //         .wait()
    //         .unwrap();
    //     let first_hash = first_signed_transaction.transaction_hash;
    //     let second_hash = second_signed_transaction.transaction_hash;
    //     //technically, the JSON values in the correct responses don't matter, we only check for errors if any came back
    //     let rpc_responses = vec![
    //         Ok(Value::String((&first_hash.to_string()[2..]).to_string())),
    //         Ok(Value::String((&second_hash.to_string()[2..]).to_string())),
    //     ];
    //     let batch_payables_tools = BatchPayableToolsMock::default()
    //         .sign_transaction_params(&sign_transaction_params_arc)
    //         .sign_transaction_result(Ok(first_signed_transaction.clone()))
    //         .sign_transaction_result(Ok(second_signed_transaction.clone()))
    //         .batch_wide_timestamp_result(batch_wide_timestamp_expected)
    //         .send_new_payable_fingerprint_credentials_params(&new_payable_fingerprint_params_arc)
    //         .append_transaction_to_batch_params(&append_transaction_to_batch_params_arc)
    //         .submit_batch_params(&submit_batch_params_arc)
    //         .submit_batch_result(Ok(rpc_responses));
    //     subject.batch_payable_tools = Box::new(batch_payables_tools);
    //     let consuming_wallet = make_paying_wallet(consuming_wallet_secret);
    //     let first_payment_amount = 333_222_111_000;
    //     let first_creditor_wallet = make_wallet("creditor321");
    //     let first_account = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
    //         first_creditor_wallet.clone(),
    //         first_payment_amount,
    //         None,
    //     );
    //     let second_payment_amount = 11_222_333_444;
    //     let second_creditor_wallet = make_wallet("creditor123");
    //     let second_account = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
    //         second_creditor_wallet.clone(),
    //         second_payment_amount,
    //         None,
    //     );
    //     let agent = make_initialized_agent(123, consuming_wallet, U256::from(4));
    //
    //     let result = subject.send_batch_of_payables(
    //         agent,
    //         &initiate_fingerprints_recipient,
    //         &vec![first_account, second_account],
    //     );
    //
    //     let first_resulting_pending_payable = PendingPayable {
    //         recipient_wallet: first_creditor_wallet.clone(),
    //         hash: first_hash,
    //     };
    //     let second_resulting_pending_payable = PendingPayable {
    //         recipient_wallet: second_creditor_wallet.clone(),
    //         hash: second_hash,
    //     };
    //     assert_eq!(
    //         result,
    //         Ok(vec![
    //             Ok(first_resulting_pending_payable),
    //             Ok(second_resulting_pending_payable)
    //         ])
    //     );
    //     let mut sign_transaction_params = sign_transaction_params_arc.lock().unwrap();
    //     let (first_transaction_params_actual, web3, secret) = sign_transaction_params.remove(0);
    //     assert_eq!(
    //         first_transaction_params_actual,
    //         first_transaction_params_expected
    //     );
    //     let check_web3_origin = |web3: &Web3<Batch<TestTransport>>| {
    //         let ref_count_before_clone = Arc::strong_count(&reference_counter_arc);
    //         let _new_ref = web3.clone();
    //         let ref_count_after_clone = Arc::strong_count(&reference_counter_arc);
    //         assert_eq!(ref_count_after_clone, ref_count_before_clone + 1);
    //     };
    //     check_web3_origin(&web3);
    //     assert_eq!(
    //         secret,
    //         (&Bip32EncryptionKeyProvider::from_raw_secret(&consuming_wallet_secret.keccak256())
    //             .unwrap())
    //             .into()
    //     );
    //     let (second_transaction_params_actual, web3_from_st_call, secret) =
    //         sign_transaction_params.remove(0);
    //     assert_eq!(
    //         second_transaction_params_actual,
    //         second_transaction_params_expected
    //     );
    //     check_web3_origin(&web3_from_st_call);
    //     assert_eq!(
    //         secret,
    //         (&Bip32EncryptionKeyProvider::from_raw_secret(&consuming_wallet_secret.keccak256())
    //             .unwrap())
    //             .into()
    //     );
    //     assert!(sign_transaction_params.is_empty());
    //     let new_payable_fingerprint_params = new_payable_fingerprint_params_arc.lock().unwrap();
    //     let (batch_wide_timestamp, recipient, actual_pending_payables) =
    //         &new_payable_fingerprint_params[0];
    //     assert_eq!(batch_wide_timestamp, &batch_wide_timestamp_expected);
    //     assert_eq!(
    //         actual_pending_payables,
    //         &vec![
    //             (first_hash, first_payment_amount),
    //             (second_hash, second_payment_amount)
    //         ]
    //     );
    //     let mut append_transaction_to_batch_params =
    //         append_transaction_to_batch_params_arc.lock().unwrap();
    //     let (bytes_first_payment, web3_from_ertb_call_1) =
    //         append_transaction_to_batch_params.remove(0);
    //     check_web3_origin(&web3_from_ertb_call_1);
    //     assert_eq!(
    //         bytes_first_payment,
    //         first_signed_transaction.raw_transaction
    //     );
    //     let (bytes_second_payment, web3_from_ertb_call_2) =
    //         append_transaction_to_batch_params.remove(0);
    //     check_web3_origin(&web3_from_ertb_call_2);
    //     assert_eq!(
    //         bytes_second_payment,
    //         second_signed_transaction.raw_transaction
    //     );
    //     assert_eq!(append_transaction_to_batch_params.len(), 0);
    //     let submit_batch_params = submit_batch_params_arc.lock().unwrap();
    //     let web3_from_sb_call = &submit_batch_params[0];
    //     assert_eq!(submit_batch_params.len(), 1);
    //     check_web3_origin(&web3_from_sb_call);
    //     assert!(accountant_recording_arc.lock().unwrap().is_empty());
    //     let system =
    //         System::new("send_payables_within_batch_components_are_used_together_properly");
    //     let probe_message = PendingPayableFingerprintSeeds {
    //         batch_wide_timestamp: SystemTime::now(),
    //         hashes_and_balances: vec![],
    //     };
    //     recipient.try_send(probe_message).unwrap();
    //     System::current().stop();
    //     system.run();
    //     let accountant_recording = accountant_recording_arc.lock().unwrap();
    //     assert_eq!(accountant_recording.len(), 1)
    // }

    // TODO: GH-744: This had batch payable tools, come back to this later
    // #[test]
    // fn gas_limit_for_polygon_mainnet_lies_within_limits_for_raw_transaction() {
    //     test_gas_limit_is_between_limits(Chain::PolyMainnet);
    // }
    // TODO: GH-744: This had batch payable tools, come back to this later
    // #[test]
    // fn gas_limit_for_eth_mainnet_lies_within_limits_for_raw_transaction() {
    //     test_gas_limit_is_between_limits(Chain::EthMainnet)
    // }
    // TODO: GH-744: This had batch payable tools, come back to this later
    // fn test_gas_limit_is_between_limits(chain: Chain) {
    //     let sign_transaction_params_arc = Arc::new(Mutex::new(vec![]));
    //     let transport = TestTransport::default();
    //     let mut subject =
    //         BlockchainInterfaceWeb3::new(transport, make_fake_event_loop_handle(), chain);
    //     let not_under_this_value =
    //         BlockchainInterfaceWeb3::<Http>::web3_gas_limit_const_part(chain);
    //     let not_above_this_value = not_under_this_value + WEB3_MAXIMAL_GAS_LIMIT_MARGIN;
    //     let consuming_wallet_secret_raw_bytes = b"my-wallet";
    //     let batch_payable_tools = BatchPayableToolsMock::<TestTransport>::default()
    //         .sign_transaction_params(&sign_transaction_params_arc)
    //         .sign_transaction_result(Ok(make_default_signed_transaction()));
    //     subject.batch_payable_tools = Box::new(batch_payable_tools);
    //     let consuming_wallet = make_paying_wallet(consuming_wallet_secret_raw_bytes);
    //     let gas_price = 123;
    //     let nonce = U256::from(5);
    //
    //     let _ = subject.sign_transaction(
    //         &make_wallet("wallet1"),
    //         &consuming_wallet,
    //         1_000_000_000,
    //         nonce,
    //         gas_price,
    //     );
    //
    //     let mut sign_transaction_params = sign_transaction_params_arc.lock().unwrap();
    //     let (transaction_params, _, secret) = sign_transaction_params.remove(0);
    //     assert!(sign_transaction_params.is_empty());
    //     assert!(
    //         transaction_params.gas >= U256::from(not_under_this_value),
    //         "actual gas limit {} isn't above or equal {}",
    //         transaction_params.gas,
    //         not_under_this_value
    //     );
    //     assert!(
    //         transaction_params.gas <= U256::from(not_above_this_value),
    //         "actual gas limit {} isn't below or equal {}",
    //         transaction_params.gas,
    //         not_above_this_value
    //     );
    //     assert_eq!(
    //         secret,
    //         (&Bip32EncryptionKeyProvider::from_raw_secret(
    //             &consuming_wallet_secret_raw_bytes.keccak256()
    //         )
    //         .unwrap())
    //             .into()
    //     );
    // }

    const TEST_PAYMENT_AMOUNT: u128 = 1_000_000_000_000;
    const TEST_GAS_PRICE_ETH: u64 = 110;
    const TEST_GAS_PRICE_POLYGON: u64 = 50;

    #[test]
    fn web3_gas_limit_const_part_returns_reasonable_values() {
        type Subject = BlockchainInterfaceWeb3;
        assert_eq!(
            Subject::web3_gas_limit_const_part(Chain::EthMainnet),
            55_000
        );
        assert_eq!(
            Subject::web3_gas_limit_const_part(Chain::EthRopsten),
            55_000
        );
        assert_eq!(
            Subject::web3_gas_limit_const_part(Chain::PolyMainnet),
            70_000
        );
        assert_eq!(
            Subject::web3_gas_limit_const_part(Chain::PolyMumbai),
            70_000
        );
        assert_eq!(Subject::web3_gas_limit_const_part(Chain::Dev), 55_000);
    }

    //an adapted test from old times when we had our own signing method
    //I don't have data for the new chains so I omit them in this kind of tests
    #[test]
    fn signs_various_transactions_for_eth_mainnet() {
        let signatures = &[
            &[
                248, 108, 9, 133, 4, 168, 23, 200, 0, 130, 82, 8, 148, 53, 53, 53, 53, 53, 53, 53,
                53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 136, 13, 224, 182, 179, 167,
                100, 0, 0, 128, 37, 160, 40, 239, 97, 52, 11, 217, 57, 188, 33, 149, 254, 83, 117,
                103, 134, 96, 3, 225, 161, 93, 60, 113, 255, 99, 225, 89, 6, 32, 170, 99, 98, 118,
                160, 103, 203, 233, 216, 153, 127, 118, 26, 236, 183, 3, 48, 75, 56, 0, 204, 245,
                85, 201, 243, 220, 100, 33, 75, 41, 127, 177, 150, 106, 59, 109, 131,
            ][..],
            &[
                248, 106, 128, 134, 213, 86, 152, 55, 36, 49, 131, 30, 132, 128, 148, 240, 16, 159,
                200, 223, 40, 48, 39, 182, 40, 92, 200, 137, 245, 170, 98, 78, 172, 31, 85, 132,
                59, 154, 202, 0, 128, 37, 160, 9, 235, 182, 202, 5, 122, 5, 53, 214, 24, 100, 98,
                188, 11, 70, 91, 86, 28, 148, 162, 149, 189, 176, 98, 31, 193, 146, 8, 171, 20,
                154, 156, 160, 68, 15, 253, 119, 92, 233, 26, 131, 58, 180, 16, 119, 114, 4, 213,
                52, 26, 111, 159, 169, 18, 22, 166, 243, 238, 44, 5, 31, 234, 106, 4, 40,
            ][..],
            &[
                248, 117, 128, 134, 9, 24, 78, 114, 160, 0, 130, 39, 16, 128, 128, 164, 127, 116,
                101, 115, 116, 50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 96, 0, 87, 38, 160, 122, 155, 12, 58, 133, 108, 183, 145, 181,
                210, 141, 44, 236, 17, 96, 40, 55, 87, 204, 250, 142, 83, 122, 168, 250, 5, 113,
                172, 203, 5, 12, 181, 160, 9, 100, 95, 141, 167, 178, 53, 101, 115, 131, 83, 172,
                199, 242, 208, 96, 246, 121, 25, 18, 211, 89, 60, 94, 165, 169, 71, 3, 176, 157,
                167, 50,
            ][..],
        ];
        assert_signature(Chain::EthMainnet, signatures)
    }

    //an adapted test from old times when we had our own signing method
    //I don't have data for the new chains so I omit them in this kind of tests
    #[test]
    fn signs_various_transactions_for_ropsten() {
        let signatures = &[
            &[
                248, 108, 9, 133, 4, 168, 23, 200, 0, 130, 82, 8, 148, 53, 53, 53, 53, 53, 53, 53,
                53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 136, 13, 224, 182, 179, 167,
                100, 0, 0, 128, 41, 160, 8, 220, 80, 201, 100, 41, 178, 35, 151, 227, 210, 85, 27,
                41, 27, 82, 217, 176, 64, 92, 205, 10, 195, 169, 66, 91, 213, 199, 124, 52, 3, 192,
                160, 94, 220, 102, 179, 128, 78, 150, 78, 230, 117, 10, 10, 32, 108, 241, 50, 19,
                148, 198, 6, 147, 110, 175, 70, 157, 72, 31, 216, 193, 229, 151, 115,
            ][..],
            &[
                248, 106, 128, 134, 213, 86, 152, 55, 36, 49, 131, 30, 132, 128, 148, 240, 16, 159,
                200, 223, 40, 48, 39, 182, 40, 92, 200, 137, 245, 170, 98, 78, 172, 31, 85, 132,
                59, 154, 202, 0, 128, 41, 160, 186, 65, 161, 205, 173, 93, 185, 43, 220, 161, 63,
                65, 19, 229, 65, 186, 247, 197, 132, 141, 184, 196, 6, 117, 225, 181, 8, 81, 198,
                102, 150, 198, 160, 112, 126, 42, 201, 234, 236, 168, 183, 30, 214, 145, 115, 201,
                45, 191, 46, 3, 113, 53, 80, 203, 164, 210, 112, 42, 182, 136, 223, 125, 232, 21,
                205,
            ][..],
            &[
                248, 117, 128, 134, 9, 24, 78, 114, 160, 0, 130, 39, 16, 128, 128, 164, 127, 116,
                101, 115, 116, 50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 96, 0, 87, 41, 160, 146, 204, 57, 32, 218, 236, 59, 94, 106, 72,
                174, 211, 223, 160, 122, 186, 126, 44, 200, 41, 222, 117, 117, 177, 189, 78, 203,
                8, 172, 155, 219, 66, 160, 83, 82, 37, 6, 243, 61, 188, 102, 176, 132, 102, 74,
                111, 180, 105, 33, 122, 106, 109, 73, 180, 65, 10, 117, 175, 190, 19, 196, 17, 128,
                193, 75,
            ][..],
        ];
        assert_signature(Chain::EthRopsten, signatures)
    }

    #[derive(Deserialize)]
    struct Signing {
        signed: Vec<u8>,
        private_key: H256,
    }

    fn assert_signature(chain: Chain, slice_of_slices: &[&[u8]]) {
        let first_part_tx_1 = r#"[{"nonce": "0x9", "gasPrice": "0x4a817c800", "gasLimit": "0x5208", "to": "0x3535353535353535353535353535353535353535", "value": "0xde0b6b3a7640000", "data": []}, {"private_key": "0x4646464646464646464646464646464646464646464646464646464646464646", "signed": "#;
        let first_part_tx_2 = r#"[{"nonce": "0x0", "gasPrice": "0xd55698372431", "gasLimit": "0x1e8480", "to": "0xF0109fC8DF283027b6285cc889F5aA624EaC1F55", "value": "0x3b9aca00", "data": []}, {"private_key": "0x4c0883a69102937d6231471b5dbb6204fe5129617082792ae468d01a3f362318", "signed": "#;
        let first_part_tx_3 = r#"[{"nonce": "0x00", "gasPrice": "0x09184e72a000", "gasLimit": "0x2710", "to": null, "value": "0x00", "data": [127,116,101,115,116,50,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,96,0,87]}, {"private_key": "0xe331b6d69882b4cb4ea581d88e0b604039a3de5967688d3dcffdd2270c0fd109", "signed": "#;
        fn compose(first_part: &str, slice: &[u8]) -> String {
            let third_part_jrc = "}]";
            format!("{}{:?}{}", first_part, slice, third_part_jrc)
        }
        let all_transactions = format!(
            "[{}]",
            vec![first_part_tx_1, first_part_tx_2, first_part_tx_3]
                .iter()
                .zip(slice_of_slices.iter())
                .zip(0usize..2)
                .fold(String::new(), |so_far, actual| [
                    so_far,
                    compose(actual.0 .0, actual.0 .1)
                ]
                .join(if actual.1 == 0 { "" } else { ", " }))
        );
        let txs: Vec<(TestRawTransaction, Signing)> =
            serde_json::from_str(&all_transactions).unwrap();
        let constant_parts = &[
            &[
                248u8, 108, 9, 133, 4, 168, 23, 200, 0, 130, 82, 8, 148, 53, 53, 53, 53, 53, 53,
                53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 53, 136, 13, 224, 182, 179,
                167, 100, 0, 0, 128,
            ][..],
            &[
                248, 106, 128, 134, 213, 86, 152, 55, 36, 49, 131, 30, 132, 128, 148, 240, 16, 159,
                200, 223, 40, 48, 39, 182, 40, 92, 200, 137, 245, 170, 98, 78, 172, 31, 85, 132,
                59, 154, 202, 0, 128,
            ][..],
            &[
                248, 117, 128, 134, 9, 24, 78, 114, 160, 0, 130, 39, 16, 128, 128, 164, 127, 116,
                101, 115, 116, 50, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0, 0, 96, 0, 87,
            ][..],
        ];

        let subject = make_blockchain_interface_web3(None);
        let lengths_of_constant_parts: Vec<usize> =
            constant_parts.iter().map(|part| part.len()).collect();
        for (((tx, signed), length), constant_part) in txs
            .iter()
            .zip(lengths_of_constant_parts)
            .zip(constant_parts)
        {
            let secret = Wallet::from(
                Bip32EncryptionKeyProvider::from_raw_secret(&signed.private_key.0.as_ref())
                    .unwrap(),
            )
            .prepare_secp256k1_secret()
            .unwrap();
            let tx_params = from_raw_transaction_to_transaction_parameters(tx, chain);
            let sign = subject
                .get_web3()
                .accounts()
                .sign_transaction(tx_params, &secret)
                .wait()
                .unwrap();
            let signed_data_bytes = sign.raw_transaction.0;
            assert_eq!(signed_data_bytes, signed.signed);
            assert_eq!(signed_data_bytes[..length], **constant_part)
        }
    }

    fn from_raw_transaction_to_transaction_parameters(
        raw_transaction: &TestRawTransaction,
        chain: Chain,
    ) -> TransactionParameters {
        TransactionParameters {
            nonce: Some(raw_transaction.nonce),
            to: raw_transaction.to,
            gas: raw_transaction.gas_limit,
            gas_price: Some(raw_transaction.gas_price),
            value: raw_transaction.value,
            data: Bytes(raw_transaction.data.clone()),
            chain_id: Some(chain.rec().num_chain_id),
        }
    }

    // TODO: GH-744 - This test was removed in master
    // #[test]
    // fn blockchain_interface_web3_can_fetch_nonce() {
    //     let prepare_params_arc = Arc::new(Mutex::new(vec![]));
    //     let send_params_arc = Arc::new(Mutex::new(vec![]));
    //     let transport = TestTransport::default()
    //         .prepare_params(&prepare_params_arc)
    //         .send_params(&send_params_arc)
    //         .send_result(json!(
    //             "0x0000000000000000000000000000000000000000000000000000000000000001"
    //         ));
    //     let subject = BlockchainInterfaceWeb3::new(
    //         transport.clone(),
    //         make_fake_event_loop_handle(),
    //         TEST_DEFAULT_CHAIN,
    //     );
    //
    //     let result = subject
    //         .get_transaction_count(&make_paying_wallet(b"gdasgsa"))
    //         .wait();
    //
    //     assert_eq!(result, Ok(U256::from(1)));
    //     let mut prepare_params = prepare_params_arc.lock().unwrap();
    //     let (method_name, actual_arguments) = prepare_params.remove(0);
    //     assert!(prepare_params.is_empty());
    //     let actual_arguments: Vec<String> = actual_arguments
    //         .into_iter()
    //         .map(|arg| serde_json::to_string(&arg).unwrap())
    //         .collect();
    //     assert_eq!(method_name, "eth_getTransactionCount".to_string());
    //     assert_eq!(
    //         actual_arguments,
    //         vec![
    //             String::from(r#""0x5c361ba8d82fcf0e5538b2a823e9d457a2296725""#),
    //             String::from(r#""pending""#),
    //         ]
    //     );
    //     let send_params = send_params_arc.lock().unwrap();
    //     let rpc_call_params = vec![
    //         Value::String(String::from("0x5c361ba8d82fcf0e5538b2a823e9d457a2296725")),
    //         Value::String(String::from("pending")),
    //     ];
    //     let expected_request =
    //         web3::helpers::build_request(1, "eth_getTransactionCount", rpc_call_params);
    //     assert_eq!(*send_params, vec![(1, expected_request)])
    // }

    #[test]
    fn blockchain_interface_web3_can_fetch_transaction_receipt() {
        let port = find_free_port();
        let _test_server = TestServer::start (port, vec![
            br#"{"jsonrpc":"2.0","id":2,"result":{"transactionHash":"0xa128f9ca1e705cc20a936a24a7fa1df73bad6e0aaf58e8e6ffcc154a7cff6e0e","blockHash":"0x6d0abccae617442c26104c2bc63d1bc05e1e002e555aec4ab62a46e826b18f18","blockNumber":"0xb0328d","contractAddress":null,"cumulativeGasUsed":"0x60ef","effectiveGasPrice":"0x22ecb25c00","from":"0x7424d05b59647119b01ff81e2d3987b6c358bf9c","gasUsed":"0x60ef","logs":[],"logsBloom":"0x00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000","status":"0x0","to":"0x384dec25e03f94931767ce4c3556168468ba24c3","transactionIndex":"0x0","type":"0x0"}}"#
                .to_vec()
        ]);
        // let (event_loop_handle, transport) = Http::with_max_parallel(
        //     &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
        //     REQUESTS_IN_PARALLEL,
        // )
        // .unwrap();
        // let chain = TEST_DEFAULT_CHAIN;
        let subject = make_blockchain_interface_web3(Some(port));
        let tx_hash =
            H256::from_str("a128f9ca1e705cc20a936a24a7fa1df73bad6e0aaf58e8e6ffcc154a7cff6e0e")
                .unwrap();

        let result = subject.get_transaction_receipt(tx_hash);

        let expected_receipt = TransactionReceipt{
            transaction_hash: tx_hash,
            transaction_index: Default::default(),
            block_hash: Some(H256::from_str("6d0abccae617442c26104c2bc63d1bc05e1e002e555aec4ab62a46e826b18f18").unwrap()),
            block_number:Some(U64::from_str("b0328d").unwrap()),
            cumulative_gas_used: U256::from_str("60ef").unwrap(),
            gas_used: Some(U256::from_str("60ef").unwrap()),
            contract_address: None,
            logs: vec![],
            status: Some(U64::from(0)),
            root: None,
            logs_bloom: H2048::from_str("00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000000000000000000000000000000080000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000800000000000000000000000000000000000").unwrap()
        };
        assert_eq!(result, Ok(Some(expected_receipt)));
    }

    #[test]
    fn get_transaction_receipt_handles_errors() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let chain = TEST_DEFAULT_CHAIN;
        let subject = BlockchainInterfaceWeb3::new(transport, event_loop_handle, chain);
        let tx_hash = make_tx_hash(4564546);

        let actual_error = subject.get_transaction_receipt(tx_hash).unwrap_err();
        let error_message = if let BlockchainError::QueryFailed(em) = actual_error {
            em
        } else {
            panic!("Expected BlockchainError::QueryFailed(msg)");
        };
        assert_string_contains(
            error_message.as_str(),
            "Transport error: Error(Connect, Os { code: ",
        );
        assert_string_contains(
            error_message.as_str(),
            ", kind: ConnectionRefused, message: ",
        );
    }

    fn make_initialized_agent(
        gas_price_gwei: u64,
        consuming_wallet: Wallet,
        nonce: U256,
    ) -> Box<dyn BlockchainAgent> {
        Box::new(
            BlockchainAgentMock::default()
                .consuming_wallet_result(consuming_wallet)
                .agreed_fee_per_computation_unit_result(gas_price_gwei)
                .pending_transaction_id_result(nonce),
        )
    }

    #[test]
    fn hash_the_smart_contract_transfer_function_signature() {
        assert_eq!(
            "transfer(address,uint256)".keccak256()[0..4],
            TRANSFER_METHOD_ID,
        );
    }
}
