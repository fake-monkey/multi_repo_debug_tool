# repo_debug

面向 **Windows + CMake Presets + Conan** 的多仓库本地联调工具。

当前实现重点：
- 读取配置并自动准备仓库
- 根据 Conan 依赖关系做拓扑排序并按顺序执行任务
- 支持中断恢复（`--continue`）
- 支持增量跳过（Conan/CMake/compile_commands）
- 构建遇到 LNK/OBJ 相关错误时，自动执行针对性重试修复（仅影响出错项目）
- 支持合并多个仓库的 `.sln`，便于单窗口调试
- 支持 `each` 子命令对所有参与仓库批量执行同一命令

## 1. 使用前自检

- 执行环境为 Windows（当前实现基于 Windows/VS 工具链假设）。
- 构建体系为 CMake Presets + Conan（不适用于非 CMake/Conan 体系）。
- 已安装 `git`、`conan`、`cmake`、Visual Studio / MSBuild。
- 如需生成 `compile_commands.json`，需额外安装 Clang Power Tools 扩展。

不支持场景（简版）：
- Conan 驱动 CMake 配置的流程（例如通过 Conan 直接发起/主导 configure）当前不支持。
- 判断标准：如果你的主流程是“先 Conan，再由 Conan 触发或控制 CMake configure”，而不是直接由 CMake 调用 Conan，则不在支持范围内。

## 2. 首次使用

建议按下面四步完成初始化：

1. 准备配置文件  
直接运行一次 `repo_debug`，工具会在当前目录尝试生成 `multi_repo_debug_param.json` 模板；按实际仓库信息补全后再继续。配置字段说明见下文 [4. 配置文件](#4-配置文件)。

2. 准备仓库并配置调试参数  
使用 `config` 子命令维护调试仓库列表与构建类型，例如：
```bash
repo_debug -c .\multi_repo_debug_param.json config --add D:\projects\lib_a D:\projects\lib_b --build-type RelWithDebInfo
```
如需移除仓库可使用 `config --remove`。

3. 检查分支状态  
先检查当前参与仓库是否位于预期分支：
```bash
repo_debug -c .\multi_repo_debug_param.json --check-branch
```
若发现分支不符合预期，请先在对应仓库切换到正确分支，再继续后续步骤。  
可使用 `repo_debug each --switch-remote origin/<branch>` 快速批量切换到目标分支。

4. 执行首次完整刷新  
首次建议执行一次全量刷新，确保 Conan 依赖与 CMake 配置都处于最新状态：
```bash
repo_debug -c .\multi_repo_debug_param.json --cmake-fresh --conan-update
```

## 3. 常见使用场景

### 3.1 日常调试前编译（无需 Conan/CMake 刷新）

```bash
repo_debug -c .\multi_repo_debug_param.json
```

### 3.2 某仓库文件结构变化后重跑 CMake

当某个仓库增加、删除或重命名了源码/头文件时，可只对该仓库重跑 CMake：

```bash
repo_debug -c .\multi_repo_debug_param.json --cmake <repo_path>
```

### 3.3 批量切换到远端目标分支后构建

先按拓扑顺序批量切换分支，再执行主流程构建：

```bash
repo_debug -c .\multi_repo_debug_param.json each --switch-remote origin/main
repo_debug -c .\multi_repo_debug_param.json --conan-update
```

### 3.4 开发分支长期演进后同步主线并构建

```bash
repo_debug -c .\multi_repo_debug_param.json --merge --conan-update
```

### 3.5 调整调试仓库和构建类型

先更新配置，再执行主流程：

```bash
repo_debug -c .\multi_repo_debug_param.json config --build-type Debug --add lib_b_path --remove lib_a_path
repo_debug -c .\multi_repo_debug_param.json
```

### 3.6 从中断点继续执行

建议按上键恢复上一次命令，并在末尾追加 `--continue`：

```bash
repo_debug -c .\multi_repo_debug_param.json <上一次的命令> --continue
```

## 4. 配置文件

默认配置文件名：`multi_repo_debug_param.json`（当前工作目录下）。

也可显式指定：

```bash
repo_debug -c path\to\multi_repo_debug_param.json
```

### 4.1 顶层字段

- `executable`: 主程序仓库（必填）
- `possible_dependencies`: 可能依赖仓库列表
- `debug_repo_names`: 需要重点调试的仓库标识名列表
- `config`: 构建类型（`Debug` / `Release` / `RelWithDebInfo` / `MinSizeRel`）
- `common_conan_options`: 全仓共用 Conan 参数
- `compile_commands_config`: `compile_commands.json` 相关配置

### 4.2 仓库字段（`executable` 与 `possible_dependencies[*]` 共用）

- `url`: 仓库地址（必填）
- `path`: 相对配置文件目录的本地路径（可选；不存在时会尝试按 `url` clone）
- `extra_conan_options`: 该仓库额外 Conan 参数
- `extra_cmake_options`: 该仓库额外 CMake configure 参数
- `conan_output_folder`: Conan `--output-folder`（默认 `build/conan`）
- `cmake_generator_keyword`: CMake 生成器的匹配关键字（默认 `"Visual Studio"`）。工具会在仓库的 `CMakeUserPresets.json` / `CMakePresets.json` 中查找 `generator` 字段包含该关键字的预设，并将其作为构建生成器。例如设置 `"Ninja"` 则匹配 `"Ninja"` 或 `"Ninja Multi-Config"` 等预设。
- `conan_toolchain_managed_by_cmake`: `true` 时不注入 `CMAKE_TOOLCHAIN_FILE`（兼容旧字段名 `use_cmake_conan_files`）
- `enable`: 是否参与构建分析（默认 `true`）

### 4.3 `compile_commands_config`

- `enabled`: 是否启用生成 `compile_commands.json`
- `options`: 传给 `clang-build.ps1` 的额外参数

### 4.4 最小示例

```json
{
  "executable": {
    "url": "https://github.com/example/app.git",
    "path": "app",
    "extra_conan_options": [],
    "extra_cmake_options": [],
    "conan_output_folder": "build/conan",
    "conan_toolchain_managed_by_cmake": false,
    "enable": true
  },
  "possible_dependencies": [
    {
      "url": "https://github.com/example/lib_a.git",
      "path": "lib_a",
      "extra_conan_options": [],
      "extra_cmake_options": [],
      "conan_output_folder": "build/conan",
      "conan_toolchain_managed_by_cmake": false,
      "enable": true
    }
  ],
  "debug_repo_names": ["lib_a"],
  "config": "RelWithDebInfo",
  "common_conan_options": ["--build=missing"],
  "compile_commands_config": {
    "enabled": false,
    "options": []
  }
}
```

## 5. 命令总览

### 5.1 主流程（无子命令）

```bash
repo_debug [OPTIONS]
```

常用参数：
- `-c, --config-file <PATH>`: 指定配置文件
- `--check-branch`: 只打印参与仓库分支信息后退出
- `--merge`: 启用 Git merge 任务
- `--conan`: 执行 Conan install（按需增量）
- `--conan-update`: Conan install 且强制带 `--update`
- `--cmake [PATH ...]`: 对全部或指定仓库执行 CMake configure
- `--cmake-fresh [PATH ...]`: 对全部或指定仓库执行 CMake configure --fresh
- `--continue`: 从上次中断进度继续
- `--verbose`: 打开 debug 日志

### 5.2 `config` 子命令（低频配置维护）

```bash
repo_debug config --add <PATH...>
repo_debug config --remove <PATH...>
repo_debug config --build-type <TYPE>
```

规则：
- `--add/--remove` 可同用
- `--build-type` 可与 `--add/--remove` 同时使用
- `PATH` 需要能映射到已管理仓库路径

### 5.3 `each` 子命令（批量执行）

`each` 由三个可组合维度组成：执行动作、分支同步、仓库范围过滤。

#### 5.3.1 执行动作（运行外部命令）

```bash
repo_debug each -- git status -sb
```

说明：
- 在每个仓库根目录执行同一条外部命令
- 当命令参数中包含 `-` 开头内容时，建议使用 `--` 分隔

#### 5.3.2 分支同步动作（`--switch-remote`）

```bash
repo_debug each --switch-remote origin/main
```

说明：
- `--switch-remote <remote/branch>` 会在每个仓库执行 fetch/switch/pull（按当前分支状态选择具体 git 动作）
- 可与“执行动作”组合：先同步分支，再执行外部命令

#### 5.3.3 仓库范围过滤（`--only` / `--except`）

```bash
repo_debug each --only D:\repo\a D:\repo\b -- git status -sb
repo_debug each --except D:\repo\legacy --switch-remote origin/main
```

说明：
- `--only <PATH...>`：仅处理这些路径对应仓库（白名单）
- `--except <PATH...>`：跳过这些路径对应仓库（黑名单）
- `--only` 与 `--except` 互斥
- `PATH` 需要能映射到已管理仓库路径

通用行为：
- 按和主流程一致的依赖拓扑顺序遍历仓库
- `each` 不会执行 Conan/CMake/build/sln merge
- 失败时会提示是否继续处理后续仓库，并汇总 deferred errors
