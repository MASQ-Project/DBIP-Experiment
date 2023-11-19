// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::blockchains::blockchain_records::{BlockchainRecord, CHAINS};
use crate::constants::{
    DEFAULT_CHAIN, DEV_CHAIN_FULL_IDENTIFIER, ETH_MAINNET_FULL_IDENTIFIER,
    ETH_ROPSTEN_FULL_IDENTIFIER, POLYGON_MAINNET_FULL_IDENTIFIER, POLYGON_MUMBAI_FULL_IDENTIFIER,
};
use serde_derive::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use core::str::FromStr;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub enum Chain {
    EthMainnet,
    EthRopsten,
    PolyMainnet,
    PolyMumbai,
    Dev,
}

#[derive(Debug, PartialEq, Eq, Copy, Clone, Hash)]
pub enum ChainFamily {
    Eth,
    Polygon,
    Dev,
}

impl Default for Chain {
    fn default() -> Self {
        DEFAULT_CHAIN
    }
}

impl FromStr for Chain {
    type Err = String;

    fn from_str(str: &str) -> Result<Self, Self::Err> {
        if str == POLYGON_MAINNET_FULL_IDENTIFIER {
            Ok(Chain::PolyMainnet)
        } else if str == ETH_MAINNET_FULL_IDENTIFIER {
            Ok(Chain::EthMainnet)
        } else if str == POLYGON_MUMBAI_FULL_IDENTIFIER {
            Ok(Chain::PolyMumbai)
        } else if str == ETH_ROPSTEN_FULL_IDENTIFIER {
            Ok(Chain::EthRopsten)
        } else if str == DEV_CHAIN_FULL_IDENTIFIER {
            Ok(Chain::Dev)
        } else {
            Err(format!("Clap let in a wrong value for chain: '{}'; if this happens we need to track down the slit", str))
        }
    }
}

impl Display for Chain {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let identifier = match self {
            Chain::PolyMainnet => POLYGON_MAINNET_FULL_IDENTIFIER,
            Chain::EthMainnet => ETH_MAINNET_FULL_IDENTIFIER,
            Chain::PolyMumbai => POLYGON_MUMBAI_FULL_IDENTIFIER,
            Chain::EthRopsten => ETH_ROPSTEN_FULL_IDENTIFIER,
            Chain::Dev => DEV_CHAIN_FULL_IDENTIFIER,
        };
        write!(f, "{}", identifier)
    }
}

impl Chain {
    pub fn rec(&self) -> &BlockchainRecord {
        CHAINS
            .iter()
            .find(|b| &b.self_id == self)
            .unwrap_or_else(|| panic!("BlockchainRecord for '{:?}' doesn't exist", self))
        //untested panic - but works as an expect()
    }

    pub fn is_mainnet(&self) -> bool {
        Self::mainnets()
            .iter()
            .any(|mainnet_chain| mainnet_chain == self)
    }

    fn mainnets() -> &'static [Chain] {
        &[Chain::PolyMainnet, Chain::EthMainnet]
    }
}

pub fn chain_from_chain_identifier_opt(identifier: &str) -> Option<Chain> {
    return_record_opt_standard_impl(&|b: &&BlockchainRecord| b.literal_identifier == identifier)
        .map(|record| record.self_id)
}

fn return_record_opt_standard_impl(
    closure: &dyn Fn(&&BlockchainRecord) -> bool,
) -> Option<&BlockchainRecord> {
    return_record_opt_body(closure, &CHAINS)
}

fn return_record_opt_body<'a>(
    closure: &dyn Fn(&&'a BlockchainRecord) -> bool,
    collection_of_chains: &'a [BlockchainRecord],
) -> Option<&'a BlockchainRecord> {
    let filtered = collection_of_chains
        .iter()
        .filter(closure)
        .collect::<Vec<&BlockchainRecord>>();
    match filtered.len() {
        0 => None,
        1 => Some(filtered[0]),
        _ => panic!("Non-unique identifier used to query a BlockchainRecord"),
    }
}

#[cfg(test)]
mod tests {
    use crate::shared_schema::official_chain_names;
    use super::*;

    #[test]
    #[should_panic(expected = "Non-unique identifier used to query a BlockchainRecord")]
    fn return_record_opt_panics_if_more_records_meet_the_condition_from_the_closure() {
        let searched_name = "BruhBruh";
        let mut record_one = make_defaulted_blockchain_record();
        record_one.literal_identifier = searched_name;
        let mut record_two = make_defaulted_blockchain_record();
        record_two.literal_identifier = "Jooodooo";
        let mut record_three = make_defaulted_blockchain_record();
        record_three.literal_identifier = searched_name;
        let collection = [record_one, record_two, record_three];

        let _ = return_record_opt_body(
            &|b: &&BlockchainRecord| b.literal_identifier == searched_name,
            &collection,
        );
    }

    #[test]
    fn return_record_opt_standard_impl_uses_the_right_collection_of_chains() {
        CHAINS.iter().for_each(|record| {
            assert_eq!(
                record,
                return_record_opt_standard_impl(
                    &|b: &&BlockchainRecord| b.num_chain_id == record.num_chain_id
                )
                .unwrap()
            )
        });
    }

    #[test]
    fn gibberish_causes_an_error() {
        let result = Chain::from_str("olala");
        assert_eq! (result, Err("Clap let in a wrong value for chain: 'olala'; if this happens we need to track down the slit".to_string()))
    }

    #[test]
    fn from_str_and_display_work() {
        official_chain_names().iter().for_each(|expected_name| {
            let chain = Chain::from_str(*expected_name).unwrap();
            let actual_name = chain.to_string();
            assert_eq! (&actual_name, expected_name);
        })
    }

    fn make_defaulted_blockchain_record<'a>() -> BlockchainRecord {
        BlockchainRecord {
            num_chain_id: 0,
            self_id: Chain::PolyMainnet,
            literal_identifier: "",
            contract: Default::default(),
            contract_creation_block: 0,
            chain_family: ChainFamily::Polygon,
        }
    }

    #[test]
    fn is_mainnet_knows_about_all_mainnets() {
        let searched_str = "mainnet";
        assert_mainnet_exist();
        CHAINS.iter().for_each(|blockchain_record| {
            if blockchain_record.literal_identifier.contains(searched_str) {
                let chain = blockchain_record.self_id;
                assert_eq!(chain.is_mainnet(), true)
            }
        })
    }

    fn assert_mainnet_exist() {
        assert!(CHAINS
            .iter()
            .find(|blockchain_record| blockchain_record.literal_identifier.contains("mainnet"))
            .is_some());
    }
}
