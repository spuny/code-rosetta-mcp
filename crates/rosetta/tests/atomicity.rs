use rosetta::{index, storage::Storage};

#[tokio::test]
async fn failed_publication_rolls_back_metadata_revision_and_files() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("atomic.db");
    let source = tempfile::tempdir().unwrap();
    std::fs::write(
        source.path().join("original.py"),
        "def original():\n    pass\n",
    )
    .unwrap();
    let storage = Storage::sqlite(&database).await.unwrap();
    index::index(&storage, source.path(), "atomic", None)
        .await
        .unwrap();
    let before = storage.load().await.unwrap();
    let connection = sqlx::SqlitePool::connect(&format!("sqlite://{}", database.display()))
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fail_publication BEFORE INSERT ON rosetta_files WHEN NEW.path='explode.txt' BEGIN SELECT RAISE(ABORT, 'synthetic failure'); END")
        .execute(&connection).await.unwrap();
    std::fs::write(source.path().join("explode.txt"), "new content\n").unwrap();
    assert!(
        index::index(&storage, source.path(), "atomic", None)
            .await
            .is_err()
    );
    let after = storage.load().await.unwrap();
    assert_eq!(after.revision, before.revision);
    assert_eq!(
        after.repositories["atomic"].generation,
        before.repositories["atomic"].generation
    );
    assert_eq!(
        after.repositories["atomic"]
            .files
            .keys()
            .collect::<Vec<_>>(),
        before.repositories["atomic"]
            .files
            .keys()
            .collect::<Vec<_>>()
    );
    sqlx::query("DROP TRIGGER fail_publication")
        .execute(&connection)
        .await
        .unwrap();
    index::index(&storage, source.path(), "atomic", None)
        .await
        .unwrap();
    assert!(
        storage.load().await.unwrap().repositories["atomic"]
            .files
            .contains_key("explode.txt")
    );
    connection.close().await;
}

#[tokio::test]
async fn unsupported_schema_is_rejected_without_mutating_it() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("future.db");
    let storage = Storage::sqlite(&database).await.unwrap();
    let connection = sqlx::SqlitePool::connect(&format!("sqlite://{}", database.display()))
        .await
        .unwrap();
    sqlx::query("UPDATE rosetta_meta SET version=999 WHERE id=1")
        .execute(&connection)
        .await
        .unwrap();
    assert!(Storage::sqlite(&database).await.is_err());
    let version: i64 = sqlx::query_scalar("SELECT version FROM rosetta_meta WHERE id=1")
        .fetch_one(&connection)
        .await
        .unwrap();
    assert_eq!(version, 999);
    assert_eq!(storage.revision().await.unwrap(), 0);
    connection.close().await;
}
