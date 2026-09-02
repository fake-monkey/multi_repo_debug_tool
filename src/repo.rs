use crate::task::{CoreTask, TaskMeta};
use crate::{conanfile, repo_config::RepoConfig};
use diag_trace::{self as diag, anyhow_loc, err_loc, LocContextExt};
use log::{debug, info, warn};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

/// 从Git URL中解析出仓库的默认路径（仓库名称）
/// 支持的URL格式：
/// - https://github.com/user/repo.git
/// - https://github.com/user/repo
/// - git@github.com:user/repo.git
/// - git://github.com/user/repo.git
pub fn parse_repo_path_from_url(url: &str) -> Option<String> {
    let url = url.trim();

    // 移除 .git 后缀（如果存在）
    let url_without_git = url.strip_suffix(".git").unwrap_or(url);

    // 处理不同的URL格式
    let repo_name = if url_without_git.starts_with("git@") {
        // SSH格式: git@github.com:user/repo 或 git@github.com:user/path/to/repo
        url_without_git
            .split(':')
            .last()
            .and_then(|s| s.split('/').last())
    } else if url_without_git.contains("://") {
        // HTTP/HTTPS/GIT格式: https://github.com/user/repo
        url_without_git
            .split("://")
            .last()
            .and_then(|s| s.split('/').last())
            .and_then(|s| if s.is_empty() { None } else { Some(s) })
    } else if url_without_git.contains(':') {
        // 可能是简化的SSH格式
        url_without_git
            .split(':')
            .last()
            .and_then(|s| s.split('/').last())
    } else {
        // 直接是路径格式
        url_without_git.split('/').last()
    };

    repo_name.filter(|s| !s.is_empty()).map(|s| s.to_string())
}

/// 从URL自动下载仓库到指定目录，返回下载后的仓库相对路径
pub fn download_repo_from_url(url: &str, base_path: &Path) -> anyhow::Result<PathBuf> {
    // 解析仓库名称
    let repo_name = parse_repo_path_from_url(url)
        .ok_or_else(|| anyhow_loc!("无法从URL解析仓库名称: {}", url))?;
    let target_path = base_path.join(&repo_name);
    // 检查目录是否已存在
    if target_path.exists() {
        return Ok(PathBuf::from(repo_name));
    }

    let parent_path = base_path;

    // 确保父目录存在
    if !parent_path.exists() {
        fs::create_dir_all(parent_path).with_loc_context(|| "创建父目录失败")?;
    }

    // 执行 git clone
    info!("正在从 {} 下载仓库到 {} ...", url, target_path.display());

    let mut clone_cmd = Command::new("git");
    clone_cmd
        .arg("clone")
        .arg("--recurse-submodules")
        .arg(url)
        .arg(&repo_name)
        .current_dir(parent_path);

    let output = diag::command_execution::execute_and_print_output_if_debug(&mut clone_cmd)
        .with_loc_context(|| format!("git clone失败，退出码: {}", -1))?;

    if !output.status.success() {
        return err_loc!("git clone失败");
    }

    info!("成功下载仓库到: {}", target_path.display());
    Ok(PathBuf::from(repo_name))
}

/// 校验并自动检出预设仓库，并更新仓库路径
fn ensure_repo_preset(repo_cfg: &mut RepoConfig, base: &Path) -> anyhow::Result<()> {
    let rel_path = &repo_cfg.path;

    // 检查rel_path是否为空，如果不为空再检查是否存在
    if !rel_path.as_os_str().is_empty() {
        let abs_path = base.join(rel_path);
        if abs_path.exists() {
            debug!("Repo exists: {}", abs_path.display());
            return Ok(());
        }
        if !repo_cfg.auto_clone {
            return err_loc!(
                "Repository path does not exist and auto_clone is disabled: {}",
                abs_path.display()
            );
        }
    } else if !repo_cfg.auto_clone {
        return err_loc!(
            "Repository path is empty and auto_clone is disabled for url: {}",
            repo_cfg.url
        );
    }

    match download_repo_from_url(&repo_cfg.url, base) {
        Ok(rel_path) => {
            info!("Repo downloaded to: {}", rel_path.display());
            repo_cfg.path = rel_path.clone();
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// 校验并自动检出所有需要的仓库
pub fn ensure_repositories(
    repo_configs: &mut [&mut RepoConfig],
    base: &Path,
) -> anyhow::Result<()> {
    for repo_cfg in repo_configs.iter_mut() {
        ensure_repo_preset(&mut **repo_cfg, base)?;
    }

    Ok(())
}

pub fn get_identification_name(repo_path: &Path, url: &str) -> anyhow::Result<String> {
    let parsing_path = repo_path;
    match conanfile::parse_package_name_from_repo(&parsing_path) {
        Ok(name) => Ok(name),
        Err(_e) => parse_repo_path_from_url(url)
            .ok_or_else(|| anyhow_loc!("无法从URL解析仓库名称: {}", url)),
    }
}

pub struct GitMergeInput {
    pub repo_path: PathBuf,
    /// 上次同步时的本地分支
    pub last_local_branch: String,
    /// 上次使用的同步分支
    pub last_sync_branch: String,
    /// 历史同步分支（最近优先）
    pub sync_branch_history: Vec<String>,
}

pub struct GitMergeOutput {
    /// 当前本地分支
    pub current_local_branch: String,
    /// 本次实际使用的同步分支
    pub current_sync_branch: String,
}

/// 交互式选择同步分支
///
/// 如果有历史分支，使用 dialoguer 供用户通过方向键选择
/// 否则提示用户输入分支名称
fn interactive_select_sync_branch(
    repo_path: &Path,
    current_branch: &str,
    last_sync_branch: &str,
    history: &[String],
) -> anyhow::Result<String> {
    use dialoguer::{theme::ColorfulTheme, Input, Select};

    println!("当前仓库路径: {}", repo_path.display());
    println!("当前本地分支: {}", current_branch);
    println!("");

    if !history.is_empty() {
        loop {
            let items = history.to_vec();
            // 标记上次使用的分支
            let mut initial_index = 0;
            let mut display_items: Vec<String> = items
                .iter()
                .enumerate()
                .map(|(i, b)| {
                    if b == last_sync_branch {
                        initial_index = i;
                        format!("{} (上次使用)", b)
                    } else {
                        b.clone()
                    }
                })
                .collect();

            // 添加一个“输入新分支”的选项
            display_items.push("手动输入新分支名称...".to_string());

            let selection = Select::with_theme(&ColorfulTheme::default())
                .with_prompt("选择同步分支 (方向键选择, Enter确认, Esc退出)")
                .items(&display_items)
                .default(initial_index)
                .interact_opt()
                .with_loc_context(|| "交互式选择失败")?;

            match selection {
                Some(idx) => {
                    if idx < items.len() {
                        // 用户选择了一个历史分支
                        return Ok(items[idx].clone());
                    } else {
                        // 用户选择了“手动输入”
                        let input: String = Input::with_theme(&ColorfulTheme::default())
                            .with_prompt("请输入要同步的分支名称 (输入为空或直接回车可返回列表)")
                            .allow_empty(true)
                            .interact_text()
                            .with_loc_context(|| "读取输入失败")?;

                        if input.trim().is_empty() {
                            continue; // 返回循环开始，显示选择列表
                        }
                        return Ok(input.trim().to_string());
                    }
                }
                None => return err_loc!("用户取消了选择"),
            }
        }
    } else {
        // 没有历史记录，直接要求输入
        let prompt = if !last_sync_branch.is_empty() {
            format!("请输入要同步的分支名称 (默认: {})", last_sync_branch)
        } else {
            "请输入要同步的分支名称".to_string()
        };

        let input: String = if !last_sync_branch.is_empty() {
            Input::<String>::with_theme(&ColorfulTheme::default())
                .with_prompt(prompt)
                .default(last_sync_branch.to_string())
                .interact_text()
        } else {
            Input::<String>::with_theme(&ColorfulTheme::default())
                .with_prompt(prompt)
                .interact_text()
        }
        .with_loc_context(|| "读取输入失败")?;

        if input.trim().is_empty() {
            return err_loc!("用户输入为空");
        }
        Ok(input.trim().to_string())
    }
}

#[allow(dead_code)]
#[deprecated]
pub fn get_commit_hash(repo_dir: &Path) -> anyhow::Result<String> {
    let repo = git2::Repository::open(repo_dir).with_loc_context(|| "Failed to open git repo")?;
    let head = repo.head().with_loc_context(|| "Failed to get HEAD")?;
    let target = head
        .target()
        .ok_or_else(|| anyhow_loc!("HEAD has no target"))?;

    Ok(target.to_string())
}

/// 已移除函数 `check_file_path_changed` 的实现说明（用于未来可逆恢复）。
///
/// 原函数签名：
/// `pub fn check_file_path_changed(repo_dir: &Path, old_hash: &str, new_hash: &str) -> anyhow::Result<bool>`
///
/// 语义约定（保守策略）：
/// 1. 返回 `Ok(true)`：认为“文件路径集合发生变化”或“无法可靠判断，按变化处理”。
/// 2. 返回 `Ok(false)`：确认“无路径变化”。
/// 3. 不主动向上抛错；内部异常统一降级为 `Ok(true)`，并记录 `warn!` 日志。
///
/// 详细实现步骤：
/// 1. 快速路径：若 `old_hash == new_hash`，直接 `debug!` 后返回 `Ok(false)`。
/// 2. 打开仓库：`git2::Repository::open(repo_dir)`。
///    - 打开失败：`warn!`，返回 `Ok(true)`。
/// 3. 解析提交 OID：
///    - `old_oid = git2::Oid::from_str(old_hash).unwrap_or(git2::Oid::zero())`
///    - `new_oid = git2::Oid::from_str(new_hash).unwrap_or(git2::Oid::zero())`
///    - 注：解析失败时退化为零 OID，后续大概率在读取 commit/tree 阶段触发失败分支。
/// 4. 读取两侧树对象：
///    - `repo.find_commit(old_oid).and_then(|c| c.tree())`
///    - `repo.find_commit(new_oid).and_then(|c| c.tree())`
///    - 任一失败：`warn!`，返回 `Ok(true)`。
/// 5. 计算 tree-to-tree diff：
///    - `repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), None)`
///    - 失败：`warn!`，返回 `Ok(true)`。
/// 6. 遍历 `diff.deltas()` 并判定状态：
///    - 命中 `git2::Delta::Added | Deleted | Renamed` 任一即返回 `Ok(true)`。
///    - 其余状态（如 Modified/Typechange 等）忽略，不视为“路径变化”。
/// 7. 若遍历结束仍未命中，返回 `Ok(false)`。
///
/// 恢复注意点：
/// 1. 该实现判断的是“路径级变化（增删改名）”，不是“内容变化”。
/// 2. 错误处理策略是“不可判定即视为变化”，用于触发后续保守流程。
/// 3. 日志级别保持：无变化短路用 `debug!`，异常分支用 `warn!`。

fn inner_merge_branch(repo_dir: &Path, sync_branch: &str) -> anyhow::Result<()> {
    if !repo_dir.exists() {
        return Err(anyhow_loc!("Repo path does not exist: {}", repo_dir.display()).into());
    }

    info!(
        "Syncing repo '{}' to branch '{}'",
        repo_dir.display(),
        sync_branch
    );

    let remotes = git_tool::git::get_remotes(repo_dir).unwrap_or_default();
    if let Some((possible_remote, local_branch)) = sync_branch.split_once('/') {
        if remotes.contains(&possible_remote.to_string()) {
            let current_branch = git_tool::git::get_branch_name(repo_dir).unwrap_or_default();
            if current_branch == local_branch {
                let mut pull_cmd = std::process::Command::new("git");
                pull_cmd
                    .arg("pull")
                    .arg("--recurse-submodules")
                    .arg(possible_remote)
                    .arg(local_branch)
                    .current_dir(&repo_dir);

                diag::command_execution::execute_and_print_output_if_debug(&mut pull_cmd)
                    .with_loc_context(|| {
                        format!("Git pull failed for repo '{}'", repo_dir.display())
                    })?;
            } else {
                let mut fetch_cmd = std::process::Command::new("git");
                fetch_cmd
                    .arg("fetch")
                    .arg(possible_remote)
                    .arg(format!("{}:{}", local_branch, local_branch))
                    .current_dir(&repo_dir);

                diag::command_execution::execute_and_print_output_if_debug(&mut fetch_cmd)
                    .with_loc_context(|| {
                        format!("Git fetch failed for repo '{}'", repo_dir.display())
                    })?;
            }
        }
    }
    let mut merge_cmd = std::process::Command::new("git");
    merge_cmd
        .arg("merge")
        .arg(sync_branch)
        .current_dir(&repo_dir);

    diag::command_execution::execute_and_print_output_if_debug(&mut merge_cmd)
        .with_loc_context(|| format!("Git merge failed for repo '{}'", repo_dir.display()))?;

    let mut submodule_cmd = Command::new("git");
    submodule_cmd
        .arg("submodule")
        .arg("update")
        .arg("--recursive")
        .current_dir(&repo_dir);

    diag::command_execution::execute_and_print_output_if_debug(&mut submodule_cmd)
        .with_loc_context(|| {
            format!(
                "Git submodule update failed for repo '{}'",
                repo_dir.display()
            )
        })?;

    Ok(())
}

/// 智能合并分支，检测本地分支变化并支持交互式选择
///
/// 流程：
/// 1. 获取当前本地分支
/// 2. 与上次记录的本地分支对比
/// 3. 如果分支没变，使用上次的同步分支
/// 4. 如果分支变了，交互式让用户选择新的同步分支
fn smart_merge_branch(merge_input: &GitMergeInput) -> anyhow::Result<GitMergeOutput> {
    // 获取当前本地分支
    let current_branch = git_tool::git::get_branch_name(&merge_input.repo_path)?;

    debug!(
        "Repository '{}': current local branch = '{}', last local branch = '{}'",
        merge_input.repo_path.display(),
        current_branch,
        merge_input.last_local_branch
    );

    let sync_branch = if current_branch == merge_input.last_local_branch {
        // 分支没变，使用上次的同步分支
        if !merge_input.last_sync_branch.is_empty() {
            debug!(
                "  本地分支未变化，使用上次的同步分支: {}",
                merge_input.last_sync_branch
            );
            merge_input.last_sync_branch.to_string()
        } else {
            debug!("  本地分支未变化，但没有记录的同步分支，需要用户输入");
            interactive_select_sync_branch(
                &merge_input.repo_path,
                &current_branch,
                &merge_input.last_sync_branch,
                &merge_input.sync_branch_history,
            )?
        }
    } else {
        // 分支已变化，需要重新选择
        warn!(
            "  警告: 本地分支已从 '{}' 切换到 '{}'，需要重新指定同步分支",
            merge_input.last_local_branch, current_branch
        );

        // 交互式选择
        interactive_select_sync_branch(
            &merge_input.repo_path,
            &current_branch,
            &merge_input.last_sync_branch,
            &merge_input.sync_branch_history,
        )?
    };

    // 执行合并
    inner_merge_branch(&merge_input.repo_path, &sync_branch)?;

    Ok(GitMergeOutput {
        current_local_branch: current_branch,
        current_sync_branch: sync_branch,
    })
}

pub struct GitMergeTask {}

impl TaskMeta for GitMergeTask {
    fn id(&self) -> &'static str {
        "git_merge"
    }
}

impl CoreTask for GitMergeTask {
    type Input<'a> = GitMergeInput;
    type Output = GitMergeOutput;

    fn execute<'a>(&self, input: Self::Input<'a>) -> anyhow::Result<Self::Output> {
        smart_merge_branch(&input)
    }
}

#[cfg(test)]
mod tests {
    use super::ensure_repo_preset;
    use crate::repo_config::RepoConfig;
    use std::{fs, path::PathBuf};
    use uuid::Uuid;

    fn temp_test_dir(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../target")
            .join(format!("repo_debug_{}_{}", name, Uuid::new_v4()))
    }

    #[test]
    fn ensure_repo_preset_errors_when_auto_clone_disabled_and_path_empty() {
        let base_dir = temp_test_dir("auto_clone_disabled_empty");
        fs::create_dir_all(&base_dir).unwrap();
        let mut repo_cfg = RepoConfig {
            url: "https://example.com/example/repo.git".to_string(),
            auto_clone: false,
            ..Default::default()
        };

        let err = ensure_repo_preset(&mut repo_cfg, &base_dir).unwrap_err();
        assert!(err
            .to_string()
            .contains("Repository path is empty and auto_clone is disabled"));

        fs::remove_dir_all(base_dir).unwrap();
    }

    #[test]
    fn ensure_repo_preset_errors_when_auto_clone_disabled_and_path_missing() {
        let base_dir = temp_test_dir("auto_clone_disabled_missing");
        fs::create_dir_all(&base_dir).unwrap();
        let mut repo_cfg = RepoConfig {
            url: "https://example.com/example/repo.git".to_string(),
            path: PathBuf::from("missing_repo"),
            auto_clone: false,
            ..Default::default()
        };

        let err = ensure_repo_preset(&mut repo_cfg, &base_dir).unwrap_err();
        assert!(err
            .to_string()
            .contains("Repository path does not exist and auto_clone is disabled"));

        fs::remove_dir_all(base_dir).unwrap();
    }

    #[test]
    fn ensure_repo_preset_accepts_existing_path_when_auto_clone_disabled() {
        let base_dir = temp_test_dir("auto_clone_disabled_existing");
        let repo_dir = base_dir.join("existing_repo");
        fs::create_dir_all(&repo_dir).unwrap();
        let mut repo_cfg = RepoConfig {
            url: "https://example.com/example/repo.git".to_string(),
            path: PathBuf::from("existing_repo"),
            auto_clone: false,
            ..Default::default()
        };

        ensure_repo_preset(&mut repo_cfg, &base_dir).unwrap();
        assert_eq!(repo_cfg.path, PathBuf::from("existing_repo"));

        fs::remove_dir_all(base_dir).unwrap();
    }
}
