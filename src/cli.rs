use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// 根级 CLI：扁平化共享参数 + 可选子命令（无子命令时行为与旧版完全一致）
#[derive(Parser, Clone, Debug)]
#[command(name = "multi_repo_debug_tool", version)]
#[command(
    about = "Multi-repository debug tool for managing complex builds",
    long_about = None,
    after_help = "Author: liuxin"
)]
pub struct Cli {
    #[command(flatten)]
    pub args: RunArgs,

    /// 生成 shell 补全脚本到 stdout（供 powershell-completion/rust-completions.ps1 调用）
    #[arg(long = "generate-completions", value_name = "SHELL", value_enum, hide = true)]
    pub generate_completions: Option<clap_complete::Shell>,

    #[command(subcommand)]
    pub command: Option<RepoCommand>,
}

/// 子命令：低频配置管理与按仓批处理
#[derive(Subcommand, Clone, Debug)]
pub enum RepoCommand {
    /// 在每个参与构建的仓库根目录下执行同一条外部命令（顺序与 Conan/CMake 遍历一致）
    ///
    /// 示例：`multi_repo_debug_tool -c my.json each -- git status -sb`
    /// （`--` 用于把带 `-` 的参数交给子进程，避免被本工具解析）
    Each(EachCommand),
    /// 配置管理（调试仓库集合与构建类型）
    Config(ConfigCommand),
}

#[derive(Args, Clone, Default, Debug)]
pub struct EachCommand {
    /// 切换并同步指定远端分支（<remote>/<branch>，对每个仓库执行）
    #[arg(long = "switch-remote", value_name = "REMOTE/BRANCH")]
    pub switch_remote: Option<String>,

    /// 目标分支被其他 worktree 占用时，切换该 worktree 到 detached HEAD
    #[arg(short = 'f', long = "force", requires = "switch_remote")]
    pub force: bool,

    /// 仅执行这些路径对应的仓库（白名单，和 --except 互斥）
    #[arg(
        long = "only",
        value_name = "PATH",
        num_args = 1..,
        conflicts_with = "except"
    )]
    pub only: Vec<PathBuf>,

    /// 跳过这些路径对应的仓库（黑名单，和 --only 互斥）
    #[arg(
        long = "except",
        value_name = "PATH",
        num_args = 1..,
        conflicts_with = "only"
    )]
    pub except: Vec<PathBuf>,

    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..,
        help = "Executable and arguments (use -- before flags, e.g. each -- git pull)"
    )]
    pub cmd: Vec<String>,
}

#[derive(Args, Clone, Default, Debug)]
pub struct ConfigCommand {
    /// 构建类型（写入配置文件）
    #[arg(long = "build-type", value_name = "TYPE")]
    pub build_type: Option<String>,

    /// 添加依赖到调试列表（可与 `--remove` 同一次命令使用）
    #[arg(long = "add", value_name = "PATH", num_args = 1..)]
    pub add: Vec<PathBuf>,

    /// 从调试列表移除依赖（可与 `--add` 同一次命令使用）
    #[arg(long = "remove", value_name = "PATH", num_args = 1..)]
    pub remove: Vec<PathBuf>,
}

/// 高频主流程参数（无子命令时默认执行）
#[derive(Args, Clone, Default, Debug)]
pub struct RunArgs {
    /// 参数文件路径，默认为 multi_repo_debug_param.json
    #[arg(
        short = 'c',
        long = "config-file",
        help = "Path to configuration file (default: multi_repo_debug_param.json)"
    )]
    pub config_file: Option<PathBuf>,

    #[arg(
        long = "check",
        help = "Validate topology and print repository branch info only (skip merge/build steps)"
    )]
    pub check_only: bool,

    /// 是否进入分支合并流程
    #[arg(long = "merge", help = "Enable branch merge process")]
    pub enable_merge: bool,

    #[arg(long = "conan", help = "Execute Conan install step")]
    pub conan: bool,
    #[arg(long = "conan-update", help = "Execute Conan install with --update")]
    pub conan_update: bool,

    /// 执行CMake配置，可选指定一个或多个仓库路径
    #[arg(
        long = "cmake",
        help = "Execute CMake configure, optionally specify repository paths",
        num_args = 0..,
        value_name = "PATH"
    )]
    pub cmake: Option<Vec<String>>,

    #[arg(
        long = "cmake-fresh",
        help = "Execute CMake configure with --fresh flag, optionally specify repository paths",
        num_args = 0..,
        value_name = "PATH"
    )]
    pub cmake_fresh: Option<Vec<String>>,

    #[arg(
        long = "continue",
        help = "Continue from the last interrupted operation"
    )]
    pub is_continue: bool,

    #[arg(long = "verbose", help = "Enable detailed logging output")]
    pub verbose: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_help_identifies_as_clap_error() {
        let result = Cli::try_parse_from(&["multi_repo_debug_tool", "-h"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(err.to_string().contains("Author: liuxin"));
    }

    #[test]
    fn test_version_identifies_as_clap_error() {
        let result = Cli::try_parse_from(&["multi_repo_debug_tool", "-V"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn test_invalid_arg_identifies_as_clap_error() {
        let result = Cli::try_parse_from(&["multi_repo_debug_tool", "--unknown-flag"]);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn test_normal_parse() {
        let result = Cli::try_parse_from(&["multi_repo_debug_tool", "--verbose"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert!(cli.args.verbose);
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_each_subcommand_parse() {
        let cli =
            Cli::try_parse_from(&["multi_repo_debug_tool", "each", "--", "git", "status"]).unwrap();
        match cli.command {
            Some(RepoCommand::Each(EachCommand {
                switch_remote,
                force,
                only,
                except,
                cmd,
            })) => {
                assert!(switch_remote.is_none());
                assert!(!force);
                assert!(only.is_empty());
                assert!(except.is_empty());
                assert_eq!(cmd, vec!["git", "status"]);
            }
            _ => panic!("expected Each subcommand"),
        }
    }

    #[test]
    fn test_each_switch_remote_only_parse() {
        let cli = Cli::try_parse_from(&[
            "multi_repo_debug_tool",
            "each",
            "--switch-remote",
            "origin/feature/test",
            "-f",
        ])
        .unwrap();
        match cli.command {
            Some(RepoCommand::Each(EachCommand {
                switch_remote,
                force,
                only,
                except,
                cmd,
            })) => {
                assert_eq!(switch_remote, Some("origin/feature/test".to_string()));
                assert!(force);
                assert!(only.is_empty());
                assert!(except.is_empty());
                assert!(cmd.is_empty());
            }
            _ => panic!("expected each --switch-remote"),
        }
    }

    #[test]
    fn test_each_switch_remote_and_cmd_parse() {
        let cli = Cli::try_parse_from(&[
            "multi_repo_debug_tool",
            "each",
            "--switch-remote",
            "origin/feature/test",
            "--",
            "git",
            "status",
        ])
        .unwrap();
        match cli.command {
            Some(RepoCommand::Each(EachCommand {
                switch_remote,
                force,
                only,
                except,
                cmd,
            })) => {
                assert_eq!(switch_remote, Some("origin/feature/test".to_string()));
                assert!(!force);
                assert!(only.is_empty());
                assert!(except.is_empty());
                assert_eq!(cmd, vec!["git", "status"]);
            }
            _ => panic!("expected each with switch-remote and cmd"),
        }
    }

    #[test]
    fn test_each_only_parse() {
        let cli = Cli::try_parse_from(&[
            "multi_repo_debug_tool",
            "each",
            "--only",
            "D:\\repo_a",
            "D:\\repo_b",
            "--",
            "git",
            "status",
        ])
        .unwrap();
        match cli.command {
            Some(RepoCommand::Each(EachCommand {
                only, except, cmd, ..
            })) => {
                assert_eq!(
                    only,
                    vec![PathBuf::from("D:\\repo_a"), PathBuf::from("D:\\repo_b")]
                );
                assert!(except.is_empty());
                assert_eq!(cmd, vec!["git", "status"]);
            }
            _ => panic!("expected each --only"),
        }
    }

    #[test]
    fn test_each_except_parse() {
        let cli = Cli::try_parse_from(&[
            "multi_repo_debug_tool",
            "each",
            "--except",
            "D:\\repo_c",
            "--",
            "git",
            "status",
        ])
        .unwrap();
        match cli.command {
            Some(RepoCommand::Each(EachCommand {
                only, except, cmd, ..
            })) => {
                assert!(only.is_empty());
                assert_eq!(except, vec![PathBuf::from("D:\\repo_c")]);
                assert_eq!(cmd, vec!["git", "status"]);
            }
            _ => panic!("expected each --except"),
        }
    }

    #[test]
    fn test_each_only_except_conflict() {
        let result = Cli::try_parse_from(&[
            "multi_repo_debug_tool",
            "each",
            "--only",
            "D:\\repo_a",
            "--except",
            "D:\\repo_b",
            "--",
            "git",
            "status",
        ]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            clap::error::ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn test_config_add_parse() {
        let cli = Cli::try_parse_from(&["multi_repo_debug_tool", "config", "--add", "D:\\repo_a"])
            .unwrap();
        match cli.command {
            Some(RepoCommand::Config(ConfigCommand {
                add,
                remove,
                build_type: None,
            })) => {
                assert_eq!(add, vec![PathBuf::from("D:\\repo_a")]);
                assert!(remove.is_empty());
            }
            _ => panic!("expected config --add"),
        }
    }

    #[test]
    fn test_config_add_and_remove_parse() {
        let cli = Cli::try_parse_from(&[
            "multi_repo_debug_tool",
            "config",
            "--add",
            "D:\\a",
            "--remove",
            "D:\\b",
        ])
        .unwrap();
        match cli.command {
            Some(RepoCommand::Config(ConfigCommand {
                add,
                remove,
                build_type: None,
            })) => {
                assert_eq!(add, vec![PathBuf::from("D:\\a")]);
                assert_eq!(remove, vec![PathBuf::from("D:\\b")]);
            }
            _ => panic!("expected config --add and --remove"),
        }
    }

    #[test]
    fn test_config_build_type_parse() {
        let cli =
            Cli::try_parse_from(&["multi_repo_debug_tool", "config", "--build-type", "Debug"])
                .unwrap();
        match cli.command {
            Some(RepoCommand::Config(ConfigCommand {
                build_type: Some(t),
                add,
                remove,
            })) => {
                assert_eq!(t, "Debug");
                assert!(add.is_empty());
                assert!(remove.is_empty());
            }
            _ => panic!("expected config --build-type"),
        }
    }

    #[test]
    fn test_legacy_root_add_flag_is_rejected() {
        let result = Cli::try_parse_from(&["multi_repo_debug_tool", "--add", "D:\\repo_a"]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            clap::error::ErrorKind::UnknownArgument
        );
    }

    #[test]
    fn test_legacy_root_config_flag_is_rejected() {
        let result = Cli::try_parse_from(&["multi_repo_debug_tool", "--config", "Debug"]);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().kind(),
            clap::error::ErrorKind::UnknownArgument
        );
    }
}
