# Changelog

## [1.2.1-alpha.1]

### Added

- 增加基于 Python 和 GitHub CLI 的 GitHub Release 发布程序，自动完成 release 构建、ZIP 打包、SHA256 计算、alpha Scoop manifest 生成以及 Release 附件上传。
- 建立 `repo_debug` stable 与 `repo_debug-alpha` 预发布通道约定；首个 alpha 版本通过 `repo_debug-alpha` manifest 分发。

## [1.2.0]

### Added

- 支持按仓库选择 CMake 生成器（`cmake_generator_keyword` 为单个仓库级别配置），可通过关键字（如 `Ninja`、`Visual Studio`）指定。选择 Ninja 时自动注入 MSVC 构建环境并生成 `compile_commands.json`；非 VS 生成器的仓库自动跳过 `.sln` 合并和 DLL 复制。注意 Ninja 模式下不再产出 `.sln`，需配合其他 IDE 进行调试。
- `multi_repo_dev_config` 预设的 `inherits` 搜索逻辑调整为优先从 `CMakeUserPresets.json` 中查找。
- `multi_repo_dev_config` 沿用基础预设的 `binaryDir` 和 `CMAKE_INSTALL_PREFIX`，不再强制覆盖仓库的构建与安装目录；Visual Studio 与 Ninja 生成的 `compile_commands.json` 统一发布到实际构建目录的第一层目录。

## [1.1.0]

### Added

- 支持配置 CMake 构建并行度（配置文件字段 `cmake_build_parallel`），可选 `false`（禁用）、`true`（默认并行）或正整数（指定并行数）。

## [1.0.0]

### Added

- `each` 子命令新增 `--only <PATH>` / `--except <PATH>` 参数，可按仓库路径白名单/黑名单过滤执行范围。
- `each` 子命令新增 `--switch-remote <remote/branch>` 参数，以本地代码最小变动的策略批量切换到远端分支（含子模块递归更新）。

### Changed

- 配置字段 `use_cmake_conan_files` 重命名为 `conan_toolchain_managed_by_cmake`，语义不变；旧名称仍可正常解析以兼容已有配置。

## 0.2.x

- 中断缓存数据移出配置文件，配置文件可直接复制共享
- `each` 命令执行失败时聚合 deferred error，不中断后续仓库处理
- `--switch-remote` 支持递归更新子模块

## 0.1.x

项目奠基阶段，建立以下核心能力：

- 配置文件驱动的多仓库本地联调
- 基于 Conan 依赖关系的拓扑排序执行
- 中断恢复（`--continue`）
- 增量跳过（Conan / CMake / compile_commands）
- 构建 LNK/OBJ 错误自动重试修复
- 多仓库 `.sln` 合并，单窗口调试
- `config` 子命令：维护调试仓库列表与构建类型
- `each` 子命令：对所有参与仓库批量执行同一命令
- `--check-branch`：检查参与仓库分支状态
- 交互式分支选择
- DLL 自动复制
- Ctrl-C 终止时自动清理子进程
- `compile_commands.json` 生成（需安装 Clang Power Tools 扩展）
