use std::collections::HashMap;
use crate::accountant::db_access_objects::payable_dao::PayableAccount;
use crate::accountant::db_access_objects::pending_payable_dao::PendingPayable;
use crate::blockchain::blockchain_bridge::PendingPayableFingerprintSeeds;
use crate::blockchain::blockchain_interface::blockchain_interface_web3::{
    to_wei, BlockchainInterfaceWeb3, HashAndAmount,
    REQUESTS_IN_PARALLEL, TRANSFER_METHOD_ID,
};

use crate::blockchain::blockchain_interface::data_structures::errors::{BlockchainError, PayableTransactionError};
use crate::blockchain::blockchain_interface::BlockchainInterface;
use crate::sub_lib::wallet::Wallet;
use actix::Recipient;
use ethereum_types::U64;
use futures::future::err;
use futures::stream::FuturesOrdered;
use futures::{Future, Stream};
use http::uri::Port;
use masq_lib::blockchains::chains::Chain;
use masq_lib::logger::Logger;
use masq_lib::test_utils::utils::TEST_DEFAULT_CHAIN;
use serde_json::Value;
use std::iter::once;
use std::net::Ipv4Addr;
use std::thread;
use std::time::{Duration, SystemTime};
use thousands::Separable;
use web3::transports::{Batch, Http};
use web3::types::{
    Address, BlockNumber, Bytes, SignedTransaction, TransactionParameters, H256, U256,
};
use web3::{BatchTransport, Transport, Web3};
use web3::{Error as Web3Error, Error};
use web3::api::{Accounts, Namespace};
use web3::contract::{Contract, Options};
use crate::blockchain::blockchain_interface::data_structures::{ProcessedPayableFallible, RpcPayableFailure};
use crate::blockchain::blockchain_interface::data_structures::errors::BlockchainError::QueryFailed;

fn base_gas_limit(chain: Chain) -> u64 {
    //TODO: GH-744: There is a duplicated function web3_gas_limit_const_part
    match chain {
        Chain::EthMainnet | Chain::EthRopsten | Chain::Dev => 55_000,
        Chain::PolyMainnet | Chain::PolyMumbai => 70_000,
    }
}

// fn web3_gas_limit_const_part(chain: Chain) -> u64 {
//     match chain {
//         Chain::EthMainnet | Chain::EthRopsten | Chain::Dev => 55_000,
//         Chain::PolyMainnet | Chain::PolyMumbai => 70_000,
//     }
// }

pub fn advance_used_nonce(current_nonce: U256) -> U256 {
    current_nonce
        .checked_add(U256::one())
        .expect("unexpected limits")
}

fn error_with_hashes(
    error: Web3Error,
    hashes_and_paid_amounts: Vec<HashAndAmount>,
) -> PayableTransactionError {
    let hashes = hashes_and_paid_amounts
        .into_iter()
        .map(|hash_and_amount| hash_and_amount.hash)
        .collect();
    PayableTransactionError::Sending {
        msg: error.to_string(),
        hashes,
    }
}

pub fn merged_output_data(
    responses: Vec<web3::transports::Result<Value>>,
    hashes_and_paid_amounts: Vec<HashAndAmount>,
    accounts: Vec<PayableAccount>,
) -> Vec<ProcessedPayableFallible> {
    // TODO: GH-744 Hashes and paid amounts are now out of sync with accounts. Need to look into this.
    let iterator_with_all_data = responses
        .into_iter()
        .zip(hashes_and_paid_amounts.into_iter())
        .zip(accounts.iter());
    iterator_with_all_data
        .map(
            |((rpc_result, hash_and_amount), account)| match rpc_result {
                Ok(_rpc_result) => {
                    // TODO: This rpc_result should be validated
                    return ProcessedPayableFallible::Correct(PendingPayable {
                        recipient_wallet: account.wallet.clone(),
                        hash: hash_and_amount.hash,
                    })
                } ,
                Err(rpc_error) => ProcessedPayableFallible::Failed(RpcPayableFailure {
                    rpc_error,
                    recipient_wallet: account.wallet.clone(),
                    hash: hash_and_amount.hash,
                }),
            },
        )
        .collect()
}

pub fn transmission_log(chain: Chain, accounts: &[PayableAccount], gas_price: u64) -> String {
    let chain_name = chain
        .rec()
        .literal_identifier
        .chars()
        .skip_while(|char| char != &'-')
        .skip(1)
        .collect::<String>();
    let introduction = once(format!(
        "\
        Paying to creditors...\n\
        Transactions in the batch:\n\
        \n\
        gas price:                                   {} gwei\n\
        chain:                                       {}\n\
        \n\
        [wallet address]                             [payment in wei]\n",
        gas_price, chain_name
    ));
    let body = accounts.iter().map(|account| {
        format!(
            "{}   {}\n",
            account.wallet,
            account.balance_wei.separate_with_commas()
        )
    });
    introduction.chain(body).collect()
}

pub fn sign_transaction_data(amount: u128, recipient_wallet: Wallet) -> [u8; 68] {
    let mut data = [0u8; 4 + 32 + 32];
    data[0..4].copy_from_slice(&TRANSFER_METHOD_ID);
    data[16..36].copy_from_slice(&recipient_wallet.address().0[..]);
    U256::from(amount).to_big_endian(&mut data[36..68]);
    return data;
}
pub fn gas_limit(data: [u8; 68], chain: Chain) -> U256 {
    let base_gas_limit = base_gas_limit(chain);
    let gas_limit = ethereum_types::U256::try_from(data.iter().fold(base_gas_limit, |acc, v| {
        acc + if v == &0u8 { 4 } else { 68 }
    }))
    .expect("Internal error");
    return gas_limit;
}
pub fn sign_transaction(
    chain: Chain,
    web3_batch: Web3<Batch<Http>>,
    recipient_wallet: Wallet,
    consuming_wallet: Wallet,
    amount: u128,
    nonce: U256,
    gas_price_in_gwei: u64,
) -> SignedTransaction {
    let data = sign_transaction_data(amount, recipient_wallet);
    let gas_limit = gas_limit(data, chain);
    let gas_price_in_wei = to_wei(gas_price_in_gwei);
    // If you flip gas_price or nonce to None this function will start making RPC calls (Do it at your own risk).
    let transaction_parameters = TransactionParameters {
        nonce: Some(nonce),
        to: Some(chain.rec().contract),
        gas: gas_limit,
        gas_price: Some(gas_price_in_wei),
        value: ethereum_types::U256::zero(),
        data: Bytes(data.to_vec()),
        chain_id: Some(chain.rec().num_chain_id),
    };
    let key = consuming_wallet.prepare_secp256k1_secret().expect("Consuming wallet doesnt contain a secret key"); // TODO: GH-744: need a test for this
    // This wait call doesnt actually make any RPC call and signing is done locally.
    let sign_transaction_result = web3_batch
        .accounts()
        .sign_transaction(transaction_parameters, &key).wait();

    match sign_transaction_result {
        Ok(signed_transaction) => { signed_transaction }
        Err(error) => {
            panic!("Signing should be done locally: {:?}", error); // TODO: GH-744: need a test for this
        }
    }
}

// TODO: GH-744
pub fn sign_transaction_2(
    chain: Chain,
    batch_web3: Web3<Batch<Http>>,
    recipient_wallet: Wallet,
    consuming_wallet: Wallet,
    amount: u128,
) -> Box<dyn Future<Item = SignedTransaction, Error = PayableTransactionError>> {
    let data = sign_transaction_data(amount, recipient_wallet);
    let gas_limit = gas_limit(data, chain);
    // let gas_price_in_wei = to_wei(gas_price_in_gwei);

    let transaction_parameters = TransactionParameters {
        nonce: None, // TODO: GH-744 Change this to None and let the BlockChain figure out the correct Nonce instead.
        to: Some(chain.rec().contract),
        gas: gas_limit,
        gas_price: None, // TODO: GH-744 Talk about this.
        value: ethereum_types::U256::zero(),
        data: Bytes(data.to_vec()),
        chain_id: Some(chain.rec().num_chain_id),
    };

    let key = match consuming_wallet.prepare_secp256k1_secret() {
        Ok(secret) => secret,
        Err(e) => return Box::new(err(PayableTransactionError::UnusableWallet(e.to_string()))),
    };

    // let signed_transaction = batch_web3.accounts().sign_transaction(transaction_parameters, &key).wait();
    // let result = batch_web3.eth().transport().submit_batch().wait();
    // eprintln!("signed_transaction: {:?}", signed_transaction);
    panic!("STOP ");

    // let sign_transaction = batch_web3.accounts().sign_transaction(transaction_parameters, &key);
    //
    // Box::new(
    //     batch_web3.eth().transport().submit_batch().then(move || {
    //         sign_transaction.map_err(|e|  PayableTransactionError::Signing(e.to_string()) ).then(|signed_transaction| signed_transaction )
    //     }),
    // )
}

pub fn handle_new_transaction(
    chain: Chain,
    web3_batch: Web3<Batch<Http>>,
    recipient_wallet: Wallet,
    consuming_wallet: Wallet,
    amount: u128,
    nonce: U256,
    gas_price: u64,
) -> H256 {
    let signed_tx = sign_transaction(
        chain,
        web3_batch.clone(),
        recipient_wallet.clone(),
        consuming_wallet.clone(),
        amount,
        nonce,
        gas_price,
    );
    append_signed_transaction_to_batch(web3_batch, signed_tx.raw_transaction);
    signed_tx.transaction_hash
}

// TODO GH-744:
pub fn sign_and_send_payment_2(
    chain: Chain,
    batch_web3: Web3<Batch<Http>>,
    recipient_wallet: Wallet,
    consuming_wallet: Wallet,
    amount: u128,
) -> Box<dyn Future<Item = H256, Error = PayableTransactionError>> {
    eprintln!("Before sign_transaction_2");
    Box::new(
        sign_transaction_2(
            chain,
            batch_web3.clone(),
            recipient_wallet.clone(),
            consuming_wallet.clone(),
            amount,
        )
            .map_err(|e| { eprintln!("sign_transaction_2 - ERROR");  e  } )
            .and_then(move |signed_tx| {
                eprintln!("Before send_transaction_2");
                // panic!("STOP HERE");
                send_transaction_2(batch_web3, signed_tx.raw_transaction)
            }),
    )
}


pub fn send_transaction_not_working(batch_web3: Web3<Batch<Http>>, raw_transaction: Bytes) -> Box<dyn Future<Item = H256, Error = PayableTransactionError>> {
    Box::new(
        batch_web3
            .eth()
            .send_raw_transaction(raw_transaction)
            .then(move |result| {
                match result {
                    Ok(result) => {
                        Ok(result)
                    }
                    Err(e) => {
                        Err(PayableTransactionError::Sending { msg: e.to_string(), hashes: vec![] })
                    }
                }
            })
    )
}

pub fn append_signed_transaction_to_batch(web3_batch: Web3<Batch<Http>>, raw_transaction: Bytes) {
    // This function only prepares a raw transaction for a batch call and doesn't actually send it right here.
    web3_batch.eth().send_raw_transaction(raw_transaction);
}

// TODO: GH-744
pub fn send_transaction_2(batch_web3: Web3<Batch<Http>>, raw_transaction: Bytes) -> Box<dyn Future<Item = H256, Error = PayableTransactionError>> {
    // batch_web3.eth().send_raw_transaction(raw_transaction);
    let result = batch_web3
        .eth()
        .send_raw_transaction(raw_transaction);

    eprintln!("send_transaction_2 Before BOX.");

    Box::new(
        result
            .then(move |result| {
                eprintln!("Send_raw_transaction result: {:?}", result);
                match result {
                    Ok(result) => {
                        Ok(result)
                    }
                    Err(e) => {
                        Err(PayableTransactionError::Sending { msg: e.to_string(), hashes: vec![] })
                    }
                }
            })
    )
}

// TODO: GH-744 Rename and refactor this function after merging with Master
pub fn sign_and_append_payment(
    chain: Chain,
    web3_batch: Web3<Batch<Http>>,
    consuming_wallet: Wallet,
    nonce: U256,
    gas_price: u64,
    account: PayableAccount,
) -> HashAndAmount {
    let hash= handle_new_transaction(
        chain,
        web3_batch,
        account.wallet.clone(),
        consuming_wallet,
        account.balance_wei,
        nonce,
        gas_price,
    );
    HashAndAmount {
        hash,
        amount: account.balance_wei,
    }
}


// TODO: GH-744
pub fn send_and_append_payment_2(
    chain: Chain,
    batch_web3: Web3<Batch<Http>>,
    consuming_wallet: Wallet,
    account: PayableAccount,
) -> Box<dyn Future<Item = HashAndAmount, Error = PayableTransactionError> + 'static> {
    Box::new(
        sign_and_send_payment_2(
            chain,
            batch_web3,
            account.wallet.clone(),
            consuming_wallet,
            account.balance_wei,
        )
            .map_err(|e| {
                return e;
            })
            .and_then(move |new_hash| {
                Ok(HashAndAmount {
                    hash: new_hash,
                    amount: account.balance_wei,
                })
            }),
    )
}

pub fn sign_and_append_multiple_payments(
    logger: Logger,
    chain: Chain,
    web3_batch: Web3<Batch<Http>>,
    consuming_wallet: Wallet,
    gas_price: u64,
    mut pending_nonce: U256,
    accounts: Vec<PayableAccount>,
) -> Vec<HashAndAmount> {
    let mut hash_and_amount_list = vec![];
    accounts.into_iter().for_each(|payable| {
        debug!(
            logger,
            "Preparing payable future of {} wei to {} with nonce {}",
            payable.balance_wei.separate_with_commas(),
            payable.wallet,
            pending_nonce
        );

        let hash_and_amount= sign_and_append_payment(
            chain,
            web3_batch.clone(),
            consuming_wallet.clone(),
            pending_nonce,
            gas_price,
            payable,
        );

        pending_nonce = advance_used_nonce(pending_nonce);
        hash_and_amount_list.push(hash_and_amount);
    });
    hash_and_amount_list
}

pub fn send_payables_within_batch(
    logger: Logger,
    chain: Chain,
    web3_batch: Web3<Batch<Http>>,
    consuming_wallet: Wallet,
    gas_price: u64,
    pending_nonce: U256,
    new_fingerprints_recipient: Recipient<PendingPayableFingerprintSeeds>,
    accounts: Vec<PayableAccount>,
) -> Box<dyn Future<Item = Vec<ProcessedPayableFallible>, Error = PayableTransactionError> + 'static> {
    debug!(
            logger,
            "Common attributes of payables to be transacted: sender wallet: {}, contract: {:?}, chain_id: {}, gas_price: {}",
            consuming_wallet,
            chain.rec().contract,
            chain.rec().num_chain_id,
            gas_price
        );

    let hashes_and_paid_amounts = sign_and_append_multiple_payments(
        logger.clone(),
        chain,
        web3_batch.clone(),
        consuming_wallet,
        gas_price,
        pending_nonce,
        accounts.clone(),
    );

    let timestamp = SystemTime::now();
    let hashes_and_paid_amounts_error = hashes_and_paid_amounts.clone();
    let hashes_and_paid_amounts_ok = hashes_and_paid_amounts.clone();

    // TODO: We are sending hashes_and_paid_amounts to the Accountant even if the payments fail.
    new_fingerprints_recipient
        .try_send(PendingPayableFingerprintSeeds {
            batch_wide_timestamp: timestamp,
            hashes_and_balances: hashes_and_paid_amounts,
        })
        .expect("Accountant is dead");

    info!(logger, "{}", transmission_log(chain, &accounts, gas_price));

    return Box::new(
        web3_batch
            .transport()
            .submit_batch()
            .map_err(|e| {
                error_with_hashes(e, hashes_and_paid_amounts_error)
            })
            .and_then(move |batch_response| {
                Ok(merged_output_data(
                    batch_response,
                    hashes_and_paid_amounts_ok,
                    accounts,
                ))
            })
    )
}

pub fn request_block_number(
    web3: Web3<Http>,
    start_block: BlockNumber,
    end_block: BlockNumber,
    logger: Logger,
) -> Box<dyn Future<Item = u64, Error = ()>> {
    // TODO: GH-744: Move fallback code outside of this function, we dont want a panic inside a Future
    let fallback_start_block_number = match end_block {
        BlockNumber::Number(eb) => eb.as_u64(),
        _ => {
            if let BlockNumber::Number(start_block_number) = start_block {
                start_block_number.as_u64() + 1u64
            } else {
                panic!("start_block of Latest, Earliest, and Pending are not supported");
            }
        }
    };

    let block_request = web3.eth().block_number();

    Box::new(
        block_request.then(move |block_nbr_result| match block_nbr_result {
            Ok(block_nbr) => {
                debug!(logger, "Latest block number: {}", block_nbr.as_u64());
                Ok(block_nbr.as_u64())
            }
            Err(err) => {
                debug!(
                    logger,
                    "Using fallback block number: {}", fallback_start_block_number
                );
                Ok(fallback_start_block_number)
            }
        }),
    )
}

pub fn get_transaction_fee_balance(
    web3: Web3<Http>,
    address: Address,
) -> Box<dyn Future<Item = U256, Error = BlockchainError>> {
    // TODO: GH-744: We could accept an wallet address instead of the wallet so this function wouldn't panic.
    Box::new(
        web3.eth()
            .balance(address, None)
            .map_err(|e| QueryFailed(e.to_string())),
    )
}

pub fn get_token_balance(
    contract: Contract<Http>,
    address: Address,
) -> Box<dyn Future<Item = U256, Error = BlockchainError>> {
    Box::new(
        contract.query(
            "balanceOf",
            address,
            None,
            Options::default(),
            None,
            )
            .map_err(|e| BlockchainError::QueryFailed(e.to_string())),
    )
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    // use crate::accountant::db_access_objects::dao_utils::from_time_t;
    use crate::accountant::gwei_to_wei;
    use crate::accountant::test_utils::{make_payable_account, make_payable_account_with_wallet_and_balance_and_timestamp_opt};
    use crate::blockchain::bip32::Bip32EncryptionKeyProvider;
    use crate::blockchain::test_utils::{make_blockchain_interface, make_tx_hash, TestTransport};
    use crate::sub_lib::wallet::Wallet;
    use crate::test_utils::make_paying_wallet;
    use crate::test_utils::make_wallet;
    use crate::test_utils::recorder::{make_recorder, Recorder};
    use crate::test_utils::unshared_test_utils::decode_hex;
    use actix::{Actor, FinishStream, System};
    use jsonrpc_core::Version::V2;
    use jsonrpc_core::{Call, Error, ErrorCode, Id, MethodCall, Params};
    use masq_lib::constants::{DEFAULT_CHAIN, DEFAULT_GAS_PRICE};
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use masq_lib::test_utils::utils::TEST_DEFAULT_CHAIN;
    use serde_json::json;
    use serde_json::Value;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, SystemTime};
    use jsonrpc_core::ErrorCode::ServerError;
    use web3::api::Namespace;
    use web3::Error as Web3Error;
    use web3::Error::{Rpc, Unreachable};
    use web3::types::{CallRequest, TransactionRequest};
    use masq_lib::blockchains::chains::Chain::EthMainnet;
    use masq_lib::test_utils::mock_blockchain_client_server::MBCSBuilder;
    use masq_lib::utils::find_free_port;
    use crate::accountant::db_access_objects::utils::from_time_t;
    use crate::blockchain::blockchain_interface::blockchain_interface_web3::{BlockchainInterfaceWeb3, REQUESTS_IN_PARALLEL, ResultForBalance};
    use crate::blockchain::blockchain_interface::BlockchainInterface;
    use crate::blockchain::blockchain_interface::data_structures::errors::PayableTransactionError::Sending;
    use crate::blockchain::blockchain_interface::data_structures::ProcessedPayableFallible::{Correct, Failed};
    use crate::blockchain::blockchain_interface::data_structures::RetrievedBlockchainTransactions;
    use crate::test_utils::http_test_server::TestServer;

    #[test]
    fn tmp_test_web3_submit_batch() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            "https://mainnet.infura.io/v3/0ead23143b174f6983c76f69ddcf4026",
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let web3_batch = subject.get_web3_batch();
        // let web3  = subject.get_web3();

        web3_batch.eth().block_number();
        web3_batch.eth().block_number();
        web3_batch.eth().block_number();
        web3_batch.eth().block_number();

        let result =  web3_batch.eth().transport().submit_batch().wait().expect("TODO: panic message");
        eprintln!("result {:?}", result);
    }

    #[test]
    fn tmp_send_transactions_works() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            "https://mainnet.infura.io/v3/0ead23143b174f6983c76f69ddcf4026",
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, EthMainnet);
        let web3_batch = subject.get_web3_batch();
        let web = subject.get_web3();

        let receiving_wallet = make_wallet("paying_wallet");
        let paying_wallet = make_paying_wallet(b"consuming_wallet");
        let amount: u128 = 1;
        let gas: U256 = 1.into();
        let gas_price: U256 = 1.into();
        let nonce: U256 = 1.into();

        let data = sign_transaction_data(amount, receiving_wallet);
        let transaction_parameters = TransactionParameters {
            nonce: None,
            to: Some(EthMainnet.rec().contract),
            gas: gas,
            gas_price: None,
            value: ethereum_types::U256::zero(),
            data: Bytes(data.to_vec()),
            chain_id: Some(EthMainnet.rec().num_chain_id),
        };
        let key = paying_wallet.prepare_secp256k1_secret().unwrap();
        let test = web3_batch.accounts().sign_transaction(transaction_parameters, &key);

        // eprintln!("Sign transaction result: {:?}", test);



        //
        // let test= sign_and_send_payment_2(
        //     EthMainnet,
        //     web3_batch.clone(),
        //     receiving_wallet,
        //     paying_wallet,
        //     amount,
        // ).wait();

        // let payable_account = make_payable_account(2);
        // let test = send_and_append_payment_2(
        //     EthMainnet,
        //     web3_batch.clone(),
        //     paying_wallet,
        //     payable_account,
        // ).wait();



        // eprintln!("Test result: {:?}", test);



        // web3_batch.eth().send_raw_transaction(Default::default());

        // web3_batch.eth().send_transaction(TransactionRequest {
        //     from: paying_wallet.address(),
        //     to: Some(receiving_wallet.address()),
        //     gas: Some(gas),
        //     gas_price: Some(gas_price),
        //     value: Some(amount),
        //     data: None,
        //     nonce: Some(nonce),
        //     condition: None,
        // });

        // web3_batch.eth().block_number();
        // web3_batch.eth().block_number();
        // web3_batch.eth().block_number();

        let result =  web3_batch.eth().transport().submit_batch().wait().expect("TODO: panic message");
        eprintln!("result {:?}", result);
        eprintln!("Test result: {:?}", test.wait());
    }

    #[test]
    fn tmp_new_payment_test() {



    }



    #[test]
    fn append_signed_transaction_to_batch_works() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .response("0x8290c22bd9b4d61bc57222698799edd7bbc8df5214be44e239a95f679249c59c".to_string(),7 )
            .end_batch()
            .start();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let pending_nonce = 1;
        let chain= TEST_DEFAULT_CHAIN;
        let gas_price = DEFAULT_GAS_PRICE;
        let consuming_wallet = make_paying_wallet(b"paying_wallet");
        let account = make_payable_account(1);
        let web3_batch = subject.get_web3_batch();
        let signed_transaction = sign_transaction(
            chain,
            web3_batch.clone(),
            account.wallet,
            consuming_wallet,
            account.balance_wei,
            pending_nonce.into(),
            gas_price,
        );

        append_signed_transaction_to_batch(web3_batch.clone(), signed_transaction.raw_transaction);

        let mut batch_result = web3_batch.eth().transport().submit_batch().wait().unwrap();
        let result = batch_result.pop().unwrap().unwrap();
        assert_eq!(result, Value::String("0x8290c22bd9b4d61bc57222698799edd7bbc8df5214be44e239a95f679249c59c".to_string()));
    }

    #[test]
    fn handle_new_transaction_works() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .response("0x94881436a9c89f48b01651ff491c69e97089daf71ab8cfb240243d7ecf9b38b2".to_string(),7 )
            .end_batch()
            .start();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let pending_nonce = 1;
        let chain= DEFAULT_CHAIN;
        let gas_price = DEFAULT_GAS_PRICE;
        let consuming_wallet = make_paying_wallet(b"paying_wallet");
        let account = make_payable_account(1);
        let web3_batch = subject.get_web3_batch();
        let result = handle_new_transaction(
            chain,
            web3_batch.clone(),
            account.wallet,
            consuming_wallet,
            account.balance_wei,
            pending_nonce.into(),
            gas_price,
        );

        let mut batch_result = web3_batch.eth().transport().submit_batch().wait().unwrap();

        assert_eq!(result, H256::from_str("94881436a9c89f48b01651ff491c69e97089daf71ab8cfb240243d7ecf9b38b2").unwrap());
        assert_eq!(batch_result.pop().unwrap().unwrap(), Value::String("0x94881436a9c89f48b01651ff491c69e97089daf71ab8cfb240243d7ecf9b38b2".to_string()));
    }

    #[test]
    fn sign_and_append_payment_works() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let pending_nonce = 1;
        let chain = DEFAULT_CHAIN;
        let gas_price = DEFAULT_GAS_PRICE;
        let consuming_wallet = make_paying_wallet(b"paying_wallet");
        let account = make_payable_account(1);
        let amount = account.balance_wei;

        let result = sign_and_append_payment(
            chain,
            subject.get_web3_batch(),
            consuming_wallet,
            pending_nonce.into(),
            gas_price,
            account,
        );

        let expected_hash_and_amount = HashAndAmount {
            hash: H256::from_str("94881436a9c89f48b01651ff491c69e97089daf71ab8cfb240243d7ecf9b38b2").unwrap(),
            amount,
        };
        assert_eq!(result, expected_hash_and_amount);
    }

    #[test]
    fn send_and_append_multiple_payments_works() {
        let port = find_free_port();
        let logger = Logger::new("send_and_append_multiple_payments_works");
        let blockchain_web3 = make_blockchain_interface(Some(port));
        let chain = DEFAULT_CHAIN;
        let gas_price = DEFAULT_GAS_PRICE;
        let pending_nonce = 1;
        let web3_batch = blockchain_web3.get_web3_batch();
        let consuming_wallet = make_paying_wallet(b"paying_wallet");
        let account_1 = make_payable_account(1);
        let account_2 = make_payable_account(2);
        let accounts = vec![account_1, account_2];

        let result = sign_and_append_multiple_payments(
            logger,
            chain,
            web3_batch,
            consuming_wallet,
            gas_price,
            pending_nonce.into(),
            accounts,
        );

        assert_eq!(result, vec![HashAndAmount { hash: H256::from_str("94881436a9c89f48b01651ff491c69e97089daf71ab8cfb240243d7ecf9b38b2").unwrap(), amount: 1000000000 }, HashAndAmount { hash: H256::from_str("3811874d2b73cecd51234c94af46bcce918d0cb4de7d946c01d7da606fe761b5").unwrap(), amount: 2000000000 }]);
    }

    #[test]
    fn transmission_log_just_works() {
        init_test_logging();
        let test_name  = "transmission_log_just_works";
        let gas_price = 120;
        let logger = Logger::new(test_name);
        let amount_1 = gwei_to_wei(900_000_000_u64);
        let account_1 = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
            make_wallet("w123"),
            amount_1,
            None,
        );
        let amount_2 = 123_456_789_u128;
        let account_2 = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
            make_wallet("w555"),
            amount_2,
            None,
        );
        let amount_3 = gwei_to_wei(33_355_666_u64);
        let account_3 = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
            make_wallet("w987"),
            amount_3,
            None,
        );
        let accounts_to_process = vec![account_1, account_2, account_3];

        info!(logger, "{}", transmission_log(TEST_DEFAULT_CHAIN, &accounts_to_process, gas_price));

        let log_handler = TestLogHandler::new();
        log_handler.exists_log_containing(
            "INFO: transmission_log_just_works: Paying to creditors...\n\
        Transactions in the batch:\n\
        \n\
        gas price:                                   120 gwei\n\
        chain:                                       ropsten\n\
        \n\
        [wallet address]                             [payment in wei]\n\
        0x0000000000000000000000000000000077313233   900,000,000,000,000,000\n\
        0x0000000000000000000000000000000077353535   123,456,789\n\
        0x0000000000000000000000000000000077393837   33,355,666,000,000,000\n",
        );
    }

    #[test]
    fn output_by_joining_sources_works() {
        let accounts = vec![
            PayableAccount {
                wallet: make_wallet("4567"),
                balance_wei: 2_345_678,
                last_paid_timestamp: from_time_t(4500000),
                pending_payable_opt: None,
            },
            PayableAccount {
                wallet: make_wallet("5656"),
                balance_wei: 6_543_210,
                last_paid_timestamp: from_time_t(333000),
                pending_payable_opt: None,
            },
        ];
        let fingerprint_inputs = vec![
            HashAndAmount {
                hash: make_tx_hash(444),
                amount: 2_345_678,
            },
            HashAndAmount {
                hash: make_tx_hash(333),
                amount: 6_543_210,
            },
        ];
        let responses = vec![
            Ok(Value::String(String::from("blah"))),
            Err(web3::Error::Rpc(Error {
                code: ErrorCode::ParseError,
                message: "I guess we've got a problem".to_string(),
                data: None,
            })),
        ];

        let result = merged_output_data(responses, fingerprint_inputs, accounts.to_vec());

        assert_eq!(
            result,
            vec![
                Correct(PendingPayable {
                    recipient_wallet: make_wallet("4567"),
                    hash: make_tx_hash(444)
                }),
                Failed(RpcPayableFailure {
                    rpc_error: web3::Error::Rpc(Error {
                        code: ErrorCode::ParseError,
                        message: "I guess we've got a problem".to_string(),
                        data: None,
                    }),
                    recipient_wallet: make_wallet("5656"),
                    hash: make_tx_hash(333)
                })
            ]
        )
    }

    #[test]
    fn send_payables_within_batch_fails_on_submit_batch_call() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST.to_string(), port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let consuming_wallet_secret_raw_bytes = b"okay-wallet";
        let recipient_wallet = make_wallet("blah123");
        let unimportant_recipient = Recorder::new().start().recipient();
        let account = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
            recipient_wallet.clone(),
            5000,
            None,
        );
        let consuming_wallet = make_paying_wallet(consuming_wallet_secret_raw_bytes);
        let gas_price = 123;
        let nonce = U256::from(1);

        let result = send_payables_within_batch(
            Logger::new("test"),
            TEST_DEFAULT_CHAIN,
            Web3::new(Batch::new(transport)),
            consuming_wallet,
            gas_price,
            nonce,
            unimportant_recipient,
            vec![account],
        )
        .wait();

        assert_eq!(
            result,
            Err(
                Sending {
                    msg: "Transport error: Error(Connect, Os { code: 61, kind: ConnectionRefused, message: \"Connection refused\" })".to_string(),
                    hashes: vec![H256::from_str("424c0231591a9879d82f25e0d81e09f39499b2bfd56b3aba708491995e35b4ac").unwrap()]
                }
            )
        );
    }

    #[test]
    fn advance_used_nonce_works() {
        let initial_nonce = U256::from(55);

        let result = advance_used_nonce(initial_nonce);

        assert_eq!(result, U256::from(56))
    }

    #[test]
    fn request_block_number_works() {
        let port = find_free_port();
        let _test_server = TestServer::start(
            port,
            vec![br#"{"jsonrpc":"2.0","id":7,"result":"0x2c7b8e7"}"#.to_vec()], // 0x400 == 1024
        );
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let start_block_nbr = BlockNumber::Number(100u64.into());
        let end_block_nbr = BlockNumber::Number(2024u64.into());
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let logger = Logger::new("request_block_number_works");

        let result =
            request_block_number(subject.get_web3(), start_block_nbr, end_block_nbr, logger).wait();

        assert_eq!(result, Ok(46643431u64));
    }

    #[test]
    fn request_block_number_fallback_works() {
        let port = find_free_port();
        let _test_server = TestServer::start(
            port,
            vec![br#"{"jsonrpc":"2.0","id":7,"result":""}"#.to_vec()], // 0x400 == 1024
        );
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        )
            .unwrap();
        let start_block_nbr = BlockNumber::Number(100u64.into());
        let end_block_nbr = BlockNumber::Number(2024u64.into());
        let subject =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let logger = Logger::new("request_block_number_works");

        let result =
            request_block_number(subject.get_web3(), start_block_nbr, end_block_nbr, logger).wait();

        assert_eq!(result, Ok(2024u64));
    }

    #[test]
    #[should_panic(expected = "Consuming wallet doesnt contain a secret key: Signature(\"Cannot sign with non-keypair wallet: Address(0x000000000000000000006261645f77616c6c6574).\")")]
    fn sign_transaction_panics_on_signing_itself() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST.to_string(), port),
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let recipient_wallet = make_wallet("unlucky man");
        let consuming_wallet = make_wallet("bad_wallet");
        let gas_price = 123;
        let nonce = U256::from(1);

        let result = sign_transaction(
            Chain::PolyMumbai,
            Web3::new(Batch::new(transport)),
            recipient_wallet,
            consuming_wallet,
            444444,
            nonce,
            gas_price,
        );
    }


    #[test]
    fn send_payables_within_batch_works() {
        init_test_logging();
        let test_name = "send_payables_within_batch_works";
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .response("rpc_result".to_string(),7 )
            .response("rpc_result_2".to_string(),7 )
            .end_batch()
            .start();
        let blockchain_interface_web3 =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let web3_batch = blockchain_interface_web3.get_web3_batch();
        let (accountant, _, accountant_recording) = make_recorder();
        let logger = Logger::new(test_name);
        let chain = DEFAULT_CHAIN;
        let consuming_wallet = make_paying_wallet(b"consuming_wallet");
        let gas_price = 1u64;
        let pending_nonce: U256 = 1.into();
        let new_fingerprints_recipient= accountant.start().recipient();
        let accounts_1 = make_payable_account(1);
        let accounts_2 = make_payable_account(2);
        let accounts = vec![accounts_1.clone(), accounts_2.clone()];
        let system = System::new(test_name);
        let timestamp_before = SystemTime::now();

        let result = send_payables_within_batch(
            logger,
            chain,
            web3_batch,
            consuming_wallet.clone(),
            gas_price.clone(),
            pending_nonce,
            new_fingerprints_recipient,
            accounts.clone(),
        ).wait();

        System::current().stop();
        system.run();
        let tlh = TestLogHandler::new();
        let timestamp_after = SystemTime::now();
        let recording_result = accountant_recording.lock().unwrap();
        let processed_payments = result.unwrap();
        let message = recording_result.get_record::<PendingPayableFingerprintSeeds>(0);
        assert_eq!(recording_result.len(), 1);
        assert!(timestamp_before <= message.batch_wide_timestamp);
        assert!(timestamp_after >= message.batch_wide_timestamp);
        assert_eq!(message.hashes_and_balances, vec![
            HashAndAmount{
                hash: H256::from_str("35f42b260f090a559e8b456718d9c91a9da0f234ed0a129b9d5c4813b6615af4").unwrap(),
                amount: accounts_1.balance_wei
            },
            HashAndAmount{
                hash: H256::from_str("7f3221109e4f1de8ba1f7cd358aab340ecca872a1456cb1b4f59ca33d3e22ee3").unwrap(),
                amount: accounts_2.balance_wei
            },
        ]);
        assert_eq!(processed_payments[0], ProcessedPayableFallible::Correct(PendingPayable {
            recipient_wallet: accounts_1.wallet,
            hash:  H256::from_str("35f42b260f090a559e8b456718d9c91a9da0f234ed0a129b9d5c4813b6615af4").unwrap()
        }));
        assert_eq!(processed_payments[1], ProcessedPayableFallible::Correct(PendingPayable {
            recipient_wallet: accounts_2.wallet,
            hash:  H256::from_str("7f3221109e4f1de8ba1f7cd358aab340ecca872a1456cb1b4f59ca33d3e22ee3").unwrap()
        }));
        tlh.exists_log_containing(
            &format!("DEBUG: {test_name}: Common attributes of payables to be transacted: sender wallet: {}, contract: {:?}, chain_id: {}, gas_price: {}",
                     consuming_wallet,
                     chain.rec().contract,
                     chain.rec().num_chain_id,
                     gas_price
            )
        );
        tlh.exists_log_containing(&format!("INFO: {test_name}: {}", transmission_log(chain, &accounts, gas_price)));
    }

    #[test]
    fn send_payables_within_batch_all_payments_fail() {
        init_test_logging();
        let test_name = "send_payables_within_batch_all_payments_fail";
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .err_response(429, "The requests per second (RPS) of your requests are higher than your plan allows.".to_string(), 7)
            .err_response(429, "The requests per second (RPS) of your requests are higher than your plan allows.".to_string(), 7)
            .end_batch()
            .start();
        let blockchain_interface_web3 =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let web3_batch = blockchain_interface_web3.get_web3_batch();
        let (accountant, _, accountant_recording) = make_recorder();
        let logger = Logger::new(test_name);
        let chain = DEFAULT_CHAIN;
        let consuming_wallet = make_paying_wallet(b"consuming_wallet");
        let gas_price = 1u64;
        let pending_nonce: U256 = 1.into();
        let new_fingerprints_recipient= accountant.start().recipient();
        let accounts_1 = make_payable_account(1);
        let accounts_2 = make_payable_account(2);
        let accounts = vec![accounts_1.clone(), accounts_2.clone()];
        let system = System::new(test_name);
        let timestamp_before = SystemTime::now();

        let result = send_payables_within_batch(
            logger,
            chain,
            web3_batch,
            consuming_wallet.clone(),
            gas_price.clone(),
            pending_nonce,
            new_fingerprints_recipient,
            accounts.clone(),
        ).wait();

        System::current().stop();
        system.run();
        let tlh = TestLogHandler::new();
        let timestamp_after = SystemTime::now();
        let recording_result = accountant_recording.lock().unwrap();
        let processed_payments = result.unwrap();
        let message = recording_result.get_record::<PendingPayableFingerprintSeeds>(0);
        assert_eq!(recording_result.len(), 1);
        assert!(timestamp_before <= message.batch_wide_timestamp);
        assert!(timestamp_after >= message.batch_wide_timestamp);
        assert_eq!(message.hashes_and_balances, vec![
            HashAndAmount{
                hash: H256::from_str("35f42b260f090a559e8b456718d9c91a9da0f234ed0a129b9d5c4813b6615af4").unwrap(),
                amount: accounts_1.balance_wei
            },
            HashAndAmount{
                hash: H256::from_str("7f3221109e4f1de8ba1f7cd358aab340ecca872a1456cb1b4f59ca33d3e22ee3").unwrap(),
                amount: accounts_2.balance_wei
            },
        ]);
        assert_eq!(processed_payments[0], ProcessedPayableFallible::Failed(RpcPayableFailure{
            rpc_error: Rpc(Error {
                code: ServerError(429),
                message: "The requests per second (RPS) of your requests are higher than your plan allows.".to_string(),
                data: None,
            }),
            recipient_wallet: accounts_1.wallet,
            hash: H256::from_str("35f42b260f090a559e8b456718d9c91a9da0f234ed0a129b9d5c4813b6615af4").unwrap(),
        }));
        assert_eq!(processed_payments[1], ProcessedPayableFallible::Failed(RpcPayableFailure{
            rpc_error: Rpc(Error {
                code: ServerError(429),
                message: "The requests per second (RPS) of your requests are higher than your plan allows.".to_string(),
                data: None,
            }),
            recipient_wallet: accounts_2.wallet,
            hash: H256::from_str("7f3221109e4f1de8ba1f7cd358aab340ecca872a1456cb1b4f59ca33d3e22ee3").unwrap(),
        }));
        tlh.exists_log_containing(
            &format!("DEBUG: {test_name}: Common attributes of payables to be transacted: sender wallet: {}, contract: {:?}, chain_id: {}, gas_price: {}",
                     consuming_wallet,
                     chain.rec().contract,
                     chain.rec().num_chain_id,
                     gas_price
            )
        );
        tlh.exists_log_containing(&format!("INFO: {test_name}: {}", transmission_log(chain, &accounts, gas_price)));
    }

    #[test]
    fn send_payables_within_batch_one_payment_works_the_other_fails() {
        init_test_logging();
        let test_name = "send_payables_within_batch_one_payment_works_the_other_fails";
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST, port),
            REQUESTS_IN_PARALLEL,
        ).unwrap();
        let blockchain_client_server = MBCSBuilder::new(port)
            .begin_batch()
            .response("rpc_result".to_string(),7 )
            .err_response(429, "The requests per second (RPS) of your requests are higher than your plan allows.".to_string(), 7)
            .end_batch()
            .start();
        let blockchain_interface_web3 =
            BlockchainInterfaceWeb3::new(transport, event_loop_handle, TEST_DEFAULT_CHAIN);
        let web3_batch = blockchain_interface_web3.get_web3_batch();
        let (accountant, _, accountant_recording) = make_recorder();
        let logger = Logger::new(test_name);
        let chain = DEFAULT_CHAIN;
        let consuming_wallet = make_paying_wallet(b"consuming_wallet");
        let gas_price = 1u64;
        let pending_nonce: U256 = 1.into();
        let new_fingerprints_recipient= accountant.start().recipient();
        let accounts_1 = make_payable_account(1);
        let accounts_2 = make_payable_account(2);
        let accounts = vec![accounts_1.clone(), accounts_2.clone()];
        let system = System::new(test_name);
        let timestamp_before = SystemTime::now();

        let result = send_payables_within_batch(
            logger,
            chain,
            web3_batch,
            consuming_wallet.clone(),
            gas_price.clone(),
            pending_nonce,
            new_fingerprints_recipient,
            accounts.clone(),
        ).wait();

        System::current().stop();
        system.run();
        let tlh = TestLogHandler::new();
        let timestamp_after = SystemTime::now();
        let recording_result = accountant_recording.lock().unwrap();
        let processed_payments = result.unwrap();
        let message = recording_result.get_record::<PendingPayableFingerprintSeeds>(0);
        assert_eq!(recording_result.len(), 1);
        assert!(timestamp_before <= message.batch_wide_timestamp);
        assert!(timestamp_after >= message.batch_wide_timestamp);
        assert_eq!(message.hashes_and_balances, vec![
            HashAndAmount{
                hash: H256::from_str("35f42b260f090a559e8b456718d9c91a9da0f234ed0a129b9d5c4813b6615af4").unwrap(),
                amount: accounts_1.balance_wei
            },
            HashAndAmount{
                hash: H256::from_str("7f3221109e4f1de8ba1f7cd358aab340ecca872a1456cb1b4f59ca33d3e22ee3").unwrap(),
                amount: accounts_2.balance_wei
            },
        ]);
        assert_eq!(processed_payments[0], ProcessedPayableFallible::Correct(PendingPayable {
            recipient_wallet: accounts_1.wallet,
            hash:  H256::from_str("35f42b260f090a559e8b456718d9c91a9da0f234ed0a129b9d5c4813b6615af4").unwrap()
        }));
        assert_eq!(processed_payments[1], ProcessedPayableFallible::Failed(RpcPayableFailure{
            rpc_error: Rpc(Error {
                code: ServerError(429),
                message: "The requests per second (RPS) of your requests are higher than your plan allows.".to_string(),
                data: None,
            }),
            recipient_wallet: accounts_2.wallet,
            hash: H256::from_str("7f3221109e4f1de8ba1f7cd358aab340ecca872a1456cb1b4f59ca33d3e22ee3").unwrap(),
        }));
        tlh.exists_log_containing(
            &format!("DEBUG: {test_name}: Common attributes of payables to be transacted: sender wallet: {}, contract: {:?}, chain_id: {}, gas_price: {}",
                     consuming_wallet,
                     chain.rec().contract,
                     chain.rec().num_chain_id,
                     gas_price
            )
        );
        tlh.exists_log_containing(&format!("INFO: {test_name}: {}", transmission_log(chain, &accounts, gas_price)));
    }

    #[test]
    fn sign_transaction_just_works() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST.to_string(), port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        // let transport = TestTransport::default();

        let web3 = Web3::new(transport.clone());
        let chain = DEFAULT_CHAIN;
        let amount = 11_222_333_444;
        let gas_price_in_gwei = 123000000000_u64;
        let nonce = U256::from(5);
        let recipient_wallet = make_wallet("recipient_wallet");
        let consuming_wallet = make_paying_wallet(b"consuming_wallet");
        let consuming_wallet_secret_key = consuming_wallet.prepare_secp256k1_secret().unwrap();
        let data = sign_transaction_data(amount, recipient_wallet.clone());

        let tx_parameters = TransactionParameters {
            nonce: Some(nonce),
            to: Some(chain.rec().contract),
            gas: gas_limit(data, chain),
            gas_price: Some(to_wei(gas_price_in_gwei)),
            value: U256::zero(),
            data: Bytes(data.to_vec()),
            chain_id: Some(chain.rec().num_chain_id),
        };

        let result = sign_transaction(
            chain,
            Web3::new(Batch::new(transport)),
            recipient_wallet,
            consuming_wallet,
            amount,
            nonce,
            gas_price_in_gwei,
        );

        let signed_transaction = web3
            .accounts()
            .sign_transaction(tx_parameters, &consuming_wallet_secret_key)
            .wait()
            .unwrap();

        assert_eq!(result, signed_transaction);
    }

    #[test]
    fn sign_and_append_payment_just_works() {
        let port = find_free_port();
        let (event_loop_handle, transport) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST.to_string(), port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let consuming_wallet = make_paying_wallet(b"consuming_wallet");
        let system = System::new("test");
        let account = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
            make_wallet("blah123"),
            9000,
            None,
        );
        let gas_price = 123;
        let nonce = U256::from(1);

        let result = sign_and_append_payment(
            TEST_DEFAULT_CHAIN,
            Web3::new(Batch::new(transport)),
            consuming_wallet,
            nonce,
            gas_price,
            account,
        );

        System::current().stop();
        system.run();

        let expected_hash =
            H256::from_str("8d278f82f42ee4f3b9eef2e099cccc91ff117e80c28d6369fec38ec50f5bd2c2")
                .unwrap();
        assert_eq!(result.hash, expected_hash);
        assert_eq!(result.amount, 9000);
    }

    //with a real confirmation through a transaction sent with this data to the network
    #[test]
    fn web3_interface_signing_a_transaction_works_for_polygon_mumbai() {
        let chain = Chain::PolyMumbai;
        let nonce = 5;
        // signed_transaction_data changed after we changed the contract address of polygon matic
        let signed_transaction_data = "f8ad05850ba43b740083011980949b27034acabd44223fb23d628ba4849867ce1db280b844a9059cbb0000000000000000000000007788df76bbd9a0c7c3e5bf0f77bb28c60a167a7b000000000000000000000000000000000000000000000000000000e8d4a5100083027126a09fdbbd7064d3b7240f5422b2164aaa13d62f0946a683d82ee26f97f242570d90a077b49dbb408c20d73e0666ba0a77ac888bf7a9cb14824a5f35c97217b9bc0a5a";

        let in_bytes = decode_hex(signed_transaction_data).unwrap();

        assert_that_signed_transactions_agrees_with_template(chain, nonce, &in_bytes)
    }

    //with a real confirmation through a transaction sent with this data to the network
    #[test]
    fn web3_interface_signing_a_transaction_works_for_eth_ropsten() {
        let chain = Chain::EthRopsten;
        let nonce = 1; //must stay like this!
        let signed_transaction_data = "f8a90185199c82cc0082dee894384dec25e03f94931767ce4c3556168468ba24c380b844a9059cbb0000000000000000000000007788df76bbd9a0c7c3e5bf0f77bb28c60a167a7b000000000000000000000000000000000000000000000000000000e8d4a510002aa0635fbb3652e1c3063afac6ffdf47220e0431825015aef7daff9251694e449bfca00b2ed6d556bd030ac75291bf58817da15a891cd027a4c261bb80b51f33b78adf";
        let in_bytes = decode_hex(signed_transaction_data).unwrap();

        assert_that_signed_transactions_agrees_with_template(chain, nonce, &in_bytes)
    }

    //not confirmed on the real network
    #[test]
    fn web3_interface_signing_a_transaction_for_polygon_mainnet() {
        let chain = Chain::PolyMainnet;
        let nonce = 10;
        //generated locally
        let signed_transaction_data = [
            248, 172, 10, 133, 11, 164, 59, 116, 0, 131, 1, 25, 128, 148, 238, 154, 53, 47, 106,
            172, 74, 241, 165, 185, 244, 103, 246, 169, 62, 15, 251, 233, 221, 53, 128, 184, 68,
            169, 5, 156, 187, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 119, 136, 223, 118, 187, 217,
            160, 199, 195, 229, 191, 15, 119, 187, 40, 198, 10, 22, 122, 123, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 232, 212, 165, 16, 0, 130,
            1, 53, 160, 7, 203, 40, 44, 202, 233, 15, 5, 64, 218, 199, 239, 94, 126, 152, 2, 108,
            30, 157, 75, 124, 129, 117, 27, 109, 163, 132, 27, 11, 123, 137, 10, 160, 18, 170, 130,
            198, 73, 190, 158, 235, 0, 77, 118, 213, 244, 229, 225, 143, 156, 214, 219, 204, 193,
            155, 199, 164, 162, 31, 134, 51, 139, 130, 152, 104,
        ];

        assert_that_signed_transactions_agrees_with_template(chain, nonce, &signed_transaction_data)
    }

    //not confirmed on the real network
    #[test]
    fn web3_interface_signing_a_transaction_for_eth_mainnet() {
        let chain = Chain::EthMainnet;
        let nonce = 10;
        //generated locally
        let signed_transaction_data = [
            248, 169, 10, 133, 25, 156, 130, 204, 0, 130, 222, 232, 148, 6, 243, 195, 35, 240, 35,
            140, 114, 191, 53, 1, 16, 113, 242, 181, 183, 244, 58, 5, 76, 128, 184, 68, 169, 5,
            156, 187, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 119, 136, 223, 118, 187, 217, 160, 199,
            195, 229, 191, 15, 119, 187, 40, 198, 10, 22, 122, 123, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 232, 212, 165, 16, 0, 38, 160, 199,
            155, 76, 106, 39, 227, 3, 151, 90, 117, 245, 211, 86, 98, 187, 117, 120, 103, 165, 131,
            99, 72, 36, 211, 10, 224, 252, 104, 51, 200, 230, 158, 160, 84, 18, 140, 248, 119, 22,
            193, 14, 148, 253, 48, 59, 185, 11, 38, 152, 103, 150, 120, 60, 74, 56, 159, 206, 22,
            15, 73, 173, 153, 11, 76, 74,
        ];

        assert_that_signed_transactions_agrees_with_template(chain, nonce, &signed_transaction_data)
    }

    #[test]
    fn gas_limit_for_polygon_mainnet_lies_within_limits_for_raw_transaction() {
        test_gas_limit_is_between_limits(Chain::PolyMainnet);
    }

    #[test]
    fn gas_limit_for_eth_mainnet_lies_within_limits_for_raw_transaction() {
        test_gas_limit_is_between_limits(Chain::EthMainnet)
    }

    #[test]
    fn get_transaction_fee_balance_works() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x23".to_string(),1 )
            .start();
        let blockchain_interface_web3 = make_blockchain_interface(Some(port));
        let web3 = blockchain_interface_web3.get_web3();
        let wallet = &Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap();

        let result = get_transaction_fee_balance(web3, wallet.address()).wait();

        assert_eq!(
            result,
            Ok(35.into())
        );
    }

    #[test]
    fn get_transaction_fee_balance_returns_an_error_for_unintelligible_response_to_requesting_eth_balance(
    ) {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0xFFFQ".to_string(),0 )
            .start();
        let blockchain_interface_web3 = make_blockchain_interface(Some(port));
        let web3 = blockchain_interface_web3.get_web3();

        let result = get_transaction_fee_balance(web3, Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap().address()).wait();

        match result {
            Err(BlockchainError::QueryFailed(msg)) if msg.contains("invalid hex character: Q") => {
                ()
            }
            x => panic!("Expected complaint about hex character, but got {:?}", x),
        };
    }

    #[test]
    fn get_token_balance_can_retrieve_token_balance_of_a_wallet() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x000000000000000000000000000000000000000000000000000000000000FFFF".to_string(),0 )
            .start();
        let blockchain_interface_web3 = make_blockchain_interface(Some(port));
        let contract = blockchain_interface_web3.get_contract();

        let result = get_token_balance(contract, Wallet::from_str( "0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap().address() )
            .wait()
            .unwrap();

        assert_eq!(result, U256::from(65_535));
    }

    #[test]
    fn get_token_balance_returns_error_for_unintelligible_response_to_token_balance() {
        let port = find_free_port();
        let blockchain_client_server = MBCSBuilder::new(port)
            .response("0x000000000000000000000000000000000000000000000000000000000000FFFQ".to_string(),0 )
            .start();
        let blockchain_interface_web3 = make_blockchain_interface(Some(port));
        let contract = blockchain_interface_web3.get_contract();
        let expected_err_msg = "Invalid hex";

        let result = get_token_balance(contract, Wallet::from_str("0x3f69f9efd4f2592fd70be8c32ecd9dce71c472fc").unwrap().address()).wait();

        let err_msg = match result {
            Err(BlockchainError::QueryFailed(msg)) => msg,
            x => panic!("Expected BlockchainError::QueryFailed, but got {:?}", x),
        };
        assert!(
            err_msg.contains(expected_err_msg),
            "Expected this fragment {} in this err msg: {}",
            expected_err_msg,
            err_msg
        )
    }


    fn assert_that_signed_transactions_agrees_with_template(
        chain: Chain,
        nonce: u64,
        template: &[u8],
    ) {
        const TEST_PAYMENT_AMOUNT: u128 = 1_000_000_000_000;
        const TEST_GAS_PRICE_ETH: u64 = 110;
        const TEST_GAS_PRICE_POLYGON: u64 = 50;

        let port = find_free_port();
        let (event_loop_handle, http) = Http::with_max_parallel(
            &format!("http://{}:{}", &Ipv4Addr::LOCALHOST.to_string(), port),
            REQUESTS_IN_PARALLEL,
        )
        .unwrap();
        let transport = http;
        // let subject = BlockchainInterfaceWeb3::new(transport, make_fake_event_loop_handle(), chain);

        let consuming_wallet = {
            let key_pair = Bip32EncryptionKeyProvider::from_raw_secret(
                &decode_hex("97923d8fd8de4a00f912bfb77ef483141dec551bd73ea59343ef5c4aac965d04")
                    .unwrap(),
            )
            .unwrap();
            Wallet::from(key_pair)
        };

        let recipient_wallet = {
            let hex_part = &"0x7788df76BBd9a0C7c3e5bf0f77bb28C60a167a7b"[2..];
            let recipient_address_bytes = decode_hex(hex_part).unwrap();
            let address = Address::from_slice(&recipient_address_bytes);
            Wallet::from(address)
        };

        let nonce_correct_type = U256::from(nonce);
        let gas_price = match chain {
            Chain::EthMainnet => TEST_GAS_PRICE_ETH,
            Chain::EthRopsten => TEST_GAS_PRICE_ETH,
            Chain::PolyMainnet => TEST_GAS_PRICE_POLYGON,
            Chain::PolyMumbai => TEST_GAS_PRICE_POLYGON,
            _ => panic!("isn't our interest in this test"),
        };
        let payable_account = make_payable_account_with_wallet_and_balance_and_timestamp_opt(
            recipient_wallet,
            TEST_PAYMENT_AMOUNT,
            None,
        );

        let signed_transaction = sign_transaction(
            chain,
            Web3::new(Batch::new(transport)),
            payable_account.wallet,
            consuming_wallet,
            payable_account.balance_wei,
            nonce_correct_type,
            gas_price,
        );

        let byte_set_to_compare = signed_transaction.raw_transaction.0;
        assert_eq!(byte_set_to_compare.as_slice(), template)
    }

    fn test_gas_limit_is_between_limits(chain: Chain) {
        let not_under_this_value = match chain {
            // TODO: GH-744 this could be use by web3_gas_limit_const_part - once Merged with Master
            Chain::EthMainnet | Chain::EthRopsten | Chain::Dev => 55_000,
            Chain::PolyMainnet | Chain::PolyMumbai => 70_000,
        };
        let not_above_this_value = not_under_this_value + 3328; // TODO: GH-744: this number can be replace by const WEB3_MAXIMAL_GAS_LIMIT_MARGIN. - once Merged with Master
        let data = sign_transaction_data(1_000_000_000, make_wallet("wallet1"));

        let gas_limit = gas_limit(data, chain);

        assert!(
            gas_limit >= U256::from(not_under_this_value),
            "actual gas limit {} isn't above or equal {}",
            gas_limit,
            not_under_this_value
        );
        assert!(
            gas_limit <= U256::from(not_above_this_value),
            "actual gas limit {} isn't below or equal {}",
            gas_limit,
            not_above_this_value
        );
    }
}
