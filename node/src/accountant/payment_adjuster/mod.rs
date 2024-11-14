// Copyright (c) 2023, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

// If possible, keep these modules private
mod criterion_calculators;
mod disqualification_arbiter;
mod inner;
mod logging_and_diagnostics;
mod miscellaneous;
#[cfg(test)]
mod non_unit_tests;
mod preparatory_analyser;
mod service_fee_adjuster;
#[cfg(test)]
mod test_utils;

use crate::accountant::db_access_objects::payable_dao::PayableAccount;
use crate::accountant::payment_adjuster::criterion_calculators::balance_calculator::BalanceCriterionCalculator;
use crate::accountant::payment_adjuster::criterion_calculators::CriterionCalculator;
use crate::accountant::payment_adjuster::logging_and_diagnostics::diagnostics::ordinary_diagnostic_functions::calculated_criterion_and_weight_diagnostics;
use crate::accountant::payment_adjuster::logging_and_diagnostics::diagnostics::{collection_diagnostics, diagnostics};
use crate::accountant::payment_adjuster::disqualification_arbiter::{
    DisqualificationArbiter,
};
use crate::accountant::payment_adjuster::inner::{
    PaymentAdjusterInner, PaymentAdjusterInnerNull, PaymentAdjusterInnerReal,
};
use crate::accountant::payment_adjuster::logging_and_diagnostics::log_functions::{
    accounts_before_and_after_debug,
};
use crate::accountant::payment_adjuster::miscellaneous::data_structures::{AdjustedAccountBeforeFinalization, WeightedPayable};
use crate::accountant::payment_adjuster::miscellaneous::helper_functions::{
    eliminate_accounts_by_tx_fee_limit,
    exhaust_cw_balance_entirely, find_largest_exceeding_balance,
    sum_as, no_affordable_accounts_found,
};
use crate::accountant::payment_adjuster::preparatory_analyser::{LateServiceFeeSingleTxErrorFactory, PreparatoryAnalyzer};
use crate::accountant::payment_adjuster::service_fee_adjuster::{
    ServiceFeeAdjuster, ServiceFeeAdjusterReal,
};
use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::PreparedAdjustment;
use crate::accountant::{AnalyzedPayableAccount, QualifiedPayableAccount};
use crate::diagnostics;
use crate::sub_lib::blockchain_bridge::OutboundPaymentsInstructions;
use crate::sub_lib::wallet::Wallet;
use itertools::Either;
use masq_lib::logger::Logger;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::time::SystemTime;
use actix::Addr;
use thousands::Separable;
use variant_count::VariantCount;
use web3::types::U256;
use masq_lib::utils::convert_collection;
use crate::accountant::payment_adjuster::preparatory_analyser::accounts_abstraction::DisqualificationLimitProvidingAccount;
use crate::test_utils::recorder::Recorder;
// PaymentAdjuster is a recursive and scalable algorithm that inspects payments under conditions
// of an acute insolvency. You can easily expand the range of evaluated parameters to determine
// an optimized allocation of scarce assets by writing your own CriterionCalculator. The calculator
// is supposed to be dedicated to a single parameter that can be tracked for each payable account.
//
// For parameters that can't be derived from each account, or even one at all, there is a way to
// provide such data up into the calculator. This can be achieved via the PaymentAdjusterInner.
//
// Once the new calculator exists, its place belongs in the vector of calculators which is the heart
// of this module.

pub type AdjustmentAnalysisResult =
    Result<Either<IntactOriginalAccounts, AdjustmentAnalysisReport>, PaymentAdjusterError>;

pub type IntactOriginalAccounts = Vec<QualifiedPayableAccount>;

pub trait PaymentAdjuster {
    fn consider_adjustment(
        &self,
        qualified_payables: Vec<QualifiedPayableAccount>,
        agent: &dyn BlockchainAgent,
    ) -> AdjustmentAnalysisResult;

    fn adjust_payments(
        &mut self,
        setup: PreparedAdjustment,
        now: SystemTime,
    ) -> Result<OutboundPaymentsInstructions, PaymentAdjusterError>;
}

pub struct PaymentAdjusterReal {
    analyzer: PreparatoryAnalyzer,
    disqualification_arbiter: DisqualificationArbiter,
    service_fee_adjuster: Box<dyn ServiceFeeAdjuster>,
    calculators: Vec<Box<dyn CriterionCalculator>>,
    inner: Box<dyn PaymentAdjusterInner>,
    logger: Logger,
}

impl PaymentAdjuster for PaymentAdjusterReal {
    fn consider_adjustment(
        &self,
        qualified_payables: Vec<QualifiedPayableAccount>,
        agent: &dyn BlockchainAgent,
    ) -> AdjustmentAnalysisResult {
        let disqualification_arbiter = &self.disqualification_arbiter;
        let logger = &self.logger;

        self.analyzer
            .analyze_accounts(agent, disqualification_arbiter, qualified_payables, logger)
    }

    fn adjust_payments(
        &mut self,
        setup: PreparedAdjustment,
        now: SystemTime,
    ) -> Result<OutboundPaymentsInstructions, PaymentAdjusterError> {
        let analyzed_payables = setup.adjustment_analysis.accounts;
        let response_skeleton_opt = setup.response_skeleton_opt;
        let agent = setup.agent;
        let initial_service_fee_balance_minor = agent.service_fee_balance_minor();
        let required_adjustment = setup.adjustment_analysis.adjustment;
        let max_debt_above_threshold_in_qualified_payables =
            find_largest_exceeding_balance(&analyzed_payables);

        self.initialize_inner(
            initial_service_fee_balance_minor,
            required_adjustment,
            max_debt_above_threshold_in_qualified_payables,
            now,
        );

        let sketched_debug_log_opt = self.sketch_debug_log_opt(&analyzed_payables);

        let affordable_accounts = self.run_adjustment(analyzed_payables)?;

        self.complete_debug_log_if_enabled(sketched_debug_log_opt, &affordable_accounts);

        self.reset_inner();

        Ok(OutboundPaymentsInstructions::new(
            Either::Right(affordable_accounts),
            agent,
            response_skeleton_opt,
        ))
    }
}

impl Default for PaymentAdjusterReal {
    fn default() -> Self {
        Self::new()
    }
}

impl PaymentAdjusterReal {
    pub fn new() -> Self {
        Self {
            analyzer: PreparatoryAnalyzer::new(),
            disqualification_arbiter: DisqualificationArbiter::default(),
            service_fee_adjuster: Box::new(ServiceFeeAdjusterReal::default()),
            calculators: vec![Box::new(BalanceCriterionCalculator::default())],
            inner: Box::new(PaymentAdjusterInnerNull::default()),
            logger: Logger::new("PaymentAdjuster"),
        }
    }

    fn initialize_inner(
        &mut self,
        cw_service_fee_balance: u128,
        required_adjustment: Adjustment,
        max_debt_above_threshold_in_qualified_payables: u128,
        now: SystemTime,
    ) {
        let transaction_fee_limitation_opt = match required_adjustment {
            Adjustment::BeginByTransactionFee {
                transaction_count_limit,
            } => Some(transaction_count_limit),
            Adjustment::ByServiceFee => None,
        };

        let inner = PaymentAdjusterInnerReal::new(
            now,
            transaction_fee_limitation_opt,
            cw_service_fee_balance,
            max_debt_above_threshold_in_qualified_payables,
        );

        self.inner = Box::new(inner);
    }

    fn reset_inner(&mut self) {
        self.inner = Box::new(PaymentAdjusterInnerNull::default())
    }

    fn run_adjustment(
        &mut self,
        analyzed_accounts: Vec<AnalyzedPayableAccount>,
    ) -> Result<Vec<PayableAccount>, PaymentAdjusterError> {
        let weighted_accounts = self.calculate_weights(analyzed_accounts);
        let processed_accounts = self.resolve_initial_adjustment_dispatch(weighted_accounts)?;

        if no_affordable_accounts_found(&processed_accounts) {
            return Err(PaymentAdjusterError::RecursionDrainedAllAccounts);
        }

        match processed_accounts {
            Either::Left(non_exhausted_accounts) => {
                let original_cw_service_fee_balance_minor =
                    self.inner.original_cw_service_fee_balance_minor();
                let exhaustive_affordable_accounts = exhaust_cw_balance_entirely(
                    non_exhausted_accounts,
                    original_cw_service_fee_balance_minor,
                );
                Ok(exhaustive_affordable_accounts)
            }
            Either::Right(finalized_accounts) => Ok(finalized_accounts),
        }
    }

    fn resolve_initial_adjustment_dispatch(
        &mut self,
        weighted_payables: Vec<WeightedPayable>,
    ) -> Result<
        Either<Vec<AdjustedAccountBeforeFinalization>, Vec<PayableAccount>>,
        PaymentAdjusterError,
    > {
        if let Some(limit) = self.inner.transaction_fee_count_limit_opt() {
            return self.begin_with_adjustment_by_transaction_fee(weighted_payables, limit);
        }

        Ok(Either::Left(self.propose_possible_adjustment_recursively(
            weighted_payables,
        )))
    }

    fn begin_with_adjustment_by_transaction_fee(
        &mut self,
        weighed_accounts: Vec<WeightedPayable>,
        transaction_count_limit: u16,
    ) -> Result<
        Either<Vec<AdjustedAccountBeforeFinalization>, Vec<PayableAccount>>,
        PaymentAdjusterError,
    > {
        diagnostics!(
            "\nBEGINNING WITH ADJUSTMENT BY TRANSACTION FEE FOR ACCOUNTS:",
            &weighed_accounts
        );

        let error_factory = LateServiceFeeSingleTxErrorFactory::new(&weighed_accounts);

        let weighted_accounts_affordable_by_transaction_fee =
            eliminate_accounts_by_tx_fee_limit(weighed_accounts, transaction_count_limit);

        let cw_service_fee_balance_minor = self.inner.original_cw_service_fee_balance_minor();

        if self.analyzer.recheck_if_service_fee_adjustment_is_needed(
            &weighted_accounts_affordable_by_transaction_fee,
            cw_service_fee_balance_minor,
            error_factory,
            &self.logger,
        )? {
            let final_set_before_exhausting_cw_balance = self
                .propose_possible_adjustment_recursively(
                    weighted_accounts_affordable_by_transaction_fee,
                );

            Ok(Either::Left(final_set_before_exhausting_cw_balance))
        } else {
            let accounts_not_needing_adjustment =
                convert_collection(weighted_accounts_affordable_by_transaction_fee);

            Ok(Either::Right(accounts_not_needing_adjustment))
        }
    }

    fn propose_possible_adjustment_recursively(
        &mut self,
        weighed_accounts: Vec<WeightedPayable>,
    ) -> Vec<AdjustedAccountBeforeFinalization> {
        diagnostics!(
            "\nUNRESOLVED ACCOUNTS IN CURRENT ITERATION:",
            &weighed_accounts
        );

        let disqualification_arbiter = &self.disqualification_arbiter;
        let unallocated_cw_service_fee_balance =
            self.inner.unallocated_cw_service_fee_balance_minor();
        let logger = &self.logger;

        let current_iteration_result = self.service_fee_adjuster.perform_adjustment_by_service_fee(
            weighed_accounts,
            disqualification_arbiter,
            unallocated_cw_service_fee_balance,
            logger,
        );

        let decided_accounts = current_iteration_result.decided_accounts;
        let remaining_undecided_accounts = current_iteration_result.remaining_undecided_accounts;

        if remaining_undecided_accounts.is_empty() {
            return decided_accounts;
        }

        if !decided_accounts.is_empty() {
            self.adjust_remaining_unallocated_cw_balance_down(&decided_accounts)
        }

        let merged =
            if self.is_cw_balance_enough_to_remaining_accounts(&remaining_undecided_accounts) {
                Self::merge_accounts(
                    decided_accounts,
                    convert_collection(remaining_undecided_accounts),
                )
            } else {
                Self::merge_accounts(
                    decided_accounts,
                    self.propose_possible_adjustment_recursively(remaining_undecided_accounts),
                )
            };

        diagnostics!(
            "\nFINAL SET OF ADJUSTED ACCOUNTS IN CURRENT ITERATION:",
            &merged
        );

        merged
    }

    fn is_cw_balance_enough_to_remaining_accounts(
        &self,
        remaining_undecided_accounts: &[WeightedPayable],
    ) -> bool {
        let unallocated_cw_service_fee_balance =
            self.inner.unallocated_cw_service_fee_balance_minor();
        let minimum_sum_required: u128 = sum_as(remaining_undecided_accounts, |weighted_account| {
            weighted_account.disqualification_limit()
        });
        minimum_sum_required <= unallocated_cw_service_fee_balance
    }

    fn merge_accounts(
        mut previously_decided_accounts: Vec<AdjustedAccountBeforeFinalization>,
        newly_decided_accounts: Vec<AdjustedAccountBeforeFinalization>,
    ) -> Vec<AdjustedAccountBeforeFinalization> {
        previously_decided_accounts.extend(newly_decided_accounts);
        previously_decided_accounts
    }

    fn calculate_weights(&self, accounts: Vec<AnalyzedPayableAccount>) -> Vec<WeightedPayable> {
        self.apply_criteria(self.calculators.as_slice(), accounts)
    }

    fn apply_criteria(
        &self,
        criteria_calculators: &[Box<dyn CriterionCalculator>],
        qualified_accounts: Vec<AnalyzedPayableAccount>,
    ) -> Vec<WeightedPayable> {
        qualified_accounts
            .into_iter()
            .map(|payable| {
                let weight =
                    criteria_calculators
                        .iter()
                        .fold(0_u128, |weight, criterion_calculator| {
                            let new_criterion = criterion_calculator
                                .calculate(&payable.qualified_as, self.inner.as_ref());

                            let summed_up = weight + new_criterion;

                            calculated_criterion_and_weight_diagnostics(
                                &payable.qualified_as.bare_account.wallet,
                                criterion_calculator.as_ref(),
                                new_criterion,
                                summed_up,
                            );

                            summed_up
                        });

                WeightedPayable::new(payable, weight)
            })
            .collect()
    }

    fn adjust_remaining_unallocated_cw_balance_down(
        &mut self,
        decided_accounts: &[AdjustedAccountBeforeFinalization],
    ) {
        let subtrahend_total: u128 = sum_as(decided_accounts, |account| {
            account.proposed_adjusted_balance_minor
        });
        self.inner
            .subtract_from_unallocated_cw_service_fee_balance_minor(subtrahend_total);

        diagnostics!(
            "LOWERED CW BALANCE",
            "Unallocated balance lowered by {} to {}",
            subtrahend_total.separate_with_commas(),
            self.inner
                .unallocated_cw_service_fee_balance_minor()
                .separate_with_commas()
        )
    }

    fn sketch_debug_log_opt(
        &self,
        qualified_payables: &[AnalyzedPayableAccount],
    ) -> Option<HashMap<Wallet, u128>> {
        self.logger.debug_enabled().then(|| {
            qualified_payables
                .iter()
                .map(|payable| {
                    (
                        payable.qualified_as.bare_account.wallet.clone(),
                        payable.qualified_as.bare_account.balance_wei,
                    )
                })
                .collect::<HashMap<Wallet, u128>>()
        })
    }

    fn complete_debug_log_if_enabled(
        &self,
        sketched_debug_info_opt: Option<HashMap<Wallet, u128>>,
        fully_processed_accounts: &[PayableAccount],
    ) {
        self.logger.debug(|| {
            let sketched_debug_info =
                sketched_debug_info_opt.expect("debug is enabled, so info should exist");
            accounts_before_and_after_debug(sketched_debug_info, fully_processed_accounts)
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Adjustment {
    ByServiceFee,
    BeginByTransactionFee { transaction_count_limit: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjustmentAnalysisReport {
    pub adjustment: Adjustment,
    pub accounts: Vec<AnalyzedPayableAccount>,
}

impl AdjustmentAnalysisReport {
    pub fn new(adjustment: Adjustment, accounts: Vec<AnalyzedPayableAccount>) -> Self {
        AdjustmentAnalysisReport {
            adjustment,
            accounts,
        }
    }
}

#[derive(Debug, PartialEq, Eq, VariantCount)]
pub enum PaymentAdjusterError {
    EarlyNotEnoughFeeForSingleTransaction {
        number_of_accounts: usize,
        transaction_fee_opt: Option<TransactionFeeImmoderateInsufficiency>,
        service_fee_opt: Option<ServiceFeeImmoderateInsufficiency>,
    },
    LateNotEnoughFeeForSingleTransaction {
        original_number_of_accounts: usize,
        number_of_accounts: usize,
        original_service_fee_required_total_minor: u128,
        cw_service_fee_balance_minor: u128,
    },
    RecursionDrainedAllAccounts,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TransactionFeeImmoderateInsufficiency {
    pub per_transaction_requirement_minor: u128,
    pub cw_transaction_fee_balance_minor: U256,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ServiceFeeImmoderateInsufficiency {
    pub total_service_fee_required_minor: u128,
    pub cw_service_fee_balance_minor: u128,
}

impl PaymentAdjusterError {
    pub fn insolvency_detected(&self) -> bool {
        match self {
            PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction { .. } => true,
            PaymentAdjusterError::LateNotEnoughFeeForSingleTransaction { .. } => true,
            PaymentAdjusterError::RecursionDrainedAllAccounts => true,
            // We haven't needed to worry in this matter yet, this is rather a future alarm that
            // will draw attention after somebody adds a possibility for an error not necessarily
            // implying that an insolvency was detected before. At the moment, each error occurs
            // only alongside an actual insolvency. (Hint: There might be consequences for
            // the wording of the error message whose forming takes place back out, nearer to the
            // Accountant's general area)
        }
    }
}

impl Display for PaymentAdjusterError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                number_of_accounts,
                transaction_fee_opt,
                service_fee_opt,
            } => {
                match (transaction_fee_opt, service_fee_opt) {
                    (Some(transaction_fee_check_summary), None) =>
                        write!(
                        f,
                        "Current transaction fee balance is not enough to pay a single payment. \
                        Number of canceled payments: {}. Transaction fee per payment: {} wei, while \
                        the wallet contains: {} wei",
                        number_of_accounts,
                        transaction_fee_check_summary.per_transaction_requirement_minor.separate_with_commas(),
                        transaction_fee_check_summary.cw_transaction_fee_balance_minor.separate_with_commas()
                    ),
                    (None, Some(service_fee_check_summary)) =>
                        write!(
                        f,
                        "Current service fee balance is not enough to pay a single payment. \
                        Number of canceled payments: {}. Total amount required: {} wei, while the wallet \
                        contains: {} wei",
                        number_of_accounts,
                        service_fee_check_summary.total_service_fee_required_minor.separate_with_commas(),
                        service_fee_check_summary.cw_service_fee_balance_minor.separate_with_commas()),
                    (Some(transaction_fee_check_summary), Some(service_fee_check_summary)) =>
                        write!(
                        f,
                        "Neither transaction fee or service fee balance is enough to pay a single payment. \
                        Number of payments considered: {}. Transaction fee per payment: {} wei, while in \
                        wallet: {} wei. Total service fee required: {} wei, while in wallet: {} wei",
                        number_of_accounts,
                        transaction_fee_check_summary.per_transaction_requirement_minor.separate_with_commas(),
                        transaction_fee_check_summary.cw_transaction_fee_balance_minor.separate_with_commas(),
                        service_fee_check_summary.total_service_fee_required_minor.separate_with_commas(),
                        service_fee_check_summary.cw_service_fee_balance_minor.separate_with_commas()
                ),
                    (None, None) => unreachable!("This error contains no specifications")
                }
            },
            PaymentAdjusterError::LateNotEnoughFeeForSingleTransaction {
                original_number_of_accounts,
                number_of_accounts,
                original_service_fee_required_total_minor,
                cw_service_fee_balance_minor,
            } => write!(f, "The original set with {} accounts was adjusted down to {} due to \
                transaction fee. The new set was tested on service fee later again and did not \
                pass. Original required amount of service fee: {} wei, while the wallet \
                contains {} wei.",
                original_number_of_accounts,
                number_of_accounts,
                original_service_fee_required_total_minor.separate_with_commas(),
                cw_service_fee_balance_minor.separate_with_commas()
            ),
            PaymentAdjusterError::RecursionDrainedAllAccounts => write!(
                f,
                "The payment adjuster wasn't able to compose any combination of payables that can \
                be paid immediately with provided finances."
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::accountant::db_access_objects::payable_dao::PayableAccount;
    use crate::accountant::payment_adjuster::disqualification_arbiter::DisqualificationArbiter;
    use crate::accountant::payment_adjuster::inner::PaymentAdjusterInnerReal;
    use crate::accountant::payment_adjuster::logging_and_diagnostics::log_functions::LATER_DETECTED_SERVICE_FEE_SEVERE_SCARCITY;
    use crate::accountant::payment_adjuster::miscellaneous::data_structures::{
        AdjustmentIterationResult, WeightedPayable,
    };
    use crate::accountant::payment_adjuster::miscellaneous::helper_functions::find_largest_exceeding_balance;
    use crate::accountant::payment_adjuster::service_fee_adjuster::AdjustmentComputer;
    use crate::accountant::payment_adjuster::test_utils::{
        make_mammoth_payables, make_meaningless_analyzed_account_by_wallet, multiply_by_billion,
        CriterionCalculatorMock, PaymentAdjusterTestBuilder, ServiceFeeAdjusterMock,
        MAX_POSSIBLE_SERVICE_FEE_BALANCE_IN_MINOR, PRESERVED_TEST_PAYMENT_THRESHOLDS,
    };
    use crate::accountant::payment_adjuster::{
        Adjustment, AdjustmentAnalysisReport, PaymentAdjuster, PaymentAdjusterError,
        PaymentAdjusterReal, ServiceFeeImmoderateInsufficiency,
        TransactionFeeImmoderateInsufficiency,
    };
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::blockchain_agent::BlockchainAgent;
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::test_utils::BlockchainAgentMock;
    use crate::accountant::scanners::mid_scan_msg_handling::payable_scanner::PreparedAdjustment;
    use crate::accountant::test_utils::{
        make_analyzed_payables, make_meaningless_analyzed_account, make_payable_account,
        make_qualified_payables,
    };
    use crate::accountant::{
        AnalyzedPayableAccount, CreditorThresholds, QualifiedPayableAccount, ResponseSkeleton,
    };
    use crate::blockchain::blockchain_interface::blockchain_interface_web3::TRANSACTION_FEE_MARGIN;
    use crate::sub_lib::wallet::Wallet;
    use crate::test_utils::make_wallet;
    use crate::test_utils::unshared_test_utils::arbitrary_id_stamp::ArbitraryIdStamp;
    use itertools::Either;
    use masq_lib::logger::Logger;
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use masq_lib::utils::convert_collection;
    use std::collections::HashMap;
    use std::panic::{catch_unwind, AssertUnwindSafe};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};
    use std::{usize, vec};
    use thousands::Separable;
    use web3::types::U256;

    #[test]
    #[should_panic(
        expected = "The PaymentAdjuster Inner is uninitialised. It was detected while \
    executing unallocated_cw_service_fee_balance_minor()"
    )]
    fn payment_adjuster_new_is_created_with_inner_null() {
        let subject = PaymentAdjusterReal::new();

        let _ = subject.inner.unallocated_cw_service_fee_balance_minor();
    }

    fn test_initialize_inner_works(
        required_adjustment: Adjustment,
        expected_tx_fee_limit_opt_result: Option<u16>,
    ) {
        let mut subject = PaymentAdjusterReal::default();
        let cw_service_fee_balance = 111_222_333_444;
        let max_debt_above_threshold_in_qualified_payables = 3_555_666;
        let now = SystemTime::now();

        subject.initialize_inner(
            cw_service_fee_balance,
            required_adjustment,
            max_debt_above_threshold_in_qualified_payables,
            now,
        );

        assert_eq!(subject.inner.now(), now);
        assert_eq!(
            subject.inner.transaction_fee_count_limit_opt(),
            expected_tx_fee_limit_opt_result
        );
        assert_eq!(
            subject.inner.original_cw_service_fee_balance_minor(),
            cw_service_fee_balance
        );
        assert_eq!(
            subject.inner.unallocated_cw_service_fee_balance_minor(),
            cw_service_fee_balance
        );
        assert_eq!(
            subject
                .inner
                .max_debt_above_threshold_in_qualified_payables(),
            max_debt_above_threshold_in_qualified_payables
        )
    }

    #[test]
    fn initialize_inner_works() {
        test_initialize_inner_works(Adjustment::ByServiceFee, None);
        test_initialize_inner_works(
            Adjustment::BeginByTransactionFee {
                transaction_count_limit: 5,
            },
            Some(5),
        );
    }

    #[test]
    fn consider_adjustment_happy_path() {
        init_test_logging();
        let test_name = "consider_adjustment_happy_path";
        let mut subject = PaymentAdjusterReal::new();
        subject.logger = Logger::new(test_name);
        // Service fee balance > payments
        let input_1 = make_input_for_initial_check_tests(
            Some(TestConfigForServiceFeeBalances {
                account_balances: Either::Right(vec![
                    multiply_by_billion(85),
                    multiply_by_billion(15) - 1,
                ]),
                cw_balance_minor: multiply_by_billion(100),
            }),
            None,
        );
        // Service fee balance == payments
        let input_2 = make_input_for_initial_check_tests(
            Some(TestConfigForServiceFeeBalances {
                account_balances: Either::Left(vec![85, 15]),
                cw_balance_minor: multiply_by_billion(100),
            }),
            None,
        );
        let transaction_fee_balance_exactly_required_minor: u128 = {
            let base_value = (100 * 6 * 53_000) as u128;
            let with_margin = TRANSACTION_FEE_MARGIN.add_percent_to(base_value);
            multiply_by_billion(with_margin)
        };
        // Transaction fee balance > payments
        let input_3 = make_input_for_initial_check_tests(
            None,
            Some(TestConfigForTransactionFees {
                gas_price_major: 100,
                number_of_accounts: 6,
                estimated_transaction_fee_units_per_transaction: 53_000,
                cw_transaction_fee_balance_minor: transaction_fee_balance_exactly_required_minor
                    + 1,
            }),
        );
        // Transaction fee balance == payments
        let input_4 = make_input_for_initial_check_tests(
            None,
            Some(TestConfigForTransactionFees {
                gas_price_major: 100,
                number_of_accounts: 6,
                estimated_transaction_fee_units_per_transaction: 53_000,
                cw_transaction_fee_balance_minor: transaction_fee_balance_exactly_required_minor,
            }),
        );

        [input_1, input_2, input_3, input_4]
            .into_iter()
            .enumerate()
            .for_each(|(idx, (qualified_payables, agent))| {
                assert_eq!(
                    subject.consider_adjustment(qualified_payables.clone(), &*agent),
                    Ok(Either::Left(qualified_payables)),
                    "failed for tested input number {:?}",
                    idx + 1
                )
            });

        TestLogHandler::new().exists_no_log_containing(&format!("WARN: {test_name}:"));
    }

    #[test]
    fn consider_adjustment_sad_path_for_transaction_fee() {
        init_test_logging();
        let test_name = "consider_adjustment_sad_path_for_transaction_fee";
        let mut subject = PaymentAdjusterReal::new();
        subject.logger = Logger::new(test_name);
        let number_of_accounts = 3;
        let (qualified_payables, agent) = make_input_for_initial_check_tests(
            None,
            Some(TestConfigForTransactionFees {
                gas_price_major: 100,
                number_of_accounts,
                estimated_transaction_fee_units_per_transaction: 55_000,
                cw_transaction_fee_balance_minor: TRANSACTION_FEE_MARGIN
                    .add_percent_to(multiply_by_billion(100 * 3 * 55_000))
                    - 1,
            }),
        );

        let result = subject.consider_adjustment(qualified_payables.clone(), &*agent);

        let analyzed_payables = convert_collection(qualified_payables);
        assert_eq!(
            result,
            Ok(Either::Right(AdjustmentAnalysisReport::new(
                Adjustment::BeginByTransactionFee {
                    transaction_count_limit: 2
                },
                analyzed_payables
            )))
        );
        let log_handler = TestLogHandler::new();
        log_handler.exists_log_containing(&format!(
            "WARN: {test_name}: Transaction fee balance of 18,974,999,999,999,999 wei cannot cover \
            the anticipated 18,975,000,000,000,000 wei for 3 transactions. Maximal count is set to 2. \
            Adjustment must be performed."
        ));
        log_handler.exists_log_containing(&format!(
            "INFO: {test_name}: Please be aware that abandoning your debts is going to result in \
            delinquency bans. In order to consume services without limitations, you will need to \
            place more funds into your consuming wallet."
        ));
    }

    #[test]
    fn consider_adjustment_sad_path_for_service_fee_balance() {
        init_test_logging();
        let test_name = "consider_adjustment_positive_for_service_fee_balance";
        let logger = Logger::new(test_name);
        let mut subject = PaymentAdjusterReal::new();
        subject.logger = logger;
        let (qualified_payables, agent) = make_input_for_initial_check_tests(
            Some(TestConfigForServiceFeeBalances {
                account_balances: Either::Right(vec![
                    multiply_by_billion(85),
                    multiply_by_billion(15) + 1,
                ]),
                cw_balance_minor: multiply_by_billion(100),
            }),
            None,
        );

        let result = subject.consider_adjustment(qualified_payables.clone(), &*agent);

        let analyzed_payables = convert_collection(qualified_payables);
        assert_eq!(
            result,
            Ok(Either::Right(AdjustmentAnalysisReport::new(
                Adjustment::ByServiceFee,
                analyzed_payables
            )))
        );
        let log_handler = TestLogHandler::new();
        log_handler.exists_log_containing(&format!(
            "WARN: {test_name}: Mature payables \
        amount to 100,000,000,001 MASQ wei while the consuming wallet holds only 100,000,000,000 \
        wei. Adjustment in their count or balances is necessary."
        ));
        log_handler.exists_log_containing(&format!(
            "INFO: {test_name}: Please be aware that abandoning your debts is going to result in \
            delinquency bans. In order to consume services without limitations, you will need to \
            place more funds into your consuming wallet."
        ));
    }

    #[test]
    fn service_fee_balance_is_fine_but_transaction_fee_balance_throws_error() {
        let subject = PaymentAdjusterReal::new();
        let number_of_accounts = 3;
        let tx_fee_exactly_required_for_single_tx = {
            let base_minor = multiply_by_billion(55_000 * 100);
            TRANSACTION_FEE_MARGIN.add_percent_to(base_minor)
        };
        let cw_transaction_fee_balance_minor = tx_fee_exactly_required_for_single_tx - 1;
        let (qualified_payables, agent) = make_input_for_initial_check_tests(
            Some(TestConfigForServiceFeeBalances {
                account_balances: Either::Left(vec![123]),
                cw_balance_minor: multiply_by_billion(444),
            }),
            Some(TestConfigForTransactionFees {
                gas_price_major: 100,
                number_of_accounts,
                estimated_transaction_fee_units_per_transaction: 55_000,
                cw_transaction_fee_balance_minor,
            }),
        );

        let result = subject.consider_adjustment(qualified_payables, &*agent);

        let per_transaction_requirement_minor = {
            let base_minor = multiply_by_billion(55_000 * 100);
            TRANSACTION_FEE_MARGIN.add_percent_to(base_minor)
        };
        assert_eq!(
            result,
            Err(
                PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                    number_of_accounts,
                    transaction_fee_opt: Some(TransactionFeeImmoderateInsufficiency {
                        per_transaction_requirement_minor,
                        cw_transaction_fee_balance_minor: cw_transaction_fee_balance_minor.into(),
                    }),
                    service_fee_opt: None
                }
            )
        );
    }

    #[test]
    fn checking_three_accounts_happy_for_transaction_fee_but_service_fee_balance_throws_error() {
        let test_name = "checking_three_accounts_happy_for_transaction_fee_but_service_fee_balance_throws_error";
        let garbage_cw_service_fee_balance = u128::MAX;
        let service_fee_balances_config_opt = Some(TestConfigForServiceFeeBalances {
            account_balances: Either::Left(vec![120, 300, 500]),
            cw_balance_minor: garbage_cw_service_fee_balance,
        });
        let (qualified_payables, boxed_agent) =
            make_input_for_initial_check_tests(service_fee_balances_config_opt, None);
        let analyzed_accounts: Vec<AnalyzedPayableAccount> =
            convert_collection(qualified_payables.clone());
        let minimal_disqualification_limit = analyzed_accounts
            .iter()
            .map(|account| account.disqualification_limit_minor)
            .min()
            .unwrap();
        // Condition for the error to be thrown
        let actual_insufficient_cw_service_fee_balance = minimal_disqualification_limit - 1;
        let agent_accessible = reconstruct_mock_agent(boxed_agent);
        // Dropping the garbage value on the floor
        let _ = agent_accessible.service_fee_balance_minor();
        let agent = agent_accessible
            .service_fee_balance_minor_result(actual_insufficient_cw_service_fee_balance);
        let mut subject = PaymentAdjusterReal::new();
        subject.logger = Logger::new(test_name);

        let result = subject.consider_adjustment(qualified_payables, &agent);

        assert_eq!(
            result,
            Err(
                PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                    number_of_accounts: 3,
                    transaction_fee_opt: None,
                    service_fee_opt: Some(ServiceFeeImmoderateInsufficiency {
                        total_service_fee_required_minor: 920_000_000_000,
                        cw_service_fee_balance_minor: actual_insufficient_cw_service_fee_balance
                    })
                }
            )
        );
    }

    #[test]
    fn both_balances_are_not_enough_even_for_single_transaction() {
        let subject = PaymentAdjusterReal::new();
        let number_of_accounts = 2;
        let (qualified_payables, agent) = make_input_for_initial_check_tests(
            Some(TestConfigForServiceFeeBalances {
                account_balances: Either::Left(vec![200, 300]),
                cw_balance_minor: 0,
            }),
            Some(TestConfigForTransactionFees {
                gas_price_major: 123,
                number_of_accounts,
                estimated_transaction_fee_units_per_transaction: 55_000,
                cw_transaction_fee_balance_minor: 0,
            }),
        );

        let result = subject.consider_adjustment(qualified_payables, &*agent);

        let per_transaction_requirement_minor =
            TRANSACTION_FEE_MARGIN.add_percent_to(55_000 * multiply_by_billion(123));
        assert_eq!(
            result,
            Err(
                PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                    number_of_accounts,
                    transaction_fee_opt: Some(TransactionFeeImmoderateInsufficiency {
                        per_transaction_requirement_minor,
                        cw_transaction_fee_balance_minor: U256::zero(),
                    }),
                    service_fee_opt: Some(ServiceFeeImmoderateInsufficiency {
                        total_service_fee_required_minor: multiply_by_billion(500),
                        cw_service_fee_balance_minor: 0
                    })
                }
            )
        );
    }

    #[test]
    fn payment_adjuster_error_implements_display() {
        let inputs = vec![
            (
                PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                    number_of_accounts: 4,
                    transaction_fee_opt: Some(TransactionFeeImmoderateInsufficiency{
                        per_transaction_requirement_minor: 70_000_000_000_000,
                        cw_transaction_fee_balance_minor: U256::from(90_000),
                    }),
                    service_fee_opt: None
                },
                "Current transaction fee balance is not enough to pay a single payment. Number of \
                canceled payments: 4. Transaction fee per payment: 70,000,000,000,000 wei, while \
                the wallet contains: 90,000 wei",
            ),
            (
                PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                    number_of_accounts: 5,
                    transaction_fee_opt: None,
                    service_fee_opt: Some(ServiceFeeImmoderateInsufficiency{
                        total_service_fee_required_minor: 6_000_000_000,
                        cw_service_fee_balance_minor: 333_000_000,
                    })
                },
                "Current service fee balance is not enough to pay a single payment. Number of \
                canceled payments: 5. Total amount required: 6,000,000,000 wei, while the wallet \
                contains: 333,000,000 wei",
            ),
            (
                PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                    number_of_accounts: 5,
                    transaction_fee_opt: Some(TransactionFeeImmoderateInsufficiency{
                        per_transaction_requirement_minor:  5_000_000_000,
                        cw_transaction_fee_balance_minor: U256::from(3_000_000_000_u64)
                    }),
                    service_fee_opt: Some(ServiceFeeImmoderateInsufficiency{
                        total_service_fee_required_minor: 7_000_000_000,
                        cw_service_fee_balance_minor: 100_000_000
                    })
                },
                "Neither transaction fee or service fee balance is enough to pay a single payment. \
                 Number of payments considered: 5. Transaction fee per payment: 5,000,000,000 wei, \
                 while in wallet: 3,000,000,000 wei. Total service fee required: 7,000,000,000 wei, \
                 while in wallet: 100,000,000 wei",
            ),
            (
                PaymentAdjusterError::LateNotEnoughFeeForSingleTransaction {
                    original_number_of_accounts: 6,
                    number_of_accounts: 3,
                    original_service_fee_required_total_minor: 1234567891011,
                    cw_service_fee_balance_minor: 333333,
                },
                "The original set with 6 accounts was adjusted down to 3 due to transaction fee. \
                The new set was tested on service fee later again and did not pass. Original \
                required amount of service fee: 1,234,567,891,011 wei, while the wallet contains \
                333,333 wei."),
            (
                PaymentAdjusterError::RecursionDrainedAllAccounts,
                "The payment adjuster wasn't able to compose any combination of payables that can \
                be paid immediately with provided finances.",
            ),
        ];
        let inputs_count = inputs.len();
        inputs
            .into_iter()
            .for_each(|(error, expected_msg)| assert_eq!(error.to_string(), expected_msg));
        assert_eq!(inputs_count, PaymentAdjusterError::VARIANT_COUNT + 2)
    }

    #[test]
    #[should_panic(
        expected = "internal error: entered unreachable code: This error contains no \
    specifications"
    )]
    fn error_message_for_input_referring_to_no_issues_cannot_be_made() {
        let _ = PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
            number_of_accounts: 0,
            transaction_fee_opt: None,
            service_fee_opt: None,
        }
        .to_string();
    }

    #[test]
    fn we_can_say_if_error_occurred_after_insolvency_was_detected() {
        let inputs = vec![
            PaymentAdjusterError::RecursionDrainedAllAccounts,
            PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                number_of_accounts: 0,
                transaction_fee_opt: Some(TransactionFeeImmoderateInsufficiency {
                    per_transaction_requirement_minor: 0,
                    cw_transaction_fee_balance_minor: Default::default(),
                }),
                service_fee_opt: None,
            },
            PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                number_of_accounts: 0,
                transaction_fee_opt: None,
                service_fee_opt: Some(ServiceFeeImmoderateInsufficiency {
                    total_service_fee_required_minor: 0,
                    cw_service_fee_balance_minor: 0,
                }),
            },
            PaymentAdjusterError::EarlyNotEnoughFeeForSingleTransaction {
                number_of_accounts: 0,
                transaction_fee_opt: Some(TransactionFeeImmoderateInsufficiency {
                    per_transaction_requirement_minor: 0,
                    cw_transaction_fee_balance_minor: Default::default(),
                }),
                service_fee_opt: Some(ServiceFeeImmoderateInsufficiency {
                    total_service_fee_required_minor: 0,
                    cw_service_fee_balance_minor: 0,
                }),
            },
            PaymentAdjusterError::LateNotEnoughFeeForSingleTransaction {
                original_number_of_accounts: 0,
                number_of_accounts: 0,
                original_service_fee_required_total_minor: 0,
                cw_service_fee_balance_minor: 0,
            },
        ];
        let inputs_count = inputs.len();
        let results = inputs
            .into_iter()
            .map(|err| err.insolvency_detected())
            .collect::<Vec<_>>();
        assert_eq!(results, vec![true, true, true, true, true]);
        assert_eq!(inputs_count, PaymentAdjusterError::VARIANT_COUNT + 2)
    }

    #[test]
    fn adjusted_balance_threats_to_outgrow_the_original_account_but_is_capped_by_disqualification_limit(
    ) {
        let cw_service_fee_balance_minor = multiply_by_billion(4_200_000);
        let mut account_1 = make_meaningless_analyzed_account_by_wallet("abc");
        let balance_1 = multiply_by_billion(3_000_000);
        let disqualification_limit_1 = multiply_by_billion(2_300_000);
        account_1.qualified_as.bare_account.balance_wei = balance_1;
        account_1.disqualification_limit_minor = disqualification_limit_1;
        let weight_account_1 = multiply_by_billion(2_000_100);
        let mut account_2 = make_meaningless_analyzed_account_by_wallet("def");
        let wallet_2 = account_2.qualified_as.bare_account.wallet.clone();
        let balance_2 = multiply_by_billion(2_500_000);
        let disqualification_limit_2 = multiply_by_billion(1_800_000);
        account_2.qualified_as.bare_account.balance_wei = balance_2;
        account_2.disqualification_limit_minor = disqualification_limit_2;
        let weighed_account_2 = multiply_by_billion(3_999_900);
        let largest_exceeding_balance = (balance_1
            - account_1.qualified_as.payment_threshold_intercept_minor)
            .max(balance_2 - account_2.qualified_as.payment_threshold_intercept_minor);
        let mut subject = PaymentAdjusterTestBuilder::default()
            .cw_service_fee_balance_minor(cw_service_fee_balance_minor)
            .max_debt_above_threshold_in_qualified_payables(largest_exceeding_balance)
            .build();
        let weighted_payables = vec![
            WeightedPayable::new(account_1, weight_account_1),
            WeightedPayable::new(account_2, weighed_account_2),
        ];

        let mut result = subject
            .resolve_initial_adjustment_dispatch(weighted_payables.clone())
            .unwrap()
            .left()
            .unwrap();

        // This shows how the weights can turn tricky for which it's important to have a hard upper
        // limit, chosen quite down, as the disqualification limit, for optimisation. In its
        // extremity, the naked algorithm of the reallocation of funds could have granted a value
        // above the original debt size, which is clearly unfair.
        illustrate_that_we_need_to_prevent_exceeding_the_original_value(
            subject,
            cw_service_fee_balance_minor,
            weighted_payables.clone(),
            wallet_2,
            balance_2,
        );
        let payable_account_1 = &weighted_payables[0]
            .analyzed_account
            .qualified_as
            .bare_account;
        let payable_account_2 = &weighted_payables[1]
            .analyzed_account
            .qualified_as
            .bare_account;
        let first_returned_account = result.remove(0);
        assert_eq!(&first_returned_account.original_account, payable_account_2);
        assert_eq!(
            first_returned_account.proposed_adjusted_balance_minor,
            disqualification_limit_2
        );
        let second_returned_account = result.remove(0);
        assert_eq!(&second_returned_account.original_account, payable_account_1);
        assert_eq!(
            second_returned_account.proposed_adjusted_balance_minor,
            disqualification_limit_1
        );
        assert!(result.is_empty());
    }

    fn illustrate_that_we_need_to_prevent_exceeding_the_original_value(
        mut subject: PaymentAdjusterReal,
        cw_service_fee_balance_minor: u128,
        weighted_accounts: Vec<WeightedPayable>,
        wallet_of_expected_outweighed: Wallet,
        original_balance_of_outweighed_account: u128,
    ) {
        let garbage_max_debt_above_threshold_in_qualified_payables = 123456789;
        subject.inner = Box::new(PaymentAdjusterInnerReal::new(
            SystemTime::now(),
            None,
            cw_service_fee_balance_minor,
            garbage_max_debt_above_threshold_in_qualified_payables,
        ));
        let unconfirmed_adjustments = AdjustmentComputer::default()
            .compute_unconfirmed_adjustments(weighted_accounts, cw_service_fee_balance_minor);
        // The results are sorted from the biggest weights down
        assert_eq!(
            unconfirmed_adjustments[1].wallet(),
            &wallet_of_expected_outweighed
        );
        // To prevent unjust reallocation we used to secure a rule an account could never demand
        // more than 100% of its size.

        // Later it was changed to a different policy, so called "outweighed" account gains
        // automatically a balance equal to its disqualification limit. Still, later on it's very
        // likely to be given a bit more from the remains languishing in the consuming wallet.
        let proposed_adjusted_balance = unconfirmed_adjustments[1].proposed_adjusted_balance_minor;
        assert!(
            proposed_adjusted_balance > (original_balance_of_outweighed_account * 11 / 10),
            "we expected the proposed balance at least 1.1 times bigger than the original balance \
            which is {} but it was {}",
            original_balance_of_outweighed_account.separate_with_commas(),
            proposed_adjusted_balance.separate_with_commas()
        );
    }

    #[test]
    fn adjustment_started_but_all_accounts_were_eliminated_anyway() {
        let test_name = "adjustment_started_but_all_accounts_were_eliminated_anyway";
        let now = SystemTime::now();
        // This simplifies the overall picture, the debt age doesn't mean anything to our calculator,
        // still, it influences the height of the intercept point read out from the payment thresholds
        // which can induce an impact on the value of the disqualification limit which is derived
        // from the intercept
        let common_unimportant_age_for_accounts =
            now.checked_sub(Duration::from_secs(200_000)).unwrap();
        let balance_1 = multiply_by_billion(3_000_000);
        let account_1 = PayableAccount {
            wallet: make_wallet("abc"),
            balance_wei: balance_1,
            last_paid_timestamp: common_unimportant_age_for_accounts,
            pending_payable_opt: None,
        };
        let balance_2 = multiply_by_billion(2_000_000);
        let account_2 = PayableAccount {
            wallet: make_wallet("def"),
            balance_wei: balance_2,
            last_paid_timestamp: common_unimportant_age_for_accounts,
            pending_payable_opt: None,
        };
        let balance_3 = multiply_by_billion(5_000_000);
        let account_3 = PayableAccount {
            wallet: make_wallet("ghi"),
            balance_wei: balance_3,
            last_paid_timestamp: common_unimportant_age_for_accounts,
            pending_payable_opt: None,
        };
        let payables = vec![account_1, account_2, account_3];
        let qualified_payables =
            make_qualified_payables(payables, &PRESERVED_TEST_PAYMENT_THRESHOLDS, now);
        let calculator_mock = CriterionCalculatorMock::default()
            .calculate_result(multiply_by_billion(2_000_000_000))
            .calculate_result(0)
            .calculate_result(0);
        let mut subject = PaymentAdjusterTestBuilder::default()
            .start_with_inner_null()
            .logger(Logger::new(test_name))
            .build();
        subject.calculators.push(Box::new(calculator_mock));
        let cw_service_fee_balance_minor = balance_2;
        let disqualification_arbiter = &subject.disqualification_arbiter;
        let agent_for_analysis = BlockchainAgentMock::default()
            .gas_price_margin_result(*TRANSACTION_FEE_MARGIN)
            .service_fee_balance_minor_result(cw_service_fee_balance_minor)
            .transaction_fee_balance_minor_result(U256::MAX)
            .estimated_transaction_fee_per_transaction_minor_result(12356);
        let analysis_result = subject.analyzer.analyze_accounts(
            &agent_for_analysis,
            disqualification_arbiter,
            qualified_payables,
            &subject.logger,
        );
        // The initial intelligent check that PA runs can feel out if the hypothetical adjustment
        // would have some minimal chance to complete successfully. Still, this aspect of it is
        // rather a weak spot, as the only guarantee it sets on works for an assurance that at
        // least the smallest account, with its specific disqualification limit, can be fulfilled
        // by the available funds.
        // In this test it would be a yes there. There's even a surplus in case of the second
        // account.
        // Then the adjustment itself spins off. The accounts get their weights. The second one as
        // to its lowest size should be granted a big one, wait until the other two are eliminated
        // by the recursion and win for the scarce money as paid in the full scale.
        // Normally, what was said would hold true. The big difference is caused by an extra,
        // actually made up, parameter which comes in with the mock calculator stuck in to join
        // the others. It changes the distribution of weights among those three accounts and makes
        // the first account be the most important one. Because of that two other accounts are
        // eliminated, the account three first, and then the account two.
        // When we look back to the preceding entry check, the minimal condition was exercised on
        // the account two, because at that time the weights hadn't been known yet. As the result,
        // the recursion will continue to even eliminate the last account, the account one, for
        // which there isn't enough money to get over its disqualification limit.
        let adjustment_analysis = match analysis_result {
            Ok(Either::Right(analysis)) => analysis,
            x => panic!(
                "We expected to be let it for an adjustments with AnalyzedAccounts but got: {:?}",
                x
            ),
        };
        let agent = Box::new(
            BlockchainAgentMock::default()
                .service_fee_balance_minor_result(cw_service_fee_balance_minor),
        );
        let adjustment_setup = PreparedAdjustment {
            agent,
            response_skeleton_opt: None,
            adjustment_analysis,
        };

        let result = subject.adjust_payments(adjustment_setup, now);

        let err = match result {
            Err(e) => e,
            Ok(ok) => panic!(
                "we expected to get an error, but it was ok: {:?}",
                ok.affordable_accounts
            ),
        };
        assert_eq!(err, PaymentAdjusterError::RecursionDrainedAllAccounts)
    }

    #[test]
    fn account_disqualification_makes_the_rest_outweighed_as_cw_balance_becomes_excessive_for_them()
    {
        // We test that a condition to short-circuit through is integrated in for a situation where
        // a performed disqualification frees means that will become available for other accounts,
        // and it happens that the remaining accounts require together less than what is left to
        // give out.
        init_test_logging();
        let test_name = "account_disqualification_makes_the_rest_outweighed_as_cw_balance_becomes_excessive_for_them";
        let now = SystemTime::now();
        // This simplifies the overall picture, the debt age doesn't mean anything to our calculator,
        // still, it influences the height of the intercept point read out from the payment thresholds
        // which can induce an impact on the value of the disqualification limit which is derived
        // from the intercept
        let common_age_for_accounts_as_unimportant =
            now.checked_sub(Duration::from_secs(200_000)).unwrap();
        let balance_1 = multiply_by_billion(80_000_000_000);
        let account_1 = PayableAccount {
            wallet: make_wallet("abc"),
            balance_wei: balance_1,
            last_paid_timestamp: common_age_for_accounts_as_unimportant,
            pending_payable_opt: None,
        };
        let balance_2 = multiply_by_billion(60_000_000_000);
        let account_2 = PayableAccount {
            wallet: make_wallet("def"),
            balance_wei: balance_2,
            last_paid_timestamp: common_age_for_accounts_as_unimportant,
            pending_payable_opt: None,
        };
        let balance_3 = multiply_by_billion(40_000_000_000);
        let account_3 = PayableAccount {
            wallet: make_wallet("ghi"),
            balance_wei: balance_3,
            last_paid_timestamp: common_age_for_accounts_as_unimportant,
            pending_payable_opt: None,
        };
        let payables = vec![account_1, account_2.clone(), account_3.clone()];
        let analyzed_accounts =
            make_analyzed_payables(payables, &PRESERVED_TEST_PAYMENT_THRESHOLDS, now);
        let calculator_mock = CriterionCalculatorMock::default()
            .calculate_result(0)
            .calculate_result(multiply_by_billion(50_000_000_000))
            .calculate_result(multiply_by_billion(50_000_000_000));
        let mut subject = PaymentAdjusterTestBuilder::default()
            .start_with_inner_null()
            .logger(Logger::new(test_name))
            .build();
        subject.calculators.push(Box::new(calculator_mock));
        let agent_id_stamp = ArbitraryIdStamp::new();
        let service_fee_balance_in_minor_units = balance_2 + balance_3 + ((balance_1 * 10) / 100);
        let agent = {
            let mock = BlockchainAgentMock::default()
                .set_arbitrary_id_stamp(agent_id_stamp)
                .service_fee_balance_minor_result(service_fee_balance_in_minor_units);
            Box::new(mock)
        };
        let adjustment_setup = PreparedAdjustment {
            agent,
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::ByServiceFee,
                analyzed_accounts,
            ),
            response_skeleton_opt: None,
        };

        let result = subject.adjust_payments(adjustment_setup, now).unwrap();

        let expected_affordable_accounts = { vec![account_3, account_2] };
        assert_eq!(result.affordable_accounts, expected_affordable_accounts);
        assert_eq!(result.response_skeleton_opt, None);
        assert_eq!(result.agent.arbitrary_id_stamp(), agent_id_stamp)
    }

    #[test]
    fn overloaded_by_mammoth_debts_to_see_if_we_can_pass_through_without_blowing_up() {
        init_test_logging();
        let test_name =
            "overloaded_by_mammoth_debts_to_see_if_we_can_pass_through_without_blowing_up";
        let now = SystemTime::now();
        // Each of the 3 accounts refers to a debt sized as the entire MASQ token supply and being
        // 10 years old which generates enormously large numbers in the algorithm, especially for
        // the calculated criteria of over accounts
        let extreme_payables = {
            let debt_age_in_months = vec![120, 120, 120];
            make_mammoth_payables(
                Either::Left((
                    debt_age_in_months,
                    *MAX_POSSIBLE_SERVICE_FEE_BALANCE_IN_MINOR,
                )),
                now,
            )
        };
        let analyzed_payables =
            make_analyzed_payables(extreme_payables, &PRESERVED_TEST_PAYMENT_THRESHOLDS, now);
        let mut subject = PaymentAdjusterReal::new();
        subject.logger = Logger::new(test_name);
        // In turn, tiny cw balance
        let cw_service_fee_balance_minor = 1_000;
        let agent = {
            let mock = BlockchainAgentMock::default()
                .service_fee_balance_minor_result(cw_service_fee_balance_minor);
            Box::new(mock)
        };
        let adjustment_setup = PreparedAdjustment {
            agent,
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::ByServiceFee,
                analyzed_payables,
            ),
            response_skeleton_opt: None,
        };

        let result = subject.adjust_payments(adjustment_setup, now);

        // The error isn't important. Received just because we set an almost empty wallet
        let err = match result {
            Ok(_) => panic!("we expected err but got ok"),
            Err(e) => e,
        };
        assert_eq!(err, PaymentAdjusterError::RecursionDrainedAllAccounts);
        let expected_log = |wallet: &str| {
            format!(
                "INFO: {test_name}: Ready payment to {wallet} was eliminated to spare MASQ for \
                those higher prioritized. {} wei owed at the moment.",
                (*MAX_POSSIBLE_SERVICE_FEE_BALANCE_IN_MINOR).separate_with_commas()
            )
        };
        let log_handler = TestLogHandler::new();
        [
            "0x000000000000000000000000000000626c616830",
            "0x000000000000000000000000000000626c616831",
            "0x000000000000000000000000000000626c616832",
        ]
        .into_iter()
        .for_each(|address| {
            let _ = log_handler.exists_log_containing(&expected_log(address));
        });

        // Nothing blew up from the giant inputs, the test was a success
    }

    fn make_weighted_payable(n: u64, initial_balance_minor: u128) -> WeightedPayable {
        let mut payable =
            WeightedPayable::new(make_meaningless_analyzed_account(111), n as u128 * 1234);
        payable
            .analyzed_account
            .qualified_as
            .bare_account
            .balance_wei = initial_balance_minor;
        payable
    }

    fn test_is_cw_balance_enough_to_remaining_accounts(
        initial_disqualification_limit_for_each_account: u128,
        untaken_cw_service_fee_balance_minor: u128,
        expected_result: bool,
    ) {
        let mut subject = PaymentAdjusterReal::new();
        subject.initialize_inner(
            untaken_cw_service_fee_balance_minor,
            Adjustment::ByServiceFee,
            1234567,
            SystemTime::now(),
        );
        let mut payable_1 =
            make_weighted_payable(111, 2 * initial_disqualification_limit_for_each_account);
        payable_1.analyzed_account.disqualification_limit_minor =
            initial_disqualification_limit_for_each_account;
        let mut payable_2 =
            make_weighted_payable(222, 3 * initial_disqualification_limit_for_each_account);
        payable_2.analyzed_account.disqualification_limit_minor =
            initial_disqualification_limit_for_each_account;
        let weighted_payables = vec![payable_1, payable_2];

        let result = subject.is_cw_balance_enough_to_remaining_accounts(&weighted_payables);

        assert_eq!(result, expected_result)
    }

    #[test]
    fn untaken_balance_is_equal_to_sum_of_disqualification_limits_in_remaining_accounts() {
        let disqualification_limit_for_each_account = 5_000_000_000;
        let untaken_cw_service_fee_balance_minor =
            disqualification_limit_for_each_account + disqualification_limit_for_each_account;

        test_is_cw_balance_enough_to_remaining_accounts(
            disqualification_limit_for_each_account,
            untaken_cw_service_fee_balance_minor,
            true,
        )
    }

    #[test]
    fn untaken_balance_is_more_than_sum_of_disqualification_limits_in_remaining_accounts() {
        let disqualification_limit_for_each_account = 5_000_000_000;
        let untaken_cw_service_fee_balance_minor =
            disqualification_limit_for_each_account + disqualification_limit_for_each_account + 1;

        test_is_cw_balance_enough_to_remaining_accounts(
            disqualification_limit_for_each_account,
            untaken_cw_service_fee_balance_minor,
            true,
        )
    }

    #[test]
    fn untaken_balance_is_less_than_sum_of_disqualification_limits_in_remaining_accounts() {
        let disqualification_limit_for_each_account = 5_000_000_000;
        let untaken_cw_service_fee_balance_minor =
            disqualification_limit_for_each_account + disqualification_limit_for_each_account - 1;

        test_is_cw_balance_enough_to_remaining_accounts(
            disqualification_limit_for_each_account,
            untaken_cw_service_fee_balance_minor,
            false,
        )
    }

    fn meaningless_timestamp() -> SystemTime {
        SystemTime::now()
    }

    // This function should take just such essential args like balances and also those that have
    // a less significant, yet important, role within the verification process of the proposed
    // adjusted balances.
    fn make_plucked_qualified_account(
        wallet_seed: &str,
        balance_minor: u128,
        threshold_intercept_major: u128,
        permanent_debt_allowed_major: u128,
    ) -> QualifiedPayableAccount {
        QualifiedPayableAccount::new(
            PayableAccount {
                wallet: make_wallet(wallet_seed),
                balance_wei: balance_minor,
                last_paid_timestamp: meaningless_timestamp(),
                pending_payable_opt: None,
            },
            multiply_by_billion(threshold_intercept_major),
            CreditorThresholds::new(multiply_by_billion(permanent_debt_allowed_major)),
        )
    }

    struct PayableAccountSeed {
        wallet_seed: &'static str,
        balance_minor: u128,
        threshold_intercept_major: u128,
        permanent_debt_allowed_major: u128,
    }

    struct DemonstrativeDisqualificationLimits {
        account_1: u128,
        account_2: u128,
        account_3: u128
    }

    impl DemonstrativeDisqualificationLimits {
        fn new(accounts: &[AnalyzedPayableAccount;3])-> Self {
            todo!()
        }
    }

    fn make_analyzed_accounts_and_show_their_actual_disqualification_limits(
        accounts_seeds: [PayableAccountSeed;3]
    ) -> ([AnalyzedPayableAccount;3], DemonstrativeDisqualificationLimits) {

        let qualified_payables: Vec<_> = accounts_seeds.map(|account_seed|
        QualifiedPayableAccount::new(
            PayableAccount {
                wallet: make_wallet(account_seed.wallet_seed),
                balance_wei: account_seed.balance_minor,
                last_paid_timestamp: meaningless_timestamp(),
                pending_payable_opt: None,
            },
            multiply_by_billion(account_seed.threshold_intercept_major),
            CreditorThresholds::new(multiply_by_billion(account_seed.permanent_debt_allowed_major)),
        )
        ).collect();
        let analyzed_accounts: Vec<AnalyzedPayableAccount> = convert_collection(qualified_payables);
        let analyzed_accounts: [AnalyzedPayableAccount;3] = analyzed_accounts.try_into().unwrap();
        let disqualification_limits = DemonstrativeDisqualificationLimits::new(&analyzed_accounts);
        (analyzed_accounts, disqualification_limits)
    }

    //----------------------------------------------------------------------------------------------
    // Main-purpose okay tests manifesting the full pallet of different adjustment scenarios

    #[test]
    fn accounts_count_does_not_change_during_adjustment() {
        init_test_logging();
        let calculate_params_arc = Arc::new(Mutex::new(vec![]));
        let test_name = "accounts_count_does_not_change_during_adjustment";
        let now = SystemTime::now();
        let balance_1 = 5_100_100_100_200_200_200;
        let qualified_account_1 =
            make_plucked_qualified_account("abc", balance_1, 2_000_000_000, 1_000_000_000);
        let balance_2 = 6_000_000_000_123_456_789;
        let qualified_account_2 =
            make_plucked_qualified_account("def", balance_2, 2_500_000_000, 2_000_000_000);
        let balance_3 = 6_666_666_666_000_000_000;
        let qualified_account_3 =
            make_plucked_qualified_account("ghi", balance_3, 2_000_000_000, 1_111_111_111);
        let qualified_payables = vec![
            qualified_account_1.clone(),
            qualified_account_2.clone(),
            qualified_account_3.clone(),
        ];
        let analyzed_payables = convert_collection(qualified_payables);
        let calculator_mock = CriterionCalculatorMock::default()
            .calculate_params(&calculate_params_arc)
            .calculate_result(multiply_by_billion(4_600_000_000))
            .calculate_result(multiply_by_billion(4_200_000_000))
            .calculate_result(multiply_by_billion(3_800_000_000));
        let mut subject = PaymentAdjusterTestBuilder::default()
            .start_with_inner_null()
            .criterion_calculator(calculator_mock)
            .logger(Logger::new(test_name))
            .build();
        let agent_id_stamp = ArbitraryIdStamp::new();
        let accounts_sum_minor = balance_1 + balance_2 + balance_3;
        let cw_service_fee_balance_minor = accounts_sum_minor - multiply_by_billion(2_000_000_000);
        let agent = BlockchainAgentMock::default()
            .set_arbitrary_id_stamp(agent_id_stamp)
            .service_fee_balance_minor_result(cw_service_fee_balance_minor);
        let adjustment_setup = PreparedAdjustment {
            agent: Box::new(agent),
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::ByServiceFee,
                analyzed_payables,
            ),
            response_skeleton_opt: None,
        };

        let result = subject.adjust_payments(adjustment_setup, now).unwrap();

        let expected_adjusted_balance_1 = 4_488_988_989_200_200_200;
        let expected_adjusted_balance_2 = 5_500_000_000_123_456_789;
        let expected_adjusted_balance_3 = 5_777_777_777_000_000_000;
        let expected_criteria_computation_output = {
            let account_1_adjusted = PayableAccount {
                balance_wei: expected_adjusted_balance_1,
                ..qualified_account_1.bare_account.clone()
            };
            let account_2_adjusted = PayableAccount {
                balance_wei: expected_adjusted_balance_2,
                ..qualified_account_2.bare_account.clone()
            };
            let account_3_adjusted = PayableAccount {
                balance_wei: expected_adjusted_balance_3,
                ..qualified_account_3.bare_account.clone()
            };
            vec![account_1_adjusted, account_2_adjusted, account_3_adjusted]
        };
        assert_eq!(
            result.affordable_accounts,
            expected_criteria_computation_output
        );
        assert_eq!(result.response_skeleton_opt, None);
        assert_eq!(result.agent.arbitrary_id_stamp(), agent_id_stamp);
        let calculate_params = calculate_params_arc.lock().unwrap();
        assert_eq!(
            *calculate_params,
            vec![
                qualified_account_1,
                qualified_account_2,
                qualified_account_3
            ]
        );
        let log_msg = format!(
            "DEBUG: {test_name}: \n\
|Payable Account                            Balance Wei
|
|                                           Original
|                                           Adjusted
|
|0x0000000000000000000000000000000000676869 {}
|                                           {}
|0x0000000000000000000000000000000000646566 {}
|                                           {}
|0x0000000000000000000000000000000000616263 {}
|                                           {}",
            balance_3.separate_with_commas(),
            expected_adjusted_balance_3.separate_with_commas(),
            balance_2.separate_with_commas(),
            expected_adjusted_balance_2.separate_with_commas(),
            balance_1.separate_with_commas(),
            expected_adjusted_balance_1.separate_with_commas()
        );
        TestLogHandler::new().exists_log_containing(&log_msg.replace("|", ""));
        test_inner_was_reset_to_null(subject)
    }

    #[test]
    fn only_transaction_fee_causes_limitations_and_the_service_fee_balance_suffices() {
        init_test_logging();
        let test_name =
            "only_transaction_fee_causes_limitations_and_the_service_fee_balance_suffices";
        let now = SystemTime::now();
        let balance_1 = multiply_by_billion(111_000_000);
        let account_1 = make_plucked_qualified_account("abc", balance_1, 100_000_000, 20_000_000);
        let balance_2 = multiply_by_billion(300_000_000);
        let account_2 = make_plucked_qualified_account("def", balance_2, 120_000_000, 50_000_000);
        let balance_3 = multiply_by_billion(222_222_222);
        let account_3 = make_plucked_qualified_account("ghi", balance_3, 100_000_000, 40_000_000);
        let qualified_payables = vec![account_1.clone(), account_2, account_3.clone()];
        let analyzed_payables = convert_collection(qualified_payables);
        let calculator_mock = CriterionCalculatorMock::default()
            .calculate_result(multiply_by_billion(400_000_000))
            // This account will be cut off because it has the lowest weight and only two accounts
            // can be kept according to the limitations detected in the transaction fee balance
            .calculate_result(multiply_by_billion(120_000_000))
            .calculate_result(multiply_by_billion(250_000_000));
        let mut subject = PaymentAdjusterTestBuilder::default()
            .start_with_inner_null()
            .criterion_calculator(calculator_mock)
            .logger(Logger::new(test_name))
            .build();
        let agent_id_stamp = ArbitraryIdStamp::new();
        let agent = BlockchainAgentMock::default()
            .set_arbitrary_id_stamp(agent_id_stamp)
            .service_fee_balance_minor_result(u128::MAX);
        let transaction_count_limit = 2;
        let adjustment_setup = PreparedAdjustment {
            agent: Box::new(agent),
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::BeginByTransactionFee {
                    transaction_count_limit,
                },
                analyzed_payables,
            ),
            response_skeleton_opt: None,
        };

        let result = subject.adjust_payments(adjustment_setup, now).unwrap();

        // The account 1 takes the first place for its weight being the biggest
        assert_eq!(
            result.affordable_accounts,
            vec![account_1.bare_account, account_3.bare_account]
        );
        assert_eq!(result.response_skeleton_opt, None);
        assert_eq!(result.agent.arbitrary_id_stamp(), agent_id_stamp);
        let log_msg = format!(
            "DEBUG: {test_name}: \n\
|Payable Account                            Balance Wei
|
|                                           Original
|                                           Adjusted
|
|0x0000000000000000000000000000000000676869 222,222,222,000,000,000
|                                           222,222,222,000,000,000
|0x0000000000000000000000000000000000616263 111,000,000,000,000,000
|                                           111,000,000,000,000,000
|
|Ruled Out Accounts                         Original
|
|0x0000000000000000000000000000000000646566 300,000,000,000,000,000"
        );
        TestLogHandler::new().exists_log_containing(&log_msg.replace("|", ""));
        test_inner_was_reset_to_null(subject)
    }

    #[test]
    fn both_balances_insufficient_but_adjustment_by_service_fee_will_not_affect_the_payments_count()
    {
        // The course of events:
        // 1) adjustment by transaction fee (always means accounts elimination),
        // 2) adjustment by service fee (can but not have to cause an account drop-off)
        init_test_logging();
        let now = SystemTime::now();
        let balance_1 = multiply_by_billion(111_000_000);
        let account_1 = make_plucked_qualified_account("abc", balance_1, 50_000_000, 10_000_000);
        let balance_2 = multiply_by_billion(333_000_000);
        let account_2 = make_plucked_qualified_account("def", balance_2, 200_000_000, 50_000_000);
        let balance_3 = multiply_by_billion(222_000_000);
        let account_3 = make_plucked_qualified_account("ghi", balance_3, 100_000_000, 35_000_000);
        let disqualification_arbiter = DisqualificationArbiter::default();
        let disqualification_limit_1 =
            disqualification_arbiter.calculate_disqualification_edge(&account_1);
        let disqualification_limit_3 =
            disqualification_arbiter.calculate_disqualification_edge(&account_3);
        let qualified_payables = vec![account_1.clone(), account_2, account_3.clone()];
        let analyzed_payables = convert_collection(qualified_payables);
        let calculator_mock = CriterionCalculatorMock::default()
            .calculate_result(multiply_by_billion(400_000_000))
            .calculate_result(multiply_by_billion(200_000_000))
            .calculate_result(multiply_by_billion(300_000_000));
        let mut subject = PaymentAdjusterTestBuilder::default()
            .start_with_inner_null()
            .criterion_calculator(calculator_mock)
            .build();
        let cw_service_fee_balance_minor =
            disqualification_limit_1 + disqualification_limit_3 + multiply_by_billion(10_000_000);
        let agent_id_stamp = ArbitraryIdStamp::new();
        let agent = BlockchainAgentMock::default()
            .set_arbitrary_id_stamp(agent_id_stamp)
            .service_fee_balance_minor_result(cw_service_fee_balance_minor);
        let response_skeleton_opt = Some(ResponseSkeleton {
            client_id: 123,
            context_id: 321,
        }); // Just hardening, not so important
        let transaction_count_limit = 2;
        let adjustment_setup = PreparedAdjustment {
            agent: Box::new(agent),
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::BeginByTransactionFee {
                    transaction_count_limit,
                },
                analyzed_payables,
            ),
            response_skeleton_opt,
        };

        let result = subject.adjust_payments(adjustment_setup, now).unwrap();

        // Account 2, the least important one, was eliminated for a lack of transaction fee in the cw
        let expected_accounts = {
            let account_1_adjusted = PayableAccount {
                balance_wei: 81_000_000_000_000_000,
                ..account_1.bare_account
            };
            let account_3_adjusted = PayableAccount {
                balance_wei: 157_000_000_000_000_000,
                ..account_3.bare_account
            };
            vec![account_1_adjusted, account_3_adjusted]
        };
        assert_eq!(result.affordable_accounts, expected_accounts);
        assert_eq!(result.response_skeleton_opt, response_skeleton_opt);
        assert_eq!(result.agent.arbitrary_id_stamp(), agent_id_stamp);
        test_inner_was_reset_to_null(subject)
    }

    #[test]
    fn only_service_fee_balance_limits_the_payments_count() {
        init_test_logging();
        let test_name = "only_service_fee_balance_limits_the_payments_count";
        let now = SystemTime::now();
        // Account to be adjusted to keep as much as it is left in the cw balance
        let balance_1 = multiply_by_billion(333_000_000);
        let account_1 = make_plucked_qualified_account("abc", balance_1, 200_000_000, 50_000_000);
        // Account to be outweighed and fully preserved
        let balance_2 = multiply_by_billion(111_000_000);
        let account_2 = make_plucked_qualified_account("def", balance_2, 50_000_000, 10_000_000);
        // Account to be disqualified
        let balance_3 = multiply_by_billion(600_000_000);
        let account_3 = make_plucked_qualified_account("ghi", balance_3, 400_000_000, 100_000_000);
        let qualified_payables = vec![account_1.clone(), account_2.clone(), account_3];
        let analyzed_payables = convert_collection(qualified_payables);
        let calculator_mock = CriterionCalculatorMock::default()
            .calculate_result(multiply_by_billion(900_000_000))
            .calculate_result(multiply_by_billion(1_100_000_000))
            .calculate_result(multiply_by_billion(600_000_000));
        let mut subject = PaymentAdjusterTestBuilder::default()
            .start_with_inner_null()
            .criterion_calculator(calculator_mock)
            .logger(Logger::new(test_name))
            .build();
        let service_fee_balance_in_minor_units = balance_1 + balance_2 - 55;
        let agent_id_stamp = ArbitraryIdStamp::new();
        let agent = BlockchainAgentMock::default()
            .set_arbitrary_id_stamp(agent_id_stamp)
            .service_fee_balance_minor_result(service_fee_balance_in_minor_units);
        let response_skeleton_opt = Some(ResponseSkeleton {
            client_id: 11,
            context_id: 234,
        });
        let adjustment_setup = PreparedAdjustment {
            agent: Box::new(agent),
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::ByServiceFee,
                analyzed_payables,
            ),
            response_skeleton_opt,
        };

        let result = subject.adjust_payments(adjustment_setup, now).unwrap();

        let expected_accounts = {
            let mut account_1_adjusted = account_1;
            account_1_adjusted.bare_account.balance_wei -= 55;
            vec![account_2.bare_account, account_1_adjusted.bare_account]
        };
        assert_eq!(result.affordable_accounts, expected_accounts);
        assert_eq!(result.response_skeleton_opt, response_skeleton_opt);
        assert_eq!(
            result.response_skeleton_opt,
            Some(ResponseSkeleton {
                client_id: 11,
                context_id: 234
            })
        );
        assert_eq!(result.agent.arbitrary_id_stamp(), agent_id_stamp);
        TestLogHandler::new().exists_log_containing(&format!(
            "INFO: {test_name}: Ready payment to 0x0000000000000000000000000000000000676869 was \
            eliminated to spare MASQ for those higher prioritized. 600,000,000,000,000,000 wei owed \
            at the moment."
        ));
        test_inner_was_reset_to_null(subject)
    }

    #[test]
    fn service_fee_as_well_as_transaction_fee_limits_the_payments_count() {
        init_test_logging();
        let test_name = "service_fee_as_well_as_transaction_fee_limits_the_payments_count";
        let now = SystemTime::now();
        let balance_1 = multiply_by_billion(100_000_000_000);
        let account_1 =
            make_plucked_qualified_account("abc", balance_1, 60_000_000_000, 10_000_000_000);
        // The second is thrown away first in a response to the shortage of transaction fee,
        // as its weight is the least significant
        let balance_2 = multiply_by_billion(500_000_000_000);
        let account_2 =
            make_plucked_qualified_account("def", balance_2, 100_000_000_000, 30_000_000_000);
        // Thrown away as the second one due to a shortage in the service fee,
        // listed among accounts to disqualify and picked eventually for its
        // lowest weight
        let balance_3 = multiply_by_billion(250_000_000_000);
        let account_3 =
            make_plucked_qualified_account("ghi", balance_3, 90_000_000_000, 20_000_000_000);
        let qualified_payables = vec![account_1.clone(), account_2, account_3];
        let analyzed_payables = convert_collection(qualified_payables);
        let calculator_mock = CriterionCalculatorMock::default()
            .calculate_result(multiply_by_billion(900_000_000_000))
            .calculate_result(multiply_by_billion(500_000_000_000))
            .calculate_result(multiply_by_billion(750_000_000_000));
        let mut subject = PaymentAdjusterTestBuilder::default()
            .start_with_inner_null()
            .criterion_calculator(calculator_mock)
            .logger(Logger::new(test_name))
            .build();
        let service_fee_balance_in_minor = balance_1 - multiply_by_billion(10_000_000_000);
        let agent_id_stamp = ArbitraryIdStamp::new();
        let agent = BlockchainAgentMock::default()
            .set_arbitrary_id_stamp(agent_id_stamp)
            .service_fee_balance_minor_result(service_fee_balance_in_minor);
        let transaction_count_limit = 2;
        let adjustment_setup = PreparedAdjustment {
            agent: Box::new(agent),
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::BeginByTransactionFee {
                    transaction_count_limit,
                },
                analyzed_payables,
            ),
            response_skeleton_opt: None,
        };

        let result = subject.adjust_payments(adjustment_setup, now).unwrap();

        let expected_accounts = {
            let mut account = account_1;
            account.bare_account.balance_wei = service_fee_balance_in_minor;
            vec![account.bare_account]
        };
        assert_eq!(result.affordable_accounts, expected_accounts);
        assert_eq!(result.response_skeleton_opt, None);
        assert_eq!(result.agent.arbitrary_id_stamp(), agent_id_stamp);
        let log_msg = format!(
            "DEBUG: {test_name}: \n\
|Payable Account                            Balance Wei
|
|                                           Original
|                                           Adjusted
|
|0x0000000000000000000000000000000000616263 100,000,000,000,000,000,000
|                                           90,000,000,000,000,000,000
|
|Ruled Out Accounts                         Original
|
|0x0000000000000000000000000000000000646566 500,000,000,000,000,000,000
|0x0000000000000000000000000000000000676869 250,000,000,000,000,000,000"
        );
        TestLogHandler::new().exists_log_containing(&log_msg.replace("|", ""));
        test_inner_was_reset_to_null(subject)
    }

    //TODO move this test above the happy path tests
    #[test]
    fn late_error_after_transaction_fee_adjustment_but_rechecked_transaction_fee_found_fatally_insufficient(
    ) {
        init_test_logging();
        let test_name = "late_error_after_transaction_fee_adjustment_but_rechecked_transaction_fee_found_fatally_insufficient";
        let now = SystemTime::now();
        let balance_1 = multiply_by_billion(500_000_000_000);
        let account_1 =
            make_plucked_qualified_account("abc", balance_1, 300_000_000_000, 100_000_000_000);
        // This account is eliminated in the transaction fee cut
        let balance_2 = multiply_by_billion(111_000_000_000);
        let account_2 =
            make_plucked_qualified_account("def", balance_2, 50_000_000_000, 10_000_000_000);
        let balance_3 = multiply_by_billion(300_000_000_000);
        let account_3 =
            make_plucked_qualified_account("ghi", balance_3, 150_000_000_000, 50_000_000_000);
        let mut subject = PaymentAdjusterReal::new();
        subject.logger = Logger::new(test_name);
        let disqualification_arbiter = DisqualificationArbiter::default();
        let disqualification_limit_2 =
            disqualification_arbiter.calculate_disqualification_edge(&account_2);
        // This is exactly the amount which will provoke an error
        let cw_service_fee_balance_minor = disqualification_limit_2 - 1;
        let qualified_payables = vec![account_1, account_2, account_3];
        let analyzed_payables = convert_collection(qualified_payables);
        let agent = BlockchainAgentMock::default()
            .service_fee_balance_minor_result(cw_service_fee_balance_minor);
        let transaction_count_limit = 2;
        let adjustment_setup = PreparedAdjustment {
            agent: Box::new(agent),
            adjustment_analysis: AdjustmentAnalysisReport::new(
                Adjustment::BeginByTransactionFee {
                    transaction_count_limit,
                },
                analyzed_payables,
            ),
            response_skeleton_opt: None,
        };

        let result = subject.adjust_payments(adjustment_setup, now);

        let err = match result {
            Ok(_) => panic!("expected an error but got Ok()"),
            Err(e) => e,
        };
        assert_eq!(
            err,
            PaymentAdjusterError::LateNotEnoughFeeForSingleTransaction {
                original_number_of_accounts: 3,
                number_of_accounts: 2,
                original_service_fee_required_total_minor: balance_1 + balance_2 + balance_3,
                cw_service_fee_balance_minor
            }
        );
        TestLogHandler::new().assert_logs_contain_in_order(vec![
            &format!(
                "WARN: {test_name}: Mature payables amount to 411,000,000,000,000,000,000 MASQ \
                wei while the consuming wallet holds only 70,999,999,999,999,999,999 wei. \
                Adjustment in their count or balances is necessary."
            ),
            &format!(
                "INFO: {test_name}: Please be aware that abandoning your debts is going to \
            result in delinquency bans. In order to consume services without limitations, you \
            will need to place more funds into your consuming wallet.",
            ),
            &format!(
                "ERROR: {test_name}: {}",
                LATER_DETECTED_SERVICE_FEE_SEVERE_SCARCITY
            ),
        ]);
    }

    struct TestConfigForServiceFeeBalances {
        // Either gwei or wei
        account_balances: Either<Vec<u64>, Vec<u128>>,
        cw_balance_minor: u128,
    }

    struct TestConfigForTransactionFees {
        gas_price_major: u64,
        number_of_accounts: usize,
        estimated_transaction_fee_units_per_transaction: u64,
        cw_transaction_fee_balance_minor: u128,
    }

    fn make_input_for_initial_check_tests(
        service_fee_balances_config_opt: Option<TestConfigForServiceFeeBalances>,
        transaction_fee_config_opt: Option<TestConfigForTransactionFees>,
    ) -> (Vec<QualifiedPayableAccount>, Box<dyn BlockchainAgent>) {
        let service_fee_balances_config =
            get_service_fee_balances_config(service_fee_balances_config_opt);
        let balances_of_accounts_minor =
            get_service_fee_balances(service_fee_balances_config.account_balances);
        let accounts_count_from_sf_config = balances_of_accounts_minor.len();

        let transaction_fee_config =
            get_transaction_fee_config(transaction_fee_config_opt, accounts_count_from_sf_config);

        let payable_accounts = prepare_payable_accounts(
            transaction_fee_config.number_of_accounts,
            accounts_count_from_sf_config,
            balances_of_accounts_minor,
        );
        let qualified_payables = prepare_qualified_payables(payable_accounts);

        let blockchain_agent = make_agent(
            transaction_fee_config.cw_transaction_fee_balance_minor,
            transaction_fee_config.estimated_transaction_fee_units_per_transaction,
            transaction_fee_config.gas_price_major,
            service_fee_balances_config.cw_balance_minor,
        );

        (qualified_payables, blockchain_agent)
    }

    fn get_service_fee_balances_config(
        service_fee_balances_config_opt: Option<TestConfigForServiceFeeBalances>,
    ) -> TestConfigForServiceFeeBalances {
        service_fee_balances_config_opt.unwrap_or_else(|| TestConfigForServiceFeeBalances {
            account_balances: Either::Left(vec![1, 1]),
            cw_balance_minor: u64::MAX as u128,
        })
    }
    fn get_service_fee_balances(account_balances: Either<Vec<u64>, Vec<u128>>) -> Vec<u128> {
        match account_balances {
            Either::Left(in_major) => in_major
                .into_iter()
                .map(|major| multiply_by_billion(major as u128))
                .collect(),
            Either::Right(in_minor) => in_minor,
        }
    }

    fn get_transaction_fee_config(
        transaction_fee_config_opt: Option<TestConfigForTransactionFees>,
        accounts_count_from_sf_config: usize,
    ) -> TestConfigForTransactionFees {
        transaction_fee_config_opt.unwrap_or(TestConfigForTransactionFees {
            gas_price_major: 120,
            number_of_accounts: accounts_count_from_sf_config,
            estimated_transaction_fee_units_per_transaction: 55_000,
            cw_transaction_fee_balance_minor: u128::MAX,
        })
    }

    fn prepare_payable_accounts(
        accounts_count_from_tf_config: usize,
        accounts_count_from_sf_config: usize,
        balances_of_accounts_minor: Vec<u128>,
    ) -> Vec<PayableAccount> {
        if accounts_count_from_tf_config != accounts_count_from_sf_config {
            (0..accounts_count_from_tf_config)
                .map(|idx| make_payable_account(idx as u64))
                .collect()
        } else {
            balances_of_accounts_minor
                .into_iter()
                .enumerate()
                .map(|(idx, balance)| {
                    let mut account = make_payable_account(idx as u64);
                    account.balance_wei = balance;
                    account
                })
                .collect()
        }
    }

    fn prepare_qualified_payables(
        payable_accounts: Vec<PayableAccount>,
    ) -> Vec<QualifiedPayableAccount> {
        payable_accounts
            .into_iter()
            .map(|payable| {
                let balance = payable.balance_wei;
                QualifiedPayableAccount {
                    bare_account: payable,
                    payment_threshold_intercept_minor: (balance / 10) * 7,
                    creditor_thresholds: CreditorThresholds {
                        permanent_debt_allowed_minor: (balance / 10) * 7,
                    },
                }
            })
            .collect()
    }

    fn make_agent(
        cw_transaction_fee_minor: u128,
        estimated_transaction_fee_units_per_transaction: u64,
        gas_price: u64,
        cw_service_fee_balance_minor: u128,
    ) -> Box<dyn BlockchainAgent> {
        let estimated_transaction_fee_per_transaction_minor = multiply_by_billion(
            (estimated_transaction_fee_units_per_transaction * gas_price) as u128,
        );

        let blockchain_agent = BlockchainAgentMock::default()
            .gas_price_margin_result(*TRANSACTION_FEE_MARGIN)
            .transaction_fee_balance_minor_result(cw_transaction_fee_minor.into())
            .service_fee_balance_minor_result(cw_service_fee_balance_minor)
            .estimated_transaction_fee_per_transaction_minor_result(
                estimated_transaction_fee_per_transaction_minor,
            );

        Box::new(blockchain_agent)
    }

    fn reconstruct_mock_agent(boxed: Box<dyn BlockchainAgent>) -> BlockchainAgentMock {
        BlockchainAgentMock::default()
            .gas_price_margin_result(boxed.gas_price_margin())
            .transaction_fee_balance_minor_result(boxed.transaction_fee_balance_minor())
            .service_fee_balance_minor_result(boxed.service_fee_balance_minor())
            .estimated_transaction_fee_per_transaction_minor_result(
                boxed.estimated_transaction_fee_per_transaction_minor(),
            )
    }

    fn test_inner_was_reset_to_null(subject: PaymentAdjusterReal) {
        let err = catch_unwind(AssertUnwindSafe(|| {
            subject.inner.original_cw_service_fee_balance_minor()
        }))
        .unwrap_err();
        let panic_msg = err.downcast_ref::<String>().unwrap();
        assert_eq!(
            panic_msg,
            "The PaymentAdjuster Inner is uninitialised. It was detected while executing \
            original_cw_service_fee_balance_minor()"
        )
    }

    // The following tests together prove the use of correct calculators in the production code

    #[test]
    fn each_of_defaulted_calculators_returns_different_value() {
        let now = SystemTime::now();
        let payment_adjuster = PaymentAdjusterReal::default();
        let qualified_payable = QualifiedPayableAccount {
            bare_account: PayableAccount {
                wallet: make_wallet("abc"),
                balance_wei: multiply_by_billion(444_666_888),
                last_paid_timestamp: now.checked_sub(Duration::from_secs(123_000)).unwrap(),
                pending_payable_opt: None,
            },
            payment_threshold_intercept_minor: multiply_by_billion(20_000),
            creditor_thresholds: CreditorThresholds::new(multiply_by_billion(10_000)),
        };
        let cw_service_fee_balance_minor = multiply_by_billion(3_000);
        let exceeding_balance = qualified_payable.bare_account.balance_wei
            - qualified_payable.payment_threshold_intercept_minor;
        let context = PaymentAdjusterInnerReal::new(
            now,
            None,
            cw_service_fee_balance_minor,
            exceeding_balance,
        );
        let _ = payment_adjuster
            .calculators
            .into_iter()
            .map(|calculator| calculator.calculate(&qualified_payable, &context))
            .fold(0, |previous_result, current_result| {
                let min = (current_result * 97) / 100;
                let max = (current_result * 97) / 100;
                assert_ne!(current_result, 0);
                assert!(min <= previous_result || previous_result <= max);
                current_result
            });
    }

    type InputMatrixConfigurator = fn(
        (QualifiedPayableAccount, QualifiedPayableAccount, SystemTime),
    ) -> Vec<[(QualifiedPayableAccount, u128); 2]>;

    #[test]
    fn defaulted_calculators_react_on_correct_params() {
        // When adding a test case for a new calculator, you need to make an array of inputs. Don't
        // create brand-new accounts but clone the provided nominal accounts and modify them
        // accordingly. Modify only those parameters that affect your calculator.
        // It's recommended to orientate the modifications rather positively (additions), because
        // there is a smaller chance you would run into some limit
        let input_matrix: InputMatrixConfigurator =
            |(nominal_account_1, nominal_account_2, _now)| {
                vec![
                    // First test case: BalanceCalculator
                    {
                        let mut account_1 = nominal_account_1;
                        account_1.bare_account.balance_wei += 123_456_789;
                        let mut account_2 = nominal_account_2;
                        account_2.bare_account.balance_wei += 999_999_999;
                        [(account_1, 8000001876543209), (account_2, 8000000999999999)]
                    },
                ]
            };
        // This is the value that is computed if the account stays unmodified. Same for both nominal
        // accounts.
        let current_nominal_weight = 8000000000000000;

        test_calculators_reactivity(input_matrix, current_nominal_weight)
    }

    #[derive(Clone, Copy)]
    struct TemplateComputedWeight {
        common_weight: u128,
    }

    struct ExpectedWeightWithWallet {
        wallet: Wallet,
        weight: u128,
    }

    fn test_calculators_reactivity(
        input_matrix_configurator: InputMatrixConfigurator,
        nominal_weight: u128,
    ) {
        let calculators_count = PaymentAdjusterReal::default().calculators.len();
        let now = SystemTime::now();
        let cw_service_fee_balance_minor = multiply_by_billion(1_000_000);
        let (template_accounts, template_computed_weight) =
            prepare_nominal_data_before_loading_actual_test_input(
                now,
                cw_service_fee_balance_minor,
            );
        assert_eq!(template_computed_weight.common_weight, nominal_weight);
        let mut template_accounts = template_accounts.to_vec();
        let mut pop_account = || template_accounts.remove(0);
        let nominal_account_1 = pop_account();
        let nominal_account_2 = pop_account();
        let input_matrix = input_matrix_configurator((nominal_account_1, nominal_account_2, now));
        assert_eq!(
            input_matrix.len(),
            calculators_count,
            "If you've recently added in a new calculator, you should add in its new test case to \
            this test. See the input matrix, it is the place where you should use the two accounts \
            you can clone. Make sure you modify only those parameters processed by your new calculator "
        );
        test_accounts_from_input_matrix(
            input_matrix,
            now,
            cw_service_fee_balance_minor,
            template_computed_weight,
        )
    }

    fn prepare_nominal_data_before_loading_actual_test_input(
        now: SystemTime,
        cw_service_fee_balance_minor: u128,
    ) -> ([QualifiedPayableAccount; 2], TemplateComputedWeight) {
        let template_accounts = initialize_template_accounts(now);
        let template_weight = compute_common_weight_for_templates(
            template_accounts.clone(),
            now,
            cw_service_fee_balance_minor,
        );
        (template_accounts, template_weight)
    }

    fn initialize_template_accounts(now: SystemTime) -> [QualifiedPayableAccount; 2] {
        let make_qualified_payable = |wallet| QualifiedPayableAccount {
            bare_account: PayableAccount {
                wallet,
                balance_wei: multiply_by_billion(20_000_000),
                last_paid_timestamp: now.checked_sub(Duration::from_secs(10_000)).unwrap(),
                pending_payable_opt: None,
            },
            payment_threshold_intercept_minor: multiply_by_billion(12_000_000),
            creditor_thresholds: CreditorThresholds::new(multiply_by_billion(1_000_000)),
        };

        [
            make_qualified_payable(make_wallet("abc")),
            make_qualified_payable(make_wallet("def")),
        ]
    }

    fn compute_common_weight_for_templates(
        template_accounts: [QualifiedPayableAccount; 2],
        now: SystemTime,
        cw_service_fee_balance_minor: u128,
    ) -> TemplateComputedWeight {
        let template_results = exercise_production_code_to_get_weighted_accounts(
            template_accounts.to_vec(),
            now,
            cw_service_fee_balance_minor,
        );
        let templates_common_weight = template_results
            .iter()
            .map(|account| account.weight)
            .reduce(|previous, current| {
                assert_eq!(previous, current);
                current
            })
            .unwrap();
        // Formal test if the value is different from zero,
        // and ideally much bigger than that
        assert!(1_000_000_000_000 < templates_common_weight);
        TemplateComputedWeight {
            common_weight: templates_common_weight,
        }
    }

    fn exercise_production_code_to_get_weighted_accounts(
        qualified_payables: Vec<QualifiedPayableAccount>,
        now: SystemTime,
        cw_service_fee_balance_minor: u128,
    ) -> Vec<WeightedPayable> {
        let analyzed_payables = convert_collection(qualified_payables);
        let max_debt_above_threshold_in_qualified_payables =
            find_largest_exceeding_balance(&analyzed_payables);
        let mut subject = PaymentAdjusterTestBuilder::default()
            .now(now)
            .cw_service_fee_balance_minor(cw_service_fee_balance_minor)
            .max_debt_above_threshold_in_qualified_payables(
                max_debt_above_threshold_in_qualified_payables,
            )
            .build();
        let perform_adjustment_by_service_fee_params_arc = Arc::new(Mutex::new(Vec::new()));
        let service_fee_adjuster_mock = ServiceFeeAdjusterMock::default()
            // We use this container to intercept those values we are after
            .perform_adjustment_by_service_fee_params(&perform_adjustment_by_service_fee_params_arc)
            // This is just a sentinel that allows us to shorten the adjustment execution.
            // We care only for the params captured inside the container from above
            .perform_adjustment_by_service_fee_result(AdjustmentIterationResult {
                decided_accounts: vec![],
                remaining_undecided_accounts: vec![],
            });
        subject.service_fee_adjuster = Box::new(service_fee_adjuster_mock);

        let result = subject.run_adjustment(analyzed_payables);

        less_important_constant_assertions_and_weighted_accounts_extraction(
            result,
            perform_adjustment_by_service_fee_params_arc,
            cw_service_fee_balance_minor,
        )
    }

    fn less_important_constant_assertions_and_weighted_accounts_extraction(
        actual_result: Result<Vec<PayableAccount>, PaymentAdjusterError>,
        perform_adjustment_by_service_fee_params_arc: Arc<Mutex<Vec<(Vec<WeightedPayable>, u128)>>>,
        cw_service_fee_balance_minor: u128,
    ) -> Vec<WeightedPayable> {
        // This error should be ignored, as it has no real meaning.
        // It allows to halt the code executions without a dive in the recursion
        assert_eq!(
            actual_result,
            Err(PaymentAdjusterError::RecursionDrainedAllAccounts)
        );
        let mut perform_adjustment_by_service_fee_params =
            perform_adjustment_by_service_fee_params_arc.lock().unwrap();
        let (weighted_accounts, captured_cw_service_fee_balance_minor) =
            perform_adjustment_by_service_fee_params.remove(0);
        assert_eq!(
            captured_cw_service_fee_balance_minor,
            cw_service_fee_balance_minor
        );
        assert!(perform_adjustment_by_service_fee_params.is_empty());
        weighted_accounts
    }

    fn test_accounts_from_input_matrix(
        input_matrix: Vec<[(QualifiedPayableAccount, u128); 2]>,
        now: SystemTime,
        cw_service_fee_balance_minor: u128,
        template_computed_weight: TemplateComputedWeight,
    ) {
        fn prepare_args_expected_weights_for_comparison(
            (qualified_payable, expected_computed_weight): (QualifiedPayableAccount, u128),
        ) -> (QualifiedPayableAccount, ExpectedWeightWithWallet) {
            let wallet = qualified_payable.bare_account.wallet.clone();
            let expected_weight = ExpectedWeightWithWallet {
                wallet,
                weight: expected_computed_weight,
            };
            (qualified_payable, expected_weight)
        }

        input_matrix
            .into_iter()
            .map(|test_case| {
                test_case
                    .into_iter()
                    .map(prepare_args_expected_weights_for_comparison)
                    .collect::<Vec<_>>()
            })
            .for_each(|qualified_payments_and_expected_computed_weights| {
                let (qualified_payments, expected_computed_weights): (Vec<_>, Vec<_>) =
                    qualified_payments_and_expected_computed_weights
                        .into_iter()
                        .unzip();

                let weighted_accounts = exercise_production_code_to_get_weighted_accounts(
                    qualified_payments,
                    now,
                    cw_service_fee_balance_minor,
                );

                assert_results(
                    weighted_accounts,
                    expected_computed_weights,
                    template_computed_weight,
                )
            });
    }

    fn make_comparison_hashmap(
        weighted_accounts: Vec<WeightedPayable>,
    ) -> HashMap<Wallet, WeightedPayable> {
        let feeding_iterator = weighted_accounts
            .into_iter()
            .map(|account| (account.wallet().clone(), account));
        HashMap::from_iter(feeding_iterator)
    }

    fn assert_results(
        weighted_accounts: Vec<WeightedPayable>,
        expected_computed_weights: Vec<ExpectedWeightWithWallet>,
        template_computed_weight: TemplateComputedWeight,
    ) {
        let weighted_accounts_as_hash_map = make_comparison_hashmap(weighted_accounts);
        expected_computed_weights.into_iter().fold(
            0,
            |previous_account_actual_weight, expected_account_weight| {
                let wallet = expected_account_weight.wallet;
                let actual_account = weighted_accounts_as_hash_map
                    .get(&wallet)
                    .unwrap_or_else(|| panic!("Account for wallet {:?} disappeared", wallet));
                assert_ne!(
                    actual_account.weight, template_computed_weight.common_weight,
                    "Weight is exactly the same as that one from the template. The inputs \
                    (modifications in the template accounts) are supposed to cause the weight to \
                    evaluated differently."
                );
                assert_eq!(
                    actual_account.weight,
                    expected_account_weight.weight,
                    "Computed weight {} differs from what was expected {}",
                    actual_account.weight.separate_with_commas(),
                    expected_account_weight.weight.separate_with_commas()
                );
                assert_ne!(
                    actual_account.weight, previous_account_actual_weight,
                    "You were expected to prepare two accounts with at least slightly \
                    different parameters. Therefore, the evenness of their weights is \
                    highly improbable and suspicious."
                );
                actual_account.weight
            },
        );
    }
}