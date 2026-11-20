//! 默认启动图形界面；可用命令行参数预选回放 / 谱面，或用 `--video` 直接无窗口导出。
//!
//! 启动参数：
//! - `ppr-render` 打开图形界面
//! - `ppr-render <replay.ppr> [chart-dir-or-zip]` 打开图形界面并预选文件
//! - `ppr-render --play <replay.ppr> [chart-dir-or-zip]` 直接播放
//! - `ppr-render --replay <replay.ppr> --chart <chart> --video <out.mp4>` 无窗口导出
//!
//! 未提供谱面时会先按回放中的谱面 ID 查找常见目录，找不到再在界面中选择。
//! 谱面可以是包含 `info.yml` 的目录，也可以是 Phira 的谱面 ZIP。
//! 指定 `--video` 或环境变量 `PPR_RENDER_VIDEO` 时不打开窗口，直接离线渲染；
//! 画面沿用 Phira Pro 的演出流程，谱面阶段使用回放录制的判定和触点。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod gui;
mod online;
mod replay;
mod scene;
mod video;

use anyhow::{Context, Result};
use macroquad::prelude::*;
use prpr::{
    build_conf,
    core::PGR_FONT,
    ext::poll_future,
    scene::{show_error, Main},
    time::TimeManager,
    ui::{cleanup_audio, FontArc, TextPainter, Ui},
};
use replay::{ChartRef, Replay};
use scene::ReplayScene;
use std::{
    future::Future,
    path::{Path, PathBuf},
};

struct Args {
    replay: Option<PathBuf>,
    chart: Option<PathBuf>,
    video_enabled: bool,
    video: video::Options,
    direct_play: bool,
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("{err:#}");
            std::process::exit(2);
        }
    };
    if args.direct_play && args.replay.is_none() {
        eprintln!("直接播放模式需要指定回放文件");
        print_usage();
        std::process::exit(2);
    }
    if args.video_enabled && args.replay.is_none() {
        eprintln!("视频导出需要指定回放文件");
        print_usage();
        std::process::exit(2);
    }
    if args.video_enabled {
        if let Err(err) = args.video.validate() {
            eprintln!("{err:#}");
            std::process::exit(2);
        }
    }

    // 字体、资源包与 HUD 贴图都内置在可执行文件里；首次运行写到缓存目录。
    let assets = match assets::prepare() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("无法准备内置资源：{err:#}");
            std::process::exit(1);
        }
    };

    let mut conf = build_conf();
    if args.video_enabled {
        // 离屏渲染不需要可见窗口；无头模式也不会抢焦点。
        conf.window_title = "PPR Render · 视频导出".to_owned();
        conf.window_width = 960;
        conf.window_height = 540;
        conf.window_resizable = false;
        conf.headless = true;
        conf.platform.swap_interval = Some(0);
        macroquad::Window::from_config(conf, async move {
            // `set_pc_assets_folder` 依赖已创建的 miniquad 上下文，必须等窗口启动后调用。
            assets::install(&assets);
            if let Err(err) = run_video(args).await {
                eprintln!("视频导出失败：{err:#}");
                cleanup_audio();
                std::process::exit(1);
            }
        });
        cleanup_audio();
        return;
    }

    conf.window_title = "PPR Render · 本地回放".to_owned();
    conf.window_width = 1280;
    conf.window_height = 720;
    conf.window_resizable = true;

    macroquad::Window::from_config(conf, async move {
        // `set_pc_assets_folder` 依赖已创建的 miniquad 上下文，必须等窗口启动后调用。
        assets::install(&assets);
        let result = if args.direct_play {
            run(args).await
        } else {
            gui::run(args.replay, args.chart).await
        };
        if let Err(err) = result {
            eprintln!("{err:#}");
            show_fatal_error(&err).await;
        }
    });
    // sasa 的音频线程必须在 GL 上下文销毁前后按固定顺序收尾。
    cleanup_audio();
}

/// 窗口内的兜底错误提示，避免双击启动时只看到闪退。
async fn show_fatal_error(err: &anyhow::Error) {
    let err = format!("{err:#}");
    loop {
        set_default_camera();
        clear_background(Color::new(0.06, 0.08, 0.10, 1.));
        draw_text(
            "PPR Render 无法继续",
            40.,
            76.,
            36.,
            Color::new(1., 0.72, 0.4, 1.),
        );
        let mut y = 124.;
        for line in err.lines().take(12) {
            draw_text(line, 40., y, 22., WHITE);
            y += 30.;
        }
        draw_text("按 Esc 退出", 40., y + 14., 22., GRAY);
        if is_key_pressed(KeyCode::Escape) || is_key_pressed(KeyCode::Q) {
            break;
        }
        next_frame().await;
    }
}

async fn run(args: Args) -> Result<()> {
    // `Task` 会从 `prpr` 内部调用 `tokio::spawn`，必须先进入运行时。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("无法创建异步运行时")?;
    let _guard = rt.enter();

    let replay_path = match args.replay {
        Some(path) => existing_path(path).context("回放文件不存在")?,
        None => pick_replay()?,
    };
    let replay = replay::load(&replay_path)?;
    replay.ensure_playable()?;

    let chart_path = match args.chart {
        Some(path) => normalize_chart_path(path)?,
        None => resolve_chart(&replay, &replay_path)?,
    };

    let mut painter = load_painter().await?;
    play_paths(replay_path, chart_path, &mut painter).await
}

/// 读取内置字体并初始化全局文本绘制器。
pub(crate) async fn load_painter() -> Result<TextPainter> {
    // 任何一项缺失都直接给出明确错误，而不是在游戏场景里 panic。
    let pgr_font = FontArc::try_from_vec(
        load_file("phigros.ttf")
            .await
            .context("缺少内置 phigros.ttf")?,
    )?;
    let ui_font = FontArc::try_from_vec(
        load_file("harmonyos.ttf")
            .await
            .context("缺少内置 harmonyos.ttf")?,
    )?;
    PGR_FONT.with(|it| *it.borrow_mut() = Some(TextPainter::new(pgr_font, Some(ui_font.clone()))));
    Ok(TextPainter::new(ui_font, None))
}

/// 加载回放与谱面，期间显示加载界面。
pub(crate) async fn load_scene(
    replay_path: PathBuf,
    chart_path: PathBuf,
    painter: &mut TextPainter,
) -> Result<ReplayScene> {
    let replay = replay::load(&replay_path)?;
    replay.ensure_playable()?;
    let loading = ReplayScene::new(replay_path.clone(), chart_path.clone());
    wait_with_loading(loading, painter, &replay, &replay_path, &chart_path).await
}

/// 进入回放播放界面，直到用户退出播放。
pub(crate) async fn play_paths(
    replay_path: PathBuf,
    chart_path: PathBuf,
    painter: &mut TextPainter,
) -> Result<()> {
    let scene = load_scene(replay_path, chart_path, painter).await?;
    let mut main = Main::new(Box::new(scene), TimeManager::default(), None).await?;

    loop {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            main.update()?;
            main.render(painter)?;
            if main.should_exit() {
                return Ok::<bool, anyhow::Error>(true);
            }
            Ok(false)
        }));
        match result {
            Ok(Ok(true)) => break,
            Ok(Ok(false)) => {}
            Ok(Err(err)) => {
                eprintln!("运行错误：{err:#}");
                show_error(err);
            }
            Err(_) => {
                eprintln!("渲染线程发生 panic");
                break;
            }
        }
        prpr::ext::flush_pending_texture_deletions();
        next_frame().await;
    }
    Ok(())
}

/// 视频导出走独立入口：不创建可见窗口，也不等待用户操作。
async fn run_video(args: Args) -> Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("无法创建异步运行时")?;
    let _guard = rt.enter();

    let replay_path =
        existing_path(args.replay.context("视频导出需要回放文件")?).context("回放文件不存在")?;
    let replay = replay::load(&replay_path)?;
    replay.ensure_playable()?;
    let chart_path = match args.chart {
        Some(path) => normalize_chart_path(path)?,
        None => {
            let path = detect_chart(&replay, &replay_path)
                .context("没有找到回放对应的谱面；视频导出模式下不会弹出选择框，请用 --chart 指定谱面目录或 ZIP")?;
            normalize_chart_path(path)?
        }
    };

    println!("正在加载回放与谱面…");
    let mut painter = load_painter().await?;

    // `ReplayScene` 内部会等待 tokio 的文件读取任务；macroquad 的 waker 不能在
    // tokio 工作线程上被唤醒，所以和播放器一样用空 waker 每帧主动轮询。
    let mut loading = std::pin::pin!(ReplayScene::new(replay_path, chart_path));
    let mut scene = loop {
        if let Some(result) = poll_future(loading.as_mut()) {
            break result?;
        }
        next_frame().await;
    };
    if let Some(warning) = scene.warning() {
        eprintln!("警告：{warning}");
    }
    let mut progress = video::CliProgress;
    let _ = video::render(&mut scene, &args.video, &mut painter, &mut progress)
        .await?
        .context("导出已取消")?;
    Ok(())
}

fn parse_args() -> Result<Args> {
    let mut video = video::Options::default();
    let video_env = std::env::var_os("PPR_RENDER_VIDEO").map(PathBuf::from);
    let video_enabled = video_env.is_some();
    if let Some(path) = video_env {
        video.output = path;
    }
    let mut args = Args {
        replay: std::env::var_os("PPR_RENDER_REPLAY").map(PathBuf::from),
        chart: std::env::var_os("PPR_RENDER_CHART").map(PathBuf::from),
        video_enabled,
        video,
        direct_play: false,
    };
    let mut positional = Vec::new();
    let mut iter = std::env::args_os().skip(1);
    while let Some(arg) = iter.next() {
        let text = arg.to_string_lossy();
        match text.as_ref() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("ppr-render {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "-r" | "--replay" => {
                let path = iter.next().context("--replay 后缺少路径")?;
                args.replay = Some(PathBuf::from(path));
            }
            "-c" | "--chart" => {
                let path = iter.next().context("--chart 后缺少路径")?;
                args.chart = Some(PathBuf::from(path));
            }
            "-o" | "--video" => {
                let path = iter.next().context("--video 后缺少输出路径")?;
                args.video.output = PathBuf::from(path);
                args.video_enabled = true;
            }
            "--fps" => {
                let value = iter.next().context("--fps 后缺少数字")?;
                args.video.fps = parse_u32(&value, "--fps")?;
            }
            "--width" => {
                let value = iter.next().context("--width 后缺少数字")?;
                args.video.width = parse_u32(&value, "--width")?;
            }
            "--height" => {
                let value = iter.next().context("--height 后缺少数字")?;
                args.video.height = parse_u32(&value, "--height")?;
            }
            "--resolution" => {
                let value = iter
                    .next()
                    .context("--resolution 后缺少尺寸（例如 1920x1080）")?;
                let (width, height) = parse_resolution(&value)?;
                args.video.width = width;
                args.video.height = height;
            }
            "--start" => {
                let value = iter.next().context("--start 后缺少秒数")?;
                args.video.start = parse_f64(&value, "--start")?;
            }
            "--end" => {
                let value = iter.next().context("--end 后缺少秒数")?;
                args.video.end = Some(parse_f64(&value, "--end")?);
            }
            "--crf" => {
                let value = iter.next().context("--crf 后缺少数字")?;
                args.video.crf = parse_u32(&value, "--crf")?;
            }
            "--no-hud" => args.video.show_hud = false,
            "--no-touches" => args.video.show_touches = false,
            "--no-audio" => args.video.audio = false,
            "--no-opening" => args.video.presentation.opening = false,
            "--no-ending" => args.video.presentation.ending = false,
            "--no-ap-fc" => args.video.visuals.ap_fc_indicator = false,
            "--no-double-hint" => args.video.visuals.double_hint = false,
            "--no-particles" => args.video.visuals.particle = false,
            "--show-acc" => args.video.visuals.show_acc = true,
            "--judgement-details" => args.video.visuals.show_judgement_details = true,
            "--perfect-combo" => args.video.visuals.show_perfect_combo = true,
            "--early-late" => args.video.visuals.show_early_late = true,
            "--play" => args.direct_play = true,
            other if other.starts_with('-') => {
                anyhow::bail!("未知参数：{other}\n\n{}", usage());
            }
            _ => positional.push(PathBuf::from(arg)),
        }
    }
    match positional.as_slice() {
        [] => {}
        [replay] => {
            if args.replay.is_some() {
                anyhow::bail!("重复指定回放文件");
            }
            args.replay = Some(replay.clone());
        }
        [replay, chart] => {
            if args.replay.is_some() || args.chart.is_some() {
                anyhow::bail!("位置参数与 --replay / --chart 不能重复");
            }
            args.replay = Some(replay.clone());
            args.chart = Some(chart.clone());
        }
        _ => anyhow::bail!("参数过多\n\n{}", usage()),
    }
    Ok(args)
}

fn parse_u32(value: &std::ffi::OsStr, name: &str) -> Result<u32> {
    value
        .to_str()
        .context("参数不是有效 UTF-8")?
        .parse()
        .with_context(|| format!("{name} 需要非负整数"))
}

fn parse_f64(value: &std::ffi::OsStr, name: &str) -> Result<f64> {
    value
        .to_str()
        .context("参数不是有效 UTF-8")?
        .parse()
        .with_context(|| format!("{name} 需要数字"))
}

fn parse_resolution(value: &std::ffi::OsStr) -> Result<(u32, u32)> {
    let text = value.to_str().context("分辨率不是有效 UTF-8")?;
    let index = text
        .find(|c: char| matches!(c, 'x' | 'X' | '*'))
        .with_context(|| format!("分辨率格式应为 1920x1080，收到 {text}"))?;
    let (width, height) = (&text[..index], &text[index + 1..]);
    Ok((
        width
            .parse()
            .with_context(|| format!("无效宽度：{width}"))?,
        height
            .parse()
            .with_context(|| format!("无效高度：{height}"))?,
    ))
}

fn usage() -> &'static str {
    "用法：
  ppr-render                      打开图形界面
  ppr-render <replay.ppr> [谱面目录或谱面 ZIP]
                                  打开图形界面并预选回放 / 谱面
  ppr-render --play <replay.ppr> [谱面]
                                  跳过主界面，直接播放
  ppr-render <replay.ppr> --chart <谱面路径> --video <输出.mp4> [选项]
                                  命令行无窗口导出视频

图形界面会记住本次选择，可随时切换回放、谱面、播放或导出。
回放和谱面都可以在界面中通过文件对话框选择。

视频导出选项（需要 FFmpeg，使用 --video 时生效）：
  -o, --video <文件>      导出 MP4；也可用 PPR_RENDER_VIDEO 指定
  --resolution <宽x高>    默认 1920x1080（也可分用 --width / --height）
  --fps <帧率>            默认 60，范围 1-240
  --start <秒>            起始谱面时间，默认 0
  --end <秒>              结束谱面时间，默认整张谱面
  --crf <0-51>            x264 质量，默认 18
  --no-hud                不绘制分数 / 连击 HUD
  --no-touches            不绘制触点
  --no-audio              只导出视频，不合成音轨
  --no-opening            不渲染 Phira Pro 开场演出（默认开启）
  --no-ending             不渲染 Phira Pro 结算演出（默认开启）
  --no-ap-fc              不显示 AP/FC 状态指示
  --no-double-hint        不显示双押提示
  --no-particles          关闭打击粒子
  --show-acc              显示实时准确率
  --judgement-details     显示判定详情
  --perfect-combo         显示 Perfect 连击
  --early-late            显示 Early / Late 统计

环境变量：
  PPR_RENDER_REPLAY     默认回放文件
  PPR_RENDER_CHART      默认谱面路径
  PPR_RENDER_CHART_DIR  谱面根目录
  PPR_RENDER_VIDEO      默认视频输出路径（设置后进入无窗口导出模式）
  PPR_RENDER_DATA_DIR   在线谱面与登录状态的保存目录
  PPR_RENDER_ASSETS     覆盖内置资源目录（开发用）
  PHIRA_FFMPEG          指定 ffmpeg 可执行文件"
}

fn print_usage() {
    println!("{}", usage());
}

pub(crate) fn existing_path(path: PathBuf) -> Result<PathBuf> {
    let path = absolute(path)?;
    anyhow::ensure!(path.exists(), "{}", path.display());
    Ok(path)
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

/// 允许用户直接传入 `chart.json`：实际交给 `prpr` 的仍是它所在的目录。
/// 谱面 ZIP 保持原样。
pub(crate) fn normalize_chart_path(path: PathBuf) -> Result<PathBuf> {
    let path = existing_path(path)?;
    if path.is_file()
        && path
            .extension()
            .and_then(|it| it.to_str())
            .is_some_and(|it| matches!(it.to_ascii_lowercase().as_str(), "json" | "pec" | "pbc"))
    {
        let parent = path.parent().context("谱面文件没有父目录")?;
        anyhow::ensure!(
            parent.join("info.yml").is_file()
                || parent.join("info.txt").is_file()
                || parent.join("info.csv").is_file(),
            "{} 所在目录缺少 info.yml",
            path.display()
        );
        return Ok(parent.to_path_buf());
    }
    Ok(path)
}

fn pick_replay() -> Result<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .add_filter("Phira 回放", &["ppr", "phirar"])
        .set_title("选择回放文件");
    if let Some(dir) = default_picker_dir() {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file().context("未选择回放文件")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub(crate) fn default_picker_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = home_dir() {
        candidates.push(home.join("Downloads"));
        candidates.push(home.join("桌面"));
        candidates.push(home.join("Desktop"));
        candidates.push(home.join(".local/share/PhiraPro/data/replays"));
        candidates.push(home.join(".local/share/Phira-Pro/data/replays"));
        candidates.push(home.join("AppData/Local/PhiraPro/data/replays"));
    }
    if let Ok(dir) = std::env::current_dir() {
        candidates.push(dir);
    }
    candidates.into_iter().find(|it| it.is_dir())
}

/// 未显式指定谱面时的查找顺序：
/// 1. 回放文件所在目录（同级放 info.yml / ZIP 即可直接双击播放）；
/// 2. 回放里记录的本地谱面路径；
/// 3. `PPR_RENDER_CHART_DIR` 与常见 Phira / Phira Pro 数据目录下的 `download/<id>`；
/// 4. 以上都没有时弹出选择框。
fn resolve_chart(replay: &Replay, replay_path: &Path) -> Result<PathBuf> {
    if let Some(path) = detect_chart(replay, replay_path) {
        return normalize_chart_path(path);
    }
    let id = replay
        .chart_id()
        .map(|it| format!("谱面 ID：{it}"))
        .unwrap_or_else(|| "回放未记录谱面 ID".to_owned());
    let description = format!(
        "没有找到回放对应的谱面（{id}）。\n\n是：选择包含 info.yml 的谱面文件夹\n否：选择谱面 ZIP 压缩包"
    );
    let choice = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title("选择谱面")
        .set_description(&description)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();
    let picked = match choice {
        rfd::MessageDialogResult::Yes => rfd::FileDialog::new()
            .set_title("选择谱面文件夹")
            .pick_folder(),
        rfd::MessageDialogResult::No => rfd::FileDialog::new()
            .add_filter("谱面压缩包", &["zip", "phira", "pez"])
            .set_title("选择谱面 ZIP")
            .pick_file(),
        _ => None,
    };
    let path = picked.context("未选择谱面")?;
    normalize_chart_path(path)
}

/// 在回放所在目录查找与 `.ppr` 同名的谱面压缩包或文件夹。
///
/// 文件名比较忽略大小写，也接受 `song.pez.zip` 这类“谱面扩展名 + ZIP”的
/// 命名；没有匹配时返回 `None`，调用方保持静默。
pub(crate) fn same_name_chart(dir: &Path, stem: &str) -> Option<PathBuf> {
    fn archive_stem(name: &str) -> &str {
        let lower = name.to_ascii_lowercase();
        for ext in [".zip", ".pez", ".phira"] {
            if lower.len() > ext.len() && lower.ends_with(ext) {
                return &name[..name.len() - ext.len()];
            }
        }
        name
    }

    let mut archives = Vec::new();
    let mut folder = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if !is_chart(&path) {
            continue;
        }
        if path.is_file() {
            if let Some(name) = path.file_stem().and_then(|it| it.to_str()) {
                if archive_stem(name).eq_ignore_ascii_case(stem) {
                    archives.push(path);
                }
            }
        } else if let Some(name) = path.file_name().and_then(|it| it.to_str()) {
            if name.eq_ignore_ascii_case(stem) {
                folder = Some(path);
            }
        }
    }
    archives.sort();
    archives.into_iter().next().or(folder)
}

pub(crate) fn detect_chart(replay: &Replay, replay_path: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    let replay_dir = replay_path.parent();

    if let Some(dir) = replay_dir {
        if is_chart(dir) {
            candidates.push(dir.to_path_buf());
        }
        // 优先使用与回放同名的谱面压缩包；没有就静默继续查找其他来源。
        if let Some(stem) = replay_path.file_stem().and_then(|it| it.to_str()) {
            if let Some(path) = same_name_chart(dir, stem) {
                candidates.push(path);
            }
        }
    }

    if let Some(ChartRef::Local(path)) = &replay.chart {
        let path = PathBuf::from(path);
        let resolved = if path.is_absolute() {
            path
        } else if let Some(dir) = replay_dir {
            dir.join(path)
        } else {
            path
        };
        if resolved.is_file() || is_chart(&resolved) {
            candidates.push(resolved);
        }
    }

    if let Some(id) = replay.chart_id() {
        if let Ok(root) = online::charts_dir() {
            let path = root.join(id.to_string());
            if is_chart(&path) {
                candidates.push(path);
            }
        }
    }

    if let Some(path) = std::env::var_os("PPR_RENDER_CHART_DIR") {
        let root = PathBuf::from(path);
        if is_chart(&root) {
            candidates.push(root.clone());
        }
        if let Some(id) = replay.chart_id() {
            for sub in ["", "charts/download", "data/charts/download"] {
                let path = root.join(sub).join(id.to_string());
                if is_chart(&path) {
                    candidates.push(path);
                }
            }
        }
    }

    if let Some(id) = replay.chart_id() {
        for root in chart_roots(replay_dir) {
            for sub in ["", "charts/download", "data/charts/download"] {
                let path = root.join(sub).join(id.to_string());
                if is_chart(&path) {
                    candidates.push(path);
                }
            }
            if let Ok(entries) = std::fs::read_dir(&root) {
                for entry in entries.flatten() {
                    let path = entry.path().join("info.yml");
                    if !path.is_file() {
                        continue;
                    }
                    let parent = entry.path();
                    if parent
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == id.to_string())
                    {
                        candidates.push(parent);
                    }
                }
            }
        }
    }

    candidates.into_iter().find(|path| is_chart(path))
}

fn chart_roots(replay_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(dir) = replay_dir {
        roots.push(dir.to_path_buf());
        if let Some(parent) = dir.parent() {
            roots.push(parent.to_path_buf());
        }
    }
    if let Ok(dir) = std::env::current_dir() {
        roots.push(dir);
    }
    if let Ok(mut exe) = std::env::current_exe() {
        while exe.pop() {
            roots.push(exe.clone());
            if roots.len() > 12 {
                break;
            }
        }
    }
    if let Some(home) = home_dir() {
        for name in [
            ".local/share/PhiraPro",
            ".local/share/Phira-Pro",
            ".local/share/phira",
            ".local/share/Phira",
            "Library/Application Support/PhiraPro",
            "Library/Application Support/Phira-Pro",
            "AppData/Local/PhiraPro",
            "AppData/Roaming/PhiraPro",
            "PhiraPro",
            "Phira-Pro",
            "文档/PhiraPro",
            "Documents/PhiraPro",
        ] {
            roots.push(home.join(name));
        }
    }
    let mut seen = std::collections::HashSet::new();
    roots.retain(|path| seen.insert(path.clone()));
    roots
}

pub(crate) fn is_chart(path: &Path) -> bool {
    (path.is_file()
        && path
            .extension()
            .and_then(|it| it.to_str())
            .is_some_and(|it| matches!(it.to_ascii_lowercase().as_str(), "zip" | "phira" | "pez")))
        || (path.is_dir()
            && (path.join("info.yml").is_file()
                || path.join("info.txt").is_file()
                || path.join("info.csv").is_file()))
}

/// 边加载边绘制，避免 `GameScene::new` 期间窗口一片黑。
async fn wait_with_loading<F>(
    future: F,
    painter: &mut TextPainter,
    replay: &Replay,
    replay_path: &Path,
    chart_path: &Path,
) -> Result<ReplayScene>
where
    F: Future<Output = Result<ReplayScene>>,
{
    let mut future = std::pin::pin!(future);
    let mut spinner = 0;
    loop {
        // `poll_future` 使用空 waker；这里每帧主动轮询一次，加载任务无需唤醒当前协程。
        if let Some(result) = poll_future(future.as_mut()) {
            return result;
        }
        {
            let mut ui = Ui::new(painter, None);
            set_camera(&ui.camera());
            clear_background(Color::new(0.06, 0.08, 0.10, 1.));
            // `Ui` 的横轴固定为 [-1, 1]，文字大小随窗口宽度缩放。
            // 用窗口宽高比修正字号和纵向间距，窄窗口 / 超宽窗口都不会挤在一起。
            let k = (screen_height() / screen_width() / (9. / 16.)).clamp(0.72, 2.);
            let spin = ["|", "/", "-", "\\"][spinner % 4];
            spinner += 1;
            ui.text(format!("{spin}  正在加载回放…"))
                .pos(0., -0.04 * k)
                .anchor(0.5, 0.5)
                .size(0.42 * k)
                .color(WHITE)
                .draw();
            ui.text(
                replay_path
                    .file_name()
                    .and_then(|it| it.to_str())
                    .unwrap_or("回放"),
            )
            .pos(0., 0.03 * k)
            .anchor(0.5, 0.5)
            .size(0.30 * k)
            .max_width(1.6)
            .color(Color::new(0.8, 0.84, 0.9, 1.))
            .draw();
            ui.text(format!("{} · {}", replay.meta.name, replay.meta.level))
                .pos(0., 0.09 * k)
                .anchor(0.5, 0.5)
                .size(0.32 * k)
                .max_width(1.6)
                .color(Color::new(0.7, 0.76, 0.84, 1.))
                .draw();
            ui.text(chart_path.display().to_string())
                .pos(0., 0.15 * k)
                .anchor(0.5, 0.5)
                .size(0.22 * k)
                .max_width(1.7)
                .color(Color::new(0.55, 0.6, 0.68, 1.))
                .draw();
            ui.text("正在解析谱面与音频，首次加载可能需要几秒")
                .pos(0., 0.22 * k)
                .anchor(0.5, 0.5)
                .size(0.24 * k)
                .color(Color::new(0.55, 0.6, 0.68, 1.))
                .draw();
        }
        next_frame().await;
    }
}

#[cfg(test)]
mod tests {
    use super::same_name_chart;
    use std::fs;

    #[test]
    fn same_name_chart_finds_archives_case_insensitively() {
        let dir = tempfile::tempdir().unwrap();
        let chart = dir.path().join("Song.PEZ");
        fs::write(&chart, b"").unwrap();
        assert_eq!(same_name_chart(dir.path(), "song"), Some(chart));
    }

    #[test]
    fn same_name_chart_accepts_double_extension_and_folder() {
        let dir = tempfile::tempdir().unwrap();
        let chart = dir.path().join("Track.pez.zip");
        fs::write(&chart, b"").unwrap();
        assert_eq!(same_name_chart(dir.path(), "Track"), Some(chart));

        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("Another");
        fs::create_dir(&folder).unwrap();
        fs::write(folder.join("info.yml"), b"").unwrap();
        assert_eq!(same_name_chart(dir.path(), "another"), Some(folder));
    }

    #[test]
    fn same_name_chart_stays_silent_without_match() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("other.zip"), b"").unwrap();
        assert_eq!(same_name_chart(dir.path(), "song"), None);
    }
}
