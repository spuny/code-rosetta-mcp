mod common;

use common::{
    assert_edge, assert_well_formed, dump, fixture, has_node, names, node, parent, targets,
};
use rosetta_model::{EdgeKind, Language, NodeKind, ParsedFile};

fn parse(text: &str) -> ParsedFile {
    let p = common::parse(Language::Jinja, "tpl/page.j2", text);
    assert_well_formed(&p);
    p
}

fn variables(p: &ParsedFile) -> Vec<String> {
    names(p, NodeKind::Variable)
}

fn child() -> ParsedFile {
    parse(&fixture("jinja", "child.html.j2"))
}

#[test]
fn empty_input_yields_the_file_node() {
    let p = common::parse(Language::Jinja, "dir/empty.j2", "");
    assert_well_formed(&p);
    assert_eq!(p.nodes[0].name, "empty.j2");
    assert_eq!(p.nodes.len(), 1);
}

#[test]
fn template_block_and_macro_nodes() {
    let p = child();
    assert_eq!(p.parse_error, None, "{}", dump(&p));
    let t = node(&p, NodeKind::Template, "template");
    assert_eq!((t.line_start, t.parent), (1, Some(0)));
    assert_eq!(names(&p, NodeKind::Block), ["content"]);
    assert_eq!(names(&p, NodeKind::Macro), ["field", "wrapper"]);
    let field = node(&p, NodeKind::Macro, "field");
    assert_eq!(field.signature, "name, value, label");
    assert_eq!((field.line_start, field.line_end), (7, 10));
    let block = node(&p, NodeKind::Block, "content");
    assert_eq!((block.line_start, block.line_end), (15, 26));
    assert_eq!(parent(&p, NodeKind::Block, "content"), "Template::template");
    assert_eq!(parent(&p, NodeKind::Macro, "wrapper"), "Template::template");
}

#[test]
fn extends_includes_and_imports_are_template_refs() {
    let p = child();
    assert_edge(
        &p,
        EdgeKind::Extends,
        NodeKind::Template,
        "template",
        "template:layouts/base.html",
    );
    assert_eq!(
        targets(&p, EdgeKind::Includes, NodeKind::Template, "template"),
        [
            "template:a.j2",
            "template:b.j2",
            "template:partials/header.j2"
        ]
    );
    assert_eq!(
        targets(&p, EdgeKind::Imports, NodeKind::Template, "template"),
        ["template:macros/forms.j2", "template:macros/ui.j2"]
    );
}

#[test]
fn dynamic_includes_extends_and_imports_make_no_edge() {
    let p = parse(
        "{% extends layout %}{% include name %}{% include [a, 'x.j2'] %}{% import path as p %}{% from p import m %}",
    );
    assert!(targets(&p, EdgeKind::Extends, NodeKind::Template, "template").is_empty());
    assert_eq!(
        targets(&p, EdgeKind::Includes, NodeKind::Template, "template"),
        ["template:x.j2"]
    );
    assert!(targets(&p, EdgeKind::Imports, NodeKind::Template, "template").is_empty());
    let v = variables(&p);
    for name in ["layout", "name", "a", "path"] {
        assert!(
            v.iter().any(|n| n.ends_with(&format!(":{name}"))),
            "{name} in {v:?}"
        );
    }
}

#[test]
fn local_macro_calls_become_local_edges() {
    let p = child();
    assert_edge(
        &p,
        EdgeKind::CallsMacro,
        NodeKind::Macro,
        "wrapper",
        "Macro::field",
    );
    assert_edge(
        &p,
        EdgeKind::CallsMacro,
        NodeKind::Block,
        "content",
        "Macro::field",
    );
    assert_edge(
        &p,
        EdgeKind::CallsMacro,
        NodeKind::Block,
        "content",
        "Macro::wrapper",
    );
}

#[test]
fn imported_macro_calls_become_template_refs_with_the_macro_name() {
    let p = child();
    // `{% import ... as forms %}` + `forms.input(...)`
    assert_edge(
        &p,
        EdgeKind::CallsMacro,
        NodeKind::Macro,
        "field",
        "template:macros/forms.j2#input",
    );
    // `{% from ... import button, icon as ico %}`
    assert_edge(
        &p,
        EdgeKind::CallsMacro,
        NodeKind::Block,
        "content",
        "template:macros/ui.j2#button",
    );
    assert_edge(
        &p,
        EdgeKind::CallsMacro,
        NodeKind::Block,
        "content",
        "template:macros/ui.j2#icon",
    );
    let e = p
        .edges
        .iter()
        .find(|e| {
            e.kind == EdgeKind::CallsMacro
                && matches!(&e.target, rosetta_model::Target::Ref(r) if r.value.ends_with("#icon"))
        })
        .expect("icon call");
    assert_eq!(e.extra["macro"], "icon");
}

#[test]
fn free_variables_by_scope() {
    let p = child();
    let v = variables(&p);
    let expect = [
        "var:template:page",
        "var:template:site_name",
        "var:template:template_name",
        "var:template:user",
        "var:template:precision",
        "var:template:count",
        "var:macro:field:default_value",
        "var:block:content:items",
        "var:block:content:prefix",
        "var:block:content:empty_message",
        "var:block:content:outer_var",
        "var:block:content:c",
        "var:block:content:user",
        "var:block:content:thing",
        "var:block:content:btn_style",
    ];
    for name in expect {
        assert!(v.iter().any(|n| n == name), "{name} missing in {v:?}");
    }
    assert_eq!(v.len(), expect.len(), "{v:?}");
    assert_edge(
        &p,
        EdgeKind::UsesVariable,
        NodeKind::Block,
        "content",
        "Variable::var:block:content:items",
    );
    assert_edge(
        &p,
        EdgeKind::UsesVariable,
        NodeKind::Template,
        "template",
        "Variable::var:template:page",
    );
    assert_edge(
        &p,
        EdgeKind::UsesVariable,
        NodeKind::Macro,
        "field",
        "Variable::var:macro:field:default_value",
    );
    assert_eq!(
        parent(&p, NodeKind::Variable, "var:block:content:items"),
        "Block::content"
    );
}

#[test]
fn bound_and_builtin_names_are_not_free() {
    let p = child();
    let v = variables(&p).join(" ");
    for name in [
        "item",
        "loop",
        "range",
        "ns",
        "title",
        "forms",
        "ico",
        "button",
        "field",
        "wrapper",
        "caller",
        "not_a_var",
        "commented",
        "name",
        "label",
        "x",
        "a",
        "b",
        "value",
        "defined",
        "none",
    ] {
        assert!(
            !v.split(' ').any(|n| n.rsplit(':').next() == Some(name)),
            "{name} wrongly free in {v}"
        );
    }
}

#[test]
fn binding_rules() {
    // for targets, tuple unpacking and the loop name; the iterable is read outside the loop
    let p = parse(
        "{% for k, v in pairs %}{{ k }}{{ v }}{{ loop.first }}{{ other }}{% endfor %}{{ k }}",
    );
    assert_eq!(
        variables(&p),
        ["var:template:pairs", "var:template:other", "var:template:k"]
    );
    // set binds from that point on; reading before the set is free
    let p = parse("{{ early }}{% set late = 1 %}{{ late }}");
    assert_eq!(variables(&p), ["var:template:early"]);
    // block set
    let p = parse("{% set body %}{{ inside }}{% endset %}{{ body }}");
    assert_eq!(variables(&p), ["var:template:inside"]);
    // with arguments are read outside, bound inside
    let p = parse("{% with a=src %}{{ a }}{{ z }}{% endwith %}{{ a }}");
    assert_eq!(
        variables(&p),
        ["var:template:src", "var:template:z", "var:template:a"]
    );
    // macro arguments and defaults; the macro scope owns its free variables
    let p = parse("{% macro m(a, b=a, c=dflt) %}{{ a }}{{ b }}{{ free }}{% endmacro %}{{ free }}");
    assert_eq!(
        variables(&p),
        ["var:macro:m:dflt", "var:macro:m:free", "var:template:free"]
    );
    // call block arguments
    let p = parse("{% call(item) m() %}{{ item }}{{ other }}{% endcall %}");
    assert_eq!(variables(&p), ["var:template:other"]);
    // is tests, filters, keyword arguments, attributes, strings
    let p = parse(
        "{{ a is defined }}{{ b is not divisibleby(3) }}{{ c|join(sep) }}{{ f(key=val) }}{{ d.e.g }}{{ 'str' ~ \"x\" }}{{ h[i] }}",
    );
    assert_eq!(
        variables(&p),
        [
            "var:template:a",
            "var:template:b",
            "var:template:c",
            "var:template:sep",
            "var:template:val",
            "var:template:d",
            "var:template:h",
            "var:template:i"
        ]
    );
    // imports bind their names
    let p = parse("{% import 'x.j2' as x %}{% from 'y.j2' import z %}{{ x }}{{ z }}{{ w }}");
    assert_eq!(variables(&p), ["var:template:w"]);
    // the for-loop condition sees the loop variable
    let p = parse("{% for i in xs if i > limit %}{% endfor %}");
    assert_eq!(variables(&p), ["var:template:xs", "var:template:limit"]);
    // names inside tags that are not expressions are skipped
    let p = parse(
        "{% filter upper %}{{ s }}{% endfilter %}{% autoescape true %}{{ t }}{% endautoescape %}",
    );
    assert_eq!(variables(&p), ["var:template:s", "var:template:t"]);
}

#[test]
fn raw_blocks_and_comments_are_ignored() {
    let p = parse("{% raw %}{{ a }}{% if b %}{% endraw %}{# {{ c }} #}{{ d }}");
    assert_eq!(variables(&p), ["var:template:d"]);
}

#[test]
fn nested_blocks_and_scopes() {
    let p = parse(
        "{% block outer %}{% block inner %}{{ x }}{% endblock %}{{ y }}{% endblock %}{{ z }}",
    );
    assert_eq!(parent(&p, NodeKind::Block, "inner"), "Block::outer");
    assert_eq!(
        variables(&p),
        ["var:block:inner:x", "var:block:outer:y", "var:template:z"]
    );
    let outer = node(&p, NodeKind::Block, "outer");
    let inner = node(&p, NodeKind::Block, "inner");
    assert!(inner.line_start >= outer.line_start && inner.line_end <= outer.line_end);
}

#[test]
fn macro_called_before_its_definition_resolves() {
    let p = parse("{{ later(1) }}{% macro later(a) %}{% endmacro %}");
    assert_edge(
        &p,
        EdgeKind::CallsMacro,
        NodeKind::Template,
        "template",
        "Macro::later",
    );
    assert!(variables(&p).is_empty());
}

#[test]
fn duplicate_names_get_a_line_suffix() {
    let p = parse("{% block a %}{% endblock %}\n{% block a %}{% endblock %}");
    assert_eq!(names(&p, NodeKind::Block), ["a", "a@2"]);
}

#[test]
fn unterminated_actions_set_parse_error_and_keep_the_rest() {
    let p = parse("{% macro m() %}{% endmacro %}\n{{ broken\n{% block b %}");
    assert!(
        p.parse_error
            .as_deref()
            .is_some_and(|e| e.contains("line 2")),
        "{:?}",
        p.parse_error
    );
    assert!(has_node(&p, NodeKind::Macro, "m"));
    assert!(has_node(&p, NodeKind::Block, "b"));
    // the open block runs to the end of the file
    let b = node(&p, NodeKind::Block, "b");
    assert_eq!(b.line_end, 3);
}

#[test]
fn unbalanced_end_tags_and_odd_input_do_not_panic() {
    for text in [
        "{% endblock %}{% endmacro %}{% endfor %}{% endif %}",
        "{% macro %}{% block %}{% for %}{% set %}{% include %}{% import %}{% from %}",
        "{% for x in %}{% with %}{% call %}{% extends %}",
        "{{ ((( }}{{ ))) }}{{ [ }}{{ { }}",
        "{%\n%}{{}}{##}",
        "{% macro m( %}{% macro n(a,, b) %}",
        "é{{ é }}é{% é %}",
    ] {
        let p = parse(text);
        assert_eq!(p.nodes[0].kind, NodeKind::File, "{text:?}");
    }
}

#[test]
fn deep_nesting_and_large_input_stay_fast() {
    let started = std::time::Instant::now();
    let mut text = String::new();
    for _ in 0..5000 {
        text.push_str("{% if a %}{% for i in xs %}");
    }
    let p = parse(&text);
    assert!(p.nodes.len() >= 2);
    assert!(
        started.elapsed().as_secs() < 5,
        "nesting {:?}",
        started.elapsed()
    );
    let big = "{% macro m(a) %}{{ a }}{{ free }}{% endmacro %}\n{{ x.y(z) }}\n".repeat(15_000);
    assert!(big.len() > 900_000);
    let p = parse(&big);
    assert!(p.nodes.len() > 2);
    let hostile = "{{ '\n".repeat(150_000);
    parse(&hostile);
    assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
}
