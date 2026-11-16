use crate::{graph::Graph, storage::Storage};
use rosetta_model::tools::{ToolError, ToolResult};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};

#[derive(Clone)]
pub struct Engine {
    pub storage: Storage,
    cache: Arc<Mutex<Option<Arc<Graph>>>>,
    permits: Arc<Semaphore>,
}

impl Engine {
    pub fn new(storage: Storage) -> Self {
        Self {
            storage,
            cache: Arc::new(Mutex::new(None)),
            permits: Arc::new(Semaphore::new(16)),
        }
    }

    pub async fn graph(&self) -> ToolResult<Arc<Graph>> {
        let mut cache = self.cache.lock().await;
        let revision = self
            .storage
            .revision()
            .await
            .map_err(|_| ToolError::unavailable("storage is unavailable"))?;
        if let Some(graph) = cache.as_ref().filter(|g| g.revision == revision) {
            return Ok(graph.clone());
        }
        let snapshot = self
            .storage
            .load()
            .await
            .map_err(|_| ToolError::unavailable("cannot read graph storage"))?;
        let graph = Arc::new(
            tokio::task::spawn_blocking(move || Graph::new(snapshot))
                .await
                .map_err(|_| ToolError::unavailable("graph construction failed"))?,
        );
        *cache = Some(graph.clone());
        Ok(graph)
    }

    pub async fn call(&self, name: &str, arguments: Value) -> ToolResult<Value> {
        if serde_json::to_vec(&arguments).map_or(true, |v| v.len() > 1_000_000) {
            return Err(ToolError::new("tool arguments exceed 1 MB"));
        }
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| ToolError::unavailable("server busy"))?;
        let graph = self.graph().await?;
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let result = graph.call(&name, arguments)?;
            if serde_json::to_vec(&result).map_or(true, |v| v.len() > 8_000_000) {
                return Err(ToolError::new(
                    "tool result exceeds 8 MB; narrow the request",
                ));
            }
            Ok(result)
        })
        .await
        .map_err(|_| ToolError::unavailable("tool execution failed"))?
    }
}
