// Copyright (c) 2023, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

pub mod accounts_abstraction;

use crate::accountant::payment_adjuster::disqualification_arbiter::DisqualificationArbiter;
use crate::accountant::payment_adjuster::logging_and_diagnostics::log_functions::{
    log_adjustment_by_service_fee_is_required, log_insufficient_transaction_fee_balance,
    log_transaction_fee_adjustment_ok_but_by_service_fee_undoable,
};
use crate::accountant::payment_adjuster::miscellaneous::data_structures::{
    AdjustmentPossibilityErrorBuilder, TransactionCountsBy16bits, TransactionFeeLimitation,
    TransactionFeePastCheckContext, WeightedPayable,
};
use crate::accountant::payment_adjuster::miscellaneous::helper_functions::{
    find_smallest_u128, sum_as,
};
use crate::accountant::payment_adjuster::preparatory_analyser::accounts_abstraction::{
    BalanceProvidingAccount, DisqualificationAnalysableAccount,
    DisqualificationLimitProvidingAccount,
};
use crate::accountant::payment_adjuster::{Adjustment, AdjustmentAnalysis, PaymentAdjusterError};
use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
use crate::accountant::{AnalyzedPayableAccount, QualifiedPayableAccount};
use ethereum_types::U256;
use itertools::Either;
use masq_lib::logger::Logger;

pub struct PreparatoryAnalyzer {}

impl PreparatoryAnalyzer {
    pub fn new() -> Self {
        Self {}
    }

    pub fn analyze_accounts(
        &self,
        agent: &dyn BlockchainAgent,
        disqualification_arbiter: &DisqualificationArbiter,
        qualified_payables: Vec<QualifiedPayableAccount>,
        logger: &Logger,
    ) -> Result<Either<Vec<QualifiedPayableAccount>, AdjustmentAnalysis>, PaymentAdjusterError>
    {
        let number_of_counts = qualified_payables.len();
        let cw_transaction_fee_balance_minor = agent.transaction_fee_balance_minor();
        let per_transaction_requirement_minor =
            agent.estimated_transaction_fee_per_transaction_minor();

        let transaction_fee_limitation_opt = self
            .determine_transaction_count_limit_by_transaction_fee(
                cw_transaction_fee_balance_minor,
                per_transaction_requirement_minor,
                number_of_counts,
                logger,
            )?;

        let cw_service_fee_balance_minor = agent.service_fee_balance_minor();
        let is_service_fee_adjustment_needed = Self::is_service_fee_adjustment_needed(
            &qualified_payables,
            cw_service_fee_balance_minor,
            logger,
        );

        if transaction_fee_limitation_opt.is_none() && !is_service_fee_adjustment_needed {
            Ok(Either::Left(qualified_payables))
        } else {
            let prepared_accounts = Self::pre_process_accounts_for_adjustments(
                qualified_payables,
                disqualification_arbiter,
            );
            if is_service_fee_adjustment_needed {
                let error_builder = AdjustmentPossibilityErrorBuilder::default().context(
                    TransactionFeePastCheckContext::initial_check_done(
                        transaction_fee_limitation_opt,
                    ),
                );

                Self::check_adjustment_possibility(
                    &prepared_accounts,
                    cw_service_fee_balance_minor,
                    error_builder,
                )?
            };
            let adjustment = match transaction_fee_limitation_opt {
                None => Adjustment::ByServiceFee,
                Some(limitation) => {
                    let affordable_transaction_count = limitation.count_limit;
                    Adjustment::TransactionFeeInPriority {
                        affordable_transaction_count,
                    }
                }
            };
            Ok(Either::Right(AdjustmentAnalysis::new(
                adjustment,
                prepared_accounts,
            )))
        }
    }

    pub fn recheck_if_service_fee_adjustment_is_needed(
        &self,
        weighted_accounts: &[WeightedPayable],
        cw_service_fee_balance_minor: u128,
        error_builder: AdjustmentPossibilityErrorBuilder,
        logger: &Logger,
    ) -> Result<bool, PaymentAdjusterError> {
        if Self::is_service_fee_adjustment_needed(
            weighted_accounts,
            cw_service_fee_balance_minor,
            logger,
        ) {
            if let Err(e) = Self::check_adjustment_possibility(
                weighted_accounts,
                cw_service_fee_balance_minor,
                error_builder,
            ) {
                log_transaction_fee_adjustment_ok_but_by_service_fee_undoable(logger);
                Err(e)
            } else {
                Ok(true)
            }
        } else {
            Ok(false)
        }
    }

    fn determine_transaction_count_limit_by_transaction_fee(
        &self,
        cw_transaction_fee_balance_minor: U256,
        per_transaction_requirement_minor: u128,
        number_of_qualified_accounts: usize,
        logger: &Logger,
    ) -> Result<Option<TransactionFeeLimitation>, PaymentAdjusterError> {
        let verified_tx_counts = Self::transaction_counts_verification(
            cw_transaction_fee_balance_minor,
            per_transaction_requirement_minor,
            number_of_qualified_accounts,
        );

        let max_tx_count_we_can_afford_u16 = verified_tx_counts.affordable;
        let required_tx_count_u16 = verified_tx_counts.required;

        if max_tx_count_we_can_afford_u16 == 0 {
            Err(
                PaymentAdjusterError::NotEnoughTransactionFeeBalanceForSingleTx {
                    number_of_accounts: number_of_qualified_accounts,
                    per_transaction_requirement_minor,
                    cw_transaction_fee_balance_minor,
                },
            )
        } else if max_tx_count_we_can_afford_u16 >= required_tx_count_u16 {
            Ok(None)
        } else {
            log_insufficient_transaction_fee_balance(
                logger,
                required_tx_count_u16,
                cw_transaction_fee_balance_minor,
                max_tx_count_we_can_afford_u16,
            );
            let transaction_fee_limitation_opt = TransactionFeeLimitation::new(
                max_tx_count_we_can_afford_u16,
                cw_transaction_fee_balance_minor.as_u128(),
                per_transaction_requirement_minor,
            );
            Ok(Some(transaction_fee_limitation_opt))
        }
    }

    fn transaction_counts_verification(
        cw_transaction_fee_balance_minor: U256,
        txn_fee_required_per_txn_minor: u128,
        number_of_qualified_accounts: usize,
    ) -> TransactionCountsBy16bits {
        let max_possible_tx_count_u256 =
            cw_transaction_fee_balance_minor / U256::from(txn_fee_required_per_txn_minor);

        TransactionCountsBy16bits::new(max_possible_tx_count_u256, number_of_qualified_accounts)
    }

    fn check_adjustment_possibility<AnalyzableAccounts>(
        prepared_accounts: &[AnalyzableAccounts],
        cw_service_fee_balance_minor: u128,
        error_builder: AdjustmentPossibilityErrorBuilder,
    ) -> Result<(), PaymentAdjusterError>
    where
        AnalyzableAccounts: DisqualificationLimitProvidingAccount + BalanceProvidingAccount,
    {
        let lowest_disqualification_limit =
            Self::find_lowest_disqualification_limit(&prepared_accounts);

        // We cannot do much in this area but stepping in if the cw balance is zero or nearly
        // zero with the assumption that the debt with the lowest disqualification limit in
        // the set fits in the available balance. If it doesn't, we're not going to bother
        // the payment adjuster by that work, so it'll abort and no payments will come out.
        if lowest_disqualification_limit <= cw_service_fee_balance_minor {
            Ok(())
        } else {
            let analyzed_accounts_count = prepared_accounts.len();
            let required_service_fee_total =
                Self::compute_total_of_service_fee_required(prepared_accounts);
            let err = error_builder
                .all_time_supplied_parameters(
                    analyzed_accounts_count,
                    required_service_fee_total,
                    cw_service_fee_balance_minor,
                )
                .build();
            Err(err)
        }
    }

    fn pre_process_accounts_for_adjustments(
        accounts: Vec<QualifiedPayableAccount>,
        disqualification_arbiter: &DisqualificationArbiter,
    ) -> Vec<AnalyzedPayableAccount> {
        accounts
            .into_iter()
            .map(|account| {
                let disqualification_limit =
                    disqualification_arbiter.calculate_disqualification_edge(&account);
                AnalyzedPayableAccount::new(account, disqualification_limit)
            })
            .collect()
    }

    fn compute_total_of_service_fee_required<Account>(payables: &[Account]) -> u128
    where
        Account: BalanceProvidingAccount,
    {
        sum_as(payables, |account| account.balance_minor())
    }

    fn is_service_fee_adjustment_needed<Account>(
        qualified_payables: &[Account],
        cw_service_fee_balance_minor: u128,
        logger: &Logger,
    ) -> bool
    where
        Account: BalanceProvidingAccount,
    {
        let service_fee_totally_required_minor =
            Self::compute_total_of_service_fee_required(qualified_payables);
        (service_fee_totally_required_minor > cw_service_fee_balance_minor)
            .then(|| {
                log_adjustment_by_service_fee_is_required(
                    logger,
                    service_fee_totally_required_minor,
                    cw_service_fee_balance_minor,
                )
            })
            .is_some()
    }

    fn find_lowest_disqualification_limit<Account>(accounts: &[Account]) -> u128
    where
        Account: DisqualificationLimitProvidingAccount,
    {
        find_smallest_u128(
            &accounts
                .iter()
                .map(|account| account.disqualification_limit())
                .collect::<Vec<u128>>(),
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::accountant::payment_adjuster::disqualification_arbiter::{
        DisqualificationArbiter, DisqualificationGauge,
    };
    use crate::accountant::payment_adjuster::miscellaneous::data_structures::{
        AdjustmentPossibilityErrorBuilder, TransactionFeeLimitation, TransactionFeePastCheckContext,
    };
    use crate::accountant::payment_adjuster::miscellaneous::helper_functions::sum_as;
    use crate::accountant::payment_adjuster::preparatory_analyser::PreparatoryAnalyzer;
    use crate::accountant::payment_adjuster::test_utils::{
        make_weighed_account, multiple_by_billion, DisqualificationGaugeMock,
    };
    use crate::accountant::payment_adjuster::{
        Adjustment, AdjustmentAnalysis, PaymentAdjusterError,
    };
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::test_utils::BlockchainAgentMock;
    use crate::accountant::test_utils::{
        make_analyzed_account, make_non_guaranteed_qualified_payable,
    };
    use crate::accountant::QualifiedPayableAccount;
    use itertools::Either;
    use masq_lib::logger::Logger;
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use std::sync::{Arc, Mutex};
    use thousands::Separable;
    use web3::types::U256;

    fn test_adjustment_possibility_nearly_rejected(
        test_name: &str,
        disqualification_gauge: DisqualificationGaugeMock,
        original_accounts: [QualifiedPayableAccount; 2],
        cw_service_fee_balance: u128,
    ) {
        init_test_logging();
        let determine_limit_params_arc = Arc::new(Mutex::new(vec![]));
        let disqualification_gauge = double_mock_results_queue(disqualification_gauge)
            .determine_limit_params(&determine_limit_params_arc);
        let total_amount_required: u128 = sum_as(original_accounts.as_slice(), |account| {
            account.bare_account.balance_wei
        });
        let disqualification_arbiter =
            DisqualificationArbiter::new(Box::new(disqualification_gauge));
        let subject = PreparatoryAnalyzer {};
        let blockchain_agent = BlockchainAgentMock::default()
            .transaction_fee_balance_minor_result(U256::MAX)
            .estimated_transaction_fee_per_transaction_minor_result(123456)
            .service_fee_balance_minor_result(cw_service_fee_balance);

        let result = subject.analyze_accounts(
            &blockchain_agent,
            &disqualification_arbiter,
            original_accounts.clone().to_vec(),
            &Logger::new(test_name),
        );

        let expected_adjustment_analysis = {
            let analyzed_accounts = PreparatoryAnalyzer::pre_process_accounts_for_adjustments(
                original_accounts.to_vec(),
                &disqualification_arbiter,
            );
            AdjustmentAnalysis::new(Adjustment::ByServiceFee, analyzed_accounts)
        };
        assert_eq!(result, Ok(Either::Right(expected_adjustment_analysis)));
        let determine_limit_params = determine_limit_params_arc.lock().unwrap();
        let account_1 = &original_accounts[0];
        let account_2 = &original_accounts[1];
        let expected_params = vec![
            (
                account_1.bare_account.balance_wei,
                account_1.payment_threshold_intercept_minor,
                account_1.creditor_thresholds.permanent_debt_allowed_minor,
            ),
            (
                account_2.bare_account.balance_wei,
                account_2.payment_threshold_intercept_minor,
                account_2.creditor_thresholds.permanent_debt_allowed_minor,
            ),
        ];
        assert_eq!(&determine_limit_params[0..2], expected_params);
        TestLogHandler::new().exists_log_containing(&format!(
            "WARN: {test_name}: Total of {} wei in MASQ was ordered while the consuming wallet \
            held only {} wei of MASQ token. Adjustment of their count or balances is required.",
            total_amount_required.separate_with_commas(),
            cw_service_fee_balance.separate_with_commas()
        ));
    }

    #[test]
    fn adjustment_possibility_nearly_rejected_when_cw_balance_slightly_bigger() {
        let mut account_1 = make_non_guaranteed_qualified_payable(111);
        account_1.bare_account.balance_wei = 1_000_000_000;
        let mut account_2 = make_non_guaranteed_qualified_payable(333);
        account_2.bare_account.balance_wei = 2_000_000_000;
        let cw_service_fee_balance = 750_000_001;
        let disqualification_gauge = DisqualificationGaugeMock::default()
            .determine_limit_result(750_000_000)
            .determine_limit_result(1_500_000_000);
        let original_accounts = [account_1, account_2];

        test_adjustment_possibility_nearly_rejected(
            "adjustment_possibility_nearly_rejected_when_cw_balance_slightly_bigger",
            disqualification_gauge,
            original_accounts,
            cw_service_fee_balance,
        )
    }

    #[test]
    fn adjustment_possibility_nearly_rejected_when_cw_balance_equal() {
        let mut account_1 = make_non_guaranteed_qualified_payable(111);
        account_1.bare_account.balance_wei = 2_000_000_000;
        let mut account_2 = make_non_guaranteed_qualified_payable(333);
        account_2.bare_account.balance_wei = 1_000_000_000;
        let cw_service_fee_balance = 750_000_000;
        let disqualification_gauge = DisqualificationGaugeMock::default()
            .determine_limit_result(1_500_000_000)
            .determine_limit_result(750_000_000);
        let original_accounts = [account_1, account_2];

        test_adjustment_possibility_nearly_rejected(
            "adjustment_possibility_nearly_rejected_when_cw_balance_equal",
            disqualification_gauge,
            original_accounts,
            cw_service_fee_balance,
        )
    }

    fn test_not_enough_for_even_the_least_demanding_account_causes_error<F>(
        error_builder: AdjustmentPossibilityErrorBuilder,
        expected_error_preparer: F,
    ) where
        F: FnOnce(u128, u128) -> PaymentAdjusterError,
    {
        let mut account_1 = make_analyzed_account(111);
        account_1.qualified_as.bare_account.balance_wei = 2_000_000_000;
        account_1.disqualification_limit_minor = 1_500_000_000;
        let mut account_2 = make_analyzed_account(222);
        account_2.qualified_as.bare_account.balance_wei = 1_000_050_000;
        account_2.disqualification_limit_minor = 1_000_000_101;
        let mut account_3 = make_analyzed_account(333);
        account_3.qualified_as.bare_account.balance_wei = 1_000_111_111;
        account_3.disqualification_limit_minor = 1_000_000_222;
        let cw_service_fee_balance = 1_000_000_100;
        let original_accounts = vec![account_1, account_2, account_3];
        let service_fee_total_of_the_known_set = 2_000_000_000 + 1_000_050_000 + 1_000_111_111;
        let subject = PreparatoryAnalyzer {};

        let result = PreparatoryAnalyzer::check_adjustment_possibility(
            &original_accounts,
            cw_service_fee_balance,
            error_builder,
        );

        let expected_error =
            expected_error_preparer(service_fee_total_of_the_known_set, cw_service_fee_balance);
        assert_eq!(result, Err(expected_error))
    }

    #[test]
    fn not_enough_for_even_the_least_demanding_account_error_right_after_positive_tx_fee_check() {
        let transaction_fee_limitation = TransactionFeeLimitation {
            count_limit: 2,
            cw_transaction_fee_balance_minor: 200_000_000,
            per_transaction_required_fee_minor: 300_000_000,
        };
        let error_builder = AdjustmentPossibilityErrorBuilder::default().context(
            TransactionFeePastCheckContext::initial_check_done(Some(transaction_fee_limitation)),
        );
        let expected_error_preparer =
            |total_amount_demanded_in_accounts_in_place, cw_service_fee_balance_minor| {
                PaymentAdjusterError::NotEnoughServiceFeeBalanceEvenForTheSmallestTransaction {
                    number_of_accounts: 3,
                    total_service_fee_required_minor: total_amount_demanded_in_accounts_in_place,
                    cw_service_fee_balance_minor,
                    transaction_fee_appendix_opt: Some(transaction_fee_limitation),
                }
            };

        test_not_enough_for_even_the_least_demanding_account_causes_error(
            error_builder,
            expected_error_preparer,
        )
    }

    #[test]
    fn not_enough_for_even_the_least_demanding_account_error_right_after_negative_tx_fee_check() {
        let error_builder = AdjustmentPossibilityErrorBuilder::default()
            .context(TransactionFeePastCheckContext::initial_check_done(None));
        let expected_error_preparer =
            |total_amount_demanded_in_accounts_in_place, cw_service_fee_balance_minor| {
                PaymentAdjusterError::NotEnoughServiceFeeBalanceEvenForTheSmallestTransaction {
                    number_of_accounts: 3,
                    total_service_fee_required_minor: total_amount_demanded_in_accounts_in_place,
                    cw_service_fee_balance_minor,
                    transaction_fee_appendix_opt: None,
                }
            };

        test_not_enough_for_even_the_least_demanding_account_causes_error(
            error_builder,
            expected_error_preparer,
        )
    }

    #[test]
    fn not_enough_for_even_the_least_demanding_account_error_right_after_tx_fee_accounts_dump() {
        let accounts = vec![
            make_weighed_account(123),
            make_weighed_account(456),
            make_weighed_account(789),
            make_weighed_account(1011),
        ];
        let initial_sum = sum_as(&accounts, |account| account.balance_minor());
        let initial_count = accounts.len();
        let error_builder = AdjustmentPossibilityErrorBuilder::default()
            .context(TransactionFeePastCheckContext::accounts_dumped(&accounts));
        let expected_error_preparer = |_, cw_service_fee_balance_minor| {
            PaymentAdjusterError::NotEnoughServiceFeeBalanceEvenForTheSmallestTransaction {
                number_of_accounts: initial_count,
                total_service_fee_required_minor: initial_sum,
                cw_service_fee_balance_minor,
                transaction_fee_appendix_opt: None,
            }
        };

        test_not_enough_for_even_the_least_demanding_account_causes_error(
            error_builder,
            expected_error_preparer,
        )
    }

    #[test]
    fn accounts_analyzing_works_even_for_weighted_payable() {
        init_test_logging();
        let test_name = "accounts_analyzing_works_even_for_weighted_payable";
        let balance_1 = multiple_by_billion(2_000_000);
        let mut weighted_account_1 = make_weighed_account(123);
        weighted_account_1
            .analyzed_account
            .qualified_as
            .bare_account
            .balance_wei = balance_1;
        let balance_2 = multiple_by_billion(3_456_000);
        let mut weighted_account_2 = make_weighed_account(456);
        weighted_account_2
            .analyzed_account
            .qualified_as
            .bare_account
            .balance_wei = balance_2;
        let accounts = vec![weighted_account_1, weighted_account_2];
        let service_fee_totally_required_minor = balance_1 + balance_2;
        let cw_service_fee_balance_minor = service_fee_totally_required_minor + 1;
        let error_builder = AdjustmentPossibilityErrorBuilder::default();
        let logger = Logger::new(test_name);
        let subject = PreparatoryAnalyzer::new();

        [(0, false), (1, false), (2, true)].iter().for_each(
            |(subtrahend_from_cw_balance, expected_result)| {
                let service_fee_balance = cw_service_fee_balance_minor - subtrahend_from_cw_balance;
                let result = subject
                    .recheck_if_service_fee_adjustment_is_needed(
                        &accounts,
                        service_fee_balance,
                        error_builder.clone(),
                        &logger,
                    )
                    .unwrap();
                assert_eq!(result, *expected_result);
            },
        );
        TestLogHandler::new().exists_log_containing(&format!(
            "WARN: {test_name}: Total of {} wei in MASQ was ordered while the consuming wallet held \
            only {}", service_fee_totally_required_minor.separate_with_commas(),
            (cw_service_fee_balance_minor - 2).separate_with_commas()
        ));
    }

    fn double_mock_results_queue(mock: DisqualificationGaugeMock) -> DisqualificationGaugeMock {
        let originally_prepared_results = (0..2)
            .map(|_| mock.determine_limit(0, 0, 0))
            .collect::<Vec<_>>();
        originally_prepared_results
            .into_iter()
            .cycle()
            .take(4)
            .fold(mock, |mock, result_to_be_added| {
                mock.determine_limit_result(result_to_be_added)
            })
    }
}
