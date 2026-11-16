use rosetta::{engine::Engine, index, storage::Storage};
use serde_json::json;

#[tokio::test]
async fn macro_callers_and_nested_import_scopes_match_native_query_rules() {
    let db = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("macros.j2"),
        "{% macro greet(name) %}Hello {{ name }}{% endmacro %}\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("page.j2"),
        "{% from 'macros.j2' import greet %}{{ greet('fixture') }}\n",
    )
    .unwrap();
    std::fs::write(root.path().join("util.py"), "def utility():\n    pass\n").unwrap();
    std::fs::write(
        root.path().join("app.py"),
        "class Handler:\n    def run(self):\n        import util\n        util.utility()\n",
    )
    .unwrap();
    let storage = Storage::sqlite(&db.path().join("graph.db")).await.unwrap();
    index::index(&storage, root.path(), "repo", None)
        .await
        .unwrap();
    let graph = Engine::new(storage).graph().await.unwrap();
    let target = graph
        .nodes
        .iter()
        .find(|n| n.kind == "Macro" && n.path == "macros.j2")
        .unwrap();
    let callers = graph
        .call(
            "query_graph",
            json!({"pattern":"callers_of","target":target.qualified_name}),
        )
        .unwrap();
    assert!(
        callers["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["path"] == "page.j2"),
        "{callers}"
    );
    let imports = graph
        .call(
            "query_graph",
            json!({"pattern":"imports_of","target":"repo/app.py::Class::Handler"}),
        )
        .unwrap();
    assert!(
        !imports["edges"].as_array().unwrap().is_empty(),
        "{imports}"
    );
    let importers = graph
        .call(
            "query_graph",
            json!({"pattern":"importers_of","target":"repo/util.py::Function::utility"}),
        )
        .unwrap();
    assert!(
        importers["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["path"] == "app.py"),
        "{importers}"
    );
}
