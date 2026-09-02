use crate::cmake_presets::{self, CMakePresets, ConfigurePreset};
use crate::{
    task::{IncrementalTask, TaskMeta},
    HashType,
};
use diag_trace::{self as diag, err_loc, loc_context, LocContextExt};
use log::{debug, info};
use path_absolutize::Absolutize;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::rc::Rc;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// 从 CMakeCache.txt 中解析 CMAKE_PROJECT_NAME 变量值
/// 传入 CMake 二进制目录（通常为 "${sourceDir}/build"），从其中的
/// CMakeCache.txt 提取 `CMAKE_PROJECT_NAME` 的值。
fn parse_cmake_project_name_from_cache(build_dir: &Path) -> anyhow::Result<String> {
    let cache_path = build_dir.join("CMakeCache.txt");
    let content = fs::read_to_string(&cache_path)
        .with_loc_context(|| format!("Failed to read '{}'", cache_path.display()))?;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("CMAKE_PROJECT_NAME:") {
            if let Some(eq_pos) = rest.find('=') {
                let value = &rest[eq_pos + 1..];
                let value = value.trim().trim_matches('"');
                if !value.is_empty() {
                    return Ok(value.to_string());
                }
            }
        }
    }
    return err_loc!("CMAKE_PROJECT_NAME not found in '{}'", cache_path.display());
}

struct DevPresetUpdateResult {
    new_cmake_user_presets_hasher: DefaultHasher,
    binary_dir: PathBuf,
    install_dir: PathBuf,
    dev_preset_name: String,
    cmake_generator: String,
}

struct DevPresetBaseUpdateResult {
    binary_dir: String,
    install_dir: String,
    base_idx: usize,
    cmake_generator: String,
}

fn convert_to_cmake_path(path: &Path) -> anyhow::Result<String> {
    let abs_path = path
        .absolutize()
        .map(|p| p.to_path_buf())
        .with_loc_context(|| format!("Failed to absolutize path '{}'", path.display()))?;

    let mut full_path = abs_path.to_string_lossy().to_string();
    full_path = full_path.replace("\\", "/");
    Ok(full_path)
}

fn resolve_repo_relative_preset_path(
    source_dir: &Path,
    preset_path: &str,
    field_name: &str,
) -> anyhow::Result<PathBuf> {
    let source_dir = source_dir
        .absolutize()
        .map(|path| path.to_path_buf())
        .with_loc_context(|| {
            format!(
                "Failed to absolutize source directory '{}' while resolving preset field '{}'",
                source_dir.display(),
                field_name
            )
        })?;
    if preset_path.contains("$env{") || preset_path.contains("$penv{") {
        return err_loc!(
            "Environment variable macros are not supported in preset field '{}': '{}'",
            field_name,
            preset_path
        );
    }

    let expanded_path = preset_path.replace("${sourceDir}", &source_dir.to_string_lossy());
    if contains_preset_macro(&expanded_path) {
        return err_loc!(
            "Unsupported unresolved macro in preset field '{}': '{}'",
            field_name,
            preset_path
        );
    }

    let path = PathBuf::from(expanded_path);
    let absolute_path = if path.is_absolute() {
        path
    } else {
        source_dir.join(path)
    };
    let absolute_path = absolute_path
        .absolutize()
        .map(|path| path.to_path_buf())
        .with_loc_context(|| {
            format!(
                "Failed to absolutize resolved preset field '{}': original='{}', resolved='{}'",
                field_name,
                preset_path,
                absolute_path.display()
            )
        })?;

    let relative_path = pathdiff::diff_paths(&absolute_path, &source_dir);
    let Some(relative_path) = relative_path.filter(|path| !path.is_absolute()) else {
        return err_loc!(
            "Failed to make preset field '{}' relative to repo root: original='{}', resolved='{}', repo='{}'",
            field_name,
            preset_path,
            absolute_path.display(),
            source_dir.display()
        );
    };
    Ok(relative_path)
}

fn contains_preset_macro(value: &str) -> bool {
    value.match_indices('$').any(|(idx, _)| {
        let rest = &value[idx + 1..];
        let Some(open_brace) = rest.find('{') else {
            return false;
        };
        rest[..open_brace]
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

#[loc_context(format!(
    "Failed to update dev preset from base for repo '{}'",
    repo_path.display()
))]
fn prepare_dev_preset(
    repo_path: &Path,
    build_config: &str,
    cmake_generator_keyword: &str,
    conan_toolchain_path: Option<&Path>,
    dependencies: &[CMakeDependencyInfo],
) -> anyhow::Result<DevPresetUpdateResult> {
    let (mut presets, mut user_presets) = cmake_presets::parse_cmake_presets(&repo_path)?;
    prepare_base_preset_candidates(&mut presets, &user_presets)?;
    let dev_preset_name = String::from(DEV_PRESET_NAME);

    let dev_idx = if let Some(idx) = user_presets
        .configure_presets
        .iter()
        .position(|p| p.borrow().name == dev_preset_name)
    {
        idx
    } else {
        let new_preset = ConfigurePreset::new_with_name(&dev_preset_name);
        user_presets
            .configure_presets
            .push(std::cell::RefCell::new(new_preset));
        user_presets.configure_presets.len() - 1
    };

    let original_preset = user_presets.configure_presets[dev_idx]
        .try_borrow()
        .with_loc_context(|| {
            format!(
                "Failed to borrow user preset '{}' immutably",
                dev_preset_name
            )
        })?
        .clone();

    debug!(
        "Using/created user preset '{}' (index: {}) for build config {}",
        original_preset.name, dev_idx, build_config
    );

    let DevPresetBaseUpdateResult {
        binary_dir: raw_binary_dir,
        install_dir: raw_install_dir,
        base_idx,
        cmake_generator,
    } = update_dev_preset_from_base(
        &presets,
        &mut user_presets,
        dev_idx,
        build_config,
        cmake_generator_keyword,
        conan_toolchain_path,
        dependencies,
    )?;

    let binary_dir = resolve_repo_relative_preset_path(repo_path, &raw_binary_dir, "binaryDir")?;
    let install_dir =
        resolve_repo_relative_preset_path(repo_path, &raw_install_dir, "CMAKE_INSTALL_PREFIX")?;

    let changed = {
        let updated_preset = user_presets.configure_presets[dev_idx]
            .try_borrow()
            .with_loc_context(|| {
                format!(
                    "Failed to borrow user preset '{}' immutably",
                    dev_preset_name
                )
            })?;
        original_preset != *updated_preset
    };

    if changed {
        cmake_presets::save_cmake_user_presets(&repo_path, &user_presets)?;
        debug!(
            "Updated CMake user presets saved to '{}'",
            repo_path.join("CMakeUserPresets.json").display()
        );
    } else {
        debug!("No changes to CMake user presets, no need to save");
    }

    let new_hash = {
        let base_preset = presets.configure_presets[base_idx]
            .try_borrow()
            .with_loc_context(|| "Failed to borrow base preset for hash calculation")?;
        let updated_preset = user_presets.configure_presets[dev_idx]
            .try_borrow()
            .with_loc_context(|| "Failed to borrow updated preset for hash calculation")?;
        let mut hasher = DefaultHasher::new();
        base_preset.hash(&mut hasher);
        updated_preset.hash(&mut hasher);
        hasher
    };

    Ok(DevPresetUpdateResult {
        new_cmake_user_presets_hasher: new_hash,
        binary_dir,
        install_dir,
        dev_preset_name,
        cmake_generator,
    })
}

const DEV_PRESET_NAME: &str = "multi_repo_dev_config";

fn prepare_base_preset_candidates(
    presets: &mut CMakePresets,
    user_presets: &CMakePresets,
) -> anyhow::Result<()> {
    let mut user_candidates = Vec::new();
    for preset_cell in &user_presets.configure_presets {
        let preset = preset_cell
            .try_borrow()
            .with_loc_context(|| "Failed to borrow CMake user preset")?;
        if preset.name != DEV_PRESET_NAME {
            user_candidates.push(RefCell::new(preset.clone()));
        }
    }

    presets.configure_presets.splice(0..0, user_candidates);
    cmake_presets::expand_cmake_preset_inheritance(&presets.configure_presets)
}

fn update_dev_preset_from_base(
    presets: &CMakePresets,
    user_presets: &mut CMakePresets,
    dev_idx: usize,
    build_config: &str,
    cmake_generator_keyword: &str,
    toolchain_file: Option<&Path>,
    dependencies: &[CMakeDependencyInfo],
) -> anyhow::Result<DevPresetBaseUpdateResult> {
    let mut dev_preset = user_presets.configure_presets[dev_idx]
        .try_borrow_mut()
        .with_loc_context(|| "Failed to borrow user preset mutably")?;

    let priority_idx = dev_preset.inherits.as_ref().and_then(|v| {
        if !v.is_empty() {
            presets
                .configure_presets
                .iter()
                .position(|p| p.borrow().name == v[0])
        } else {
            None
        }
    });

    let base_idx = find_based_preset(presets, build_config, cmake_generator_keyword, priority_idx)?;
    debug!(
        "Base preset for '{}' is '{}' (index: {})",
        dev_preset.name,
        presets.configure_presets[base_idx].borrow().name,
        base_idx
    );
    let base_preset = presets.configure_presets[base_idx]
        .try_borrow()
        .with_loc_context(|| "Failed to borrow base preset")?;
    let cmake_generator = base_preset
        .generator
        .clone()
        .ok_or_else(|| diag::anyhow_loc!("Base preset generator is missing"))?;
    let binary_dir = base_preset
        .binary_dir
        .clone()
        .ok_or_else(|| diag::anyhow_loc!("Base preset binaryDir is missing"))?;
    let install_dir = base_preset
        .cache_variables
        .as_ref()
        .and_then(|variables| variables.get("CMAKE_INSTALL_PREFIX"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            diag::anyhow_loc!("Base preset CMAKE_INSTALL_PREFIX must be a string")
        })?;

    dev_preset.inherits = Some(vec![base_preset.name.clone()]);
    dev_preset.binary_dir = None;

    let cache_var = dev_preset.cache_variables.get_or_insert_with(BTreeMap::new);
    cache_var.remove("CMAKE_INSTALL_PREFIX");
    cache_var.insert(
        "CMAKE_CONFIGURATION_TYPES".to_string(),
        Value::String(build_config.to_string()),
    );
    cache_var.insert(
        "CMAKE_BUILD_TYPE".to_string(),
        Value::String(build_config.to_string()),
    );

    if cmake_generator.contains("Ninja") {
        cache_var.insert(
            "CMAKE_EXPORT_COMPILE_COMMANDS".to_string(),
            Value::String("ON".to_string()),
        );
    } else {
        cache_var.remove("CMAKE_EXPORT_COMPILE_COMMANDS");
    }

    if let Some(toolchain_file) = toolchain_file {
        cache_var.insert(
            "CMAKE_TOOLCHAIN_FILE".to_string(),
            convert_to_cmake_path(toolchain_file)?.into(),
        );
    } else {
        cache_var.remove("CMAKE_TOOLCHAIN_FILE");
    }

    let keys_to_remove: Vec<String> = cache_var
        .keys()
        .filter(|k| k.ends_with("_DIR"))
        .cloned()
        .collect();
    for key in keys_to_remove {
        cache_var.remove(&key);
    }

    for dependency in dependencies {
        let dep_dir = convert_to_cmake_path(&dependency.cmake_pkg_config_dir)?;
        let var_name = format!("{}_DIR", dependency.cmake_pkg_name);
        cache_var.insert(var_name, dep_dir.into());
    }

    Ok(DevPresetBaseUpdateResult {
        binary_dir,
        install_dir,
        base_idx,
        cmake_generator,
    })
}

fn check_preset_conditions(
    preset: &ConfigurePreset,
    build_config: &str,
    cmake_generator_keyword: &str,
) -> bool {
    let has_matching_generator = preset
        .generator
        .as_ref()
        .map(|generator| generator.contains(cmake_generator_keyword))
        .unwrap_or(false);
    if !has_matching_generator {
        return false;
    }
    if preset.binary_dir.is_none() {
        return false;
    }
    let cache_vars = match &preset.cache_variables {
        Some(vars) => vars,
        None => return false,
    };
    if !cache_vars.contains_key("CMAKE_INSTALL_PREFIX") {
        return false;
    }
    let has_matching_configuration_type = cache_vars
        .get("CMAKE_CONFIGURATION_TYPES")
        .and_then(|v| v.as_str())
        .is_some_and(|config_types| {
            config_types
                .split(';')
                .any(|config_type| config_type == build_config)
        });
    let has_matching_build_type = cache_vars
        .get("CMAKE_BUILD_TYPE")
        .and_then(|v| v.as_str())
        .is_some_and(|build_type| build_type == build_config);

    has_matching_configuration_type || has_matching_build_type
}

fn find_based_preset(
    presets: &CMakePresets,
    build_config: &str,
    cmake_generator_keyword: &str,
    priority_preset: Option<usize>,
) -> anyhow::Result<usize> {
    if priority_preset.is_some_and(|idx| {
        presets.configure_presets.get(idx).map_or(false, |p| {
            check_preset_conditions(&p.borrow(), build_config, cmake_generator_keyword)
        })
    }) {
        return Ok(priority_preset.unwrap());
    }
    for (idx, preset_cell) in presets.configure_presets.iter().enumerate() {
        let preset = preset_cell.borrow();
        if check_preset_conditions(&preset, build_config, cmake_generator_keyword) {
            return Ok(idx);
        }
    }
    return err_loc!(
        "No suitable CMake preset found for config '{}'. Required: generator containing '{}', binaryDir, CMAKE_INSTALL_PREFIX, and either CMAKE_CONFIGURATION_TYPES containing '{}' or CMAKE_BUILD_TYPE equal to '{}'",
        build_config,
        cmake_generator_keyword,
        build_config,
        build_config
    );
}

fn cmake_configure(
    repo_config: &CMakeConfigureInfo,
    dependencies: &[CMakeDependencyInfo],
    last_fingerprint: &anyhow::Result<CMakeConfigureFingerprint>,
    vs_dev_env: &crate::vs_dev_env::VsDevEnv,
) -> anyhow::Result<(CMakeConfigureOutput, CMakeConfigureFingerprint)> {
    let repo_path = &repo_config.repo_path;
    let cpp_build_config = repo_config.build_config.as_str();
    let update_result = prepare_dev_preset(
        &repo_path,
        &cpp_build_config,
        &repo_config.cmake_generator_keyword,
        repo_config.conan_toolchain_path.as_deref(),
        dependencies,
    )?;

    let dev_preset_name = &update_result.dev_preset_name;
    let new_injection_repo_names = dependencies
        .iter()
        .map(|d| d.cmake_pkg_name.clone())
        .collect::<BTreeSet<String>>();

    let mut executing_cmake_hasher = update_result.new_cmake_user_presets_hasher;
    repo_config
        .extra_cmake_options
        .hash(&mut executing_cmake_hasher);
    let executing_cmake_hash = executing_cmake_hasher.finish();

    let mut executing_fresh_hasher = DefaultHasher::new();
    new_injection_repo_names.hash(&mut executing_fresh_hasher);
    update_result
        .cmake_generator
        .hash(&mut executing_fresh_hasher);
    let executing_fresh_hash = executing_fresh_hasher.finish();

    let (cmake_hash_changed, fresh_hash_changed, conan_toolchain_changed) = match last_fingerprint {
        Ok(fingerprint) => (
            fingerprint.executing_cmake_hash != executing_cmake_hash,
            fingerprint.executing_fresh_hash != executing_fresh_hash,
            fingerprint.conan_toolchain_hash != repo_config.conan_toolchain_hash,
        ),
        Err(_) => (true, true, true),
    };
    let need_fresh_configure =
        repo_config.need_cmake_fresh || fresh_hash_changed || conan_toolchain_changed;
    let need_cmake_configure = repo_config.need_cmake || need_fresh_configure || cmake_hash_changed;
    let new_fingerprint = CMakeConfigureFingerprint {
        executing_cmake_hash,
        executing_fresh_hash,
        conan_toolchain_hash: repo_config.conan_toolchain_hash,
    };

    if need_cmake_configure {
        let mut configure_cmd = std::process::Command::new("cmake");
        configure_cmd
            .arg(format!("--preset={}", dev_preset_name))
            .arg("-Wno-dev")
            .current_dir(&repo_path);
        if need_fresh_configure {
            configure_cmd.arg("--fresh");
        }
        let extra_args = &repo_config.extra_cmake_options;
        if !extra_args.is_empty() {
            configure_cmd.args(extra_args);
        }
        vs_dev_env.apply_to_ninja_command(&update_result.cmake_generator, &mut configure_cmd)?;
        diag::command_execution::print_command_force(&configure_cmd);
        let status = configure_cmd
            .status()
            .with_loc_context(|| "Failed to execute cmake command")?;
        if !status.success() {
            return err_loc!("CMake configure failed for repo '{}'", repo_path.display());
        }
        info!(
            "CMake configure completed successfully for repo '{}'",
            repo_path.display()
        );
    } else {
        debug!(
            "CMake configure skipped for repo '{}' as no changes were made",
            repo_path.display()
        );
    }

    let cmake_binary_dir = repo_path
        .join(&update_result.binary_dir)
        .absolutize()
        .map(|path| path.to_path_buf())
        .with_loc_context(|| {
            format!(
                "Failed to absolutize CMake binary directory '{}' for repo '{}'",
                update_result.binary_dir.display(),
                repo_path.display()
            )
        })?;
    let install_prefix = repo_path
        .join(&update_result.install_dir)
        .absolutize()
        .map(|path| path.to_path_buf())
        .with_loc_context(|| {
            format!(
                "Failed to absolutize CMake install directory '{}' for repo '{}'",
                update_result.install_dir.display(),
                repo_path.display()
            )
        })?;
    let project_name = parse_cmake_project_name_from_cache(&cmake_binary_dir)?;
    Ok((
        CMakeConfigureOutput {
            cmake_pkg_name: project_name,
            install_prefix,
            cmake_binary_dir,
            cmake_generator: update_result.cmake_generator,
        },
        new_fingerprint,
    ))
}

pub struct CMakeConfigureInfo {
    pub repo_path: PathBuf,
    pub build_config: String,
    pub need_cmake: bool,
    pub need_cmake_fresh: bool,
    pub conan_toolchain_path: Option<PathBuf>,
    pub conan_toolchain_hash: HashType,
    pub extra_cmake_options: Vec<String>,
    pub cmake_generator_keyword: String,
}

pub struct CMakeDependencyInfo {
    pub cmake_pkg_name: String,
    pub cmake_pkg_config_dir: PathBuf,
}

pub struct CMakeConfigureInput {
    pub repo_config: CMakeConfigureInfo,
    pub dependencies: Vec<CMakeDependencyInfo>,
}

#[derive(Serialize, Deserialize)]
pub struct CMakeConfigureFingerprint {
    executing_cmake_hash: HashType,
    /// `toolchain_hash` 的来源比较特殊（只能由专门的 Conan task 计算），
    /// 为避免配置变更漏检，需要在 fingerprint 中单独记录。
    conan_toolchain_hash: HashType,
    executing_fresh_hash: HashType,
}

pub struct CMakeConfigureOutput {
    pub cmake_pkg_name: String,
    pub install_prefix: PathBuf,
    pub cmake_binary_dir: PathBuf,
    pub cmake_generator: String,
}

pub struct CMakeConfigureTask {
    vs_dev_env: Rc<crate::vs_dev_env::VsDevEnv>,
}

impl CMakeConfigureTask {
    pub fn new(vs_dev_env: Rc<crate::vs_dev_env::VsDevEnv>) -> Self {
        Self { vs_dev_env }
    }
}

impl TaskMeta for CMakeConfigureTask {
    fn id(&self) -> &'static str {
        "cmake_configure"
    }
}

impl IncrementalTask for CMakeConfigureTask {
    type Input<'a> = CMakeConfigureInput;
    type Output = CMakeConfigureOutput;
    type Fingerprint = CMakeConfigureFingerprint;

    fn execute<'a>(
        &self,
        input: Self::Input<'a>,
        last_fingerprint: &anyhow::Result<Self::Fingerprint>,
    ) -> anyhow::Result<(Self::Output, Self::Fingerprint)> {
        let (output, new_fingerprint) = cmake_configure(
            &input.repo_config,
            &input.dependencies,
            last_fingerprint,
            &self.vs_dev_env,
        )?;
        Ok((output, new_fingerprint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_repo(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("cmake_presets")
            .join(name)
    }

    fn create_test_preset_repo(binary_dir: &str, install_dir: &str) -> PathBuf {
        let repo_dir = std::env::temp_dir().join(format!(
            "repo_debug_prepare_dev_preset_{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&repo_dir).unwrap();
        let presets = serde_json::json!({
            "version": 7,
            "configurePresets": [
                {
                    "name": "base",
                    "generator": "Visual Studio 17 2022",
                    "binaryDir": binary_dir,
                    "cacheVariables": {
                        "CMAKE_INSTALL_PREFIX": install_dir,
                        "CMAKE_CONFIGURATION_TYPES": "Debug;Release"
                    }
                }
            ]
        });
        fs::write(
            repo_dir.join("CMakePresets.json"),
            serde_json::to_string_pretty(&presets).unwrap(),
        )
        .unwrap();
        repo_dir
    }

    #[test]
    fn find_based_preset_prefers_expanded_user_preset_without_mutating_user_presets() {
        let (mut presets, user_presets) =
            cmake_presets::parse_cmake_presets(&fixture_repo("user_priority")).unwrap();

        let original_user_candidate = user_presets.configure_presets[0].borrow();
        assert_eq!(original_user_candidate.name, "user-candidate");
        assert!(original_user_candidate.generator.is_none());
        assert_eq!(
            original_user_candidate.binary_dir.as_deref(),
            Some("${sourceDir}/build/user")
        );
        assert!(original_user_candidate.cache_variables.is_none());
        drop(original_user_candidate);

        prepare_base_preset_candidates(&mut presets, &user_presets).unwrap();

        assert_eq!(presets.configure_presets[0].borrow().name, "user-candidate");
        assert!(presets
            .configure_presets
            .iter()
            .all(|preset| preset.borrow().name != DEV_PRESET_NAME));

        let user_candidate = presets.configure_presets[0].borrow();
        assert_eq!(
            user_candidate.generator.as_deref(),
            Some("Visual Studio 17 2022")
        );
        assert_eq!(
            user_candidate.binary_dir.as_deref(),
            Some("${sourceDir}/build/user")
        );
        assert!(user_candidate.cache_variables.is_some());
        drop(user_candidate);

        let base_idx = find_based_preset(&presets, "Debug", "Visual Studio", None).unwrap();
        assert_eq!(
            presets.configure_presets[base_idx].borrow().name,
            "user-candidate"
        );

        let original_user_candidate = user_presets.configure_presets[0].borrow();
        assert!(original_user_candidate.generator.is_none());
        assert_eq!(
            original_user_candidate.binary_dir.as_deref(),
            Some("${sourceDir}/build/user")
        );
        assert!(original_user_candidate.cache_variables.is_none());
    }

    #[test]
    fn check_preset_conditions_accepts_matching_cmake_build_type() {
        let mut preset = ConfigurePreset::new_with_name("single-config");
        preset.generator = Some("Visual Studio 17 2022".to_string());
        preset.binary_dir = Some("${sourceDir}/build".to_string());
        preset.cache_variables = Some(BTreeMap::from([
            (
                "CMAKE_INSTALL_PREFIX".to_string(),
                Value::String("${sourceDir}/install".to_string()),
            ),
            (
                "CMAKE_BUILD_TYPE".to_string(),
                Value::String("Debug".to_string()),
            ),
        ]));

        assert!(check_preset_conditions(&preset, "Debug", "Visual Studio"));
        assert!(!check_preset_conditions(
            &preset,
            "Release",
            "Visual Studio"
        ));
    }

    #[test]
    fn check_preset_conditions_accepts_configured_ninja_generator() {
        let mut preset = ConfigurePreset::new_with_name("ninja");
        preset.generator = Some("Ninja Multi-Config".to_string());
        preset.binary_dir = Some("${sourceDir}/build".to_string());
        preset.cache_variables = Some(BTreeMap::from([
            (
                "CMAKE_INSTALL_PREFIX".to_string(),
                Value::String("${sourceDir}/install".to_string()),
            ),
            (
                "CMAKE_CONFIGURATION_TYPES".to_string(),
                Value::String("Debug;Release".to_string()),
            ),
        ]));

        assert!(check_preset_conditions(&preset, "Debug", "Ninja"));
        assert!(!check_preset_conditions(&preset, "Debug", "Visual Studio"));
    }

    #[test]
    fn update_dev_preset_uses_base_directories_without_overriding_them() {
        let (mut presets, mut user_presets) =
            cmake_presets::parse_cmake_presets(&fixture_repo("user_priority")).unwrap();
        let dev_idx = user_presets
            .configure_presets
            .iter()
            .position(|preset| preset.borrow().name == DEV_PRESET_NAME)
            .unwrap();
        user_presets.configure_presets[dev_idx].borrow_mut().inherits =
            Some(vec!["user-candidate".to_string()]);
        prepare_base_preset_candidates(&mut presets, &user_presets).unwrap();

        let result = update_dev_preset_from_base(
            &presets,
            &mut user_presets,
            dev_idx,
            "Debug",
            "Visual Studio",
            None,
            &[],
        )
        .unwrap();

        assert_eq!(result.binary_dir, "${sourceDir}/build/user");
        assert_eq!(result.install_dir, "${sourceDir}/install");
        let dev_preset = user_presets.configure_presets[dev_idx].borrow();
        assert!(dev_preset.binary_dir.is_none());
        assert!(!dev_preset
            .cache_variables
            .as_ref()
            .unwrap()
            .contains_key("CMAKE_INSTALL_PREFIX"));
    }

    #[test]
    fn update_dev_preset_toggles_native_compile_commands_export_with_generator() {
        let (mut presets, mut user_presets) =
            cmake_presets::parse_cmake_presets(&fixture_repo("without_user")).unwrap();
        presets.configure_presets[0].borrow_mut().generator = Some("Ninja".to_string());
        let mut dev_preset = ConfigurePreset::new_with_name(DEV_PRESET_NAME);
        dev_preset.inherits = Some(vec!["base".to_string()]);
        user_presets
            .configure_presets
            .push(RefCell::new(dev_preset));
        let dev_idx = user_presets.configure_presets.len() - 1;
        prepare_base_preset_candidates(&mut presets, &user_presets).unwrap();

        update_dev_preset_from_base(
            &presets,
            &mut user_presets,
            dev_idx,
            "Debug",
            "Ninja",
            None,
            &[],
        )
        .unwrap();

        assert_eq!(
            user_presets.configure_presets[dev_idx]
                .borrow()
                .cache_variables
                .as_ref()
                .unwrap()
                .get("CMAKE_EXPORT_COMPILE_COMMANDS"),
            Some(&Value::String("ON".to_string()))
        );

        presets.configure_presets[0].borrow_mut().generator =
            Some("Visual Studio 17 2022".to_string());
        update_dev_preset_from_base(
            &presets,
            &mut user_presets,
            dev_idx,
            "Debug",
            "Visual Studio",
            None,
            &[],
        )
        .unwrap();

        assert!(!user_presets.configure_presets[dev_idx]
            .borrow()
            .cache_variables
            .as_ref()
            .unwrap()
            .contains_key("CMAKE_EXPORT_COMPILE_COMMANDS"));
    }

    #[test]
    fn prepare_dev_preset_returns_repo_relative_directories() {
        let repo_dir = create_test_preset_repo(
            "${sourceDir}/out/build/debug",
            "${sourceDir}/out/install/debug",
        );

        let result = prepare_dev_preset(&repo_dir, "Debug", "Visual Studio", None, &[]).unwrap();

        assert_eq!(result.binary_dir, PathBuf::from("out/build/debug"));
        assert_eq!(result.install_dir, PathBuf::from("out/install/debug"));
        assert!(repo_dir.join("CMakeUserPresets.json").is_file());
        fs::remove_dir_all(repo_dir).unwrap();
    }

    #[test]
    fn resolves_plain_relative_preset_path_from_repo_root() {
        let repo_dir = std::env::temp_dir().join(format!(
            "repo_debug_relative_preset_path_{}",
            uuid::Uuid::new_v4()
        ));

        let resolved =
            resolve_repo_relative_preset_path(&repo_dir, "out/../build/debug", "binaryDir")
                .unwrap();

        assert_eq!(resolved, PathBuf::from("build/debug"));
    }

    #[test]
    fn resolves_same_volume_external_absolute_path_with_parent_components() {
        let unique = uuid::Uuid::new_v4();
        let parent_dir = std::env::temp_dir().join(format!("repo_debug_path_parent_{unique}"));
        let repo_dir = parent_dir.join("repo");
        let external_dir = parent_dir.join("external");

        let resolved = resolve_repo_relative_preset_path(
            &repo_dir,
            external_dir.to_str().unwrap(),
            "binaryDir",
        )
        .unwrap();

        assert_eq!(resolved, PathBuf::from("..").join("external"));
    }

    #[cfg(windows)]
    #[test]
    fn rejects_cross_volume_absolute_preset_path() {
        let error = resolve_repo_relative_preset_path(
            Path::new(r"C:\repo"),
            r"D:\external\build",
            "binaryDir",
        )
        .err()
        .expect("cross-volume path should fail");

        let message = format!("{error:?}");
        assert!(message.contains("Failed to make preset field 'binaryDir' relative to repo root"));
        assert!(message.contains(r"D:\external\build"));
        assert!(message.contains(r"C:\repo"));
    }

    #[test]
    fn unsupported_preset_macros_fail_before_saving_user_presets() {
        for (binary_dir, install_dir, expected_message) in [
            (
                "$env{BUILD_ROOT}/build",
                "${sourceDir}/install",
                "Environment variable macros are not supported",
            ),
            (
                "$penv{BUILD_ROOT}/build",
                "${sourceDir}/install",
                "Environment variable macros are not supported",
            ),
            (
                "${sourceDir}/build",
                "${sourceParentDir}/install",
                "Unsupported unresolved macro",
            ),
        ] {
            let repo_dir = create_test_preset_repo(binary_dir, install_dir);

            let error = prepare_dev_preset(&repo_dir, "Debug", "Visual Studio", None, &[])
                .err()
                .expect("unsupported macro should fail");

            assert!(format!("{error:?}").contains(expected_message));
            assert!(!repo_dir.join("CMakeUserPresets.json").exists());
            fs::remove_dir_all(repo_dir).unwrap();
        }
    }

    #[test]
    fn path_resolution_failure_does_not_modify_existing_user_presets() {
        let repo_dir =
            create_test_preset_repo("$env{BUILD_ROOT}/build", "${sourceDir}/install");
        let user_presets_path = repo_dir.join("CMakeUserPresets.json");
        let original_content = r#"{
  "version": 7,
  "configurePresets": [
    {
      "name": "multi_repo_dev_config",
      "inherits": "base"
    }
  ]
}
"#;
        fs::write(&user_presets_path, original_content).unwrap();

        assert!(
            prepare_dev_preset(&repo_dir, "Debug", "Visual Studio", None, &[]).is_err()
        );

        assert_eq!(fs::read_to_string(&user_presets_path).unwrap(), original_content);
        fs::remove_dir_all(repo_dir).unwrap();
    }

    #[test]
    fn reads_project_name_from_prepared_binary_directory() {
        let repo_dir = std::env::temp_dir().join(format!(
            "repo_debug_cmake_configure_{}",
            uuid::Uuid::new_v4()
        ));
        let binary_dir = repo_dir.join("out").join("build").join("debug");
        fs::create_dir_all(&binary_dir).unwrap();
        fs::write(
            binary_dir.join("CMakeCache.txt"),
            "CMAKE_PROJECT_NAME:STATIC=fixture_project\n",
        )
        .unwrap();

        let relative = resolve_repo_relative_preset_path(
            &repo_dir,
            "${sourceDir}/out/build/debug",
            "binaryDir",
        )
        .unwrap();
        let resolved = repo_dir.join(relative);

        assert_eq!(resolved, binary_dir);
        assert_eq!(
            parse_cmake_project_name_from_cache(&resolved).unwrap(),
            "fixture_project"
        );
        fs::remove_dir_all(repo_dir).unwrap();
    }
}
