# TODO

本文件记录 `repo_debug` 的待办事项。已完成历史见 [CHANGELOG.md](CHANGELOG.md) 与 [设计文档.md](docs/设计文档.md) 的 TODO List。

## 1. 不假定一个 Project 只有一个 xxxConfig；扫描 xxxConfig 自动生成

**状态**: 待做，优先级高。

**背景**: 当前配置模型隐含两个假设，需要打破：

- 一个 Project（仓库/工程）只对应一个 xxxConfig。例如 `FileConfig.executable` 是单个 `RepoConfig`，`RepoConfig` 与仓库路径一一对应。
- xxxConfig 里的名称与 project 名称一致。例如 `identification_name` 被当作配置检索键和 project 名称使用，`src/main.rs`（判定主程序）、`src/config/task_containers.rs:208`（判定是否需要 install）都直接拿它做判断。

**目标**: 一个 Project 可有多个 xxxConfig，xxxConfig 的名称不必等于 project 名称；改为扫描 xxxConfig 文件自动生成配置条目与映射，而不是靠硬编码的一对一假设。

**涉及**: `src/config.rs`（`FileConfig.executable` / `possible_dependencies` 组装）、`src/repo_config.rs`（`identification_name`）、`src/config/task_containers.rs:208`、`src/main.rs` 的主程序判定。与「桌面端多 worktree 管理」的仓库定义 / worktree / 编译工作区数据模型相关。

**注意**: 「xxxConfig」为占位名，具体指哪种配置（CMake preset / RepoConfig / 编译工作区配置）待定，此处先记录原则。

## 2. 桌面端多 worktree 管理工具（只写文档索引）

**状态**: 计划已定，尚未开工。

完整的产品定位、项目布局、数据关系、前端 / Tauri 壳布局、依赖方向与第一阶段验收标准均已在 [桌面端项目计划.md](docs/桌面端项目计划.md) 中写明，此处只做索引，不重复。

实施前需先完成该文档「后续计划待补内容」一节列的六项：第一版编译工作区的用户流程和页面信息架构、仓库定义 / worktree 选择 / 编译工作区的数据模型、现有 `main.rs` 提取 library 入口的边界、编译进度与中断恢复协议、worktree 写操作的安全规则、分阶段实现与真实临时仓库测试计划。

第一条 Rust 主改动应是：把现有 `multi_repo_debug_tool` 主流程从 `main.rs` 组装形式暴露为 `lib.rs` 库入口（同时保留 CLI），供 Tauri 后端复用同一套 Conan/CMake 编排，而不是复制一套。

## 3. Conan 驱动的构建过程适配

**状态**: 待做。

**背景**: README「不支持场景」中写明，当前只支持「CMake 调用 Conan」的流程（仓库内 CMakeLists 内部执行 conan install，工具用 `conan_toolchain_managed_by_cmake` 不注入 toolchain）；当主流程是「先 Conan，再由 Conan 发起/主导 CMake configure」（如 `conan build .`、conanfile.py 的 generate/build 驱动 configure）时，当前不支持。

**目标**: 识别此类仓库并适配其 configure 入口，使工具能正确编排「Conan 驱动」的构建过程，而不是假设 configure 一律由工具直接调用 CMake 触发。

**涉及**: `conan_executor.rs`（Conan install/生成物路径）、`cmake_configure.rs`（configure 触发方式）、conanfile 解析与拓扑信息。

## 4. `conan_output_folder` 重命名

**状态**: 待做，新字段名待定。

**背景**: `RepoConfig.conan_output_folder`（`src/repo_config.rs:39`）承载 Conan `--output-folder`，会被传给 conan install（`src/conan_executor.rs:19,47`）并经 `src/config/task_containers.rs:132` 传递。

**现状问题**:

- 字段无 `#[serde(default)]`，`RepoConfig::default()` 给出 `"."`（`src/repo_config.rs:63`），而 README 4.2 文档化的默认值是 `build/conan`（`README.md:126`），两者不一致。缺省时按哪个值生效、文档与实际行为对不上，需要一并理顺。
- 重命名后旧配置文件里用旧键名的会反序列化失败，需保留 `#[serde(alias = "旧名")]` 兼容，参照 `use_cmake_conan_files` → `conan_toolchain_managed_by_cmake` 的先例（`src/repo_config.rs:42`）。

**涉及**: `src/repo_config.rs`（字段、`Default`、测试）、`README.md` 4.2 与 4.4、`src/config.rs` 的模板 JSON（`BASE_FILE_CONFIG_JSON_PREFIX`，约 631 行）、`src/config/task_containers.rs` 的字段传递。

**注意**: 与第 5 项联动 —— 重命名时旧键名会进入「旧配置读取失败」路径，修复提示必须一并更新。

## 5. 旧配置文件读取失败时，补充详细的修复信息

**状态**: 待做。

**背景**: `load_or_create_file_config`（`src/config.rs:176-184`）用 `serde_json::from_str::<FileConfig>` 反序列化，失败时只报 `Failed to deserialize config` 加 serde 原始错误。旧配置缺字段（例如字段重命名后旧键名失效）、字段类型不匹配时，用户无法从报错得知具体是哪个仓库、哪个字段、该怎么补。

**目标**: 反序列化失败时给出可操作的修复信息：

- 指出具体位置与字段，如 `possible_dependencies[2].conan_output_folder`。
- 说明新旧字段名映射（若涉及重命名）。
- 给出补全该字段的示例值或默认值。
- 指向 README 对应配置章节。

**注意**: 与第 4 项联动，重命名时必须同步补好这里的提示。

## 6. 打包与分发流程

**状态**: 已完成。

CLI 的打包与分发方案已经落地，完整约定见 [发布方案.md](docs/发布方案.md)：

- `scripts/publish_release.py` 从根 Cargo workspace 构建 Windows release 产物，并生成确定性 ZIP 和 SHA256。
- 本子仓库同时作为 Scoop bucket，manifest 固定维护在 `bucket/` 目录。
- 通过 GitHub Release 分发产物，并提供相互独立的 `repo_debug` stable 与 `repo_debug-alpha` 更新通道。
- 正式发布前检查版本号、Git 标签和 GitHub Release，禁止直接覆盖已经发布的版本。

桌面端（`desktop` + Tauri）尚未实现，其发布产物是否并入这套流程由桌面端任务另行决定，不影响本项 CLI 发布工作的完成状态。
