mod access_anyhow;
mod build_fix;
mod cli;
mod cmake_build_install;
mod cmake_configure;
mod cmake_presets;
mod compile_commands;
mod conan_executor;
mod conanfile;
mod config;
mod dll_copy_util;
mod each_repo_command;
mod repo;
mod repo_config;
mod sln_merge_kit;
mod task;
mod vs_dev_env;

use clap::{CommandFactory, Parser};
use colored::Colorize;
use log::{debug, error, info};
use std::error::Error as StdError;
use std::path::PathBuf;
use std::rc::Rc;

use crate::task::TaskMeta;
use cli::{Cli, ConfigCommand, RepoCommand, RunArgs};
use cmake_build_install::BuildInstallTask;
use cmake_configure::CMakeConfigureTask;
use compile_commands::CompileCommandsTask;
use conan_executor::ConanInstallTask;
use config::FileConfig;
use diag_trace::{cli_support, err_loc, LocContextExt};
use repo::GitMergeTask;

type HashType = u64;
type SharedError = std::sync::Arc<dyn StdError + Send + Sync + 'static>;

fn copy_dll(all_deps: &[String], file_config: &FileConfig) -> anyhow::Result<()> {
    if !file_config.is_visual_studio_generator(&file_config.executable.identification_name)? {
        info!("Skipping DLL copy because the executable does not use a Visual Studio generator.");
        return Ok(());
    }

    let dependencies: Vec<PathBuf> = all_deps
        .iter()
        .filter(|name| **name != file_config.executable.identification_name)
        .map(|name| file_config.get_cmake_binary_dir(name).unwrap())
        .collect();
    dll_copy_util::copy_dll(
        &file_config.get_sln_file_path(&file_config.executable.identification_name)?,
        &dependencies,
        &file_config.config,
    )?;

    Ok(())
}

fn copy_compile_commands(
    repo_names: &[String],
    file_config: &FileConfig,
) -> anyhow::Result<()> {
    for repo_name in repo_names {
        if file_config.is_visual_studio_generator(repo_name)?
            && !file_config.compile_commands_config.enabled
        {
            debug!(
                "Skipping compile_commands publish for Visual Studio repo '{}' because generation is disabled.",
                repo_name
            );
            continue;
        }

        let repo_dir = file_config.get_repo_dir(repo_name)?;
        let cmake_binary_dir = file_config.get_cmake_binary_dir(repo_name)?;
        let publish_path =
            compile_commands::publish_compile_commands(&repo_dir, &cmake_binary_dir)
                .with_loc_context(|| {
                    format!(
                        "Failed to publish compile_commands for repo '{}'",
                        repo_name
                    )
                })?;
        info!(
            "Published compile_commands for repo '{}' to '{}'",
            repo_name,
            publish_path.display()
        );
    }
    Ok(())
}

fn merge_sln(file_config: &FileConfig, need_debug_pkgs: &[String]) -> anyhow::Result<()> {
    if !file_config.is_visual_studio_generator(&file_config.executable.identification_name)? {
        info!(
            "Skipping solution merge because the executable does not use a Visual Studio generator."
        );
        return Ok(());
    }

    let target_sln = file_config.get_sln_file_path(&file_config.executable.identification_name)?;
    let mut merged_pkg_names = Vec::new();
    for name in need_debug_pkgs {
        if *name == file_config.executable.identification_name {
            continue;
        }
        if file_config.is_visual_studio_generator(name)? {
            merged_pkg_names.push(name.as_str());
        } else {
            info!(
                "Skipping repository '{}' during solution merge because it does not use a Visual Studio generator.",
                name
            );
        }
    }
    let source_sln_list = merged_pkg_names
        .iter()
        .map(|name| file_config.get_sln_file_path(name))
        .collect::<::std::result::Result<Vec<PathBuf>, anyhow::Error>>()?;
    debug!("Merged solution file at {}", target_sln.display());
    sln_merge_kit::merge_sln(&target_sln, &source_sln_list)?;
    info!("Merged packages: {}", merged_pkg_names.join(", "));
    Ok(())
}

fn batch_print_branch_names(file_config: &FileConfig, name_list: &[String]) -> anyhow::Result<()> {
    println!("Build type: {}", file_config.config);
    for name in name_list.iter() {
        let repo_dir = file_config.get_repo_dir(name)?;
        let branch_name = git_tool::git::get_branch_name(&repo_dir)?;
        println!("Repository '{}': branch '{}'", name, branch_name);
    }
    Ok(())
}

/// 打印原始配置（debug_repo_names）中实际参与调试的仓库，
/// 便于 config --remove 时有的放矢。
fn print_debug_config_participation(
    runtime_ctx: &FileConfig,
    topological_info: &conanfile::DependencyTopologicalInfo,
) {
    if runtime_ctx.debug_repo_names.is_empty() {
        return;
    }

    let in_graph: std::collections::HashSet<&str> = topological_info
        .need_debug_pkgs
        .iter()
        .map(|name| name.as_str())
        .collect();

    let participating: Vec<&str> = runtime_ctx
        .debug_repo_names
        .iter()
        .filter(|name| {
            // 既要在依赖图中，又要未禁用（enable=false 时不参与构建）
            in_graph.contains(name.as_str()) && runtime_ctx.is_dependency_enabled_in_config(name)
        })
        .map(|name| name.as_str())
        .collect();

    if !participating.is_empty() {
        println!("Participating in debug (from config): {}", participating.join(", "));
    }
}

impl_variadic_task_runner!(&mut FileConfig, &GitMergeTask,);
impl_variadic_task_runner!(&mut FileConfig, &ConanInstallTask,);
impl_variadic_task_runner!(&mut FileConfig, &CMakeConfigureTask, &BuildInstallTask,);
impl_variadic_task_runner!(&mut FileConfig, &CompileCommandsTask,);

pub(crate) fn prepare_runtime_ctx(run_args: &RunArgs) -> anyhow::Result<FileConfig> {
    let mut runtime_ctx =
        config::build_runtime_context(run_args).with_loc_context(|| "构建运行时上下文失败")?;

    runtime_ctx
        .ensure_repositories()
        .with_loc_context(|| "确保仓库目录存在失败")?;
    runtime_ctx
        .update_repo_path()
        .with_loc_context(|| "更新仓库路径信息失败")?;
    Ok(runtime_ctx)
}

pub(crate) fn build_and_print_topology(
    runtime_ctx: &FileConfig,
) -> anyhow::Result<conanfile::DependencyTopologicalInfo> {
    let mut topological_info = conanfile::build_dependency_graph_and_topological_sort(
        &runtime_ctx.executable.identification_name,
        runtime_ctx,
        &runtime_ctx.debug_repo_names,
    )?;

    // 按 enable 过滤构建列表（graph 保持完整）
    topological_info.sorted_names.retain(|name| {
        if runtime_ctx.is_dependency_enabled_in_config(name) {
            true
        } else {
            eprintln!(
                "Warning: dependency {} is disabled (enable=false) and excluded from build topology.",
                name.red().bold()
            );
            false
        }
    });

    batch_print_branch_names(runtime_ctx, &topological_info.sorted_names)?;
    print_debug_config_participation(runtime_ctx, &topological_info);
    Ok(topological_info)
}

fn run(cli_args: &RunArgs) -> anyhow::Result<()> {
    // 第二步-第八步：构建运行时上下文
    let mut runtime_ctx = prepare_runtime_ctx(cli_args)?;
    let topological_info = build_and_print_topology(&runtime_ctx)?;

    let git_merge_task = GitMergeTask {};

    if !cli_args.is_continue {
        if !cli_args.check_only {
            runtime_ctx.clear_progress_data();
        } else if cli_args.enable_merge {
            // 仅查看分支名时，不清除其他的进度数据，以便下次继续使用
            runtime_ctx.clear_specific_progress(git_merge_task.id());
        }
    }
    runtime_ctx.update_cli_param(cli_args)?;
    // 这里必须保存一次，以防后续步骤失败或跳过时，设置参数仍然可以起作用
    runtime_ctx.save_file_config()?;
    runtime_ctx.save_interrupt_cache()?;

    if runtime_ctx.enable_merge {
        task::run_all(
            &mut (&mut runtime_ctx, &git_merge_task),
            &topological_info.sorted_names,
        )?;

        let new_topological_graph = {
            let file_config = &runtime_ctx;
            // 重新拓扑排序依赖并生成构建顺序
            conanfile::build_dependency_graph_and_topological_sort(
                &runtime_ctx.executable.identification_name,
                file_config,
                &runtime_ctx.debug_repo_names,
            )?
            .graph
        };

        if !conanfile::check_graph_equal(&topological_info.graph, &new_topological_graph) {
            return err_loc!("Dependency graph changed after git merge. Please re-run the tool.");
        }
    }

    if cli_args.check_only {
        return Ok(());
    }

    let conan_install_task = ConanInstallTask {};
    task::run_all(
        &mut (&mut runtime_ctx, &conan_install_task),
        &topological_info.sorted_names,
    )?;

    let sorted_names = topological_info.sorted_names.clone();
    runtime_ctx.dependency_topological_info = topological_info;

    let vs_dev_env = Rc::new(vs_dev_env::VsDevEnv::new());
    let cmake_configure_task = CMakeConfigureTask::new(Rc::clone(&vs_dev_env));
    let build_install_task = BuildInstallTask::new(Rc::clone(&vs_dev_env));
    let cmake_build_res = task::run_all(
        &mut (&mut runtime_ctx, &cmake_configure_task, &build_install_task),
        &sorted_names,
    );

    let compile_commands_task = CompileCommandsTask::new();
    let batch_make_compile_commands_res = task::run_all(
        &mut (&mut runtime_ctx, &compile_commands_task),
        &sorted_names,
    );

    let copy_compile_commands_res = copy_compile_commands(&sorted_names, &runtime_ctx);

    // 无论构建是否成功，都合并 sln 文件，便于排查问题。
    let merge_sln_res = merge_sln(
        &runtime_ctx,
        &runtime_ctx.dependency_topological_info.sorted_names,
    );

    cmake_build_res?;
    batch_make_compile_commands_res?;
    copy_compile_commands_res?;
    merge_sln_res?;

    copy_dll(
        &runtime_ctx.dependency_topological_info.sorted_names,
        &runtime_ctx,
    )?;

    Ok(())
}

fn run_config_command(run_args: &RunArgs, config_cmd: &ConfigCommand) -> anyhow::Result<()> {
    let has_add = !config_cmd.add.is_empty();
    let has_remove = !config_cmd.remove.is_empty();
    let has_action = has_add || has_remove;
    let has_build_type = config_cmd.build_type.is_some();
    if !has_action && !has_build_type {
        return err_loc!("config command requires either --add/--remove or --build-type");
    }

    let mut runtime_ctx = prepare_runtime_ctx(run_args)?;

    if has_add {
        runtime_ctx.apply_config_add(&config_cmd.add)?;
        info!("Updated debug list: added {} repo(s)", config_cmd.add.len());
    }
    if has_remove {
        runtime_ctx.apply_config_remove(&config_cmd.remove)?;
        info!(
            "Updated debug list: removed {} repo(s)",
            config_cmd.remove.len()
        );
    }
    info!(
        "Updated debug list: {}",
        runtime_ctx.debug_repo_names.join(",")
    );
    if let Some(build_type) = &config_cmd.build_type {
        runtime_ctx.set_build_type(build_type)?;
        info!("Updated build type: {}", build_type);
    }

    runtime_ctx.save_file_config()?;
    let _topological_info = build_and_print_topology(&runtime_ctx)?;

    Ok(())
}

fn main() {
    let _ = job_guard::init_child_cleaner().inspect_err(|e| {
        // 注意：此时日志系统尚未初始化，通过 stderr 输出
        eprintln!("Failed to initialize job object: {}", e);
    });

    // 1. 解析命令行参数
    // 从命令行参数中解析，返回错误时会自动显示帮助信息
    let cli = Cli::parse();

    // 补全生成约定：向 stdout 输出脚本后退出（供 powershell-completion 加载脚本调用）
    if let Some(shell) = cli.generate_completions {
        let mut cmd = Cli::command();
        clap_complete::generate(shell, &mut cmd, "repo_debug", &mut std::io::stdout());
        return;
    }

    cli_support::init_env_logger(cli.args.verbose, "info");

    let start = std::time::Instant::now();
    let res = match &cli.command {
        None => run(&cli.args),
        Some(RepoCommand::Each(each_cmd)) => each_repo_command::run(&cli.args, each_cmd),
        Some(RepoCommand::Config(config_cmd)) => run_config_command(&cli.args, config_cmd),
    };
    let elapsed = start.elapsed();
    let total_secs = elapsed.as_secs();
    info!(
        "Total execution time: {}分钟{}秒",
        total_secs / 60,
        total_secs % 60
    );

    match res {
        Ok(_) => {
            info!("Execution completed successfully.");
        }
        Err(e) => {
            error!("\n{e:?}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn compile_commands_publish_skips_disabled_visual_studio_repo() {
        let mut file_config = FileConfig::default();
        file_config.compile_commands_config.enabled = false;
        let mut repo_cache = config::InterruptRepoCache::default();
        repo_cache.repo_dir = PathBuf::from("missing_repo");
        repo_cache.cmake_binary_dir = PathBuf::from("missing_build");
        repo_cache.cmake_generator = "Visual Studio 17 2022".to_string();
        file_config
            .interrupt_cache
            .interrupt_repo_cache
            .insert("repo".to_string(), RefCell::new(repo_cache));

        copy_compile_commands(&["repo".to_string()], &file_config).unwrap();
    }
}
