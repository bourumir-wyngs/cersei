//! Can safely run Rust tests that are wasm32-wasip1 compatible (no filesystem, no network).
//!
//! Skeleton module for future wasm test integration.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmTestConfig {
    pub project_root: PathBuf,
}

impl WasmTestConfig {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmTestArtifact {
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmTestRun {
    pub artifact: PathBuf,
    pub test_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WasmTestOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum WasmTestError {
    #[error("wasm test support is not implemented yet")]
    NotImplemented,
}

pub type Result<T> = std::result::Result<T, WasmTestError>;

pub fn discover_artifacts(_config: &WasmTestConfig) -> Result<Vec<WasmTestArtifact>> {
    Err(WasmTestError::NotImplemented)
}

pub fn run_test_by_name(_config: &WasmTestConfig, _test_name: &str) -> Result<Vec<WasmTestOutput>> {
    Err(WasmTestError::NotImplemented)
}

pub fn run_artifact(_config: &WasmTestConfig, _artifact: &Path, _args: &[String]) -> Result<WasmTestOutput> {
    Err(WasmTestError::NotImplemented)
}
