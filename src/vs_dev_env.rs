use diag_trace::{anyhow_loc, err_loc, LocContextExt};
use std::cell::{Ref, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

type Environment = BTreeMap<OsString, OsString>;

pub struct VsDevEnv {
    resolved_env: RefCell<Option<Environment>>,
}

impl VsDevEnv {
    pub fn new() -> Self {
        Self {
            resolved_env: RefCell::new(None),
        }
    }

    pub fn apply_to_ninja_command(
        &self,
        cmake_generator: &str,
        command: &mut Command,
    ) -> anyhow::Result<()> {
        if !cmake_generator.contains("Ninja") {
            return Ok(());
        }

        let resolved_env = self
            .resolved_env()
            .with_loc_context(|| "Failed to resolve the MSVC environment for Ninja")?;
        command.envs(resolved_env.iter());
        Ok(())
    }

    fn resolved_env(&self) -> anyhow::Result<Ref<'_, Environment>> {
        self.resolved_env_with(resolve_vs_dev_env)
    }

    fn resolved_env_with(
        &self,
        resolver: impl FnOnce() -> anyhow::Result<Environment>,
    ) -> anyhow::Result<Ref<'_, Environment>> {
        if self.resolved_env.borrow().is_none() {
            let resolved_env = resolver().with_loc_context(|| {
                "Failed to generate the Visual Studio developer environment"
            })?;
            *self.resolved_env.borrow_mut() = Some(resolved_env);
        }

        Ok(Ref::map(self.resolved_env.borrow(), |env| {
            env.as_ref()
                .expect("Visual Studio developer environment should be cached")
        }))
    }

    #[cfg(test)]
    fn with_resolved_env(resolved_env: Environment) -> Self {
        Self {
            resolved_env: RefCell::new(Some(resolved_env)),
        }
    }
}

fn resolve_vs_dev_env() -> anyhow::Result<Environment> {
    let base_env = std::env::vars_os().collect::<Environment>();
    let host_arch = detect_host_arch(&base_env)?;
    let vswhere_path = get_vswhere_path(&base_env)?;
    let vs_install_path = get_vs_install_path(&vswhere_path)?;
    let vs_dev_cmd = vs_install_path
        .join("Common7")
        .join("Tools")
        .join("VsDevCmd.bat");
    if !vs_dev_cmd.is_file() {
        return err_loc!("VsDevCmd.bat not found: '{}'", vs_dev_cmd.display());
    }

    let command_line = format!(
        r#"call "{}" -arch=x64 -host_arch={} && set"#,
        vs_dev_cmd.display(),
        host_arch
    );
    let output = Command::new("cmd.exe")
        .args(["/D", "/U", "/S", "/C"])
        .raw_arg(command_line)
        .envs(base_env.iter())
        .output()
        .with_loc_context(|| format!("Failed to execute '{}'", vs_dev_cmd.display()))?;
    if !output.status.success() {
        let stderr = decode_utf16_le(&output.stderr)
            .unwrap_or_else(|_| String::from_utf8_lossy(&output.stderr).into_owned());
        return err_loc!(
            "VsDevCmd.bat failed with code {}: {}",
            output.status.code().unwrap_or(-1),
            stderr.trim()
        );
    }

    let env_dump = decode_utf16_le(&output.stdout)
        .with_loc_context(|| "Failed to decode VsDevCmd.bat environment output")?;
    Ok(parse_env_dump(&env_dump, &base_env))
}

fn detect_host_arch(base_env: &Environment) -> anyhow::Result<&'static str> {
    let native_arch = get_env_ignore_ascii_case(base_env, "PROCESSOR_ARCHITEW6432")
        .or_else(|| get_env_ignore_ascii_case(base_env, "PROCESSOR_ARCHITECTURE"))
        .ok_or_else(|| anyhow_loc!("Unable to detect the native Windows architecture"))?;
    let native_arch = native_arch.to_string_lossy();
    if native_arch.eq_ignore_ascii_case("amd64") || native_arch.eq_ignore_ascii_case("x64") {
        return Ok("x64");
    }
    if native_arch.eq_ignore_ascii_case("x86") {
        return Ok("x86");
    }
    if native_arch.eq_ignore_ascii_case("arm64") {
        return Ok("x64");
    }
    return err_loc!("Unsupported native Windows architecture: {}", native_arch);
}

fn get_vswhere_path(base_env: &Environment) -> anyhow::Result<PathBuf> {
    for env_name in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base_dir) = get_env_ignore_ascii_case(base_env, env_name) {
            let candidate = PathBuf::from(base_dir)
                .join("Microsoft Visual Studio")
                .join("Installer")
                .join("vswhere.exe");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    return err_loc!("vswhere.exe not found");
}

fn get_vs_install_path(vswhere_path: &Path) -> anyhow::Result<PathBuf> {
    let output = Command::new(vswhere_path)
        .args([
            "-latest",
            "-products",
            "*",
            "-requires",
            "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
            "-property",
            "installationPath",
            "-utf8",
        ])
        .output()
        .with_loc_context(|| format!("Failed to execute '{}'", vswhere_path.display()))?;
    if !output.status.success() {
        return err_loc!(
            "vswhere.exe failed with code {}: {}",
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let installation_path = String::from_utf8(output.stdout)
        .with_loc_context(|| "Failed to decode vswhere.exe output as UTF-8")?;
    let installation_path = installation_path.trim();
    if installation_path.is_empty() {
        return err_loc!("Visual Studio with C++ tools was not found");
    }
    Ok(PathBuf::from(installation_path))
}

fn decode_utf16_le(bytes: &[u8]) -> anyhow::Result<String> {
    if bytes.len() % 2 != 0 {
        return err_loc!("UTF-16LE output has an odd byte length: {}", bytes.len());
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    String::from_utf16(&units)
        .map(|value| value.trim_start_matches('\u{feff}').to_string())
        .with_loc_context(|| "Invalid UTF-16LE output")
}

fn parse_env_dump(env_dump: &str, base_env: &Environment) -> Environment {
    let mut resolved_env = base_env.clone();
    for line in env_dump.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        insert_env_ignore_ascii_case(
            &mut resolved_env,
            OsString::from(name),
            OsString::from(value),
        );
    }

    if let Some(path) = get_env_ignore_ascii_case(&resolved_env, "PATH").cloned() {
        insert_env_ignore_ascii_case(
            &mut resolved_env,
            OsString::from("PATH"),
            prioritize_visual_studio_path_entries(&path),
        );
    }
    resolved_env
}

fn prioritize_visual_studio_path_entries(path_value: &OsStr) -> OsString {
    let mut visual_studio_entries = Vec::new();
    let mut other_entries = Vec::new();
    let mut seen_entries = HashSet::new();
    for entry in std::env::split_paths(path_value) {
        let normalized_entry = entry.to_string_lossy().trim().to_string();
        if normalized_entry.is_empty() {
            continue;
        }
        let entry_key = normalized_entry.replace('/', "\\").to_ascii_lowercase();
        if !seen_entries.insert(entry_key.clone()) {
            continue;
        }
        if entry_key.contains("microsoft visual studio") {
            visual_studio_entries.push(PathBuf::from(normalized_entry));
        } else {
            other_entries.push(PathBuf::from(normalized_entry));
        }
    }
    visual_studio_entries.extend(other_entries);
    std::env::join_paths(visual_studio_entries).unwrap_or_else(|_| path_value.to_os_string())
}

fn get_env_ignore_ascii_case<'a>(environment: &'a Environment, name: &str) -> Option<&'a OsString> {
    environment
        .iter()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
        .map(|(_, value)| value)
}

fn insert_env_ignore_ascii_case(environment: &mut Environment, name: OsString, value: OsString) {
    let existing_name = environment
        .keys()
        .find(|key| {
            key.to_string_lossy()
                .eq_ignore_ascii_case(&name.to_string_lossy())
        })
        .cloned();
    if let Some(existing_name) = existing_name {
        environment.remove(&existing_name);
    }
    environment.insert(name, value);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn environment(entries: &[(&str, &str)]) -> Environment {
        entries
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn detects_supported_host_architectures() {
        for (native_arch, expected) in [
            ("AMD64", "x64"),
            ("x64", "x64"),
            ("x86", "x86"),
            ("ARM64", "x64"),
        ] {
            let env = environment(&[("PROCESSOR_ARCHITECTURE", native_arch)]);
            assert_eq!(detect_host_arch(&env).unwrap(), expected);
        }
    }

    #[test]
    fn prefers_wow64_native_architecture() {
        let env = environment(&[
            ("PROCESSOR_ARCHITECTURE", "x86"),
            ("PROCESSOR_ARCHITEW6432", "AMD64"),
        ]);
        assert_eq!(detect_host_arch(&env).unwrap(), "x64");
    }

    #[test]
    fn rejects_missing_or_unsupported_host_architecture() {
        assert!(detect_host_arch(&Environment::new()).is_err());
        let env = environment(&[("PROCESSOR_ARCHITECTURE", "mips")]);
        assert!(detect_host_arch(&env).is_err());
    }

    #[test]
    fn parses_environment_dump_and_preserves_equals_in_values() {
        let base_env = environment(&[("KEEP", "base"), ("OVERRIDE", "old")]);
        let parsed = parse_env_dump(
            "VsDevCmd output\r\nOVERRIDE=new\r\nVALUE=a=b=c\r\nINVALID\r\n",
            &base_env,
        );
        assert_eq!(get_env_ignore_ascii_case(&parsed, "KEEP").unwrap(), "base");
        assert_eq!(
            get_env_ignore_ascii_case(&parsed, "OVERRIDE").unwrap(),
            "new"
        );
        assert_eq!(
            get_env_ignore_ascii_case(&parsed, "VALUE").unwrap(),
            "a=b=c"
        );
    }

    #[test]
    fn prioritizes_and_deduplicates_visual_studio_path_entries() {
        let separator = if cfg!(windows) { ";" } else { ":" };
        let input = OsString::from(format!(
            r"C:\Tools{separator}C:\Microsoft Visual Studio\VC\bin{separator}c:\tools{separator}C:\Other"
        ));
        let prioritized = prioritize_visual_studio_path_entries(&input);
        let entries = std::env::split_paths(&prioritized)
            .map(|entry| entry.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![
                r"C:\Microsoft Visual Studio\VC\bin",
                r"C:\Tools",
                r"C:\Other"
            ]
        );
    }

    #[test]
    fn applies_cached_environment_only_to_ninja_commands() {
        let vs_dev_env = VsDevEnv::with_resolved_env(environment(&[("VS_TEST_ENV", "ready")]));
        let mut ninja_command = Command::new("cmake");
        vs_dev_env
            .apply_to_ninja_command("Ninja Multi-Config", &mut ninja_command)
            .unwrap();
        assert!(ninja_command
            .get_envs()
            .any(|(name, value)| { name == "VS_TEST_ENV" && value == Some(OsStr::new("ready")) }));

        let mut visual_studio_command = Command::new("cmake");
        vs_dev_env
            .apply_to_ninja_command("Visual Studio 17 2022", &mut visual_studio_command)
            .unwrap();
        assert!(visual_studio_command.get_envs().next().is_none());
    }

    #[test]
    fn resolves_environment_once_and_does_not_cache_failures() {
        let vs_dev_env = VsDevEnv::new();
        let resolve_count = Cell::new(0);
        {
            let resolved_env = vs_dev_env
                .resolved_env_with(|| {
                    resolve_count.set(resolve_count.get() + 1);
                    Ok(environment(&[("READY", "1")]))
                })
                .unwrap();
            assert_eq!(
                get_env_ignore_ascii_case(&resolved_env, "READY").unwrap(),
                "1"
            );
        }
        let _resolved_env = vs_dev_env
            .resolved_env_with(|| {
                resolve_count.set(resolve_count.get() + 1);
                Ok(Environment::new())
            })
            .unwrap();
        assert_eq!(resolve_count.get(), 1);

        let failing_env = VsDevEnv::new();
        assert!(failing_env
            .resolved_env_with(|| err_loc!("expected resolver failure"))
            .is_err());
        let recovered_env = failing_env
            .resolved_env_with(|| Ok(environment(&[("RECOVERED", "1")])))
            .unwrap();
        assert_eq!(
            get_env_ignore_ascii_case(&recovered_env, "RECOVERED").unwrap(),
            "1"
        );
    }
}
