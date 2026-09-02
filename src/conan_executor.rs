use crate::conanfile;
use crate::task::{IncrementalTask, TaskMeta};
use crate::HashType;
use diag_trace::command_execution::print_command_force;
use diag_trace::{self as diag, anyhow_loc, err_loc, LocContextExt};
use log::info;
use serde_json::Value;
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    path::PathBuf,
    process::Command,
};

pub struct ConanInstallInput {
    pub repo_dir: PathBuf,
    pub extra_conan_options: Vec<String>,
    pub conan_output_folder: PathBuf,
    pub need_install: bool,
    pub need_update: bool,
}

pub struct ConanInstallOutput {
    pub conan_toolchain_path: Option<PathBuf>,
    pub new_conan_toolchain_hash: Option<HashType>,
}

pub struct ConanInstallTask {}

fn extract_generators_folder(stdout: &[u8]) -> Option<PathBuf> {
    let text = String::from_utf8_lossy(stdout);
    let json = serde_json::from_str::<Value>(&text).ok()?;
    json.get("graph")?
        .get("nodes")?
        .get("0")?
        .get("generators_folder")?
        .as_str()
        .map(|s| PathBuf::from(s))
}

pub fn conan_install(
    install_input: &ConanInstallInput,
    last_fingerprint: &anyhow::Result<HashType>,
) -> anyhow::Result<(ConanInstallOutput, HashType)> {
    let repo_dir = &install_input.repo_dir;
    let out_dir = &install_input.conan_output_folder;

    // 通过 conanfile.get_conanfile_content 获得 conanfile 路径，并计算hash
    let conanfile_content = conanfile::get_conanfile_content(repo_dir)?;
    let mut hasher = DefaultHasher::new();
    conanfile_content.hash(&mut hasher);
    install_input.extra_conan_options.hash(&mut hasher);
    let new_conanfile_hash = hasher.finish();

    let need_update = match last_fingerprint {
        Ok(fingerprint) => new_conanfile_hash != *fingerprint,
        Err(_) => true,
    } || install_input.need_update;

    let need_install = install_input.need_install || need_update;

    let (new_conan_toolchain_hash, conan_toolchain_path) = if need_install {
        let mut command = Command::new("conan");
        command
            .arg("install")
            .arg(".")
            .arg("--output-folder")
            .arg(out_dir)
            .arg("--format=json")
            .current_dir(&repo_dir);

        if install_input.extra_conan_options.len() > 0 {
            command.args(&install_input.extra_conan_options);
        }

        if need_update {
            command.arg("--update");
        }
        print_command_force(&command);
        let cmd_output = diag::command_execution::execute_and_print_output_force(&mut command)
            .with_loc_context(|| "conan install 命令执行失败")?;
        let generators_folder = extract_generators_folder(&cmd_output.stdout)
            .ok_or_else(|| anyhow_loc!("Failed to extract generators folder from conan output"))?;

        let conan_toolchain_path = generators_folder.join("conan_toolchain.cmake");
        info!("Conan toolchain path: {}", &conan_toolchain_path.display());

        if conan_toolchain_path.exists() {
            let content = fs::read_to_string(&conan_toolchain_path).with_loc_context(|| {
                format!(
                    "Failed to read conan toolchain file for hash calculation: {}",
                    conan_toolchain_path.display()
                )
            })?;
            let mut hasher = DefaultHasher::new();
            content.hash(&mut hasher);
            (Some(hasher.finish()), Some(conan_toolchain_path))
        } else {
            return err_loc!(
                "Conan toolchain file not found at expected path: {:?}",
                conan_toolchain_path
            );
        }
    } else {
        (None, None)
    };

    Ok((
        ConanInstallOutput {
            conan_toolchain_path,
            new_conan_toolchain_hash: new_conan_toolchain_hash,
        },
        new_conanfile_hash,
    ))
}

impl TaskMeta for ConanInstallTask {
    fn id(&self) -> &'static str {
        "conan_install"
    }
}

impl IncrementalTask for ConanInstallTask {
    type Input<'a> = ConanInstallInput;
    type Output = ConanInstallOutput;
    type Fingerprint = HashType;

    fn execute<'a>(
        &self,
        input: Self::Input<'a>,
        last_fingerprint: &anyhow::Result<Self::Fingerprint>,
    ) -> anyhow::Result<(Self::Output, Self::Fingerprint)> {
        conan_install(&input, last_fingerprint)
    }
}
