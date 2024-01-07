// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::accountant::db_access_objects::payable_dao::PayableAccount;
use crate::accountant::payment_adjuster::diagnostics;
use crate::accountant::payment_adjuster::diagnostics::separately_defined_diagnostic_functions::{
    account_nominated_for_disqualification_diagnostics, exhausting_cw_balance_diagnostics,
    not_exhausting_cw_balance_diagnostics, possibly_outweighed_accounts_diagnostics,
    try_finding_an_account_to_disqualify_diagnostics,
};
use crate::accountant::payment_adjuster::log_fns::info_log_for_disqualified_account;
use crate::accountant::payment_adjuster::miscellaneous::data_structures::{
    AdjustedAccountBeforeFinalization, PercentageAccountInsignificance,
    ProposedAdjustmentResolution,
};
use crate::sub_lib::wallet::Wallet;
use itertools::Itertools;
use masq_lib::logger::Logger;
use std::cmp::Ordering;
use std::iter::successors;
use std::ops::Not;
use web3::types::U256;

const MAX_EXPONENT_FOR_10_IN_U128: u32 = 38;
const EMPIRIC_PRECISION_COEFFICIENT: usize = 8;
// Represents 50%
pub const ACCOUNT_INSIGNIFICANCE_BY_PERCENTAGE: PercentageAccountInsignificance =
    PercentageAccountInsignificance {
        multiplier: 1,
        divisor: 2,
    };

pub fn sum_as<N, T, F>(collection: &[T], arranger: F) -> N
where
    N: From<u128>,
    F: Fn(&T) -> u128,
{
    collection.iter().map(arranger).sum::<u128>().into()
}

pub fn weights_total(weights_and_accounts: &[(u128, PayableAccount)]) -> u128 {
    sum_as(weights_and_accounts, |(weight, _)| *weight)
}

pub fn drop_accounts_that_cannot_be_afforded_due_to_service_fee(
    weights_and_accounts_in_descending_order: Vec<(u128, PayableAccount)>,
    affordable_transaction_count: u16,
) -> Vec<(u128, PayableAccount)> {
    diagnostics!(
        "ACCOUNTS CUTBACK FOR TRANSACTION FEE",
        "keeping {} out of {} accounts",
        affordable_transaction_count,
        weights_and_accounts_in_descending_order.len()
    );
    weights_and_accounts_in_descending_order
        .into_iter()
        .take(affordable_transaction_count as usize)
        .collect()
}

// TODO U256 possible + test for extreme input fed??
pub fn compute_mul_coefficient_preventing_fractional_numbers(
    cw_masq_balance_minor: u128,
    account_weight: u128,
) -> U256 {
    let weight_digits_count = log_10(account_weight);
    let cw_balance_digits_count = log_10(cw_masq_balance_minor);
    let positive_difference = weight_digits_count.saturating_sub(cw_balance_digits_count);
    let safe_mul_coefficient = positive_difference + EMPIRIC_PRECISION_COEFFICIENT;
    U256::from(10)
        .checked_pow(safe_mul_coefficient.into())
        // .unwrap_or_else(|| 10_u128.pow(MAX_EXPONENT_FOR_10_IN_U128))
        //     TODO fix me in the test
        .unwrap_or_else(|| U256::MAX)
}

pub fn possibly_outweighed_accounts_fold_guts(
    (mut outweighed, mut passing_through): (
        Vec<AdjustedAccountBeforeFinalization>,
        Vec<AdjustedAccountBeforeFinalization>,
    ),
    current_account_info: AdjustedAccountBeforeFinalization,
) -> (
    Vec<AdjustedAccountBeforeFinalization>,
    Vec<AdjustedAccountBeforeFinalization>,
) {
    if current_account_info.proposed_adjusted_balance
        > current_account_info.original_account.balance_wei
    {
        possibly_outweighed_accounts_diagnostics(&current_account_info);

        let new_account_info = AdjustedAccountBeforeFinalization {
            proposed_adjusted_balance: current_account_info.original_account.balance_wei,
            ..current_account_info
        };

        outweighed.push(new_account_info);
        (outweighed, passing_through)
    } else {
        passing_through.push(current_account_info);
        (outweighed, passing_through)
    }
}

pub fn find_largest_nominated_account<'a>(
    accounts: &'a [&'a AdjustedAccountBeforeFinalization],
) -> &'a AdjustedAccountBeforeFinalization {
    let first_account = &accounts.first().expect("collection was empty");
    accounts
        .iter()
        .fold(**first_account, |largest_so_far, current| {
            match Ord::cmp(
                &current.original_account.balance_wei,
                &largest_so_far.original_account.balance_wei,
            ) {
                Ordering::Less => largest_so_far,
                Ordering::Greater => current,
                Ordering::Equal => {
                    // Greater value means younger
                    if current.original_account.last_paid_timestamp
                        > largest_so_far.original_account.last_paid_timestamp
                    {
                        current
                    } else {
                        largest_so_far
                    }
                }
            }
        })
}

pub fn exhaust_cw_till_the_last_drop(
    approved_accounts: Vec<AdjustedAccountBeforeFinalization>,
    original_cw_service_fee_balance_minor: u128,
) -> Vec<PayableAccount> {
    let adjusted_balances_total: u128 = sum_as(&approved_accounts, |account_info| {
        account_info.proposed_adjusted_balance
    });

    let cw_reminder = original_cw_service_fee_balance_minor
        .checked_sub(adjusted_balances_total)
        .unwrap_or_else(|| {
            panic!(
                "Remainder should've been a positive number but wasn't after {} - {}",
                original_cw_service_fee_balance_minor, adjusted_balances_total
            )
        });

    let init = CwExhaustingStatus::new(cw_reminder);
    approved_accounts
        .into_iter()
        .sorted_by(|info_a, info_b| {
            Ord::cmp(
                &info_a.proposed_adjusted_balance,
                &info_b.proposed_adjusted_balance,
            )
        })
        .fold(
            init,
            run_cw_exhausting_on_possibly_sub_optimal_account_balances,
        )
        .accounts_finalized_so_far
        .into_iter()
        .sorted_by(|account_a, account_b| Ord::cmp(&account_b.balance_wei, &account_a.balance_wei))
        .collect()
}

fn run_cw_exhausting_on_possibly_sub_optimal_account_balances(
    status: CwExhaustingStatus,
    non_finalized_account: AdjustedAccountBeforeFinalization,
) -> CwExhaustingStatus {
    if status.remainder != 0 {
        let balance_gap_minor = non_finalized_account
            .original_account
            .balance_wei
            .checked_sub(non_finalized_account.proposed_adjusted_balance)
            .unwrap_or_else(|| {
                panic!(
                    "Proposed balance should never be bigger than the original one. Proposed: \
                        {}, original: {}",
                    non_finalized_account.proposed_adjusted_balance,
                    non_finalized_account.original_account.balance_wei
                )
            });
        let possible_extra_addition = if balance_gap_minor < status.remainder {
            balance_gap_minor
        } else {
            status.remainder
        };

        exhausting_cw_balance_diagnostics(&non_finalized_account, possible_extra_addition);

        status.handle_balance_update_and_add(non_finalized_account, possible_extra_addition)
    } else {
        not_exhausting_cw_balance_diagnostics(&non_finalized_account);

        status.add(non_finalized_account)
    }
}

pub fn try_finding_an_account_to_disqualify_in_this_iteration(
    non_finalized_adjusted_accounts: &[AdjustedAccountBeforeFinalization],
    logger: &Logger,
) -> Option<Wallet> {
    let disqualification_suspected_accounts =
        list_accounts_nominated_for_disqualification(non_finalized_adjusted_accounts);
    disqualification_suspected_accounts
        .is_empty()
        .not()
        .then(|| {
            let account_to_disqualify = find_largest_nominated_account(
                &disqualification_suspected_accounts
            );

            let wallet = account_to_disqualify.original_account.wallet.clone();

            try_finding_an_account_to_disqualify_diagnostics(
                &disqualification_suspected_accounts,
                &wallet,
            );

            debug!(
                    logger,
                    "Found accounts {:?} whose proposed adjusted balances didn't get above the limit \
                    for disqualification. Chose the least desirable disqualified account as the one \
                    with the biggest balance, which is {}. To be thrown away in this iteration.",
                    disqualification_suspected_accounts,
                    wallet
                );

            info_log_for_disqualified_account(logger, account_to_disqualify);

            wallet
        })
}

pub fn finalize_collection(
    non_finalized_accounts: Vec<AdjustedAccountBeforeFinalization>,
    resolution: ProposedAdjustmentResolution,
) -> Vec<PayableAccount> {
    non_finalized_accounts
        .into_iter()
        .map(|account_info| PayableAccount::from((account_info, resolution)))
        .collect()
}

struct CwExhaustingStatus {
    remainder: u128,
    accounts_finalized_so_far: Vec<PayableAccount>,
}

impl CwExhaustingStatus {
    fn new(remainder: u128) -> Self {
        Self {
            remainder,
            accounts_finalized_so_far: vec![],
        }
    }

    fn handle_balance_update_and_add(
        mut self,
        mut non_finalized_account_info: AdjustedAccountBeforeFinalization,
        possible_extra_addition: u128,
    ) -> Self {
        let corrected_adjusted_account_before_finalization = {
            non_finalized_account_info.proposed_adjusted_balance += possible_extra_addition;
            non_finalized_account_info
        };
        self.remainder = self
            .remainder
            .checked_sub(possible_extra_addition)
            .expect("we hit zero");
        self.add(corrected_adjusted_account_before_finalization)
    }

    fn add(mut self, non_finalized_account_info: AdjustedAccountBeforeFinalization) -> Self {
        let finalized_account = PayableAccount::from((
            non_finalized_account_info,
            ProposedAdjustmentResolution::Finalize,
        ));
        self.accounts_finalized_so_far.push(finalized_account);
        self
    }
}

pub fn sort_in_descendant_order_by_weights(
    unsorted: impl Iterator<Item = (u128, PayableAccount)>,
) -> Vec<(u128, PayableAccount)> {
    unsorted
        .sorted_by(|(weight_a, _), (weight_b, _)| Ord::cmp(weight_b, weight_a))
        .collect()
}

pub fn isolate_accounts_from_weights(
    weights_and_accounts: Vec<(u128, PayableAccount)>,
) -> Vec<PayableAccount> {
    weights_and_accounts
        .into_iter()
        .map(|(_, account)| account)
        .collect()
}

pub fn list_accounts_nominated_for_disqualification(
    non_finalized_adjusted_accounts: &[AdjustedAccountBeforeFinalization],
) -> Vec<&AdjustedAccountBeforeFinalization> {
    non_finalized_adjusted_accounts
        .iter()
        .flat_map(|account_info| {
            let disqualification_edge =
                calculate_disqualification_edge(account_info.original_account.balance_wei);
            let proposed_adjusted_balance = account_info.proposed_adjusted_balance;

            if proposed_adjusted_balance <= disqualification_edge {
                account_nominated_for_disqualification_diagnostics(
                    account_info,
                    proposed_adjusted_balance,
                    disqualification_edge,
                );

                Some(account_info)
            } else {
                None
            }
        })
        .collect()
}

pub fn calculate_disqualification_edge(account_balance: u128) -> u128 {
    (ACCOUNT_INSIGNIFICANCE_BY_PERCENTAGE.multiplier * account_balance)
        / ACCOUNT_INSIGNIFICANCE_BY_PERCENTAGE.divisor
}

// Replace with std lib method log10() for u128 which will be introduced by
// Rust 1.67.0; this was written using 1.63.0
pub fn log_10(num: u128) -> usize {
    successors(Some(num), |&n| (n >= 10).then(|| n / 10)).count()
}

const fn num_bits<N>() -> usize {
    std::mem::size_of::<N>() * 8
}

pub fn log_2(x: u128) -> u32 {
    if x < 1 {
        return 0;
    }
    num_bits::<i128>() as u32 - x.leading_zeros() - 1
}

pub fn x_or_1(x: u128) -> u128 {
    if x == 0 {
        1
    } else {
        x
    }
}

type NonFinalizedAccountAndAdjustmentResolution = (
    AdjustedAccountBeforeFinalization,
    ProposedAdjustmentResolution,
);

impl From<NonFinalizedAccountAndAdjustmentResolution> for PayableAccount {
    fn from(
        (account_info, proposed_adjustment_resolution): NonFinalizedAccountAndAdjustmentResolution,
    ) -> Self {
        match proposed_adjustment_resolution {
            ProposedAdjustmentResolution::Finalize => PayableAccount {
                balance_wei: account_info.proposed_adjusted_balance,
                ..account_info.original_account
            },
            ProposedAdjustmentResolution::Revert => account_info.original_account,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::accountant::db_access_objects::payable_dao::PayableAccount;
    use crate::accountant::payment_adjuster::miscellaneous::data_structures::{
        AdjustedAccountBeforeFinalization, PercentageAccountInsignificance,
    };
    use crate::accountant::payment_adjuster::miscellaneous::helper_functions::{
        calculate_disqualification_edge, compute_mul_coefficient_preventing_fractional_numbers,
        exhaust_cw_till_the_last_drop, find_largest_nominated_account,
        list_accounts_nominated_for_disqualification, log_10, log_2,
        possibly_outweighed_accounts_fold_guts,
        try_finding_an_account_to_disqualify_in_this_iteration, weights_total, CwExhaustingStatus,
        ACCOUNT_INSIGNIFICANCE_BY_PERCENTAGE, EMPIRIC_PRECISION_COEFFICIENT,
        MAX_EXPONENT_FOR_10_IN_U128,
    };
    use crate::accountant::payment_adjuster::test_utils::{
        make_extreme_accounts, make_initialized_subject, MAX_POSSIBLE_MASQ_BALANCE_IN_MINOR,
    };
    use crate::accountant::test_utils::make_payable_account;
    use crate::sub_lib::wallet::Wallet;
    use crate::test_utils::make_wallet;
    use itertools::{Either, Itertools};
    use masq_lib::logger::Logger;
    use std::time::{Duration, SystemTime};
    use web3::types::U256;

    #[test]
    fn constants_are_correct() {
        assert_eq!(MAX_EXPONENT_FOR_10_IN_U128, 38);
        assert_eq!(EMPIRIC_PRECISION_COEFFICIENT, 8);
        assert_eq!(
            ACCOUNT_INSIGNIFICANCE_BY_PERCENTAGE,
            PercentageAccountInsignificance {
                multiplier: 1,
                divisor: 2
            }
        );
    }

    #[test]
    fn log_10_works() {
        [
            (4_565_u128, 4),
            (1_666_777, 7),
            (3, 1),
            (123, 3),
            (111_111_111_111_111_111, 18),
        ]
        .into_iter()
        .for_each(|(num, expected_result)| assert_eq!(log_10(num), expected_result))
    }

    #[test]
    fn log_2_works() {
        [
            (1, 0),
            (2, 1),
            (3, 1),
            (4, 2),
            (8192, 13),
            (18446744073709551616, 64),
            (1267650600228229401496703205376, 100),
            (170141183460469231731687303715884105728, 127),
        ]
        .into_iter()
        .for_each(|(num, expected_result)| assert_eq!(log_2(num), expected_result))
    }

    #[test]
    fn log_2_for_0() {
        let result = log_2(0);

        assert_eq!(result, 0)
    }

    #[test]
    fn multiplication_coefficient_can_give_numbers_preventing_fractional_numbers() {
        let final_weight = 5_000_000_000_000_u128;
        let cw_balances = vec![
            222_222_222_222_u128,
            100_000,
            123_456_789,
            5_555_000_000_000,
            5_000_555_000_000_000,
            1_000_000_000_000_000_000, //1 MASQ
        ];

        let result = cw_balances
            .clone()
            .into_iter()
            .map(|cw_balance| {
                compute_mul_coefficient_preventing_fractional_numbers(cw_balance, final_weight)
            })
            .collect::<Vec<U256>>();

        let expected_result = vec![
            1_000_000_000_u128,
            1_000_000_000_000_000,
            1_000_000_000_000,
            100_000_000,
            100_000_000,
            100_000_000,
        ]
        .into_iter()
        .map(U256::from)
        .collect::<Vec<U256>>();
        assert_eq!(result, expected_result)
    }

    #[test]
    fn multiplication_coefficient_extreme_feeding_and_safety_ceiling() {
        // We cannot say by heart which of the evaluated weights from
        // these parameters below will be bigger than another and therefore
        // we cannot line them up in an order
        let accounts_as_months_and_balances = vec![
            (1, *MAX_POSSIBLE_MASQ_BALANCE_IN_MINOR),
            (5, 10_u128.pow(18)),
            (12, 10_u128.pow(18)),
            (120, 10_u128.pow(20)),
            (600, *MAX_POSSIBLE_MASQ_BALANCE_IN_MINOR),
            (1200, *MAX_POSSIBLE_MASQ_BALANCE_IN_MINOR),
            (1200, *MAX_POSSIBLE_MASQ_BALANCE_IN_MINOR * 1000),
        ];
        let (accounts_with_their_weights, reserved_initial_accounts_order_according_to_wallets) =
            get_extreme_weights_and_initial_accounts_order(accounts_as_months_and_balances);
        let cw_balance_in_minor = 1;

        let results = accounts_with_their_weights
            .into_iter()
            .map(|(weight, account)| {
                // Scenario simplification: we assume there is always just one account to process in a time
                let computed_coefficient = compute_mul_coefficient_preventing_fractional_numbers(
                    cw_balance_in_minor,
                    weight,
                );
                (computed_coefficient, account.wallet, weight)
            })
            .collect::<Vec<(U256, Wallet, u128)>>();

        let reserved_initial_accounts_order_according_to_wallets_iter =
            reserved_initial_accounts_order_according_to_wallets
                .iter()
                .enumerate();
        let mul_coefficients_and_weights_in_the_same_order_as_original_inputs = results
            .into_iter()
            .map(|(computed_coefficient, account_wallet, account_weight)| {
                let (idx, _) = reserved_initial_accounts_order_according_to_wallets_iter
                    .clone()
                    .find(|(_, wallet_ordered)| wallet_ordered == &&account_wallet)
                    .unwrap();
                (idx, computed_coefficient, account_weight)
            })
            .sorted_by(|(idx_a, _, _), (idx_b, _, _)| Ord::cmp(&idx_b, &idx_a))
            .map(|(_, coefficient, weight)| (coefficient, weight))
            .collect::<Vec<(U256, u128)>>();
        let templates_for_coefficients = vec![
            100000000000000000000000000000000000000_u128,
            100000000000000000000000000000000000,
            100000000000000000000000000000000000,
            100000000000000000000000000000000,
            10000000000000000000000000000000,
            10000000000000000000000000000000,
            100000000000000000000000000000000000,
        ]
        .into_iter()
        .map(U256::from)
        .collect::<Vec<U256>>();
        // I was trying to write these assertions so that it wouldn't require us to rewrite
        // the expected values everytime someone pokes into the formulas.
        check_relation_to_computed_weight_fairly_but_with_enough_benevolence(
            &mul_coefficients_and_weights_in_the_same_order_as_original_inputs,
        );
        compare_coefficients_to_templates(
            &mul_coefficients_and_weights_in_the_same_order_as_original_inputs,
            &templates_for_coefficients,
        );
    }

    fn check_relation_to_computed_weight_fairly_but_with_enough_benevolence(
        output: &[(U256, u128)],
    ) {
        output.iter().for_each(|(coefficient, corresponding_weight)| {
            let coefficient_num_decimal_length = log_10(coefficient.as_u128());
            let weight_decimal_length = log_10(*corresponding_weight);
            assert_eq!(coefficient_num_decimal_length, weight_decimal_length + EMPIRIC_PRECISION_COEFFICIENT,
                       "coefficient with bad safety margin; should be {} but was {}, as one of this set {:?}",
                       coefficient_num_decimal_length,
                       weight_decimal_length + EMPIRIC_PRECISION_COEFFICIENT,
                       output
            );

            let expected_division_by_10_if_wrong = 10_u128.pow(coefficient_num_decimal_length as u32 - 1);
            let experiment_result = corresponding_weight / 10;
            match experiment_result == expected_division_by_10_if_wrong {
                false => (),
                true => match corresponding_weight % 10 {
                    0 => panic!("the weight is a pure power of ten, such a suspicious result, \
                                check it in {:?}", output),
                    _ => ()
                }
            }
        })
    }

    fn compare_coefficients_to_templates(outputs: &[(U256, u128)], templates: &[U256]) {
        assert_eq!(
            outputs.len(),
            templates.len(),
            "count of actual values {:?} and templates don't match {:?}",
            outputs,
            templates
        );
        outputs
            .iter()
            .zip(templates.iter())
            .for_each(|((actual_coeff, _), expected_coeff)| {
                assert_eq!(
                    actual_coeff, expected_coeff,
                    "actual coefficient {} does not match the expected one {} in the full set {:?}",
                    actual_coeff, expected_coeff, outputs
                )
            })
    }

    fn make_non_finalized_adjusted_accounts(
        payable_accounts: Vec<(PayableAccount, u128)>,
    ) -> Vec<AdjustedAccountBeforeFinalization> {
        payable_accounts
            .into_iter()
            .map(|(original_account, proposed_adjusted_balance)| {
                AdjustedAccountBeforeFinalization::new(original_account, proposed_adjusted_balance)
            })
            .collect()
    }

    fn by_reference(
        adjusted_accounts: &[AdjustedAccountBeforeFinalization],
    ) -> Vec<&AdjustedAccountBeforeFinalization> {
        adjusted_accounts.iter().collect()
    }

    #[test]
    fn calculate_disqualification_edge_works() {
        let mut account = make_payable_account(111);
        account.balance_wei = 300_000_000;

        let result = calculate_disqualification_edge(account.balance_wei);

        assert_eq!(result, calculate_disqualification_edge(account.balance_wei))
    }

    #[test]
    fn find_largest_nominated_account_works_for_unequal_balances() {
        let account_1 = make_payable_account(111);
        let account_2 = make_payable_account(333);
        let account_3 = make_payable_account(222);
        let account_4 = make_payable_account(332);
        let accounts = make_non_finalized_adjusted_accounts(vec![
            (account_1, 100_000_000),
            (account_2.clone(), 222_000_000),
            (account_3, 300_000_000),
            (account_4, 400_000_000),
        ]);
        let referenced_non_finalized_accounts = by_reference(&accounts);

        let result = find_largest_nominated_account(&referenced_non_finalized_accounts);

        assert_eq!(
            result,
            &AdjustedAccountBeforeFinalization {
                original_account: account_2,
                proposed_adjusted_balance: 222_000_000
            }
        )
    }

    #[test]
    fn find_largest_nominated_account_for_equal_balances_chooses_younger_account() {
        // We prefer to keep the older and so more important accounts in the game
        let now = SystemTime::now();
        let account_1 = make_payable_account(111);
        let mut account_2 = make_payable_account(333);
        account_2.last_paid_timestamp = now.checked_sub(Duration::from_secs(10000)).unwrap();
        let account_3 = make_payable_account(222);
        let mut account_4 = make_payable_account(333);
        account_4.last_paid_timestamp = now.checked_sub(Duration::from_secs(9999)).unwrap();
        let accounts = make_non_finalized_adjusted_accounts(vec![
            (account_1, 100_000_000),
            (account_2, 200_000_000),
            (account_3, 300_000_000),
            (account_4.clone(), 444_000_000),
        ]);
        let referenced_non_finalized_accounts = by_reference(&accounts);

        let result = find_largest_nominated_account(&referenced_non_finalized_accounts);

        assert_eq!(
            result,
            &AdjustedAccountBeforeFinalization {
                original_account: account_4,
                proposed_adjusted_balance: 444_000_000
            }
        )
    }

    #[test]
    fn find_largest_nominated_account_for_accounts_with_equal_balances_as_well_as_age() {
        let account = make_payable_account(111);
        let wallet_1 = make_wallet("abc");
        let wallet_2 = make_wallet("def");
        let mut account_1 = account.clone();
        account_1.wallet = wallet_1.clone();
        let mut account_2 = account;
        account_2.wallet = wallet_2;
        let accounts = make_non_finalized_adjusted_accounts(vec![
            (account_1.clone(), 100_111_000),
            (account_2, 200_000_000),
        ]);
        let referenced_non_finalized_accounts = by_reference(&accounts);

        let result = find_largest_nominated_account(&referenced_non_finalized_accounts);

        assert_eq!(
            result,
            &AdjustedAccountBeforeFinalization {
                original_account: account_1,
                proposed_adjusted_balance: 100_111_000
            }
        )
    }

    #[test]
    fn accounts_with_original_balances_equal_to_the_proposed_ones_are_not_outweighed() {
        let account_info = AdjustedAccountBeforeFinalization {
            original_account: PayableAccount {
                wallet: make_wallet("blah"),
                balance_wei: 9_000_000_000,
                last_paid_timestamp: SystemTime::now(),
                pending_payable_opt: None,
            },
            proposed_adjusted_balance: 9_000_000_000,
        };
        let init = (vec![], vec![]);

        let (outweighed, ok) = possibly_outweighed_accounts_fold_guts(init, account_info.clone());

        assert_eq!(outweighed, vec![]);
        assert_eq!(ok, vec![account_info])
    }

    #[test]
    fn picks_only_the_biggest_and_youngest_disqualified_account_in_one_iteration() {
        let test_name = "picks_only_the_biggest_and_youngest_disqualified_account_in_one_iteration";
        let now = SystemTime::now();
        let cw_masq_balance = 2_000_000_000_000;
        let logger = Logger::new(test_name);
        let subject = make_initialized_subject(now, Some(cw_masq_balance), None);
        let wallet_1 = make_wallet("abc");
        let account_1 = PayableAccount {
            wallet: wallet_1.clone(),
            balance_wei: 700_000_000_000,
            last_paid_timestamp: now.checked_sub(Duration::from_secs(100_000)).unwrap(),
            pending_payable_opt: None,
        };
        let wallet_2 = make_wallet("def");
        let account_2 = PayableAccount {
            wallet: wallet_2.clone(),
            balance_wei: 333_000_000_000,
            last_paid_timestamp: now.checked_sub(Duration::from_secs(1_000_000)).unwrap(),
            pending_payable_opt: None,
        };
        let wallet_3 = make_wallet("ghi");
        let account_3 = PayableAccount {
            wallet: wallet_3.clone(),
            balance_wei: 700_000_000_000,
            last_paid_timestamp: now.checked_sub(Duration::from_secs(99_999)).unwrap(),
            pending_payable_opt: None,
        };
        let wallet_4 = make_wallet("jkl");
        let account_4 = PayableAccount {
            wallet: wallet_4.clone(),
            balance_wei: 120_000_000_000,
            last_paid_timestamp: now.checked_sub(Duration::from_secs(7_000)).unwrap(),
            pending_payable_opt: None,
        };
        let weights_and_accounts = subject
            .calculate_weights_for_accounts(vec![account_1, account_2, account_3, account_4]);
        let weights_total = weights_total(&weights_and_accounts);
        let non_finalized_adjusted_accounts = subject
            .compute_adjusted_but_non_finalized_accounts(weights_and_accounts, weights_total);

        let result = try_finding_an_account_to_disqualify_in_this_iteration(
            &non_finalized_adjusted_accounts,
            &logger,
        );

        assert_eq!(result, Some(wallet_3));
    }

    fn make_non_finalized_adjusted_account(
        wallet: &Wallet,
        original_balance: u128,
        proposed_adjusted_balance: u128,
    ) -> AdjustedAccountBeforeFinalization {
        AdjustedAccountBeforeFinalization {
            original_account: PayableAccount {
                wallet: wallet.clone(),
                balance_wei: original_balance,
                last_paid_timestamp: SystemTime::now(),
                pending_payable_opt: None,
            },
            proposed_adjusted_balance,
        }
    }

    fn assert_payable_accounts_after_adjustment_finalization(
        actual_accounts: Vec<PayableAccount>,
        expected_account_parts: Vec<(Wallet, u128)>,
    ) {
        let actual_accounts_simplified_and_sorted = actual_accounts
            .into_iter()
            .map(|account| (account.wallet.address(), account.balance_wei))
            .sorted()
            .collect::<Vec<_>>();
        let expected_account_parts_sorted = expected_account_parts
            .into_iter()
            .map(|(expected_wallet, expected_balance)| {
                (expected_wallet.address(), expected_balance)
            })
            .sorted()
            .collect::<Vec<_>>();
        assert_eq!(
            actual_accounts_simplified_and_sorted,
            expected_account_parts_sorted
        )
    }

    #[test]
    fn exhaustive_status_is_constructed_properly() {
        let cw_balance_remainder = 45678;

        let result = CwExhaustingStatus::new(cw_balance_remainder);

        assert_eq!(result.remainder, cw_balance_remainder);
        assert_eq!(result.accounts_finalized_so_far, vec![])
    }

    #[test]
    fn three_non_exhaustive_accounts_all_refilled() {
        // A seemingly irrational situation, this can happen when some of those
        // originally qualified payables could get disqualified. Those would free some
        // means that could be used for the other accounts.
        // In the end, we have a final set with sub-optimal balances, despite
        // the unallocated cw balance is larger than the entire sum of the original balances
        // for this few resulting accounts.
        // We can pay every account fully, so, why did we need to call the PaymentAdjuster
        // in first place?
        // The detail is in the loss of some accounts, allowing to pay more for the others.
        let wallet_1 = make_wallet("abc");
        let original_requested_balance_1 = 45_000_000_000;
        let proposed_adjusted_balance_1 = 44_999_897_000;
        let wallet_2 = make_wallet("def");
        let original_requested_balance_2 = 33_500_000_000;
        let proposed_adjusted_balance_2 = 33_487_999_999;
        let wallet_3 = make_wallet("ghi");
        let original_requested_balance_3 = 41_000_000;
        let proposed_adjusted_balance_3 = 40_980_000;
        let original_cw_balance = original_requested_balance_1
            + original_requested_balance_2
            + original_requested_balance_3
            + 5000;
        let non_finalized_adjusted_accounts = vec![
            make_non_finalized_adjusted_account(
                &wallet_1,
                original_requested_balance_1,
                proposed_adjusted_balance_1,
            ),
            make_non_finalized_adjusted_account(
                &wallet_2,
                original_requested_balance_2,
                proposed_adjusted_balance_2,
            ),
            make_non_finalized_adjusted_account(
                &wallet_3,
                original_requested_balance_3,
                proposed_adjusted_balance_3,
            ),
        ];

        let result =
            exhaust_cw_till_the_last_drop(non_finalized_adjusted_accounts, original_cw_balance);

        let expected_resulted_balances = vec![
            (wallet_1, original_requested_balance_1),
            (wallet_2, original_requested_balance_2),
            (wallet_3, original_requested_balance_3),
        ];
        assert_payable_accounts_after_adjustment_finalization(result, expected_resulted_balances)
    }

    #[test]
    fn three_non_exhaustive_accounts_with_one_completely_refilled_one_partly_one_not_at_all() {
        // The smallest proposed adjusted balance gets refilled first, and then gradually on...
        let wallet_1 = make_wallet("abc");
        let original_requested_balance_1 = 54_000_000_000;
        let proposed_adjusted_balance_1 = 53_898_000_000;
        let wallet_2 = make_wallet("def");
        let original_requested_balance_2 = 33_500_000_000;
        let proposed_adjusted_balance_2 = 33_487_999_999;
        let wallet_3 = make_wallet("ghi");
        let original_requested_balance_3 = 41_000_000;
        let proposed_adjusted_balance_3 = 40_980_000;
        let original_cw_balance = original_requested_balance_2
            + original_requested_balance_3
            + proposed_adjusted_balance_1
            - 2_000_000;
        let non_finalized_adjusted_accounts = vec![
            make_non_finalized_adjusted_account(
                &wallet_1,
                original_requested_balance_1,
                proposed_adjusted_balance_1,
            ),
            make_non_finalized_adjusted_account(
                &wallet_2,
                original_requested_balance_2,
                proposed_adjusted_balance_2,
            ),
            make_non_finalized_adjusted_account(
                &wallet_3,
                original_requested_balance_3,
                proposed_adjusted_balance_3,
            ),
        ];

        let result =
            exhaust_cw_till_the_last_drop(non_finalized_adjusted_accounts, original_cw_balance);

        let expected_resulted_balances = vec![
            (wallet_1, proposed_adjusted_balance_1),
            (wallet_2, 33_498_000_000),
            (wallet_3, original_requested_balance_3),
        ];
        let check_sum: u128 = expected_resulted_balances
            .iter()
            .map(|(_, balance)| balance)
            .sum();
        assert_payable_accounts_after_adjustment_finalization(result, expected_resulted_balances);
        assert_eq!(check_sum, original_cw_balance)
    }

    #[test]
    fn list_accounts_nominated_for_disqualification_uses_the_right_manifest_const() {
        let account_balance = 1_000_000;
        let prepare_account = |n: u64| {
            let mut account = make_payable_account(n);
            account.balance_wei = account_balance;
            account
        };
        let payable_account_1 = prepare_account(1);
        let payable_account_2 = prepare_account(2);
        let payable_account_3 = prepare_account(3);
        let edge = calculate_disqualification_edge(account_balance);
        let proposed_ok_balance = edge + 1;
        let account_info_1 =
            AdjustedAccountBeforeFinalization::new(payable_account_1, proposed_ok_balance);
        let proposed_bad_balance_because_equal = edge;
        let account_info_2 = AdjustedAccountBeforeFinalization::new(
            payable_account_2,
            proposed_bad_balance_because_equal,
        );
        let proposed_bad_balance_because_smaller = edge - 1;
        let account_info_3 = AdjustedAccountBeforeFinalization::new(
            payable_account_3,
            proposed_bad_balance_because_smaller,
        );
        let non_finalized_adjusted_accounts = vec![
            account_info_1,
            account_info_2.clone(),
            account_info_3.clone(),
        ];

        let result = list_accounts_nominated_for_disqualification(&non_finalized_adjusted_accounts);

        let expected_disqualified_accounts = vec![&account_info_2, &account_info_3];
        assert_eq!(result, expected_disqualified_accounts)
    }

    fn get_extreme_weights_and_initial_accounts_order(
        months_of_debt_and_balances: Vec<(usize, u128)>,
    ) -> (Vec<(u128, PayableAccount)>, Vec<Wallet>) {
        let now = SystemTime::now();
        let accounts = make_extreme_accounts(Either::Right(months_of_debt_and_balances), now);
        let wallets_in_order = accounts
            .iter()
            .map(|account| account.wallet.clone())
            .collect();
        let subject = make_initialized_subject(now, None, None);
        // The initial order is remembered because when the weight are applied the collection the collection
        // also gets sorted and will not necessarily have to match the initial order
        let weights_and_accounts = subject.calculate_weights_for_accounts(accounts);
        (weights_and_accounts, wallets_in_order)
    }
}
