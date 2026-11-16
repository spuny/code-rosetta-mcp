use rosetta::{engine::Engine, index, storage::Storage};
use serde_json::{Value, json};
use std::path::Path;

fn fixture(root: &Path) {
    std::fs::write(
        root.join("core.py"),
        "class Base:\n    pass\n\ndef helper():\n    return 42\n",
    )
    .unwrap();
    std::fs::write(root.join("app.py"), "from core import helper, Base\n\ndef run():\n    return helper()\n\nclass Child(Base):\n    pass\n").unwrap();
    std::fs::write(
        root.join("settings.yaml"),
        "api:\n  endpoint: synthetic-marker\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.txt"), "synthetic-marker\nsecond line\n").unwrap();
    std::fs::write(root.join(".env"), "SECRET=never-publish-this-fixture\n").unwrap();
}

async fn contract(storage: Storage) {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let report = index::index(&storage, root.path(), "team/app", None)
        .await
        .unwrap();
    assert_eq!(report.files, 5);
    assert_eq!(report.skipped, 1);
    let engine = Engine::new(storage.clone());
    let call = |name: &'static str, args: Value| {
        let engine = engine.clone();
        async move { engine.call(name, args).await.unwrap() }
    };
    let search = call("search_nodes", json!({"query":"helper"})).await;
    assert!(!search["results"].as_array().unwrap().is_empty());
    let callers = call(
        "query_graph",
        json!({"pattern":"callers_of","target":"team/app/core.py::Function::helper"}),
    )
    .await;
    assert!(
        callers["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "run"),
        "{callers}"
    );
    let inherits = call(
        "query_graph",
        json!({"pattern":"inheritors_of","target":"team/app/core.py::Class::Base"}),
    )
    .await;
    assert!(
        inherits["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["name"] == "Child"),
        "{inherits}"
    );
    let grep = call(
        "grep_code",
        json!({"pattern":"synthetic-marker","path_glob":"team/app/*.txt"}),
    )
    .await;
    assert_eq!(grep["matches"].as_array().unwrap().len(), 1);
    assert_eq!(
        call("grep_code", json!({"pattern":"never-publish"})).await["matches"],
        json!([])
    );
    let impact = call(
        "get_impact_radius",
        json!({"changed_files":["team/app/core.py"]}),
    )
    .await;
    assert!(
        impact["impacted_files"]
            .as_array()
            .unwrap()
            .contains(&json!("team/app/app.py")),
        "{impact}"
    );
    let review = call(
        "get_review_context",
        json!({"changed_files":["team/app/.env"]}),
    )
    .await;
    assert_eq!(
        review["context"]["source_snippets"]["team/app/.env"],
        "(content withheld by file policy)"
    );
    assert!(
        engine
            .call("grep_code", json!({"pattern":"[","regex":true}))
            .await
            .is_err()
    );
    assert!(
        engine
            .call("search_nodes", json!({"query":""}))
            .await
            .is_err()
    );
    assert!(
        engine
            .call("query_graph", json!({"pattern":"made_up"}))
            .await
            .is_err()
    );
    assert!(
        engine
            .call("search_nodes", json!({"query":"x","limit":0}))
            .await
            .is_err()
    );
    assert!(
        engine
            .call("get_impact_radius", json!({"changed_files":["../outside"]}))
            .await
            .is_err()
    );
    assert_eq!(
        call("search_nodes", json!({"query":"helper","limit":999999})).await["limit_clamped"]["applied"],
        500
    );
    let before = storage.load().await.unwrap();
    let stale = before.repositories["team/app"].clone();
    let reused = index::index(&storage, root.path(), "team/app", None)
        .await
        .unwrap();
    assert!(reused.reused >= 4);
    assert!(storage.publish(stale, 0).await.is_err());
    assert_eq!(storage.load().await.unwrap().revision, before.revision + 1);
    std::fs::rename(root.path().join("core.py"), root.path().join("renamed.py")).unwrap();
    index::index(&storage, root.path(), "team/app", None)
        .await
        .unwrap();
    let after = call(
        "query_graph",
        json!({"pattern":"callers_of","target":"team/app/renamed.py::Function::helper"}),
    )
    .await;
    assert_eq!(
        after["results"],
        json!([]),
        "renamed module must not retain a stale import target"
    );
    assert!(
        !storage.load().await.unwrap().repositories["team/app"]
            .files
            .contains_key("core.py")
    );
    storage.remove("team/app").await.unwrap();
    assert_eq!(call("list_graph_stats", json!({})).await["repositories"], 0);
}

#[tokio::test]
async fn sqlite_storage_and_graph_contract() {
    let db = tempfile::tempdir().unwrap();
    contract(
        Storage::sqlite(&db.path().join("db with spaces.db"))
            .await
            .unwrap(),
    )
    .await;
}

#[tokio::test]
#[ignore = "requires an isolated PostgreSQL database in ROSETTA_TEST_POSTGRES_URL"]
async fn postgres_storage_and_graph_contract() {
    let url = std::env::var("ROSETTA_TEST_POSTGRES_URL")
        .expect("set ROSETTA_TEST_POSTGRES_URL to an isolated test database");
    contract(Storage::postgres(&url).await.unwrap()).await;
}

#[tokio::test]
async fn sqlite_refreshes_across_process_equivalent_connections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("shared.db");
    let reader = Engine::new(Storage::sqlite(&path).await.unwrap());
    assert_eq!(
        reader.call("list_graph_stats", json!({})).await.unwrap()["repositories"],
        0
    );
    let writer = Storage::sqlite(&path).await.unwrap();
    let source = tempfile::tempdir().unwrap();
    fixture(source.path());
    index::index(&writer, source.path(), "shared", None)
        .await
        .unwrap();
    assert_eq!(
        reader.call("list_graph_stats", json!({})).await.unwrap()["repositories"],
        1
    );
}

#[tokio::test]
async fn linker_does_not_guess_ambiguous_cross_repository_packages() {
    let db = tempfile::tempdir().unwrap();
    let storage = Storage::sqlite(&db.path().join("graph.db")).await.unwrap();
    let first = tempfile::tempdir().unwrap();
    std::fs::create_dir(first.path().join("pkg")).unwrap();
    std::fs::write(first.path().join("pkg/__init__.py"), "").unwrap();
    std::fs::write(
        first.path().join("pkg/api.py"),
        "def execute():\n    pass\n",
    )
    .unwrap();
    let second = tempfile::tempdir().unwrap();
    std::fs::write(
        second.path().join("client.py"),
        "from pkg.api import execute\ndef call():\n    execute()\n",
    )
    .unwrap();
    index::index(&storage, first.path(), "library", None)
        .await
        .unwrap();
    index::index(&storage, second.path(), "client", None)
        .await
        .unwrap();
    let engine = Engine::new(storage.clone());
    let args = json!({"pattern":"callers_of","target":"library/pkg/api.py::Function::execute"});
    assert_eq!(
        engine.call("query_graph", args.clone()).await.unwrap()["results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    index::index(&storage, first.path(), "duplicate-library", None)
        .await
        .unwrap();
    assert_eq!(
        engine.call("query_graph", args).await.unwrap()["results"],
        json!([])
    );
}

#[tokio::test]
async fn symlink_content_is_not_indexed() {
    #[cfg(unix)]
    {
        let db = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), "outside-private-marker").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("linked.py")).unwrap();
        let storage = Storage::sqlite(&db.path().join("graph.db")).await.unwrap();
        index::index(&storage, root.path(), "safe", None)
            .await
            .unwrap();
        assert_eq!(
            Engine::new(storage)
                .call("grep_code", json!({"pattern":"outside-private-marker"}))
                .await
                .unwrap()["matches"],
            json!([])
        );
    }
}
