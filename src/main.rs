// 小白云 XiaoBaiYun: a floating cloud that translates any text you select, Chinese <-> English.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod clipboard;
mod cloud;
mod log;
mod ocr;
mod selection;
mod settings;
mod settings_window;
mod translate;
mod update;
mod window;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Image, PhysicalPosition, Timer, TimerMode};
use windows::Win32::Foundation::HWND;

use settings::{Engine, Settings};

slint::include_modules!();

/// Seed for the cloud's shape: the same seed always paints the same cloud.
const CLOUD_SEED: u32 = 7;

/// Command-line options, handy for trying things out.
struct Options {
    /// `--translate "text"`: start with that text translated.
    translate: Option<String>,
    /// `--mini`: start minimized.
    mini: bool,
    /// `--settings`: open the settings window.
    settings: bool,
    /// `--update-now`: check for a newer version and install it at once.
    update_now: bool,
    /// `--updated-from <version>`: just installed by the update (see `update`).
    updated: bool,
    /// `--self-test`: exit as soon as the window could be created; the update
    /// checks a downloaded version this way.
    self_test: bool,
}

impl Options {
    fn parse() -> Self {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let translate = args.iter().position(|a| a == "--translate").and_then(|i| args.get(i + 1)).cloned();
        Options {
            translate,
            mini: args.iter().any(|a| a == "--mini"),
            settings: args.iter().any(|a| a == "--settings"),
            update_now: args.iter().any(|a| a == "--update-now"),
            updated: args.iter().any(|a| a == "--updated-from"),
            self_test: args.iter().any(|a| a == "--self-test"),
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let options = Options::parse();
    let hud = Hud::new()?;
    if options.self_test {
        return Ok(());
    }
    let paused = Arc::new(AtomicBool::new(false));
    let settings = Arc::new(RwLock::new(settings::load()));
    log!("started, engine {:?}", settings.read().unwrap().engine);
    warm_engine(&settings.read().unwrap());

    // Keep the connection to Google fresh, so a selection never waits for a new
    // proxy/TLS handshake.
    thread::spawn(|| loop {
        translate::warm_up();
        thread::sleep(Duration::from_secs(120));
    });

    let (x, y) = window::load_position().unwrap_or_else(|| {
        let work = window::primary_work_area();
        (work.right - 900, work.top + 40)
    });
    hud.window().set_position(PhysicalPosition::new(x, y));

    // Paint the first cloud before showing the window, so it never appears bare.
    let first = cloud::render(&cloud_spec(&hud));
    show_cloud(&hud, &first);
    // Taken before `--mini` can shrink the text area, so that change gets painted.
    let first_key = cloud_key(&hud);

    wire_hud(&hud, paused.clone());
    let translator = Translator { hud: hud.as_weak(), generation: Arc::default(), settings: settings.clone() };
    selection::start(paused, {
        let translator = translator.clone();
        move |text| translator.translate(text)
    });
    let settings_window = SettingsWindow::new()?;
    settings_window::wire(&hud, &settings_window, settings);

    let selections = translator.generation.clone();
    hud.show()?;
    if let Some(text) = options.translate {
        thread::spawn(move || translator.translate(text));
    }
    hud.set_mini(options.mini);
    if options.updated {
        let weak = hud.as_weak();
        Timer::single_shot(Duration::from_millis(1500), move || {
            if let (Some(hud), Some((version, notes))) = (weak.upgrade(), update::finish()) {
                let notes: Vec<String> = notes.iter().map(|n| format!("· {n}")).collect();
                show_result(
                    &hud,
                    format!("已升级到 v{version}"),
                    &notes.join(
                        "
",
                    ),
                );
                hud.invoke_open();
            }
        });
    }
    update::start_checking();
    if options.update_now {
        update::update_now(hud.as_weak(), |status| log!("update: {status}"));
    }
    if options.settings {
        let weak = hud.as_weak();
        Timer::single_shot(Duration::from_millis(300), move || {
            if let Some(hud) = weak.upgrade() {
                hud.invoke_open_settings();
            }
        });
    }

    let ticker = Timer::default();
    ticker.start(TimerMode::Repeated, Duration::from_millis(33), tick(&hud, first.region, first_key, selections));

    slint::run_event_loop_until_quit()?;
    let pos = hud.window().position();
    window::save_position(pos.x, pos.y);
    Ok(())
}

/// The cloud's own buttons and dragging.
fn wire_hud(hud: &Hud, paused: Arc<AtomicBool>) {
    let drag = Rc::new(window::Drag::default());
    hud.on_drag_started({
        let (weak, drag) = (hud.as_weak(), drag.clone());
        move || {
            if let Some(hwnd) = window::hwnd(weak.unwrap().window()) {
                drag.start(hwnd);
            }
        }
    });
    hud.on_dragging({
        let weak = hud.as_weak();
        move || {
            if let Some(hwnd) = window::hwnd(weak.unwrap().window()) {
                drag.follow(hwnd);
            }
        }
    });

    hud.on_drag_finished({
        let weak = hud.as_weak();
        move || {
            let pos = weak.unwrap().window().position();
            window::save_position(pos.x, pos.y);
        }
    });

    hud.on_copy_result({
        let weak = hud.as_weak();
        move || {
            let hud = weak.unwrap();
            let text = hud.get_result_text();
            if text.is_empty() {
                return;
            }
            clipboard::set_text(&text);
            hud.set_copied(true);
            let weak = weak.clone();
            Timer::single_shot(Duration::from_millis(1200), move || {
                if let Some(hud) = weak.upgrade() {
                    hud.set_copied(false);
                }
            });
        }
    });

    hud.on_toggle_pause({
        let weak = hud.as_weak();
        move || {
            let now_paused = !paused.fetch_xor(true, Ordering::Relaxed);
            weak.unwrap().set_paused(now_paused);
        }
    });

    hud.on_quit(|| {
        let _ = slint::quit_event_loop();
    });
}

/// Translates captured selections in the background and shows the result on
/// the cloud. A newer selection overtakes an older one still in flight.
#[derive(Clone)]
struct Translator {
    hud: slint::Weak<Hud>,
    /// Bumped for every selection; results of older ones are dropped.
    generation: Arc<AtomicU64>,
    settings: Arc<RwLock<Settings>>,
}

impl Translator {
    fn translate(&self, text: String) {
        let id = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self.hud.upgrade_in_event_loop(|hud| {
            // A new selection opens the minimized cloud.
            hud.invoke_open();
            hud.set_busy(true);
            hud.set_error_text("".into());
        });

        let this = self.clone();
        let settings = self.settings.read().unwrap().clone();
        thread::spawn(move || {
            let is_current = || this.generation.load(Ordering::SeqCst) == id;
            // LLM engines stream: show the text as it grows, a few times a second.
            let last_shown = Cell::new(Instant::now() - Duration::from_secs(1));
            let on_partial = |partial: &str| {
                if !is_current() || last_shown.get().elapsed() < Duration::from_millis(80) {
                    return;
                }
                last_shown.set(Instant::now());
                let partial = partial.trim().to_string();
                let _ = this.hud.upgrade_in_event_loop(move |hud| show_result(&hud, partial, ""));
            };

            let started = Instant::now();
            let result = translate::translate(&text, &settings, &on_partial);
            let outcome = match &result {
                Ok(t) => t.note.clone().unwrap_or_else(|| "ok".into()),
                Err(e) => e.clone(),
            };
            log!("{:?} translation: {outcome} in {} ms", settings.engine, started.elapsed().as_millis());
            if !is_current() {
                return;
            }
            let _ = this.hud.upgrade_in_event_loop(move |hud| {
                hud.set_busy(false);
                match result {
                    Ok(t) => {
                        show_result(&hud, t.text, &t.dict.join("\n"));
                        hud.set_note_text(t.note.unwrap_or_default().into());
                    }
                    Err(e) => hud.set_error_text(format!("翻译失败 · {e}").into()),
                }
            });
        });
    }
}

/// Puts a (possibly still growing) translation on the cloud.
fn show_result(hud: &Hud, text: String, dict: &str) {
    fit_to_content(hud, &text);
    hud.set_latin(!text.chars().any(translate::is_cjk));
    hud.set_result_text(text.into());
    hud.set_dict_text(dict.into());
}

/// The cloud minimizes itself after this long without being used.
const IDLE_MINIMIZE: Duration = Duration::from_secs(30);

/// The cloud's heartbeat, about 30 times a second: advances the drift clock,
/// finishes the window setup once the native window exists, keeps the cloud on
/// screen and in front, minimizes it when idle, and repaints it (off the UI thread) whenever
/// its text area changes size. `selections` counts the selections translated.
fn tick(hud: &Hud, first_region: Vec<[i32; 4]>, first_key: (i32, i32), selections: Arc<AtomicU64>) -> impl FnMut() {
    let weak = hud.as_weak();
    let clock = Instant::now();
    let mut last_used = Instant::now();
    let mut seen_selections = 0;
    let mut ticks = 0u32;
    let mut first_region = Some(first_region);
    let mut native: Option<HWND> = None;
    let mut painted_for = first_key;
    let mut last_rect = window::Area::default();
    let paint_id = Arc::new(AtomicU64::new(0));
    move || {
        let Some(hud) = weak.upgrade() else { return };
        hud.set_t(clock.elapsed().as_secs_f32());
        let hwnd = match native {
            Some(h) => h,
            None => {
                let Some(h) = window::hwnd(hud.window()) else { return };
                window::make_floating(h);
                native = Some(h);
                h
            }
        };
        if let Some(region) = first_region.take() {
            window::set_region(hwnd, &region, hud.window().scale_factor());
        }

        // Whenever the cloud moved or grew, pull it back on screen, but not
        // while it is being dragged.
        let rect = window::window_rect(hwnd);
        if rect != last_rect && !window::left_button_down() {
            last_rect = rect;
            keep_on_screen(&hud, hwnd);
        }

        ticks = ticks.wrapping_add(1);
        if ticks.is_multiple_of(15) {
            window::stay_on_top(hwnd);
            // A downloaded update goes in while nobody uses the cloud.
            if hud.get_mini() && !hud.get_busy() {
                update::install_when_idle(&hud);
            }
        }

        // Used = the pointer is on it (hover, click, drag) or a new selection
        // came in; minimized, it waits for a click instead.
        let selected = selections.load(Ordering::Relaxed);
        if hud.get_mini() || hud.get_pointer_in() || hud.get_busy() || selected != seen_selections {
            seen_selections = selected;
            last_used = Instant::now();
        } else if last_used.elapsed() > IDLE_MINIMIZE {
            log!("idle for {} s, minimizing", IDLE_MINIMIZE.as_secs());
            hud.invoke_minimize();
            last_used = Instant::now();
        }

        let key = cloud_key(&hud);
        if key == painted_for {
            return;
        }
        painted_for = key;
        let spec = cloud_spec(&hud);
        let id = paint_id.fetch_add(1, Ordering::SeqCst) + 1;
        let paint_id = paint_id.clone();
        let weak = weak.clone();
        thread::spawn(move || {
            let layers = cloud::render(&spec);
            if paint_id.load(Ordering::SeqCst) != id {
                return;
            }
            let _ = weak.upgrade_in_event_loop(move |hud| {
                show_cloud(&hud, &layers);
                if let Some(hwnd) = window::hwnd(hud.window()) {
                    window::set_region(hwnd, &layers.region, hud.window().scale_factor());
                }
            });
        });
    }
}

/// What the painted cloud depends on: the text area's size.
fn cloud_key(hud: &Hud) -> (i32, i32) {
    (hud.get_body_width().round() as i32, hud.get_body_height().round() as i32)
}

fn cloud_spec(hud: &Hud) -> cloud::Spec {
    let (side, top) = (hud.get_side(), hud.get_top_margin());
    let (bw, bh) = (hud.get_body_width(), hud.get_body_height());
    cloud::Spec {
        width: (bw + 2.0 * side).round() as u32,
        height: (top + bh + hud.get_bottom_margin()).round() as u32,
        core: [side, top, side + bw, top + bh],
        seed: CLOUD_SEED,
    }
}

/// Hands repainted layers to the cloud, which fades them in over the old ones.
fn show_cloud(hud: &Hud, layers: &cloud::Layers) {
    hud.set_old_body_image(hud.get_body_image());
    hud.set_old_fringe_image(hud.get_fringe_image());
    hud.set_old_wisp_image(hud.get_wisp_image());
    hud.set_body_image(Image::from_rgba8(layers.body.clone()));
    hud.set_fringe_image(Image::from_rgba8(layers.fringe.clone()));
    hud.set_wisp_image(Image::from_rgba8(layers.wisps.clone()));
    hud.set_fading(true);
    hud.invoke_cloud_repainted();
    let weak = hud.as_weak();
    Timer::single_shot(Duration::from_millis(400), move || {
        if let Some(hud) = weak.upgrade() {
            hud.set_fading(false);
        }
    });
}

/// Moves the window so the cloud stays inside the screen's work area.
fn keep_on_screen(hud: &Hud, hwnd: HWND) {
    let scale = hud.window().scale_factor();
    let px = |v: f32| (v * scale) as i32;
    let rect = window::window_rect(hwnd);
    // The text area plus the billows around it (the margins beyond are mostly sky).
    let cloud = window::Area {
        left: rect.left + px(hud.get_side() - 20.0),
        top: rect.top + px(hud.get_top_margin() - 40.0),
        right: rect.left + px(hud.get_side() + hud.get_body_width() + 20.0),
        bottom: rect.top + px(hud.get_top_margin() + hud.get_body_height() + 20.0),
    };
    let work = window::work_area(hwnd);
    let dx = (work.right - cloud.right).min(0) + (work.left - cloud.left).max(0);
    let dy = (work.bottom - cloud.bottom).min(0) + (work.top - cloud.top).max(0);
    if dx != 0 || dy != 0 {
        window::move_to(hwnd, rect.left + dx, rect.top + dy);
    }
}

/// Lets the cloud widen for longer texts (it otherwise just fits the text) and
/// grow to most of the screen height.
fn fit_to_content(hud: &Hud, text: &str) {
    // Rough single-line width at 20px: Chinese characters ~21px, Latin ~10px.
    let line_px: f32 = text.chars().map(|c| if c.is_ascii() { 10.5 } else { 21.0 }).sum();
    let text_width = match line_px {
        w if w < 360.0 => 380.0,
        w if w < 380.0 * 4.0 => 460.0,
        w if w < 380.0 * 10.0 => 580.0,
        _ => 700.0,
    };
    let mut width: f32 = text_width;
    if let Some(hwnd) = window::hwnd(hud.window()) {
        let scale = hud.window().scale_factor();
        let work = window::work_area(hwnd);
        width = width.min((work.right - work.left) as f32 / scale - 2.0 * hud.get_side() - 20.0);
        hud.set_max_result_height(((work.bottom - work.top) as f32 / scale - 260.0).max(200.0));
    }
    hud.set_max_body_width(width);
}

/// A local model is loaded into memory on its first request, which takes a few
/// seconds; an online API opens its connection. Do that now, not on the first
/// selection.
fn warm_engine(s: &Settings) {
    if s.engine != Engine::Google {
        let s = s.clone();
        thread::spawn(move || {
            let _ = translate::try_engine("hi", &s);
        });
    }
}
