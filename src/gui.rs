//! 图形界面：回放 / 谱面选择、播放入口和视频导出设置。
//!
//! 整个流程不依赖命令行参数：启动后直接进入主界面，选择文件、播放或导出。
//! 播放与导出复用 `ReplayScene` / `GameScene`，视觉与 Phira Pro 保持一致。

use crate::{default_picker_dir, detect_chart, normalize_chart_path, online, replay, video};
use anyhow::{Context, Result};
use macroquad::prelude::*;
use prpr::{
    chart_video::{PresentationOptions, VisualOptions},
    ui::TextPainter,
};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{Receiver, TryRecvError},
};

/// 主界面操作。
#[derive(Clone, Copy, PartialEq, Eq)]
enum HomeAction {
    None,
    Play,
    Export,
    Quit,
}

/// 导出设置界面操作。
enum FormAction {
    None,
    Back,
    Start,
}

enum FlowResult {
    Back,
    Done(PathBuf),
    Cancelled,
}

pub async fn run(mut replay: Option<PathBuf>, mut chart: Option<PathBuf>) -> Result<()> {
    // `ReplayScene` 内部会通过 `tokio::spawn` 读取文件，整个图形界面都进入同一运行时。
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("无法创建异步运行时")?;
    let _guard = rt.enter();

    let fonts = Fonts::load().await?;
    let background = load_texture("background.jpg")
        .await
        .context("缺少内置 background.jpg")?;
    background.set_filter(FilterMode::Linear);
    let icon = load_texture("icon.png").await.ok();
    icon.inspect(|it| it.set_filter(FilterMode::Linear));
    let mut painter = crate::load_painter().await?;

    let mut home = Home::default();
    if let Some(path) = replay.take() {
        home.set_replay(path);
    }
    if let Some(path) = chart.take() {
        home.set_chart(path);
    }

    // 播放和导出里也会使用 Esc；返回主界面后先跳过几帧，
    // 避免同一次按键又被主界面当作「退出」处理。
    let mut cooldown = 0u32;
    loop {
        if cooldown > 0 {
            cooldown -= 1;
            next_frame().await;
            continue;
        }
        match home.frame(&fonts, &background, icon.as_ref()) {
            HomeAction::None => {}
            HomeAction::Quit => return Ok(()),
            HomeAction::Play => {
                if let Some((replay, chart)) = home.ready() {
                    match crate::play_paths(replay, chart, &mut painter).await {
                        Ok(()) => home.status = Some(("已退出播放，返回主界面".into(), false)),
                        Err(err) => home.status = Some((format!("播放失败：{err:#}"), true)),
                    }
                    home.escape_guard = true;
                    cooldown = 2;
                }
            }
            HomeAction::Export => {
                if let Some((replay, chart)) = home.ready() {
                    let name = home
                        .meta
                        .as_ref()
                        .map(|it| it.meta.name.clone())
                        .unwrap_or_default();
                    match export_flow(replay, chart, name, &fonts, &mut painter).await {
                        Ok(FlowResult::Done(path)) => {
                            home.status = Some((format!("视频已保存：{}", path.display()), false));
                        }
                        Ok(FlowResult::Cancelled) => {
                            home.status = Some(("已取消导出".into(), false));
                        }
                        Ok(FlowResult::Back) => {}
                        Err(err) => home.status = Some((format!("导出失败：{err:#}"), true)),
                    }
                    home.escape_guard = true;
                    cooldown = 2;
                }
            }
        }
        next_frame().await;
    }
}

struct Fonts {
    ui: Font,
    title: Font,
}

impl Fonts {
    async fn load() -> Result<Self> {
        Ok(Self {
            ui: load_ttf_font("harmonyos.ttf")
                .await
                .context("缺少内置 harmonyos.ttf")?,
            title: load_ttf_font("phigros.ttf")
                .await
                .context("缺少内置 phigros.ttf")?,
        })
    }
}

#[derive(Default)]
struct Home {
    replay_path: Option<PathBuf>,
    chart_path: Option<PathBuf>,
    meta: Option<replay::Replay>,
    status: Option<(String, bool)>,
    /// 从播放 / 导出返回后，等 Esc 松开再响应主界面退出。
    escape_guard: bool,
    online: OnlineUi,
    /// 未选谱面时点击播放 / 导出触发的自动下载。
    auto_download: Option<AutoDownload>,
}

struct AutoDownload {
    rx: Receiver<online::Event>,
    /// 下载完成后要继续执行的操作（播放或导出）。
    pending: HomeAction,
}

impl Home {
    fn set_replay(&mut self, path: PathBuf) {
        self.auto_download = None;
        match replay::load(&path) {
            Ok(replay) => {
                // 换回放时重新自动匹配谱面，避免沿用上一张谱面。
                let same_name = path.parent().and_then(|dir| {
                    path.file_stem()
                        .and_then(|stem| stem.to_str())
                        .and_then(|stem| crate::same_name_chart(dir, stem))
                });
                self.chart_path = detect_chart(&replay, &path);
                // 没有同名谱面压缩包时保持静默；播放 / 导出时再按谱面 ID 处理。
                let hint = if same_name.is_some() {
                    "已选择回放，并自动匹配同名谱面"
                } else {
                    "已选择回放"
                };
                self.replay_path = Some(path);
                self.meta = Some(replay);
                self.status = Some((hint.to_owned(), false));
            }
            Err(err) => {
                self.status = Some((format!("无法读取回放：{err:#}"), true));
            }
        }
    }

    fn set_chart(&mut self, path: PathBuf) {
        self.auto_download = None;
        match normalize_chart_path(path) {
            Ok(path) => {
                self.chart_path = Some(path);
                self.status = Some(("已选择谱面".into(), false));
            }
            Err(err) => {
                self.status = Some((format!("无效的谱面：{err:#}"), true));
            }
        }
    }

    fn auto_chart(&mut self) {
        self.auto_download = None;
        let Some(meta) = &self.meta else {
            self.status = Some(("请先选择回放".into(), true));
            return;
        };
        let Some(replay_path) = &self.replay_path else {
            return;
        };
        match detect_chart(meta, replay_path) {
            Some(path) => {
                self.chart_path = Some(path);
                self.status = Some(("已自动找到谱面".into(), false));
            }
            None => {
                self.status = Some(("没有找到匹配的谱面，请手动选择文件夹或 ZIP".into(), true));
            }
        }
    }

    fn ready(&mut self) -> Option<(PathBuf, PathBuf)> {
        match (&self.replay_path, &self.chart_path) {
            (Some(replay), Some(chart)) => Some((replay.clone(), chart.clone())),
            (None, _) => {
                self.status = Some(("请先选择回放文件".into(), true));
                None
            }
            (_, None) => {
                self.status = Some(("请先选择谱面文件夹或 ZIP".into(), true));
                None
            }
        }
    }

    /// 点击播放 / 导出：有谱面就直接开始；缺少谱面时按回放中的 ID 自动下载。
    fn request_action(&mut self, action: HomeAction) -> HomeAction {
        if self.chart_path.is_some() {
            return action;
        }
        let Some(replay_path) = self.replay_path.clone() else {
            self.status = Some(("请先选择回放文件".into(), true));
            return HomeAction::None;
        };
        let Some(meta) = &self.meta else {
            self.status = Some(("请先选择回放文件".into(), true));
            return HomeAction::None;
        };
        // 选择回放后文件可能才被放到本地，点击时再匹配一次。
        let detected = detect_chart(meta, &replay_path);
        let chart_id = meta.chart_id();
        if let Some(path) = detected {
            self.chart_path = Some(path);
            return action;
        }
        let Some(id) = chart_id else {
            self.status = Some((
                "该回放未指定在线谱面，请手动选择谱面文件夹或 ZIP".into(),
                true,
            ));
            return HomeAction::None;
        };
        if self.auto_download.is_some() {
            return HomeAction::None;
        }
        self.status = Some((format!("本地缺少谱面 {id}，正在在线下载…"), false));
        self.auto_download = Some(AutoDownload {
            rx: online::spawn_download(id, self.online.account.clone()),
            pending: action,
        });
        HomeAction::None
    }

    /// 轮询自动下载进度；下载完成后返回原先待执行的播放 / 导出操作。
    fn poll_auto_download(&mut self) -> HomeAction {
        let Some(download) = self.auto_download.take() else {
            return HomeAction::None;
        };
        let mut finished = false;
        let mut action = HomeAction::None;
        loop {
            match download.rx.try_recv() {
                Ok(online::Event::Status(message)) => {
                    self.status = Some((message, false));
                }
                Ok(online::Event::Login(_)) => {}
                Ok(online::Event::Download {
                    result,
                    account,
                    session_expired,
                }) => {
                    finished = true;
                    if session_expired {
                        self.online.account = None;
                        online::clear_account();
                    } else if let Some(account) = account {
                        if let Err(err) = online::save_account(&account) {
                            eprintln!("无法保存续期后的登录状态：{err:#}");
                        }
                        self.online.account = Some(account);
                    }
                    match result {
                        Ok(path) => {
                            self.chart_path = Some(path);
                            self.status = Some(("已自动下载并选中在线谱面".into(), false));
                            action = download.pending;
                        }
                        Err(err) => {
                            self.status = Some((format!("在线下载失败：{err}"), true));
                        }
                    }
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    self.status = Some(("在线下载失败：后台任务已退出".into(), true));
                    break;
                }
            }
        }
        if !finished {
            self.auto_download = Some(download);
        }
        action
    }

    fn frame(
        &mut self,
        fonts: &Fonts,
        background: &Texture2D,
        icon: Option<&Texture2D>,
    ) -> HomeAction {
        set_default_camera();
        if self.online.open {
            if let Some(path) = self.online.frame(fonts) {
                let label = display_path(&path);
                self.set_chart(path);
                self.status = Some((format!("已下载并选中在线谱面：{label}"), false));
            }
            return HomeAction::None;
        }
        let auto_action = self.poll_auto_download();
        if auto_action != HomeAction::None {
            return auto_action;
        }
        let (w, h) = (screen_width(), screen_height());
        let s = (w / 1280.).min(h / 720.).clamp(0.5, 2.);
        let busy = self.auto_download.is_some();
        draw_background(background, w, h);
        let mut action = HomeAction::None;

        let margin = 48. * s;
        let replay_chart_id = self.meta.as_ref().and_then(|it| it.chart_id());
        if let Some(icon) = icon {
            draw_texture_ex(
                *icon,
                margin,
                margin,
                WHITE,
                DrawTextureParams {
                    dest_size: Some(vec2(56. * s, 56. * s)),
                    ..Default::default()
                },
            );
        }
        text(
            &fonts.title,
            "PPR",
            margin + if icon.is_some() { 72. * s } else { 0. },
            margin + 34. * s,
            42. * s,
            WHITE,
        );
        let title_width =
            measure_text("PPR", Some(fonts.title), (42. * s).round() as u16, 1.).width;
        text(
            &fonts.title,
            "RENDER",
            margin + if icon.is_some() { 72. * s } else { 0. } + title_width + 14. * s,
            margin + 34. * s,
            42. * s,
            Color::new(0.95, 0.97, 1., 1.),
        );
        text(
            &fonts.ui,
            "本地回放 · 视频导出 · 可选在线谱面",
            margin + if icon.is_some() { 74. * s } else { 0. },
            margin + 58. * s,
            17. * s,
            Color::new(0.75, 0.8, 0.88, 0.9),
        );
        // 右上角账号状态；点击打开在线 / 登录面板。
        let account_w = (224. * s).min(w * 0.34);
        let account_h = 60. * s;
        let account_rect = Rect::new(
            w - margin - account_w,
            margin + 4. * s,
            account_w,
            account_h,
        );
        let pointer = {
            let (x, y) = mouse_position();
            vec2(x, y)
        };
        let account_hover = !busy && account_rect.contains(pointer);
        round_rect(
            account_rect,
            14. * s,
            if account_hover {
                Color::new(0.07, 0.1, 0.15, 0.95)
            } else {
                Color::new(0.025, 0.045, 0.075, 0.82)
            },
        );
        draw_rectangle(
            account_rect.x + 1.5,
            account_rect.y + 12. * s,
            3. * s,
            account_rect.h - 24. * s,
            Color::new(1., 0.72, 0.32, if busy { 0.25 } else { 0.75 }),
        );
        let (account_state, account_detail, account_detail_color) = match &self.online.account {
            Some(account) => (
                "已登录",
                account.name.clone(),
                Color::new(0.93, 0.96, 1., 1.),
            ),
            None => (
                "未登录",
                if busy {
                    "下载中…".to_owned()
                } else {
                    "点击登录 Phira 账号".to_owned()
                },
                Color::new(0.66, 0.72, 0.8, 1.),
            ),
        };
        text(
            &fonts.ui,
            &format!("PHIRA 账号 · {account_state}"),
            account_rect.x + 16. * s,
            account_rect.y + 23. * s,
            12. * s,
            Color::new(0.95, 0.72, 0.38, 1.),
        );
        text(
            &fonts.ui,
            &fit_text(
                &fonts.ui,
                &account_detail,
                17. * s,
                account_rect.w - 30. * s,
            ),
            account_rect.x + 16. * s,
            account_rect.y + 46. * s,
            17. * s,
            account_detail_color,
        );
        if account_hover && is_mouse_button_pressed(MouseButton::Left) {
            self.online.open(replay_chart_id);
        }

        let card_w = (920. * s).min(w - margin * 2.);
        let card_h = (430. * s).min(h - margin * 2. - 60. * s);
        let card = Rect::new(
            (w - card_w) / 2.,
            (h - card_h) / 2. + 18. * s,
            card_w,
            card_h,
        );
        round_rect(card, 24. * s, Color::new(0.02, 0.035, 0.06, 0.88));
        round_rect(
            Rect::new(card.x + 1.5, card.y + 1.5, card.w - 3., card.h - 3.),
            22.5 * s,
            Color::new(0.04, 0.06, 0.09, 0.86),
        );

        let pad = 34. * s;
        let label_w = 70. * s;
        let row_h = 52. * s;
        let right = card.right() - pad;

        // 回放
        let row1 = card.y + pad;
        text(
            &fonts.ui,
            "回放",
            card.x + pad,
            row1 + row_h / 2. + 6. * s,
            19. * s,
            accent(),
        );
        let pick_replay = Rect::new(right - 150. * s, row1, 150. * s, row_h);
        let replay_label = self
            .replay_path
            .as_deref()
            .map(display_path)
            .unwrap_or_else(|| "尚未选择 .ppr 回放".into());
        let color = if self.replay_path.is_some() {
            Color::new(0.9, 0.92, 0.96, 1.)
        } else {
            Color::new(0.62, 0.68, 0.76, 1.)
        };
        text(
            &fonts.ui,
            &fit_text(
                &fonts.ui,
                &replay_label,
                20. * s,
                pick_replay.x - (card.x + pad + label_w) - 14. * s,
            ),
            card.x + pad + label_w,
            row1 + row_h / 2. + 7. * s,
            20. * s,
            color,
        );
        if button(
            pick_replay,
            "选择 .ppr",
            &fonts.ui,
            18. * s,
            ButtonKind::Secondary,
            !busy,
        ) {
            if let Some(path) =
                pick_file("选择 Phira 回放", &["ppr", "phirar"], default_picker_dir())
            {
                self.set_replay(path);
            }
        }

        // 谱面
        let row2 = row1 + row_h + 18. * s;
        text(
            &fonts.ui,
            "谱面",
            card.x + pad,
            row2 + row_h / 2. + 6. * s,
            19. * s,
            accent(),
        );
        let zip = Rect::new(right - 82. * s, row2, 82. * s, row_h);
        let folder = Rect::new(zip.x - 10. * s - 96. * s, row2, 96. * s, row_h);
        let auto = Rect::new(folder.x - 10. * s - 74. * s, row2, 74. * s, row_h);
        let online_btn = Rect::new(auto.x - 10. * s - 74. * s, row2, 74. * s, row_h);
        let chart_label = self
            .chart_path
            .as_deref()
            .map(display_path)
            .unwrap_or_else(|| "尚未选择谱面".into());
        text(
            &fonts.ui,
            &fit_text(
                &fonts.ui,
                &chart_label,
                19. * s,
                online_btn.x - (card.x + pad + label_w) - 12. * s,
            ),
            card.x + pad + label_w,
            row2 + row_h / 2. + 7. * s,
            19. * s,
            if self.chart_path.is_some() {
                Color::new(0.9, 0.92, 0.96, 1.)
            } else {
                Color::new(0.62, 0.68, 0.76, 1.)
            },
        );
        if button(
            online_btn,
            "在线",
            &fonts.ui,
            16. * s,
            ButtonKind::Ghost,
            !busy,
        ) {
            self.online.open(replay_chart_id);
        }
        if button(
            auto,
            "自动",
            &fonts.ui,
            16. * s,
            ButtonKind::Ghost,
            !busy && self.meta.is_some(),
        ) {
            self.auto_chart();
        }
        if button(
            folder,
            "文件夹",
            &fonts.ui,
            16. * s,
            ButtonKind::Ghost,
            !busy,
        ) {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("选择谱面文件夹")
                .pick_folder()
            {
                self.set_chart(path);
            }
        }
        if button(zip, "ZIP", &fonts.ui, 16. * s, ButtonKind::Ghost, !busy) {
            if let Some(path) = pick_file("选择谱面 ZIP", &["zip", "phira", "pez"], None) {
                self.set_chart(path);
            }
        }

        // 回放信息
        let info_y = row2 + row_h + 26. * s;
        if let Some(meta) = &self.meta {
            let player = meta.meta.player.as_deref().unwrap_or("未记录玩家");
            let level = if meta.meta.level.is_empty() {
                String::new()
            } else {
                format!(" · {}", meta.meta.level)
            };
            let line = format!(
                "{player} · {}{level} · {} 个判定 · {} 个触点",
                meta.meta.name,
                meta.judges.len(),
                meta.touches.len()
            );
            text(
                &fonts.ui,
                &fit_text(&fonts.ui, &line, 18. * s, card.w - pad * 2.),
                card.x + pad,
                info_y,
                18. * s,
                Color::new(0.78, 0.83, 0.9, 1.),
            );
        } else {
            text(
                &fonts.ui,
                "选择回放后会自动查找同一目录或 Phira Pro 数据目录中的谱面。",
                card.x + pad,
                info_y,
                18. * s,
                Color::new(0.62, 0.68, 0.76, 1.),
            );
        }

        // 已选好回放时，在空白处显示一条简短的使用提示。
        if self.meta.is_some() {
            let hint = Rect::new(
                card.x + pad,
                card.bottom() - 190. * s,
                card.w - pad * 2.,
                62. * s,
            );
            round_rect(hint, 12. * s, Color::new(0.045, 0.07, 0.105, 0.9));
            draw_rectangle(
                hint.x,
                hint.y + 11. * s,
                3. * s,
                hint.h - 22. * s,
                Color::new(1., 0.72, 0.32, 0.65),
            );
            text(
                &fonts.ui,
                "导出前建议先播放一次，确认判定、音画同步和谱面匹配。",
                hint.x + 18. * s,
                hint.y + 24. * s,
                16. * s,
                Color::new(0.8, 0.85, 0.92, 1.),
            );
            text(
                &fonts.ui,
                "视频导出需要 FFmpeg，可在导出界面设置分辨率、帧率与时间区间。",
                hint.x + 18. * s,
                hint.y + 47. * s,
                16. * s,
                Color::new(0.62, 0.69, 0.78, 1.),
            );
        }

        // 状态
        if let Some((message, error)) = &self.status {
            text(
                &fonts.ui,
                &fit_text(&fonts.ui, message, 17. * s, card.w - pad * 2.),
                card.x + pad,
                card.bottom() - 104. * s,
                17. * s,
                if *error {
                    Color::new(1., 0.55, 0.5, 1.)
                } else {
                    Color::new(0.7, 0.85, 0.8, 1.)
                },
            );
        }

        // 底部操作
        let buttons_y = card.bottom() - pad - 48. * s;
        let play = Rect::new(card.x + pad, buttons_y, 170. * s, 48. * s);
        let export = Rect::new(play.right() + 14. * s, buttons_y, 170. * s, 48. * s);
        let quit = Rect::new(card.right() - pad - 96. * s, buttons_y, 96. * s, 48. * s);
        // 只选了回放也可以开始；缺少谱面时会在 request_action 里自动下载。
        let can_start = self.replay_path.is_some() && !busy;
        if button(
            play,
            "开始播放",
            &fonts.ui,
            19. * s,
            ButtonKind::Primary,
            can_start,
        ) {
            action = self.request_action(HomeAction::Play);
        }
        if button(
            export,
            "导出视频",
            &fonts.ui,
            19. * s,
            ButtonKind::Secondary,
            can_start,
        ) {
            action = self.request_action(HomeAction::Export);
        }
        if button(quit, "退出", &fonts.ui, 17. * s, ButtonKind::Ghost, true) {
            action = HomeAction::Quit;
        }

        let version = concat!("v", env!("CARGO_PKG_VERSION"));
        let version_width =
            measure_text(version, Some(fonts.ui), (16. * s).round() as u16, 1.).width;
        text(
            &fonts.ui,
            version,
            w - margin - version_width,
            h - 18. * s,
            16. * s,
            Color::new(0.72, 0.78, 0.86, 0.6),
        );

        if self.escape_guard {
            if !is_key_down(KeyCode::Escape) {
                self.escape_guard = false;
            }
        } else if is_key_pressed(KeyCode::Escape) {
            action = HomeAction::Quit;
        } else if is_key_pressed(KeyCode::Enter) && can_start {
            action = self.request_action(HomeAction::Play);
        }
        action
    }
}

struct OnlineUi {
    open: bool,
    account: Option<online::Account>,
    email: TextField,
    password: TextField,
    chart_id: TextField,
    focus: OnlineFocus,
    busy: bool,
    rx: Option<Receiver<online::Event>>,
    status: Option<(String, bool)>,
    downloaded: Option<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OnlineFocus {
    None,
    Email,
    Password,
    ChartId,
}

impl Default for OnlineUi {
    fn default() -> Self {
        Self {
            open: false,
            account: online::load_account(),
            email: TextField::default(),
            password: TextField::default(),
            chart_id: TextField::default(),
            focus: OnlineFocus::None,
            busy: false,
            rx: None,
            status: None,
            downloaded: None,
        }
    }
}

impl OnlineUi {
    fn open(&mut self, replay_id: Option<i32>) {
        if !self.open {
            if let Some(id) = replay_id {
                self.chart_id.value = id.to_string();
            }
            self.focus = if self.account.is_some() {
                OnlineFocus::ChartId
            } else {
                OnlineFocus::Email
            };
            self.status = None;
        }
        self.open = true;
    }

    fn close(&mut self) {
        self.open = false;
        self.busy = false;
        self.rx = None;
        self.password.value.clear();
        self.focus = OnlineFocus::None;
    }

    fn poll(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }
        for event in events {
            match event {
                online::Event::Status(message) => {
                    self.status = Some((message, false));
                }
                online::Event::Login(result) => {
                    self.busy = false;
                    self.rx = None;
                    match result {
                        Ok(account) => {
                            if let Err(err) = online::save_account(&account) {
                                self.status =
                                    Some((format!("已登录，但保存登录状态失败：{err:#}"), true));
                            } else {
                                self.status = Some((format!("已登录：{}", account.name), false));
                            }
                            self.account = Some(account);
                            self.focus = OnlineFocus::ChartId;
                        }
                        Err(err) => {
                            self.status = Some((err, true));
                        }
                    }
                }
                online::Event::Download {
                    result,
                    account,
                    session_expired,
                } => {
                    self.busy = false;
                    self.rx = None;
                    if session_expired {
                        self.account = None;
                        online::clear_account();
                    } else if let Some(account) = account {
                        if let Err(err) = online::save_account(&account) {
                            self.status =
                                Some((format!("无法保存续期后的登录状态：{err:#}"), true));
                        }
                        self.account = Some(account);
                    }
                    match result {
                        Ok(path) => {
                            self.status = Some((format!("已下载：{}", path.display()), false));
                            self.downloaded = Some(path);
                        }
                        Err(err) => {
                            if session_expired {
                                self.status =
                                    Some((format!("登录状态已失效，请重新登录。{err}"), true));
                                self.focus = OnlineFocus::Email;
                            } else {
                                self.status = Some((err, true));
                            }
                        }
                    }
                }
            }
        }
    }

    fn start_login(&mut self) {
        let email = self.email.value.trim().to_owned();
        if email.is_empty() {
            self.status = Some(("请填写 Phira 邮箱".into(), true));
            self.focus = OnlineFocus::Email;
            return;
        }
        if self.password.value.is_empty() {
            self.status = Some(("请填写密码".into(), true));
            self.focus = OnlineFocus::Password;
            return;
        }
        self.busy = true;
        self.status = Some(("正在登录…".into(), false));
        self.rx = Some(online::spawn_login(email, self.password.value.clone()));
    }

    fn start_download(&mut self) {
        let Some(id) = parse_chart_id(&self.chart_id.value) else {
            self.status = Some(("谱面 ID 必须是正整数".into(), true));
            self.focus = OnlineFocus::ChartId;
            return;
        };
        self.busy = true;
        self.status = Some((format!("准备下载谱面 {id}…"), false));
        self.rx = Some(online::spawn_download(id, self.account.clone()));
    }

    fn frame(&mut self, fonts: &Fonts) -> Option<PathBuf> {
        self.poll();
        if let Some(path) = self.downloaded.take() {
            self.close();
            return Some(path);
        }

        set_default_camera();
        let (w, h) = (screen_width(), screen_height());
        let s = (w / 1280.).min(h / 720.).clamp(0.3, 2.);
        draw_plain_background(w, h);
        let margin = 28. * s;
        let card_w = (780. * s).min(w - margin * 2.);
        let card_h = (520. * s).min(h - margin * 2.);
        let card = Rect::new((w - card_w) / 2., (h - card_h) / 2., card_w, card_h);
        round_rect(card, 24. * s, Color::new(0.02, 0.035, 0.06, 0.96));
        round_rect(
            Rect::new(card.x + 1.5, card.y + 1.5, card.w - 3., card.h - 3.),
            22.5 * s,
            Color::new(0.045, 0.065, 0.095, 0.96),
        );

        let pad = 30. * s;
        let left = card.x + pad;
        let right = card.right() - pad;
        text(
            &fonts.ui,
            "Phira 在线谱面",
            left,
            card.y + 48. * s,
            28. * s,
            WHITE,
        );
        text(
            &fonts.ui,
            "登录可选；公开谱面无需登录即可下载。",
            left,
            card.y + 74. * s,
            15. * s,
            Color::new(0.64, 0.7, 0.78, 1.),
        );

        let mut start_login = false;
        let mut start_download = false;
        let mut logout = false;
        let account_y = card.y + 96. * s;
        text(
            &fonts.ui,
            "账号",
            left,
            account_y + 17. * s,
            17. * s,
            accent(),
        );

        if let Some(account) = &self.account {
            text(
                &fonts.ui,
                &fit_text(
                    &fonts.ui,
                    &format!("已登录：{}（ID {}）", account.name, account.id),
                    17. * s,
                    card.w - pad * 2. - 120. * s,
                ),
                left,
                account_y + 48. * s,
                17. * s,
                Color::new(0.86, 0.9, 0.95, 1.),
            );
            let logout_btn = Rect::new(right - 104. * s, account_y + 24. * s, 104. * s, 34. * s);
            if button(
                logout_btn,
                "退出登录",
                &fonts.ui,
                14. * s,
                ButtonKind::Ghost,
                !self.busy,
            ) {
                logout = true;
            }
        } else {
            let usable = card.w - pad * 2.;
            let gap = 10. * s;
            let email_w = (usable - gap * 2.) * 0.42;
            let password_w = (usable - gap * 2.) * 0.34;
            let email_rect = Rect::new(left, account_y + 46. * s, email_w, 38. * s);
            let password_rect = Rect::new(
                email_rect.right() + gap,
                account_y + 46. * s,
                password_w,
                38. * s,
            );
            let login_btn = Rect::new(
                password_rect.right() + gap,
                account_y + 46. * s,
                right - (password_rect.right() + gap),
                38. * s,
            );
            self.email.draw(
                email_rect,
                "邮箱",
                "you@example.com",
                self.focus == OnlineFocus::Email,
                &fonts.ui,
                s,
            );
            self.password.draw_password(
                password_rect,
                "密码",
                "不保存密码",
                self.focus == OnlineFocus::Password,
                &fonts.ui,
                s,
            );
            if button(
                login_btn,
                if self.busy { "登录中…" } else { "登录" },
                &fonts.ui,
                15. * s,
                ButtonKind::Secondary,
                !self.busy,
            ) {
                start_login = true;
            }
        }

        let divider_y = card.y + 190. * s;
        draw_rectangle(
            left,
            divider_y,
            card.w - pad * 2.,
            1. * s,
            Color::new(0.16, 0.2, 0.26, 1.),
        );

        text(
            &fonts.ui,
            "在线谱面",
            left,
            divider_y + 34. * s,
            17. * s,
            accent(),
        );
        let id_rect = Rect::new(left, divider_y + 60. * s, 180. * s, 38. * s);
        self.chart_id.draw(
            id_rect,
            "谱面 ID",
            "",
            self.focus == OnlineFocus::ChartId,
            &fonts.ui,
            s,
        );
        let download_btn = Rect::new(
            id_rect.right() + 12. * s,
            divider_y + 60. * s,
            150. * s,
            38. * s,
        );
        if button(
            download_btn,
            if self.busy {
                "下载中…"
            } else {
                "下载并载入"
            },
            &fonts.ui,
            15. * s,
            ButtonKind::Primary,
            !self.busy,
        ) {
            start_download = true;
        }
        text(
            &fonts.ui,
            "下载后保存到本地数据目录，并自动选中；仍按本地谱面加载。",
            left,
            divider_y + 124. * s,
            14. * s,
            Color::new(0.58, 0.64, 0.72, 1.),
        );

        if let Some((message, error)) = &self.status {
            text(
                &fonts.ui,
                &fit_text(&fonts.ui, message, 15. * s, card.w - pad * 2.),
                left,
                divider_y + 164. * s,
                15. * s,
                if *error {
                    Color::new(1., 0.58, 0.52, 1.)
                } else {
                    Color::new(0.7, 0.85, 0.8, 1.)
                },
            );
        }

        let close_btn = Rect::new(
            card.x + pad,
            card.bottom() - pad - 40. * s,
            110. * s,
            40. * s,
        );
        let mut close = button(
            close_btn,
            "关闭",
            &fonts.ui,
            16. * s,
            ButtonKind::Ghost,
            true,
        );

        let mut clicked_field = OnlineFocus::None;
        let mouse = mouse_position();
        let mouse = vec2(mouse.0, mouse.1);
        if is_mouse_button_pressed(MouseButton::Left) {
            if self.account.is_none() {
                let usable = card.w - pad * 2.;
                let gap = 10. * s;
                let email_w = (usable - gap * 2.) * 0.42;
                let email_rect = Rect::new(left, account_y + 46. * s, email_w, 38. * s);
                let password_rect = Rect::new(
                    email_rect.right() + gap,
                    account_y + 46. * s,
                    (usable - gap * 2.) * 0.34,
                    38. * s,
                );
                if email_rect.contains(mouse) {
                    clicked_field = OnlineFocus::Email;
                } else if password_rect.contains(mouse) {
                    clicked_field = OnlineFocus::Password;
                }
            }
            let id_rect = Rect::new(left, divider_y + 60. * s, 180. * s, 38. * s);
            if id_rect.contains(mouse) {
                clicked_field = OnlineFocus::ChartId;
            }
        }
        if clicked_field != OnlineFocus::None {
            self.focus = clicked_field;
        }

        if is_key_pressed(KeyCode::Tab) {
            self.focus = match (self.account.is_some(), self.focus) {
                (false, OnlineFocus::Email) => OnlineFocus::Password,
                (false, OnlineFocus::Password) => OnlineFocus::ChartId,
                (_, OnlineFocus::ChartId) => {
                    if self.account.is_some() {
                        OnlineFocus::ChartId
                    } else {
                        OnlineFocus::Email
                    }
                }
                _ => OnlineFocus::ChartId,
            };
        }

        if !self.busy {
            match self.focus {
                OnlineFocus::Email => self.email.input_text(254),
                OnlineFocus::Password => self.password.input_text(128),
                OnlineFocus::ChartId => self.chart_id.input_chart_id(),
                OnlineFocus::None => {}
            }
            if is_key_pressed(KeyCode::Enter) {
                match self.focus {
                    OnlineFocus::Email | OnlineFocus::Password if self.account.is_none() => {
                        start_login = true;
                    }
                    OnlineFocus::ChartId => start_download = true,
                    _ => {}
                }
            }
        }

        if start_login {
            self.start_login();
        }
        if start_download {
            self.start_download();
        }
        if logout {
            self.account = None;
            online::clear_account();
            self.status = Some(("已退出 Phira 账号".into(), false));
            self.focus = OnlineFocus::Email;
        }
        if is_key_pressed(KeyCode::Escape) {
            close = true;
        }
        if close {
            self.close();
        }
        None
    }
}

async fn export_flow(
    replay_path: PathBuf,
    chart_path: PathBuf,
    name: String,
    fonts: &Fonts,
    painter: &mut TextPainter,
) -> Result<FlowResult> {
    let mut form = ExportForm::new(replay_path, chart_path, name);
    loop {
        match form.frame(fonts) {
            FormAction::None => {
                next_frame().await;
            }
            FormAction::Back => return Ok(FlowResult::Back),
            FormAction::Start => {
                let options = match form.options() {
                    Ok(options) => options,
                    Err(err) => {
                        form.status = Some((format!("{err:#}"), true));
                        continue;
                    }
                };
                let mut scene = match crate::load_scene(
                    form.replay_path.clone(),
                    form.chart_path.clone(),
                    painter,
                )
                .await
                {
                    Ok(scene) => scene,
                    Err(err) => {
                        form.status = Some((format!("无法加载谱面：{err:#}"), true));
                        continue;
                    }
                };
                if let Some(warning) = scene.warning() {
                    eprintln!("警告：{warning}");
                }

                let mut progress = GuiProgress {
                    title: format!(
                        "{} · {}×{} · {} fps",
                        form.name, options.width, options.height, options.fps
                    ),
                    font: &fonts.ui,
                };
                match video::render(&mut scene, &options, painter, &mut progress).await {
                    Ok(Some(path)) => return Ok(FlowResult::Done(path)),
                    Ok(None) => {
                        let _ = std::fs::remove_file(&options.output);
                        return Ok(FlowResult::Cancelled);
                    }
                    Err(err) => {
                        let _ = std::fs::remove_file(&options.output);
                        form.status = Some((format!("{err:#}"), true));
                    }
                }
            }
        }
    }
}

struct ExportForm {
    replay_path: PathBuf,
    chart_path: PathBuf,
    name: String,
    width: u32,
    height: u32,
    fps: u32,
    crf: u32,
    hud: bool,
    touches: bool,
    audio: bool,
    opening: bool,
    ending: bool,
    ap_fc: bool,
    double_hint: bool,
    particle: bool,
    show_acc: bool,
    judgement_details: bool,
    perfect_combo: bool,
    early_late: bool,
    start: TextField,
    end: TextField,
    output: PathBuf,
    output_auto: bool,
    focus: Focus,
    status: Option<(String, bool)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    None,
    Start,
    End,
}

impl ExportForm {
    fn new(replay_path: PathBuf, chart_path: PathBuf, name: String) -> Self {
        let output = default_output(&replay_path);
        Self {
            replay_path,
            chart_path,
            name,
            width: 1920,
            height: 1080,
            fps: 60,
            crf: 18,
            hud: true,
            touches: true,
            audio: true,
            // 与 Phira Pro 的完整演出预设一致；可在界面里关闭开场 / 结算。
            opening: true,
            ending: true,
            ap_fc: true,
            double_hint: true,
            particle: true,
            show_acc: false,
            judgement_details: false,
            perfect_combo: false,
            early_late: false,
            start: TextField::default(),
            end: TextField::default(),
            output,
            output_auto: true,
            focus: Focus::None,
            status: None,
        }
    }

    fn options(&self) -> Result<video::Options> {
        let start = self
            .start
            .parse()
            .context("起点必须是秒数，例如 12.5；留空表示 0")?
            .unwrap_or(0.);
        let end = self
            .end
            .parse()
            .context("终点必须是秒数，例如 90；留空表示整张谱面")?;
        if let Some(end) = end {
            anyhow::ensure!(end > start, "终点必须晚于起点");
        }
        let mut presentation = PresentationOptions::default();
        presentation.opening = self.opening;
        presentation.ending = self.ending;
        let mut visuals = VisualOptions::default();
        visuals.ap_fc_indicator = self.ap_fc;
        visuals.double_hint = self.double_hint;
        visuals.particle = self.particle;
        visuals.show_acc = self.show_acc;
        visuals.show_judgement_details = self.judgement_details;
        visuals.show_perfect_combo = self.perfect_combo;
        visuals.show_early_late = self.early_late;
        let mut options = video::Options {
            output: ensure_mp4(self.output.clone()),
            width: self.width,
            height: self.height,
            fps: self.fps,
            start,
            end,
            show_hud: self.hud,
            show_touches: self.touches,
            audio: self.audio,
            crf: self.crf,
            presentation,
            visuals,
        };
        options.validate()?;
        // 让输出文件与界面显示保持一致。
        options.output = ensure_mp4(self.output.clone());
        Ok(options)
    }

    fn frame(&mut self, fonts: &Fonts) -> FormAction {
        set_default_camera();
        let (w, h) = (screen_width(), screen_height());
        let s = (w / 1280.).min(h / 720.).clamp(0.3, 2.);
        draw_plain_background(w, h);
        let margin = 40. * s;

        let card_w = (980. * s).min(w - margin * 2.);
        let card_h = (620. * s).min(h - margin * 2.);
        let card = Rect::new((w - card_w) / 2., (h - card_h) / 2., card_w, card_h);
        round_rect(card, 24. * s, Color::new(0.02, 0.035, 0.06, 0.92));
        round_rect(
            Rect::new(card.x + 1.5, card.y + 1.5, card.w - 3., card.h - 3.),
            22.5 * s,
            Color::new(0.04, 0.06, 0.09, 0.9),
        );

        let pad = 32. * s;
        let left = card.x + pad;
        let right = card.right() - pad;
        text(
            &fonts.ui,
            "导出视频",
            left,
            card.y + 52. * s,
            30. * s,
            WHITE,
        );
        text(
            &fonts.ui,
            &fit_text(
                &fonts.ui,
                &format!("{} · {}", self.name, display_path(&self.replay_path)),
                16. * s,
                card.w - pad * 2.,
            ),
            left,
            card.y + 78. * s,
            16. * s,
            Color::new(0.66, 0.72, 0.8, 1.),
        );

        let mut action = FormAction::None;
        let mut focus = self.focus;
        let mouse = mouse_position();
        let mouse = vec2(mouse.0, mouse.1);
        let clicked = is_mouse_button_pressed(MouseButton::Left);

        // 分辨率 / 帧率
        let mut x = left;
        let y = card.y + 112. * s;
        text(&fonts.ui, "画面", x, y + 18. * s, 17. * s, accent());
        x += 58. * s;
        for (width, height, label) in [
            (1920u32, 1080u32, "1080p"),
            (1280, 720, "720p"),
            (960, 540, "540p"),
        ] {
            let r = Rect::new(x, y, 74. * s, 32. * s);
            let selected = self.width == width && self.height == height;
            if choice_button(r, label, &fonts.ui, 15. * s, selected) {
                self.width = width;
                self.height = height;
            }
            x += 82. * s;
        }
        x += 12. * s;
        for fps in [60u32, 30] {
            let r = Rect::new(x, y, 70. * s, 32. * s);
            if choice_button(
                r,
                &format!("{fps} fps"),
                &fonts.ui,
                15. * s,
                self.fps == fps,
            ) {
                self.fps = fps;
            }
            x += 78. * s;
        }

        // 画质
        let y2 = y + 48. * s;
        x = left;
        text(&fonts.ui, "画质", x, y2 + 18. * s, 17. * s, accent());
        x += 58. * s;
        for (crf, label) in [(16u32, "高画质"), (18, "标准"), (23, "小体积")] {
            let r = Rect::new(x, y2, 88. * s, 32. * s);
            if choice_button(r, label, &fonts.ui, 15. * s, self.crf == crf) {
                self.crf = crf;
            }
            x += 96. * s;
        }

        // 区间
        let y3 = y2 + 56. * s;
        text(&fonts.ui, "区间", left, y3 + 18. * s, 17. * s, accent());
        let start_rect = Rect::new(left + 58. * s, y3, 120. * s, 38. * s);
        let end_rect = Rect::new(start_rect.right() + 26. * s, y3, 120. * s, 38. * s);
        self.start.draw(
            start_rect,
            "起点（秒）",
            "0",
            focus == Focus::Start,
            &fonts.ui,
            s,
        );
        self.end.draw(
            end_rect,
            "终点（秒）",
            "留空 = 整段",
            focus == Focus::End,
            &fonts.ui,
            s,
        );
        text(
            &fonts.ui,
            "谱面时间，不是视频时长",
            end_rect.right() + 18. * s,
            y3 + 24. * s,
            15. * s,
            Color::new(0.58, 0.64, 0.72, 1.),
        );

        // 输出
        let y4 = y3 + 58. * s;
        text(&fonts.ui, "输出", left, y4 + 20. * s, 17. * s, accent());
        let choose = Rect::new(right - 100. * s, y4, 100. * s, 38. * s);
        let output_text = fit_text(
            &fonts.ui,
            &self.output.display().to_string(),
            17. * s,
            choose.x - (left + 58. * s) - 14. * s,
        );
        text(
            &fonts.ui,
            &output_text,
            left + 58. * s,
            y4 + 24. * s,
            17. * s,
            Color::new(0.86, 0.9, 0.95, 1.),
        );
        if button(choose, "选择…", &fonts.ui, 15. * s, ButtonKind::Ghost, true) {
            let suggestion = self
                .output
                .file_name()
                .map(|it| it.to_string_lossy().into_owned())
                .unwrap_or_else(|| "replay.mp4".into());
            let mut dialog = rfd::FileDialog::new()
                .set_title("保存视频")
                .set_file_name(&suggestion)
                .add_filter("MP4 视频", &["mp4"]);
            if let Some(dir) = self.output.parent().filter(|it| it.is_dir()) {
                dialog = dialog.set_directory(dir);
            }
            if let Some(path) = dialog.save_file() {
                self.output = ensure_mp4(path);
                self.output_auto = false;
            }
        }

        // 内容
        let y5 = y4 + 54. * s;
        text(&fonts.ui, "内容", left, y5 + 20. * s, 17. * s, accent());
        let hud = Rect::new(left + 58. * s, y5, 96. * s, 36. * s);
        let touches = Rect::new(hud.right() + 16. * s, y5, 110. * s, 36. * s);
        let audio = Rect::new(touches.right() + 16. * s, y5, 110. * s, 36. * s);
        if toggle(hud, "分数 HUD", self.hud, &fonts.ui, 15. * s, s) {
            self.hud = !self.hud;
        }
        if toggle(touches, "回放触点", self.touches, &fonts.ui, 15. * s, s) {
            self.touches = !self.touches;
        }
        if toggle(audio, "音轨", self.audio, &fonts.ui, 15. * s, s) {
            self.audio = !self.audio;
        }

        // Phira Pro 的演出流程
        let y6 = y5 + 50. * s;
        text(&fonts.ui, "演出", left, y6 + 20. * s, 17. * s, accent());
        let opening = Rect::new(left + 58. * s, y6, 96. * s, 36. * s);
        let ending = Rect::new(opening.right() + 16. * s, y6, 96. * s, 36. * s);
        if toggle(opening, "开场", self.opening, &fonts.ui, 15. * s, s) {
            self.opening = !self.opening;
        }
        if toggle(ending, "结算", self.ending, &fonts.ui, 15. * s, s) {
            self.ending = !self.ending;
        }
        text(
            &fonts.ui,
            &fit_text(
                &fonts.ui,
                "谱面阶段按回放绘制",
                14. * s,
                (card.x + card.w * 0.54 - 22. * s) - (ending.right() + 24. * s),
            ),
            ending.right() + 24. * s,
            y6 + 22. * s,
            14. * s,
            Color::new(0.58, 0.64, 0.72, 1.),
        );

        // 游玩画面选项：与 Phira Pro 的渲染页共用同一组 VisualOptions。
        let opt_x = card.x + card.w * 0.56;
        let opt_w = right - opt_x;
        let opt_y = y5 - 10. * s;
        round_rect(
            Rect::new(opt_x - 18. * s, opt_y, opt_w + 18. * s, 208. * s),
            14. * s,
            Color::new(0.045, 0.07, 0.105, 0.88),
        );
        text(
            &fonts.ui,
            "游玩画面",
            opt_x,
            opt_y + 34. * s,
            17. * s,
            accent(),
        );
        let col_w = (opt_w - 12. * s) / 2.;
        let mut toggles = [
            (opt_x, opt_y + 52. * s, "AP/FC 指示", &mut self.ap_fc),
            (
                opt_x + col_w + 12. * s,
                opt_y + 52. * s,
                "双押提示",
                &mut self.double_hint,
            ),
            (opt_x, opt_y + 92. * s, "打击粒子", &mut self.particle),
            (
                opt_x + col_w + 12. * s,
                opt_y + 92. * s,
                "实时准确率",
                &mut self.show_acc,
            ),
            (
                opt_x,
                opt_y + 132. * s,
                "判定详情",
                &mut self.judgement_details,
            ),
            (
                opt_x + col_w + 12. * s,
                opt_y + 132. * s,
                "Perfect 连击",
                &mut self.perfect_combo,
            ),
            (
                opt_x,
                opt_y + 172. * s,
                "Early / Late",
                &mut self.early_late,
            ),
        ];
        for (x, row_y, label, value) in &mut toggles {
            let rect = Rect::new(*x, *row_y, col_w, 30. * s);
            if toggle(rect, label, **value, &fonts.ui, 15. * s, s) {
                **value = !**value;
            }
        }

        if clicked {
            focus = if start_rect.contains(mouse) {
                Focus::Start
            } else if end_rect.contains(mouse) {
                Focus::End
            } else {
                Focus::None
            };
        }
        if is_key_pressed(KeyCode::Tab) {
            focus = match focus {
                Focus::None | Focus::Start => Focus::End,
                Focus::End => Focus::Start,
            };
        }
        if focus == Focus::Start {
            self.start.input();
        } else if focus == Focus::End {
            self.end.input();
        }
        self.focus = focus;

        if let Some((message, error)) = &self.status {
            text(
                &fonts.ui,
                &fit_text(&fonts.ui, message, 16. * s, card.w - pad * 2.),
                left,
                card.bottom() - 82. * s,
                16. * s,
                if *error {
                    Color::new(1., 0.55, 0.5, 1.)
                } else {
                    Color::new(0.7, 0.85, 0.8, 1.)
                },
            );
        }

        let buttons_y = card.bottom() - pad - 46. * s;
        let back = Rect::new(left, buttons_y, 100. * s, 46. * s);
        let start_button = Rect::new(right - 170. * s, buttons_y, 170. * s, 46. * s);
        if button(back, "返回", &fonts.ui, 17. * s, ButtonKind::Ghost, true)
            || is_key_pressed(KeyCode::Escape)
        {
            action = FormAction::Back;
        }
        if button(
            start_button,
            "开始导出",
            &fonts.ui,
            18. * s,
            ButtonKind::Primary,
            true,
        ) || (is_key_pressed(KeyCode::Enter) && focus == Focus::None)
        {
            action = FormAction::Start;
        }
        action
    }
}

#[derive(Default)]
struct TextField {
    value: String,
}

/// 单行输入框允许的字符：过滤 ASCII / Unicode 控制符及零宽格式字符。
fn is_text_input_char(c: char) -> bool {
    !c.is_control()
        && !matches!(
            c,
            '\u{00ad}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{feff}'
        )
}

/// Ctrl / Cmd 组合键不应被当成普通字符输入；保留 AltGr（Ctrl+Alt）字符。
///
/// 同时检查 `is_key_pressed` / `is_key_released`：一次很快的 Ctrl+A 可能在
/// 同一帧内按下又松开，仅看 `is_key_down` 会漏掉，随后字符事件才被输入框取走。
fn is_shortcut_modifier() -> bool {
    let active = |key| is_key_down(key) || is_key_pressed(key) || is_key_released(key);
    let modifier = active(KeyCode::LeftControl)
        || active(KeyCode::RightControl)
        || active(KeyCode::LeftSuper)
        || active(KeyCode::RightSuper);
    let alt = active(KeyCode::LeftAlt) || active(KeyCode::RightAlt);
    modifier && !alt
}

/// 解析谱面 ID：允许带 `#` 或全角 `＃` 前缀，也可以直接输入数字。
fn parse_chart_id(text: &str) -> Option<i32> {
    let text = text
        .trim()
        .trim_start_matches(|c| c == '#' || c == '＃')
        .trim();
    let id = text.parse::<i32>().ok()?;
    (id > 0).then_some(id)
}

impl TextField {
    fn input_chart_id(&mut self) {
        let shortcut = is_shortcut_modifier();
        while let Some(c) = get_char_pressed() {
            let accepted = c.is_ascii_digit() || c == '#' || c == '＃';
            if !shortcut && accepted && self.value.chars().count() < 12 {
                self.value.push(c);
            }
        }
        self.value
            .retain(|c| c.is_ascii_digit() || c == '#' || c == '＃');
        if is_key_pressed(KeyCode::Backspace) {
            self.value.pop();
        }
    }

    fn input(&mut self) {
        let shortcut = is_shortcut_modifier();
        while let Some(c) = get_char_pressed() {
            if !shortcut && (c.is_ascii_digit() || c == '.') && self.value.chars().count() < 8 {
                self.value.push(c);
            }
        }
        self.value.retain(|c| c.is_ascii_digit() || c == '.');
        if is_key_pressed(KeyCode::Backspace) {
            self.value.pop();
        }
    }

    fn input_text(&mut self, max: usize) {
        let shortcut = is_shortcut_modifier();
        while let Some(c) = get_char_pressed() {
            if !shortcut && is_text_input_char(c) && self.value.chars().count() < max {
                self.value.push(c);
            }
        }
        // 输入法或系统粘贴可能绕过逐字符判断，这里再清理一次。
        self.value.retain(is_text_input_char);
        if is_key_pressed(KeyCode::Backspace) {
            self.value.pop();
        }
    }

    fn parse(&self) -> Result<Option<f64>> {
        let text = self.value.trim();
        if text.is_empty() {
            return Ok(None);
        }
        let value: f64 = text.parse().context("不是有效数字")?;
        anyhow::ensure!(value.is_finite() && value >= 0., "时间必须是非负数");
        Ok(Some(value))
    }

    fn draw(&self, rect: Rect, label: &str, hint: &str, focused: bool, font: &Font, s: f32) {
        self.draw_inner(rect, label, hint, focused, font, s, false);
    }

    fn draw_password(
        &self,
        rect: Rect,
        label: &str,
        hint: &str,
        focused: bool,
        font: &Font,
        s: f32,
    ) {
        self.draw_inner(rect, label, hint, focused, font, s, true);
    }

    fn draw_inner(
        &self,
        rect: Rect,
        label: &str,
        hint: &str,
        focused: bool,
        font: &Font,
        s: f32,
        masked: bool,
    ) {
        text(
            font,
            label,
            rect.x,
            rect.y - 6. * s,
            14. * s,
            Color::new(0.62, 0.68, 0.76, 1.),
        );
        round_rect(
            rect,
            9. * s,
            if focused {
                Color::new(0.09, 0.13, 0.19, 1.)
            } else {
                Color::new(0.06, 0.085, 0.12, 1.)
            },
        );
        if focused {
            round_rect(
                Rect::new(rect.x + 1., rect.y + 1., rect.w - 2., rect.h - 2.),
                8. * s,
                Color::new(0.09, 0.13, 0.19, 1.),
            );
        }
        let masked_value = if masked {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        };
        let shown = if self.value.is_empty() {
            hint
        } else {
            masked_value.as_str()
        };
        let color = if self.value.is_empty() {
            Color::new(0.5, 0.56, 0.64, 1.)
        } else {
            WHITE
        };
        text(
            font,
            shown,
            rect.x + 12. * s,
            rect.y + rect.h / 2. + 6. * s,
            17. * s,
            color,
        );
        if focused && (get_time() * 2.) as i64 % 2 == 0 {
            let content = if self.value.is_empty() {
                ""
            } else {
                masked_value.as_str()
            };
            let width = measure_text(content, Some(*font), (17. * s) as u16, 1.).width;
            draw_rectangle(
                (rect.x + 12. * s + width).min(rect.right() - 8.),
                rect.y + 8. * s,
                2. * s,
                rect.h - 16. * s,
                accent(),
            );
        }
    }
}

struct GuiProgress<'a> {
    title: String,
    font: &'a Font,
}

impl video::Progress for GuiProgress<'_> {
    fn update(&mut self, frame: u64, total: u64, elapsed: f64, preview: Texture2D) -> bool {
        let (w, h) = (screen_width(), screen_height());
        let s = (w / 1280.).min(h / 720.).clamp(0.3, 2.);
        set_default_camera();
        clear_background(Color::new(0.015, 0.02, 0.03, 1.));

        let max = vec2(w - 80. * s, h - 180. * s);
        let scale = (max.x / preview.width()).min(max.y / preview.height());
        let size = vec2(preview.width(), preview.height()) * scale;
        draw_texture_ex(
            preview,
            (w - size.x) / 2.,
            24. * s + (max.y - size.y) / 2.,
            WHITE,
            DrawTextureParams {
                dest_size: Some(size),
                // `glReadPixels` 的预览纹理上下颠倒，绘制时翻回来。
                flip_y: true,
                ..Default::default()
            },
        );

        let bar = Rect::new(40. * s, h - 82. * s, w - 80. * s, 10. * s);
        round_rect(bar, 5. * s, Color::new(0.1, 0.13, 0.18, 1.));
        let p = (frame as f32 / total.max(1) as f32).clamp(0., 1.);
        if p > 0. {
            round_rect(Rect::new(bar.x, bar.y, bar.w * p, bar.h), 5. * s, accent());
        }
        text(
            self.font,
            &format!("正在导出视频  {:.1}%", p * 100.),
            40. * s,
            h - 130. * s,
            24. * s,
            WHITE,
        );
        let rate = frame as f64 / elapsed.max(1e-6);
        let remaining = (total.saturating_sub(frame) as f64 / rate.max(1e-6)).ceil() as u64;
        let cancel_hint = "Esc 或点击右下角可取消";
        let hint_width =
            measure_text(cancel_hint, Some(*self.font), (15. * s).round() as u16, 1.).width;
        let info = format!(
            "{}  {} / {} 帧  ·  {:.1} fps  ·  约 {} 秒后完成",
            self.title, frame, total, rate, remaining
        );
        text(
            self.font,
            &fit_text(
                self.font,
                &info,
                16. * s,
                w - 80. * s - hint_width - 20. * s,
            ),
            40. * s,
            h - 104. * s,
            16. * s,
            Color::new(0.72, 0.78, 0.86, 1.),
        );
        text(
            self.font,
            cancel_hint,
            w - 40. * s - hint_width,
            h - 104. * s,
            15. * s,
            Color::new(0.58, 0.64, 0.72, 1.),
        );
        let cancel = Rect::new(w - 150. * s, h - 66. * s, 110. * s, 38. * s);
        let clicked = button(
            cancel,
            "取消",
            self.font,
            16. * s,
            ButtonKind::Secondary,
            true,
        );
        !clicked && !is_key_pressed(KeyCode::Escape)
    }

    fn stage(&mut self, message: &str) -> bool {
        let (w, h) = (screen_width(), screen_height());
        let s = (w / 1280.).min(h / 720.).clamp(0.3, 2.);
        set_default_camera();
        clear_background(Color::new(0.015, 0.02, 0.03, 1.));
        text_center(
            self.font,
            &fit_text(self.font, message, 28. * s, w - 80. * s),
            w / 2.,
            h / 2.,
            28. * s,
            WHITE,
        );
        text_center(
            self.font,
            "完成后会自动开始逐帧渲染",
            w / 2.,
            h / 2. + 30. * s,
            16. * s,
            Color::new(0.6, 0.66, 0.74, 1.),
        );
        !is_key_pressed(KeyCode::Escape)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
}

fn accent() -> Color {
    Color::new(1., 0.72, 0.32, 1.)
}

fn text(font: &Font, text: &str, x: f32, y: f32, size: f32, color: Color) {
    draw_text_ex(
        text,
        x,
        y,
        TextParams {
            font: *font,
            font_size: size.round() as u16,
            color,
            ..Default::default()
        },
    );
}

fn text_center(font: &Font, content: &str, x: f32, y: f32, size: f32, color: Color) {
    let width = measure_text(content, Some(*font), size.round() as u16, 1.).width;
    text(font, content, x - width / 2., y, size, color);
}

fn fit_text(font: &Font, content: &str, size: f32, max_width: f32) -> String {
    if measure_text(content, Some(*font), size.round() as u16, 1.).width <= max_width {
        return content.to_owned();
    }
    let mut out = String::new();
    for c in content.chars() {
        let mut candidate = out.clone();
        candidate.push(c);
        candidate.push('…');
        if measure_text(&candidate, Some(*font), size.round() as u16, 1.).width > max_width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

fn button(
    rect: Rect,
    label: &str,
    font: &Font,
    size: f32,
    kind: ButtonKind,
    enabled: bool,
) -> bool {
    let mouse = mouse_position();
    let mouse = vec2(mouse.0, mouse.1);
    let hover = enabled && rect.contains(mouse);
    let base = match kind {
        ButtonKind::Primary => accent(),
        ButtonKind::Secondary => Color::new(0.12, 0.16, 0.22, 1.),
        ButtonKind::Ghost => Color::new(0., 0., 0., 0.),
    };
    let fill = if !enabled {
        Color::new(0.09, 0.115, 0.16, 1.)
    } else if hover {
        Color::new(
            (base.r + 0.1).min(1.),
            (base.g + 0.1).min(1.),
            (base.b + 0.1).min(1.),
            base.a,
        )
    } else {
        base
    };
    if kind == ButtonKind::Ghost && !hover {
        // 透明按钮只画文字，保持界面简洁。
    } else {
        round_rect(rect, 10., fill);
    }
    let content = label.to_owned();
    let color = if !enabled {
        Color::new(0.48, 0.53, 0.6, 1.)
    } else if kind == ButtonKind::Primary {
        Color::new(0.08, 0.07, 0.05, 1.)
    } else {
        Color::new(0.9, 0.93, 0.97, 1.)
    };
    let tw = measure_text(&content, Some(*font), size.round() as u16, 1.).width;
    text(
        font,
        &fit_text(font, &content, size, rect.w - 14.),
        rect.x + (rect.w - tw.min(rect.w - 14.)) / 2.,
        rect.y + rect.h / 2. + size * 0.35,
        size,
        color,
    );
    hover && is_mouse_button_pressed(MouseButton::Left)
}

fn choice_button(rect: Rect, label: &str, font: &Font, size: f32, selected: bool) -> bool {
    let mouse = mouse_position();
    let mouse = vec2(mouse.0, mouse.1);
    let hover = rect.contains(mouse);
    let fill = if selected {
        Color::new(1., 0.72, 0.32, 0.95)
    } else if hover {
        Color::new(0.14, 0.18, 0.24, 1.)
    } else {
        Color::new(0.08, 0.11, 0.16, 1.)
    };
    round_rect(rect, 8., fill);
    text_center(
        font,
        label,
        rect.x + rect.w / 2.,
        rect.y + rect.h / 2. + size * 0.35,
        size,
        if selected {
            Color::new(0.08, 0.07, 0.05, 1.)
        } else {
            WHITE
        },
    );
    hover && is_mouse_button_pressed(MouseButton::Left)
}

fn toggle(rect: Rect, label: &str, value: bool, font: &Font, size: f32, s: f32) -> bool {
    let mouse = mouse_position();
    let mouse = vec2(mouse.0, mouse.1);
    let hover = rect.contains(mouse);
    let box_size = 20. * s;
    let box_rect = Rect::new(
        rect.x,
        rect.y + (rect.h - box_size) / 2.,
        box_size,
        box_size,
    );
    round_rect(
        box_rect,
        5. * s,
        if value {
            accent()
        } else if hover {
            Color::new(0.2, 0.24, 0.3, 1.)
        } else {
            Color::new(0.1, 0.13, 0.18, 1.)
        },
    );
    if value {
        draw_line(
            box_rect.x + 5. * s,
            box_rect.y + 10. * s,
            box_rect.x + 9. * s,
            box_rect.y + 14. * s,
            2. * s,
            Color::new(0.08, 0.07, 0.05, 1.),
        );
        draw_line(
            box_rect.x + 9. * s,
            box_rect.y + 14. * s,
            box_rect.x + 16. * s,
            box_rect.y + 6. * s,
            2. * s,
            Color::new(0.08, 0.07, 0.05, 1.),
        );
    }
    text(
        font,
        label,
        rect.x + 28. * s,
        rect.y + rect.h / 2. + size * 0.35,
        size,
        if hover {
            WHITE
        } else {
            Color::new(0.82, 0.86, 0.92, 1.)
        },
    );
    hover && is_mouse_button_pressed(MouseButton::Left)
}

fn round_rect(rect: Rect, radius: f32, color: Color) {
    let r = radius.min(rect.w / 2.).min(rect.h / 2.);
    if r <= 0. {
        draw_rectangle(rect.x, rect.y, rect.w, rect.h, color);
        return;
    }
    draw_rectangle(rect.x + r, rect.y, rect.w - r * 2., rect.h, color);
    draw_rectangle(rect.x, rect.y + r, rect.w, rect.h - r * 2., color);
    for (x, y) in [
        (rect.x + r, rect.y + r),
        (rect.right() - r, rect.y + r),
        (rect.x + r, rect.bottom() - r),
        (rect.right() - r, rect.bottom() - r),
    ] {
        draw_circle(x, y, r, color);
    }
}

fn draw_background(texture: &Texture2D, w: f32, h: f32) {
    let scale = (w / texture.width()).max(h / texture.height());
    let size = vec2(texture.width(), texture.height()) * scale;
    draw_texture_ex(
        *texture,
        (w - size.x) / 2.,
        (h - size.y) / 2.,
        WHITE,
        DrawTextureParams {
            dest_size: Some(size),
            ..Default::default()
        },
    );
    draw_rectangle(0., 0., w, h, Color::new(0.01, 0.02, 0.04, 0.58));
}

fn draw_plain_background(w: f32, h: f32) {
    clear_background(Color::new(0.025, 0.035, 0.055, 1.));
    for i in 0..12 {
        let p = i as f32 / 11.;
        draw_rectangle(
            0.,
            h * p * 0.55,
            w,
            h * 0.05,
            Color::new(0.06, 0.08, 0.12, (0.7 - p * 0.5) * 0.3),
        );
    }
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn pick_file(title: &str, extensions: &[&str], dir: Option<PathBuf>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .set_title(title)
        .add_filter("文件", extensions);
    if let Some(dir) = dir {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file()
}

/// 移动端默认导出目录：安卓为 `/sdcard/Download/ppr-Render`，iOS 为应用
/// Documents 下的 `ppr-Render`；桌面端保持“保存在回放文件旁边”。
/// `PPR_RENDER_OUTPUT_DIR` 可覆盖默认目录，桌面端也可用它固定输出位置。
fn default_output_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("PPR_RENDER_OUTPUT_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    platform_output_dir()
}

#[cfg(any(target_os = "android", target_os = "linux"))]
fn android_download_dir() -> PathBuf {
    std::env::var_os("EXTERNAL_STORAGE")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| PathBuf::from("/sdcard"))
        .join("Download")
}

#[cfg(target_os = "android")]
fn platform_output_dir() -> Option<PathBuf> {
    Some(android_download_dir().join("ppr-Render"))
}

#[cfg(target_os = "ios")]
fn platform_output_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Documents").join("ppr-Render"))
}

#[cfg(target_os = "linux")]
fn platform_output_dir() -> Option<PathBuf> {
    // Termux / chroot 等 Linux 环境运行在 Android 上时也使用下载目录。
    let android = ["TERMUX_VERSION", "ANDROID_ROOT", "ANDROID_DATA"]
        .iter()
        .any(|name| std::env::var_os(name).is_some());
    android.then(|| android_download_dir().join("ppr-Render"))
}

#[cfg(not(any(target_os = "android", target_os = "ios", target_os = "linux")))]
fn platform_output_dir() -> Option<PathBuf> {
    None
}

fn default_output(replay: &Path) -> PathBuf {
    let stem = replay
        .file_stem()
        .map(|it| it.to_string_lossy().into_owned())
        .unwrap_or_else(|| "replay".into());
    if let Some(dir) = default_output_dir() {
        // 用户可能第一次导出，默认目录尚不存在。
        if let Err(err) = std::fs::create_dir_all(&dir) {
            eprintln!(
                "无法创建默认输出目录 {}：{err}；改为保存到回放文件旁边",
                dir.display()
            );
            return replay.with_file_name(format!("{stem}-video.mp4"));
        }
        return dir.join(format!("{stem}-video.mp4"));
    }
    replay.with_file_name(format!("{stem}-video.mp4"))
}

fn ensure_mp4(mut path: PathBuf) -> PathBuf {
    if path.extension().is_none() {
        path.set_extension("mp4");
    }
    path
}

#[cfg(test)]
mod tests {
    use super::{is_text_input_char, parse_chart_id};

    #[test]
    fn chart_id_accepts_optional_number_sign() {
        for text in ["51121", "#51121", "＃51121", " #51121 ", "＃ 51121"] {
            assert_eq!(parse_chart_id(text), Some(51121), "输入 {text:?}");
        }
        for text in ["", "#", "＃", "0", "-1", "abc", "51121x", "# 0"] {
            assert_eq!(parse_chart_id(text), None, "输入 {text:?}");
        }
    }

    #[test]
    fn text_input_rejects_control_and_format_chars() {
        for c in [
            '\0', '\u{1}', '\n', '\r', '\t', '\u{7f}', '\u{85}', '\u{200b}', '\u{2028}',
            '\u{202e}', '\u{feff}',
        ] {
            assert!(!is_text_input_char(c), "应过滤 {c:?}");
        }
        for c in ['a', 'Z', '0', '.', '@', ' ', '灵'] {
            assert!(is_text_input_char(c), "不应过滤 {c:?}");
        }
    }
}
