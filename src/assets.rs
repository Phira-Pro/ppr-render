//! 内置渲染资源。
//!
//! 桌面播放器以单个可执行文件分发；首次启动时把字体、默认资源包与 HUD
//! 贴图解压到系统临时目录，再交给 macroquad 的资源加载器。谱面、音乐和
//! 曲绘仍从用户选择的谱面目录读取，不复制、不上传。
//!
//! Android 的 macroquad 资源加载器直接读取 APK 内的 assets，不经过文件
//! 系统，所以这些文件由 Gradle 原样打进 APK，不在 Rust 侧重复嵌入。

#[cfg(not(target_os = "android"))]
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[cfg(not(target_os = "android"))]
macro_rules! bundled {
    ($($path:literal),* $(,)?) => {
        &[$(($path, include_bytes!(concat!("../assets/", $path)) as &[u8])),*]
    };
}

/// 与 `assets/` 目录内容一一对应。新增贴图时同时更新这里。
#[cfg(not(target_os = "android"))]
const BUNDLED: &[(&str, &[u8])] = bundled![
    "back.png",
    "background.jpg",
    "click.ogg",
    "drag.ogg",
    "ending.ogg",
    "error.png",
    "flick.ogg",
    "harmonyos.ttf",
    "icon.png",
    "info.png",
    "mod/autoplay.png",
    "mod/fade_in.png",
    "mod/fade_out.png",
    "mod/flip_x.png",
    "mod/fullscreen-judge.png",
    "mod/nightcore.png",
    "mod/no-combo-score.png",
    "mod/no-fail.png",
    "mod/no-shader.png",
    "mod/perfect-sound.png",
    "mod/rainbow.png",
    "mod/sudden-death.png",
    "offset_indicator.png",
    "ok.png",
    "phigros.ttf",
    "player.jpg",
    "proceed.png",
    "resume.png",
    "retry.png",
    "warn.png",
    "rank/A.png",
    "rank/B.png",
    "rank/C.png",
    "rank/F.png",
    "rank/FC.png",
    "rank/S.png",
    "rank/V.png",
    "rank/phi.png",
    "respack/click.png",
    "respack/click_mh.png",
    "respack/drag.png",
    "respack/drag_mh.png",
    "respack/flick.png",
    "respack/flick_mh.png",
    "respack/hit_fx.png",
    "respack/hold.png",
    "respack/hold_mh.png",
    "respack/info.yml",
];

/// 解包内置资源并返回可直接传给 `set_pc_assets_folder` 的目录。
///
/// `PPR_RENDER_ASSETS` 可指向源码目录中的 `assets/`，方便开发时不重建。
#[cfg(not(target_os = "android"))]
pub fn prepare() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("PPR_RENDER_ASSETS") {
        let path = PathBuf::from(path);
        anyhow::ensure!(
            path.is_dir(),
            "PPR_RENDER_ASSETS 不是目录：{}",
            path.display()
        );
        return Ok(path);
    }
    let dir = cache_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("无法创建资源目录 {}", dir.display()))?;
    for (name, bytes) in BUNDLED {
        let path = dir.join(name);
        if std::fs::read(&path).is_ok_and(|existing| existing == *bytes) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)
            .with_context(|| format!("无法写入内置资源 {}", path.display()))?;
    }
    Ok(dir)
}

/// Android 的内置资源由 APK 的 assets 提供，无需准备本地目录。
#[cfg(target_os = "android")]
pub fn prepare() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::new())
}

/// 让 macroquad 以与 Android / 桌面一致的方式读取资源目录。
#[cfg(not(target_os = "android"))]
pub fn install(dir: &Path) {
    macroquad::file::set_pc_assets_folder(&dir.display().to_string());
}

/// Android 的 macroquad 直接读取 APK assets，`pc_assets_folder` 不起作用。
#[cfg(target_os = "android")]
pub fn install(_: &Path) {}

#[cfg(not(target_os = "android"))]
fn cache_dir() -> PathBuf {
    let tag = env!("CARGO_PKG_VERSION");
    std::env::temp_dir()
        .join("ppr-render")
        .join(format!("assets-{tag}"))
}
