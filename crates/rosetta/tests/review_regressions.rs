use rosetta::{
    engine::Engine,
    index,
    storage::{File, Storage},
};
use serde_json::json;
use std::path::Path;

fn put(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

async fn credential_contract(storage: &Storage) {
    let root = tempfile::tempdir().unwrap();
    let paths = [
        ".kube/config",
        ".ssh/config",
        ".ssh/id_ecdsa",
        ".git-credentials",
        ".aws/config",
        ".docker/config.json",
    ];
    for path in paths {
        put(root.path(), path, "review-sensitive-fixture-marker\n");
    }
    put(root.path(), "app.py", "def ordinary():\n    pass\n");
    index::index(storage, root.path(), "review-credentials", None)
        .await
        .unwrap();
    let snapshot = storage.load().await.unwrap();
    for path in paths {
        let file = &snapshot.repositories["review-credentials"].files[path];
        assert!(
            file.text.is_none() && file.parsed.is_none() && file.skipped.is_some(),
            "{path}"
        );
    }
    let engine = Engine::new(storage.clone());
    assert_eq!(
        engine
            .call(
                "grep_code",
                json!({"pattern":"review-sensitive-fixture-marker"})
            )
            .await
            .unwrap()["matches"],
        json!([])
    );
    for path in paths {
        let qualified = format!("review-credentials/{path}");
        let review = engine.call("get_review_context", json!({"changed_files":[qualified], "sources":{qualified.clone():"caller override"}})).await.unwrap();
        assert_eq!(
            review["context"]["source_snippets"][&qualified],
            "(content withheld by file policy)"
        );
    }
    storage.remove("review-credentials").await.unwrap();
}

async fn identity_contract(storage: &Storage) {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    put(first.path(), "lib/shared.py", "def first():\n    pass\n");
    put(second.path(), "shared.py", "def second():\n    pass\n");
    index::index(storage, first.path(), "review-acme", None)
        .await
        .unwrap();
    let revision = storage.revision().await.unwrap();
    let error = index::index(storage, second.path(), "review-acme/lib", None)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("identity collision"));
    assert_eq!(storage.revision().await.unwrap(), revision);
    let engine = Engine::new(storage.clone());
    assert_eq!(engine.call("query_graph", json!({"pattern":"file_summary", "target":"review-acme/lib/shared.py", "repo":"review-acme"})).await.unwrap()["status"], "ok");
    std::fs::rename(
        second.path().join("shared.py"),
        second.path().join("different.py"),
    )
    .unwrap();
    index::index(storage, second.path(), "review-acme/lib", None)
        .await
        .unwrap();
    let revision = storage.revision().await.unwrap();
    put(
        first.path(),
        "lib/different.py",
        "def collision_on_update():\n    pass\n",
    );
    assert!(
        index::index(storage, first.path(), "review-acme", None)
            .await
            .is_err()
    );
    assert_eq!(storage.revision().await.unwrap(), revision);
    assert!(
        !storage.load().await.unwrap().repositories["review-acme"]
            .files
            .contains_key("lib/different.py")
    );
    put(second.path(), "reserved::name.txt", "ordinary fixture\n");
    assert!(
        index::index(storage, second.path(), "review-acme/lib", None)
            .await
            .is_err()
    );
    assert_eq!(storage.revision().await.unwrap(), revision);
    storage.remove("review-acme").await.unwrap();
    storage.remove("review-acme/lib").await.unwrap();
}

async fn state_contract(storage: &Storage) {
    let many = tempfile::tempdir().unwrap();
    let one = tempfile::tempdir().unwrap();
    let consumer = tempfile::tempdir().unwrap();
    let producer = "terraform {\n backend \"s3\" {\n  bucket = \"fixture\"\n  key = \"shared/state\"\n }\n}\noutput \"vpc_id\" { value = \"fixture\" }\n";
    put(many.path(), "first/main.tf", producer);
    put(many.path(), "second/main.tf", producer);
    put(one.path(), "main.tf", producer);
    put(
        consumer.path(),
        "main.tf",
        "data \"terraform_remote_state\" \"network\" {\n backend = \"s3\"\n config = {\n  bucket = \"fixture\"\n  key = \"shared/state\"\n }\n}\nresource \"aws_subnet\" \"consumer\" {\n vpc_id = data.terraform_remote_state.network.outputs.vpc_id\n}\n",
    );
    for (name, root) in [
        ("review-state-many", many.path()),
        ("review-state-one", one.path()),
        ("review-state-consumer", consumer.path()),
    ] {
        index::index(storage, root, name, None).await.unwrap();
    }
    let engine = Engine::new(storage.clone());
    let request =
        json!({"pattern":"references_to", "target":"review-state-one/main.tf::Output::vpc_id"});
    assert_eq!(
        engine.call("query_graph", request.clone()).await.unwrap()["results"],
        json!([])
    );
    storage.remove("review-state-many").await.unwrap();
    assert_eq!(
        engine.call("query_graph", request).await.unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    storage.remove("review-state-one").await.unwrap();
    storage.remove("review-state-consumer").await.unwrap();
}

#[tokio::test]
async fn sqlite_credential_paths_are_withheld() {
    let db = tempfile::tempdir().unwrap();
    credential_contract(&Storage::sqlite(&db.path().join("graph.db")).await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_identity_collisions_are_rejected_atomically() {
    let db = tempfile::tempdir().unwrap();
    identity_contract(&Storage::sqlite(&db.path().join("graph.db")).await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_ambiguous_state_candidates_are_not_discarded() {
    let db = tempfile::tempdir().unwrap();
    state_contract(&Storage::sqlite(&db.path().join("graph.db")).await.unwrap()).await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in ROSETTA_TEST_POSTGRES_URL"]
async fn postgres_review_regression_contracts() {
    let url =
        std::env::var("ROSETTA_TEST_POSTGRES_URL").expect("set an isolated test database URL");
    let storage = Storage::postgres(&url).await.unwrap();
    credential_contract(&storage).await;
    identity_contract(&storage).await;
    state_contract(&storage).await;
}

#[tokio::test]
async fn legacy_sensitive_payloads_are_withheld_before_tools_can_read_them() {
    let db = tempfile::tempdir().unwrap();
    let path = db.path().join("legacy.db");
    let root = tempfile::tempdir().unwrap();
    put(root.path(), ".kube/config", "initial fixture\n");
    let storage = Storage::sqlite(&path).await.unwrap();
    index::index(&storage, root.path(), "legacy", None)
        .await
        .unwrap();
    let connection = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    let unsafe_file = File {
        hash: "old".into(),
        text: Some("legacy-sensitive-fixture-marker".into()),
        parsed: None,
        skipped: None,
    };
    sqlx::query("UPDATE rosetta_files SET payload=$1 WHERE repo='legacy' AND path='.kube/config'")
        .bind(serde_json::to_string(&unsafe_file).unwrap())
        .execute(&connection)
        .await
        .unwrap();
    let file = &storage.load().await.unwrap().repositories["legacy"].files[".kube/config"];
    assert!(file.text.is_none() && file.skipped.is_some());
    assert_eq!(
        Engine::new(storage.clone())
            .call(
                "grep_code",
                json!({"pattern":"legacy-sensitive-fixture-marker"})
            )
            .await
            .unwrap()["matches"],
        json!([])
    );
    index::index(&storage, root.path(), "legacy", None)
        .await
        .unwrap();
    let payload: String = sqlx::query_scalar(
        "SELECT payload FROM rosetta_files WHERE repo='legacy' AND path='.kube/config'",
    )
    .fetch_one(&connection)
    .await
    .unwrap();
    assert!(!payload.contains("legacy-sensitive-fixture-marker"));
    connection.close().await;
}

#[tokio::test]
async fn legacy_colliding_indexes_fail_closed_and_can_be_recovered_by_removal() {
    let db = tempfile::tempdir().unwrap();
    let path = db.path().join("legacy.db");
    let root = tempfile::tempdir().unwrap();
    put(root.path(), "lib/shared.py", "def original():\n    pass\n");
    let storage = Storage::sqlite(&path).await.unwrap();
    index::index(&storage, root.path(), "legacy", None)
        .await
        .unwrap();
    let snapshot = storage.load().await.unwrap();
    let mut duplicate = snapshot.repositories["legacy"].clone();
    duplicate.name = "legacy/lib".into();
    let connection = sqlx::SqlitePool::connect(&format!("sqlite://{}", path.display()))
        .await
        .unwrap();
    sqlx::query("INSERT INTO rosetta_repositories (name,generation,payload) VALUES ($1,1,$2)")
        .bind(&duplicate.name)
        .bind(serde_json::to_string(&duplicate).unwrap())
        .execute(&connection)
        .await
        .unwrap();
    sqlx::query("INSERT INTO rosetta_files (repo,path,payload) VALUES ($1,'shared.py',$2)")
        .bind(&duplicate.name)
        .bind(
            serde_json::to_string(&snapshot.repositories["legacy"].files["lib/shared.py"]).unwrap(),
        )
        .execute(&connection)
        .await
        .unwrap();
    assert!(storage.load().await.is_err());
    assert!(
        Engine::new(storage.clone())
            .call("list_graph_stats", json!({}))
            .await
            .is_err()
    );
    storage.remove("legacy/lib").await.unwrap();
    assert_eq!(
        Engine::new(storage)
            .call(
                "query_graph",
                json!({"pattern":"file_summary","target":"legacy/lib/shared.py"})
            )
            .await
            .unwrap()["status"],
        "ok"
    );
    connection.close().await;
}
