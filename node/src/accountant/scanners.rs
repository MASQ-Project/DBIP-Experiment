// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

pub(in crate::accountant) mod scanners {
    use crate::accountant::payable_dao::PayableDao;
    use crate::accountant::{
        Accountant, CancelFailedPendingTransaction, ConfirmPendingTransaction, ReceivedPayments,
        ReportTransactionReceipts, RequestTransactionReceipts, ResponseSkeleton, ScanForPayables,
        ScanForPendingPayables, ScanForReceivables, SentPayable,
    };
    use crate::blockchain::blockchain_bridge::RetrieveTransactions;
    use crate::sub_lib::blockchain_bridge::ReportAccountsPayable;
    use crate::sub_lib::utils::{NotifyHandle, NotifyLaterHandle};
    use actix::dev::SendError;
    use actix::{Context, Message, Recipient};
    use masq_lib::logger::timestamp_as_string;
    use masq_lib::messages::ScanType;
    use std::cell::RefCell;
    use std::time::SystemTime;

    type Error = String;

    pub struct Scanners {
        pub payables: Box<dyn Scanner<ReportAccountsPayable, SentPayable>>,
        pub pending_payables:
            Box<dyn Scanner<RequestTransactionReceipts, ReportTransactionReceipts>>,
        pub receivables: Box<dyn Scanner<RetrieveTransactions, ReceivedPayments>>,
    }

    //
    // pub struct Scanners {
    //     pub payables: Box<dyn Scanner<ReportAccountsPayable, SentPayable>>,
    //     pub pending_payable:
    //     Box<dyn Scanner<RequestTransactionReceipts, ReportTransactionReceipts>>,
    //     pub receivables: Box<dyn Scanner<RetrieveTransactions, ReceivedPayments>>,
    // }

    impl Default for Scanners {
        fn default() -> Self {
            todo!()
        }
    }

    // struct ScannerADao {}
    // struct ScannerBDao {}
    //
    // struct BeginScanAMessage{
    //
    // }
    //
    // impl Message for BeginScanAMessage{}
    //
    // struct FinishScanAMessage {
    //
    // }
    //
    // impl Message for FinishScanAMessage{}
    //
    // struct BeginScanBMessage {
    //
    // }
    //
    // impl Message for BeginScanBMessage{}
    //
    // struct FinishScanBMessage {
    //
    // }
    //
    // impl Message for FinishScanAMessage{}

    pub trait Scanner<BeginMessage, EndMessage>
    where
        BeginMessage: Message + Send + 'static,
        BeginMessage::Result: Send,
        EndMessage: Message,
    {
        fn begin_scan(
            &mut self,
            timestamp: SystemTime,
            response_skeleton_opt: Option<ResponseSkeleton>,
            ctx: &mut Context<Accountant>,
        ) -> Result<Box<dyn BeginMessageWrapper<BeginMessage>>, Error>;
        fn scan_finished(&mut self, message: EndMessage) -> Result<(), Error>;
        fn scan_started_at(&self) -> Option<SystemTime>;
    }

    struct ScannerCommon {
        initiated_at_opt: Option<SystemTime>,
    }

    impl Default for ScannerCommon {
        fn default() -> Self {
            Self {
                initiated_at_opt: None,
            }
        }
    }

    pub struct PayableScanner {
        common: ScannerCommon,
        dao: Box<dyn PayableDao>,
    }

    impl<BeginMessage, EndMessage> Scanner<BeginMessage, EndMessage> for PayableScanner
    where
        BeginMessage: Message + Send + 'static,
        BeginMessage::Result: Send,
        EndMessage: Message,
    {
        fn begin_scan(
            &mut self,
            timestamp: SystemTime,
            response_skeleton_opt: Option<ResponseSkeleton>,
            ctx: &mut Context<Accountant>,
        ) -> Result<Box<dyn BeginMessageWrapper<BeginMessage>>, Error> {
            todo!()
            // common::start_scan_at(&mut self.common, timestamp);
            // let start_message = BeginScanAMessage {};
            // // Use the DAO, if necessary, to populate start_message
            // Ok(start_message)
        }

        fn scan_finished(&mut self, message: EndMessage) -> Result<(), Error> {
            todo!()
            // Use the passed-in message and the internal DAO to finish the scan
            // Ok(())
        }

        fn scan_started_at(&self) -> Option<SystemTime> {
            todo!()
            // common::scan_started_at(&self.common)
        }
    }

    impl PayableScanner {
        pub fn new(dao: Box<dyn PayableDao>) -> Self {
            Self {
                common: ScannerCommon::default(),
                dao,
            }
        }
    }

    // pub struct ScannerB {
    //     common: ScannerCommon,
    //     dao: ScannerBDao,
    // }
    //
    // impl ScannerB {
    //     pub fn new(dao: ScannerBDao) -> Self {
    //         Self {
    //             common: ScannerCommon::default(),
    //             dao,
    //         }
    //     }
    // }
    //
    // mod common {
    //     use crate::scanner_experiment::ScannerCommon;
    //     use std::time::SystemTime;
    //
    //     pub fn scan_started_at(scanner: &ScannerCommon) -> Option<DateTime> {
    //         scanner.initiated_at_opt
    //     }
    //
    //     pub fn start_scan_at(scanner: &mut ScannerCommon, timestamp: SystemTime) {
    //         if let Some(initiated_at) = scanner.initiated_at_opt {
    //             panic! ("Scan {:?} has been running for {:?} seconds; it cannot be restarted until it finishes.", "blah", SystemTime::now().duration_since(initiated_at));
    //         }
    //         scanner.initiated_at_opt = Some(timestamp);
    //     }
    // }

    pub trait BeginMessageWrapper<BeginMessage>
    where
        BeginMessage: Message + Send + 'static,
        BeginMessage::Result: Send,
    {
        fn try_send(
            &mut self,
            recipient: &Recipient<BeginMessage>,
        ) -> Result<(), SendError<BeginMessage>>;
    }

    pub struct NullScanner {}

    impl<BeginMessage, EndMessage> Scanner<BeginMessage, EndMessage> for NullScanner
    where
        BeginMessage: Message + Send + 'static,
        BeginMessage::Result: Send,
        EndMessage: Message,
    {
        fn begin_scan(
            &mut self,
            timestamp: SystemTime,
            response_skeleton_opt: Option<ResponseSkeleton>,
            ctx: &mut Context<Accountant>,
        ) -> Result<Box<dyn BeginMessageWrapper<BeginMessage>>, Error> {
            todo!()
        }

        fn scan_finished(&mut self, message: EndMessage) -> Result<(), Error> {
            todo!()
        }

        fn scan_started_at(&self) -> Option<SystemTime> {
            todo!()
        }
    }

    #[derive(Default)]
    pub struct NotifyLaterForScanners {
        pub scan_for_pending_payable:
            Box<dyn NotifyLaterHandle<ScanForPendingPayables, Accountant>>,
        pub scan_for_payable: Box<dyn NotifyLaterHandle<ScanForPayables, Accountant>>,
        pub scan_for_receivable: Box<dyn NotifyLaterHandle<ScanForReceivables, Accountant>>,
    }

    #[derive(Default)]
    pub struct TransactionConfirmationTools {
        pub notify_confirm_transaction:
            Box<dyn NotifyHandle<ConfirmPendingTransaction, Accountant>>,
        pub notify_cancel_failed_transaction:
            Box<dyn NotifyHandle<CancelFailedPendingTransaction, Accountant>>,
        pub request_transaction_receipts_subs_opt: Option<Recipient<RequestTransactionReceipts>>,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accountant::payable_dao::PayableDaoReal;
    use crate::accountant::scanners::scanners::PayableScanner;
    use crate::accountant::test_utils::PayableDaoMock;

    #[test]
    fn payable_scanner_can_be_constructed() {
        let payable_dao = PayableDaoMock::new();

        let payable_scanner = PayableScanner::new(Box::new(payable_dao));
    }
}
