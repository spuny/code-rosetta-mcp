mod common;

use common::{assert_edge, assert_well_formed, fixture, has_node, names, node, parent, targets};
use rosetta_model::{EdgeKind, Language, NodeKind};

fn parse(text: &str) -> rosetta_model::ParsedFile {
    let p = common::parse(Language::Bash, "scripts/run.sh", text);
    assert_well_formed(&p);
    p
}

fn exported(p: &rosetta_model::ParsedFile, name: &str) -> bool {
    node(p, NodeKind::Variable, name).extra["exported"] == true
}

#[test]
fn empty_input_yields_the_file_node() {
    let p = common::parse(Language::Bash, "dir/empty.sh", "");
    common::assert_well_formed(&p);
    assert_eq!(p.nodes[0].name, "empty.sh");
    assert_eq!(p.nodes.len(), 1);
    assert!(p.edges.is_empty());
}

#[test]
fn functions_in_both_syntaxes_with_lines() {
    let p = parse(&fixture("bash", "deploy.sh"));
    let functions = names(&p, NodeKind::Function);
    for expected in [
        "log",
        "build",
        "make_it",
        "deploy",
        "push",
        "summarize",
        "cleanup",
        "main",
        "setup",
    ] {
        assert!(
            functions.iter().any(|f| f == expected),
            "{expected} in {functions:?}"
        );
    }
    let log = node(&p, NodeKind::Function, "log");
    assert_eq!((log.line_start, log.line_end), (17, 19));
    assert_eq!(parent(&p, NodeKind::Function, "log"), "File::run.sh");
}

#[test]
fn variables_and_the_exported_flag() {
    let p = parse(&fixture("bash", "deploy.sh"));
    assert!(exported(&p, "APP_NAME"));
    assert!(exported(&p, "REGION"), "export without a value");
    assert!(exported(&p, "STAGE"), "declare -x");
    assert!(!exported(&p, "LOCAL_ONLY"), "plain declare");
    assert!(!exported(&p, "VERSION"), "readonly");
    assert!(!exported(&p, "COUNT"), "plain assignment");
    // Assignments inside functions are not top-level variables.
    assert!(!has_node(&p, NodeKind::Variable, "result"));
    assert!(!has_node(&p, NodeKind::Variable, "host"));
}

#[test]
fn a_later_export_marks_an_earlier_assignment_exported() {
    let p = parse("X=1\nexport X\n");
    assert_eq!(names(&p, NodeKind::Variable), ["X"]);
    assert!(exported(&p, "X"));
}

#[test]
fn source_with_literal_paths_only() {
    let p = parse(&fixture("bash", "deploy.sh"));
    let imports = targets(&p, EdgeKind::Imports, NodeKind::File, "run.sh");
    assert_eq!(imports, ["file:./lib/common.sh", "file:config/env.sh"]);
    let dot = p
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::Imports && e.extra.get("via").is_some_and(|v| v == "."))
        .expect("dot form");
    assert!(dot.line > 0);
}

#[test]
fn source_inside_a_function_comes_from_that_function() {
    let p = parse("load() {\n  source lib.sh\n}\n");
    assert_edge(
        &p,
        EdgeKind::Imports,
        NodeKind::Function,
        "load",
        "file:lib.sh",
    );
}

#[test]
fn calls_from_every_command_position() {
    let p = parse(&fixture("bash", "deploy.sh"));
    // inside if/pipeline
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "build",
        "Function::log",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "build",
        "Function::make_it",
    );
    // && / || lists in a subshell body, for, while, command substitution, case
    let deploy = targets(&p, EdgeKind::Calls, NodeKind::Function, "deploy");
    for expected in [
        "Function::build",
        "Function::log",
        "Function::push",
        "Function::summarize",
        "Function::cleanup",
    ] {
        assert!(
            deploy.iter().any(|t| t == expected),
            "{expected} in {deploy:?}"
        );
    }
    // chained: cleanup -> cleanup_later
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "cleanup",
        "Function::cleanup_later",
    );
}

#[test]
fn functions_defined_later_still_resolve() {
    let p = parse(&fixture("bash", "deploy.sh"));
    // main calls setup, which is defined after main.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "main",
        "Function::setup",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "main",
        "Function::deploy",
    );
}

#[test]
fn top_level_calls_come_from_the_file_node() {
    let p = parse(&fixture("bash", "deploy.sh"));
    let from_file = targets(&p, EdgeKind::Calls, NodeKind::File, "run.sh");
    assert_eq!(from_file, ["Function::deploy", "Function::main"]);
}

#[test]
fn calls_to_unknown_commands_make_no_edge() {
    let p = parse("f() { ls -l; grep x y; }\n");
    assert!(p.edges.is_empty(), "{}", common::dump(&p));
}

#[test]
fn nested_function_definitions_get_the_outer_function_as_parent() {
    let p = parse("outer() {\n  inner() { echo hi; }\n  inner\n}\n");
    assert_eq!(parent(&p, NodeKind::Function, "inner"), "Function::outer");
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "outer",
        "Function::inner",
    );
}

#[test]
fn recursion_is_a_self_edge() {
    let p = parse("fact() { fact; }\n");
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "fact",
        "Function::fact",
    );
}

#[test]
fn duplicate_function_names_stay_unique() {
    let p = parse("a() { :; }\na() { :; }\n");
    let functions = names(&p, NodeKind::Function);
    assert_eq!(functions.len(), 2, "{functions:?}");
    assert_eq!(functions[0], "a");
    assert_ne!(functions[1], "a");
}

#[test]
fn syntax_errors_keep_what_was_recovered() {
    let p = parse("ok() { echo fine; }\nif then fi ((( \nexport KEPT=1\n");
    assert!(p.parse_error.is_some());
    assert!(has_node(&p, NodeKind::Function, "ok"));
}

#[test]
fn hostile_input_is_total() {
    let deep = format!("{}echo x{}", "$(".repeat(5000), ")".repeat(5000));
    let p = common::parse(Language::Bash, "a.sh", &deep);
    assert_eq!(p.nodes[0].kind, NodeKind::File);
    let binary = String::from_utf8_lossy(&[0u8, 1, 2, 255, 254, 10, 123, 125]).into_owned();
    let p = common::parse(Language::Bash, "b.sh", &binary);
    assert_eq!(p.nodes[0].kind, NodeKind::File);
}

#[test]
fn a_large_file_parses_quickly() {
    let mut text = String::new();
    for i in 0..20_000 {
        text.push_str(&format!("f{i}() {{ echo {i}; f{}; }}\n", i / 2));
    }
    let start = std::time::Instant::now();
    let p = common::parse(Language::Bash, "big.sh", &text);
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
    assert_eq!(names(&p, NodeKind::Function).len(), 20_000);
}
