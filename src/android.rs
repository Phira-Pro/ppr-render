//! Android 运行时初始化。
//!
//! 应用的代码、资源和 FFmpeg 都打包在 APK 里。Android 没有桌面那样的
//! `HOME` / `/tmp`，所以启动时从 Java 上下文取应用目录，供临时文件、
//! 在线谱面缓存和 FFmpeg 子进程使用。
//!
//! Android 10 起，应用私有目录里的文件默认不可执行。`targetSdk 28` 的
//! APK 仍允许执行缓存目录中的文件，因此这里把随 APK 解压的
//! `libffmpeg.so` 复制到缓存目录并补上可执行位，再把路径交给 `prpr`
//! 的 `PHIRA_FFMPEG` 环境变量。

use anyhow::{bail, Context, Result};
use jni::{
    objects::{JObject, JString, JValue},
    JavaVM,
};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

fn java_vm() -> Result<JavaVM> {
    let context = ndk_context::android_context();
    Ok(unsafe { JavaVM::from_raw(context.vm().cast()) }?)
}

fn activity() -> JObject<'static> {
    let context = ndk_context::android_context();
    unsafe { JObject::from_raw(context.context().cast()) }
}

fn call_path(
    env: &mut jni::JNIEnv<'_>,
    object: &JObject<'_>,
    name: &str,
) -> Result<PathBuf> {
    env.with_local_frame(16, |env| {
        let value = env
            .call_method(object, name, "()Ljava/io/File;", &[])
            .with_context(|| format!("无法调用 Android 方法 {name}"))?;
        let value = value.l().context("Android 方法返回空值")?;
        let string = env
            .call_method(&value, "getAbsolutePath", "()Ljava/lang/String;", &[])
            .context("无法读取文件绝对路径")?
            .l()
            .context("文件路径为空")?;
        let text: String = env
            .get_string(&string.into())
            .context("无法读取文件路径字符串")?
            .into();
        Ok(PathBuf::from(text))
    })
}

fn paths() -> Result<(PathBuf, PathBuf, PathBuf)> {
    let vm = java_vm()?;
    let mut env = vm.attach_current_thread()?;
    let activity = activity();
    let files = call_path(&mut env, &activity, "pprFilesDir")?;
    let cache = call_path(&mut env, &activity, "pprCacheDir")?;
    let native = call_path(&mut env, &activity, "pprNativeLibraryDir")?;
    Ok((files, cache, native))
}

/// 应用的外部文件目录；用户可通过文件管理器把回放放进这里。
///
/// 文件浏览器每帧都会读取这个路径，所以只查询一次并缓存，避免反复
/// 通过 JNI 创建局部引用。
pub fn external_files_dir() -> Option<PathBuf> {
    static CACHE: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHE
        .get_or_init(|| {
            (|| -> Result<PathBuf> {
                let vm = java_vm()?;
                let mut env = vm.attach_current_thread()?;
                let activity = activity();
                call_path(&mut env, &activity, "pprExternalFilesDir")
            })()
            .ok()
        })
        .clone()
}

/// 把 nativeLibraryDir 里的 `libffmpeg.so` 准备成可执行副本。
///
/// 重新安装后 APK 里的文件会变化，所以只在大小不同或副本不可执行时复制。
fn prepare_ffmpeg(native: &Path, cache: &Path) -> Result<PathBuf> {
    let source = native.join("libffmpeg.so");
    anyhow::ensure!(
        source.is_file(),
        "APK 中没有 libffmpeg.so，无法导出视频"
    );
    let dir = cache.join("ffmpeg");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("无法创建 FFmpeg 目录 {}", dir.display()))?;
    let target = dir.join("ffmpeg");
    let ready = std::fs::metadata(&source).ok().zip(std::fs::metadata(&target).ok()).is_some_and(|(a, b)| {
        a.len() == b.len() && b.permissions().mode() & 0o111 != 0
    });
    if !ready {
        // 先写临时文件再改名，避免上次中断留下半个可执行文件。
        let temp = dir.join("ffmpeg.tmp");
        std::fs::copy(&source, &temp)
            .with_context(|| format!("无法复制 FFmpeg 到 {}", temp.display()))?;
        let mut permissions = std::fs::metadata(&temp)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&temp, permissions)?;
        std::fs::rename(&temp, &target)?;
    }
    Ok(target)
}

/// 在创建窗口之前调用。失败时仍允许启动，只是导出或在线缓存可能不可用。
pub fn init() {
    match paths() {
        Ok((files, cache, native)) => {
            // Rust 的 tempfile / std::env::temp_dir 在 Android 上默认不可写。
            std::env::set_var("TMPDIR", &cache);
            std::env::set_var("TMP", &cache);
            std::env::set_var("TEMP", &cache);
            if std::env::var_os("PPR_RENDER_DATA_DIR").is_none() {
                std::env::set_var("PPR_RENDER_DATA_DIR", files.join("data"));
            }
            if std::env::var_os("PHIRA_FFMPEG").is_none() {
                match prepare_ffmpeg(&native, &cache) {
                    Ok(path) => {
                        eprintln!("Android FFmpeg：{}", path.display());
                        std::env::set_var("PHIRA_FFMPEG", path);
                    }
                    Err(err) => eprintln!("Android FFmpeg 不可用：{err:#}"),
                }
            }
            eprintln!(
                "Android 路径：数据 {}，临时 {}",
                files.display(),
                cache.display()
            );
        }
        Err(err) => eprintln!("无法初始化 Android 路径：{err:#}"),
    }
}

/// 原生输入对话框交回的文本。
static INPUT: Mutex<Option<String>> = Mutex::new(None);

/// 弹出 Android 原生输入框。
pub fn request_text(initial: &str, masked: bool) {
    let result = (|| -> Result<()> {
        let vm = java_vm()?;
        let mut env = vm.attach_current_thread()?;
        let activity = activity();
        env.with_local_frame(16, |env| {
            let text = env.new_string(initial)?;
            env.call_method(
                &activity,
                "editText",
                "(Ljava/lang/String;Z)V",
                &[JValue::Object(&text), JValue::Bool(masked as u8)],
            )?;
            Ok(())
        })
    })();
    if let Err(err) = result {
        eprintln!("无法打开 Android 输入框：{err:#}");
    }
}

/// 取出原生输入框交回的内容（如果有）。
pub fn take_input() -> Option<String> {
    INPUT.lock().ok().and_then(|mut input| input.take())
}

/// 由 Java 输入框调用。
#[no_mangle]
pub extern "system" fn Java_quad_1native_QuadNative_submitInput(
    env: *mut jni::sys::JNIEnv,
    _class: jni::sys::jobject,
    text: jni::sys::jstring,
) {
    let Ok(mut env) = (unsafe { jni::JNIEnv::from_raw(env) }) else {
        return;
    };
    let text = unsafe { JString::from_raw(text) };
    let value = env.get_string(&text);
    if let Ok(value) = value {
        if let Ok(mut input) = INPUT.lock() {
            *input = Some(value.into());
        }
    }
}

/// 结束 Activity。主界面点「退出」或出现无法恢复的错误时调用。
pub fn quit() {
    let result = (|| -> Result<()> {
        let vm = java_vm()?;
        let mut env = vm.attach_current_thread()?;
        let activity = activity();
        env.call_method(&activity, "pprQuit", "()V", &[])?;
        Ok(())
    })();
    if let Err(err) = result {
        eprintln!("无法结束 Android Activity：{err:#}");
    }
}

/// 把导出完成的视频发布到系统下载目录，返回给用户看的位置。
pub fn publish_video(path: &Path, display_name: &str) -> Result<String> {
    let vm = java_vm()?;
    let mut env = vm.attach_current_thread()?;
    let activity = activity();
    env.with_local_frame(16, |env| {
        let path_text = env
            .new_string(path.display().to_string())
            .context("无法创建视频路径字符串")?;
        let name_text = env
            .new_string(display_name)
            .context("无法创建视频名称字符串")?;
        let value = env
            .call_method(
                &activity,
                "publishVideo",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&path_text), JValue::Object(&name_text)],
            )
            .context("无法调用 Android 视频发布接口")?
            .l()
            .context("视频发布接口返回空值")?;
        if value.is_null() {
            bail!("系统下载目录不可写");
        }
        let value = JString::from(value);
        let result = env
            .get_string(&value)
            .context("无法读取视频保存位置")?;
        Ok(result.into())
    })
}
