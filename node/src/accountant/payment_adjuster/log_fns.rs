// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::accountant::database_access_objects::payable_dao::PayableAccount;
use crate::accountant::payment_adjuster::DisqualifiedPayableAccount;
use crate::accountant::scanners::payable_scan_setup_msgs::FinancialAndTechDetails;
use crate::masq_lib::utils::ExpectValue;
use crate::sub_lib::wallet::Wallet;
use itertools::Itertools;
use masq_lib::constants::WALLET_ADDRESS_LENGTH;
use masq_lib::logger::Logger;
use std::collections::HashMap;
use std::iter::once;
use std::ops::Not;
use thousands::Separable;

const REFILL_RECOMMENDATION: &str = "\
In order to continue using services of other Nodes and avoid delinquency \
bans you will need to put more funds into your consuming wallet.";

const NO_CHARS: &str = "";

pub fn format_brief_adjustment_summary(
    original_account_balances_mapped: HashMap<Wallet, u128>,
    adjusted_accounts: &[PayableAccount],
) -> String {
    fn format_summary_for_included_accounts(
        original_account_balances_mapped: &HashMap<Wallet, u128>,
        adjusted_accounts: &[PayableAccount],
    ) -> String {
        adjusted_accounts
            .into_iter()
            .sorted_by(|account_a, account_b| {
                Ord::cmp(&account_b.balance_wei, &account_a.balance_wei)
            })
            .map(|account| {
                format!(
                    "{} {}\n{:^length$} {}",
                    account.wallet,
                    original_account_balances_mapped
                        .get(&account.wallet)
                        .expectv("initial balance"),
                    NO_CHARS,
                    account.balance_wei,
                    length = WALLET_ADDRESS_LENGTH
                )
            })
            .join("\n")
    }
    fn format_summary_for_excluded_accounts(excluded: &[(&Wallet, u128)]) -> String {
        let title = once(format!(
            "\n{:<length$} Original\n",
            "Ignored minor payables",
            length = WALLET_ADDRESS_LENGTH
        ));
        let list = excluded
            .into_iter()
            .sorted_by(|(_, balance_account_a), (_, balance_account_b)| {
                Ord::cmp(&balance_account_b, &balance_account_a)
            })
            .map(|(wallet, original_balance)| format!("{} {}", wallet, original_balance));
        title.chain(list).join("\n")
    }

    let adjusted_accounts_wallets: Vec<&Wallet> = adjusted_accounts
        .iter()
        .map(|account| &account.wallet)
        .collect();
    let excluded: Vec<(&Wallet, u128)> = original_account_balances_mapped.iter().fold(
        vec![],
        |mut acc, (wallet, original_balance)| {
            if !adjusted_accounts_wallets.contains(&wallet) {
                acc.push((wallet, *original_balance));
            }
            acc
        },
    );
    let adjusted_accounts_summary =
        format_summary_for_included_accounts(&original_account_balances_mapped, adjusted_accounts);
    let excluded_accounts_summary_opt = excluded
        .is_empty()
        .not()
        .then(|| format_summary_for_excluded_accounts(&excluded));
    vec![
        Some(adjusted_accounts_summary),
        excluded_accounts_summary_opt,
    ]
    .into_iter()
    .flatten()
    .join("\n")
}

pub fn before_and_after_debug_msg(
    original_account_balances_mapped: HashMap<Wallet, u128>,
    adjusted_accounts: &[PayableAccount],
) -> String {
    format!(
        "\n\
            {:<length$} {}\n\
            \n\
            {:<length$} {}\n\
            {:<length$} {}\n\
            \n\
            {}",
        "Account wallet",
        "Balance wei",
        "Adjusted payables",
        "Original",
        NO_CHARS,
        "Adjusted",
        format_brief_adjustment_summary(original_account_balances_mapped, adjusted_accounts),
        length = WALLET_ADDRESS_LENGTH
    )
}

pub fn log_info_for_disqualified_account(
    logger: &Logger,
    disqualified_account_opt: Option<&DisqualifiedPayableAccount>,
) {
    disqualified_account_opt.map(|account| {
        info!(
                logger,
            "Consuming wallet low in MASQ balance. Recently qualified \
            payable for wallet {} will not be paid as the consuming wallet handles to provide only {} wei \
            which is not at least more than a half of the original debt {}",
                account.wallet,
                account.proposed_adjusted_balance.separate_with_commas(),
                account.original_balance.separate_with_commas()
            )
    });
}

pub fn log_adjustment_by_masq_required(logger: &Logger, payables_sum: u128, cw_masq_balance: u128) {
    warning!(
        logger,
        "Total of {} wei in MASQ was ordered while the consuming wallet held only {} wei of \
            the MASQ token. Adjustment in their count or the amounts is required.",
        payables_sum.separate_with_commas(),
        cw_masq_balance.separate_with_commas()
    );
    info!(logger, "{}", REFILL_RECOMMENDATION)
}

pub fn log_insufficient_transaction_fee_balance(
    logger: &Logger,
    required_transactions_count: usize,
    this_stage_data: &FinancialAndTechDetails,
    limiting_count: u16,
) {
    warning!(
        logger,
        "Gas amount {} wei cannot cover anticipated fees from sending {} \
            transactions. Maximum is {}. The payments need to be adjusted in \
            their count.",
        this_stage_data
            .consuming_wallet_balances
            .masq_tokens_wei
            .separate_with_commas(),
        required_transactions_count,
        limiting_count
    );
    info!(logger, "{}", REFILL_RECOMMENDATION)
}

#[cfg(test)]
mod tests {
    use crate::accountant::payment_adjuster::log_fns::{
        log_info_for_disqualified_account, REFILL_RECOMMENDATION,
    };
    use crate::accountant::payment_adjuster::DisqualifiedPayableAccount;
    use crate::sub_lib::wallet::Wallet;
    use crate::test_utils::make_wallet;
    use masq_lib::logger::Logger;
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use thousands::Separable;

    #[test]
    fn constants_are_correct() {
        assert_eq!(
            REFILL_RECOMMENDATION,
            "\
In order to continue using services of other Nodes and avoid delinquency \
bans you will need to put more funds into your consuming wallet."
        )
    }
}
