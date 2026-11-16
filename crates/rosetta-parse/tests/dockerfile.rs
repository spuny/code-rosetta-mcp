mod common;

use common::{assert_well_formed, fixture, has_node, names, node, parent, targets};
use rosetta_model::{EdgeKind, Language, NodeKind, ParsedFile};

fn parse(text: &str) -> ParsedFile {
    let p = common::parse(Language::Dockerfile, "Dockerfile", text);
    assert_well_formed(&p);
    p
}

fn multistage() -> ParsedFile {
    parse(&fixture("dockerfile", "multistage.dockerfile"))
}

#[test]
fn empty_input_yields_the_file_node() {
    let p = common::parse(Language::Dockerfile, "dir/empty.dockerfile", "");
    common::assert_well_formed(&p);
    assert_eq!(p.nodes[0].name, "empty.dockerfile");
    assert_eq!(p.nodes.len(), 1);
}

#[test]
fn stages_with_aliases_images_tags_and_platform() {
    let p = multistage();
    assert_eq!(
        names(&p, NodeKind::Stage),
        ["builder", "runtime", "debug", "scratch"]
    );
    let builder = node(&p, NodeKind::Stage, "builder");
    assert_eq!(builder.extra["image"], "rust");
    assert_eq!(
        builder.extra["tag"], "1.98.1-trixie",
        "ARG default substituted"
    );
    assert_eq!(builder.extra["alias"], "builder");
    assert_eq!(builder.extra["platform"], "$BUILDPLATFORM");
    assert_eq!(builder.extra["index"], 0);
    assert_eq!((builder.line_start, builder.line_end), (6, 19));
    let runtime = node(&p, NodeKind::Stage, "runtime");
    assert_eq!(runtime.extra["image"], "debian");
    assert_eq!(runtime.extra["tag"], "trixie-slim");
    assert_eq!(runtime.extra["index"], 1);
    let scratch = node(&p, NodeKind::Stage, "scratch");
    assert!(scratch.extra.get("alias").is_none());
    assert!(scratch.extra.get("tag").is_none());
}

#[test]
fn unaliased_stage_is_named_image_and_tag() {
    let p = parse("FROM registry.local:5000/team/base:2.1\nFROM nginx\nFROM app@sha256:abc\n");
    assert_eq!(
        names(&p, NodeKind::Stage),
        ["registry.local:5000/team/base:2.1", "nginx", "app"]
    );
    let first = node(&p, NodeKind::Stage, "registry.local:5000/team/base:2.1");
    assert_eq!(first.extra["image"], "registry.local:5000/team/base");
    assert_eq!(first.extra["tag"], "2.1");
    let digest = node(&p, NodeKind::Stage, "app");
    assert_eq!(digest.extra["digest"], "sha256:abc");
}

#[test]
fn registry_port_is_not_a_tag() {
    let p = parse("FROM registry.local:5000/base\n");
    let stage = node(&p, NodeKind::Stage, "registry.local:5000/base");
    assert!(stage.extra.get("tag").is_none());
}

#[test]
fn identical_unaliased_stages_stay_unique() {
    let p = parse("FROM nginx\nFROM nginx\n");
    assert_eq!(names(&p, NodeKind::Stage).len(), 2);
}

#[test]
fn unknown_arg_substitution_stays_as_written() {
    let p = parse("ARG UNSET\nFROM ${UNSET}\n");
    assert!(has_node(&p, NodeKind::Stage, "${UNSET}"));
}

#[test]
fn depends_on_from_stage_aliases_and_copy_from() {
    let p = multistage();
    assert_eq!(
        targets(&p, EdgeKind::DependsOn, NodeKind::Stage, "runtime"),
        ["Stage::builder", "Stage::builder"],
        "COPY --from=builder and COPY --from=0"
    );
    assert_eq!(
        targets(&p, EdgeKind::DependsOn, NodeKind::Stage, "debug"),
        ["Stage::runtime"],
        "FROM <alias>"
    );
    assert_eq!(
        targets(&p, EdgeKind::DependsOn, NodeKind::Stage, "scratch"),
        ["Stage::runtime"]
    );
}

#[test]
fn copy_from_an_external_image_makes_no_edge() {
    let p = multistage();
    assert!(
        !targets(&p, EdgeKind::DependsOn, NodeKind::Stage, "runtime")
            .iter()
            .any(|t| t.contains("busybox"))
    );
}

#[test]
fn copy_and_add_sources_become_file_references() {
    let p = multistage();
    let refs = targets(&p, EdgeKind::References, NodeKind::Stage, "builder");
    assert_eq!(
        refs,
        [
            "file:./scripts/build.sh",
            "file:Cargo.lock",
            "file:Cargo.toml",
            "file:config/app.yaml",
            "file:crates/",
            "file:data/x.bin",
            "file:spaced name.txt",
        ]
    );
    let first = p
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::References)
        .expect("a reference");
    assert_eq!(first.extra["instruction"], "COPY");
    assert_eq!(first.line, 8);
}

#[test]
fn globs_urls_absolute_paths_and_the_context_root_are_skipped() {
    let p = parse(
        "FROM a\nCOPY src/*.rs /x/\nCOPY . .\nCOPY ./ /x/\nADD https://h/x.tgz /t/\nADD /abs /t/\nCOPY $FILE /t/\nCOPY a?.txt /t/\nCOPY lone\n",
    );
    assert!(p.edges.is_empty(), "{}", common::dump(&p));
}

#[test]
fn ports_with_protocols() {
    let p = multistage();
    let ports = names(&p, NodeKind::Port);
    assert_eq!(
        ports,
        [
            "runtime:8080",
            "runtime:9090/udp",
            "runtime:5000",
            "runtime:8000-8010",
            "debug:2345"
        ]
    );
    assert_eq!(
        node(&p, NodeKind::Port, "runtime:9090/udp").extra["protocol"],
        "udp"
    );
    assert_eq!(
        node(&p, NodeKind::Port, "runtime:8080").extra["protocol"],
        "tcp"
    );
    assert_eq!(
        node(&p, NodeKind::Port, "runtime:8080").extra["port"],
        "8080"
    );
    assert_eq!(parent(&p, NodeKind::Port, "debug:2345"), "Stage::debug");
}

#[test]
fn entrypoint_wins_over_cmd_and_cmd_is_the_fallback() {
    let p = multistage();
    let runtime = node(&p, NodeKind::Entrypoint, "runtime");
    assert_eq!(runtime.extra["kind"], "ENTRYPOINT");
    assert_eq!(runtime.extra["command"], "[\"app\", \"serve\"]");
    assert_eq!(runtime.extra["form"], "exec");
    assert_eq!(
        parent(&p, NodeKind::Entrypoint, "runtime"),
        "Stage::runtime"
    );
    let debug = node(&p, NodeKind::Entrypoint, "debug");
    assert_eq!(debug.extra["kind"], "CMD");
    assert_eq!(debug.extra["command"], "sleep infinity");
    assert_eq!(debug.extra["form"], "shell");
    assert!(!has_node(&p, NodeKind::Entrypoint, "builder"));
}

#[test]
fn the_last_entrypoint_of_a_stage_counts() {
    let p = parse("FROM a\nENTRYPOINT [\"one\"]\nENTRYPOINT [\"two\"]\n");
    assert_eq!(
        node(&p, NodeKind::Entrypoint, "a").extra["command"],
        "[\"two\"]"
    );
}

#[test]
fn heredoc_bodies_are_not_instructions() {
    let p = multistage();
    // `COPY not-an-instruction /x` sits inside the RUN heredoc of the debug stage.
    let refs = targets(&p, EdgeKind::References, NodeKind::Stage, "debug");
    assert!(refs.is_empty(), "{refs:?}");
    // The instructions after the heredocs are still seen.
    assert!(has_node(&p, NodeKind::Port, "debug:2345"));
}

#[test]
fn dash_heredoc_terminator_may_be_indented_with_tabs() {
    let p = parse("FROM a\nRUN <<-EOF\n\techo hi\n\tEOF\nEXPOSE 80\n");
    assert!(has_node(&p, NodeKind::Port, "a:80"));
}

#[test]
fn lowercase_instructions_and_continuations() {
    let p = parse("from alpine as base\nexpose 1\\\n2\nCOPY a \\\n  b \\\n  /dest\n");
    assert!(has_node(&p, NodeKind::Stage, "base"));
    assert!(has_node(&p, NodeKind::Port, "base:12") || has_node(&p, NodeKind::Port, "base:1"));
    let refs = targets(&p, EdgeKind::References, NodeKind::Stage, "base");
    assert_eq!(refs, ["file:a", "file:b"]);
}

#[test]
fn escape_directive_switches_the_continuation_character() {
    let p = parse("# escape=`\nFROM base\nCOPY a `\n  /dest\nEXPOSE 7\n");
    assert!(has_node(&p, NodeKind::Port, "base:7"));
    assert_eq!(
        targets(&p, EdgeKind::References, NodeKind::Stage, "base"),
        ["file:a"],
        "the backtick continues the COPY onto the next line"
    );
}

#[test]
fn instructions_before_the_first_from_are_ignored() {
    let p = parse("COPY a /b\nEXPOSE 80\nCMD x\nFROM a\n");
    assert_eq!(names(&p, NodeKind::Stage), ["a"]);
    assert!(names(&p, NodeKind::Port).is_empty());
    assert!(p.edges.is_empty());
}

#[test]
fn stage_node_extends_to_its_last_instruction() {
    let p = parse("FROM a AS one\nRUN x\nRUN y\n\nFROM b\n");
    let one = node(&p, NodeKind::Stage, "one");
    assert_eq!((one.line_start, one.line_end), (1, 3));
}

#[test]
fn malformed_input_is_total() {
    for text in [
        "FROM\n",
        "FROM a AS\n",
        "COPY\n",
        "COPY --from=\n",
        "EXPOSE\n",
        "EXPOSE /tcp\n",
        "ENTRYPOINT\n",
        "RUN <<EOF\nnever terminated",
        "garbage ::: ### \x00\x01",
        "FROM a\nCOPY [\"unterminated\n",
        "FROM a\\",
    ] {
        let p = common::parse(Language::Dockerfile, "Dockerfile", text);
        assert_well_formed(&p);
    }
}

#[test]
fn a_large_file_parses_quickly() {
    let mut text = String::new();
    for i in 0..20_000 {
        text.push_str(&format!(
            "FROM base{i} AS s{i}\nCOPY f{i} /x \\\n  && y\nEXPOSE {i}\n"
        ));
    }
    let start = std::time::Instant::now();
    let p = common::parse(Language::Dockerfile, "Dockerfile", &text);
    assert!(start.elapsed().as_secs() < 10, "took {:?}", start.elapsed());
    assert_eq!(names(&p, NodeKind::Stage).len(), 20_000);
}
