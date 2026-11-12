use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::task::{spawn_blocking, yield_now};
use tokio::time::timeout;

use aion_protocol::events::ToolCategory;
use aion_types::file_state::FileState;
use aion_types::tool::{JsonSchema, ToolResult};

use crate::Tool;
use crate::file_cache::{FileStateCache, file_mtime_ms};

pub struct ReadTool {
    file_cache: Option<Arc<RwLock<FileStateCache>>>,
}

impl ReadTool {
    /// Create a ReadTool with an optional file state cache.
    ///
    /// Pass `None` to disable caching (all reads return full content).
    pub fn new(file_cache: Option<Arc<RwLock<FileStateCache>>>) -> Self {
        Self { file_cache }
    }

    fn read_file(&self, file_path: &str, offset: Option<usize>, limit: Option<usize>) -> ToolResult {
        let mtime_ms = file_mtime_ms(Path::new(file_path));
        if let (Some(cache_arc), Some(current_mtime)) = (&self.file_cache, mtime_ms)
            && let Ok(mut cache) = cache_arc.write()
            && let Some(cached) = cache.get(Path::new(file_path))
            && cached.offset == offset
            && cached.limit == limit
            && cached.mtime_ms == current_mtime
        {
            return ToolResult {
                content: cached.content.clone(),
                is_error: false,
            };
        }
        let content = match std::fs::read(file_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                return ToolResult {
                    content: format!("Failed to read file {file_path}: {error}"),
                    is_error: true,
                };
            }
        };
        if content.iter().take(8192).any(|&byte| byte == 0) {
            return ToolResult {
                content: format!("(binary file, {} bytes)", content.len()),
                is_error: false,
            };
        }
        let text = String::from_utf8_lossy(&content);
        let lines: Vec<&str> = text.lines().collect();
        let effective_offset = offset.unwrap_or(0);
        let effective_limit = limit.unwrap_or(lines.len());
        let end = effective_offset.saturating_add(effective_limit).min(lines.len());
        let numbered: Vec<String> = lines[effective_offset.min(lines.len())..end]
            .iter()
            .enumerate()
            .map(|(index, line)| format!("{:>6}\t{}", effective_offset + index + 1, line))
            .collect();
        let result_content = numbered.join("\n");
        if let Some(cache_arc) = &self.file_cache
            && let (Ok(mut cache), Some(mtime)) = (cache_arc.write(), mtime_ms)
        {
            cache.insert(
                file_path.into(),
                FileState {
                    content: result_content.clone(),
                    mtime_ms: mtime,
                    offset,
                    limit,
                },
            );
        }
        ToolResult {
            content: result_content,
            is_error: false,
        }
    }
}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Reads a file from the local filesystem. Returns content with line numbers.\n\n\
         Usage:\n\
         - The file_path parameter must be an absolute path, not a relative path.\n\
         - By default, it reads the entire file. Use offset and limit for partial reads on large files.\n\
         - Results are returned with line numbers (1-based) followed by a tab and the line content.\n\
         - Binary files return \"(binary file, N bytes)\" instead of content.\n\
         - This tool can only read files, not directories. To list a directory, use ExecCommand with ls."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "The absolute path to the file to read"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (0-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["file_path"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(file_path) = input["file_path"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: file_path".to_string(),
                is_error: true,
            };
        };

        let offset = input["offset"].as_u64().map(|v| v as usize);
        let limit = input["limit"].as_u64().map(|v| v as usize);

        let file_path = file_path.to_owned();
        let reader = Self::new(self.file_cache.clone());
        // Disk and large text processing stay off the async executor. Dropping
        // this future stops waiting, but cannot forcibly abort a disk syscall.
        yield_now().await;
        let worker = spawn_blocking(move || reader.read_file(&file_path, offset, limit));
        match timeout(Duration::from_secs(30), worker).await {
            Ok(Ok(result)) => result,
            result => {
                let message = if result.is_err() {
                    "File read timed out; narrow the input or check the filesystem"
                } else {
                    "File read worker failed"
                };
                tracing::warn!(
                    tool = "Read",
                    failure_kind = if result.is_err() { "timeout" } else { "worker" },
                    "File read did not complete"
                );
                ToolResult {
                    content: message.to_owned(),
                    is_error: true,
                }
            }
        }
    }

    fn max_result_size(&self) -> usize {
        100_000
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    fn describe(&self, input: &Value) -> String {
        let path = input.get("file_path").and_then(|v| v.as_str()).unwrap_or("unknown");
        format!("Read {}", path)
    }
}

#[cfg(test)]
#[path = "read_test.rs"]
mod read_test;
