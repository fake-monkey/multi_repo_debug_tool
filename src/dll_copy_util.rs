use crate::cmake_build_install;
use crate::sln_merge_kit;
use diag_trace::{anyhow_loc, err_loc, LocContextExt};
use log::{debug, warn};
use std::fs;
use std::path::{Path, PathBuf};

fn find_build_out_dir_from_vcxproj_file(
    vcxproj_file: &Path,
    cpp_build_config: &str,
) -> Option<PathBuf> {
    // 读取 vcxproj 文件内容
    let content = fs::read_to_string(vcxproj_file).ok()?;

    // 解析 XML
    let doc = roxmltree::Document::parse(&content).ok()?;

    // 获取根节点 Project
    let root = doc.root_element();

    // 遍历所有 PropertyGroup 节点
    for property_group in root.children().filter(|n| n.has_tag_name("PropertyGroup")) {
        // 查找 OutDir 节点
        for child in property_group.children() {
            if child.has_tag_name("OutDir") {
                // 检查 OutDir 节点的 Condition 属性是否包含 cpp_build_config
                if let Some(condition) = child.attribute("Condition") {
                    if condition.contains(cpp_build_config) {
                        if let Some(out_dir) = child.text() {
                            let out_dir_path = Path::new(out_dir);
                            // 如果已经是绝对路径，直接使用；否则基于 vcxproj 文件目录拼接
                            let out_path = if out_dir_path.is_absolute() {
                                out_dir_path.to_path_buf()
                            } else {
                                let vcxproj_dir = vcxproj_file.parent()?;
                                vcxproj_dir.join(out_dir)
                            };
                            return Some(out_path);
                        }
                    }
                }
            }
        }
    }

    None
}

fn find_build_out_dir_from_sln_file(
    target_sln_file: &Path,
    cpp_build_config: &str,
) -> anyhow::Result<PathBuf> {
    let sln_data = sln_merge_kit::parse_sln(target_sln_file)?;
    for project in sln_data.projects.iter() {
        let project_path = &project.project_path;
        if project_path.starts_with("..") {
            continue;
        }
        if let Some(vcxproj_path) = project_path.extension().and_then(|ext| {
            if ext == "vcxproj" {
                Some(project.project_path.clone())
            } else {
                None
            }
        }) {
            let vcxproj_file = target_sln_file
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(vcxproj_path);
            if let Some(out_dir) =
                find_build_out_dir_from_vcxproj_file(&vcxproj_file, cpp_build_config)
            {
                return Ok(out_dir);
            }
        }
    }
    err_loc!(
        "Failed to find build output directory from sln file '{}'",
        target_sln_file.display()
    )
}

/// 从 install_manifest.txt 复制 DLL 文件到目标目录
/// target_binary_dir: 目标二进制目录
/// dependencies: 依赖的 install_manifest.txt 所在目录列表
/// cpp_build_config: 构建配置名（如 Release/Debug）
pub fn copy_dll(
    target_sln_file: &Path,
    dependencies: &[PathBuf],
    cpp_build_config: &str,
) -> anyhow::Result<()> {
    // 查找构建目标目录
    let target_dir = find_build_out_dir_from_sln_file(target_sln_file, cpp_build_config)?;
    debug!("Target binary directory: '{}'", target_dir.display());
    // 如果目标目录不存在，创建它
    if !target_dir.exists() {
        fs::create_dir_all(&target_dir).with_loc_context(|| {
            format!(
                "Failed to create target directory '{}'",
                target_dir.display()
            )
        })?;
    }

    // 遍历所有依赖
    for dependency in dependencies {
        // 读取 install_manifest.txt
        let content = cmake_build_install::load_install_manifest_txt(&dependency)?;

        // 逐行处理
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            // 检查是否为 DLL 文件
            if !line.to_lowercase().ends_with(".dll") {
                continue;
            }

            let src_path = Path::new(line);

            // 检查源文件是否存在
            if !src_path.exists() {
                warn!(
                    "Warning: DLL file '{}' from manifest does not exist",
                    src_path.display()
                );
                continue;
            }

            // 获取文件名
            let file_name = src_path.file_name().ok_or_else(|| {
                anyhow_loc!("Failed to extract file name from '{}'", src_path.display())
            })?;

            let dst_path = target_dir.join(file_name);

            // 检查是否需要复制（文件不存在或已改变）
            let need_copy = if !dst_path.exists() {
                true
            } else {
                // 比较源文件和目标文件的大小和修改时间
                let src_metadata = fs::metadata(src_path).with_loc_context(|| {
                    format!("Failed to get metadata for src '{}'", src_path.display())
                })?;
                let dst_metadata = fs::metadata(&dst_path).with_loc_context(|| {
                    format!("Failed to get metadata for dst '{}'", dst_path.display())
                })?;

                // 如果大小不同或源文件更新，则需要复制
                let size_different = src_metadata.len() != dst_metadata.len();
                let src_newer = src_metadata
                    .modified()
                    .with_loc_context(|| "Failed to get src modification time")?
                    > dst_metadata
                        .modified()
                        .with_loc_context(|| "Failed to get dst modification time")?;

                size_different || src_newer
            };

            if need_copy {
                fs::copy(src_path, &dst_path).with_loc_context(|| {
                    format!(
                        "Failed to copy '{}' to '{}'",
                        src_path.display(),
                        dst_path.display()
                    )
                })?;
                debug!(
                    "Copied DLL '{}' to '{}'",
                    src_path.file_name().unwrap().to_string_lossy(),
                    dst_path.display()
                );
            }
        }
    }

    Ok(())
}
