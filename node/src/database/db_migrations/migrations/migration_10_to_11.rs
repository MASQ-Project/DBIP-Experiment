use crate::database::db_migrations::db_migrator::DatabaseMigration;
use crate::database::db_migrations::migrator_utils::DBMigDeclarator;

#[allow(non_camel_case_types)]
pub struct Migrate_10_to_11;

impl DatabaseMigration for Migrate_10_to_11 {
    fn migrate<'a>(
        &self,
        declaration_utils: Box<dyn DBMigDeclarator + 'a>,
    ) -> rusqlite::Result<()> {
        // todo!("test drive me");

        let sql_statement = "create table if not exists sent_payable (
                rowid integer primary key,
                tx_hash text not null,
                receiver_address text not null,
                amount_high_b integer not null,
                amount_low_b integer not null,
                timestamp integer not null,
                gas_price_wei integer not null,
                nonce integer not null,
                status text not null,
                retried integer not null
            )";

        declaration_utils.execute_upon_transaction(&[&sql_statement])
    }

    fn old_version(&self) -> usize {
        10
    }
}

#[cfg(test)]
mod tests {
    use crate::database::db_initializer::{
        DbInitializationConfig, DbInitializer, DbInitializerReal, DATABASE_FILE,
    };
    use crate::database::rusqlite_wrappers::ConnectionWrapper;
    use crate::test_utils::database_utils::{
        bring_db_0_back_to_life_and_return_connection, make_external_data, retrieve_config_row,
        retrieve_sent_payable_row,
    };
    use masq_lib::test_utils::logging::{init_test_logging, TestLogHandler};
    use masq_lib::test_utils::utils::ensure_node_home_directory_exists;
    use std::fs::create_dir_all;

    fn assert_table_exists(conn: &dyn ConnectionWrapper, table_name: &str) {
        let result = conn.prepare(&format!("SELECT 1 FROM {} LIMIT 1", table_name));
        assert!(result.is_ok(), "Table {} should exist", table_name);
    }

    fn assert_column_exists(
        connection: &dyn ConnectionWrapper,
        table_name: &str,
        column_name: &str,
    ) {
        let query = format!("PRAGMA table_info({})", table_name);
        let mut stmt = connection
            .prepare(&query)
            .expect("Failed to prepare statement");
        let column_info_iter = stmt
            .query_map([], |row| {
                let name: String = row.get(1)?;
                Ok(name)
            })
            .expect("Failed to query column info");

        let column_names: Vec<String> = column_info_iter.filter_map(Result::ok).collect();
        assert!(
            column_names.contains(&column_name.to_string()),
            "Column '{}' does not exist in table '{}'",
            column_name,
            table_name
        );
    }

    #[test]
    fn migration_from_10_to_11_is_applied_correctly() {
        init_test_logging();
        let dir_path = ensure_node_home_directory_exists(
            "db_migrations",
            "migration_from_10_to_11_is_properly_set",
        );
        create_dir_all(&dir_path).unwrap();
        let db_path = dir_path.join(DATABASE_FILE);
        let _ = bring_db_0_back_to_life_and_return_connection(&db_path);
        let subject = DbInitializerReal::default();

        let result = subject.initialize_to_version(
            &dir_path,
            10,
            DbInitializationConfig::create_or_migrate(make_external_data()),
        );

        assert!(result.is_ok());

        let result = subject.initialize_to_version(
            &dir_path,
            11,
            DbInitializationConfig::create_or_migrate(make_external_data()),
        );

        let connection = result.unwrap();
        let expected_columns = vec![
            "rowid",
            "tx_hash",
            "receiver_address",
            "amount_high_b",
            "amount_low_b",
            "timestamp",
            "gas_price_wei",
            "nonce",
            "status",
            "retried",
        ];

        assert_table_exists(connection.as_ref(), "sent_payable");
        for column in expected_columns {
            assert_column_exists(connection.as_ref(), "sent_payable", column);
        }
        TestLogHandler::new().assert_logs_contain_in_order(vec![
            "DbMigrator: Database successfully migrated from version 10 to 11",
        ]);
    }
}
