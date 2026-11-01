use super::*;

#[tokio::test]
async fn goal_check_migration_rejects_incompatible_acknowledgment_storage_without_advancing_version()
 {
    let version = 50;
    for (column, index) in [
        (
            "TEXT NOT NULL DEFAULT 0",
            "CREATE INDEX runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1",
        ),
        (
            "INTEGER DEFAULT NULL",
            "CREATE INDEX runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1",
        ),
        (
            "INTEGER NOT NULL DEFAULT 0",
            "CREATE INDEX runs_pending_check_ack ON runs(session_id) WHERE check_acknowledgment_pending=1",
        ),
        (
            "INTEGER NOT NULL DEFAULT 0 CHECK(check_acknowledgment_pending IN (0,1) AND (check_acknowledgment_pending=0 OR kind='goal_check'))",
            "CREATE INDEX runs_pending_check_ack ON runs(id) WHERE check_acknowledgment_pending=0",
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sqlite3");
        Store::open(path.clone())
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(&format!("DROP INDEX runs_pending_check_ack; ALTER TABLE runs DROP COLUMN check_acknowledgment_pending; ALTER TABLE runs ADD COLUMN check_acknowledgment_pending {column}; {index};")).unwrap();
        connection
            .execute(
                "UPDATE metadata SET value=?1 WHERE key='schema_version'",
                [version.to_string()],
            )
            .unwrap();
        drop(connection);
        assert!(
            matches!(
                Store::open(path.clone()).await,
                Err(SessionRuntimeError::CONSTRAINT)
            ),
            "schema {version} accepted {column} / {index}"
        );
        let connection = Connection::open(path).unwrap();
        let actual: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key='schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            actual, "56",
            "the failed step 57 must roll back its metadata"
        );
    }
}
