use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_enable() -> bool {
    true
}

fn default_auto_clone_for_legacy_config() -> bool {
    true
}

fn default_cmake_generator_keyword() -> String {
    "Visual Studio".to_string()
}

/// 仓库配置信息
#[derive(Deserialize, Serialize, Debug)]
pub struct RepoConfig {
    #[serde(default)]
    pub url: String,

    /// 仓库路径，相对配置文件目录；也允许直接填写绝对路径
    #[serde(default)]
    pub path: PathBuf,

    #[serde(skip)]
    pub identification_name: String,

    #[serde(default)]
    pub extra_conan_options: Vec<String>,

    #[serde(default)]
    pub extra_cmake_options: Vec<String>,

    /// 在 CMake preset 的 generator 字段中查找该关键字。
    #[serde(default = "default_cmake_generator_keyword")]
    pub cmake_generator_keyword: String,

    pub conan_output_folder: PathBuf,

    /// 为 true 时不在 CMake 预设中注入 `CMAKE_TOOLCHAIN_FILE`，由仓库内 CMake/Conan 流程自行提供 toolchain。
    #[serde(default, alias = "use_cmake_conan_files")]
    pub conan_toolchain_managed_by_cmake: bool,

    /// 路径不可用时是否允许自动 clone 仓库；关闭后必须提供可用的本地 path。
    #[serde(default = "default_auto_clone_for_legacy_config")]
    pub auto_clone: bool,

    // 允许记录配置但完全不参与构建分析
    #[serde(default = "default_enable")]
    pub enable: bool,
}

impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            path: PathBuf::new(),
            identification_name: String::new(),
            extra_conan_options: Vec::new(),
            extra_cmake_options: Vec::new(),
            cmake_generator_keyword: default_cmake_generator_keyword(),
            conan_output_folder: PathBuf::from("."),
            conan_toolchain_managed_by_cmake: true,
            auto_clone: false,
            enable: true,
        }
    }
}

impl RepoConfig {
    pub fn empty() -> Self {
        Self {
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RepoConfig;

    #[test]
    fn missing_auto_clone_deserializes_to_enabled() {
        let config: RepoConfig = serde_json::from_str(
            r#"{"url":"https://example.com/example/repo.git","conan_output_folder":"build/conan"}"#,
        )
        .unwrap();

        assert!(config.auto_clone);
    }

    #[test]
    fn default_repo_config_disables_auto_clone() {
        assert!(!RepoConfig::default().auto_clone);
        assert!(!RepoConfig::empty().auto_clone);
    }

    #[test]
    fn missing_url_deserializes_to_empty_string() {
        let config: RepoConfig =
            serde_json::from_str(r#"{"conan_output_folder":"build/conan","auto_clone":false}"#)
                .unwrap();

        assert!(config.url.is_empty());
    }

    #[test]
    fn missing_cmake_generator_keyword_defaults_to_visual_studio() {
        let config: RepoConfig = serde_json::from_str(
            r#"{"url":"https://example.com/example/repo.git","conan_output_folder":"build/conan"}"#,
        )
        .unwrap();

        assert_eq!(config.cmake_generator_keyword, "Visual Studio");
    }
}
