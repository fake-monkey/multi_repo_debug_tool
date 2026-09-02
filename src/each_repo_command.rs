use crate::cli::{EachCommand, RunArgs};
use crate::{build_and_print_topology, prepare_runtime_ctx};
use diag_trace::command_execution::get_cmd_str;
use diag_trace::{anyhow_loc, err_loc, LocContextExt};
use dialoguer::{theme::ColorfulTheme, Confirm};
use log::{info, warn};
use path_absolutize::Absolutize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
#[error("{detail}")]
pub(crate) struct EachRepoExecutionError {
    detail: String,
    deferred_errors: Vec<anyhow::Error>,
    #[source]
    interrupted: Option<anyhow::Error>,
}

impl EachRepoExecutionError {
    fn new(deferred_errors: Vec<anyhow::Error>, interrupted: Option<anyhow::Error>) -> Self {
        let mut detail = String::new();
        if !deferred_errors.is_empty() {
            detail.push_str(&format!(
                "each command recorded {} deferred error(s):",
                deferred_errors.len()
            ));
        }

        for (idx, err) in deferred_errors.iter().enumerate() {
            detail.push_str(&format!("\n  {}. {}", idx + 1, err));
        }

        if let Some(interrupted) = &interrupted {
            if !detail.is_empty() {
                detail.push_str("\n\n");
            }
            detail.push_str(&format!(
                "each command interrupted at repo '{}'",
                interrupted
            ));
        }

        Self {
            detail,
            deferred_errors,
            interrupted,
        }
    }
}

fn run_repo_command<F>(
    repo_names: &[String],
    deferred_errors: &mut Vec<anyhow::Error>,
    action: &str,
    op: F,
) -> anyhow::Result<()>
where
    F: Fn(&str) -> anyhow::Result<()>,
{
    for repo_name in repo_names {
        if let Err(error) =
            run_or_prompt_continue(deferred_errors, repo_name, action, || op(repo_name))
        {
            return Err(
                EachRepoExecutionError::new(std::mem::take(deferred_errors), Some(error)).into(),
            );
        }
    }
    Ok(())
}

/// `each` 子命令：配置与拓扑与主流程一致，但不执行 Conan/CMake/merge sln；不按 `--merge` 跑合并任务。
pub(crate) fn run(cli_args: &RunArgs, each_cmd: &EachCommand) -> anyhow::Result<()> {
    let switch_remote = each_cmd.switch_remote.as_deref();
    let force = each_cmd.force;
    let cmd = each_cmd.cmd.as_slice();
    if switch_remote.is_none() && cmd.is_empty() {
        return err_loc!(
            "each command arguments are invalid!\n\teach: either --switch-remote or command must be provided"
        );
    }

    let runtime_ctx = prepare_runtime_ctx(cli_args)?;
    let topological_info = build_and_print_topology(&runtime_ctx)?;

    if cli_args.check_only {
        return Ok(());
    }

    let selected_repo_names = filter_repo_names_by_path(
        &runtime_ctx,
        &topological_info.sorted_names,
        each_cmd.only.as_slice(),
        each_cmd.except.as_slice(),
    )?;
    info!(
        "each: selected {} repo(s): {}",
        selected_repo_names.len(),
        selected_repo_names.join(", ")
    );
    let mut deferred_errors = Vec::new();

    fn get_repo_dir(runtime_ctx: &crate::FileConfig, repo_name: &str) -> anyhow::Result<PathBuf> {
        runtime_ctx
            .get_repo_dir(repo_name)
            .with_loc_context(|| format!("failed to get repo dir for repo '{}'", repo_name))
    }

    run_repo_command(
        selected_repo_names.as_slice(),
        &mut deferred_errors,
        "switch remote",
        |repo_name| {
            let repo_dir = get_repo_dir(&runtime_ctx, repo_name)?;
            if let Some(branch) = switch_remote {
                sync_branch_in_repo(repo_name, &repo_dir, branch, force)?;
            }
            Ok(())
        },
    )?;

    run_repo_command(
        selected_repo_names.as_slice(),
        &mut deferred_errors,
        &format!("custom command: {}", cmd.join(" ")),
        |repo_name| {
            let repo_dir = get_repo_dir(&runtime_ctx, repo_name)?;
            run_custom_command_in_repo(repo_name, &repo_dir, cmd)?;
            Ok(())
        },
    )?;

    if !deferred_errors.is_empty() {
        return Err(EachRepoExecutionError::new(deferred_errors, None).into());
    }

    Ok(())
}

fn filter_repo_names_by_path(
    runtime_ctx: &crate::FileConfig,
    sorted_repo_names: &[String],
    only_paths: &[PathBuf],
    except_paths: &[PathBuf],
) -> anyhow::Result<Vec<String>> {
    let full_path_map = runtime_ctx.build_full_path_map();
    let only_names = convert_paths_to_repo_name_set(&full_path_map, only_paths)?;
    let except_names = convert_paths_to_repo_name_set(&full_path_map, except_paths)?;

    Ok(sorted_repo_names
        .iter()
        .filter(|repo_name| only_names.is_empty() || only_names.contains(*repo_name))
        .filter(|repo_name| !except_names.contains(*repo_name))
        .cloned()
        .collect())
}

fn convert_paths_to_repo_name_set(
    full_path_map: &HashMap<PathBuf, String>,
    paths: &[PathBuf],
) -> anyhow::Result<HashSet<String>> {
    paths
        .iter()
        .map(|path| {
            let full_path = path
                .absolutize()
                .with_loc_context(|| format!("failed to absolutize path {}", path.display()))
                .map(|p| p.to_path_buf())?;
            full_path_map.get(&full_path).cloned().ok_or_else(|| {
                anyhow_loc!(
                    "No identification name found for directory: {}",
                    full_path.display()
                )
            })
        })
        .collect()
}

fn run_or_prompt_continue<F>(
    deferred_errors: &mut Vec<anyhow::Error>,
    repo_name: &str,
    action: &str,
    op: F,
) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()>,
{
    run_or_prompt_continue_with_confirm(deferred_errors, repo_name, action, op, |err| {
        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Repo '{}' failed while running {}. The error will be recorded. Continue with remaining repos?",
                repo_name, action
            ))
            .default(false)
            .interact()
            .with_loc_context(|| {
                format!(
                    "failed to read continue confirmation after repo '{}' error while running {}",
                    repo_name,
                    action,
                )
            })
            .inspect(|should_continue| {
                if *should_continue {
                    warn!(
                        "each: deferred error for repo '{}' while running {}: {:#}",
                        repo_name, action, err
                    );
                }
            })
    })
}

fn run_or_prompt_continue_with_confirm<F, C>(
    deferred_errors: &mut Vec<anyhow::Error>,
    repo_name: &str,
    action: &str,
    op: F,
    confirm_continue: C,
) -> anyhow::Result<()>
where
    F: FnOnce() -> anyhow::Result<()>,
    C: FnOnce(&anyhow::Error) -> anyhow::Result<bool>,
{
    let Err(source) = op() else {
        return Ok(());
    };

    let should_continue = confirm_continue(&source).with_loc_context(|| {
        format!(
            "failed to get continue confirmation after repo '{}' error while running {}",
            repo_name, action
        )
    })?;
    if should_continue {
        deferred_errors.push(source);
        Ok(())
    } else {
        Err(source)
    }
}

fn sync_branch_in_repo(
    repo_name: &str,
    repo_dir: &Path,
    branch: &str,
    force: bool,
) -> anyhow::Result<()> {
    info!(
        "each: repo '{}' syncing to branch '{}' in {}",
        repo_name,
        branch,
        repo_dir.display()
    );
    git_tool::sync_branch::sync_branch(repo_dir, branch, force).with_loc_context(|| {
        format!(
            "failed to sync repo '{}' to branch '{}' in {}",
            repo_name,
            branch,
            repo_dir.display()
        )
    })
}

fn run_custom_command_in_repo(
    repo_name: &str,
    repo_dir: &Path,
    cmd: &[String],
) -> anyhow::Result<()> {
    if cmd.is_empty() {
        return Ok(());
    }

    let program = &cmd[0];
    let args = &cmd[1..];
    info!(
        "each: repo '{}' -> `{}` in {}",
        repo_name,
        cmd.join(" "),
        repo_dir.display()
    );

    let mut command = Command::new(program);
    command.args(args).current_dir(repo_dir);
    let cmd_str = get_cmd_str(&command);
    let status = command.status().with_loc_context(|| {
        format!(
            "failed to spawn command for repo '{}':\n  {}",
            repo_name, cmd_str
        )
    })?;
    if !status.success() {
        return err_loc!(
            "command failed for repo '{}' (exit {:?}):\n  {}",
            repo_name,
            status.code(),
            cmd_str
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{run_or_prompt_continue_with_confirm, EachRepoExecutionError};
    use anyhow::bail;

    #[test]
    fn run_or_prompt_continue_returns_ok_without_recording_when_op_succeeds() {
        let mut deferred_errors = Vec::new();

        run_or_prompt_continue_with_confirm(
            &mut deferred_errors,
            "repo_a",
            "test action",
            || Ok(()),
            |_| panic!("confirmation should not be requested"),
        )
        .expect("successful op should return ok");

        assert!(deferred_errors.is_empty());
    }

    #[test]
    fn run_or_prompt_continue_records_error_when_user_continues() {
        let mut deferred_errors = Vec::new();

        run_or_prompt_continue_with_confirm(
            &mut deferred_errors,
            "repo_a",
            "test action",
            || bail!("op failed"),
            |_| Ok(true),
        )
        .expect("continued error should return ok");

        assert_eq!(deferred_errors.len(), 1);
        assert!(format!("{:#}", deferred_errors[0]).contains("op failed"));
    }

    #[test]
    fn interrupted_execution_error_keeps_previous_deferred_errors() {
        let mut deferred_errors = vec![anyhow::anyhow!(
            "repo_before previous action: previous failed"
        )];

        let err = run_or_prompt_continue_with_confirm(
            &mut deferred_errors,
            "repo_a",
            "test action",
            || bail!("op failed"),
            |_| Ok(false),
        )
        .expect_err("stop response should interrupt");

        let err = EachRepoExecutionError::new(deferred_errors, Some(err));

        let msg = err.to_string();
        assert!(msg.contains("repo_before"));
        assert!(msg.contains("previous action"));
        assert!(msg.contains("previous failed"));
        assert!(msg.contains("op failed"));
    }

    #[test]
    fn deferred_errors_display_contains_all_recorded_failures() {
        let err = EachRepoExecutionError::new(
            vec![
                anyhow::anyhow!("repo_a first action: first failed"),
                anyhow::anyhow!("repo_b second action: second failed"),
            ],
            None,
        );

        let msg = err.to_string();
        assert!(msg.contains("2 deferred error(s)"));
        assert!(msg.contains("repo_a"));
        assert!(msg.contains("first action"));
        assert!(msg.contains("first failed"));
        assert!(msg.contains("repo_b"));
        assert!(msg.contains("second action"));
        assert!(msg.contains("second failed"));
    }
}
