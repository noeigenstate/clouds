//! The settings window, opened by right-clicking the cloud. It edits a copy of
//! the settings and only replaces the live ones on "save".

use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Instant;

use slint::{ComponentHandle, ModelRc, PhysicalPosition, SharedString, VecModel};

use crate::settings::{self, ApiKind, Engine, Settings};
use crate::{translate, window, Hud, SettingsWindow};

/// The engine pills in the window, in order.
const ENGINES: [Engine; 3] = [Engine::Google, Engine::Api, Engine::Local];

pub fn wire(hud: &Hud, win: &SettingsWindow, settings: Arc<RwLock<Settings>>) {
    hud.on_open_settings({
        let hud = hud.as_weak();
        let win = win.as_weak();
        let settings = settings.clone();
        move || {
            let (Some(hud), Some(win)) = (hud.upgrade(), win.upgrade()) else { return };
            fill_form(&win, &settings.read().unwrap());
            open_next_to(&win, &hud);
            if engine(&win) != Engine::Google {
                refresh_models(&win);
            }
        }
    });

    win.on_engine_changed({
        let win = win.as_weak();
        move || {
            let win = win.unwrap();
            win.set_models(ModelRc::default());
            win.set_status("".into());
            let can_list = match engine(&win) {
                Engine::Google => false,
                Engine::Api => !win.get_api_key().is_empty(),
                Engine::Local => true,
            };
            if can_list {
                refresh_models(&win);
            }
        }
    });

    // Switching between Claude and OpenAI-compatible swaps in sensible defaults.
    win.on_api_kind_changed({
        let win = win.as_weak();
        move || {
            let win = win.unwrap();
            let base = win.get_api_base();
            let model = win.get_api_model();
            let claude = win.get_api_kind() == 0;
            let (default_base, other_base) = if claude {
                (settings::CLAUDE_BASE, settings::OPENAI_BASE)
            } else {
                (settings::OPENAI_BASE, settings::CLAUDE_BASE)
            };
            if base.trim().is_empty() || base == other_base {
                win.set_api_base(default_base.into());
            }
            // A Claude model name means nothing to other services, and vice versa.
            if claude != model.starts_with("claude") {
                win.set_api_model(if claude { settings::CLAUDE_MODEL } else { "" }.into());
            }
            win.set_models(ModelRc::default());
        }
    });

    win.on_refresh_models({
        let win = win.as_weak();
        move || refresh_models(&win.unwrap())
    });

    win.on_test({
        let win = win.as_weak();
        move || {
            let form = win.unwrap();
            let s = read_form(&form);
            form.set_busy(true);
            set_status(&form, "正在测试…", true);
            let win = win.clone();
            thread::spawn(move || {
                let started = Instant::now();
                let result = translate::try_engine("The clouds drift slowly across the summer sky.", &s);
                let seconds = started.elapsed().as_secs_f32();
                let _ = win.upgrade_in_event_loop(move |form| {
                    form.set_busy(false);
                    match result {
                        Ok(text) => set_status(&form, &format!("✓ {seconds:.1} 秒：{text}"), true),
                        Err(e) => set_status(&form, &e, false),
                    }
                });
            });
        }
    });

    win.on_save({
        let win = win.as_weak();
        let hud = hud.as_weak();
        move || {
            let form = win.unwrap();
            let s = read_form(&form);
            if let Err(e) = settings::save(&s) {
                set_status(&form, &format!("保存失败：{e}"), false);
                return;
            }
            crate::warm_engine(&s);
            *settings.write().unwrap() = s;
            let _ = form.hide();
            if let Some(hud) = hud.upgrade() {
                hud.set_note_text("".into());
            }
        }
    });

    win.on_close_window({
        let win = win.as_weak();
        move || {
            let _ = win.unwrap().hide();
        }
    });

    let drag = Rc::new(window::Drag::default());
    win.on_drag_started({
        let (win, drag) = (win.as_weak(), drag.clone());
        move || {
            if let Some(hwnd) = window::hwnd(win.unwrap().window()) {
                drag.start(hwnd);
            }
        }
    });
    win.on_dragging({
        let win = win.as_weak();
        move || {
            if let Some(hwnd) = window::hwnd(win.unwrap().window()) {
                drag.follow(hwnd);
            }
        }
    });
}

/// Shows the window just below the cloud, kept on screen, with keyboard focus.
fn open_next_to(win: &SettingsWindow, hud: &Hud) {
    let scale = hud.window().scale_factor();
    let pos = hud.window().position();
    let mut x = pos.x + ((hud.get_side() + 10.0) * scale) as i32;
    let mut y = pos.y + ((hud.get_top_margin() + hud.get_body_height() + 10.0) * scale) as i32;
    if let Some(hwnd) = window::hwnd(hud.window()) {
        let work = window::work_area(hwnd);
        // Roughly the window's largest size.
        let (w, h) = ((420.0 * scale) as i32, (520.0 * scale) as i32);
        x = x.min(work.right - w).max(work.left);
        y = y.min(work.bottom - h).max(work.top);
    }
    win.window().set_position(PhysicalPosition::new(x, y));
    let _ = win.show();
    if let Some(hwnd) = window::hwnd(win.window()) {
        window::bring_to_front(hwnd);
    }
}

fn engine(win: &SettingsWindow) -> Engine {
    ENGINES.get(win.get_engine() as usize).copied().unwrap_or_default()
}

fn set_status(win: &SettingsWindow, text: &str, ok: bool) {
    win.set_status(text.into());
    win.set_status_ok(ok);
}

fn fill_form(win: &SettingsWindow, s: &Settings) {
    win.set_engine(ENGINES.iter().position(|&e| e == s.engine).unwrap_or(0) as i32);
    win.set_api_kind(if s.api_kind == ApiKind::Claude { 0 } else { 1 });
    win.set_api_base(s.api_base.clone().into());
    win.set_api_key(s.api_key.clone().into());
    win.set_api_model(s.api_model.clone().into());
    win.set_local_base(s.local_base.clone().into());
    win.set_local_model(s.local_model.clone().into());
    win.set_models(ModelRc::default());
    win.set_status("".into());
}

fn read_form(win: &SettingsWindow) -> Settings {
    Settings {
        engine: engine(win),
        api_kind: if win.get_api_kind() == 0 { ApiKind::Claude } else { ApiKind::OpenAi },
        api_base: win.get_api_base().trim().to_string(),
        api_key: win.get_api_key().trim().to_string(),
        api_model: win.get_api_model().trim().to_string(),
        local_base: win.get_local_base().trim().to_string(),
        local_model: win.get_local_model().trim().to_string(),
    }
}

/// Asks the service in the current section which models it offers.
fn refresh_models(win: &SettingsWindow) {
    let s = read_form(win);
    if s.engine == Engine::Google {
        return;
    }
    win.set_busy(true);
    let weak = win.as_weak();
    thread::spawn(move || {
        let result = translate::list_models(&s);
        let _ = weak.upgrade_in_event_loop(move |form| {
            form.set_busy(false);
            let models = match result {
                Ok(models) => models,
                Err(e) => return set_status(&form, &e, false),
            };
            // A local model is needed to translate at all: pick the first one.
            if engine(&form) == Engine::Local && form.get_local_model().is_empty() {
                if let Some(first) = models.first() {
                    form.set_local_model(first.into());
                }
            }
            set_status(&form, &format!("找到 {} 个模型", models.len()), !models.is_empty());
            let items: Vec<SharedString> = models.into_iter().map(Into::into).collect();
            form.set_models(ModelRc::new(VecModel::from(items)));
        });
    });
}
