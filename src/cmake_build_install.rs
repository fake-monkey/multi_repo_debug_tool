use crate::SharedError;
use crate::{
    build_fix::{
        vcxproj_target_clean_first::VcxprojTargetCleanFirstFixer, CommandErrorFixer,
        FixActionResult, FixContext,
    },
    task::{CoreTask, TaskMeta},
};
use diag_trace::{self as diag, anyhow_loc, err_loc, LocContextExt};
use log::{debug, info};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use walkdir::WalkDir;

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum CMakeBuildParallel {
    #[default]
    Disabled,
    Auto,
    Jobs(usize),
}

impl Serialize for CMakeBuildParallel {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Disabled => serializer.serialize_bool(false),
            Self::Auto => serializer.serialize_bool(true),
            Self::Jobs(jobs) => serializer.serialize_u64(*jobs as u64),
        }
    }
}

impl<'de> Deserialize<'de> for CMakeBuildParallel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum RawCMakeBuildParallel {
            Bool(bool),
            Int(i64),
        }

        match RawCMakeBuildParallel::deserialize(deserializer)? {
            RawCMakeBuildParallel::Bool(enabled) => {
                Ok(if enabled { Self::Auto } else { Self::Disabled })
            }
            RawCMakeBuildParallel::Int(jobs) if jobs > 0 => Ok(Self::Jobs(jobs as usize)),
            RawCMakeBuildParallel::Int(jobs) => Err(serde::de::Error::custom(format!(
                "cmake build parallel value must be a positive integer, got {}",
                jobs
            ))),
        }
    }
}

fn find_cmake_config_dir(install_prefix: &Path, pkg_name: &str) -> anyhow::Result<PathBuf> {
    let pkg_lower = pkg_name.to_lowercase();
    let mut fallback: Option<std::path::PathBuf> = None;
    for entry in WalkDir::new(install_prefix)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_cmake = path
            .extension()
            .map(|e| e.to_string_lossy().eq_ignore_ascii_case("cmake"))
            .unwrap_or(false);
        if !is_cmake {
            continue;
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if name.contains(&pkg_lower) {
            let parent = path.parent().map(|p| p.to_path_buf());
            if name.contains("config") {
                return Ok(parent.unwrap_or_default());
            }
            if fallback.is_none() {
                fallback = parent;
            }
        }
    }
    fallback.ok_or_else(|| {
        anyhow_loc!(
            "No suitable CMake config directory found for package: {}",
            pkg_name
        )
    })
}

pub struct BuildInstallInput {
    pub repo_path: PathBuf,
    pub build_config: String,
    pub need_install: bool,
    pub cmake_pkg_name: String,
    pub install_prefix: PathBuf,
    pub cmake_binary_dir: PathBuf,
    pub build_parallel: CMakeBuildParallel,
    pub cmake_generator: String,
}

fn append_parallel_args(cmd: &mut std::process::Command, build_parallel: &CMakeBuildParallel) {
    match build_parallel {
        CMakeBuildParallel::Disabled => {}
        CMakeBuildParallel::Auto => {
            cmd.arg("--parallel");
        }
        CMakeBuildParallel::Jobs(jobs) => {
            cmd.arg("--parallel").arg(jobs.to_string());
        }
    }
}

pub fn load_install_manifest_txt(binary_path: &Path) -> anyhow::Result<String> {
    let manifest_path = binary_path.join("install_manifest.txt");
    if !manifest_path.exists() {
        return err_loc!(
            "install_manifest.txt not found at '{}', skipping",
            manifest_path.display()
        );
    }
    fs::read_to_string(&manifest_path).with_loc_context(|| {
        format!(
            "Failed to read install_manifest.txt from '{}'",
            manifest_path.display()
        )
    })
}

fn supplement_other_cmake_config(build_path: &Path, build_config: &str) -> anyhow::Result<()> {
    if build_config == "Release" || build_config == "Debug" {
        return Ok(());
    }
    let install_manifest = load_install_manifest_txt(build_path)?;
    let build_config_suffix = format!("-{}.cmake", build_config.to_lowercase());
    let release_config_suffix = "-release.cmake";
    for line in install_manifest.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.to_lowercase().ends_with(&build_config_suffix) {
            let src_path = Path::new(line);
            let file_name = src_path.file_name().ok_or_else(|| {
                anyhow_loc!("Failed to extract file name from '{}'", src_path.display())
            })?;
            let file_name_str = file_name.to_string_lossy();
            let base_name = &file_name_str[..file_name_str.len() - build_config_suffix.len()];
            let target_file = src_path
                .parent()
                .ok_or_else(|| {
                    anyhow_loc!("Failed to get parent directory of '{}'", src_path.display())
                })?
                .join(format!("{}{}", base_name, release_config_suffix));
            let mut context = fs::read_to_string(src_path).with_loc_context(|| {
                format!("Failed to read build config file '{}'", src_path.display())
            })?;
            context = context.replace(build_config, "Release");
            context = context.replace(&build_config.to_uppercase(), "RELEASE");
            let exist_context = if target_file.exists() {
                fs::read_to_string(&target_file).with_loc_context(|| {
                    format!(
                        "Failed to read existing release config file '{}'",
                        target_file.display()
                    )
                })?
            } else {
                String::new()
            };
            if exist_context != context {
                fs::write(&target_file, context).with_loc_context(|| {
                    format!(
                        "Failed to write release config file '{}'",
                        target_file.display()
                    )
                })?;
                debug!(
                    "Supplemented release config file '{}'",
                    target_file.display()
                );
            }
        }
    }
    Ok(())
}

pub struct BuildInstallOutput {
    pub cmake_pkg_config_dir: Option<PathBuf>,
}

fn build_with_auto_fix(
    ms_build_path: &Result<PathBuf, SharedError>,
    rel_binary_prefix: &Path,
    build_config: &str,
    repo_path: &Path,
    build_parallel: &CMakeBuildParallel,
    cmake_generator: &str,
    vs_dev_env: &crate::vs_dev_env::VsDevEnv,
) -> anyhow::Result<()> {
    let mut ctx = FixContext {
        repo_path,
        rel_binary_prefix,
        build_config,
        attempt: 0,
        max_attempts: 32,
    };
    let mut fixers: Vec<Box<dyn CommandErrorFixer>> = Vec::new();
    if cmake_generator.contains("Visual Studio") {
        fixers.push(Box::new(VcxprojTargetCleanFirstFixer::new(&ms_build_path)?));
    }
    loop {
        if ctx.attempt >= ctx.max_attempts {
            return err_loc!(
                "Auto-fix attempts reached limit ({}) for repo '{}'",
                ctx.max_attempts,
                ctx.repo_path.display()
            );
        }
        let mut build_cmd = std::process::Command::new("cmake");
        build_cmd
            .arg("--build")
            .arg(ctx.rel_binary_prefix)
            .arg("--config")
            .arg(ctx.build_config)
            .current_dir(ctx.repo_path);
        append_parallel_args(&mut build_cmd, build_parallel);
        vs_dev_env.apply_to_ninja_command(cmake_generator, &mut build_cmd)?;
        let build_result = diag::command_execution::execute_and_print_output_force(&mut build_cmd);
        match build_result {
            Ok(_) => return Ok(()),
            Err(e) => {
                let Some(source) = e.get_expected() else {
                    return Err(diag::add_loc_context(
                        e,
                        "Build failed with unexpected error",
                    ));
                };
                ctx.attempt += 1;
                let mut handled = false;
                for fixer in &mut fixers {
                    match fixer.try_fix(&source, &mut ctx) {
                        Ok(FixActionResult::Applied) => {
                            info!(
                                "Applied by fixer '{}', retry build",
                                std::any::type_name_of_val(&**fixer)
                            );
                            handled = true;
                            break;
                        }
                        Ok(FixActionResult::NotHandled) => continue,
                        Err(fix_err) => {
                            info!(
                                "Fixer '{}' returned fatal error: {}",
                                std::any::type_name_of_val(&**fixer),
                                fix_err
                            );
                            return Err(fix_err);
                        }
                    }
                }
                if !handled {
                    return Err(diag::add_loc_context(
                        e,
                        "No fixer handled this failure, return original build error",
                    ));
                }
            }
        }
    }
}

fn build_and_install(
    repo_config: &BuildInstallInput,
    ms_build_path: &Result<PathBuf, SharedError>,
    vs_dev_env: &crate::vs_dev_env::VsDevEnv,
) -> anyhow::Result<BuildInstallOutput> {
    let repo_path = &repo_config.repo_path;
    let rel_binary_prefix = pathdiff::diff_paths(&repo_config.cmake_binary_dir, &repo_path)
        .ok_or_else(|| anyhow_loc!("Failed to get relative binary dir"))?;
    build_with_auto_fix(
        ms_build_path,
        &rel_binary_prefix,
        &repo_config.build_config,
        &repo_path,
        &repo_config.build_parallel,
        &repo_config.cmake_generator,
        vs_dev_env,
    )?;

    let pkg_cmake_file_dir = if repo_config.need_install {
        let cpp_build_config = &repo_config.build_config;
        fn cmake_install(
            cpp_build_config: &str,
            work_dir: &Path,
            binary_dir: &Path,
        ) -> anyhow::Result<()> {
            let mut install_cmd = std::process::Command::new("cmake");
            install_cmd
                .arg("--install")
                .arg(&binary_dir)
                .arg("--config")
                .arg(cpp_build_config)
                .current_dir(work_dir);
            diag::command_execution::execute_and_print_output_if_debug(&mut install_cmd)
                .with_loc_context(|| {
                    format!("CMake install failed in directory '{}'", work_dir.display())
                })?;
            Ok(())
        }
        cmake_install(&cpp_build_config, &repo_path, &rel_binary_prefix)?;
        supplement_other_cmake_config(&repo_config.cmake_binary_dir, &repo_config.build_config)?;
        find_cmake_config_dir(&repo_config.install_prefix, &repo_config.cmake_pkg_name).ok()
    } else {
        None
    };
    Ok(BuildInstallOutput {
        cmake_pkg_config_dir: pkg_cmake_file_dir,
    })
}

fn get_ms_build_path() -> anyhow::Result<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles(x86)")
        .or_else(|| std::env::var_os("ProgramFiles"))
        .ok_or_else(|| {
            anyhow_loc!("Neither ProgramFiles(x86) nor ProgramFiles environment variable is set")
        })?;
    let vs_where_path = PathBuf::from(program_files)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !vs_where_path.exists() {
        return err_loc!("vswhere.exe not found at '{}'", vs_where_path.display());
    }
    let vs_where_output = std::process::Command::new(&vs_where_path)
        .args([
            "-latest",
            "-requires",
            "Microsoft.Component.MSBuild",
            "-find",
            r"MSBuild\**\Bin\MSBuild.exe",
        ])
        .output()
        .with_loc_context(|| format!("Failed to execute '{}'", vs_where_path.display()))?;
    vs_where_output
        .status
        .success()
        .then(|| ())
        .ok_or_else(|| {
            anyhow_loc!(
                "vswhere.exe failed with code {}: {}",
                vs_where_output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&vs_where_output.stderr)
            )
        })?;
    let ms_build_path_str = String::from_utf8(vs_where_output.stdout)
        .with_loc_context(|| format!("Failed to parse output of '{}'", vs_where_path.display()))?;
    let ms_build_path = PathBuf::from(ms_build_path_str.trim());
    if !ms_build_path.exists() {
        return err_loc!("MSBuild.exe not found at '{}'", ms_build_path.display());
    }
    Ok(ms_build_path)
}

pub struct BuildInstallTask {
    ms_build_path: Result<PathBuf, SharedError>,
    vs_dev_env: Rc<crate::vs_dev_env::VsDevEnv>,
}

impl BuildInstallTask {
    pub fn new(vs_dev_env: Rc<crate::vs_dev_env::VsDevEnv>) -> Self {
        Self {
            ms_build_path: get_ms_build_path()
                .map_err(anyhow::Error::into_boxed_dyn_error)
                .map_err(SharedError::from),
            vs_dev_env,
        }
    }
}

impl TaskMeta for BuildInstallTask {
    fn id(&self) -> &'static str {
        "build_install"
    }
}

impl CoreTask for BuildInstallTask {
    type Input<'a> = BuildInstallInput;
    type Output = BuildInstallOutput;

    fn execute<'a>(&self, input: Self::Input<'a>) -> anyhow::Result<Self::Output> {
        build_and_install(&input, &self.ms_build_path, &self.vs_dev_env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_args(build_parallel: &CMakeBuildParallel) -> Vec<String> {
        let mut cmd = std::process::Command::new("cmake");
        cmd.arg("--build").arg("build").arg("--config").arg("Debug");
        append_parallel_args(&mut cmd, build_parallel);
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn build_parallel_disabled_adds_no_parallel_arg() {
        assert_eq!(
            collect_args(&CMakeBuildParallel::Disabled),
            vec!["--build", "build", "--config", "Debug"]
        );
    }

    #[test]
    fn build_parallel_auto_adds_parallel_without_jobs() {
        assert_eq!(
            collect_args(&CMakeBuildParallel::Auto),
            vec!["--build", "build", "--config", "Debug", "--parallel"]
        );
    }

    #[test]
    fn build_parallel_jobs_adds_parallel_with_jobs() {
        assert_eq!(
            collect_args(&CMakeBuildParallel::Jobs(4)),
            vec!["--build", "build", "--config", "Debug", "--parallel", "4"]
        );
    }
}
