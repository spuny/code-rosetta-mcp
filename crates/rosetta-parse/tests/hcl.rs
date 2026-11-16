mod common;

use common::{assert_edge, assert_well_formed, dump, fixture, has_node, names, node, targets};
use rosetta_model::{EdgeKind, Language, NodeKind, ParsedFile};

fn parse(path: &str, text: &str) -> ParsedFile {
    let parsed = common::parse(Language::Hcl, path, text);
    assert_well_formed(&parsed);
    parsed
}

fn main_tf() -> ParsedFile {
    parse("stack/main.tf", &fixture("hcl", "main.tf"))
}

/// Runs `f` on a thread with the 2 MiB stack of a rayon or tokio worker.
fn on_small_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .stack_size(2 << 20)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("parser must not panic or overflow")
}

#[test]
fn empty_input_yields_the_file_node() {
    let p = parse("dir/empty.tf", "");
    assert_eq!(p.nodes.len(), 1);
    assert_eq!(p.nodes[0].name, "empty.tf");
    assert!(p.parse_error.is_none());
}

#[test]
fn comments_only_yield_the_file_node() {
    let p = parse("c.tf", "# nothing\n// here\n/* at all */\n");
    assert_eq!(p.nodes.len(), 1);
    assert!(p.edges.is_empty());
}

#[test]
fn every_node_kind_is_emitted() {
    let p = main_tf();
    assert_eq!(
        names(&p, NodeKind::Resource),
        ["aws_instance.web", "aws_security_group.web"]
    );
    assert_eq!(names(&p, NodeKind::DataSource), ["aws_ami.base"]);
    assert_eq!(names(&p, NodeKind::Module), ["vpc", "registry"]);
    assert_eq!(
        names(&p, NodeKind::Variable),
        ["name", "db_password", "disks"]
    );
    assert_eq!(names(&p, NodeKind::Output), ["instance_id", "vpc"]);
    assert_eq!(names(&p, NodeKind::Provider), ["aws", "aws.west"]);
    assert_eq!(
        names(&p, NodeKind::Local),
        ["prefix", "tags", "config", "ca", "pairs", "api_token"]
    );
    assert_eq!(names(&p, NodeKind::TerraformConfig), ["terraform"]);
    assert_eq!(p.nodes[0].name, "main.tf");
    assert!(p.parse_error.is_none());
}

#[test]
fn nodes_carry_real_line_numbers() {
    let text = fixture("hcl", "main.tf");
    let p = parse("stack/main.tf", &text);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.starts_with("resource \"aws_instance\" \"web\""))
        .expect("fixture") as u32
        + 1;
    let end = lines
        .iter()
        .enumerate()
        .skip(start as usize)
        .find(|(_, l)| **l == "}")
        .map(|(i, _)| i as u32 + 1)
        .expect("fixture");
    let n = node(&p, NodeKind::Resource, "aws_instance.web");
    assert_eq!((n.line_start, n.line_end), (start, end));
    let local = node(&p, NodeKind::Local, "ca");
    let ca_line = lines
        .iter()
        .position(|l| l.trim_start().starts_with("ca "))
        .expect("fixture") as u32
        + 1;
    assert_eq!((local.line_start, local.line_end), (ca_line, ca_line));
    assert!(
        p.nodes
            .iter()
            .skip(1)
            .all(|n| n.line_start > 0 && n.line_end >= n.line_start)
    );
}

#[test]
fn resource_and_data_extras() {
    let p = main_tf();
    let r = node(&p, NodeKind::Resource, "aws_instance.web");
    assert_eq!(r.extra["provider"], "aws");
    assert_eq!(r.extra["resource_type"], "aws_instance");
    let d = node(&p, NodeKind::DataSource, "aws_ami.base");
    assert_eq!(d.extra["provider"], "aws");
    assert_eq!(d.extra["data_type"], "aws_ami");
}

#[test]
fn module_variable_output_provider_extras() {
    let p = main_tf();
    let m = node(&p, NodeKind::Module, "registry");
    assert_eq!(m.extra["source"], "terraform-aws-modules/s3-bucket/aws");
    assert_eq!(m.extra["version"], "4.0.0");
    let v = node(&p, NodeKind::Variable, "name");
    assert_eq!(v.extra["type"], "string");
    assert_eq!(v.extra["default"], "demo");
    assert_eq!(v.extra["description"], "Name prefix");
    let disks = node(&p, NodeKind::Variable, "disks");
    assert!(
        disks.extra["type"]
            .as_str()
            .is_some_and(|t| t.starts_with("list(object("))
    );
    assert_eq!(disks.extra["default"], "[]");
    let o = node(&p, NodeKind::Output, "instance_id");
    assert_eq!(o.extra["description"], "Web instance");
    let west = node(&p, NodeKind::Provider, "aws.west");
    assert_eq!(west.extra["alias"], "west");
    assert_eq!(west.extra["region"], "us-west-2");
    let local = node(&p, NodeKind::Local, "prefix");
    assert_eq!(local.extra["value"], "\"${var.name}-app\"");
}

#[test]
fn secret_looking_values_are_not_stored() {
    let p = main_tf();
    assert!(
        !node(&p, NodeKind::Variable, "db_password")
            .extra
            .contains_key("default")
    );
    assert!(
        !node(&p, NodeKind::Local, "api_token")
            .extra
            .contains_key("value")
    );
    assert!(!dump(&p).contains("hunter2"));
    let s = parse(
        "s.tf",
        "variable \"x\" {\n  default = \"topsecretvalue\"\n  sensitive = true\n}\n",
    );
    assert!(
        !node(&s, NodeKind::Variable, "x")
            .extra
            .contains_key("default")
    );
}

#[test]
fn terraform_block_records_backend_and_providers() {
    let p = main_tf();
    let t = node(&p, NodeKind::TerraformConfig, "terraform");
    assert_eq!(t.extra["backend"], "s3");
    assert_eq!(t.extra["backend_key"], "network/terraform.tfstate");
    assert_eq!(t.extra["required_providers"]["aws"], "hashicorp/aws");
}

#[test]
fn references_resolve_to_local_nodes_or_tf_refs() {
    let p = main_tf();
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Local,
        "prefix",
        "Variable::name",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Local,
        "tags",
        "Local::prefix",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Local,
        "pairs",
        "Variable::disks",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Output,
        "instance_id",
        "Resource::aws_instance.web",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Output,
        "vpc",
        "Module::vpc",
    );
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Resource,
        "aws_security_group.web",
        "Local::prefix",
    );
    let web = targets(
        &p,
        EdgeKind::References,
        NodeKind::Resource,
        "aws_instance.web",
    );
    for expected in [
        "DataSource::aws_ami.base",
        "Local::tags",
        "Variable::name",
        "Variable::disks",
        "Resource::aws_security_group.web",
        "tf:var.instance_type",
        "tf:aws_ebs_volume.shared",
    ] {
        assert!(
            web.iter().any(|t| t == expected),
            "missing {expected} in {web:?}"
        );
    }
}

#[test]
fn trailing_attributes_and_indexes_are_stripped() {
    let p = parse(
        "t.tf",
        "resource \"aws_a\" \"x\" {\n  a = aws_b.y[0].id\n  b = module.m.out.deep\n  c = data.aws_c.z.attr.more\n  d = var.v[\"k\"].x\n  e = local.l.m\n}\n",
    );
    let t = targets(&p, EdgeKind::References, NodeKind::Resource, "aws_a.x");
    assert_eq!(
        t,
        [
            "tf:aws_b.y",
            "tf:data.aws_c.z",
            "tf:local.l",
            "tf:module.m",
            "tf:var.v"
        ]
    );
}

#[test]
fn loop_variables_and_dynamic_iterators_are_not_references() {
    let p = main_tf();
    let web = targets(
        &p,
        EdgeKind::References,
        NodeKind::Resource,
        "aws_instance.web",
    );
    assert!(
        web.iter().all(|t| !t.contains("ebs_block_device")),
        "dynamic iterator leaked: {web:?}"
    );
    let p = parse(
        "t.tf",
        "locals {\n  names = [for my_item in var.items : my_item.name]\n  other = { for my_key, my_val in var.items : my_key => my_val.id }\n}\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::References, NodeKind::Local, "names"),
        ["tf:var.items"]
    );
    assert_eq!(
        targets(&p, EdgeKind::References, NodeKind::Local, "other"),
        ["tf:var.items"]
    );
    let p = parse(
        "t.tf",
        "resource \"aws_a\" \"x\" {\n  dynamic \"rule\" {\n    for_each = var.rules\n    iterator = my_rule\n    content {\n      name = my_rule.value.name\n      id   = rule.value.id\n    }\n  }\n}\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::References, NodeKind::Resource, "aws_a.x"),
        ["tf:var.rules"]
    );
}

#[test]
fn reserved_roots_are_not_references() {
    let p = parse(
        "t.tf",
        "resource \"aws_a\" \"x\" {\n  a = path.module\n  b = terraform.workspace\n  c = each.value\n  d = count.index\n  e = self.id\n}\n",
    );
    assert!(p.edges.is_empty(), "{}", dump(&p));
}

#[test]
fn references_inside_interpolations_and_heredocs() {
    let p = parse("p/heredoc.tf", &fixture("hcl", "heredoc.tf"));
    let t = targets(
        &p,
        EdgeKind::References,
        NodeKind::Resource,
        "aws_iam_policy.p",
    );
    assert_eq!(t, ["Resource::aws_iam_role.r", "Variable::name"]);
}

#[test]
fn modules_use_modules_and_pass_variables() {
    let p = main_tf();
    assert_eq!(
        targets(&p, EdgeKind::UsesModule, NodeKind::Module, "vpc"),
        ["tfmodule:./modules/vpc"]
    );
    assert_eq!(
        targets(&p, EdgeKind::PassesVar, NodeKind::Module, "vpc"),
        ["tfvar:./modules/vpc|cidr"]
    );
    assert!(targets(&p, EdgeKind::UsesModule, NodeKind::Module, "registry").is_empty());
    assert!(targets(&p, EdgeKind::PassesVar, NodeKind::Module, "registry").is_empty());
    assert_edge(
        &p,
        EdgeKind::References,
        NodeKind::Module,
        "vpc",
        "Variable::name",
    );
    let e = p
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::PassesVar)
        .expect("edge");
    assert!(e.line > 0);
}

#[test]
fn file_functions_read_config_and_render() {
    let p = main_tf();
    assert_eq!(
        targets(&p, EdgeKind::ReadsConfig, NodeKind::Local, "config"),
        ["file:config/app.yaml"]
    );
    assert_eq!(
        targets(&p, EdgeKind::ReadsConfig, NodeKind::Local, "ca"),
        ["file:../shared/ca.pem"]
    );
    assert_eq!(
        targets(
            &p,
            EdgeKind::Renders,
            NodeKind::Resource,
            "aws_instance.web"
        ),
        ["file:files/boot.sh.tpl"]
    );
    let p = parse(
        "t.tf",
        "locals {\n  a = file(\"x.j2\")\n  b = templatefile(\"y.yaml\", {})\n  c = file(var.path)\n  d = file(\"${var.dir}/z.txt\")\n  e = filebase64(\"${path.root}/img.png\")\n  f = templatefile(\"${path.module}/t.tftpl\", {})\n}\n",
    );
    assert_eq!(
        targets(&p, EdgeKind::Renders, NodeKind::Local, "a"),
        ["file:x.j2"]
    );
    assert_eq!(
        targets(&p, EdgeKind::ReadsConfig, NodeKind::Local, "b"),
        ["file:y.yaml"]
    );
    assert!(targets(&p, EdgeKind::ReadsConfig, NodeKind::Local, "c").is_empty());
    assert!(targets(&p, EdgeKind::ReadsConfig, NodeKind::Local, "d").is_empty());
    assert_eq!(
        targets(&p, EdgeKind::ReadsConfig, NodeKind::Local, "e"),
        ["file:img.png"]
    );
    assert_eq!(
        targets(&p, EdgeKind::Renders, NodeKind::Local, "f"),
        ["file:t.tftpl"]
    );
}

#[test]
fn remote_state_needs_a_literal_backend_key() {
    let p = parse("net/remote.tf", &fixture("hcl", "remote_state.tf"));
    let network = node(&p, NodeKind::DataSource, "terraform_remote_state.network");
    assert_eq!(network.extra["backend"], "s3");
    assert_eq!(network.extra["backend_key"], "network/terraform.tfstate");
    let dynamic = node(&p, NodeKind::DataSource, "terraform_remote_state.dynamic");
    assert_eq!(dynamic.extra["backend"], "s3");
    assert!(!dynamic.extra.contains_key("backend_key"));
    assert_eq!(
        targets(
            &p,
            EdgeKind::RemoteState,
            NodeKind::Resource,
            "aws_subnet.a"
        ),
        [
            "tfstate:network/terraform.tfstate|cidr",
            "tfstate:network/terraform.tfstate|vpc_id"
        ]
    );
    let refs = targets(&p, EdgeKind::References, NodeKind::Resource, "aws_subnet.a");
    assert!(refs.contains(&"DataSource::terraform_remote_state.network".to_string()));
    assert!(refs.contains(&"DataSource::terraform_remote_state.dynamic".to_string()));
    assert!(refs.contains(&"tf:data.terraform_remote_state.missing".to_string()));
}

#[test]
fn remote_state_keys_for_other_backends() {
    let p = parse(
        "t.tf",
        "terraform {\n  backend \"gcs\" {\n    prefix = \"envs/prod\"\n  }\n}\ndata \"terraform_remote_state\" \"x\" {\n  backend = \"local\"\n  config = {\n    path = \"../x/terraform.tfstate\"\n  }\n}\n",
    );
    assert_eq!(
        node(&p, NodeKind::TerraformConfig, "terraform").extra["backend_key"],
        "envs/prod"
    );
    assert_eq!(
        node(&p, NodeKind::DataSource, "terraform_remote_state.x").extra["backend_key"],
        "../x/terraform.tfstate"
    );
}

#[test]
fn tfvars_yield_assigned_variables_only() {
    let p = parse("env/prod.tfvars", &fixture("hcl", "prod.tfvars"));
    assert_eq!(
        names(&p, NodeKind::Variable),
        ["name", "db_password", "disks"]
    );
    assert_eq!(p.nodes.len(), 4);
    assert!(p.edges.is_empty());
    let v = node(&p, NodeKind::Variable, "disks");
    assert_eq!(v.extra["assigned"], true);
    assert_eq!((v.line_start, v.line_end), (3, 5));
    assert!(!dump(&p).contains("s3cret"));
}

#[test]
fn duplicate_names_are_suffixed_not_lost() {
    let p = parse(
        "t.tf",
        "variable \"a\" {}\nvariable \"a\" {}\nlocals {\n  x = 1\n}\nlocals {\n  x = 2\n}\n",
    );
    assert_eq!(names(&p, NodeKind::Variable), ["a", "a@2"]);
    assert_eq!(names(&p, NodeKind::Local), ["x", "x@7"]);
}

#[test]
fn unknown_blocks_are_ignored_but_the_file_still_parses() {
    let p = parse(
        "t.tf",
        "moved {\n  from = aws_a.x\n  to   = aws_a.y\n}\ncheck \"h\" {\n  assert {\n    condition = true\n  }\n}\n",
    );
    assert_eq!(p.nodes.len(), 1);
    assert!(p.parse_error.is_none());
}

#[test]
fn malformed_input_yields_the_file_node_and_an_error() {
    let p = parse("bad/malformed.tf", &fixture("hcl", "malformed.tf"));
    assert_eq!(p.nodes.len(), 1);
    assert!(p.edges.is_empty());
    let error = p.parse_error.as_deref().unwrap_or("");
    assert!(error.contains("line"), "{error}");
}

#[test]
fn binary_and_odd_input_never_panics() {
    for text in [
        "\u{0}\u{1}\u{2}",
        "resource",
        "resource \"a\"",
        "resource \"a\" \"b\" {",
        "}}}}",
        "a = \"unterminated",
        "x = <<EOT\nnever closed",
        "locals { a = file(",
        "variable \"é\" { default = \"ü\" }",
    ] {
        let p = parse("odd.tf", text);
        assert_eq!(p.nodes[0].name, "odd.tf");
    }
}

#[test]
fn pathological_nesting_is_rejected_without_overflow() {
    let p = on_small_stack(|| {
        let nested = format!("a = {}{}\n", "[".repeat(5000), "]".repeat(5000));
        common::parse(Language::Hcl, "deep.tf", &nested)
    });
    assert_eq!(p.nodes.len(), 1);
    assert_eq!(p.parse_error.as_deref(), Some("nesting too deep"));
}

#[test]
fn a_hundred_thousand_operators_on_one_line_do_not_overflow() {
    let p = on_small_stack(|| {
        let chained = format!("locals {{\n  a = {}1\n}}\n", "1 + ".repeat(100_000));
        common::parse(Language::Hcl, "chain.tf", &chained)
    });
    assert_eq!(p.nodes[0].name, "chain.tf");
}

#[test]
fn long_operator_chains_across_lines_do_not_overflow() {
    let p = on_small_stack(|| {
        let mut text = String::from("locals {\n  a = (1\n");
        for _ in 0..60_000 {
            text.push_str("    + 1\n");
        }
        text.push_str("  )\n}\n");
        common::parse(Language::Hcl, "long.tf", &text)
    });
    assert_eq!(p.nodes[0].name, "long.tf");
}

#[test]
fn a_one_megabyte_file_parses_quickly() {
    let mut text = String::new();
    let mut i = 0;
    while text.len() < 1_000_000 {
        text.push_str(&format!(
            "resource \"aws_instance\" \"r{i}\" {{\n  ami = var.ami\n  tags = local.tags\n  subnet = aws_subnet.s{}.id\n}}\n",
            i / 2
        ));
        i += 1;
    }
    let started = std::time::Instant::now();
    let p = on_small_stack(move || common::parse(Language::Hcl, "big.tf", &text));
    // Catches quadratic work (minutes on this input), not a benchmark: debug build, shared runner.
    assert!(started.elapsed().as_secs() < 15, "{:?}", started.elapsed());
    assert_eq!(names(&p, NodeKind::Resource).len(), i);
    assert!(has_node(&p, NodeKind::Resource, "aws_instance.r0"));
}

#[test]
fn multi_line_nested_conditionals_and_unary_chains_do_not_overflow() {
    let (cond, unary, calls) = on_small_stack(|| {
        let mut cond = String::from("locals {\n  a = (true\n");
        for _ in 0..40_000 {
            cond.push_str("   ? 1 :\n   true\n");
        }
        cond.push_str("  ? 1 : 2)\n}\n");
        let mut unary = String::from("locals {\n  a = (\n");
        for _ in 0..80_000 {
            unary.push_str("  !\n");
        }
        unary.push_str("  true)\n}\n");
        let mut calls = String::from("locals {\n  a = ");
        for _ in 0..2_000 {
            calls.push_str("f(\n");
        }
        calls.push('1');
        for _ in 0..2_000 {
            calls.push_str(")\n");
        }
        calls.push_str("}\n");
        (
            common::parse(Language::Hcl, "cond.tf", &cond),
            common::parse(Language::Hcl, "unary.tf", &unary),
            common::parse(Language::Hcl, "calls.tf", &calls),
        )
    });
    for p in [cond, unary, calls] {
        assert_eq!(p.nodes[0].kind, NodeKind::File);
    }
}
