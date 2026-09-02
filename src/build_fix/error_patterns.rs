use diag_trace::{anyhow_loc, LocContextExt};
use regex::Regex;
use std::sync::OnceLock;

pub const CAP_OBJ: &str = "obj";
pub const CAP_VCXPROJ: &str = "vcxproj";

const LNK_OBJ_VCXPROJ_PATTERN: &str = r"(?is)(?P<obj>[a-zA-Z0-9_\-.]+\.obj)\s*:\s*fatal error LNK\d{4}:.*?\[(?P<vcxproj>[^\]]+\.vcxproj)\]";

pub fn lnk_obj_vcxproj_regex() -> anyhow::Result<&'static Regex> {
    static RE: OnceLock<anyhow::Result<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(LNK_OBJ_VCXPROJ_PATTERN)
            .with_loc_context(|| "Failed to compile lnk obj/vcxproj regex")
    })
    .as_ref()
    .map_err(|e| anyhow_loc!("Failed to initialize shared lnk obj/vcxproj regex: {}", e))
}
