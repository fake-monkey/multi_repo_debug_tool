from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tomllib
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Sequence


PACKAGE_NAME = "repo_debug"
TARGET_TRIPLE = "x86_64-pc-windows-msvc"
GITHUB_REPOSITORY = "fake-monkey/multi_repo_debug_tool"
GITHUB_REPOSITORY_URL = "https://github.com/fake-monkey/multi_repo_debug_tool"

STABLE_VERSION_PATTERN = re.compile(r"^(?P<base>\d+\.\d+\.\d+)(?:\+[0-9A-Za-z.-]+)?$")
ALPHA_VERSION_PATTERN = re.compile(
    r"^(?P<base>\d+\.\d+\.\d+)-alpha\.(?P<number>0|[1-9]\d*)(?:\+[0-9A-Za-z.-]+)?$"
)


class ReleaseError(RuntimeError):
    pass


@dataclass(frozen=True)
class Channel:
    name: str
    app_name: str
    manifest_name: str
    prerelease: bool
    checkver_regex: str


@dataclass(frozen=True)
class ReleaseContext:
    repo_root: Path
    workspace_root: Path
    target_directory: Path
    version: str
    channel: Channel
    tag: str
    archive_name: str
    archive_url: str
    output_directory: Path

    @property
    def manifest_path(self) -> Path:
        return self.repo_root / "bucket" / self.channel.manifest_name

    @property
    def archive_path(self) -> Path:
        return self.output_directory / self.archive_name

    @property
    def executable_path(self) -> Path:
        return (
            self.target_directory
            / TARGET_TRIPLE
            / "release"
            / f"{PACKAGE_NAME}.exe"
        )


def print_step(message: str) -> None:
    print(f"\n==> {message}", flush=True)


def display_command(command: Sequence[str]) -> str:
    return subprocess.list2cmdline([str(argument) for argument in command])


def run_command(
    command: Sequence[str],
    *,
    cwd: Path,
    capture_output: bool = False,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    arguments = [str(argument) for argument in command]
    print(f"> {display_command(arguments)}", flush=True)
    result = subprocess.run(
        arguments,
        cwd=cwd,
        check=False,
        text=True,
        encoding="utf-8",
        errors="replace",
        stdout=subprocess.PIPE if capture_output else None,
        stderr=subprocess.PIPE if capture_output else None,
    )
    if check and result.returncode != 0:
        details = ""
        if capture_output:
            details = (result.stderr or result.stdout or "").strip()
        suffix = f"\n{details}" if details else ""
        raise ReleaseError(
            f"命令执行失败（退出码 {result.returncode}）：{display_command(arguments)}{suffix}"
        )
    return result


def command_output(command: Sequence[str], *, cwd: Path) -> str:
    return run_command(command, cwd=cwd, capture_output=True).stdout.strip()


def require_command(name: str) -> None:
    if shutil.which(name) is None:
        raise ReleaseError(f"找不到必需命令：{name}")


def load_package_version(repo_root: Path) -> str:
    cargo_toml = repo_root / "Cargo.toml"
    with cargo_toml.open("rb") as stream:
        document = tomllib.load(stream)

    package = document.get("package", {})
    if package.get("name") != PACKAGE_NAME:
        raise ReleaseError(
            f"{cargo_toml} 的 package.name 不是预期的 {PACKAGE_NAME}"
        )

    version = package.get("version")
    if not isinstance(version, str) or not version:
        raise ReleaseError(f"{cargo_toml} 未提供有效的 package.version")
    return version


def resolve_channel(version: str) -> Channel:
    if STABLE_VERSION_PATTERN.fullmatch(version):
        return Channel(
            name="stable",
            app_name="repo_debug",
            manifest_name="repo_debug.json",
            prerelease=False,
            checkver_regex=r'"tag_name"\s*:\s*"v(?<version>\d+\.\d+\.\d+)"',
        )
    if ALPHA_VERSION_PATTERN.fullmatch(version):
        return Channel(
            name="alpha",
            app_name="repo_debug-alpha",
            manifest_name="repo_debug-alpha.json",
            prerelease=True,
            checkver_regex=(
                r'"tag_name"\s*:\s*"v(?<version>\d+\.\d+\.\d+-alpha\.\d+)"'
            ),
        )
    raise ReleaseError(
        f"版本 {version!r} 不属于 stable 或 alpha 通道；"
        "只接受 X.Y.Z 或 X.Y.Z-alpha.N"
    )


def load_cargo_metadata(repo_root: Path) -> dict[str, Any]:
    output = command_output(
        [
            "cargo",
            "metadata",
            "--manifest-path",
            str(repo_root / "Cargo.toml"),
            "--no-deps",
            "--format-version",
            "1",
        ],
        cwd=repo_root,
    )
    try:
        return json.loads(output)
    except json.JSONDecodeError as error:
        raise ReleaseError(f"无法解析 cargo metadata 输出：{error}") from error


def create_context(repo_root: Path) -> ReleaseContext:
    version = load_package_version(repo_root)
    channel = resolve_channel(version)
    metadata = load_cargo_metadata(repo_root)
    workspace_root = Path(metadata["workspace_root"]).resolve()
    target_directory = Path(metadata["target_directory"]).resolve()
    archive_name = f"repo_debug-v{version}-{TARGET_TRIPLE}.zip"
    tag = f"v{version}"
    return ReleaseContext(
        repo_root=repo_root,
        workspace_root=workspace_root,
        target_directory=target_directory,
        version=version,
        channel=channel,
        tag=tag,
        archive_name=archive_name,
        archive_url=(
            f"{GITHUB_REPOSITORY_URL}/releases/download/{tag}/{archive_name}"
        ),
        output_directory=repo_root / ".release" / version,
    )


def extract_changelog_body(changelog_path: Path, version: str) -> str:
    text = changelog_path.read_text(encoding="utf-8")
    heading = re.compile(rf"^## \[{re.escape(version)}\]\s*$", re.MULTILINE)
    match = heading.search(text)
    if match is None:
        raise ReleaseError(f"{changelog_path} 中不存在版本 {version} 的二级标题")

    next_heading = re.search(r"^##\s+", text[match.end() :], re.MULTILINE)
    end = match.end() + next_heading.start() if next_heading else len(text)
    body = text[match.end() : end].strip()
    if not body:
        raise ReleaseError(f"{changelog_path} 中版本 {version} 的发布说明为空")
    return body


def ensure_formal_preconditions(context: ReleaseContext) -> None:
    print_step("校验正式发布前置条件")
    for command in ("git", "cargo", "gh"):
        require_command(command)

    status = command_output(
        ["git", "-C", str(context.repo_root), "status", "--porcelain"],
        cwd=context.repo_root,
    )
    if status:
        raise ReleaseError("子仓库存在未提交改动；请先提交并推送发布源文件")

    branch = command_output(
        ["git", "-C", str(context.repo_root), "branch", "--show-current"],
        cwd=context.repo_root,
    )
    if not branch:
        raise ReleaseError("当前处于 detached HEAD，不能执行正式发布")

    run_command(
        ["git", "-C", str(context.repo_root), "fetch", "--prune", "origin"],
        cwd=context.repo_root,
    )
    upstream = command_output(
        [
            "git",
            "-C",
            str(context.repo_root),
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ],
        cwd=context.repo_root,
    )
    ahead_behind = command_output(
        [
            "git",
            "-C",
            str(context.repo_root),
            "rev-list",
            "--left-right",
            "--count",
            f"{upstream}...HEAD",
        ],
        cwd=context.repo_root,
    )
    if ahead_behind.split() != ["0", "0"]:
        raise ReleaseError(
            f"当前分支与 {upstream} 未完全同步（behind/ahead: {ahead_behind}）"
        )

    run_command(["gh", "auth", "status"], cwd=context.repo_root)


def build_release(context: ReleaseContext) -> None:
    print_step(f"构建 {PACKAGE_NAME} {context.version}")
    run_command(
        [
            "cargo",
            "build",
            "--release",
            "--locked",
            "--manifest-path",
            str(context.workspace_root / "Cargo.toml"),
            "-p",
            PACKAGE_NAME,
            "--target",
            TARGET_TRIPLE,
        ],
        cwd=context.repo_root,
    )
    if not context.executable_path.is_file():
        raise ReleaseError(f"构建成功后未找到 {context.executable_path}")


def create_archive(context: ReleaseContext) -> str:
    print_step(f"生成发布压缩包 {context.archive_name}")
    context.output_directory.mkdir(parents=True, exist_ok=True)
    archive_inputs = (
        (context.executable_path, "repo_debug.exe"),
        (context.repo_root / "README.md", "README.md"),
        (context.repo_root / "CHANGELOG.md", "CHANGELOG.md"),
    )
    for source, _ in archive_inputs:
        if not source.is_file():
            raise ReleaseError(f"发布文件不存在：{source}")

    with zipfile.ZipFile(
        context.archive_path, mode="w", compression=zipfile.ZIP_DEFLATED
    ) as archive:
        for source, archive_name in archive_inputs:
            entry = zipfile.ZipInfo(archive_name, date_time=(1980, 1, 1, 0, 0, 0))
            entry.compress_type = zipfile.ZIP_DEFLATED
            entry.external_attr = 0o644 << 16
            archive.writestr(entry, source.read_bytes())

    with zipfile.ZipFile(context.archive_path, mode="r") as archive:
        actual_names = sorted(archive.namelist())
    expected_names = sorted(name for _, name in archive_inputs)
    if actual_names != expected_names:
        raise ReleaseError(
            f"ZIP 内容不符合预期：expected={expected_names}, actual={actual_names}"
        )

    digest = hashlib.sha256(context.archive_path.read_bytes()).hexdigest()
    print(f"SHA256: {digest}")
    return digest


def make_manifest(context: ReleaseContext, sha256: str) -> dict[str, Any]:
    release_api = (
        f"https://api.github.com/repos/{GITHUB_REPOSITORY}/releases?per_page=100"
    )
    manifest: dict[str, Any] = {
        "version": context.version,
        "description": "Windows + CMake Presets + Conan 多仓库本地联调工具",
        "homepage": GITHUB_REPOSITORY_URL,
        "license": "Unknown",
        "architecture": {
            "64bit": {
                "url": context.archive_url,
                "hash": sha256,
            }
        },
        "bin": "repo_debug.exe",
        "checkver": {
            "url": release_api,
            "regex": context.channel.checkver_regex,
        },
        "autoupdate": {
            "architecture": {
                "64bit": {
                    "url": (
                        f"{GITHUB_REPOSITORY_URL}/releases/download/"
                        f"v$version/repo_debug-v$version-{TARGET_TRIPLE}.zip"
                    )
                }
            }
        },
    }
    if context.channel.name == "stable":
        manifest["checkver"] = "github"
    return manifest


def validate_manifest(
    context: ReleaseContext, manifest: dict[str, Any], sha256: str
) -> None:
    architecture = manifest.get("architecture", {}).get("64bit", {})
    expected = {
        "version": context.version,
        "bin": "repo_debug.exe",
        "url": context.archive_url,
        "hash": sha256,
    }
    actual = {
        "version": manifest.get("version"),
        "bin": manifest.get("bin"),
        "url": architecture.get("url"),
        "hash": architecture.get("hash"),
    }
    if actual != expected:
        raise ReleaseError(f"manifest 校验失败：expected={expected}, actual={actual}")
    if not re.fullmatch(r"[0-9a-f]{64}", sha256):
        raise ReleaseError("SHA256 格式无效")
    if context.channel.name not in ("stable", "alpha"):
        raise ReleaseError(f"未知发布通道：{context.channel.name}")


def write_manifest(path: Path, manifest: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(manifest_text(manifest), encoding="utf-8")
    with path.open("r", encoding="utf-8") as stream:
        json.load(stream)


def manifest_text(manifest: dict[str, Any]) -> str:
    return json.dumps(manifest, ensure_ascii=False, indent=2) + "\n"


def git_ref_target(repo_root: Path, ref: str) -> str | None:
    result = run_command(
        ["git", "-C", str(repo_root), "rev-list", "-n", "1", ref],
        cwd=repo_root,
        capture_output=True,
        check=False,
    )
    return result.stdout.strip() if result.returncode == 0 else None


def git_path_is_tracked(repo_root: Path, relative_path: Path) -> bool:
    result = run_command(
        [
            "git",
            "-C",
            str(repo_root),
            "ls-files",
            "--error-unmatch",
            "--",
            relative_path.as_posix(),
        ],
        cwd=repo_root,
        capture_output=True,
        check=False,
    )
    return result.returncode == 0


def remote_tag_target(repo_root: Path, tag: str) -> str | None:
    output = command_output(
        [
            "git",
            "-C",
            str(repo_root),
            "ls-remote",
            "--tags",
            "origin",
            f"refs/tags/{tag}",
            f"refs/tags/{tag}^{{}}",
        ],
        cwd=repo_root,
    )
    if not output:
        return None
    lines = output.splitlines()
    peeled = next((line for line in lines if line.endswith("^{}")), None)
    return (peeled or lines[0]).split()[0]


def commit_manifest(context: ReleaseContext) -> str:
    print_step(f"提交 {context.channel.name} manifest")
    relative_manifest = context.manifest_path.relative_to(context.repo_root)
    changed = run_command(
        [
            "git",
            "-C",
            str(context.repo_root),
            "diff",
            "--quiet",
            "--",
            str(relative_manifest),
        ],
        cwd=context.repo_root,
        check=False,
    ).returncode

    untracked = not git_path_is_tracked(context.repo_root, relative_manifest)
    if changed == 0 and not untracked:
        print("manifest 内容未变化，复用当前 HEAD。")
        return command_output(
            ["git", "-C", str(context.repo_root), "rev-parse", "HEAD"],
            cwd=context.repo_root,
        )
    if changed not in (0, 1):
        raise ReleaseError("无法判断 manifest 是否发生变化")

    run_command(
        [
            "git",
            "-C",
            str(context.repo_root),
            "add",
            "--",
            str(relative_manifest),
        ],
        cwd=context.repo_root,
    )
    run_command(
        [
            "git",
            "-C",
            str(context.repo_root),
            "commit",
            "-m",
            (
                f"chore(release): update Scoop {context.channel.name} "
                f"manifest for {context.version}"
            ),
        ],
        cwd=context.repo_root,
    )
    branch = command_output(
        ["git", "-C", str(context.repo_root), "branch", "--show-current"],
        cwd=context.repo_root,
    )
    run_command(
        ["git", "-C", str(context.repo_root), "push", "origin", branch],
        cwd=context.repo_root,
    )
    return command_output(
        ["git", "-C", str(context.repo_root), "rev-parse", "HEAD"],
        cwd=context.repo_root,
    )


def create_tag(context: ReleaseContext, target_commit: str) -> None:
    print_step(f"创建标签 {context.tag}")
    run_command(
        ["git", "-C", str(context.repo_root), "tag", context.tag, target_commit],
        cwd=context.repo_root,
    )
    run_command(
        ["git", "-C", str(context.repo_root), "push", "origin", context.tag],
        cwd=context.repo_root,
    )


def load_release(context: ReleaseContext) -> dict[str, Any] | None:
    result = run_command(
        [
            "gh",
            "release",
            "view",
            context.tag,
            "--repo",
            GITHUB_REPOSITORY,
            "--json",
            "tagName,name,body,isDraft,isPrerelease,targetCommitish,url,assets",
        ],
        cwd=context.repo_root,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        details = (result.stderr or result.stdout or "").strip()
        if "release not found" in details.lower():
            return None
        raise ReleaseError(f"查询 GitHub Release 失败：{details}")
    try:
        release = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise ReleaseError(f"无法解析 GitHub Release：{error}") from error
    if not isinstance(release, dict):
        raise ReleaseError("GitHub Release 返回格式不符合预期")
    return release


def ensure_version_is_unpublished(context: ReleaseContext) -> None:
    print_step(f"检查版本 {context.version} 是否已经发布")
    occupied_by: list[str] = []
    if git_ref_target(context.repo_root, context.tag) is not None:
        occupied_by.append("本地 Git 标签")
    if remote_tag_target(context.repo_root, context.tag) is not None:
        occupied_by.append("远端 Git 标签")
    if load_release(context) is not None:
        occupied_by.append("GitHub Release")
    if occupied_by:
        locations = "、".join(occupied_by)
        raise ReleaseError(
            f"版本 {context.version}（标签 {context.tag}）已经存在于{locations}，"
            "禁止直接覆盖或重复发布。请升级 Cargo.toml 中的版本号并在 "
            "CHANGELOG.md 中增加对应版本章节；如果已有发布确认作废，"
            "也可以先删除该 GitHub Release 及对应的本地、远端标签后重试。"
        )


def ensure_release(
    context: ReleaseContext, target_commit: str, changelog_body: str
) -> dict[str, Any]:
    print_step(f"创建 GitHub Release {context.tag}")
    command = [
        "gh",
        "release",
        "create",
        context.tag,
        str(context.archive_path),
        "--repo",
        GITHUB_REPOSITORY,
        "--title",
        context.tag,
        "--notes",
        changelog_body,
        "--target",
        target_commit,
        "--verify-tag",
    ]
    if context.channel.prerelease:
        command.append("--prerelease")
        command.append("--latest=false")
    run_command(command, cwd=context.repo_root)
    release = load_release(context)
    if release is None:
        raise ReleaseError("GitHub Release 创建后无法重新查询")

    expected = {
        "tagName": context.tag,
        "name": context.tag,
        "body": changelog_body,
        "isDraft": False,
        "isPrerelease": context.channel.prerelease,
    }
    actual = {key: release.get(key) for key in expected}
    actual["body"] = str(actual["body"]).replace("\r\n", "\n")
    if actual != expected:
        raise ReleaseError(
            f"新建的 GitHub Release 与本次发布不一致："
            f"expected={expected}, actual={actual}"
        )
    return release


def ensure_attachment(context: ReleaseContext, release: dict[str, Any], sha256: str) -> None:
    print_step(f"确认 Release 附件 {context.archive_name}")
    assets = release.get("assets")
    if not isinstance(assets, list):
        raise ReleaseError("GitHub Release 附件列表返回格式不符合预期")
    matching = [asset for asset in assets if asset.get("name") == context.archive_name]
    if len(matching) != 1:
        raise ReleaseError(
            f"新建 Release 中没有唯一的预期附件：{context.archive_name}"
        )

    remote_size = matching[0].get("size")
    local_size = context.archive_path.stat().st_size
    if not isinstance(remote_size, int) or remote_size != local_size:
        raise ReleaseError(
            f"同名附件大小不一致：remote={remote_size}, local={local_size}"
        )

    verification_path = context.output_directory / f"verified-{context.archive_name}"
    run_command(
        [
            "gh",
            "release",
            "download",
            context.tag,
            "--repo",
            GITHUB_REPOSITORY,
            "--pattern",
            context.archive_name,
            "--output",
            str(verification_path),
            "--clobber",
        ],
        cwd=context.repo_root,
    )
    remote_sha256 = hashlib.sha256(verification_path.read_bytes()).hexdigest()
    if remote_sha256 != sha256:
        raise ReleaseError(
            f"同名附件 SHA256 不一致：remote={remote_sha256}, local={sha256}"
        )
    print("同名附件已存在且内容一致。")


def perform_dry_run(
    context: ReleaseContext, manifest: dict[str, Any]
) -> None:
    print_step("校验 dry-run manifest")
    print(manifest_text(manifest), end="")
    print(f"产物目录：{context.output_directory}")
    print(f"正式 manifest 路径：{context.manifest_path}")
    print("dry-run 完成：未修改受 Git 跟踪文件，未执行远端操作。")


def perform_formal_release(
    context: ReleaseContext,
    manifest: dict[str, Any],
    changelog_body: str,
    sha256: str,
) -> None:
    ensure_version_is_unpublished(context)
    write_manifest(context.manifest_path, manifest)
    target_commit = commit_manifest(context)
    create_tag(context, target_commit)
    release = ensure_release(context, target_commit, changelog_body)
    ensure_attachment(context, release, sha256)
    print_step("正式发布完成")
    print(f"Release：{GITHUB_REPOSITORY_URL}/releases/tag/{context.tag}")
    print(f"附件：{context.archive_url}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="构建并发布 repo_debug 到 GitHub/Scoop")
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="构建并在终端预览 manifest，但不修改跟踪文件或远端状态",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    repo_root = Path(__file__).resolve().parent.parent
    try:
        require_command("cargo")
        context = create_context(repo_root)
        changelog_body = extract_changelog_body(
            context.repo_root / "CHANGELOG.md", context.version
        )
        print(f"版本：{context.version}", flush=True)
        print(f"通道：{context.channel.name}", flush=True)
        print(f"manifest：{context.channel.manifest_name}", flush=True)

        if not args.dry_run:
            ensure_formal_preconditions(context)
            ensure_version_is_unpublished(context)

        build_release(context)
        sha256 = create_archive(context)
        manifest = make_manifest(context, sha256)
        validate_manifest(context, manifest, sha256)

        if args.dry_run:
            perform_dry_run(context, manifest)
        else:
            perform_formal_release(context, manifest, changelog_body, sha256)
        return 0
    except (OSError, ReleaseError, tomllib.TOMLDecodeError) as error:
        print(f"\nERROR: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
