use chardet;
use diag_trace::LocContextExt;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    path::Path,
};
use unified_access::{Access, AccessMut};

use crate::access_anyhow::{AccessAnyhow, AccessMutAnyhow};

/// 自定义反序列化：支持单个字符串或字符串数组
fn deserialize_string_or_vec<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        String(String),
        Vec(Vec<String>),
    }

    match Option::<StringOrVec>::deserialize(deserializer)? {
        None => Ok(None),
        Some(StringOrVec::String(s)) => Ok(Some(vec![s])),
        Some(StringOrVec::Vec(v)) => Ok(Some(v)),
    }
}

/// 只解析常用字段,其余通过 extra 保留，方便容错。
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Hash)]
pub struct ConfigurePreset {
    pub name: String,
    #[serde(
        rename = "inherits",
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_string_or_vec"
    )]
    pub inherits: Option<Vec<String>>,
    #[serde(rename = "generator", skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(rename = "binaryDir", skip_serializing_if = "Option::is_none")]
    pub binary_dir: Option<String>,
    /// 使用 BTreeMap 而非 HashMap 以保证 JSON 序列化顺序稳定，
    /// 避免因 HashMap 迭代顺序随机导致的 hash 计算不一致问题
    #[serde(rename = "cacheVariables", skip_serializing_if = "Option::is_none")]
    pub cache_variables: Option<BTreeMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// 使用 BTreeMap 保证序列化顺序稳定
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,

    #[serde(skip)]
    pub depth: u32,
    #[serde(skip)]
    has_expanded: bool,
}

impl ConfigurePreset {
    pub fn new_with_name(name: &str) -> Self {
        ConfigurePreset {
            name: name.to_string(),
            inherits: None,
            generator: None,
            binary_dir: None,
            cache_variables: None,
            hidden: None,
            extra: BTreeMap::new(),
            depth: 0,
            has_expanded: false,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct BuildPreset {
    pub name: String,
    #[serde(rename = "configurePreset", skip_serializing_if = "Option::is_none")]
    pub configure_preset: Option<String>,
    #[serde(rename = "configuration", skip_serializing_if = "Option::is_none")]
    pub configuration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// 使用 BTreeMap 保证序列化顺序稳定
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// 顶层结构覆盖 CMakePresets.json 与 CMakeUserPresets.json
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct CMakePresets {
    pub version: u32,
    #[serde(
        rename = "configurePresets",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub configure_presets: Vec<RefCell<ConfigurePreset>>,
    #[serde(
        rename = "buildPresets",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub build_presets: Option<Vec<BuildPreset>>,
    /// 使用 BTreeMap 保证序列化顺序稳定
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

pub type CMakeUserPresets = CMakePresets;

pub(crate) fn expand_cmake_preset_inheritance<'a, P>(presets: &'a [P]) -> anyhow::Result<()>
where
    &'a P: AccessAnyhow<'a, ConfigurePreset> + AccessMutAnyhow<'a, ConfigurePreset>,
{
    let mut name_to_preset: HashMap<String, usize> = HashMap::new();
    for (idx, preset) in presets.iter().enumerate() {
        name_to_preset.insert(
            preset
                .access()
                .with_loc_context(|| "Failed to borrow preset")?
                .name
                .clone(),
            idx,
        );
    }

    for idx in 0..presets.len() {
        expand_preset(presets, &name_to_preset, idx, 0)?;
    }

    Ok(())
}

fn expand_preset<'a, P>(
    presets: &'a [P],
    name_to_preset: &HashMap<String, usize>,
    idx: usize,
    depth: u32,
) -> anyhow::Result<()>
where
    &'a P: AccessAnyhow<'a, ConfigurePreset> + AccessMutAnyhow<'a, ConfigurePreset>,
{
    let inherits = {
        let preset = (&presets[idx])
            .access_mut()
            .with_loc_context(|| "Failed to borrow preset")?;
        if preset.has_expanded {
            return Ok(());
        }
        preset.inherits.clone()
    };
    if let Some(parent_names) = &inherits {
        // 按顺序展开所有父预设
        for parent_name in parent_names {
            if let Some(&parent_idx) = name_to_preset.get(parent_name) {
                expand_preset(presets, name_to_preset, parent_idx, depth + 1)?;

                let mut preset = (&presets[idx])
                    .access_mut()
                    .with_loc_context(|| "Failed to borrow preset mutably")?;
                let parent_preset = (&presets[parent_idx])
                    .access()
                    .with_loc_context(|| "Failed to borrow preset immutably")?;
                // 继承父预设的字段（子预设优先）
                if preset.generator.is_none() {
                    preset.generator = parent_preset.generator.clone();
                }
                if preset.binary_dir.is_none() {
                    preset.binary_dir = parent_preset.binary_dir.clone();
                }
                if let Some(parent_cache_var) = parent_preset.cache_variables.as_ref() {
                    let cache_var = preset.cache_variables.get_or_insert(BTreeMap::new());
                    // 合并 cache_variables 字段
                    for (k, v) in parent_cache_var.iter() {
                        cache_var.entry(k.clone()).or_insert(v.clone());
                    }
                }
            }
        }
    }
    {
        let mut preset = (&presets[idx])
            .access_mut()
            .with_loc_context(|| "Failed to borrow preset mutably")?;
        preset.depth = depth;
        preset.has_expanded = true;
    }
    Ok(())
}

fn read_json_file_to_string(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(&path)
        .with_loc_context(|| format!("Failed to read file: {}", path.display()))?;

    let detection = chardet::detect(&bytes);
    let encoding_name = detection.0;
    let (cow, _encoding_used, had_errors) =
        encoding_rs::Encoding::for_label(encoding_name.as_bytes())
            .unwrap_or(encoding_rs::UTF_8)
            .decode(&bytes);

    if had_errors {
        eprintln!("Warning: encoding conversion had errors");
    }

    let content = cow.into_owned();
    Ok(content)
}

pub fn parse_cmake_presets(repo_dir: &Path) -> anyhow::Result<(CMakePresets, CMakeUserPresets)> {
    let presets_path = repo_dir.join("CMakePresets.json");
    let content = read_json_file_to_string(&presets_path)?;

    let user_presets_path = repo_dir.join("CMakeUserPresets.json");
    let user_presets = if user_presets_path.exists() {
        let content = read_json_file_to_string(&user_presets_path)?;
        let user_presets: CMakeUserPresets = serde_json::from_str(&content)
            .with_loc_context(|| "Failed to parse CMake user presets JSON")?;
        user_presets
    } else {
        CMakeUserPresets {
            // 先用哨兵值占位，后续在 parse_cmake_presets_from_content 中回填
            version: 0,
            configure_presets: Vec::new(),
            build_presets: None,
            extra: BTreeMap::new(),
        }
    };

    parse_cmake_presets_from_content(content, user_presets).with_loc_context(|| {
        format!(
            "Failed to parse CMake presets JSON: path: {}",
            presets_path.display()
        )
    })
}

fn parse_cmake_presets_from_content(
    content: String,
    mut user_presets: CMakeUserPresets,
) -> anyhow::Result<(CMakePresets, CMakeUserPresets)> {
    let presets: CMakePresets = serde_json::from_str(&content)
        .with_loc_context(|| "Failed to parse CMake presets JSON content")?;

    if user_presets.version == 0 {
        user_presets.version = presets.version;
    }

    Ok((presets, user_presets))
}

pub fn save_cmake_user_presets(repo_dir: &Path, user_presets: &CMakePresets) -> anyhow::Result<()> {
    let user_presets_path = repo_dir.join("CMakeUserPresets.json");
    let content = serde_json::to_string_pretty(user_presets)
        .with_loc_context(|| "Failed to serialize CMake user presets to JSON")?;

    std::fs::write(&user_presets_path, content)
        .with_loc_context(|| "Failed to write CMake user presets file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_repo(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("cmake_presets")
            .join(name)
    }

    #[test]
    fn parse_cmake_presets_fills_user_version_when_file_is_missing() {
        let (_presets, parsed_user) = parse_cmake_presets(&fixture_repo("without_user")).unwrap();

        assert_eq!(parsed_user.version, 7);
    }

    #[test]
    fn parse_cmake_presets_does_not_expand_inheritance() {
        let (presets, _parsed_user) = parse_cmake_presets(&fixture_repo("without_user")).unwrap();

        let child = presets.configure_presets[1].borrow();
        assert_eq!(child.name, "child");
        assert!(child.generator.is_none());
        assert!(child.binary_dir.is_none());
    }
}
