use std::{fs, path::Path, sync::Mutex, time::Duration};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::{schema::SCHEMA_V1, CoreError};

pub const CORE_DB_FILE: &str = "core.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

pub struct CoreStore {
    connection: Mutex<Connection>,
}

impl CoreStore {
    pub fn open(data_dir: &Path) -> Result<Self, CoreError> {
        let database_dir = data_dir.join("data");
        fs::create_dir_all(&database_dir)?;

        let connection = Connection::open(database_dir.join(CORE_DB_FILE))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn migrate(&self) -> Result<(), CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(CoreError::migration)?;
        let version = Self::schema_version_in_transaction(&transaction)?;

        match version {
            0 => {
                transaction.execute_batch(SCHEMA_V1).map_err(CoreError::migration)?;
                transaction
                    .execute(
                        "INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?1)",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            CURRENT_SCHEMA_VERSION => {}
            version => return Err(CoreError::UnsupportedSchemaVersion { version }),
        }

        transaction.commit().map_err(CoreError::migration)
    }

    pub fn schema_version(&self) -> Result<u32, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let value = connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        match value {
            Some(value) => value
                .parse()
                .map_err(|_| CoreError::InvalidSchemaVersion { value }),
            None => Ok(0),
        }
    }

    pub fn foreign_keys_enabled(&self) -> Result<bool, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let enabled = connection.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))?;
        Ok(enabled == 1)
    }

    pub fn table_count(&self, table_name: &str) -> Result<u32, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let count = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get::<_, u32>(0),
        )?;
        Ok(count)
    }

    fn schema_version_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<u32, CoreError> {
        let schema_meta_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !schema_meta_exists {
            return Ok(0);
        }

        let value = transaction
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        match value {
            Some(value) => value
                .parse()
                .map_err(|_| CoreError::InvalidSchemaVersion { value }),
            None => Ok(0),
        }
    }
}

impl CoreError {
    fn migration(source: rusqlite::Error) -> Self {
        Self::Migration { source }
    }
}
