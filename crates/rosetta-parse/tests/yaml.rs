mod common;

use common::{
    assert_edge, assert_well_formed, dump, fixture, has_node, names, node, parent, targets,
};
use rosetta_model::{EdgeKind, Language, NodeKind, ParsedFile, Target};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn parse_fixture(name: &str) -> ParsedFile {
    parse_as(name, &format!("deploy/{name}"))
}

fn parse_as(name: &str, path: &str) -> ParsedFile {
    let p = common::parse(Language::Yaml, path, &fixture("yaml", name));
    assert_well_formed(&p);
    p
}

fn parse_text(text: &str) -> ParsedFile {
    let p = common::parse(Language::Yaml, "dir/inline.yaml", text);
    assert_well_formed(&p);
    p
}

fn extra<'a>(p: &'a ParsedFile, kind: NodeKind, name: &str) -> &'a serde_json::Map<String, Value> {
    &node(p, kind, name).extra
}

fn refs(p: &ParsedFile, kind: NodeKind, name: &str) -> Vec<String> {
    targets(p, EdgeKind::References, kind, name)
}

#[test]
fn empty_input_yields_the_file_node() {
    let p = common::parse(Language::Yaml, "dir/empty.yaml", "");
    assert_well_formed(&p);
    assert_eq!(p.nodes[0].name, "empty.yaml");
    assert_eq!(p.nodes.len(), 1);
    assert!(p.parse_error.is_none());
}

#[test]
fn comments_and_null_documents_yield_only_the_file_node() {
    for text in ["# only a comment\n", "---\n", "---\n---\n~\n", "\n\n   \n"] {
        let p = parse_text(text);
        assert_eq!(p.nodes.len(), 1, "{text:?}\n{}", dump(&p));
        assert!(p.parse_error.is_none(), "{text:?}");
    }
}

#[test]
fn plain_config_makes_one_section_per_top_level_key() {
    let p = parse_fixture("plain.yaml");
    assert_eq!(
        names(&p, NodeKind::Section),
        [
            "name",
            "replicas",
            "debug",
            "ratio",
            "nothing",
            "database",
            "endpoints",
            "api_token",
            "dsn",
            "motd"
        ]
    );
    assert!(!has_node(&p, NodeKind::Document, "doc0"));
    assert_eq!(parent(&p, NodeKind::Section, "name"), "File::plain.yaml");

    let name = node(&p, NodeKind::Section, "name");
    assert_eq!(name.line_start, 2);
    assert_eq!(name.extra["key"], "name");
    assert_eq!(name.extra["value_type"], "string");
    assert_eq!(name.extra["value"], "demo-service");
    assert_eq!(
        extra(&p, NodeKind::Section, "replicas")["value_type"],
        "int"
    );
    assert_eq!(extra(&p, NodeKind::Section, "debug")["value_type"], "bool");
    assert_eq!(extra(&p, NodeKind::Section, "ratio")["value_type"], "float");
    assert_eq!(
        extra(&p, NodeKind::Section, "nothing")["value_type"],
        "null"
    );

    let database = node(&p, NodeKind::Section, "database");
    assert_eq!((database.line_start, database.line_end), (7, 10));
    assert_eq!(database.extra["value_type"], "mapping");
    assert_eq!(
        database.extra["child_keys"],
        json!(["host", "port", "password"])
    );
    let endpoints = extra(&p, NodeKind::Section, "endpoints");
    assert_eq!(endpoints["value_type"], "sequence");
    assert_eq!(endpoints["length"], 2);
    let motd = node(&p, NodeKind::Section, "motd");
    assert_eq!((motd.line_start, motd.line_end), (16, 18));
    assert_eq!(motd.extra["value"], "hello\nworld\n");
    assert!(p.edges.is_empty());
    assert!(!p.nodes[0].extra.contains_key("helm"));
}

#[test]
fn secret_looking_keys_and_credentials_never_store_values() {
    let p = parse_fixture("plain.yaml");
    assert!(!extra(&p, NodeKind::Section, "api_token").contains_key("value"));
    assert!(!extra(&p, NodeKind::Section, "dsn").contains_key("value"));
    // a nested secret is not exposed either: only key names are listed
    assert!(!dump(&p).contains("hunter2"));
    assert!(!dump(&p).contains("s3cret"));
    assert!(!dump(&p).contains("abc123"));

    let s = parse_fixture("secrets.yaml");
    let dump_text = dump(&s);
    for leaked in [
        "hunter2", "c2VjcmV0", "key-123", "shh", "BEGIN", "bob", "top",
    ] {
        assert!(
            !dump_text.contains(leaked),
            "{leaked} leaked in\n{dump_text}"
        );
    }
    let prefix = "Secret/web-secrets.";
    for key in [
        "password",
        "db_password",
        "apiKey",
        "private_key",
        "credentials",
        "client-secret",
    ] {
        let n = node(&s, NodeKind::Section, &format!("{prefix}{key}"));
        assert!(!n.extra.contains_key("value"), "{key}");
    }
    // PEM bodies are not stored even under an innocent key name
    assert!(!extra(&s, NodeKind::Section, "Secret/web-secrets.cert").contains_key("value"));
    // an innocent key still keeps its value
    assert_eq!(
        extra(&s, NodeKind::Section, "Secret/web-secrets.visible")["value"],
        "ok"
    );
    // child key NAMES of secret mappings are listed, their values are not
    assert_eq!(
        extra(&s, NodeKind::Section, "Secret/web-secrets.stringData")["child_keys"],
        json!(["password"])
    );
}

#[test]
fn long_scalars_are_not_stored_as_values() {
    let long = "x".repeat(300);
    let p = parse_text(&format!("short: ok\nlong: {long}\n"));
    assert_eq!(extra(&p, NodeKind::Section, "short")["value"], "ok");
    assert!(!extra(&p, NodeKind::Section, "long").contains_key("value"));
}

#[test]
fn multi_document_files_get_document_nodes() {
    let p = parse_fixture("multi.yaml");
    // the empty document between the separators is dropped, the list document is kept
    assert_eq!(names(&p, NodeKind::Document), ["doc0", "doc1", "doc2"]);
    assert_eq!(
        names(&p, NodeKind::Section),
        ["doc0.service", "doc0.port", "doc1.service", "doc1.port"]
    );
    assert_eq!(
        parent(&p, NodeKind::Section, "doc1.service"),
        "Document::doc1"
    );
    assert_eq!(parent(&p, NodeKind::Document, "doc2"), "File::multi.yaml");
    assert_eq!(node(&p, NodeKind::Document, "doc1").extra["doc_index"], 1);
    assert_eq!(
        extra(&p, NodeKind::Section, "doc1.service")["value"],
        "beta"
    );
    assert!(p.parse_error.is_none());
}

#[test]
fn duplicate_keys_and_resources_get_line_suffixes() {
    let p = parse_fixture("duplicates.yaml");
    assert!(has_node(&p, NodeKind::Section, "doc0.key"));
    assert!(has_node(&p, NodeKind::Section, "doc0.key@2"));
    assert!(has_node(&p, NodeKind::K8sResource, "ConfigMap/same"));
    assert!(has_node(&p, NodeKind::K8sResource, "ConfigMap/same@11"));
    // sections follow the stored (suffixed) name of their resource
    assert!(has_node(&p, NodeKind::Section, "ConfigMap/same@11.data"));
    assert_eq!(
        parent(&p, NodeKind::Section, "ConfigMap/same@11.data"),
        "K8sResource::ConfigMap/same@11"
    );
}

#[test]
fn anchors_and_aliases() {
    let p = parse_fixture("anchors.yaml");
    assert_eq!(
        names(&p, NodeKind::Anchor),
        ["&defaults", "&first", "&typed"]
    );
    assert_eq!(
        parent(&p, NodeKind::Anchor, "&defaults"),
        "Section::defaults"
    );
    assert_eq!(parent(&p, NodeKind::Anchor, "&first"), "Section::tags");
    let defaults = node(&p, NodeKind::Anchor, "&defaults");
    assert_eq!(defaults.extra["anchor_name"], "defaults");
    assert_eq!(defaults.extra["value_type"], "mapping");
    assert_eq!((defaults.line_start, defaults.line_end), (1, 3));
    assert_eq!(
        extra(&p, NodeKind::Anchor, "&first")["value_type"],
        "string"
    );
    // an anchor behind a tag is still named
    assert!(has_node(&p, NodeKind::Anchor, "&typed"));

    // the alias edge starts at the section that USES the alias, not at the document
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Section,
        "development",
        "Anchor::&defaults",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Section,
        "test",
        "Anchor::&defaults",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Section,
        "tags",
        "Anchor::&first",
    );
    assert_eq!(p.edges.len(), 3);
    let e = &p.edges[0];
    assert_eq!(e.line, 5);
    assert_eq!(e.extra["via"], "alias");
    assert_eq!(e.extra["anchor"], "defaults");
}

#[test]
fn anchor_names_are_per_document_and_survive_non_ascii_text() {
    let text = "---\nname: é\nbase: &x\n  k: ü\nuse: *x\n---\nbase: &x\n  k: 1\nuse: *x\n";
    let p = parse_text(text);
    assert_eq!(names(&p, NodeKind::Anchor), ["doc0.&x", "doc1.&x"]);
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Section,
        "doc0.use",
        "Anchor::doc0.&x",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Section,
        "doc1.use",
        "Anchor::doc1.&x",
    );
    assert_eq!(p.edges.len(), 2);
}

#[test]
fn an_alias_without_its_anchor_makes_no_edge() {
    let p = parse_text("a: &x 1\nb: *x\nc: [*x, *x]\n");
    assert_eq!(p.edges.len(), 2, "{}", dump(&p)); // b and c (two aliases, same line: one edge)
}

#[test]
fn kubernetes_deployment_references() {
    let p = parse_fixture("deployment.yaml");
    let resource = node(&p, NodeKind::K8sResource, "Deployment/web");
    assert_eq!(resource.parent, Some(0));
    assert_eq!(resource.extra["apiVersion"], "apps/v1");
    assert_eq!(resource.extra["k8s_kind"], "Deployment");
    assert_eq!(resource.extra["namespace"], "shop");
    assert_eq!(
        resource.extra["labels"],
        json!({"app": "web", "team": "storefront"})
    );
    assert_eq!(
        resource.extra["images"],
        json!([
            "registry.local/shop/web:2.0",
            "registry.local/shop/migrate:1.2"
        ])
    );
    let spec_keys = resource.extra["spec_keys"].as_array().unwrap();
    for wanted in [
        "replicas",
        "template.spec",
        "template.spec.containers.image",
        "template.spec.volumes.configMap.name",
    ] {
        assert!(spec_keys.iter().any(|k| k == wanted), "{wanted}");
    }

    // sections of a resource: apiVersion and kind are not repeated
    assert_eq!(
        names(&p, NodeKind::Section),
        ["Deployment/web.metadata", "Deployment/web.spec"]
    );
    let spec = extra(&p, NodeKind::Section, "Deployment/web.spec");
    assert_eq!(spec["k8s_section"], true);
    assert_eq!(spec["key"], "spec");
    assert_eq!(
        parent(&p, NodeKind::Section, "Deployment/web.spec"),
        "K8sResource::Deployment/web"
    );

    let mut found = refs(&p, NodeKind::K8sResource, "Deployment/web");
    found.sort();
    let mut expected = vec![
        "k8s:ConfigMap/web-files",
        "k8s:Secret/web-tls",
        "k8s:PersistentVolumeClaim/web-data",
        "k8s:ConfigMap/projected-cm",
        "k8s:Secret/projected-secret",
        "k8s:ConfigMap/web-config",
        "k8s:Secret/web-secrets",
        "k8s:Secret/db-credentials",
        "k8s:ConfigMap/web-mode",
        "k8s:ConfigMap/migrate-config",
        "k8s:ServiceAccount/web-sa",
    ];
    expected.sort();
    assert_eq!(found, expected, "{}", dump(&p));

    // every reference carries the referencing resource's namespace, the kind and the name
    for e in &p.edges {
        assert_eq!(e.kind, EdgeKind::References);
        assert_eq!(e.extra["namespace"], "shop");
        assert!(e.extra.contains_key("via") && e.extra.contains_key("ref_kind"));
        assert!(e.line > 0);
    }
    let sa = p
        .edges
        .iter()
        .find(|e| matches!(&e.target, Target::Ref(r) if r.value == "ServiceAccount/web-sa"))
        .unwrap();
    assert_eq!(
        (sa.line, sa.extra["ref_name"].clone()),
        (14, json!("web-sa"))
    );
    assert_eq!(sa.extra["ref_kind"], "ServiceAccount");
}

#[test]
fn kubernetes_resources_without_a_namespace_have_no_namespace_on_their_references() {
    let p = parse_fixture("statefulset.yaml");
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::K8sResource,
        "StatefulSet/db",
        "k8s:Service/db-headless",
    );
    assert!(p.edges.iter().all(|e| !e.extra.contains_key("namespace")));
    assert_eq!(
        extra(&p, NodeKind::K8sResource, "StatefulSet/db")["images"],
        json!(["postgres:17"])
    );
    assert!(!extra(&p, NodeKind::K8sResource, "StatefulSet/db").contains_key("namespace"));
}

#[test]
fn ingress_backends() {
    let p = parse_fixture("ingress.yaml");
    let mut found = refs(&p, NodeKind::K8sResource, "Ingress/web");
    found.sort();
    assert_eq!(
        found,
        [
            "k8s:Service/fallback",
            "k8s:Service/legacy",
            "k8s:Service/web"
        ],
        "{}",
        dump(&p)
    );
    assert!(p.edges.iter().all(|e| e.extra["namespace"] == "shop"));
}

#[test]
fn http_route_backends_default_to_service() {
    let p = parse_fixture("httproute.yaml");
    let mut found = refs(&p, NodeKind::K8sResource, "HTTPRoute/web");
    found.sort();
    assert_eq!(
        found,
        [
            "k8s:Backend/bucket",
            "k8s:Service/canary",
            "k8s:Service/web"
        ]
    );
}

#[test]
fn horizontal_pod_autoscaler_target() {
    let p = parse_fixture("hpa.yaml");
    assert_eq!(
        refs(&p, NodeKind::K8sResource, "HorizontalPodAutoscaler/web"),
        ["k8s:Deployment/web"]
    );
    assert_eq!(p.edges[0].extra["via"], "scaleTargetRef");
}

#[test]
fn pod_specs_inside_other_resources_are_found() {
    let text = "apiVersion: batch/v1\nkind: CronJob\nmetadata:\n  name: nightly\nspec:\n  jobTemplate:\n    spec:\n      template:\n        spec:\n          serviceAccountName: runner\n          containers:\n            - name: c\n              image: img:1\n              envFrom:\n                - secretRef:\n                    name: job-secret\n";
    let p = parse_text(text);
    let mut found = refs(&p, NodeKind::K8sResource, "CronJob/nightly");
    found.sort();
    assert_eq!(
        found,
        ["k8s:Secret/job-secret", "k8s:ServiceAccount/runner"]
    );
}

#[test]
fn generate_name_and_missing_names() {
    let p = parse_text(
        "apiVersion: batch/v1\nkind: Job\nmetadata:\n  generateName: run-\nspec: {}\n---\napiVersion: v1\nkind: List\nitems: []\n",
    );
    assert!(has_node(&p, NodeKind::K8sResource, "Job/run-"));
    assert!(has_node(&p, NodeKind::K8sResource, "List"));
}

#[test]
fn not_every_mapping_with_a_kind_is_a_resource() {
    let p = parse_text("kind: widget\nname: x\n");
    assert!(names(&p, NodeKind::K8sResource).is_empty());
    assert_eq!(names(&p, NodeKind::Section), ["kind", "name"]);
    let q = parse_text("apiVersion: v1\nkind: not a kind\n");
    assert!(names(&q, NodeKind::K8sResource).is_empty());
}

#[test]
fn kustomization_file_references() {
    let p = parse_fixture("kustomization.yaml");
    let section = |key: &str| format!("Kustomization.{key}");
    assert_eq!(
        refs(&p, NodeKind::Section, &section("resources")),
        ["file:../base", "file:deployment.yaml"]
    );
    assert_eq!(
        refs(&p, NodeKind::Section, &section("patches")),
        ["file:patch-replicas.yaml"]
    );
    assert_eq!(
        refs(&p, NodeKind::Section, &section("patchesStrategicMerge")),
        ["file:extra.yaml"]
    );
    assert_eq!(
        refs(&p, NodeKind::Section, &section("configMapGenerator")),
        ["file:app.properties", "file:config/app.cfg"]
    );
    assert_eq!(
        refs(&p, NodeKind::Section, &section("secretGenerator")),
        ["file:secrets.env"]
    );
    // remote bases, URLs and inline patches are not file references
    assert_eq!(p.edges.len(), 7, "{}", dump(&p));
    assert_eq!(p.edges[0].extra["via"], "resources");
    assert_eq!(p.edges[0].line, 4);
    assert!(has_node(&p, NodeKind::K8sResource, "Kustomization"));
}

#[test]
fn kustomization_without_kind_is_recognised_by_file_name() {
    let plain = parse_as("kustomization-bare.yaml", "base/other.yaml");
    assert!(plain.edges.is_empty());
    let named = parse_as("kustomization-bare.yaml", "overlay/kustomization.yaml");
    assert_eq!(
        refs(&named, NodeKind::Section, "resources"),
        ["file:a.yaml"]
    );
}

#[test]
fn helm_values_and_chart_files() {
    let v = parse_fixture("values.yaml");
    assert_eq!(v.nodes[0].extra["helm"], true);
    assert_eq!(extra(&v, NodeKind::Section, "image")["helm"], true);
    assert_eq!(extra(&v, NodeKind::Section, "replicaCount")["helm"], true);

    let c = parse_fixture("chart.yaml");
    assert_eq!(c.nodes[0].extra["helm"], true);

    // detection by content: three well-known top-level keys
    let guessed = common::parse(
        Language::Yaml,
        "deploy/custom.yaml",
        "replicaCount: 2\nimage: x\nresources: {}\nother: 1\n",
    );
    assert_eq!(guessed.nodes[0].extra["helm"], true);
    // Chart.yaml shape in an oddly named file
    let chart_shape = common::parse(
        Language::Yaml,
        "x/meta.yaml",
        &fixture("yaml", "chart.yaml"),
    );
    assert_eq!(chart_shape.nodes[0].extra["helm"], true);
    // not helm
    let plain = parse_fixture("plain.yaml");
    assert!(!plain.nodes[0].extra.contains_key("helm"));
    assert!(!extra(&plain, NodeKind::Section, "name").contains_key("helm"));
}

#[test]
fn malformed_later_document_keeps_earlier_documents() {
    let p = parse_fixture("malformed.yaml");
    assert!(p.parse_error.as_deref().is_some_and(|e| !e.is_empty()));
    assert!(
        has_node(&p, NodeKind::Section, "first") || has_node(&p, NodeKind::Section, "doc0.first")
    );
    assert!(dump(&p).contains("parse_error"));
}

#[test]
fn malformed_single_document_keeps_what_precedes_the_error() {
    let p = parse_fixture("malformed-single.yaml");
    assert!(p.parse_error.is_some());
    assert_eq!(extra(&p, NodeKind::Section, "good")["value"], "1");
    assert!(has_node(&p, NodeKind::Section, "bad"));
    assert!(!has_node(&p, NodeKind::Section, "more"));
}

#[test]
fn garbage_input_never_panics() {
    for text in [
        "\u{0}\u{1}\u{2}",
        "{{{{{{",
        "}}}}",
        "a: b: c: d",
        "- - - - -",
        "? ? ?",
        "&a &b &c",
        "*x",
        "key: |\n",
        "key: >-\n\n\n",
        "---\n---\n---\n...\n...",
        "a:\n\t- tab",
        "%YAML 1.2\n---\na: 1",
        "\"unterminated",
        "'unterminated",
        "a: !!tag\n",
        "[a, b",
    ] {
        let p = parse_text(text);
        assert_eq!(p.nodes[0].kind, NodeKind::File, "{text:?}");
    }
}

#[test]
fn deeply_nested_input_stays_bounded() {
    let started = Instant::now();
    let flow = format!("a: {}{}\nb: 1\n", "[".repeat(50_000), "]".repeat(50_000));
    let p = parse_text(&flow);
    // the event parser refuses absurd nesting: the error is reported, what precedes it is kept
    assert!(p.parse_error.is_some());
    assert!(has_node(&p, NodeKind::Section, "a"), "{}", dump(&p));

    let mut block = String::new();
    for depth in 0..3000 {
        block.push_str(&" ".repeat(depth));
        block.push_str("k:\n");
    }
    block.push_str(&" ".repeat(3000));
    block.push_str("leaf: 1\n");
    let q = parse_text(&block);
    assert!(has_node(&q, NodeKind::Section, "k"));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn deep_alias_chains_and_anchors_inside_skipped_depth_are_harmless() {
    let mut text = String::from("a: &top\n");
    let mut inner = String::new();
    for depth in 0..100 {
        inner.push_str(&"  ".repeat(depth + 1));
        inner.push_str(&format!("l{depth}: &d{depth}\n"));
    }
    text.push_str(&inner);
    text.push_str(&"  ".repeat(101));
    text.push_str("end: *d99\nb: *top\n");
    let p = parse_text(&text);
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Section,
        "b",
        "Anchor::&top",
    );
}

#[test]
fn a_twenty_thousand_line_file_parses_quickly() {
    let mut text = String::new();
    for i in 0..4000 {
        text.push_str(&format!(
            "svc{i}:\n  image: registry.local/app{i}:1\n  port: {i}\n  env:\n    - A\n"
        ));
    }
    assert_eq!(text.lines().count(), 20_000);
    let started = Instant::now();
    let p = parse_text(&text);
    let elapsed = started.elapsed();
    assert_eq!(p.nodes.len(), 4001);
    assert!(p.parse_error.is_none());
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    let last = node(&p, NodeKind::Section, "svc3999");
    assert_eq!((last.line_start, last.line_end), (19_996, 20_000));
}

#[test]
fn a_one_megabyte_file_is_cheap() {
    let mut text = String::new();
    let mut i = 0;
    while text.len() < 1_048_576 {
        text.push_str(&format!("key{i}: {}\n", "v".repeat(40)));
        i += 1;
    }
    let started = Instant::now();
    let p = parse_text(&text);
    assert!(p.nodes.len() > 10_000);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn long_keys_and_empty_keys_get_clean_names() {
    let long = "k".repeat(500);
    let p = parse_text(&format!("{long}: 1\n\"\": 2\n\"a\\nb\": 3\n"));
    let sections = names(&p, NodeKind::Section);
    assert_eq!(sections[0].chars().count(), 200);
    assert_eq!(sections[1], "(empty)");
    assert_eq!(sections[2], "a b");
}
