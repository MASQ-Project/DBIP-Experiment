// Copyright (c) 2019, MASQ (https://masq.ai) and/or its affiliates. All rights reserved.

use crate::command_context::CommandContext;
use crate::commands::commands_common::{
    transaction, Command, CommandError, STANDARD_COMMAND_TIMEOUT_MILLIS,
};
use crate::terminal::terminal_interface::TerminalWriter;
use crate::terminal::terminal_interface::WTermInterface;
use async_trait::async_trait;
use clap::{Arg, Command as ClapCommand};
use masq_lib::messages::{
    UiChangePasswordRequest, UiChangePasswordResponse, UiNewPasswordBroadcast,
};
use masq_lib::{implement_as_any, short_writeln};
#[cfg(test)]
use std::any::Any;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
pub struct ChangePasswordCommand {
    pub old_password: Option<String>,
    pub new_password: String,
}

const CHANGE_PASSWORD_ABOUT: &str = "Changes the existing password on the Node database.";
const OLD_DB_PASSWORD_HELP: &str = "The existing password.";
const NEW_DB_PASSWORD_HELP: &str = "The new password to set.";
const SET_PASSWORD_ABOUT: &str = "Sets an initial password on the Node database.";
const SET_PASSWORD_HELP: &str = "Password to be set; must not already exist.";

impl ChangePasswordCommand {
    pub fn new_set(pieces: &[String]) -> Result<Self, String> {
        match set_password_subcommand().try_get_matches_from(pieces) {
            Ok(matches) => Ok(Self {
                old_password: None,
                new_password: matches
                    .get_one::<String>("new-db-password")
                    .expect("new-db-password is not properly required")
                    .to_string(),
            }),
            Err(e) => Err(format!("{}", e)),
        }
    }

    pub fn new_change(pieces: &[String]) -> Result<Self, String> {
        match change_password_subcommand().try_get_matches_from(pieces) {
            Ok(matches) => Ok(Self {
                old_password: Some(
                    matches
                        .get_one::<String>("old-db-password")
                        .expect("old-db-password is not properly required")
                        .to_string(),
                ),
                new_password: matches
                    .get_one::<String>("new-db-password")
                    .expect("new-db-password is not properly required")
                    .to_string(),
            }),
            Err(e) => Err(format!("{}", e)),
        }
    }

    pub async fn handle_broadcast(
        _body: UiNewPasswordBroadcast,
        stdout: &TerminalWriter,
        _stderr: &TerminalWriter,
    ) {
        short_writeln!(stdout, "\nThe Node's database password has changed.\n\n");
    }
}

#[async_trait]
impl Command for ChangePasswordCommand {
    async fn execute(
        self: Box<Self>,
        context: &mut dyn CommandContext,
        term_interface: &mut dyn WTermInterface,
    ) -> Result<(), CommandError> {
        let (stdout, _stdout_flush_handle) = term_interface.stdout();
        let (stderr, _stderr_flush_handle) = term_interface.stderr();
        let input = UiChangePasswordRequest {
            old_password_opt: self.old_password.clone(),
            new_password: self.new_password.clone(),
        };
        let _: UiChangePasswordResponse =
            transaction(input, context, stderr, STANDARD_COMMAND_TIMEOUT_MILLIS).await?;
        short_writeln!(stdout, "Database password has been changed");
        Ok(())
    }

    implement_as_any!();
}

pub fn change_password_subcommand() -> ClapCommand {
    ClapCommand::new("change-password")
        .about(CHANGE_PASSWORD_ABOUT)
        .arg(
            Arg::new("old-db-password")
                .help(OLD_DB_PASSWORD_HELP)
                .value_name("OLD-DB-PASSWORD")
                .index(1)
                .required(true)
                .ignore_case(false),
        )
        .arg(
            Arg::new("new-db-password")
                .help(NEW_DB_PASSWORD_HELP)
                .value_name("NEW-DB-PASSWORD")
                .index(2)
                .required(true)
                .ignore_case(false),
        )
}

pub fn set_password_subcommand() -> ClapCommand {
    ClapCommand::new("set-password")
        .about(SET_PASSWORD_ABOUT)
        .arg(
            Arg::new("new-db-password")
                .help(SET_PASSWORD_HELP)
                .index(1)
                .required(true)
                .ignore_case(false),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command_factory::{CommandFactory, CommandFactoryError, CommandFactoryReal};
    use crate::terminal::terminal_interface::NonInteractiveWTermInterface;
    use crate::test_utils::mocks::{CommandContextMock, WTermInterfaceMock};
    use masq_lib::messages::{ToMessageBody, UiChangePasswordRequest, UiChangePasswordResponse};
    use masq_lib::test_utils::fake_stream_holder::ByteArrayHelperMethods;
    use std::sync::{Arc, Mutex};

    #[test]
    fn constants_have_correct_values() {
        assert_eq!(
            CHANGE_PASSWORD_ABOUT,
            "Changes the existing password on the Node database."
        );
        assert_eq!(OLD_DB_PASSWORD_HELP, "The existing password.");
        assert_eq!(NEW_DB_PASSWORD_HELP, "The new password to set.");
        assert_eq!(
            SET_PASSWORD_ABOUT,
            "Sets an initial password on the Node database."
        );
        assert_eq!(
            SET_PASSWORD_HELP,
            "Password to be set; must not already exist."
        );
    }

    #[tokio::test]
    async fn set_password_command_works_when_changing_from_no_password() {
        let transact_params_arc = Arc::new(Mutex::new(vec![]));
        let mut context = CommandContextMock::new()
            .transact_params(&transact_params_arc)
            .transact_result(Ok(UiChangePasswordResponse {}.tmb(0)));
        let factory = CommandFactoryReal::new();
        let mut term_interface = WTermInterfaceMock::default();
        let stdout_arc = term_interface.stdout_arc().clone();
        let stderr_arc = term_interface.stderr_arc().clone();
        let subject = factory
            .make(&["set-password".to_string(), "abracadabra".to_string()])
            .unwrap();

        let result = subject.execute(&mut context, &mut term_interface).await;

        assert_eq!(result, Ok(()));
        assert_eq!(
            stdout_arc.lock().unwrap().get_string(),
            "Database password has been changed\n"
        );
        assert_eq!(stderr_arc.lock().unwrap().get_string(), String::new());
        let transact_params = transact_params_arc.lock().unwrap();
        assert_eq!(
            *transact_params,
            vec![(
                UiChangePasswordRequest {
                    old_password_opt: None,
                    new_password: "abracadabra".to_string()
                }
                .tmb(0), // there is hard-coded 0
                1000
            )]
        )
    }

    #[tokio::test]
    async fn change_password_command_changed_db_password_successfully_with_both_parameters_supplied(
    ) {
        let transact_params_arc = Arc::new(Mutex::new(vec![]));
        let mut context = CommandContextMock::new()
            .transact_params(&transact_params_arc)
            .transact_result(Ok(UiChangePasswordResponse {}.tmb(0)));
        let factory = CommandFactoryReal::new();
        let subject = factory
            .make(&[
                "change-password".to_string(),
                "abracadabra".to_string(),
                "boringPassword".to_string(),
            ])
            .unwrap();
        let mut term_interface = WTermInterfaceMock::default();
        let stdout_arc = term_interface.stdout_arc().clone();
        let stderr_arc = term_interface.stderr_arc().clone();

        let result = subject.execute(&mut context, &mut term_interface).await;

        assert_eq!(result, Ok(()));
        assert_eq!(
            stdout_arc.lock().unwrap().get_string(),
            "Database password has been changed\n"
        );
        assert_eq!(stderr_arc.lock().unwrap().get_string(), String::new());
        let transact_params = transact_params_arc.lock().unwrap();
        assert_eq!(
            *transact_params,
            vec![(
                UiChangePasswordRequest {
                    old_password_opt: Some("abracadabra".to_string()),
                    new_password: "boringPassword".to_string()
                }
                .tmb(0),
                1000
            )]
        )
    }

    #[test]
    fn change_password_command_fails_if_only_one_argument_supplied() {
        let factory = CommandFactoryReal::new();

        let result = factory.make(&["change-password".to_string(), "abracadabra".to_string()]);

        let msg = match result {
            Err(CommandFactoryError::CommandSyntax(s)) => s,
            x => panic!("Expected CommandSyntax error, found {:?}", x),
        };
        assert_eq!(
            msg.contains("the following required arguments were not provided"),
            true,
            "{}",
            msg
        );
    }

    #[test]
    fn change_password_new_set_handles_error_of_missing_both_arguments() {
        let result = ChangePasswordCommand::new_set(&["set-password".to_string()]);

        let msg = match result {
            Err(s) => s,
            x => panic!("Expected string, found {:?}", x),
        };
        assert_eq!(
            msg.contains("the following required arguments were not provided"),
            true,
            "{}",
            msg
        );
    }

    #[test]
    fn change_password_new_change_handles_error_of_missing_both_arguments() {
        let result = ChangePasswordCommand::new_change(&["change-password".to_string()]);

        let msg = match result {
            Err(s) => s,
            x => panic!("Expected string, found {:?}", x),
        };
        assert_eq!(
            msg.contains("the following required arguments were not provided"),
            true,
            "{}",
            msg
        );
    }
}
