use crate::engine::Engine;
use rmcp::{ErrorData, RoleServer, ServerHandler, model::*, service::RequestContext};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Mcp(pub Engine);

pub fn catalog() -> Vec<Tool> {
    let string = json!({"type":"string"});
    let limit = json!({"type":"integer","minimum":1});
    let boolean = json!({"type":"boolean"});
    let files = json!({"type":"array","items":{"type":"string"},"maxItems":50});
    let definitions = [
        (
            "search_nodes",
            "Search indexed node names and YAML keys, with camelCase/snake_case tokenization. Filter by repository glob, node kind or language. Default limit 20; maximum 500.",
            json!({"query":string,"kind":string,"language":string,"repo":string,"limit":limit}),
            vec!["query"],
        ),
        (
            "grep_code",
            "Search indexed text using a literal or Rust regular expression. Filter by repository or file glob. Default limit 100; maximum 5000 matches or 20000 files. Credential files are withheld.",
            json!({"pattern":string,"regex":boolean,"ignore_case":boolean,"repo":string,"path_glob":string,"limit":limit,"files_only":boolean}),
            vec!["pattern"],
        ),
        (
            "query_graph",
            "Query callers_of, callees_of, imports_of, importers_of, children_of, tests_for, inheritors_of, references_to, file_summary or cross_language. Prefer qualified targets to avoid ambiguity. Default limit 100; maximum 1000.",
            json!({"pattern":string,"target":string,"repo":string,"limit":limit}),
            vec!["pattern"],
        ),
        (
            "get_impact_radius",
            "Trace dependencies from changed indexed files, addressed as <repo>/<path>. CONTAINS edges seed file members but never fan out. At most 50 files, depth 6 and 5000 results.",
            json!({"changed_files":files,"max_depth":{"type":"integer","minimum":0},"max_results":limit}),
            vec!["changed_files"],
        ),
        (
            "get_review_context",
            "Get changed and impacted graph nodes plus bounded source snippets. Optional sources override indexed text for changed files only. At most 50 files; source overrides are limited to 200 kB each.",
            json!({"changed_files":files,"max_depth":{"type":"integer","minimum":0},"include_source":boolean,"max_lines_per_file":limit,"sources":{"type":"object","additionalProperties":{"type":"string"}}}),
            vec!["changed_files"],
        ),
        (
            "list_graph_stats",
            "Count indexed repositories, files, nodes, edges, unresolved references and languages. Optionally restrict counts to a repository glob.",
            json!({"repo":string}),
            vec![],
        ),
    ];
    definitions.into_iter().map(|(name,description,mut properties,required)| {
        let properties = properties.as_object_mut().expect("static schema object");
        properties.insert("group".into(), json!({"type":"string","description":"Accepted for legacy clients; ignored."}));
        properties.insert("repo_root".into(), json!({"type":"string","description":"Unsupported; index local paths using the CLI."}));
        let schema = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
        Tool::new(name, description, schema.as_object().expect("static schema object").clone())
            .with_annotations(ToolAnnotations::new().read_only(true).destructive(false).idempotent(true).open_world(false))
    }).collect()
}

impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("rosetta", env!("CARGO_PKG_VERSION")))
            .with_instructions("Read-only code graph and text search. Index repositories with the Rosetta CLI before querying.")
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        catalog().into_iter().find(|t| t.name == name)
    }

    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult {
            tools: catalog(),
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if self.get_tool(&request.name).is_none() {
            return Err(ErrorData::invalid_params("unknown tool", None));
        }
        let arguments = Value::Object(request.arguments.unwrap_or_default());
        let result = tokio::select! {
            _ = context.ct.cancelled() => Err(rosetta_model::tools::ToolError::new("request cancelled")),
            result = tokio::time::timeout(Duration::from_secs(30), self.0.call(&request.name,arguments)) => {
                result.unwrap_or_else(|_| Err(rosetta_model::tools::ToolError::unavailable("request timed out")))
            }
        };
        let response = match result {
            Ok(value) => CallToolResult::success(vec![ContentBlock::text(value.to_string())]),
            Err(error) => CallToolResult::error(vec![ContentBlock::text(
                serde_json::to_string(&error).unwrap_or_else(|_| "tool failed".into()),
            )]),
        };
        Ok(response.into())
    }
}
