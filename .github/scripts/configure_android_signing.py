#!/usr/bin/env python3
"""把 Android 签名 Secret 恢复成 Gradle 可读取的 keystore.properties。

Android Job 在构建前调用本脚本；密钥写到 runner 临时目录，不进入仓库，
构建结束后随 runner 销毁。本地构建仍可直接在 android/keystore.properties
填写自己的签名配置。
"""

import base64
import os
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ANDROID = ROOT / "android"
PROPERTIES = ANDROID / "keystore.properties"
SECRET_NAMES = (
    "ANDROID_KEYSTORE_BASE64",
    "ANDROID_KEYSTORE_PASSWORD",
    "ANDROID_KEY_ALIAS",
    "ANDROID_KEY_PASSWORD",
)


def java_property(value):
    """按 java.util.Properties 的格式转义一个值。"""
    pieces = []
    for index, char in enumerate(value):
        code = ord(char)
        if char == "\\":
            pieces.append("\\\\")
        elif char == "\n":
            pieces.append("\\n")
        elif char == "\r":
            pieces.append("\\r")
        elif char == "\t":
            pieces.append("\\t")
        elif char == " " and index == 0:
            pieces.append("\\ ")
        elif 0x20 <= code <= 0x7E:
            pieces.append(char)
        elif code > 0xFFFF:
            code -= 0x10000
            pieces.append(
                "\\u%04x\\u%04x"
                % (0xD800 + (code >> 10), 0xDC00 + (code & 0x3FF))
            )
        else:
            pieces.append("\\u%04x" % code)
    return "".join(pieces)


def fail(message):
    print("::error::" + message, file=sys.stderr)
    return 1


def main():
    values = {name: os.environ.get(name, "") for name in SECRET_NAMES}
    missing = [name for name in SECRET_NAMES if not values[name].strip()]
    if missing:
        return fail("缺少 Android 签名配置：" + "、".join(missing))

    encoded = "".join(values["ANDROID_KEYSTORE_BASE64"].split())
    try:
        keystore_bytes = base64.b64decode(encoded, validate=True)
    except Exception as error:
        return fail("ANDROID_KEYSTORE_BASE64 不是有效的 Base64：" + str(error))
    if not keystore_bytes:
        return fail("ANDROID_KEYSTORE_BASE64 解码后为空")

    runner_temp = os.environ.get("RUNNER_TEMP") or tempfile.gettempdir()
    keystore = Path(runner_temp) / "ppr-render-release.jks"
    keystore.write_bytes(keystore_bytes)
    keystore.chmod(0o600)

    properties = [
        "storeFile=" + java_property(keystore.as_posix()),
        "storePassword=" + java_property(values["ANDROID_KEYSTORE_PASSWORD"]),
        "keyAlias=" + java_property(values["ANDROID_KEY_ALIAS"]),
        "keyPassword=" + java_property(values["ANDROID_KEY_PASSWORD"]),
    ]
    ANDROID.mkdir(parents=True, exist_ok=True)
    PROPERTIES.write_text(
        "\n".join(properties) + "\n",
        encoding="ascii",
        newline="\n",
    )
    PROPERTIES.chmod(0o600)
    print("已生成 android/keystore.properties，签名文件位于 runner 临时目录")
    return 0


if __name__ == "__main__":
    sys.exit(main())
