//! 这里只保留实际接入构建重试链的修复器。仅删除报错 obj 的修复方式很不稳定，
//! 容易引发更多莫名其妙的构建错误，因此彻底禁用并删除该实现。

use diag_trace::command_execution::CommandExecutionError;
use std::path::Path;

pub mod error_patterns;
pub mod vcxproj_target_clean_first;

pub trait CommandErrorFixer {
    fn try_fix(
        &mut self,
        err: &CommandExecutionError,
        ctx: &mut FixContext<'_>,
    ) -> anyhow::Result<FixActionResult>;
}

#[derive(Debug)]
pub struct FixContext<'a> {
    pub repo_path: &'a Path,
    pub rel_binary_prefix: &'a Path,
    pub build_config: &'a str,
    pub attempt: usize,
    pub max_attempts: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixActionResult {
    Applied,
    NotHandled,
}
