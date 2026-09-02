mod task_containers;

use crate::cli::RunArgs;
use crate::cmake_build_install::CMakeBuildParallel;
use crate::cmake_configure::{CMakeConfigureInfo, CMakeConfigureOutput, CMakeDependencyInfo};
use crate::compile_commands::CompileCommandsConfig;
use crate::conanfile::{DependencyQueryInput, DependencyTopologicalInfo};
use crate::repo::{self};
use crate::repo_config::RepoConfig;
use crate::HashType;
use diag_trace::{anyhow_loc, err_loc, LocContextExt};
use log::{debug, info};
use path_absolutize::Absolutize;
use serde::{Deserialize, Serialize};
use serde_json;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

fn default_cmake_generator() -> String {
    "Visual Studio".to_string()
}

#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
pub struct InterruptRepoCache {
    #[serde(skip)]
    pub repo_dir: PathBuf,

    /// 由于某些cmake过程会修改conan_toolchain.cmake文件，
    /// 导致直接读取文件生成的hash总是会触发cmake fresh，
    /// 所以这里统一以conan install任务执行的结果记为conan_toolchain.cmake文件的hash值。
    conan_toolchain_hash: HashType,

    pub conan_toolchain_path: PathBuf,

    /// 是否执行 cmake，其实不是中断数据，而是 cli 参数解析结果
    #[serde(skip)]
    pub need_cmake: bool,
    /// 执行 cmake 时是否 fresh，其实不是中断数据，而是 cli 参数解析结果
    #[serde(skip)]
    pub need_cmake_fresh: bool,

    pub cmake_pkg_name: String,

    /// 可直接访问的cmake二进制目录
    pub cmake_binary_dir: PathBuf,

    /// 当前仓库最后一次 CMake configure 实际选中的 generator。
    /// 旧缓存没有该字段时按历史行为视为 Visual Studio。
    #[serde(default = "default_cmake_generator")]
    pub cmake_generator: String,

    /// 可直接访问的cmake install目录
    pub install_prefix: PathBuf,

    /// 可直接访问的cmake包配置文件所在目录
    pub cmake_pkg_config_dir: PathBuf,

    /// Git合并分支信息
    pub last_local_branch: String, // 上次同步时的本地分支
    pub last_sync_branch: String, // 上次使用的同步分支

    pub task_fingerprints: HashMap<String, serde_json::Value>,
}

impl InterruptRepoCache {
    fn get_abs_path(&self, relative_path: &PathBuf) -> PathBuf {
        self.repo_dir.join(relative_path)
    }

    fn get_rel_path(&self, abs_path: &PathBuf) -> anyhow::Result<PathBuf> {
        if abs_path.is_absolute() {
            let repo_dir = self
                .repo_dir
                .absolutize()
                .with_loc_context(|| {
                    format!("Failed to absolutize repo dir {}", self.repo_dir.display())
                })?
                .to_path_buf();
            pathdiff::diff_paths(&abs_path, &repo_dir).ok_or_else(|| {
                anyhow_loc!("Failed to get relative path for: {}", abs_path.display())
            })
        } else {
            pathdiff::diff_paths(abs_path, &self.repo_dir).ok_or_else(|| {
                anyhow_loc!("Failed to get relative path for: {}", abs_path.display())
            })
        }
    }
}

#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
pub struct InterruptCache {
    pub interrupt_repo_cache: HashMap<String, RefCell<InterruptRepoCache>>,
    // 第一层是任务，第二层是仓库名称
    pub completed_tasks: HashMap<String, HashSet<String>>,
    /// 全局历史同步分支（不区分仓库，最近优先）
    pub sync_branch_history: RefCell<Vec<String>>,
}

/// 文件参数 - 需要持久化到JSON文件中
#[derive(Deserialize, Serialize, Default)]
pub struct FileConfig {
    /// 工作目录（参数文件所在目录）
    #[serde(skip)]
    pub work_dir: PathBuf,
    #[serde(skip)]
    pub self_file_path: PathBuf,

    /// 可执行程序仓库信息
    pub executable: RepoConfig,
    /// 可能的依赖仓库列表 - 使用 RefCell 支持混合借用
    #[serde(default)]
    pub possible_dependencies: Vec<RepoConfig>,
    /// 需要调试的仓库conan包名称
    #[serde(default)]
    pub debug_repo_names: Vec<String>,
    /// CMake配置类型 (Debug/Release/RelWithDebInfo等)
    #[serde(default)]
    pub config: String,
    #[serde(default)]
    pub common_conan_options: Vec<String>,

    #[serde(default)]
    pub cmake_build_parallel: CMakeBuildParallel,

    #[serde(skip)]
    pub enable_merge: bool,
    #[serde(skip)]
    pub need_conan_install: bool,
    #[serde(skip)]
    pub need_conan_install_update: bool,

    #[serde(default)]
    pub compile_commands_config: CompileCommandsConfig,

    #[serde(skip)]
    pub interrupt_cache: InterruptCache,

    #[serde(skip)]
    pub dependency_topological_info: DependencyTopologicalInfo,
}

/// 加载或创建文件参数
fn load_or_create_file_config(
    config_file_path: &PathBuf,
    is_auto_create: bool,
) -> anyhow::Result<FileConfig> {
    if !config_file_path.exists() {
        if !is_auto_create {
            return err_loc!("Config file not found: {}", config_file_path.display());
        }
        let default_config = FileConfig::default();
        let json_content = serde_json::to_string_pretty(&default_config)
            .with_loc_context(|| "Failed to serialize default config")?;

        fs::write(&config_file_path, json_content)
            .with_loc_context(|| "Failed to write config file")?;

        info!(
            "Created default config file: {}",
            config_file_path.display()
        );
        info!("Please edit the configuration file and run again with:");
        info!("  multi_repo_debug_tool -c {}", config_file_path.display());

        return err_loc!(
            "Created default config at {}. Please edit the file and run: multi_repo_debug_tool -c {}",
            config_file_path.display(),
            config_file_path.display()
        );
    }

    let config_content =
        fs::read_to_string(&config_file_path).with_loc_context(|| "Failed to read config file")?;

    let file_config: FileConfig = serde_json::from_str(&config_content)
        .with_loc_context(|| "Failed to deserialize config")?;

    file_config.validate()?;

    Ok(file_config)
}

fn derive_interrupt_cache_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("multi_repo_debug_param");
    let cache_name = format!("interrupt-cache.{}.json", stem);
    config_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(cache_name)
}

fn load_interrupt_cache(cache_file_path: &Path) -> anyhow::Result<InterruptCache> {
    if !cache_file_path.exists() {
        return Ok(InterruptCache::default());
    }

    let cache_content = fs::read_to_string(cache_file_path).with_loc_context(|| {
        format!(
            "Failed to read interrupt cache {}",
            cache_file_path.display()
        )
    })?;
    let cache: InterruptCache = serde_json::from_str(&cache_content).with_loc_context(|| {
        format!(
            "Failed to deserialize interrupt cache {}",
            cache_file_path.display()
        )
    })?;
    Ok(cache)
}

fn convert_path_dir_to_conan_name(
    full_path_map: &HashMap<PathBuf, String>,
    path: &Path,
) -> anyhow::Result<String> {
    let path = path
        .absolutize()
        .with_loc_context(|| format!("Failed to absolutize path {}", path.display()))
        .map(|p| p.to_path_buf())?;
    full_path_map.get(&path).cloned().ok_or_else(|| {
        anyhow_loc!(
            "No identification name found for directory: {}",
            path.display()
        )
    })
}

impl FileConfig {
    pub(crate) fn build_full_path_map(&self) -> HashMap<PathBuf, String> {
        self.interrupt_cache
            .interrupt_repo_cache
            .iter()
            .map(|(name, cache_cell)| {
                let cache = cache_cell.borrow();
                let full_path = cache
                    .repo_dir
                    .absolutize()
                    .with_loc_context(|| {
                        format!("Failed to absolutize repo dir {}", cache.repo_dir.display())
                    })
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|_| cache.repo_dir.clone());
                (full_path, name.clone())
            })
            .collect::<HashMap<PathBuf, String>>()
    }

    pub fn apply_config_add(&mut self, add_deps: &[PathBuf]) -> anyhow::Result<()> {
        let full_path_map = self.build_full_path_map();
        for dep in add_deps {
            let dep_conan_name = convert_path_dir_to_conan_name(&full_path_map, dep)?;
            if !self.debug_repo_names.contains(&dep_conan_name) {
                self.debug_repo_names.push(dep_conan_name);
            }
        }
        Ok(())
    }

    pub fn apply_config_remove(&mut self, remove_deps: &[PathBuf]) -> anyhow::Result<()> {
        let full_path_map = self.build_full_path_map();
        for dep in remove_deps {
            let dep_conan_name = convert_path_dir_to_conan_name(&full_path_map, dep)?;
            self.debug_repo_names.retain(|name| name != &dep_conan_name);
        }
        Ok(())
    }

    pub fn set_build_type(&mut self, build_type: &str) -> anyhow::Result<()> {
        if self.config.is_empty() {
            self.config = "RelWithDebInfo".to_string();
        }
        if !["Debug", "Release", "RelWithDebInfo", "MinSizeRel"].contains(&build_type) {
            return err_loc!(
                "Invalid config type: {}. Must be one of: Debug, Release, RelWithDebInfo, MinSizeRel",
                build_type
            );
        }
        self.config = build_type.to_string();
        Ok(())
    }

    /// 生成默认配置
    pub fn default() -> Self {
        Self {
            executable: RepoConfig::empty(),
            possible_dependencies: Vec::new(),
            debug_repo_names: Vec::new(),
            config: "RelWithDebInfo".to_string(),
            ..Default::default()
        }
    }

    /// 检查指定依赖是否启用（enable=true）。
    /// 用于在拓扑排序结果、CMake 依赖收集等消费方按 enable 过滤。
    /// 不在 possible_dependencies 中的名字（如 executable）默认视为启用。
    pub(crate) fn is_dependency_enabled_in_config(&self, name: &str) -> bool {
        self.possible_dependencies
            .iter()
            .find(|dep| dep.identification_name == name)
            .map(|dep| dep.enable)
            .unwrap_or(true)
    }

    /// 检验文件配置是否有效
    pub fn validate(&self) -> anyhow::Result<()> {
        if !["Debug", "Release", "RelWithDebInfo", "MinSizeRel", ""].contains(&self.config.as_str())
        {
            return err_loc!(
                "Invalid config type: {}. Must be one of: Debug, Release, RelWithDebInfo, MinSizeRel",
                self.config
            );
        }

        if matches!(self.cmake_build_parallel, CMakeBuildParallel::Jobs(0)) {
            return err_loc!(
                "cmake_build_parallel must be a positive integer when set to a number"
            );
        }

        Ok(())
    }

    fn get_work_dir(&self) -> &Path {
        &self.work_dir
    }

    pub fn ensure_repositories(&mut self) -> anyhow::Result<()> {
        let work_dir = self.get_work_dir().to_path_buf();
        let mut repo_configs: Vec<&mut RepoConfig> =
            Vec::with_capacity(1 + self.possible_dependencies.len());
        repo_configs.push(&mut self.executable);
        repo_configs.extend(self.possible_dependencies.iter_mut().filter(|d| d.enable));

        repo::ensure_repositories(&mut repo_configs, &work_dir)
    }

    pub fn update_repo_path(&mut self) -> anyhow::Result<()> {
        let work_dir = self.get_work_dir().to_path_buf();

        let interrupt_cache = &mut self.interrupt_cache;
        let repo_cache = &mut interrupt_cache.interrupt_repo_cache;

        let mut write_name_path_info = |repo_cfg: &mut RepoConfig| -> anyhow::Result<()> {
            let identification_name =
                repo::get_identification_name(&work_dir.join(&repo_cfg.path), &repo_cfg.url)?;
            let repo_entry = repo_cache
                .entry(identification_name.clone())
                .or_insert_with(|| RefCell::new(InterruptRepoCache::default()));
            repo_entry.borrow_mut().repo_dir = work_dir.join(&repo_cfg.path);
            repo_cfg.identification_name = identification_name;
            Ok(())
        };

        write_name_path_info(&mut self.executable)?;

        for repo_config in self.possible_dependencies.iter_mut() {
            write_name_path_info(repo_config)?;
        }
        Ok(())
    }

    /// 更新调试仓库列表
    pub fn update_cli_param(&mut self, cli_args: &RunArgs) -> anyhow::Result<()> {
        let full_path_map = self.build_full_path_map();
        {
            self.enable_merge = cli_args.enable_merge;
            self.need_conan_install = cli_args.conan || cli_args.conan_update;
            self.need_conan_install_update = cli_args.conan_update;
        }

        let need_cmake_pkg = cli_args
            .cmake
            .as_ref()
            .map(|vec| {
                vec.iter()
                    .map(|path| {
                        convert_path_dir_to_conan_name(&full_path_map, &PathBuf::from(path))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;
        let need_cmake_fresh_pkg = cli_args
            .cmake_fresh
            .as_ref()
            .map(|vec| {
                vec.iter()
                    .map(|path| {
                        convert_path_dir_to_conan_name(&full_path_map, &PathBuf::from(path))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?;

        for (pkg_name, repo_cache_cell) in &mut self.interrupt_cache.interrupt_repo_cache {
            let mut repo_cache = repo_cache_cell.borrow_mut();
            let need_cmake = need_cmake_pkg
                .as_ref()
                .map_or(false, |pkgs| pkgs.is_empty() || pkgs.contains(pkg_name));
            let need_cmake_fresh = need_cmake_fresh_pkg
                .as_ref()
                .map_or(false, |pkgs| pkgs.is_empty() || pkgs.contains(pkg_name));
            // 直接覆盖，不再追加
            repo_cache.need_cmake = need_cmake || need_cmake_fresh;
            repo_cache.need_cmake_fresh = need_cmake_fresh;
        }

        Ok(())
    }

    /// 保存文件配置到JSON
    pub fn save_file_config(&self) -> anyhow::Result<()> {
        let config_file_path = &self.self_file_path;
        let json_content = serde_json::to_string_pretty(&self)
            .with_loc_context(|| "Failed to serialize config")?;

        std::fs::write(&config_file_path, json_content)
            .with_loc_context(|| "Failed to write config file")?;

        debug!("Saved config file: {}", config_file_path.display());
        Ok(())
    }

    pub fn save_interrupt_cache(&self) -> anyhow::Result<()> {
        let cache_file_path = derive_interrupt_cache_path(&self.self_file_path);
        let json_content = serde_json::to_string_pretty(&self.interrupt_cache)
            .with_loc_context(|| "Failed to serialize interrupt cache")?;

        std::fs::write(&cache_file_path, json_content)
            .with_loc_context(|| "Failed to write interrupt cache file")?;

        debug!("Saved interrupt cache file: {}", cache_file_path.display());
        Ok(())
    }
}

impl FileConfig {
    pub fn get_interrupt_repo_cache(
        &self,
        name: &str,
    ) -> anyhow::Result<&RefCell<InterruptRepoCache>> {
        self.interrupt_cache
            .interrupt_repo_cache
            .get(name)
            .ok_or_else(|| anyhow_loc!("Dependency not found: {}", name))
    }

    fn get_repo_config(&self, name: &str) -> anyhow::Result<&RepoConfig> {
        match self
            .possible_dependencies
            .iter()
            .find(|cfg| cfg.identification_name == name)
            .ok_or_else(|| anyhow_loc!("RepoConfig not found for: {}", name))
        {
            Ok(cfg) => Ok(cfg),
            Err(_) => {
                if self.executable.identification_name == name {
                    Ok(&self.executable)
                } else {
                    err_loc!("RepoConfig not found for: {}", name)
                }
            }
        }
    }

    pub fn get_cmake_executor_info(&self, name: &str) -> anyhow::Result<CMakeConfigureInfo> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let repo_dir = &cache.borrow().repo_dir;

        let repo_config = self.get_repo_config(name)?;

        let cache_borrow = cache.borrow();
        // 如果 Conan 重新安装且 toolchain 内容变化，需要强制 cmake fresh
        // 这里用「新 hash」对比「上次缓存的 hash」来判断是否变化
        let need_cmake_fresh = cache_borrow.need_cmake_fresh;

        let conan_toolchain_path = if repo_config.conan_toolchain_managed_by_cmake {
            None
        } else {
            let conan_toolchain_path =
                cache_borrow.get_abs_path(&cache_borrow.conan_toolchain_path);
            if conan_toolchain_path.as_os_str().is_empty() {
                return err_loc!(
                    "Conan toolchain path is empty while external toolchain injection is enabled"
                );
            }
            Some(conan_toolchain_path)
        };

        let extra_cmake_options = repo_config.extra_cmake_options.clone();

        Ok(CMakeConfigureInfo {
            repo_path: repo_dir.clone(),
            build_config: self.config.clone(),
            need_cmake: cache_borrow.need_cmake || need_cmake_fresh,
            need_cmake_fresh,
            conan_toolchain_path,
            conan_toolchain_hash: cache_borrow.conan_toolchain_hash,
            extra_cmake_options,
            cmake_generator_keyword: repo_config.cmake_generator_keyword.clone(),
        })
    }

    pub fn get_cmake_dependency_info(&self, name: &str) -> anyhow::Result<CMakeDependencyInfo> {
        let cache = self.get_interrupt_repo_cache(name)?;

        Ok(CMakeDependencyInfo {
            cmake_pkg_name: cache.borrow().cmake_pkg_name.clone(),
            cmake_pkg_config_dir: cache
                .borrow()
                .get_abs_path(&cache.borrow().cmake_pkg_config_dir),
        })
    }

    pub fn set_cmake_executor_output(
        &self,
        name: &str,
        output: CMakeConfigureOutput,
    ) -> anyhow::Result<()> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let mut repo_cache = cache.borrow_mut();
        repo_cache.cmake_binary_dir = repo_cache.get_rel_path(&output.cmake_binary_dir)?;
        repo_cache.install_prefix = repo_cache.get_rel_path(&output.install_prefix)?;
        repo_cache.cmake_pkg_name = output.cmake_pkg_name;
        repo_cache.cmake_generator = output.cmake_generator;
        Ok(())
    }

    pub fn get_cmake_binary_dir(&self, name: &str) -> anyhow::Result<PathBuf> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let repo_cache = cache.borrow();
        Ok(repo_cache.get_abs_path(&repo_cache.cmake_binary_dir))
    }

    pub fn get_sln_file_path(&self, name: &str) -> anyhow::Result<PathBuf> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let repo_cache = cache.borrow();
        if repo_cache.cmake_pkg_name.is_empty() {
            return err_loc!("CMake package name is empty for repo: {}", name);
        }
        let sln_path = repo_cache
            .get_abs_path(&repo_cache.cmake_binary_dir)
            .join(format!("{}.sln", repo_cache.cmake_pkg_name));
        Ok(sln_path)
    }

    pub fn get_repo_dir(&self, name: &str) -> anyhow::Result<PathBuf> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let repo_cache = cache.borrow();
        Ok(repo_cache.repo_dir.clone())
    }

    pub fn is_visual_studio_generator(&self, name: &str) -> anyhow::Result<bool> {
        let cache = self.get_interrupt_repo_cache(name)?;
        Ok(cache.borrow().cmake_generator.contains("Visual Studio"))
    }

    pub fn clear_progress_data(&mut self) {
        let interrupt_cache = &mut self.interrupt_cache;
        interrupt_cache.completed_tasks.clear();
    }

    pub fn clear_specific_progress(&mut self, task_id: &str) {
        let interrupt_cache = &mut self.interrupt_cache;
        interrupt_cache.completed_tasks.remove(task_id);
    }
}

/// 根据 CLI 参数构建运行时上下文，并在需要时更新/保存配置
pub fn build_runtime_context(cli_args: &RunArgs) -> anyhow::Result<FileConfig> {
    let config_file_path = cli_args
        .config_file
        .clone()
        .unwrap_or_else(|| PathBuf::from("multi_repo_debug_param.json"));

    let work_dir = match config_file_path.parent() {
        None => PathBuf::from("."),
        Some(p) => {
            let abs = p
                .absolutize()
                .with_loc_context(|| format!("Failed to absolutize path {}", p.display()))?
                .to_path_buf();
            abs
        }
    };

    let mut file_config =
        load_or_create_file_config(&config_file_path, cli_args.config_file.is_none())?;
    let interrupt_cache_file_path = derive_interrupt_cache_path(&config_file_path);
    file_config.interrupt_cache = load_interrupt_cache(&interrupt_cache_file_path)?;
    file_config.work_dir = work_dir;
    file_config.self_file_path = config_file_path;

    Ok(file_config)
}

impl DependencyQueryInput for FileConfig {
    fn get_conanfile_content_by_conan_name(&self, conan_name: &str) -> anyhow::Result<String> {
        let cell = self.get_interrupt_repo_cache(conan_name)?;

        let repo_dir = &cell.borrow().repo_dir;
        crate::conanfile::get_conanfile_content(repo_dir)
    }

    fn get_all_dependencies(&self) -> Vec<String> {
        let mut res: Vec<String> = self
            .possible_dependencies
            .iter()
            .map(|dep| dep.identification_name.clone())
            .collect();
        res.push(self.executable.identification_name.clone());
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_FILE_CONFIG_JSON_PREFIX: &str = r#"{"executable":{"url":"https://example.com/repo.git","conan_output_folder":"build/conan"},"possible_dependencies":[],"debug_repo_names":[],"config":"Debug","common_conan_options":[],"compile_commands_config":{}"#;

    #[test]
    fn file_config_defaults_build_parallel_to_disabled_when_missing() {
        let config: FileConfig =
            serde_json::from_str(&format!("{}}}", BASE_FILE_CONFIG_JSON_PREFIX))
                .expect("config without cmake_build_parallel should deserialize");

        assert_eq!(config.cmake_build_parallel, CMakeBuildParallel::Disabled);
    }

    #[test]
    fn legacy_interrupt_repo_cache_defaults_to_visual_studio_generator() {
        let cache: InterruptRepoCache = serde_json::from_str("{}").unwrap();

        assert_eq!(cache.cmake_generator, "Visual Studio");
    }

    #[test]
    fn file_config_parses_bool_build_parallel_values() {
        let config_true: FileConfig = serde_json::from_str(&format!(
            r#"{}, "cmake_build_parallel":true}}"#,
            BASE_FILE_CONFIG_JSON_PREFIX
        ))
        .expect("true should deserialize");
        let config_false: FileConfig = serde_json::from_str(&format!(
            r#"{}, "cmake_build_parallel":false}}"#,
            BASE_FILE_CONFIG_JSON_PREFIX
        ))
        .expect("false should deserialize");

        assert_eq!(config_true.cmake_build_parallel, CMakeBuildParallel::Auto);
        assert_eq!(
            config_false.cmake_build_parallel,
            CMakeBuildParallel::Disabled
        );
    }

    #[test]
    fn file_config_parses_integer_build_parallel_value() {
        let config: FileConfig = serde_json::from_str(&format!(
            r#"{}, "cmake_build_parallel":4}}"#,
            BASE_FILE_CONFIG_JSON_PREFIX
        ))
        .expect("positive integer should deserialize");

        assert_eq!(config.cmake_build_parallel, CMakeBuildParallel::Jobs(4));
    }

    #[test]
    fn file_config_rejects_invalid_build_parallel_values() {
        for json in [
            format!(
                r#"{}, "cmake_build_parallel":0}}"#,
                BASE_FILE_CONFIG_JSON_PREFIX
            ),
            format!(
                r#"{}, "cmake_build_parallel":-1}}"#,
                BASE_FILE_CONFIG_JSON_PREFIX
            ),
            format!(
                r#"{}, "cmake_build_parallel":"true"}}"#,
                BASE_FILE_CONFIG_JSON_PREFIX
            ),
            format!(
                r#"{}, "cmake_build_parallel":1.5}}"#,
                BASE_FILE_CONFIG_JSON_PREFIX
            ),
        ] {
            assert!(
                serde_json::from_str::<FileConfig>(&json).is_err(),
                "json should be rejected: {}",
                json
            );
        }
    }
}
