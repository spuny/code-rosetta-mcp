use rosetta_model::Language::{self, *};
use rosetta_parse::detect;

fn d(path: &str) -> Option<Language> {
    detect(path, b"")
}

#[test]
fn file_names_and_extensions() {
    assert_eq!(d("Dockerfile"), Some(Dockerfile));
    assert_eq!(d("build/Dockerfile.prod"), Some(Dockerfile));
    assert_eq!(d("x/app.dockerfile"), Some(Dockerfile));
    assert_eq!(d("Containerfile"), Some(Dockerfile));
    assert_eq!(d("dockerfile"), None);
    assert_eq!(d("a/b.py"), Some(Python));
    assert_eq!(d("A.PY"), Some(Python));
    assert_eq!(d("main.tf"), Some(Hcl));
    assert_eq!(d("prod.tfvars"), Some(Hcl));
    assert_eq!(d("terragrunt.hcl"), Some(Hcl));
    assert_eq!(d("t/x.j2"), Some(Jinja));
    assert_eq!(d("config.yaml.j2"), Some(Jinja));
    assert_eq!(d("x.jinja"), Some(Jinja));
    assert_eq!(d("x.jinja2"), Some(Jinja));
    assert_eq!(d("chart/templates/_helpers.tpl"), Some(GoTemplate));
    assert_eq!(d("values.yaml"), Some(Yaml));
    assert_eq!(d(".gitlab-ci.yml"), Some(Yaml));
    assert_eq!(d("run.sh"), Some(Bash));
    assert_eq!(d("x.bash"), Some(Bash));
    assert_eq!(d("README.md"), None);
    assert_eq!(d("Makefile"), None);
    assert_eq!(d(".bashrc"), None);
    assert_eq!(d(""), None);
    assert_eq!(d("dir/"), None);
}

#[test]
fn helm_templates_need_the_directory_and_template_syntax() {
    let t = b"apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Release.Name }}\n";
    assert_eq!(detect("chart/templates/cm.yaml", t), Some(GoTemplate));
    assert_eq!(detect("chart/templates/sub/cm.yml", t), Some(GoTemplate));
    assert_eq!(detect("chart/templates/NOTES.txt", t), Some(GoTemplate));
    assert_eq!(
        detect("chart/templates/cm.yaml", b"kind: ConfigMap\n"),
        Some(Yaml)
    );
    assert_eq!(detect("chart/mytemplates/cm.yaml", t), Some(Yaml));
    assert_eq!(detect("templates.yaml", t), Some(Yaml));
    assert_eq!(detect("chart/templates/x.md", t), None);
    assert_eq!(detect("chart/templates/x.py", t), Some(Python));
}

#[test]
fn shebangs() {
    assert_eq!(detect("bin/run", b"#!/bin/sh\necho"), Some(Bash));
    assert_eq!(detect("bin/run", b"#!/usr/bin/env bash\n"), Some(Bash));
    assert_eq!(
        detect("bin/run", b"#!/usr/bin/env -S bash -e\n"),
        Some(Bash)
    );
    assert_eq!(detect("bin/run", b"#!/bin/bash -eu\n"), Some(Bash));
    assert_eq!(detect("bin/run", b"#!/usr/bin/env python3\n"), None);
    assert_eq!(detect("bin/run", b"#!/bin/zsh\n"), None);
    assert_eq!(detect("bin/run", b"echo hi\n"), None);
    assert_eq!(detect("bin/run.txt", b"#!/bin/sh\n"), None);
}

#[test]
fn parse_is_total() {
    for language in Language::ALL {
        for text in ["", "\0\0\0", "{{{{ ((( [[[ '''", &"x".repeat(100_000)] {
            let p = rosetta_parse::parse(*language, "f", text);
            assert_eq!(p.nodes[0].kind, rosetta_model::NodeKind::File);
        }
    }
}
