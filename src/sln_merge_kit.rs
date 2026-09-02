use diag_trace::{anyhow_loc, err_loc, LocContextExt};
use pathdiff;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const CMAKE_PREDEFINED_TARGETS: &str = "CMakePredefinedTargets";
const PROJECT_CONFIGURATION_PLATFORMS: &str = "ProjectConfigurationPlatforms";
const NESTED_PROJECTS: &str = "NestedProjects";

/* 其他项目的 ALL_BUILD 会干扰 cmake build 指令
而且这里还无法通过改名来规避，visual studio 保存时好像会强制从 vcxproj 文件中读取项目名称写入到 sln 文件中。 */
const EXCLUDE_PROJECT_NAMES: [&str; 1] = ["ALL_BUILD"];

#[derive(Clone)]
pub struct ProjectBlock {
    first_guid: String,
    name: String,
    guid: String,
    pub project_path: PathBuf,
    project_section: Vec<String>,
    is_file: bool,
}

#[derive(Clone)]
struct GlobalSection {
    name: String,
    header: String,
    lines: Vec<String>,
    footer: String,
}

#[derive(Clone)]
struct GlobalBlock {
    global_sections: Vec<GlobalSection>,
}

impl GlobalBlock {
    fn find_section(&self, name: &str) -> Option<&GlobalSection> {
        self.global_sections.iter().find(|s| s.name == name)
    }

    fn find_mut_section(&mut self, name: &str) -> Option<&mut GlobalSection> {
        self.global_sections.iter_mut().find(|s| s.name == name)
    }
}

#[derive(Clone)]
pub struct SlnData {
    header: Vec<String>,
    pub projects: Vec<ProjectBlock>,
    globals: GlobalBlock,
    newline: String,
}

struct FolderProjectTree {
    parent_uuid: HashMap<String, String>,
    uuid_name: HashMap<String, String>,
}

impl FolderProjectTree {
    fn from(sln: &SlnData) -> Self {
        let mut parent_uuid = HashMap::new();
        let mut uuid_name = HashMap::new();
        for proj in sln.projects.iter().filter(|p| !p.is_file) {
            uuid_name.insert(proj.guid.clone(), proj.name.clone());
        }
        for line in sln
            .globals
            .find_section(NESTED_PROJECTS)
            .map(|s| &s.lines)
            .unwrap_or(&Vec::new())
        {
            let split: Vec<&str> = line.split('=').map(|s| s.trim()).collect();
            if split.len() == 2 {
                let child = split[0];
                let parent = split[1];
                parent_uuid.insert(child.to_string(), parent.to_string());
            }
        }
        FolderProjectTree {
            parent_uuid,
            uuid_name,
        }
    }

    fn get_folder_path(&self, uuid: &str) -> PathBuf {
        let mut path = Vec::new();
        let mut current_uuid = uuid;
        if let Some(name) = self.uuid_name.get(current_uuid) {
            path.push(name.clone());
        }
        while let Some(parent_uuid) = self.parent_uuid.get(current_uuid) {
            if let Some(name) = self.uuid_name.get(parent_uuid) {
                path.push(name.clone());
            }
            current_uuid = parent_uuid;
        }
        path.reverse();
        path.into_iter().collect()
    }
}

fn build_sln_content(sln: SlnData) -> String {
    let mut output = Vec::new();
    output.extend(sln.header.iter().cloned());
    for proj in sln.projects.iter() {
        output.push(format!(
            "Project(\"{}\") = \"{}\", \"{}\", \"{}\"",
            proj.first_guid,
            proj.name,
            proj.project_path.display(),
            proj.guid
        ));
        output.extend(proj.project_section.iter().cloned());
        output.push("EndProject".to_string());
    }
    output.push("Global".to_string());

    for sec in sln.globals.global_sections.into_iter() {
        output.push(sec.header);
        output.extend(sec.lines.into_iter());
        output.push(sec.footer);
    }

    output.push("EndGlobal".to_string());

    let mut final_content = output.join(&sln.newline);
    final_content.push_str(&sln.newline);
    final_content
}

fn extract_guid_from_line(line: &str) -> Option<String> {
    let start = line.rfind('{')?;
    let tail = &line[start..];
    let end = tail.find('}')?;
    Some(tail[..=end].to_string())
}

fn extract_first_guid_from_line(line: &str) -> Option<String> {
    let start = line.find('{')?;
    let tail = &line[start..];
    let end = tail.find('}')?;
    Some(tail[..=end].to_string())
}

fn detect_indent(lines: &[String], default: &str) -> String {
    lines
        .first()
        .map(|l| l.chars().take_while(|c| c.is_whitespace()).collect())
        .unwrap_or_else(|| default.to_string())
}

fn parse_project(idx: &mut usize, lines: &[String]) -> anyhow::Result<ProjectBlock> {
    let first_line = &lines[*idx];
    let first_guid = extract_first_guid_from_line(first_line)
        .ok_or_else(|| anyhow_loc!("Failed to parse project first guid: {}", first_line))?;
    let guid = extract_guid_from_line(first_line)
        .ok_or_else(|| anyhow_loc!("Failed to parse project guid: {}", first_line))?;
    let right_split: Vec<&str> = first_line
        .split('=')
        .nth(1)
        .ok_or_else(|| {
            anyhow_loc!(
                "Failed to parse project name in line {}: {}",
                idx,
                first_line
            )
        })?
        .split(',')
        .map(|s| s.trim().trim_matches('\"'))
        .collect();
    let name = right_split
        .get(0)
        .map(|s| s.trim().to_string())
        .ok_or_else(|| {
            anyhow_loc!(
                "Failed to parse project name in line {}: {}",
                idx,
                first_line
            )
        })?;
    let path = right_split
        .get(1)
        .map(|s| PathBuf::from(s.trim()))
        .ok_or_else(|| {
            anyhow_loc!(
                "Failed to parse project path in line {}: {}",
                idx,
                first_line
            )
        })?;
    let start_idx = *idx + 1;
    while *idx < lines.len() && lines[*idx].trim() != "EndProject" {
        *idx += 1;
    }
    if *idx >= lines.len() {
        return err_loc!("Malformed sln, missing EndProject in project '{}'", name);
    }
    let mut block = Vec::new();
    for i in start_idx..*idx {
        block.push(lines[i].clone());
    }
    *idx += 1; // skip EndProject
    Ok(ProjectBlock {
        name,
        first_guid,
        guid,
        project_path: path,
        project_section: block,
        is_file: true, // 先暂时标记为true，外部再统一计算结果
    })
}

fn parse_global_section(idx: &mut usize, lines: &[String]) -> anyhow::Result<GlobalSection> {
    let header_line = &lines[*idx];
    let trimmed = header_line.trim_start();
    let name = trimmed
        .trim_start_matches("GlobalSection(")
        .split(')')
        .next()
        .unwrap_or("")
        .to_string();
    *idx += 1;
    let mut sec_lines = Vec::new();
    while *idx < lines.len() && !lines[*idx].trim_start().starts_with("EndGlobalSection") {
        sec_lines.push(lines[*idx].clone());
        *idx += 1;
    }
    if *idx >= lines.len() {
        return err_loc!(
            "Malformed sln, missing EndGlobalSection in global section '{}'",
            name
        );
    }
    let footer_line = lines[*idx].clone();
    *idx += 1; // skip EndGlobalSection
    Ok(GlobalSection {
        name,
        header: header_line.clone(),
        lines: sec_lines,
        footer: footer_line,
    })
}

fn parse_global(idx: &mut usize, lines: &[String]) -> anyhow::Result<GlobalBlock> {
    let mut all_sections: Vec<GlobalSection> = Vec::new();

    while *idx < lines.len() {
        let line = &lines[*idx];
        let trimmed = line.trim_start();
        if trimmed.starts_with("GlobalSection(") {
            let section = parse_global_section(idx, lines)?;
            all_sections.push(section);
        } else {
            *idx += 1;
        }
        if trimmed == "EndGlobal" {
            break;
        }
    }

    Ok(GlobalBlock {
        global_sections: all_sections,
    })
}

pub fn parse_sln(path: &Path) -> anyhow::Result<SlnData> {
    let content = fs::read_to_string(path)
        .with_loc_context(|| format!("Failed to read sln '{}'", path.display()))?;
    let newline = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
    .to_string();
    let lines: Vec<String> = content.lines().map(|s| s.to_string()).collect();

    let parent_dir = path.parent().unwrap_or_else(|| Path::new("."));

    let mut header = Vec::new();
    let mut projects = Vec::new();
    let mut globals: Option<GlobalBlock> = None;
    let mut idx = 0usize;
    // 先收集 header 与 project 块
    while idx < lines.len() {
        let line = &lines[idx];
        let trimmed = line.trim_start();
        if trimmed.starts_with("Project(") {
            let mut project = parse_project(&mut idx, &lines)?;
            project.is_file = parent_dir.join(&project.project_path).is_file();
            projects.push(project);
        } else if trimmed == "Global" {
            globals = Some(parse_global(&mut idx, &lines)?);
            break;
        } else {
            header.push(line.clone());
            idx += 1;
        }
    }

    let globals = globals.ok_or_else(|| anyhow_loc!("Malformed sln, missing Global block"))?;

    Ok(SlnData {
        header,
        projects,
        globals,
        newline,
    })
}

pub fn merge_sln(target_sln: &Path, source_sln_list: &Vec<PathBuf>) -> anyhow::Result<()> {
    if !target_sln.exists() {
        return err_loc!("Target sln does not exist: {}", target_sln.display());
    }

    let target_parent = target_sln.parent().unwrap_or(Path::new("."));
    let mut target = parse_sln(target_sln)?;

    let target_folder_tree = FolderProjectTree::from(&target);

    let mut exist_project_set: HashSet<PathBuf> = target
        .projects
        .iter()
        .map(|p| {
            if p.is_file {
                p.project_path.clone()
            } else {
                target_folder_tree.get_folder_path(&p.guid)
            }
        })
        .collect();

    let folder_first_guid = target
        .projects
        .iter()
        .find_map(|p| {
            (p.project_path.as_os_str() == CMAKE_PREDEFINED_TARGETS).then_some(p.first_guid.clone())
        })
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    // 收集合并结果
    for src_path in source_sln_list {
        if !src_path.exists() {
            continue;
        }
        let src_parent = src_path.parent().unwrap_or(Path::new("."));
        let relative_path = pathdiff::diff_paths(src_parent, target_parent).ok_or_else(|| {
            anyhow_loc!(
                "Failed to compute relative path from '{}' to '{}'",
                target_sln.display(),
                src_path.display()
            )
        })?;
        let src = parse_sln(src_path)?;

        let src_folder_tree = FolderProjectTree::from(&src);

        let folder_name = src_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "UnknownFolder".to_string());
        let folder_uuid = format!(
            "{{{}}}",
            Uuid::new_v5(&Uuid::NAMESPACE_DNS, folder_name.as_bytes())
                .to_string()
                .to_uppercase()
        );

        if !exist_project_set.contains(&PathBuf::from(&folder_name)) {
            target.projects.push(ProjectBlock {
                first_guid: folder_first_guid.clone(),
                name: folder_name.clone(),
                guid: folder_uuid.clone(),
                project_path: PathBuf::from(&folder_name),
                project_section: Vec::new(),
                is_file: false,
            });
        }

        let mut need_merge_guid: HashSet<String> = HashSet::new();

        for mut proj in src.projects {
            if src_parent.join(&proj.project_path).is_file() {
                if EXCLUDE_PROJECT_NAMES.contains(&proj.name.as_str()) {
                    continue;
                }
                proj.project_path = relative_path.join(&proj.project_path);
                if exist_project_set.insert(proj.project_path.clone()) {
                    need_merge_guid.insert(proj.guid.clone());
                    target.projects.push(proj);
                }
            } else {
                let folder_path =
                    PathBuf::from(&folder_name).join(src_folder_tree.get_folder_path(&proj.guid));
                if exist_project_set.insert(folder_path.clone()) {
                    need_merge_guid.insert(proj.guid.clone());
                    let mut copy_proj = proj.clone();
                    copy_proj.first_guid = folder_first_guid.clone();
                    target.projects.push(copy_proj);
                }
            }
        }

        let src_project_cfg = src
            .globals
            .find_section(PROJECT_CONFIGURATION_PLATFORMS)
            .map(|s| {
                s.lines
                    .iter()
                    .filter(|s| {
                        extract_first_guid_from_line(s)
                            .is_some_and(|s| need_merge_guid.contains(&s))
                    })
                    .cloned()
                    .collect::<Vec<String>>()
            })
            .unwrap_or_default();
        {
            let target_project_config = target
                .globals
                .find_mut_section(PROJECT_CONFIGURATION_PLATFORMS)
                .ok_or(anyhow_loc!(
                    "Can not find project configuration platforms section"
                ))?;
            target_project_config.lines.extend(src_project_cfg);
        }
        {
            let target_nested_projects = target
                .globals
                .find_mut_section(NESTED_PROJECTS)
                .ok_or(anyhow_loc!("Can not find nested projects section"))?;
            let indent = detect_indent(&target_nested_projects.lines, "        ");
            for line in src
                .globals
                .find_section(NESTED_PROJECTS)
                .map(|s| &s.lines)
                .unwrap_or(&Vec::new())
            {
                if let Some(guid) = extract_first_guid_from_line(line) {
                    if need_merge_guid.contains(&guid) {
                        target_nested_projects.lines.push(line.clone());
                        need_merge_guid.remove(&guid);
                    }
                }
            }
            for guid in need_merge_guid.iter() {
                target_nested_projects
                    .lines
                    .push(format!("{}{} = {}", indent, guid, folder_uuid));
            }
        }
    }

    // 重新拼装 sln
    let final_content = build_sln_content(target);

    let ori_content = fs::read_to_string(target_sln)
        .with_loc_context(|| format!("Failed to read sln '{}'", target_sln.display()))?;

    if ori_content != final_content {
        fs::write(target_sln, final_content)
            .with_loc_context(|| format!("Failed to write sln '{}'", target_sln.display()))?;
    }
    Ok(())
}

pub fn get_vcxproj_hash(hasher: &mut DefaultHasher, sln_path: &Path) -> anyhow::Result<()> {
    let sln_data = parse_sln(sln_path)
        .with_loc_context(|| format!("Failed to parse sln '{}'", sln_path.display()))?;
    let parent_dir = sln_path.parent().unwrap_or_else(|| Path::new("."));
    for proj in sln_data.projects.iter().filter(|p| {
        p.is_file
            && p.project_path
                .extension()
                .map(|s| s == "vcxproj")
                .unwrap_or(false)
            && !p.project_path.starts_with("..")
    }) {
        let context =
            fs::read_to_string(parent_dir.join(&proj.project_path)).with_loc_context(|| {
                format!(
                    "Failed to read vcxproj file '{}'",
                    proj.project_path.display()
                )
            })?;
        context.hash(hasher);
    }
    Ok(())
}
