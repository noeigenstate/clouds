//! Silent updates from the GitHub releases.
//!
//! A newer release is downloaded in the background, checked (it must start and
//! exit with `--self-test`), and installed once the cloud is idle: the running
//! exe is renamed aside, the new one takes its name and is started with
//! `--updated-from <version>`. When the new version reports that it runs, the
//! old process exits; the new one cleans up and shows what changed. If it
//! doesn't report in time, the old exe is put back, nothing is shown, and that
//! version is never installed automatically again.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use slint::ComponentHandle;

use crate::settings::data_file;
use crate::{log, translate, Hud};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST: &str = "https://api.github.com/repos/noeigenstate/clouds/releases/latest";
const ASSET: &str = "xiaobaiyun.exe";
/// The new version creates this file once it runs.
const STARTED: &str = "update-started";
/// Version and notes of the release being installed, for the new version to show.
const NOTES: &str = "update-notes.txt";
/// A version that failed to install, not to be tried again automatically.
const FAILED: &str = "update-failed.txt";

pub struct Release {
    pub version: String,
    /// One line per change: the headline of each item in the release notes.
    pub notes: Vec<String>,
    url: String,
    size: u64,
}

/// A release downloaded and checked, waiting to be installed.
pub struct Update {
    release: Release,
    exe: PathBuf,
}

/// The latest release, if it is newer than this version.
pub fn check() -> Result<Option<Release>, String> {
    let url = std::env::var("XIAOBAIYUN_UPDATE_URL").ok().filter(|_| cfg!(debug_assertions));
    let response = agent()
        .get(url.as_deref().unwrap_or(LATEST))
        .header("User-Agent", "xiaobaiyun")
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("连接失败 {e}"))?;
    let data = translate::read_json(response, "GitHub")?;
    let version = data["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
    if !newer(&version, VERSION) {
        return Ok(None);
    }
    let asset = data["assets"]
        .as_array()
        .and_then(|assets| assets.iter().find(|a| a["name"] == ASSET))
        .ok_or("新版本没有可下载的程序")?;
    Ok(Some(Release {
        version,
        notes: headlines(data["body"].as_str().unwrap_or_default()),
        url: asset["browser_download_url"].as_str().unwrap_or_default().to_string(),
        size: asset["size"].as_u64().unwrap_or_default(),
    }))
}

/// Downloads a release next to the running exe and makes sure it starts.
pub fn download(release: Release) -> Result<Update, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("xiaobaiyun-update.exe");
    let mut response =
        agent().get(&release.url).header("User-Agent", "xiaobaiyun").call().map_err(|e| format!("下载失败 {e}"))?;
    translate::check_status(&mut response, "下载")?;
    let written = fs::File::create(&exe)
        .and_then(|mut file| std::io::copy(&mut response.body_mut().as_reader(), &mut file))
        .map_err(|e| format!("下载失败 {e}"))?;
    if written != release.size || !self_test(&exe) {
        let _ = fs::remove_file(&exe);
        return Err(format!("v{} 下载的程序无法运行", release.version));
    }
    Ok(Update { release, exe })
}

/// Whether the new exe starts and exits cleanly with `--self-test`.
fn self_test(exe: &Path) -> bool {
    let Ok(mut child) = Command::new(exe).arg("--self-test").spawn() else { return false };
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if let Ok(Some(status)) = child.try_wait() {
            return status.success();
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

/// Every few hours, downloads a newer release (unless it failed before) for
/// `install_when_idle` to install.
pub fn start_checking() {
    if cfg!(debug_assertions) {
        return;
    }
    thread::spawn(|| {
        thread::sleep(Duration::from_secs(60));
        loop {
            match check() {
                Ok(Some(release)) if failed_version().as_deref() != Some(release.version.as_str()) => {
                    log!("update: downloading v{}", release.version);
                    match download(release) {
                        Ok(update) => *ready().lock().unwrap() = Some(update),
                        Err(e) => log!("update: {e}"),
                    }
                }
                Ok(_) => {}
                Err(e) => log!("update: {e}"),
            }
            thread::sleep(Duration::from_secs(6 * 3600));
        }
    });
}

/// The upgrade button: checks now and installs a newer version right away,
/// even one that failed before. `report` gets the progress, from a worker
/// thread; an empty message means the new version is being started.
pub fn update_now(hud: slint::Weak<Hud>, report: impl Fn(String) + Send + 'static) {
    thread::spawn(move || match check() {
        Ok(None) => report(format!("已是最新版本 v{VERSION}")),
        Ok(Some(release)) => {
            report(format!("正在下载 v{}…", release.version));
            match download(release) {
                Ok(update) => {
                    report(String::new());
                    let _ = hud.upgrade_in_event_loop(move |hud| install(&hud, update));
                }
                Err(e) => report(e),
            }
        }
        Err(e) => report(format!("检查更新失败：{e}")),
    });
}

fn ready() -> &'static Mutex<Option<Update>> {
    static READY: OnceLock<Mutex<Option<Update>>> = OnceLock::new();
    READY.get_or_init(Default::default)
}

/// Installs a downloaded update if there is one; call it while the cloud is idle.
pub fn install_when_idle(hud: &Hud) {
    if let Some(update) = ready().lock().unwrap().take() {
        install(hud, update);
    }
}

/// Swaps in the new exe and starts it, hiding the cloud meanwhile. Exits once
/// the new version runs; otherwise puts everything back as it was.
pub fn install(hud: &Hud, update: Update) {
    static INSTALLING: AtomicBool = AtomicBool::new(false);
    let (Some(started), Some(notes)) = (data_file(STARTED), data_file(NOTES)) else { return };
    if INSTALLING.swap(true, Ordering::SeqCst) {
        return;
    }
    let version = update.release.version.clone();
    log!("update: installing v{version}");
    let _ = fs::remove_file(&started);
    let _ = fs::write(&notes, format!("{version}\n{}", update.release.notes.join("\n")));
    let child = std::env::current_exe().map_err(|e| e.to_string()).and_then(|exe| {
        swap(&exe, &update.exe)?;
        Command::new(&exe).args(["--updated-from", VERSION]).spawn().map_err(|e| {
            roll_back(&exe);
            e.to_string()
        })
    });
    let child = match child {
        Ok(child) => child,
        Err(e) => {
            log!("update: {e}");
            give_up(&version);
            INSTALLING.store(false, Ordering::SeqCst);
            return;
        }
    };
    let _ = hud.hide();
    let weak = hud.as_weak();
    thread::spawn(move || {
        let ok = wait_until_started(child, &started);
        if !ok {
            if let Ok(exe) = std::env::current_exe() {
                roll_back(&exe);
            }
            give_up(&version);
        }
        let _ = weak.upgrade_in_event_loop(move |hud| {
            if ok {
                let _ = slint::quit_event_loop();
            } else {
                let _ = hud.show();
                INSTALLING.store(false, Ordering::SeqCst);
            }
        });
    });
}

/// Puts `new` in place of `exe`, keeping the old one as `exe.old`.
fn swap(exe: &Path, new: &Path) -> Result<(), String> {
    let old = exe.with_extension("old");
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).map_err(|e| e.to_string())?;
    fs::rename(new, exe).map_err(|e| {
        let _ = fs::rename(&old, exe);
        e.to_string()
    })
}

/// Puts `exe.old` back in place of `exe`. The stopped new version may hold its
/// file for a moment, so this retries.
fn roll_back(exe: &Path) {
    let old = exe.with_extension("old");
    for _ in 0..25 {
        let _ = fs::remove_file(exe);
        if fs::rename(&old, exe).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
    log!("update: could not put the old version back");
}

/// Waits up to 30 s for the new version to create `started`; stops it if it
/// exits or takes too long.
fn wait_until_started(mut child: Child, started: &Path) -> bool {
    let since = Instant::now();
    while since.elapsed() < Duration::from_secs(30) {
        if started.exists() {
            return true;
        }
        if let Ok(Some(_)) = child.try_wait() {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    false
}

fn give_up(version: &str) {
    log!("update: v{version} failed, staying on v{VERSION}");
    if let Some(file) = data_file(FAILED) {
        let _ = fs::write(file, version);
    }
}

fn failed_version() -> Option<String> {
    fs::read_to_string(data_file(FAILED)?).ok().map(|v| v.trim().to_string())
}

/// Run by the new version once it is up: lets the old one exit, removes the
/// old exe after it, and returns the version and notes to show.
pub fn finish() -> Option<(String, Vec<String>)> {
    let _ = fs::write(data_file(STARTED)?, "");
    log!("update: now running v{VERSION}");
    if let Ok(exe) = std::env::current_exe() {
        thread::spawn(move || {
            let old = exe.with_extension("old");
            for _ in 0..60 {
                if fs::remove_file(&old).is_ok() || !old.exists() {
                    return;
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    }
    let notes = data_file(NOTES)?;
    let text = fs::read_to_string(&notes).ok()?;
    let _ = fs::remove_file(notes);
    let mut lines = text.lines().map(str::to_string);
    let version = lines.next()?;
    Some((version, lines.collect()))
}

/// Whether version `a` ("0.1.10") is newer than `b` ("0.1.9").
fn newer(a: &str, b: &str) -> bool {
    let parts = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    !a.is_empty() && parts(a) > parts(b)
}

/// The headline of each "- " item in the release notes: the part before "：".
fn headlines(body: &str) -> Vec<String> {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix("- "))
        .map(|item| item.split('：').next().unwrap_or(item).trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Downloads can take a while; they go through the proxy like the translations.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| translate::build_agent(Duration::from_secs(600), true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(newer("0.1.4", "0.1.3"));
        assert!(newer("0.1.10", "0.1.9"));
        assert!(newer("1.0.0", "0.9.9"));
        assert!(!newer("0.1.3", "0.1.3"));
        assert!(!newer("0.1.2", "0.1.3"));
        assert!(!newer("", "0.1.3"));
    }

    #[test]
    fn reads_headlines() {
        let body = "## 小白云 v0.1.3\n\n- 全屏远程桌面不再盖住云朵：被盖住时会回到最上面。\n- 新 Logo：彩虹托着小白云。\n\n下载 `xiaobaiyun.exe` 直接运行即可。";
        assert_eq!(headlines(body), ["全屏远程桌面不再盖住云朵", "新 Logo"]);
    }

    #[test]
    fn swaps_and_rolls_back() {
        let dir = std::env::temp_dir().join(format!("xiaobaiyun-update-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (exe, new) = (dir.join("app.exe"), dir.join("app-update.exe"));
        fs::write(&exe, "old").unwrap();
        fs::write(&new, "new").unwrap();
        swap(&exe, &new).unwrap();
        assert_eq!(fs::read_to_string(&exe).unwrap(), "new");
        assert_eq!(fs::read_to_string(exe.with_extension("old")).unwrap(), "old");
        roll_back(&exe);
        assert_eq!(fs::read_to_string(&exe).unwrap(), "old");
        assert!(!exe.with_extension("old").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_new_version_that_exits_is_not_accepted() {
        let marker = std::env::temp_dir().join(format!("xiaobaiyun-never-{}", std::process::id()));
        let child = Command::new("cmd").args(["/c", "exit 1"]).spawn().unwrap();
        assert!(!wait_until_started(child, &marker));
    }
}
