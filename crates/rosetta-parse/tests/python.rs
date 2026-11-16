mod common;

use common::{assert_edge, assert_well_formed, dump, has_node, names, node, parent, targets};
use rosetta_model::{EdgeKind, Language, NodeKind, ParsedFile};
use std::time::{Duration, Instant};

fn parse(path: &str, text: &str) -> ParsedFile {
    let p = common::parse(Language::Python, path, text);
    assert_well_formed(&p);
    p
}

fn app() -> ParsedFile {
    parse("src/pkg/app.py", &common::fixture("python", "app.py"))
}

fn edges(p: &ParsedFile, kind: EdgeKind) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in p.edges.iter().filter(|e| e.kind == kind) {
        let source = &p.nodes[e.source as usize];
        let target = match &e.target {
            rosetta_model::Target::Local(i) => {
                let n = &p.nodes[*i as usize];
                format!("{}::{}", n.kind, n.name)
            }
            rosetta_model::Target::Ref(r) => r.to_string(),
        };
        out.push(format!("{}::{} -> {target}", source.kind, source.name));
    }
    out.sort();
    out
}

#[test]
fn empty_input_yields_the_file_node() {
    let p = common::parse(Language::Python, "dir/empty.py", "");
    assert_well_formed(&p);
    assert_eq!(p.nodes[0].name, "empty.py");
    assert!(p.parse_error.is_none());
    assert_eq!(p.nodes.len(), 1);
    assert!(p.edges.is_empty());
}

#[test]
fn comments_and_blank_lines_only() {
    let p = parse("x.py", "# nothing here\n\n   \n# more\n");
    assert_eq!(p.nodes.len(), 1);
    assert!(p.edges.is_empty());
    assert!(p.parse_error.is_none());
}

#[test]
fn nodes_use_the_naming_rules() {
    let p = app();
    assert_eq!(
        names(&p, NodeKind::Function),
        [
            "top",
            "later",
            "helper",
            "helper.inner",
            "fetch",
            "controlled",
            "test_something",
            "conditional",
            "conditional@93"
        ]
    );
    assert_eq!(names(&p, NodeKind::Class), ["Base", "Child", "Child.Inner"]);
    assert_eq!(
        names(&p, NodeKind::Method),
        [
            "Base.ping",
            "Child.__init__",
            "Child.helper_method",
            "Child.build",
            "Child.Inner.deep"
        ]
    );
    assert_eq!(
        parent(&p, NodeKind::Function, "helper.inner"),
        "Function::helper"
    );
    assert_eq!(parent(&p, NodeKind::Method, "Child.build"), "Class::Child");
    assert_eq!(parent(&p, NodeKind::Class, "Child.Inner"), "Class::Child");
    assert_eq!(parent(&p, NodeKind::Function, "top"), "File::app.py");
}

#[test]
fn nodes_carry_lines_signature_and_extra() {
    let p = app();
    let top = node(&p, NodeKind::Function, "top");
    assert_eq!((top.line_start, top.line_end), (14, 17));
    assert_eq!(top.signature, "a, b: int = 3, *args, **kw");
    assert_eq!(top.extra["return_type"], "int");
    assert!(!top.extra.contains_key("async"));

    let fetch = node(&p, NodeKind::Function, "fetch");
    assert_eq!(fetch.extra["async"], true);
    assert_eq!(
        fetch.extra["decorators"],
        serde_json::json!(["decorator", "other.deco"])
    );
    // The span of a decorated definition starts at its first decorator.
    assert_eq!((fetch.line_start, fetch.line_end), (31, 35));

    let child = node(&p, NodeKind::Class, "Child");
    assert_eq!(child.signature, "Base, Generic[T], metaclass=Meta");
    assert_eq!((child.line_start, child.line_end), (43, 61));

    let build = node(&p, NodeKind::Method, "Child.build");
    assert_eq!(
        build.extra["decorators"],
        serde_json::json!(["staticmethod"])
    );
    assert_eq!(build.signature, "");
}

#[test]
fn test_files_and_test_functions_are_marked() {
    let p = app();
    assert!(node(&p, NodeKind::Function, "test_something").is_test);
    assert!(!node(&p, NodeKind::Function, "top").is_test);
    assert!(!p.nodes[0].is_test);

    for path in ["tests/test_app.py", "pkg/app_test.py"] {
        let p = parse(
            path,
            "class A:\n    def m(self):\n        pass\n\ndef f():\n    pass\n",
        );
        assert!(p.nodes.iter().all(|n| n.is_test), "{path}\n{}", dump(&p));
    }
    let p = parse(
        "pkg/app.py",
        "class TestA:\n    def test_m(self):\n        pass\n    def helper(self):\n        pass\n",
    );
    assert!(node(&p, NodeKind::Method, "TestA.test_m").is_test);
    assert!(!node(&p, NodeKind::Method, "TestA.helper").is_test);
}

#[test]
fn async_methods_and_nested_scopes() {
    let p = parse(
        "x.py",
        "class C:\n    async def run(self):\n        def step():\n            pass\n        class Local:\n            pass\n",
    );
    assert!(node(&p, NodeKind::Method, "C.run").extra["async"] == true);
    assert!(has_node(&p, NodeKind::Function, "C.run.step"));
    assert!(has_node(&p, NodeKind::Class, "C.run.Local"));
    assert_eq!(
        parent(&p, NodeKind::Function, "C.run.step"),
        "Method::C.run"
    );
}

#[test]
fn conditional_definitions_are_found_and_stay_unique() {
    let p = app();
    assert_eq!(
        parent(&p, NodeKind::Function, "conditional"),
        "File::app.py"
    );
    assert!(has_node(&p, NodeKind::Function, "conditional@93"));
    let p = parse(
        "x.py",
        "try:\n    import fast\n    def impl():\n        pass\nexcept ImportError:\n    def impl():\n        pass\n",
    );
    assert_eq!(names(&p, NodeKind::Function), ["impl", "impl@6"]);
}

#[test]
fn imports_become_py_references() {
    let p = app();
    let imports = edges(&p, EdgeKind::Imports);
    for expected in [
        "File::app.py -> py:os",
        "File::app.py -> py:os.path",
        "File::app.py -> py:json",
        "File::app.py -> py:sys",
        "File::app.py -> py:collections.OrderedDict",
        "File::app.py -> py:collections.defaultdict",
        "File::app.py -> py:typing.Generic",
        "Function::controlled -> py:yaml",
    ] {
        assert!(
            imports.iter().any(|i| i == expected),
            "{expected}\n{}",
            dump(&p)
        );
    }
    // `from __future__` is not an import of anything.
    assert!(!imports.iter().any(|i| i.contains("__future__")));
    let e = p
        .edges
        .iter()
        .find(|e| matches!(&e.target, rosetta_model::Target::Ref(r) if r.value == "collections.OrderedDict"))
        .unwrap();
    assert_eq!(e.extra["import_type"], "from");
    assert_eq!(e.extra["module"], "collections");
    let e = p
        .edges
        .iter()
        .find(|e| matches!(&e.target, rosetta_model::Target::Ref(r) if r.value == "os.path"))
        .unwrap();
    assert_eq!(e.extra["import_type"], "module");
    assert_eq!(e.line, 2);
}

#[test]
fn relative_imports_keep_their_dots() {
    let p = app();
    let imports = edges(&p, EdgeKind::Imports);
    for expected in [
        "File::app.py -> py:.sibling",
        "File::app.py -> py:.pkg.mod.Helper",
        "File::app.py -> py:.pkg.mod.other",
        "File::app.py -> py:..up",
    ] {
        assert!(
            imports.iter().any(|i| i == expected),
            "{expected}\n{}",
            dump(&p)
        );
    }
    let p = parse(
        "x.py",
        "from ... import a\nfrom .. import b, c\nfrom . import (d,\n    e as f)\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::Imports, NodeKind::File, "x.py"),
        ["py:...a", "py:..b", "py:..c", "py:.d", "py:.e"]
    );
}

#[test]
fn imports_inside_functions_classes_and_guards() {
    let p = parse(
        "x.py",
        "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    import heavy\nclass C:\n    import inside_class\n    def m(self):\n        from lib import tool\n        tool.go()\n",
    );
    assert_edge(&p, EdgeKind::Imports, NodeKind::File, "x.py", "py:heavy");
    assert_edge(
        &p,
        EdgeKind::Imports,
        NodeKind::Class,
        "C",
        "py:inside_class",
    );
    assert_edge(
        &p,
        EdgeKind::Imports,
        NodeKind::Method,
        "C.m",
        "py:lib.tool",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "C.m",
        "py:lib.tool.go",
    );
}

#[test]
fn inheritance_is_local_or_a_reference() {
    let p = app();
    let inherits = edges(&p, EdgeKind::Inherits);
    assert_eq!(
        inherits,
        [
            "Class::Child -> Class::Base",
            "Class::Child -> py:typing.Generic"
        ]
    );
    let p = parse(
        "x.py",
        "import models\nfrom base import Mixin\n\nclass A(models.Model, Mixin, object, Exception):\n    pass\n\nclass B(A, metaclass=M):\n    pass\n\nclass C(list[int], Outer.Nested, make()):\n    pass\n",
    );
    assert_eq!(
        edges(&p, EdgeKind::Inherits),
        [
            "Class::A -> py:base.Mixin",
            "Class::A -> py:models.Model",
            "Class::B -> Class::A",
            "Class::C -> py:Outer.Nested",
        ]
    );
}

#[test]
fn calls_resolve_to_definitions_regardless_of_order() {
    let p = app();
    // `later` is defined below `top`.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "top",
        "Function::later",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "top",
        "Function::helper",
    );
    // Nested function: visible in its parent, and the parent's own name from inside.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "helper",
        "Function::helper.inner",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "helper.inner",
        "Function::helper",
    );
    // Constructor call resolves to the class.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "Child.build",
        "Class::Child",
    );
    // Class attribute access through the class name.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "Child.Inner.deep",
        "Method::Child.build",
    );
}

#[test]
fn self_and_cls_calls_use_the_enclosing_class_and_local_bases() {
    let p = app();
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "Child.__init__",
        "Method::Child.helper_method",
    );
    // `ping` is defined on the local base class.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "Child.__init__",
        "Method::Base.ping",
    );
    // Not defined anywhere in the file: raw text.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "Child.__init__",
        "py:self.missing",
    );
    let p = parse(
        "x.py",
        "class K:\n    @classmethod\n    def make(cls):\n        return cls.helper()\n    @classmethod\n    def helper(cls):\n        def nested():\n            return self_like()\n        return cls.make()\n",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "K.make",
        "Method::K.helper",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Method,
        "K.helper",
        "Method::K.make",
    );
    // The class body is not visible from a nested function: this stays a raw reference.
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "K.helper.nested",
        "py:self_like",
    );
}

#[test]
fn calls_through_import_aliases_and_unknown_names() {
    let p = app();
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "later",
        "py:os.path.join",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "top",
        "py:os.getcwd",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "app.py",
        "py:.sibling.run",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "app.py",
        "py:collections.defaultdict",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "app.py",
        "py:typing.TypeVar",
    );
    assert_edge(&p, EdgeKind::Calls, NodeKind::File, "app.py", "py:setup");
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "fetch",
        "py:client.get",
    );
    let p = parse(
        "x.py",
        "from pkg import mod as m, Thing\ndef f():\n    m.sub.run()\n    Thing.create()\n    Thing()\n    mystery.call()\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::Calls, NodeKind::Function, "f"),
        [
            "py:mystery.call",
            "py:pkg.Thing",
            "py:pkg.Thing.create",
            "py:pkg.mod.sub.run"
        ]
    );
}

#[test]
fn builtins_make_no_edges_unless_redefined() {
    let p = app();
    assert!(
        !edges(&p, EdgeKind::Calls)
            .iter()
            .any(|e| e.ends_with("py:print"))
    );
    assert!(
        !edges(&p, EdgeKind::Calls)
            .iter()
            .any(|e| e.ends_with("py:range"))
    );
    let p = parse(
        "x.py",
        "def print(x):\n    pass\ndef f():\n    print(1)\n    len([])\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::Calls, NodeKind::Function, "f"),
        ["Function::print"]
    );
    let p = parse("x.py", "from mine import open\ndef f():\n    open('x')\n");
    assert_edge(&p, EdgeKind::Calls, NodeKind::Function, "f", "py:mine.open");
}

#[test]
fn call_sources_follow_the_scope_rules() {
    let p = app();
    // Module level, a decorator and a class attribute all belong to the File node.
    assert_edge(&p, EdgeKind::Calls, NodeKind::File, "app.py", "py:setup");
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "app.py",
        "py:decorator",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "app.py",
        "py:make_default",
    );
    // Control flow, comprehension, lambda, with, for, while: all in the function.
    let controlled = targets(&p, EdgeKind::Calls, NodeKind::Function, "controlled");
    for expected in [
        "py:first",
        "py:other",
        "py:second",
        "py:third",
        "py:yaml.safe_load",
        "py:g",
        "py:z",
        "py:h",
        "py:cond",
        "py:lam",
    ] {
        assert!(
            controlled.iter().any(|t| t == expected),
            "{expected} in {controlled:?}"
        );
    }
    // A call inside a nested function is the nested function's.
    let p = parse(
        "x.py",
        "def outer():\n    inner_a()\n    def inner():\n        deep()\n    return inner\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::Calls, NodeKind::Function, "outer"),
        ["py:inner_a"]
    );
    assert_eq!(
        targets(&p, EdgeKind::Calls, NodeKind::Function, "outer.inner"),
        ["py:deep"]
    );
}

#[test]
fn defaults_annotations_and_arguments_are_evaluated_outside_the_function() {
    let p = parse(
        "x.py",
        "def f(a=compute(), b: make_type() = None) -> build_type():\n    inside()\n\nresult = wrap(inner_call(1), key=other_call())\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::Calls, NodeKind::File, "x.py"),
        [
            "py:build_type",
            "py:compute",
            "py:inner_call",
            "py:make_type",
            "py:other_call",
            "py:wrap"
        ]
    );
    assert_eq!(
        targets(&p, EdgeKind::Calls, NodeKind::Function, "f"),
        ["py:inside"]
    );
}

#[test]
fn calls_in_every_statement_form_are_found() {
    let p = parse(
        "x.py",
        "async def f(items):\n    async with lock() as l:\n        await send(l)\n    async for x in stream():\n        yield transform(x)\n    match command():\n        case Point(x=0):\n            handle()\n    assert check(), msg()\n    del registry.pop(1)\n    return f'{fmt(1)} {1 + other()}'\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::Calls, NodeKind::Function, "f"),
        [
            "py:check",
            "py:command",
            "py:fmt",
            "py:handle",
            "py:lock",
            "py:msg",
            "py:other",
            "py:registry.pop",
            "py:send",
            "py:stream",
            "py:transform",
        ]
    );
}

#[test]
fn method_chains_call_each_link_once_on_its_own_line() {
    let p = parse(
        "x.py",
        "def f(q):\n    return (\n        q.select()\n        .where()\n        .order()\n    )\n",
    );
    let lines: Vec<(String, u32)> = p
        .edges
        .iter()
        .filter(|e| e.kind == EdgeKind::Calls)
        .map(|e| {
            let rosetta_model::Target::Ref(r) = &e.target else {
                panic!()
            };
            (r.value.clone(), e.line)
        })
        .collect();
    assert!(lines.contains(&("q.select".to_owned(), 3)), "{lines:?}");
    // Calls whose callee is not a plain dotted name (the result of a call) are not edges.
    assert_eq!(lines.len(), 1, "{lines:?}");
}

#[test]
fn the_same_call_twice_on_one_line_is_one_edge() {
    let p = parse("x.py", "def f():\n    g(); g(); g()\n    g()\n");
    assert_eq!(p.edges.len(), 2);
    let p = parse("x.py", "def f():\n    a = g(g(1))\n");
    assert_eq!(p.edges.len(), 1);
}

#[test]
fn a_syntax_error_keeps_what_was_recovered() {
    let p = parse(
        "x.py",
        "import good\n\ndef fine():\n    helper()\n\ndef broken(:\n    pass\n\ndef after():\n    other()\n",
    );
    assert!(
        p.parse_error.as_deref().is_some_and(|e| e.contains("line")),
        "{}",
        dump(&p)
    );
    assert!(has_node(&p, NodeKind::Function, "fine"));
    assert_edge(&p, EdgeKind::Imports, NodeKind::File, "x.py", "py:good");
    assert_edge(&p, EdgeKind::Calls, NodeKind::Function, "fine", "py:helper");
    let p = parse("x.py", "class A(:\n    def m(self\n");
    assert!(p.parse_error.is_some());
}

#[test]
fn unicode_and_odd_text_do_not_matter() {
    let p = parse(
        "x.py",
        "def zpracuj(údaj: str) -> \"Výsledek\":\n    \"\"\"Dokumentace s češtinou a 😀.\"\"\"\n    return převeď(údaj)\n\nclass Přehled:\n    pass\n",
    );
    assert!(has_node(&p, NodeKind::Function, "zpracuj"));
    assert!(has_node(&p, NodeKind::Class, "Přehled"));
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Function,
        "zpracuj",
        "py:převeď",
    );
    assert_eq!(
        parse("x.py", "\u{feff}x = 1\r\ndef f():\r\n    g()\r\n")
            .nodes
            .len(),
        2
    );
    assert_eq!(parse("x.py", "\0\u{1}\u{2}").nodes[0].name, "x.py");
}

#[test]
fn hostile_nesting_neither_panics_nor_overflows() {
    let started = Instant::now();
    let parens = format!("x = {}1{}\n", "(".repeat(20_000), ")".repeat(20_000));
    parse("deep.py", &parens);
    let calls = format!("x = {}\n", "f(".repeat(5_000) + &")".repeat(5_000));
    let p = parse("calls.py", &calls);
    assert!(p.edges.len() <= 1);
    let attrs = format!("x = {}()\n", vec!["a"; 20_000].join("."));
    parse("attrs.py", &attrs);

    let mut ifs = String::new();
    for i in 0..1_500 {
        ifs.push_str(&"    ".repeat(i.min(90)));
        ifs.push_str("if x:\n");
    }
    ifs.push_str(&"    ".repeat(90));
    ifs.push_str("    f()\n");
    let p = parse("ifs.py", &ifs);
    let _ = dump(&p);

    let mut defs = String::new();
    for i in 0..600 {
        defs.push_str(&" ".repeat(i));
        defs.push_str("def f():\n");
    }
    defs.push_str(&" ".repeat(600));
    defs.push_str("pass\n");
    let p = parse("defs.py", &defs);
    assert!(p.nodes.len() > 100, "{}", p.nodes.len());
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn large_files_parse_quickly() {
    let mut text = String::new();
    for i in 0..4_000 {
        text.push_str(&format!(
            "def func_{i}(a, b):\n    value = helper_{}(a) + other.method_{i}(b)\n    return value\n\n",
            i % 50
        ));
    }
    assert!(text.len() > 200_000);
    let started = Instant::now();
    let p = parse("big.py", &text);
    assert_eq!(names(&p, NodeKind::Function).len(), 4_000);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

/// `py` references derived from an import carry `extra.via = "import"`; raw dotted names on
/// local variables do not (the linker only resolves the former).
#[test]
fn import_derived_references_are_marked() {
    let p = parse(
        "svc/main.py",
        "import os.path\nimport mrobot.review\nfrom pkg import mod as m\n\n\
         def run(app):\n    app.add_middleware(1)\n    m.go()\n    mrobot.review.review_mr()\n    \
         os.path.join('a')\n    undefined_name()\n",
    );
    let via = |value: &str| -> Option<String> {
        p.edges
            .iter()
            .find(|e| matches!(&e.target, rosetta_model::Target::Ref(r) if r.value == value))
            .map(|e| {
                e.extra
                    .get("via")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned()
            })
    };
    assert_eq!(via("app.add_middleware").as_deref(), Some(""));
    assert_eq!(via("undefined_name").as_deref(), Some(""));
    assert_eq!(via("pkg.mod.go").as_deref(), Some("import"));
    assert_eq!(via("mrobot.review.review_mr").as_deref(), Some("import"));
    assert_eq!(via("os.path.join").as_deref(), Some("import"));
    assert!(
        p.edges
            .iter()
            .filter(|e| e.kind == EdgeKind::Imports)
            .all(|e| e.extra.get("via").and_then(|v| v.as_str()) == Some("import"))
    );
}
