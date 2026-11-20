"""Windows / Linux / macOS / Android 构建与打包入口。需要 Python 3.8+，仅使用标准库。"""
import argparse
import glob
import json
import os
import platform
import re
import shutil
import socket
import subprocess
import tarfile
import tempfile
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent
DESKTOP = {"windows", "linux", "macos"}
FILES = ("README.md", "LICENSE")

ANDROID_ABI = "arm64-v8a"
ANDROID_TARGET = "aarch64-linux-android"
ANDROID_API = "26"
ANDROID_NDK_VERSION = "27.2.12479018"
GRADLE_VERSION = "8.14.4"
FFMPEG_VERSION = "7.1.1"
X264_URL = (
    "https://code.videolan.org/videolan/x264/-/archive/stable/x264-stable.tar.bz2"
)
FFMPEG_URL = f"https://ffmpeg.org/releases/ffmpeg-{FFMPEG_VERSION}.tar.xz"


def host_platform():
    return {"Windows": "windows", "Linux": "linux", "Darwin": "macos"}.get(platform.system())


def run(command, cwd=ROOT, env=None, log=None):
    print("+ " + " ".join(str(argument) for argument in command), flush=True)
    if log is None:
        subprocess.run([str(argument) for argument in command], cwd=cwd, env=env, check=True)
        return
    with open(log, "ab") as output:
        output.write(("+ " + " ".join(str(argument) for argument in command) + "\n").encode())
        output.flush()
        result = subprocess.run(
            [str(argument) for argument in command],
            cwd=cwd,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
        )
    if result.returncode != 0:
        tail = Path(log).read_text(encoding="utf-8", errors="replace").splitlines()[-40:]
        raise RuntimeError("命令失败，日志 " + str(log) + "：\n" + "\n".join(tail))


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


# ---------------------------------------------------------------------------
# Android
#
# FFmpeg 沿用 PhiraPro 的做法：用 NDK 交叉编译静态 x264 与 ffmpeg CLI，
# 输出命名为 libffmpeg.so 放进 jniLibs；安装时随 APK 解压到 nativeLibraryDir，
# 应用启动后再复制到缓存目录并补上可执行位。这里把 PhiraPro 的构建脚本
# 移植成 Python，保持相同的源码版本与 configure 参数。
# ---------------------------------------------------------------------------


def android_sdk():
    local_app_data = os.environ.get("LOCALAPPDATA")
    candidates = [
        os.environ.get("ANDROID_HOME"),
        os.environ.get("ANDROID_SDK_ROOT"),
        Path.home() / "Android/Sdk",
        Path.home() / "Library/Android/sdk",
        Path(local_app_data) / "Android/Sdk" if local_app_data else None,
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_dir():
            return Path(candidate)
    raise ValueError("找不到 Android SDK；请设置 ANDROID_HOME")


def android_ndk(sdk):
    candidates = [
        os.environ.get("ANDROID_NDK_HOME"),
        sdk / "ndk" / ANDROID_NDK_VERSION,
    ]
    for candidate in candidates:
        if candidate and Path(candidate).is_dir():
            return Path(candidate)
    raise ValueError(
        f"找不到 Android NDK {ANDROID_NDK_VERSION}；请设置 ANDROID_NDK_HOME"
    )


def ndk_toolchain(ndk):
    if platform.system() == "Windows":
        host = "windows-x86_64"
    elif platform.system() == "Darwin":
        host = "darwin-x86_64"
    else:
        host = "linux-x86_64"
    toolchain = ndk / "toolchains/llvm/prebuilt" / host
    clang = toolchain / "bin" / f"{ANDROID_TARGET}{ANDROID_API}-clang"
    if not clang.is_file():
        raise ValueError(f"NDK 工具链不完整：{toolchain}")
    return toolchain


def download(url, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.is_file() and destination.stat().st_size > 0:
        return
    temporary = destination.with_name(destination.name + ".part")
    run(
        [
            "curl",
            "-fL",
            "--retry",
            "3",
            "--connect-timeout",
            "30",
            "-o",
            temporary,
            url,
        ]
    )
    os.replace(temporary, destination)


def extract(archive, destination):
    destination.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive) as source:
        source.extractall(destination)


def ffmpeg_build_dir():
    return ROOT / "target/android-ffmpeg"


def build_android_ffmpeg(ndk, force=False):
    """按 PhiraPro 的参数构建 arm64 FFmpeg，返回可执行文件路径。"""
    work = ffmpeg_build_dir()
    source_dir = work / "src"
    prefix = work / "prefix"
    output = work / "ffmpeg-arm64"
    if output.is_file() and not force:
        print(f"复用已构建的 FFmpeg：{output}")
        return output

    toolchain = ndk_toolchain(ndk)
    source_dir.mkdir(parents=True, exist_ok=True)
    prefix.mkdir(parents=True, exist_ok=True)
    work.mkdir(parents=True, exist_ok=True)
    jobs = str(max(1, os.cpu_count() or 1))

    x264_archive = source_dir / "x264.tar.bz2"
    x264_source = source_dir / "x264-stable"
    if not x264_source.is_dir():
        download(X264_URL, x264_archive)
        extract(x264_archive, source_dir)

    ffmpeg_archive = source_dir / f"ffmpeg-{FFMPEG_VERSION}.tar.xz"
    ffmpeg_source = source_dir / f"ffmpeg-{FFMPEG_VERSION}"
    if not ffmpeg_source.is_dir():
        download(FFMPEG_URL, ffmpeg_archive)
        extract(ffmpeg_archive, source_dir)

    bin_dir = toolchain / "bin"
    env = os.environ.copy()
    env.update(
        {
            "CC": str(bin_dir / f"{ANDROID_TARGET}{ANDROID_API}-clang"),
            "CXX": str(bin_dir / f"{ANDROID_TARGET}{ANDROID_API}-clang++"),
            "AR": str(bin_dir / "llvm-ar"),
            "NM": str(bin_dir / "llvm-nm"),
            "RANLIB": str(bin_dir / "llvm-ranlib"),
            "STRIP": str(bin_dir / "llvm-strip"),
            "PKG_CONFIG_PATH": str(prefix / "lib/pkgconfig"),
            "PKG_CONFIG_LIBDIR": str(prefix / "lib/pkgconfig"),
            "PATH": str(bin_dir) + os.pathsep + env.get("PATH", ""),
        }
    )
    toolchain_bin = str(bin_dir)

    x264_library = prefix / "lib/libx264.a"
    if not x264_library.is_file() or force:
        print("[ffmpeg] 编译 x264")
        run(
            [
                "./configure",
                f"--prefix={prefix}",
                "--host=aarch64-linux-android",
                f"--cross-prefix={toolchain_bin}/llvm-",
                f"--sysroot={toolchain}/sysroot",
                "--enable-static",
                "--enable-pic",
                "--disable-cli",
                "--disable-opencl",
                "--extra-cflags=-O2 -fPIC",
            ],
            cwd=x264_source,
            env=env,
            log=work / "x264-configure.log",
        )
        run(["make", f"-j{jobs}"], cwd=x264_source, env=env, log=work / "x264-make.log")
        run(["make", "install"], cwd=x264_source, env=env, log=work / "x264-install.log")

    print("[ffmpeg] 编译 ffmpeg")
    run(
        [
            "./configure",
            f"--prefix={prefix}",
            "--target-os=android",
            "--arch=aarch64",
            "--cpu=armv8-a",
            "--enable-cross-compile",
            f"--sysroot={toolchain}/sysroot",
            f"--cc={env['CC']}",
            f"--cxx={env['CXX']}",
            f"--ar={env['AR']}",
            f"--ranlib={env['RANLIB']}",
            f"--nm={env['NM']}",
            f"--strip={env['STRIP']}",
            "--enable-static",
            "--disable-shared",
            "--enable-pic",
            "--enable-ffmpeg",
            "--disable-ffplay",
            "--disable-ffprobe",
            "--disable-doc",
            "--disable-avdevice",
            "--disable-postproc",
            "--disable-network",
            "--disable-debug",
            "--disable-autodetect",
            "--enable-libx264",
            "--enable-gpl",
            f"--extra-cflags=-I{prefix}/include -O2 -fPIC",
            f"--extra-ldflags=-L{prefix}/lib",
            "--extra-libs=-lm",
        ],
        cwd=ffmpeg_source,
        env=env,
        log=work / "ffmpeg-configure.log",
    )
    run(
        ["make", f"-j{jobs}", "ffmpeg"],
        cwd=ffmpeg_source,
        env=env,
        log=work / "ffmpeg-make.log",
    )
    shutil.copy2(ffmpeg_source / "ffmpeg", output)
    run([env["STRIP"], "--strip-all", output], cwd=work, env=env)
    output.chmod(0o755)
    print(f"[ffmpeg] 完成：{output}（{output.stat().st_size / 1024 / 1024:.1f} MB）")
    return output


def gradle_environment():
    """返回 Gradle 环境；若全局配置里的代理已经不可用，则让 JVM 直连。"""
    env = os.environ.copy()
    direct = False
    properties = Path.home() / ".gradle/gradle.properties"
    if properties.is_file():
        text = properties.read_text(encoding="utf-8", errors="replace")
        host = re.search(r"(?m)^systemProp\.https?\.proxyHost\s*=\s*(\S+)", text)
        port = re.search(r"(?m)^systemProp\.https?\.proxyPort\s*=\s*(\d+)", text)
        if host and port:
            try:
                with socket.create_connection((host.group(1), int(port.group(1))), timeout=1):
                    pass
            except OSError:
                direct = True
    if direct:
        options = env.get("GRADLE_OPTS", "")
        env["GRADLE_OPTS"] = (
            options + " -Dhttp.nonProxyHosts=* -Dhttps.nonProxyHosts=*"
        ).strip()
        print("检测到 Gradle 全局代理不可用，本次构建改为直连")
    return env, direct


def gradle_binary():
    configured = os.environ.get("GRADLE")
    if configured and Path(configured).is_file():
        return Path(configured)
    pattern = (
        Path.home()
        / ".gradle/wrapper/dists"
        / f"gradle-{GRADLE_VERSION}-bin"
        / "*"
        / f"gradle-{GRADLE_VERSION}"
        / "bin"
        / "gradle"
    )
    for candidate in glob.glob(str(pattern)):
        if Path(candidate).is_file():
            return Path(candidate)
    found = shutil.which("gradle")
    if found:
        return Path(found)

    # 没有现成 Gradle 时下载官方发行包，缓存在 target 下。
    destination = ROOT / "target/android-gradle" / f"gradle-{GRADLE_VERSION}"
    archive = destination.parent / f"gradle-{GRADLE_VERSION}-bin.zip"
    if not (destination / "bin/gradle").is_file():
        download(
            f"https://services.gradle.org/distributions/gradle-{GRADLE_VERSION}-bin.zip",
            archive,
        )
        with zipfile.ZipFile(archive) as source:
            source.extractall(destination.parent)
    return destination / "bin/gradle"


def build_android(version, delivery, args):
    if platform.system() not in ("Linux", "Darwin", "Windows"):
        raise ValueError("Android 构建只支持 Linux / macOS / Windows 主机")
    sdk = android_sdk()
    ndk = android_ndk(sdk)
    print(f"PPR Render v{version} · Android · {ANDROID_ABI}")

    if not args.skip_ffmpeg:
        ffmpeg = build_android_ffmpeg(ndk, force=args.force_ffmpeg)
    else:
        ffmpeg = ffmpeg_build_dir() / "ffmpeg-arm64"
        if not ffmpeg.is_file():
            raise ValueError("--skip-ffmpeg 需要已有的 target/android-ffmpeg/ffmpeg-arm64")

    try:
        subprocess.run(
            ["rustup", "target", "add", ANDROID_TARGET],
            cwd=ROOT,
            check=True,
        )
    except FileNotFoundError:
        pass

    gradle = gradle_binary()
    env, direct = gradle_environment()
    env.setdefault("ANDROID_HOME", str(sdk))
    env.setdefault("ANDROID_NDK_HOME", str(ndk))
    command = [
        gradle,
        "-p",
        ROOT / "android",
        "assembleRelease",
        f"-PpprFfmpeg={ffmpeg}",
    ]
    if direct:
        # 新的 GRADLE_OPTS 只对新建的守护进程生效。
        command.append("--no-daemon")
    if args.offline:
        command.append("-PpprCargoOffline=true")
        command.append("--offline")
    run(command, cwd=ROOT, env=env)

    apk = ROOT / "android/app/build/outputs/apk/release/app-release.apk"
    if not apk.is_file():
        unsigned = apk.with_name("app-release-unsigned.apk")
        raise ValueError(f"没有找到签名后的 APK：{apk}（未签名版本：{unsigned}）")
    delivery.mkdir(parents=True, exist_ok=True)
    target = delivery / f"ppr-render-v{version}-android-{ANDROID_ABI}.apk"
    shutil.copy2(apk, target)
    print(f"已生成 {target}（{target.stat().st_size / 1024 / 1024:.1f} MB）")
    return target


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "action",
        nargs="?",
        choices=("desktop", "android"),
        default="desktop",
        help="构建目标；默认 desktop",
    )
    parser.add_argument(
        "--platform",
        choices=sorted(DESKTOP),
        default=host_platform(),
        help="桌面目标平台；默认当前系统",
    )
    parser.add_argument(
        "--package", action="store_true", help="桌面构建后生成包含程序、文档和许可证的 ZIP"
    )
    parser.add_argument(
        "--skip-build", action="store_true", help="跳过桌面构建，直接打包已有的 release 程序"
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="产物输出目录；默认 ppr-render/dist",
    )
    parser.add_argument(
        "--force-ffmpeg", action="store_true", help="忽略缓存重新编译 Android FFmpeg"
    )
    parser.add_argument(
        "--skip-ffmpeg", action="store_true", help="跳过 Android FFmpeg 构建（需要已有产物）"
    )
    parser.add_argument("--offline", action="store_true", help="Cargo 与 Gradle 使用离线缓存")
    args = parser.parse_args()
    try:
        version = cargo_version()
        if args.action == "android":
            build_android(version, (args.output or ROOT / "dist").resolve(), args)
            return

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

        target = os.environ.get("CARGO_BUILD_TARGET") or rust_host_target()
        print(f"PPR Render v{version} · {args.platform} · {target}")

        if not args.skip_build:
            command = ["cargo", "build", "--locked", "--release"]
            if args.offline:
                command.append("--offline")
            run(command)

        if args.package:
            binary_dir, target = release_directory()
            package_desktop(
                binary_dir,
                (args.output or ROOT / "dist").resolve(),
                target,
                version,
            )
    except (ValueError, OSError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"构建失败：{error}\n")


if __name__ == "__main__":
    main()
