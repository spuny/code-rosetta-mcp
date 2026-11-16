mod common;

use common::{assert_edge, assert_well_formed, dump, fixture, has_node, names, node, targets};
use rosetta_model::{EdgeKind, Language, NodeKind, ParsedFile};

fn parse_as(path: &str, text: &str) -> ParsedFile {
    let p = common::parse(Language::GoTemplate, path, text);
    assert_well_formed(&p);
    p
}

fn helpers() -> ParsedFile {
    parse_as(
        "charts/mychart/templates/_helpers.tpl",
        &fixture("gotemplate", "_helpers.tpl"),
    )
}

fn deployment() -> ParsedFile {
    parse_as(
        "charts/mychart/templates/deployment.yaml",
        &fixture("gotemplate", "deployment.yaml"),
    )
}

fn strings(p: &ParsedFile, key: &str) -> Vec<String> {
    p.nodes[0].extra[key]
        .as_array()
        .unwrap_or_else(|| panic!("no {key} in {}", dump(p)))
        .iter()
        .map(|v| v.as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn empty_input_yields_the_file_node() {
    let p = common::parse(Language::GoTemplate, "dir/empty.tpl", "");
    common::assert_well_formed(&p);
    assert_eq!(p.nodes[0].name, "empty.tpl");
    assert_eq!(p.nodes.len(), 1);
}

#[test]
fn text_without_actions_is_just_the_file_node() {
    let p = parse_as("templates/NOTES.txt", "plain text\nwith no actions\n");
    assert_eq!(p.nodes.len(), 1);
    assert!(p.edges.is_empty());
}

#[test]
fn helpers_define_templates_with_their_extent() {
    let p = helpers();
    assert_eq!(p.parse_error, None, "{}", dump(&p));
    assert_eq!(
        names(&p, NodeKind::Template),
        ["mychart.name", "mychart.fullname", "mychart.labels"]
    );
    let name = node(&p, NodeKind::Template, "mychart.name");
    assert_eq!((name.line_start, name.line_end), (4, 6));
    let fullname = node(&p, NodeKind::Template, "mychart.fullname");
    assert_eq!((fullname.line_start, fullname.line_end), (8, 15));
    assert_eq!(fullname.parent, Some(0));
    assert_eq!(fullname.extra["directive"], "define");
    assert_eq!(
        strings(&p, "templates_defined"),
        ["mychart.name", "mychart.fullname", "mychart.labels"]
    );
}

#[test]
fn include_calls_are_local_when_defined_in_the_file_and_helm_refs_otherwise() {
    let p = helpers();
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Template,
        "mychart.fullname",
        "Template::mychart.name",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Template,
        "mychart.labels",
        "helm:mychart.chart",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::Template,
        "mychart.labels",
        "helm:mychart.selectorLabels",
    );
    assert_eq!(
        strings(&p, "templates_included"),
        ["mychart.name", "mychart.chart", "mychart.selectorLabels"]
    );
}

#[test]
fn calls_outside_a_define_come_from_the_file() {
    let p = deployment();
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "deployment.yaml",
        "helm:mychart.fullname",
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "deployment.yaml",
        "helm:mychart.labels",
    );
    assert_eq!(
        strings(&p, "templates_included"),
        ["mychart.fullname", "mychart.labels"]
    );
}

#[test]
fn values_reads_use_exact_top_level_keys_and_record_paths() {
    let p = helpers();
    assert_eq!(
        targets(&p, EdgeKind::ReadsConfig, NodeKind::File, "_helpers.tpl"),
        ["values:fullnameOverride", "values:nameOverride"]
    );
    assert_eq!(
        strings(&p, "values_paths"),
        ["nameOverride", "fullnameOverride"]
    );
    // `.Chart.Name` and `.Release.Name` are not values
    assert!(
        !strings(&p, "values_paths")
            .iter()
            .any(|v| v.contains("Chart"))
    );
}

#[test]
fn values_in_a_manifest_with_with_and_dollar_and_index() {
    let p = deployment();
    assert_eq!(
        targets(&p, EdgeKind::ReadsConfig, NodeKind::File, "deployment.yaml"),
        [
            "values:autoscaling",
            "values:env",
            "values:extra",
            "values:image",
            "values:imagePullSecrets",
            "values:quoted",
            "values:replicaCount",
            "values:securityContext"
        ]
    );
    let paths = strings(&p, "values_paths");
    for expected in [
        "autoscaling.enabled",
        "replicaCount",
        "imagePullSecrets",
        "image.repository",
        "image.tag",
        "securityContext",
        "securityContext.runAsUser",
        "extra.flag",
        "env",
        "quoted.key",
    ] {
        assert!(
            paths.iter().any(|p| p == expected),
            "{expected} in {paths:?}"
        );
    }
    // inside `range` the dot is an element: `.name` is no values path
    assert!(
        !paths.iter().any(|p| p.ends_with(".name") || p == "name"),
        "{paths:?}"
    );
    let edge = p
        .edges
        .iter()
        .find(
            |e| matches!(&e.target, rosetta_model::Target::Ref(r) if r.value == "securityContext"),
        )
        .expect("securityContext edge");
    assert_eq!(
        edge.extra["paths"],
        serde_json::json!(["securityContext", "securityContext.runAsUser"])
    );
    assert_eq!(edge.line, 20);
}

#[test]
fn else_restores_the_dot_of_with() {
    let p = parse_as(
        "t.tpl",
        "{{ with .Values.a }}{{ .x }}{{ else }}{{ .Values.b }}{{ .y }}{{ end }}{{ .z }}",
    );
    let paths = strings(&p, "values_paths");
    assert_eq!(paths, ["a", "a.x", "b"]);
}

#[test]
fn block_defines_and_calls_its_template() {
    let p = parse_as(
        "t.tpl",
        "{{ block \"foo\" . }}default {{ .Values.x }}{{ end }}\n{{ template \"foo\" . }}",
    );
    assert_eq!(names(&p, NodeKind::Template), ["foo"]);
    assert_eq!(
        node(&p, NodeKind::Template, "foo").extra["directive"],
        "block"
    );
    assert_edge(
        &p,
        EdgeKind::Calls,
        NodeKind::File,
        "t.tpl",
        "Template::foo",
    );
    // both the `block` itself and the later `template` call come from the file
    let calls = p.edges.iter().filter(|e| e.kind == EdgeKind::Calls).count();
    assert_eq!(calls, 2);
}

#[test]
fn dynamic_template_names_are_skipped() {
    let p = parse_as(
        "t.tpl",
        "{{ include (print $.Template.BasePath \"/x\") . }}{{ template $name . }}{{ include .Values.name . }}",
    );
    assert!(
        p.edges.iter().all(|e| e.kind != EdgeKind::Calls),
        "{}",
        dump(&p)
    );
    assert!(p.nodes[0].extra.get("templates_included").is_none());
}

#[test]
fn comments_and_trim_markers_are_understood() {
    let p = parse_as(
        "t.tpl",
        "{{/* {{ define \"no\" }} .Values.no */}}{{- /* c */ -}}{{- define \"yes\" -}}{{- .Values.k -}}{{- end -}}",
    );
    assert_eq!(names(&p, NodeKind::Template), ["yes"]);
    assert_eq!(strings(&p, "values_paths"), ["k"]);
}

#[test]
fn multi_line_actions_and_strings() {
    let p = parse_as(
        "t.tpl",
        "{{ include \"a\"\n  .\n  | nindent 4 }}\n{{ `raw }} {{ .Values.not` }}{{ \"}} .Values.nor\" }}\n{{ .Values.real }}",
    );
    assert_eq!(strings(&p, "values_paths"), ["real"]);
    let e = p
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::Calls)
        .expect("call");
    assert_eq!(e.line, 1);
}

#[test]
fn values_alone_and_negative_numbers_are_not_keys() {
    let p = parse_as("t.tpl", "{{ toYaml .Values }}{{-3}}{{ .Values.a.b.c }}");
    assert_eq!(strings(&p, "values_paths"), ["a.b.c"]);
}

#[test]
fn unterminated_actions_set_parse_error_and_keep_the_rest() {
    let p = parse_as(
        "t.tpl",
        "{{ define \"x\" }}{{ .Values.ok }}\nkind: {{ .Values.broken\nname: y",
    );
    assert!(
        p.parse_error
            .as_deref()
            .is_some_and(|e| e.contains("line 2")),
        "{:?}",
        p.parse_error
    );
    assert!(has_node(&p, NodeKind::Template, "x"));
    // the broken action is still read up to the end of its line
    assert_eq!(strings(&p, "values_paths"), ["ok", "broken"]);
    // the unclosed define runs to the end of the file
    assert_eq!(node(&p, NodeKind::Template, "x").line_end, 3);
}

#[test]
fn unbalanced_ends_and_odd_input_do_not_panic() {
    for text in [
        "{{ end }}{{ end }}{{ else }}",
        "{{ define }}{{ block }}{{ range }}{{ with }}{{ if }}",
        "{{ template }}{{ include }}{{ index }}{{ index .Values }}",
        "{{ $ }}{{ $. }}{{ . }}{{ .. }}{{ .Values. }}{{ $x.y.z }}",
        "{{",
        "{{}}{{-}}{{ - }}",
        "é{{ é }}é{{ \"é }}",
    ] {
        let p = parse_as("t.tpl", text);
        assert_eq!(p.nodes[0].kind, NodeKind::File, "{text:?}");
    }
}

#[test]
fn large_and_hostile_input_stays_fast() {
    let started = std::time::Instant::now();
    let big =
        "{{- define \"a.b\" -}}{{ .Values.x.y }}{{ include \"c.d\" . }}{{- end }}\n".repeat(15_000);
    assert!(big.len() > 900_000);
    let p = parse_as("t.tpl", &big);
    assert!(p.nodes.len() > 10_000);
    parse_as("t.tpl", &"{{ with .Values.a }}".repeat(20_000));
    parse_as("t.tpl", &"{{ \"\n".repeat(150_000));
    assert!(started.elapsed().as_secs() < 20, "{:?}", started.elapsed());
}

/// `scheme:value` of every reference target of edges of `kind`, from any source.
fn ref_targets(p: &ParsedFile, kind: EdgeKind) -> Vec<String> {
    p.edges
        .iter()
        .filter(|e| e.kind == kind)
        .filter_map(|e| match &e.target {
            rosetta_model::Target::Ref(r) => Some(r.to_string()),
            rosetta_model::Target::Local(_) => None,
        })
        .collect()
}

#[test]
fn templated_manifest_yields_kubernetes_structure() {
    let text = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {{ include \"app.fullname\" . }}-web\n  namespace: {{ .Release.Namespace }}\nspec:\n  replicas: {{ .Values.replicaCount }}\n  template:\n    spec:\n      {{- with .Values.serviceAccount }}\n      serviceAccountName: {{ .name }}\n      {{- end }}\n      containers:\n        - name: web\n          image: \"{{ .Values.image.repository }}:{{ .Values.image.tag }}\"\n          envFrom:\n            - configMapRef:\n                name: shared-config\n            - secretRef:\n                name: {{ .Release.Name }}-secret\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: web\nspec:\n  ports:\n    - port: 80\n";
    let p = parse_as("chart/templates/deployment.yaml", text);
    assert!(p.parse_error.is_none(), "{}", dump(&p));
    let deployment = node(&p, NodeKind::K8sResource, "Deployment/*-web");
    assert_eq!(
        deployment.extra.get("templated"),
        Some(&serde_json::Value::Bool(true))
    );
    assert!(deployment.extra.get("namespace").is_none(), "{}", dump(&p));
    assert!(
        has_node(&p, NodeKind::K8sResource, "Service/web"),
        "{}",
        dump(&p)
    );
    let refs = ref_targets(&p, EdgeKind::References);
    assert!(
        refs.iter().any(|t| t == "k8s:ConfigMap/shared-config"),
        "{refs:?}"
    );
    assert!(
        refs.iter()
            .all(|t| !t.contains("xxxxx") && !t.contains('*')),
        "{refs:?}"
    );
    assert!(
        ref_targets(&p, EdgeKind::ReadsConfig)
            .iter()
            .any(|t| t == "values:replicaCount"),
        "{}",
        dump(&p)
    );
}

#[test]
fn template_that_is_not_yaml_keeps_template_output_without_noise() {
    let text = "{{- range .Values.items }}\n{{ .name }}: [unclosed\n  - {{ .value }}\n{{- end }}\nkey: {value\n";
    let p = parse_as("chart/templates/list.yaml", text);
    assert!(p.parse_error.is_none(), "{}", dump(&p));
    assert!(names(&p, NodeKind::K8sResource).is_empty());
    assert!(names(&p, NodeKind::Section).is_empty(), "{}", dump(&p));
    assert!(
        ref_targets(&p, EdgeKind::ReadsConfig)
            .iter()
            .any(|t| t == "values:items"),
        "{}",
        dump(&p)
    );
}

#[test]
fn helpers_are_not_parsed_as_yaml() {
    let p = helpers();
    assert!(names(&p, NodeKind::Section).is_empty(), "{}", dump(&p));
    assert!(names(&p, NodeKind::K8sResource).is_empty());
}
