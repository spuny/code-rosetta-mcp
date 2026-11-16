//! The closed vocabularies of the graph. Every enum serialises as its `as_str()` text, which is
//! also what the database stores, so the strings below are part of the stable API.
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{fmt, str::FromStr};

/// Error of the `FromStr` implementations: the input is not one of the known names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownName {
    pub kind: &'static str,
    pub value: String,
}

impl fmt::Display for UnknownName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unknown {} {:?}", self.kind, self.value)
    }
}

impl std::error::Error for UnknownName {}

macro_rules! vocabulary {
    ($(#[$meta:meta])* $name:ident, $label:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum $name { $($variant),+ }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }

        impl FromStr for $name {
            type Err = UnknownName;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok($name::$variant),)+
                    _ => Err(UnknownName { kind: $label, value: value.to_owned() }),
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = String::deserialize(deserializer)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

vocabulary!(
    /// Source language of a parsed file.
    Language, "language", {
        Python => "python",
        Hcl => "hcl",
        Yaml => "yaml",
        Jinja => "jinja2",
        Dockerfile => "dockerfile",
        Bash => "bash",
        GoTemplate => "gotemplate",
    }
);

vocabulary!(
    /// Kind of a graph node. The text is the variant name.
    NodeKind, "node kind", {
        File => "File",
        Class => "Class",
        Function => "Function",
        Method => "Method",
        Resource => "Resource",
        DataSource => "DataSource",
        Module => "Module",
        Variable => "Variable",
        Output => "Output",
        Provider => "Provider",
        Local => "Local",
        TerraformConfig => "TerraformConfig",
        Document => "Document",
        Section => "Section",
        K8sResource => "K8sResource",
        Anchor => "Anchor",
        Template => "Template",
        Block => "Block",
        Macro => "Macro",
        Stage => "Stage",
        Port => "Port",
        Entrypoint => "Entrypoint",
    }
);

vocabulary!(
    /// Kind of a graph edge.
    EdgeKind, "edge kind", {
        Contains => "CONTAINS",
        Calls => "CALLS",
        Imports => "IMPORTS",
        Inherits => "INHERITS",
        References => "REFERENCES",
        PassesVar => "PASSES_VAR",
        UsesModule => "USES_MODULE",
        Extends => "EXTENDS",
        Includes => "INCLUDES",
        UsesVariable => "USES_VARIABLE",
        CallsMacro => "CALLS_MACRO",
        DependsOn => "DEPENDS_ON",
        ReadsConfig => "READS_CONFIG",
        Renders => "RENDERS",
        RemoteState => "REMOTE_STATE",
    }
);

impl EdgeKind {
    /// Structural edges (only CONTAINS) are derived by the store from `Node::parent` and are
    /// never followed by impact analysis.
    pub const fn is_structural(self) -> bool {
        matches!(self, EdgeKind::Contains)
    }
}

vocabulary!(
    /// Scheme of a reference that a parser could not resolve inside its own file. The value
    /// format and the resolution rule of every scheme are defined in docs/design.md section 4.
    RefScheme, "reference scheme", {
        Py => "py",
        File => "file",
        Template => "template",
        Tf => "tf",
        TfModule => "tfmodule",
        TfVar => "tfvar",
        TfState => "tfstate",
        K8s => "k8s",
        Helm => "helm",
        Values => "values",
    }
);

vocabulary!(
    /// The graph query patterns of the `query_graph` tool.
    QueryPattern, "query pattern", {
        CallersOf => "callers_of",
        CalleesOf => "callees_of",
        ImportsOf => "imports_of",
        ImportersOf => "importers_of",
        ChildrenOf => "children_of",
        TestsFor => "tests_for",
        InheritorsOf => "inheritors_of",
        ReferencesTo => "references_to",
        FileSummary => "file_summary",
        CrossLanguage => "cross_language",
    }
);

impl QueryPattern {
    /// One-line description, used in tool results and documentation.
    pub const fn description(self) -> &'static str {
        match self {
            QueryPattern::CallersOf => "Nodes that call the target (CALLS edges into it)",
            QueryPattern::CalleesOf => "Nodes the target calls (CALLS edges out of it)",
            QueryPattern::ImportsOf => "What the target imports (IMPORTS edges out of it)",
            QueryPattern::ImportersOf => {
                "Nodes that import the target or any node of the target's file"
            }
            QueryPattern::ChildrenOf => "Nodes the target contains (CONTAINS edges out of it)",
            QueryPattern::TestsFor => "Test nodes that call or import the target, plus test_<name>",
            QueryPattern::InheritorsOf => "Classes that inherit from the target",
            QueryPattern::ReferencesTo => {
                "Nodes that reference, configure, render, pass variables to or use the target"
            }
            QueryPattern::FileSummary => "Every node of one file",
            QueryPattern::CrossLanguage => "Edges whose two ends are in different languages",
        }
    }

    /// Whether the pattern needs a resolved target node (all except `cross_language`;
    /// `file_summary` resolves its target as a file).
    pub const fn needs_target(self) -> bool {
        !matches!(self, QueryPattern::CrossLanguage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for kind in NodeKind::ALL {
            assert_eq!(kind.as_str().parse::<NodeKind>(), Ok(*kind));
        }
        for kind in EdgeKind::ALL {
            assert_eq!(kind.as_str().parse::<EdgeKind>(), Ok(*kind));
        }
        for scheme in RefScheme::ALL {
            assert_eq!(scheme.as_str().parse::<RefScheme>(), Ok(*scheme));
        }
        for language in Language::ALL {
            assert_eq!(language.as_str().parse::<Language>(), Ok(*language));
        }
        for pattern in QueryPattern::ALL {
            assert_eq!(pattern.as_str().parse::<QueryPattern>(), Ok(*pattern));
        }
        assert_eq!(Language::Jinja.as_str(), "jinja2");
        assert_eq!(NodeKind::K8sResource.as_str(), "K8sResource");
        assert!("k8sresource".parse::<NodeKind>().is_err());
    }

    #[test]
    fn serde_uses_the_text_form() {
        assert_eq!(
            serde_json::to_string(&EdgeKind::PassesVar).unwrap(),
            "\"PASSES_VAR\""
        );
        assert_eq!(
            serde_json::from_str::<Language>("\"gotemplate\"").unwrap(),
            Language::GoTemplate
        );
        assert!(serde_json::from_str::<Language>("\"rust\"").is_err());
        assert!(EdgeKind::Contains.is_structural());
        assert!(!EdgeKind::Calls.is_structural());
    }
}
