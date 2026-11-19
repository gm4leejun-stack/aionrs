use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::task::{spawn_blocking, yield_now};
use tokio::time::timeout;

use aion_protocol::events::ToolCategory;
use aion_types::tool::{JsonSchema, ToolResult};

use crate::Tool;

const MAX_RESULTS: usize = 100;
const SCAN_TIMEOUT: Duration = Duration::from_secs(30);

struct ScanCancellation(Arc<AtomicBool>);

impl Drop for ScanCancellation {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

fn scan_error(message: &str) -> ToolResult {
    ToolResult {
        content: message.to_owned(),
        is_error: true,
    }
}

fn scan_files(full_pattern: String, root_path: PathBuf, cancelled: &AtomicBool, deadline: Instant) -> ToolResult {
    if cancelled.load(Ordering::Relaxed) {
        return scan_error("Glob scan cancelled");
    }
    if Instant::now() >= deadline {
        return scan_error("Glob scan timed out; narrow the root directory or pattern");
    }
    let mut entries = match glob::glob(&full_pattern) {
        Ok(paths) => paths,
        Err(error) => return scan_error(&format!("Invalid glob pattern: {error}")),
    };
    let mut files: Vec<(SystemTime, String)> = Vec::new();
    while files.len() < MAX_RESULTS {
        if cancelled.load(Ordering::Relaxed) {
            return scan_error("Glob scan cancelled");
        }
        if Instant::now() >= deadline {
            return scan_error("Glob scan timed out; narrow the root directory or pattern");
        }
        let next = entries.next();
        if cancelled.load(Ordering::Relaxed) {
            return scan_error("Glob scan cancelled");
        }
        if Instant::now() >= deadline {
            return scan_error("Glob scan timed out; narrow the root directory or pattern");
        }
        let Some(entry) = next else { break };
        let Ok(path) = entry else { continue };
        if !path.is_file() {
            continue;
        }
        let mtime = path
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let display_path = path.strip_prefix(&root_path).unwrap_or(&path).display().to_string();
        files.push((mtime, display_path));
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.0));
    ToolResult {
        content: if files.is_empty() {
            "No files matched the pattern".to_owned()
        } else {
            files.into_iter().map(|(_, path)| path).collect::<Vec<_>>().join("\n")
        },
        is_error: false,
    }
}

pub struct GlobTool {
    cwd: PathBuf,
}

impl GlobTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Fast file pattern matching tool that works with any codebase size.\n\n\
         - Supports glob patterns like \"**/*.rs\" or \"src/**/*.ts\".\n\
         - Returns matching file paths sorted by modification time (newest first).\n\
         - Returns at most 100 results. Only returns files, not directories.\n\
         - The path parameter defaults to the current working directory.\n\
         - Use this tool when you need to find files by name or extension patterns."
    }

    fn input_schema(&self) -> JsonSchema {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, e.g. \"**/*.rs\""
                },
                "path": {
                    "type": "string",
                    "description": "Root directory (default: cwd)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn is_concurrency_safe(&self, _input: &Value) -> bool {
        true
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(pattern) = input["pattern"].as_str() else {
            return ToolResult {
                content: "Missing required parameter: pattern".to_string(),
                is_error: true,
            };
        };

        let root = input["path"].as_str().unwrap_or(".");
        let root_path = if Path::new(root).is_relative() {
            self.cwd.join(root)
        } else {
            PathBuf::from(root)
        };

        // Build full glob pattern
        let full_pattern = if pattern.starts_with('/') {
            pattern.to_string()
        } else {
            format!("{}/{}", root_path.display(), pattern)
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let _guard = ScanCancellation(Arc::clone(&cancelled));
        let deadline = Instant::now() + SCAN_TIMEOUT;
        // Blocking filesystem operations cannot be forcibly aborted. The worker
        // checks cancellation between entries; the runtime remains responsive.
        yield_now().await;
        let worker = spawn_blocking(move || scan_files(full_pattern, root_path, &cancelled, deadline));
        match timeout(SCAN_TIMEOUT, worker).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                tracing::error!(tool = "Glob", failure_kind = "scan_worker", "Glob worker failed");
                scan_error("Glob scan worker failed")
            }
            Err(_) => {
                tracing::warn!(
                    tool = "Glob",
                    timeout_secs = SCAN_TIMEOUT.as_secs(),
                    "Glob scan deadline reached"
                );
                scan_error("Glob scan timed out; narrow the root directory or pattern")
            }
        }
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Info
    }

    fn describe(&self, input: &Value) -> String {
        let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("*");
        format!("Search for {}", pattern)
    }
}

#[cfg(test)]
#[path = "glob_test.rs"]
mod glob_test;
