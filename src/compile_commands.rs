use crate::sln_merge_kit;
use crate::task::{IncrementalTask, TaskMeta};
use crate::HashType;
use crate::SharedError;
use diag_trace::LocContextExt;
use log::debug;
use path_absolutize::Absolutize;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::fs;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::{Component, Path, PathBuf},
};

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CompileCommandsConfig {
    pub options: Vec<String>,
    pub enabled: bool,
}

pub struct CompileCommandsInput<'a> {
    pub compile_commands_config: &'a CompileCommandsConfig,
    pub sln_path: Option<PathBuf>,
}

impl<'a> CompileCommandsInput<'a> {
    fn is_enabled(&self) -> bool {
        self.compile_commands_config.enabled && self.sln_path.is_some()
    }

    fn get_options(&self) -> &Vec<String> {
        self.compile_commands_config.options.as_ref()
    }
}

#[derive(Clone, Copy, Serialize, Default)]
#[serde(default)]
pub struct CompileCommandsFingerprint {
    #[serde(alias = "vcxproj_hash")]
    compile_commands_input_hash: HashType,
    compile_commands_output_hash: HashType,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CompileCommandsFingerprintSerde {
    Legacy(HashType),
    Structured {
        #[serde(alias = "vcxproj_hash")]
        compile_commands_input_hash: HashType,
        compile_commands_output_hash: HashType,
    },
}

impl<'de> Deserialize<'de> for CompileCommandsFingerprint {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match CompileCommandsFingerprintSerde::deserialize(deserializer)? {
            CompileCommandsFingerprintSerde::Legacy(legacy_input_hash) => Ok(Self {
                compile_commands_input_hash: legacy_input_hash,
                compile_commands_output_hash: 0,
            }),
            CompileCommandsFingerprintSerde::Structured {
                compile_commands_input_hash,
                compile_commands_output_hash,
            } => Ok(Self {
                compile_commands_input_hash,
                compile_commands_output_hash,
            }),
        }
    }
}

fn calculate_compile_commands_input_hash(
    compile_commands_input: &CompileCommandsInput,
) -> anyhow::Result<HashType> {
    let mut hasher = DefaultHasher::new();
    compile_commands_input.get_options().hash(&mut hasher);
    let sln_path = compile_commands_input.sln_path.as_ref().ok_or_else(|| {
        diag_trace::anyhow_loc!("Solution path is unavailable for compile commands generation")
    })?;
    sln_merge_kit::get_vcxproj_hash(&mut hasher, sln_path)?;
    Ok(hasher.finish())
}

fn calculate_file_hash(path: &Path) -> anyhow::Result<HashType> {
    let mut hasher = DefaultHasher::new();
    fs::read(path)
        .with_loc_context(|| format!("Failed to read file: {}", path.display()))?
        .hash(&mut hasher);
    Ok(hasher.finish())
}

fn read_json_file_to_string(path: &Path) -> anyhow::Result<String> {
    let bytes =
        fs::read(path).with_loc_context(|| format!("Failed to read file: {}", path.display()))?;

    let detection = chardet::detect(&bytes);
    let encoding_name = detection.0;
    let (cow, _encoding_used, had_errors) =
        encoding_rs::Encoding::for_label(encoding_name.as_bytes())
            .unwrap_or(encoding_rs::UTF_8)
            .decode(&bytes);

    if had_errors {
        eprintln!("Warning: encoding conversion had errors");
    }

    Ok(cow.into_owned())
}

fn ensure_compile_commands_not_empty(path: &Path) -> anyhow::Result<()> {
    let content = read_json_file_to_string(path)?;
    let content = content.strip_prefix('\u{feff}').unwrap_or(&content);
    let compile_commands: Vec<Value> = serde_json::from_str(content).with_loc_context(|| {
        format!("Failed to parse compile_commands JSON: {}", path.display())
    })?;
    if compile_commands.is_empty() {
        return Err(diag_trace::anyhow_loc!(
            "compile_commands JSON is empty: {}",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn publish_compile_commands(
    repo_dir: &Path,
    cmake_binary_dir: &Path,
) -> anyhow::Result<PathBuf> {
    let repo_dir = repo_dir
        .absolutize()
        .map(|path| path.to_path_buf())
        .with_loc_context(|| format!("Failed to absolutize repo dir: {}", repo_dir.display()))?;
    let cmake_binary_dir = cmake_binary_dir
        .absolutize()
        .map(|path| path.to_path_buf())
        .with_loc_context(|| {
            format!(
                "Failed to absolutize CMake binary dir: {}",
                cmake_binary_dir.display()
            )
        })?;
    let relative_binary_dir = cmake_binary_dir
        .strip_prefix(&repo_dir)
        .with_loc_context(|| {
            format!(
                "CMake binary dir '{}' is outside repo '{}'",
                cmake_binary_dir.display(),
                repo_dir.display()
            )
        })?;
    let publish_dir = match relative_binary_dir.components().next() {
        Some(Component::Normal(first_component)) => repo_dir.join(first_component),
        None => repo_dir.clone(),
        Some(component) => {
            return diag_trace::err_loc!(
                "Invalid first component '{:?}' in CMake binary dir '{}'",
                component,
                cmake_binary_dir.display()
            );
        }
    };

    let source_path = cmake_binary_dir.join("compile_commands.json");
    let publish_path = publish_dir.join("compile_commands.json");
    ensure_compile_commands_not_empty(&source_path)?;
    if source_path == publish_path {
        return Ok(publish_path);
    }

    fs::copy(&source_path, &publish_path).with_loc_context(|| {
        format!(
            "Failed to publish compile_commands from '{}' to '{}'",
            source_path.display(),
            publish_path.display()
        )
    })?;
    Ok(publish_path)
}

fn should_skip_compile_commands_rebuild(
    last_fingerprint: CompileCommandsFingerprint,
    current_input_hash: HashType,
    current_output_hash: anyhow::Result<HashType>,
) -> bool {
    match current_output_hash {
        Ok(current_output_hash) => {
            last_fingerprint.compile_commands_input_hash == current_input_hash
                && last_fingerprint.compile_commands_output_hash == current_output_hash
        }
        Err(_) => false,
    }
}

fn make_compile_commands(
    clang_build_script_path: &Result<PathBuf, SharedError>,
    compile_commands_input: &CompileCommandsInput,
    last_fingerprint: &anyhow::Result<CompileCommandsFingerprint>,
) -> anyhow::Result<CompileCommandsFingerprint> {
    let last_fingerprint = last_fingerprint.as_ref().copied().unwrap_or_default();
    if !compile_commands_input.is_enabled() {
        return Ok(last_fingerprint);
    }

    let clang_build_script_path = clang_build_script_path
        .as_ref()
        .map(|p| p.as_path())
        .map_err(|err| anyhow::Error::new(err.clone()))?;

    let sln_path = compile_commands_input.sln_path.as_ref().ok_or_else(|| {
        diag_trace::anyhow_loc!("Solution path is unavailable for compile commands generation")
    })?;
    let current_input_hash = calculate_compile_commands_input_hash(compile_commands_input)?;

    let sln_full_path = sln_path
        .absolutize()
        .map(|path| path.to_path_buf())
        .unwrap_or_else(|_| sln_path.to_path_buf());
    let sln_work_dir = sln_full_path.parent().unwrap_or_else(|| Path::new("."));
    let compile_commands_json_path = sln_work_dir.join("compile_commands.json");

    let current_output_hash = calculate_file_hash(&compile_commands_json_path);
    if should_skip_compile_commands_rebuild(
        last_fingerprint,
        current_input_hash,
        current_output_hash,
    ) {
        debug!("No changes in compile_commands fingerprint, skipping clang build script.");
        return Ok(last_fingerprint);
    } else {
        debug!("Compile_commands fingerprint mismatch, triggering rebuild.");
    }

    let mut cmd = vs_clang_power_tools::create_clang_build_command(
        clang_build_script_path,
        &sln_full_path,
        compile_commands_input.get_options(),
    );

    diag_trace::command_execution::execute_and_print_output_if_debug(&mut cmd)
        .with_loc_context(|| "Clang build script failed")?;

    ensure_compile_commands_not_empty(&compile_commands_json_path)?;
    let new_output_hash = calculate_file_hash(&compile_commands_json_path)?;
    Ok(CompileCommandsFingerprint {
        compile_commands_input_hash: current_input_hash,
        compile_commands_output_hash: new_output_hash,
    })
}

pub struct CompileCommandsTask {
    clang_build_script_path: Result<PathBuf, SharedError>,
}

impl CompileCommandsTask {
    pub fn new() -> Self {
        Self {
            clang_build_script_path: vs_clang_power_tools::find_clang_build_script_path()
                .map_err(anyhow::Error::into_boxed_dyn_error)
                .map_err(SharedError::from),
        }
    }
}

impl TaskMeta for CompileCommandsTask {
    fn id(&self) -> &'static str {
        "compile_commands"
    }
}

impl IncrementalTask for CompileCommandsTask {
    type Input<'a> = CompileCommandsInput<'a>;
    type Output = ();
    type Fingerprint = CompileCommandsFingerprint;

    fn execute<'a>(
        &self,
        input: Self::Input<'a>,
        last_fingerprint: &anyhow::Result<Self::Fingerprint>,
    ) -> anyhow::Result<(Self::Output, Self::Fingerprint)> {
        let output =
            make_compile_commands(&self.clang_build_script_path, &input, last_fingerprint)?;
        Ok(((), output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "repo_debug_compile_commands_{}_{}",
            name,
            uuid::Uuid::new_v4()
        ))
    }

    fn write_compile_commands(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn compile_commands_is_disabled_without_solution_path() {
        let config = CompileCommandsConfig {
            options: Vec::new(),
            enabled: true,
        };
        let input = CompileCommandsInput {
            compile_commands_config: &config,
            sln_path: None,
        };

        assert!(!input.is_enabled());
    }

    #[test]
    fn skip_when_input_and_output_hash_match() {
        let last = CompileCommandsFingerprint {
            compile_commands_input_hash: 11,
            compile_commands_output_hash: 22,
        };
        assert!(should_skip_compile_commands_rebuild(last, 11, Ok(22)));
    }

    #[test]
    fn rebuild_when_output_file_missing_or_unreadable() {
        let last = CompileCommandsFingerprint {
            compile_commands_input_hash: 11,
            compile_commands_output_hash: 22,
        };
        let output_error = anyhow::anyhow!("missing compile_commands.json");
        assert!(!should_skip_compile_commands_rebuild(
            last,
            11,
            Err(output_error)
        ));
    }

    #[test]
    fn rebuild_when_output_hash_changed() {
        let last = CompileCommandsFingerprint {
            compile_commands_input_hash: 11,
            compile_commands_output_hash: 22,
        };
        assert!(!should_skip_compile_commands_rebuild(last, 11, Ok(33)));
    }

    #[test]
    fn rebuild_when_input_hash_changed() {
        let last = CompileCommandsFingerprint {
            compile_commands_input_hash: 11,
            compile_commands_output_hash: 22,
        };
        assert!(!should_skip_compile_commands_rebuild(last, 44, Ok(22)));
    }

    #[test]
    fn publish_compile_commands_to_first_binary_dir_component() {
        let repo_dir = test_dir("nested");
        let binary_dir = repo_dir.join("out").join("build").join("debug");
        let source_path = binary_dir.join("compile_commands.json");
        write_compile_commands(&source_path, r#"[{"file":"main.cpp"}]"#);

        let publish_path = publish_compile_commands(&repo_dir, &binary_dir).unwrap();

        assert_eq!(publish_path, repo_dir.join("out/compile_commands.json"));
        assert_eq!(
            fs::read_to_string(&publish_path).unwrap(),
            r#"[{"file":"main.cpp"}]"#
        );
        fs::remove_dir_all(repo_dir).unwrap();
    }

    #[test]
    fn publish_compile_commands_skips_copy_when_source_is_already_published() {
        let repo_dir = test_dir("same_path");
        let binary_dir = repo_dir.join("build");
        let source_path = binary_dir.join("compile_commands.json");
        write_compile_commands(&source_path, r#"[{"file":"main.cpp"}]"#);

        let publish_path = publish_compile_commands(&repo_dir, &binary_dir).unwrap();

        assert_eq!(publish_path, source_path);
        fs::remove_dir_all(repo_dir).unwrap();
    }

    #[test]
    fn publish_compile_commands_rejects_empty_database() {
        let repo_dir = test_dir("empty");
        let binary_dir = repo_dir.join("out").join("build");
        let source_path = binary_dir.join("compile_commands.json");
        write_compile_commands(&source_path, "[]");

        let error = publish_compile_commands(&repo_dir, &binary_dir).unwrap_err();

        assert!(format!("{error:#}").contains("compile_commands JSON is empty"));
        fs::remove_dir_all(repo_dir).unwrap();
    }
}
