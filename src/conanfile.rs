use diag_trace::{anyhow_loc, err_loc, LocContextExt};
use petgraph::graph::DiGraph;
use regex::Regex;
use std::{collections::HashMap, fs, path::Path};

/// 从 conanfile.py 的内容中解析包名
///
/// # 参数
/// * `content` - conanfile.py 文件的内容
///
/// # 返回
/// * `Ok(String)` - 成功找到包名
/// * `Err(String)` - 正则匹配失败或未找到包名
///
/// # 示例
/// ```
/// let content = r#"name = "my_package""#;
/// let package_name = parse_package_name_from_content(content);
/// ```
fn parse_package_name_from_content(content: &str) -> anyhow::Result<String> {
    // 正则表达式匹配 name = "..." 或 name = '...'
    // 支持多种格式：
    // - name = "package_name"
    // - name = 'package_name'
    // - name="package_name"
    // - name='package_name'
    // - 可能有多余的空格
    let re = Regex::new(r#"(?m)^\s*name\s*=\s*["']([^"']+)["']"#)
        .with_loc_context(|| "正则表达式编译失败")?;

    // 查找第一个匹配
    if let Some(captures) = re.captures(content) {
        if let Some(name) = captures.get(1) {
            return Ok(name.as_str().to_string());
        }
    }

    return err_loc!("未找到包名 (name = \"...\")");
}

pub fn get_conanfile_content(repo_path: &Path) -> anyhow::Result<String> {
    let conanfile_path = repo_path.join("conanfile.py");
    if !conanfile_path.exists() {
        return err_loc!("conanfile.py 不存在于仓库路径: {}", repo_path.display());
    }
    let content = fs::read_to_string(&conanfile_path)
        .with_loc_context(|| format!("无法读取 conanfile.py 文件: {}", conanfile_path.display()))?;
    Ok(content)
}

/// 从仓库路径中自动查找并解析 conanfile.py 的包名
///
/// # 参数
/// * `repo_path` - 仓库的根目录路径
///
/// # 返回
/// * `Ok(String)` - 成功找到包名
/// * `Err(String)` - 读取失败、文件不存在或未找到包名
///
/// # 示例
/// ```
/// use std::path::PathBuf;
/// let repo_path = PathBuf::from("path/to/repo");
/// let package_name = parse_package_name_from_repo(&repo_path)?;
/// ```
pub fn parse_package_name_from_repo(repo_path: &Path) -> anyhow::Result<String> {
    let conanfile_content = get_conanfile_content(repo_path)?;

    parse_package_name_from_content(&conanfile_content)
}

/// 从 conanfile.py 的内容中查找指定的包名列表
///
/// # 参数
/// * `content` - conanfile.py 文件的内容
/// * `package_names` - 要查找的包名列表
///
/// # 返回
/// * `Vec<String>` - 返回在 conanfile 中找到的包名列表
///
/// # 示例
/// ```
/// let content = r#"
/// requires = ["boost/1.70.0", "zlib/1.2.11", "openssl/1.1.1"]
/// "#;
/// let packages = vec!["boost", "zlib", "protobuf"];
/// let found = find_packages_in_conanfile(content, &packages);
/// // found 包含 ["boost", "zlib"]
/// ```
fn find_dependence_in_conanfile(content: &str, package_names: &[String]) -> Vec<String> {
    let mut found_packages = Vec::new();

    for package_name in package_names.iter() {
        // 使用正则表达式查找包名
        // 匹配格式：依赖项名称后面必须跟着 /
        // - requires = ["package/version", ...]
        // - requires = ("package/version", ...)
        // - self.requires("package/version")
        let pattern = format!(r#"["']{}/"#, regex::escape(package_name));

        if let Ok(re) = Regex::new(&pattern) {
            if re.is_match(content) {
                found_packages.push(package_name.to_string());
            }
        }
    }

    found_packages
}

/// 依赖访问器 - 提供解耦的依赖获取接口
pub trait DependencyQueryInput {
    /// 根据 conan 包名获取 conanfile.py 内容（不可变）
    fn get_conanfile_content_by_conan_name(&self, conan_name: &str) -> anyhow::Result<String>;

    fn get_all_dependencies(&self) -> Vec<String>;
}

fn build_all_dependency_graph(
    possible_dependencies: &dyn DependencyQueryInput,
) -> anyhow::Result<DiGraph<String, ()>> {
    use petgraph::graph::DiGraph;

    let mut graph = DiGraph::<String, ()>::new();
    let mut node_indices: HashMap<String, petgraph::graph::NodeIndex> = HashMap::new();

    let package_names = possible_dependencies.get_all_dependencies();

    // 添加所有节点并缓存索引
    for package_name in package_names.iter() {
        let idx = graph.add_node(package_name.clone());
        node_indices.insert(package_name.clone(), idx);
    }

    // 添加边（依赖关系）
    for (package_name, &node_index) in node_indices.iter() {
        let conanfile_content =
            possible_dependencies.get_conanfile_content_by_conan_name(package_name)?;
        let dependencies = find_dependence_in_conanfile(&conanfile_content, &package_names);

        for dep in dependencies {
            if let Some(&dep_index) = node_indices.get(&dep) {
                graph.add_edge(node_index, dep_index, ());
            }
        }
    }

    Ok(graph)
}

#[derive(Default)]
pub struct DependencyTopologicalInfo {
    pub graph: DiGraph<String, ()>,
    pub sorted_names: Vec<String>,
    /// 原始调试列表中实际仍在图中的包名（含 executable）
    pub need_debug_pkgs: Vec<String>,
}

pub fn build_dependency_graph_and_topological_sort(
    executable_name: &str,
    possible_dependencies: &dyn DependencyQueryInput,
    need_debug_pkgs: &[String],
) -> anyhow::Result<DependencyTopologicalInfo> {
    use petgraph::algo;
    use petgraph::visit::{Dfs, Reversed};

    let mut need_debug_pkgs: Vec<String> = need_debug_pkgs.to_vec();
    if !need_debug_pkgs.contains(&executable_name.to_string()) {
        need_debug_pkgs.push(executable_name.to_string());
    }

    let graph = build_all_dependency_graph(possible_dependencies)?;

    // 1. 正向遍历：从 start_node 能到达哪些节点
    let mut forward_reachable = std::collections::HashSet::new();
    if let Some(start_idx) = graph.node_indices().find(|&idx| {
        graph
            .node_weight(idx)
            .filter(|name| name.as_str() == executable_name)
            .is_some()
    }) {
        let mut dfs = Dfs::new(&graph, start_idx);
        while let Some(node_idx) = dfs.next(&graph) {
            forward_reachable.insert(node_idx);
        }
    }

    // 2. 反向遍历：能到达 need_debug_pkgs 的节点（使用视图，避免修改原图）
    let mut backward_reachable = std::collections::HashSet::new();
    let reversed_graph = Reversed(&graph);

    for debug_pkg in need_debug_pkgs.iter() {
        if let Some(debug_idx) = graph
            .node_indices()
            .find(|&idx| graph.node_weight(idx) == Some(debug_pkg))
        {
            let mut dfs = Dfs::new(&reversed_graph, debug_idx);
            while let Some(node_idx) = dfs.next(&reversed_graph) {
                backward_reachable.insert(node_idx);
            }
        }
    }

    // 3. 取交集，保留既能从start_node到达，又能到达need_debug_pkgs的节点
    let reachable: std::collections::HashSet<_> = forward_reachable
        .intersection(&backward_reachable)
        .cloned()
        .collect();

    // 使用 filter_map 过滤图，只保留可达节点
    let graph = graph.filter_map(
        |idx, node| {
            if reachable.contains(&idx) {
                Some(node.clone())
            } else {
                None
            }
        },
        |_, edge| Some(*edge),
    );

    // 4. 拓扑排序
    let sort_result =
        algo::toposort(&graph, None).map_err(|e| anyhow_loc!("图中存在环: {:?}", e.node_id()))?;

    let mut sorted_names: Vec<String> = sort_result
        .iter()
        .filter_map(|idx| graph.node_weight(*idx).cloned())
        .collect();
    sorted_names.reverse();

    // 最后把 start_node 也过滤掉不在图中的节点
    let remaining: std::collections::HashSet<String> = graph.node_weights().cloned().collect();

    Ok(DependencyTopologicalInfo {
        graph,
        sorted_names,
        need_debug_pkgs: need_debug_pkgs
            .iter()
            .filter(|name| remaining.contains(*name))
            .cloned()
            .collect(),
    })
}

pub fn check_graph_equal(graph1: &DiGraph<String, ()>, graph2: &DiGraph<String, ()>) -> bool {
    if graph1.node_count() != graph2.node_count() {
        return false;
    }

    // 为 graph2 构建节点名称到索引的映射
    let mut graph2_node_map: HashMap<String, petgraph::graph::NodeIndex> = HashMap::new();
    for idx in graph2.node_indices() {
        if let Some(name) = graph2.node_weight(idx) {
            graph2_node_map.insert(name.clone(), idx);
        }
    }

    // 对 graph1 中的每个节点进行检查
    for node_idx in graph1.node_indices() {
        let node_name = match graph1.node_weight(node_idx) {
            Some(name) => name.clone(),
            None => return false,
        };

        // 在 graph2 中查找同名节点
        let node_idx2 = match graph2_node_map.get(&node_name) {
            Some(&idx) => idx,
            None => return false, // graph2 中没有这个节点
        };

        // 获取 graph1 中该节点能直接到达的所有节点
        let mut neighbors1: Vec<String> = graph1
            .neighbors(node_idx)
            .filter_map(|idx| graph1.node_weight(idx).cloned())
            .collect();
        neighbors1.sort();

        // 获取 graph2 中该节点能直接到达的所有节点
        let mut neighbors2: Vec<String> = graph2
            .neighbors(node_idx2)
            .filter_map(|idx| graph2.node_weight(idx).cloned())
            .collect();
        neighbors2.sort();

        // 比较直接邻接节点是否相同
        if neighbors1 != neighbors2 {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct MockDependencyQueryInput {
        conanfiles: HashMap<String, String>,
        all_dependencies: Vec<String>,
    }

    impl DependencyQueryInput for MockDependencyQueryInput {
        fn get_conanfile_content_by_conan_name(&self, conan_name: &str) -> anyhow::Result<String> {
            self.conanfiles
                .get(conan_name)
                .cloned()
                .ok_or_else(|| anyhow_loc!("Missing mock conanfile for {}", conan_name))
        }

        fn get_all_dependencies(&self) -> Vec<String> {
            self.all_dependencies.clone()
        }
    }

    #[test]
    fn test_parse_package_name_with_double_quotes() {
        let content = r#"name = "my_package""#;
        let result = parse_package_name_from_content(content)
            .expect("Failed to parse package name from content with double quotes");
        assert_eq!(result, "my_package".to_string());
    }

    #[test]
    fn test_parse_package_name_with_single_quotes() {
        let content = r#"name = 'my_package'"#;
        let result = parse_package_name_from_content(content)
            .expect("Failed to parse package name from content with single quotes");
        assert_eq!(result, "my_package".to_string());
    }

    #[test]
    fn test_parse_package_name_without_spaces() {
        let content = r#"name="my_package""#;
        let result = parse_package_name_from_content(content)
            .expect("Failed to parse package name from content without spaces");
        assert_eq!(result, "my_package".to_string());
    }

    #[test]
    fn test_parse_package_name_in_class() {
        let content = r#"
from conan import ConanFile

class MyPackageConan(ConanFile):
    name = "my_package"
    version = "1.0.0"
"#;
        let result = parse_package_name_from_content(content)
            .expect("Failed to parse package name from content in class");
        assert_eq!(result, "my_package".to_string());
    }

    #[test]
    fn test_parse_package_name_with_extra_spaces() {
        let content = r#"  name   =   "my_package"  "#;
        let result = parse_package_name_from_content(content)
            .expect("Failed to parse package name from content with extra spaces");
        assert_eq!(result, "my_package".to_string());
    }

    #[test]
    fn test_parse_package_name_not_found() {
        let content = r#"version = "1.0.0""#;
        let result = parse_package_name_from_content(content);
        assert!(result.is_err());
    }

    #[test]
    fn test_build_all_dependency_graph_from_in_memory_contents() {
        let mut conanfiles = HashMap::new();
        conanfiles.insert(
            "app".to_string(),
            r#"requires = ["lib_a/1.0", "lib_b/1.0"]"#.to_string(),
        );
        conanfiles.insert(
            "lib_a".to_string(),
            r#"requires = ["lib_b/1.0"]"#.to_string(),
        );
        conanfiles.insert("lib_b".to_string(), r#"requires = []"#.to_string());

        let input = MockDependencyQueryInput {
            conanfiles,
            all_dependencies: vec!["app".to_string(), "lib_a".to_string(), "lib_b".to_string()],
        };

        let graph = build_all_dependency_graph(&input).expect("graph should be built");
        assert_eq!(graph.node_count(), 3);

        let info =
            build_dependency_graph_and_topological_sort("app", &input, &["lib_b".to_string()])
                .expect("topological sort should succeed");
        assert_eq!(info.sorted_names, vec!["lib_b", "lib_a", "app"]);
        // 调试列表（含 executable）中所有包都在图中，因此全部保留
        assert_eq!(info.need_debug_pkgs, vec!["lib_b", "app"]);
    }

    #[test]
    fn test_need_debug_pkgs_filters_out_packages_not_in_graph() {
        let mut conanfiles = HashMap::new();
        conanfiles.insert(
            "app".to_string(),
            r#"requires = ["lib_a/1.0", "lib_b/1.0"]"#.to_string(),
        );
        conanfiles.insert(
            "lib_a".to_string(),
            r#"requires = ["lib_b/1.0"]"#.to_string(),
        );
        conanfiles.insert("lib_b".to_string(), r#"requires = []"#.to_string());

        let input = MockDependencyQueryInput {
            conanfiles,
            all_dependencies: vec!["app".to_string(), "lib_a".to_string(), "lib_b".to_string()],
        };

        // debug 列表里有一个包 "orphan" 不在任何 conanfile 中，也不在图中
        let info = build_dependency_graph_and_topological_sort(
            "app",
            &input,
            &["lib_b".to_string(), "orphan".to_string()],
        )
        .expect("topological sort should succeed");
        assert_eq!(info.need_debug_pkgs, vec!["lib_b", "app"]);
    }

    #[test]
    fn test_build_all_dependency_graph_errors_when_content_missing() {
        let mut conanfiles = HashMap::new();
        conanfiles.insert("app".to_string(), r#"requires = ["lib_a/1.0"]"#.to_string());
        // lib_a intentionally missing

        let input = MockDependencyQueryInput {
            conanfiles,
            all_dependencies: vec!["app".to_string(), "lib_a".to_string()],
        };

        let err = build_all_dependency_graph(&input).expect_err("missing content should fail");
        let msg = format!("{:#}", err);
        assert!(msg.contains("Missing mock conanfile for lib_a"));
    }
}
