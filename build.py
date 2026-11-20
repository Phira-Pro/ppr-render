"""Windows / Linux / macOS 构建与打包入口。需要 Python 3.8+，仅使用标准库。"""
import argparse
import json
import os
import platform
import re
import shutil
import subprocess
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
DESKTOP = {"windows", "linux", "macos"}
FILES = ("README.md", "LICENSE")


def host_platform():
    return {"Windows": "windows", "Linux": "linux", "Darwin": "macos"}.get(platform.system())


def run(command, cwd=ROOT):
    print("+ " + " ".join(str(argument) for argument in command), flush=True)
    subprocess.run([str(argument) for argument in command], cwd=cwd, check=True)


def cargo_version():
    text = (ROOT / "Cargo.toml").read_text(encoding="utf-8")
    package = re.search(r"(?ms)^\[package\]\s*(.*?)(?=^\[|\Z)", text)
    if package is None:
        raise ValueError("Cargo.toml 缺少 [package] 段")
    version = re.search(r'(?m)^version\s*=\s*"([^"]+)"', package.group(1))
    if version is None:
        raise ValueError("Cargo.toml 缺少 package.version")
    return version.group(1)


def rust_host_target():
    output = subprocess.check_output(["rustc", "-vV"], text=True)
    for line in output.splitlines():
        if line.startswith("host: "):
            return line[6:].strip()
    raise ValueError("无法从 rustc -vV 读取主机目标")


def desktop_suffix(target):
    arch, _, system, *_ = target.split("-")
    if system == "windows":
        return "win64" if arch == "x86_64" else "windows-" + arch
    if system == "linux":
        return "linux-" + arch
    if system == "darwin":
        return "macos-" + arch
    raise ValueError("不支持的桌面 Rust 目标：" + target)


def release_directory():
    metadata = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"],
            cwd=ROOT,
            text=True,
        )
    )
    directory = Path(metadata["target_directory"])
    configured = os.environ.get("CARGO_BUILD_TARGET")
    target = configured or rust_host_target()
    if configured:
        directory /= configured
    return directory / "release", target


def package_desktop(binary_dir, delivery, target, version):
    suffix = desktop_suffix(target)
    name = f"ppr-render-v{version}-{suffix}"
    executable = "ppr-render.exe" if "windows" in target else "ppr-render"
    binary = binary_dir / executable
    if not binary.is_file():
        raise ValueError(f"缺少构建产物 {binary}；请先执行完整构建")

    delivery.mkdir(parents=True, exist_ok=True)
    archive = delivery / (name + ".zip")
    # 只清理本次临时目录，不碰用户已解压的同名目录。
    with tempfile.TemporaryDirectory(prefix=".ppr-render-package-", dir=delivery) as temporary:
        Path(temporary).resolve().relative_to(delivery.resolve())
        stage = Path(temporary) / name
        stage.mkdir()
        shutil.copy2(binary, stage / executable)
        for filename in FILES:
            shutil.copy2(ROOT / filename, stage / filename)
        docs = ROOT / "docs"
        if docs.is_dir():
            shutil.copytree(docs, stage / "docs")
        if "windows" in target:
            for library in binary_dir.glob("*.dll"):
                shutil.copy2(library, stage / library.name)

        staged_zip = Path(temporary) / archive.name
        with zipfile.ZipFile(
            staged_zip, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9
        ) as output:
            for path in sorted(stage.rglob("*")):
                if path.is_file():
                    output.write(path, path.relative_to(Path(temporary)).as_posix())
        with zipfile.ZipFile(staged_zip) as output:
            if output.testzip() is not None:
                raise ValueError("ZIP 完整性检查失败")
        os.replace(staged_zip, archive)

    print(f"已生成 {archive}")
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--platform",
        choices=sorted(DESKTOP),
        default=host_platform(),
        help="目标桌面平台；默认当前系统",
    )
    parser.add_argument(
        "--package", action="store_true", help="构建后生成包含程序、文档和许可证的 ZIP"
    )
    parser.add_argument(
        "--skip-build", action="store_true", help="跳过构建，直接打包已有的 release 程序"
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="ZIP 输出目录；默认 ppr-render/dist",
    )
    args = parser.parse_args()
    try:
        host = host_platform()
        if host is None:
            raise ValueError("无法识别当前系统；可选 --platform windows/linux/macos")
        if args.platform != host:
            raise ValueError(
                f"请在 {args.platform} 主机上构建该平台，当前系统是 {host}"
            )
        if args.skip_build and not args.package:
            raise ValueError("--skip-build 需要与 --package 一起使用")
        if args.output and not args.package:
            raise ValueError("--output 需要与 --package 一起使用")

        version = cargo_version()
        target = os.environ.get("CARGO_BUILD_TARGET") or rust_host_target()
        print(f"PPR Render v{version} · {args.platform} · {target}")

        if not args.skip_build:
            run(["cargo", "build", "--locked", "--release"])

        if args.package:
            binary_dir, target = release_directory()
            package_desktop(
                binary_dir,
                (args.output or ROOT / "dist").resolve(),
                target,
                version,
            )
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"构建失败：{error}\n")


if __name__ == "__main__":
    main()
