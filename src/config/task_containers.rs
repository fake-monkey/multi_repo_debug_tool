use super::FileConfig;
use crate::cmake_build_install::{BuildInstallInput, BuildInstallOutput, BuildInstallTask};
use crate::cmake_configure::{CMakeConfigureInput, CMakeConfigureOutput, CMakeConfigureTask};
use crate::compile_commands::{CompileCommandsInput, CompileCommandsTask};
use crate::conan_executor::{ConanInstallInput, ConanInstallOutput, ConanInstallTask};
use crate::repo;
use crate::task::{
    DataContainer, DataContainerBase, FingerprintProvider, IncrementalDataContainer,
};
use diag_trace::{anyhow_loc, err_loc};
use petgraph::visit::Dfs;
use std::collections::HashSet;
use std::path::PathBuf;

impl DataContainerBase for FileConfig {
    fn save(&self) -> anyhow::Result<()> {
        self.save_interrupt_cache()
    }

    fn mark_task_finish(&mut self, repo_name: &str, task_id: &str) -> anyhow::Result<()> {
        self.interrupt_cache
            .completed_tasks
            .entry(task_id.to_string())
            .or_insert(HashSet::new())
            .insert(repo_name.to_string());
        Ok(())
    }

    fn is_task_finished(&self, repo_name: &str, task_id: &str) -> anyhow::Result<bool> {
        Ok(self
            .interrupt_cache
            .completed_tasks
            .get(task_id)
            .map_or(false, |repo_names| repo_names.contains(repo_name)))
    }
}

impl FingerprintProvider for FileConfig {
    fn get_fingerprint_json(
        &self,
        repo_name: &str,
        task_id: &str,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(self
            .interrupt_cache
            .interrupt_repo_cache
            .get(repo_name)
            .map_or(serde_json::Value::Null, |repo_cache| {
                repo_cache
                    .borrow()
                    .task_fingerprints
                    .get(task_id)
                    .cloned()
                    .unwrap_or(serde_json::Value::Null)
            }))
    }

    fn save_fingerprint_json(
        &mut self,
        repo_name: &str,
        task_id: &str,
        fingerprint: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.interrupt_cache
            .interrupt_repo_cache
            .get(repo_name)
            .ok_or_else(|| anyhow_loc!("Repo cache not found for: {}", repo_name))?
            .borrow_mut()
            .task_fingerprints
            .insert(task_id.to_string(), fingerprint);
        Ok(())
    }
}

impl DataContainer<repo::GitMergeTask> for FileConfig {
    fn get_input(&self, name: &str, _task_id: &str) -> anyhow::Result<repo::GitMergeInput> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let cache_borrow = cache.borrow();
        let sync_history = self.interrupt_cache.sync_branch_history.borrow().clone();
        Ok(repo::GitMergeInput {
            repo_path: cache_borrow.repo_dir.clone(),
            last_local_branch: cache_borrow.last_local_branch.clone(),
            last_sync_branch: cache_borrow.last_sync_branch.clone(),
            sync_branch_history: sync_history,
        })
    }

    fn save_output(
        &mut self,
        repo_name: &str,
        _task_id: &str,
        output: repo::GitMergeOutput,
    ) -> anyhow::Result<()> {
        let cache = self.get_interrupt_repo_cache(repo_name)?;
        let cache_mut = &mut *cache.borrow_mut();

        // 更新仓库管特定的缓存信息
        cache_mut.last_local_branch = output.current_local_branch.clone();
        cache_mut.last_sync_branch = output.current_sync_branch.clone();

        // 更新全局历史记录
        let mut history = self.interrupt_cache.sync_branch_history.borrow_mut();
        let branch = &output.current_sync_branch;
        // 避免重复
        if let Some(pos) = history.iter().position(|b| b == branch) {
            history.remove(pos);
        }
        history.insert(0, branch.clone());
        history.truncate(10); // 最多保留10条

        Ok(())
    }
}

impl IncrementalDataContainer<ConanInstallTask> for FileConfig {
    fn get_input(&self, name: &str) -> anyhow::Result<ConanInstallInput> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let repo_dir = &cache.borrow().repo_dir;

        let repo_config = self.get_repo_config(name)?;

        let extra_conan_options = {
            let mut options = self.common_conan_options.clone();
            options.extend(repo_config.extra_conan_options.clone());
            options
        };
        Ok(ConanInstallInput {
            repo_dir: repo_dir.clone(),
            extra_conan_options,
            conan_output_folder: repo_config.conan_output_folder.clone(),
            need_update: self.need_conan_install_update,
            need_install: self.need_conan_install || self.need_conan_install_update,
        })
    }

    fn save_output(&mut self, repo_name: &str, output: ConanInstallOutput) -> anyhow::Result<()> {
        let cache = self.get_interrupt_repo_cache(repo_name)?;

        let mut repo_cache = cache.borrow_mut();
        if output.conan_toolchain_path.is_some() {
            repo_cache.conan_toolchain_path =
                repo_cache.get_rel_path(&output.conan_toolchain_path.as_ref().unwrap())?;
        }
        repo_cache.conan_toolchain_hash = output
            .new_conan_toolchain_hash
            .unwrap_or(repo_cache.conan_toolchain_hash);

        Ok(())
    }
}

impl IncrementalDataContainer<CMakeConfigureTask> for FileConfig {
    fn get_input(&self, repo_name: &str) -> anyhow::Result<CMakeConfigureInput> {
        // 从 graph 获得当前节点的所有依赖项
        let graph = &self.dependency_topological_info.graph;
        let name = repo_name;
        let start_node = graph
            .node_indices()
            .find(|&idx| graph.node_weight(idx).map(|s| s.as_str()) == Some(name))
            .ok_or_else(|| anyhow_loc!("Package {} not found in dependency graph", name))?;

        let mut all_deps = HashSet::new();

        // 使用深度优先搜索遍历依赖项
        let mut dfs = Dfs::new(&graph, start_node);
        while let Some(nx) = dfs.next(&graph) {
            if nx != start_node {
                // 跳过起始节点本身
                all_deps.insert(nx);
            }
        }

        let mut all_dep_info = Vec::new();
        for dep in all_deps {
            let dep_name = graph
                .node_weight(dep)
                .ok_or_else(|| anyhow_loc!("Node weight not found for node {:?}", dep))?;
            // 跳过 enable=false 的依赖，不为其构造 CMake 依赖信息
            if !self.is_dependency_enabled_in_config(dep_name) {
                continue;
            }
            let dep_info = self.get_cmake_dependency_info(dep_name)?;
            all_dep_info.push(dep_info);
        }

        let cmake_executor_info = self.get_cmake_executor_info(name)?;
        Ok(CMakeConfigureInput {
            repo_config: cmake_executor_info,
            dependencies: all_dep_info,
        })
    }
    fn save_output(&mut self, repo_name: &str, output: CMakeConfigureOutput) -> anyhow::Result<()> {
        self.set_cmake_executor_output(repo_name, output)?;
        Ok(())
    }
}

impl DataContainer<BuildInstallTask> for FileConfig {
    fn get_input(&self, name: &str, _task_id: &str) -> anyhow::Result<BuildInstallInput> {
        let cache = self.get_interrupt_repo_cache(name)?;
        let repo_cache = cache.borrow();

        Ok(BuildInstallInput {
            repo_path: repo_cache.repo_dir.clone(),
            build_config: self.config.clone(),
            need_install: name != self.executable.identification_name, // 可执行程序不安装，只构建
            cmake_pkg_name: repo_cache.cmake_pkg_name.clone(),
            install_prefix: repo_cache.get_abs_path(&repo_cache.install_prefix),
            cmake_binary_dir: repo_cache.get_abs_path(&repo_cache.cmake_binary_dir),
            build_parallel: self.cmake_build_parallel.clone(),
            cmake_generator: repo_cache.cmake_generator.clone(),
        })
    }
    fn save_output(
        &mut self,
        name: &str,
        _task_id: &str,
        output: BuildInstallOutput,
    ) -> anyhow::Result<()> {
        let cache = self.get_interrupt_repo_cache(name)?;

        let mut repo_cache = cache.borrow_mut();
        repo_cache.cmake_pkg_config_dir = match &output.cmake_pkg_config_dir {
            Some(dir) => repo_cache.get_rel_path(dir)?,
            None => PathBuf::new(),
        };

        Ok(())
    }
}

impl IncrementalDataContainer<CompileCommandsTask> for FileConfig {
    fn get_input<'a>(&'a self, name: &str) -> anyhow::Result<CompileCommandsInput<'a>> {
        let sln_path = if self.is_visual_studio_generator(name)? {
            let sln_path = self.get_sln_file_path(name)?;
            if !sln_path.exists() {
                return err_loc!(
                    "Solution file not found for compile commands generation: {}",
                    sln_path.display()
                );
            }
            Some(sln_path)
        } else {
            None
        };
        Ok(CompileCommandsInput {
            compile_commands_config: &self.compile_commands_config,
            sln_path,
        })
    }
    fn save_output(&mut self, _repo_name: &str, _output: ()) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmake_build_install::CMakeBuildParallel;
    use crate::config::InterruptRepoCache;
    use crate::repo_config::RepoConfig;
    use std::cell::RefCell;

    #[test]
    fn build_install_input_receives_global_build_parallel_config() {
        let mut file_config = FileConfig {
            cmake_build_parallel: CMakeBuildParallel::Jobs(4),
            executable: RepoConfig {
                identification_name: "app".to_string(),
                ..RepoConfig::empty()
            },
            ..FileConfig::default()
        };
        let mut repo_cache = InterruptRepoCache::default();
        repo_cache.repo_dir = PathBuf::from("D:\\repo");
        repo_cache.cmake_pkg_name = "pkg".to_string();
        repo_cache.install_prefix = PathBuf::from("install");
        repo_cache.cmake_binary_dir = PathBuf::from("build");
        file_config
            .interrupt_cache
            .interrupt_repo_cache
            .insert("pkg".to_string(), RefCell::new(repo_cache));

        let input = <FileConfig as DataContainer<BuildInstallTask>>::get_input(
            &file_config,
            "pkg",
            "build_install",
        )
        .expect("build install input should be created");

        assert_eq!(input.build_parallel, CMakeBuildParallel::Jobs(4));
    }

    #[test]
    fn compile_commands_input_omits_solution_for_non_visual_studio_repo() {
        let mut file_config = FileConfig::default();
        let mut repo_cache = InterruptRepoCache::default();
        repo_cache.cmake_generator = "Ninja".to_string();
        file_config
            .interrupt_cache
            .interrupt_repo_cache
            .insert("pkg".to_string(), RefCell::new(repo_cache));

        let input = <FileConfig as IncrementalDataContainer<CompileCommandsTask>>::get_input(
            &file_config,
            "pkg",
        )
        .unwrap();

        assert!(input.sln_path.is_none());
    }
}
