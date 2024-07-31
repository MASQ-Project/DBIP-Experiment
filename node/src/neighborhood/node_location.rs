// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use ip_country_lib;
use ip_country_lib::country_finder::{CountryCodeFinder, COUNTRY_CODE_FINDER};
use std::net::IpAddr;

#[allow(dead_code)]
#[derive(Clone, Debug, Default, Eq)]
pub struct NodeLocation {
    pub country_code: String,
    pub free_world_bit: bool,
}

impl PartialEq<Self> for NodeLocation {
    fn eq(&self, other: &Self) -> bool {
        self.country_code == other.country_code
    }
}

pub fn get_node_location(ip_opt: Option<IpAddr>) -> Option<NodeLocation> {
    match ip_opt {
        Some(ip_addr) => {
            let country_opt = CountryCodeFinder::find_country(&COUNTRY_CODE_FINDER, ip_addr);
            country_opt.map(|country| NodeLocation {
                country_code: country.iso3166.to_string(),
                free_world_bit: country.free_world,
            })
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::neighborhood::gossip::GossipBuilder;
    use crate::neighborhood::node_location::{get_node_location, NodeLocation};
    use crate::neighborhood::node_record::{NodeRecord, NodeRecordMetadata};
    use crate::test_utils::neighborhood_test_utils::{
        db_from_node, make_node_record, pick_country_code_record,
    };
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn test_node_location() {
        let node_location =
            get_node_location(Some(IpAddr::V4(Ipv4Addr::new(125, 125, 125, 1)))).unwrap();

        assert_eq!(node_location.country_code, "CN");
        assert_eq!(node_location.free_world_bit, false);
    }

    #[test]
    fn construct_node_record_metadata_with_free_world_bit() {
        let mut metadata = NodeRecordMetadata::new();
        metadata.node_location_opt = get_node_location(Some(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert_eq!(
            metadata.node_location_opt.as_ref().unwrap(),
            &NodeLocation {
                country_code: "AU".to_string(),
                free_world_bit: true
            }
        );
    }

    #[test]
    fn construct_node_record_for_test() {
        let node_record = make_node_record(3333, true);
        let (record_addr, record_cc, record_fwb) = pick_country_code_record(3333);

        assert_eq!(
            node_record.node_addr_opt().as_ref().unwrap().ip_addr(),
            record_addr
        );
        assert_eq!(node_record.inner.country_code_opt.unwrap(), record_cc);
        assert_eq!(
            node_record
                .metadata
                .node_location_opt
                .as_ref()
                .unwrap()
                .free_world_bit,
            record_fwb
        );
        assert_eq!(
            node_record.metadata.node_location_opt,
            Some(NodeLocation {
                country_code: "AU".to_string(),
                free_world_bit: true
            })
        )
    }

    #[test]
    fn node_record_from_gossip_with_addr_and_country_is_populated_with_right_addr_and_free_world_bit(
    ) {
        let mut original_node_record = make_node_record(2222, true);

        let db = db_from_node(&original_node_record);
        let builder = GossipBuilder::new(&db);

        let builder = builder.node(original_node_record.public_key(), true);

        let mut gossip = builder.build();
        let gossip_result = gossip.node_records.remove(0);
        let result_node_record = NodeRecord::try_from(&gossip_result).unwrap();

        original_node_record.metadata.last_update = result_node_record.last_updated();
        assert_eq!(result_node_record, original_node_record)
    }
}

#[allow(dead_code, unused_imports)]
mod test_ip_country_performance {
    use crate::neighborhood::node_location::{get_node_location, NodeLocation};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    use std::time::SystemTime;

    #[test]
    fn get_node_location_for_test_with_1000_v4_high_val_ips() {
        let mut result_vec: Vec<Option<NodeLocation>> = vec![];
        let _ = get_node_location(Some(IpAddr::V4(Ipv4Addr::from(3333))));
        let start = 0xdfff8000u32;
        let time_start = SystemTime::now();

        (start..(start + 1000)).for_each(|num| {
            let address = IpAddr::V4(Ipv4Addr::from(num));
            result_vec.push(get_node_location(Some(address)));
        });

        let time_end = SystemTime::now();

        let duration = time_end.duration_since(time_start).unwrap();
        assert!(
            duration.as_secs() < 1,
            "Duration of the search was too long: {} ms",
            duration.as_millis()
        );
        while !result_vec.is_empty() {
            let location = result_vec.remove(0);
            assert_eq!(location.unwrap().free_world_bit, true);
        }
    }

    #[test]
    fn get_node_location_for_test_with_1000_v4_low_val_ips() {
        let mut result_vec: Vec<Option<NodeLocation>> = vec![];
        let _ = get_node_location(Some(IpAddr::V4(Ipv4Addr::from(3333))));
        let start = 0x10100101u32;
        let time_start = SystemTime::now();

        (start..(start + 1000)).for_each(|num| {
            let address = IpAddr::V4(Ipv4Addr::from(num));
            result_vec.push(get_node_location(Some(address)));
        });

        let time_end = SystemTime::now();

        let duration = time_end.duration_since(time_start).unwrap();
        assert!(
            duration.as_secs() < 1,
            "Duration of the search was too long: {} ms",
            duration.as_millis()
        );
        while !result_vec.is_empty() {
            let location = result_vec.remove(0);
            assert_eq!(location.unwrap().free_world_bit, true);
        }
    }

    #[test]
    fn get_node_location_for_test_with_1000_v6_middle_val_ips() {
        let mut result_vec: Vec<Option<NodeLocation>> = vec![];
        let _ = get_node_location(Some(IpAddr::V6(Ipv6Addr::from(3333))));
        let start = 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFF0000u128;
        let time_start = SystemTime::now();

        (start..(start + 1000)).for_each(|num| {
            let address = IpAddr::V6(Ipv6Addr::from(num));
            result_vec.push(get_node_location(Some(address)));
        });

        let time_end = SystemTime::now();

        let duration = time_end.duration_since(time_start).unwrap();
        assert!(
            duration.as_secs() < 1,
            "Duration of the search was too long: {} ms",
            duration.as_millis()
        );
        while !result_vec.is_empty() {
            let location = result_vec.remove(0);
            assert_eq!(location.unwrap().free_world_bit, true);
        }
    }
}
