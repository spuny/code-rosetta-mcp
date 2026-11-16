use rosetta::{engine::Engine, index, storage::Storage};
use serde_json::json;

#[tokio::test]
async fn helm_values_and_templates_link_across_languages() {
    let db = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("templates")).unwrap();
    std::fs::write(
        root.path().join("Chart.yaml"),
        "name: fixture\nversion: 1.0.0\n",
    )
    .unwrap();
    std::fs::write(root.path().join("values.yaml"), "image:\n  tag: latest\n").unwrap();
    std::fs::write(
        root.path().join("templates/_helpers.tpl"),
        "{{ define \"fixture.name\" }}fixture{{ end }}\n",
    )
    .unwrap();
    std::fs::write(root.path().join("templates/deployment.yaml"),"apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {{ include \"fixture.name\" . }}\nspec:\n  image: {{ .Values.image.tag }}\n").unwrap();
    let storage = Storage::sqlite(&db.path().join("graph.db")).await.unwrap();
    index::index(&storage, root.path(), "chart", None)
        .await
        .unwrap();
    let graph = Engine::new(storage).graph().await.unwrap();
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.target_ref.as_deref() == Some("values:image") && e.target.is_some()),
        "{:?}",
        graph.edges
    );
    assert!(
        graph
            .edges
            .iter()
            .any(|e| e.target_ref.as_deref() == Some("helm:fixture.name") && e.target.is_some())
    );
    let result = graph
        .call("query_graph", json!({"pattern":"cross_language"}))
        .unwrap();
    assert!(result["total_edges"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn terraform_reads_yaml_without_structural_impact_fanout() {
    let db = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("main.tf"),
        "locals { config = yamldecode(file(\"config.yaml\")) }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("config.yaml"),
        "service:\n  enabled: true\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("unrelated.py"),
        "def unrelated():\n    return 0\n",
    )
    .unwrap();
    let storage = Storage::sqlite(&db.path().join("graph.db")).await.unwrap();
    index::index(&storage, root.path(), "infra", None)
        .await
        .unwrap();
    let graph = Engine::new(storage).graph().await.unwrap();
    let result = graph
        .call("query_graph", json!({"pattern":"cross_language"}))
        .unwrap();
    assert!(result["total_edges"].as_u64().unwrap() > 0, "{result}");
    let impact = graph
        .call(
            "get_impact_radius",
            json!({"changed_files":["infra/config.yaml"]}),
        )
        .unwrap();
    assert!(
        !impact["impacted_files"]
            .as_array()
            .unwrap()
            .contains(&json!("infra/unrelated.py"))
    );
}

#[tokio::test]
async fn git_index_excludes_untracked_files_and_clears_refs_for_dirty_files() {
    let db = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let result = std::process::Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init"]);
    std::fs::write(root.path().join("app.py"), "def original():\n    pass\n").unwrap();
    git(&["add", "app.py"]);
    git(&["commit", "-m", "synthetic fixture"]);
    std::fs::write(
        root.path().join("untracked.txt"),
        "untracked-private-marker\n",
    )
    .unwrap();
    let storage = Storage::sqlite(&db.path().join("graph.db")).await.unwrap();
    index::index(&storage, root.path(), "repo", None)
        .await
        .unwrap();
    let first = storage.load().await.unwrap();
    assert!(first.repositories["repo"].git_ref.is_some());
    assert!(
        !first.repositories["repo"]
            .files
            .contains_key("untracked.txt")
    );
    std::fs::write(root.path().join("app.py"), "def changed():\n    pass\n").unwrap();
    index::index(&storage, root.path(), "repo", None)
        .await
        .unwrap();
    assert!(
        storage.load().await.unwrap().repositories["repo"]
            .git_ref
            .is_none()
    );
}

#[tokio::test]
async fn ambiguous_bare_files_are_not_guessed_in_review_or_impact() {
    let db = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("app.py"), "def x():\n    pass\n").unwrap();
    let storage = Storage::sqlite(&db.path().join("graph.db")).await.unwrap();
    index::index(&storage, root.path(), "first", None)
        .await
        .unwrap();
    index::index(&storage, root.path(), "second", None)
        .await
        .unwrap();
    let engine = Engine::new(storage);
    assert!(
        engine
            .call("get_impact_radius", json!({"changed_files":["app.py"]}))
            .await
            .is_err()
    );
    assert!(
        engine
            .call("get_review_context", json!({"changed_files":["app.py"]}))
            .await
            .is_err()
    );
    assert_eq!(
        engine
            .call(
                "query_graph",
                json!({"pattern":"file_summary","target":"app.py"})
            )
            .await
            .unwrap()["status"],
        "ambiguous"
    );
}
