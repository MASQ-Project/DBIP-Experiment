// Copyright (c) 2024, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use std::collections::HashMap;
use crate::country_block_stream::Country;

#[derive(Debug, PartialEq, Clone)]
pub struct Countries {
    countries: Vec<Country>,
    index_by_iso3166: HashMap<String, usize>,
}

impl Countries {
    pub fn old_new(countries: Vec<Country>) -> Self {
        let index_by_iso3166 = countries
            .iter()
            .map(|country| (country.iso3166.clone(), country.index))
            .collect::<HashMap<String, usize>>();
        Self {
            countries,
            index_by_iso3166,
        }
    }

    pub fn new(mut country_pairs: Vec<(&str, &str)>) -> Self {
        country_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let countries = country_pairs
            .iter()
            .enumerate()
            .map(|(index, (iso3166, name))| Country::new(index, *iso3166, *name))
            .collect::<Vec<Country>>();
        Self::old_new(countries)
    }

    pub fn country_from_code(&self, iso3166: &str) -> Result<Country, String> {
        let index = match self.index_by_iso3166.get(&iso3166.to_ascii_uppercase()) {
            None => return Err(format!("'{}' is not a valid ISO3166 country code", iso3166)),
            Some(index) => *index,
        };
        let country = self.country_from_index(index).unwrap_or_else(|_| {
            panic!(
                "Data error: ISO3166 {} maps to index {}, but there is no such Country",
                iso3166, index
            )
        });
        Ok(country)
    }

    pub fn country_from_index(&self, index: usize) -> Result<Country, String> {
        match self.countries.get(index) {
            None => Err(format!(
                "There are only {} Countries; no Country is at index {}",
                self.countries.len(),
                index
            )),
            Some(country) => Ok(country.clone()),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Country> {self.countries.iter()}

    pub fn len(&self) -> usize {self.countries.len()}
}

// impl TryFrom<&str> for Country {
//     type Error = String;
//
//     fn try_from(iso3166: &str) -> Result<Self, Self::Error> {
//         let index = match INDEX_BY_ISO3166.get(&iso3166.to_ascii_uppercase()) {
//             None => return Err(format!("'{}' is not a valid ISO3166 country code", iso3166)),
//             Some(index) => *index,
//         };
//         let country = Country::try_from(index).unwrap_or_else(|_| {
//             panic!(
//                 "Data error: ISO3166 {} maps to index {}, but there is no such Country",
//                 iso3166, index
//             )
//         });
//         Ok(country)
//     }
// }
//
// impl From<usize> for Country {
//     fn from(index: usize) -> Self {
//         match COUNTRIES.get(index) {
//             None => panic!(
//                 "There are only {} Countries; no Country is at index {}",
//                 COUNTRIES.len(),
//                 index
//             ),
//             Some(country) => country.clone(),
//         }
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbip_country::COUNTRIES;
    use crate::country_block_stream::Country;
    use itertools::Itertools;


    #[test]
    fn sentinel_is_first() {
        let sentinel = COUNTRIES.countries.get(0).unwrap();

        assert_eq!(sentinel.iso3166.as_str(), "ZZ");
        assert_eq!(sentinel.name.as_str(), "Sentinel");
    }

    #[test]
    fn countries_are_properly_ordered() {
        COUNTRIES.countries
            .iter()
            .skip(1)
            .tuple_windows()
            .for_each(|(a, b)|
                assert!(
                    a.iso3166 < b.iso3166,
                    "Country code {} should have come before {}, but was after",
                    b.iso3166, a.iso3166
                )
            );
    }

    #[test]
    fn countries_are_properly_indexed() {
        COUNTRIES.countries.iter().enumerate().for_each(|(index, country)| {
            assert_eq!(
                country.index, index,
                "Index for {} should have been {} but was {}",
                country.name, index, country.index
            )
        });
    }

    #[test]
    fn string_length_check() {
        COUNTRIES.countries.iter().for_each(|country| {
            assert_eq!(country.iso3166.len(), 2);
            assert_eq!(
                country.name.len() > 0,
                true,
                "Blank country name for {} at index {}",
                country.iso3166,
                country.index
            );
        })
    }

    #[test]
    fn try_from_str_happy_path() {
        for country in COUNTRIES.countries.iter() {
            let result = COUNTRIES.country_from_code(country.iso3166.as_str()).unwrap();

            assert_eq!(result, *country);
        }
    }

    #[test]
    fn try_from_str_wrong_case() {
        for country in COUNTRIES.countries.iter() {
            let result = COUNTRIES.country_from_code(country.iso3166.to_lowercase().as_str()).unwrap();

            assert_eq!(result, *country);
        }
    }

    #[test]
    fn try_from_str_bad_iso3166() {
        let result = COUNTRIES.country_from_code("Booga");

        assert_eq!(
            result,
            Err("'Booga' is not a valid ISO3166 country code".to_string())
        );
    }

    #[test]
    fn from_index_happy_path() {
        for country in COUNTRIES.countries.iter() {
            let result = COUNTRIES.country_from_index(country.index).unwrap();

            assert_eq!(result, *country);
        }
    }

    #[test]
    fn try_from_index_bad_index() {
        let count = COUNTRIES.len();

        let result = COUNTRIES.country_from_index(4096usize).err().unwrap();

        assert_eq!(result, format!("There are only {} Countries; no Country is at index 4096", count));
    }
}
