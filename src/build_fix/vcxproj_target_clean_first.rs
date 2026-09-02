use crate::build_fix::error_patterns::{lnk_obj_vcxproj_regex, CAP_OBJ, CAP_VCXPROJ};
use crate::build_fix::{CommandErrorFixer, FixActionResult, FixContext};
use crate::SharedError;
use diag_trace::{self as diag, anyhow_loc, err_loc, LocContextExt};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct VcxprojTargetCleanFirstFixer {
    seen_pairs: HashSet<(String, String)>,
    ms_build_path: Result<PathBuf, SharedError>,
}

impl VcxprojTargetCleanFirstFixer {
    pub fn new(ms_build_path: &Result<PathBuf, SharedError>) -> anyhow::Result<Self> {
        let _ = lnk_obj_vcxproj_regex()?;
        Ok(Self {
            seen_pairs: HashSet::new(),
            ms_build_path: ms_build_path.clone(),
        })
    }

    fn parse_target(vcxproj_str: &str) -> anyhow::Result<String> {
        let vcxproj_path = Path::new(vcxproj_str);
        vcxproj_path
            .file_stem()
            .ok_or_else(|| anyhow_loc!("Failed to get file stem from '{}'", vcxproj_str))?
            .to_str()
            .ok_or_else(|| anyhow_loc!("Failed to convert file stem to str from '{}'", vcxproj_str))
            .map(|s| s.to_string())
    }
}

impl CommandErrorFixer for VcxprojTargetCleanFirstFixer {
    fn try_fix(
        &mut self,
        err: &diag::command_execution::CommandExecutionError,
        ctx: &mut FixContext<'_>,
    ) -> anyhow::Result<FixActionResult> {
        let err_msg = err.full_output_text();
        let re = lnk_obj_vcxproj_regex()?;

        let mut current_pairs: HashSet<(String, String)> = HashSet::new();
        let mut first_vcxproj: Option<String> = None;
        for caps in re.captures_iter(&err_msg) {
            let obj_name = caps
                .name(CAP_OBJ)
                .map(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            let vcxproj_str = caps
                .name(CAP_VCXPROJ)
                .map(|m| m.as_str())
                .unwrap_or_default()
                .to_string();
            if obj_name.is_empty() || vcxproj_str.is_empty() {
                continue;
            }
            if first_vcxproj.is_none() {
                first_vcxproj = Some(vcxproj_str.clone());
            }
            current_pairs.insert((vcxproj_str.clone(), obj_name));
        }

        if current_pairs.is_empty() {
            return Ok(FixActionResult::NotHandled);
        }

        if current_pairs
            .iter()
            .any(|pair| self.seen_pairs.contains(pair))
        {
            return err_loc!("New lnk obj/vcxproj error intersects with history, abort auto-fix");
        }

        let vcxproj_str = first_vcxproj
            .ok_or_else(|| anyhow_loc!("Failed to locate first vcxproj from parsed error"))?;
        let target = Self::parse_target(&vcxproj_str)?;
        let ms_build_path = self
            .ms_build_path
            .as_ref()
            .map(|p| p.as_path())
            .map_err(|err| anyhow::Error::new(err.clone()))?;
        let mut build_cmd = std::process::Command::new(ms_build_path);
        build_cmd
            .arg(&vcxproj_str)
            .arg("/t:Clean;Build")
            .arg(format!("/p:Configuration={}", ctx.build_config))
            .arg("/p:BuildProjectReferences=false")
            .current_dir(ctx.repo_path);
        diag::command_execution::execute_and_print_output_force(&mut build_cmd).with_loc_context(
            || {
                format!(
                    "Failed clean-first rebuild for first target '{}' parsed from '{}'",
                    target, vcxproj_str
                )
            },
        )?;

        self.seen_pairs.extend(current_pairs);
        Ok(FixActionResult::Applied)
    }
}
