//! 桌面程序入口。真正的应用逻辑在 `lib.rs`，Android 由同一份库导出 `quad_main`。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    ppr_render::app_main();
}
