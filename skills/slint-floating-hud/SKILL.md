---
name: slint-floating-hud
description: Building, styling and verifying the transparent always-on-top Slint cloud window (xiaobaiyun) on Windows: window region instead of layered click-through, DPI-aware screenshots, the --translate test hook, OCR checks. Use when changing ui/hud.slint, src/window.rs, or when the cloud looks wrong or blocks clicks.
---

# Slint floating HUD on Windows

## Rules learned the hard way

- **Never add `WS_EX_LAYERED`** to the Slint window. Slint renders with OpenGL
  (femtovg); a layered window shows nothing at all. `WS_EX_TRANSPARENT` without
  layered does not pass clicks through either.
- **Click-through = window region.** `cloud::render` returns the visible rows
  (`spans`, dilated by `MOTION_X`/`MOTION_Y`) and `window::set_region` turns
  them into `SetWindowRgn`. If the layers in `hud.slint` move farther than those
  constants, raise them, or the moving edges get clipped.
- The cloud is three images painted in Rust (on all cores) and animated
  **inside** the window. Rust advances the clock `t` about 30 times a second.
  Any binding that reads `animation-tick()` makes Slint redraw at every screen
  refresh for as long as it is read (measured 14% vs 7% CPU): only read it
  behind a flag that is on for a short animation (see `fading`).
- The text layer has `cache-rendering-hint: true` and moves as one piece, so
  keep anything that changes every frame out of it.
- Put `opacity` on an `Image`, not on a container around it: a container with
  opacity is drawn off-screen first, every frame.
- Repainted layers fade in over the old ones (`CloudLayer`); images keep their
  painted size, so a stale layer is never stretched while the window resizes.
- Move windows from the cursor's screen position (`window::Drag`), not from
  pointer offsets inside the window that is moving (those lag and jitter).
- Compare CPU/GPU numbers only within one session: an RDP session and the
  local console render very differently. Measure the released exe next to the
  new build.
- Preview the painting without the app:
  `CLOUD_DUMP_DIR=<dir> cargo test --release dump_cloud -- --ignored`, then
  composite `cloud-*.rgba` (484x200 RGBA) over a sky-blue and a dark background.
- Translucency check: sample screenshot pixels over a known background; the text
  area must stay opaque (body `opacity` 1.0, text mask in `paint`).
- Slint `Text` takes a gradient `color` plus `stroke`/`stroke-style`; the
  gradient spans the element, so size the text to `min(preferred-width, max)`
  or short words get only the first colour.
- `WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW` keeps focus (and the user's selection)
  in the other app and hides the taskbar button.
- Layout inside the cloud needs `height: self.preferred-height;` on the
  VerticalLayout, otherwise it stretches to the window height and overflows.
- Fonts: the UI embeds Fredoka (English results) and ZCOOL KuaiLe (Chinese)
  via `import "fonts/...ttf"`; Rust sets `latin` to pick one. Google Fonts often
  ships only variable fonts: cut a static weight with
  `python -m fontTools.varLib.instancer <var.ttf> wght=600 wdth=100 --update-name-table -o <out.ttf>`.
  Draw icons with `Path` rather than symbol glyphs.

## Logo and icon

`assets/logo.svg` is the source. To regenerate `logo.ico` / `logo-128.png`,
render the SVG at 1024px with headless Edge
(`msedge --headless=new --default-background-color=00000000 --window-size=1024,1024 --screenshot=<png> <html with the svg>`),
then with Pillow: `im.resize((256,256)).save("logo.ico", sizes=[(16,16),(20,20),(24,24),(32,32),(40,40),(48,48),(64,64),(128,128),(256,256)])`.
`build.rs` embeds the .ico through `embed-resource` and `assets/app.rc`; both
windows set `icon:` to `logo-128.png`. In an HTML preview page, don't put SVG
inside `<template>`: the HTML parser ignores `/>` and nests the shapes.

## Staying in front

Full-screen Remote Desktop (and other always-on-top apps) are topmost too, so
the one activated last covers the cloud. `window::stay_on_top` (every 0.5 s
from `tick()`) walks the windows above the cloud (`GW_HWNDPREV`) and re-raises
it only when another app's visible, non-tool window overlaps it, so tooltips
and our own settings window can stay in front. To test it, show a `TopMost`
WinForms window over the cloud and hit-test with `WindowFromPoint`; a script
started in the background can't take the foreground, so don't test a
"foreground is topmost" condition that way.

## Idle

`tick()` minimizes the cloud after `IDLE_MINIMIZE` (30 s) with no pointer on it
(`pointer-in`), no translation running and no new selection; a new selection
opens it again (`Translator::translate`).

## Code style

`cargo fmt` (settings in `rustfmt.toml`) and `cargo clippy --release --all-targets`
must both be clean before a commit.

## Verifying visually

1. `cargo build`, then start with a known text:
   `target\debug\xiaobaiyun.exe --translate "some text"` (also `--mini`,
   `--settings`).
2. Screenshot from a **DPI-aware** process (call `SetProcessDPIAware()` first).
   Unaware screenshots silently miss the cloud on scaled displays.
3. Get the window with `(Get-Process xiaobaiyun).MainWindowHandle`, not by
   title.
4. Hit-test corners vs. body with `WindowFromPoint` + `GetAncestor(..., 2)`:
   corners must "pass through", the body must hit the cloud.
5. PowerShell 5.1 scripts that contain Chinese paths/text must be saved as
   UTF-8 **with BOM**.

`scripts/shot.ps1` does steps 1–4 and saves a PNG next to itself:

```
powershell -File skills/slint-floating-hud/scripts/shot.ps1 -Exe <repo>\target\debug\xiaobaiyun.exe -Text "hello" -Out word.png
```

## OCR check

```
OCR_GESTURE="drag x1 y1 x2 y2" cargo test reads_screen -- --ignored --nocapture
OCR_GESTURE="dbl x y" cargo test reads_screen -- --ignored --nocapture
```

Coordinates are physical pixels. OCR only reads inside the **foreground**
window's rect, so point it at the active window's text.

The highlight logic (`Shot::highlight_color`, `share_of`) is unit-tested on a
synthetic page (`keeps_only_highlighted_words`): extend that test rather than
selecting text on the user's screen.

## When a selection isn't translated

Read `%APPDATA%\xiaobaiyun\debug.log` before guessing: it logs each
gesture, the foreground app, whether Ctrl+C changed the clipboard, and the
engine's answer and time.

## Don't

- Don't simulate mouse input for tests while the user is working: the global
  hook captures their real selections and fake clicks land in their apps.
- Don't kill and relaunch the app for screenshots while the user is trying it:
  their copy disappears. Launch one copy for them at the end.
- In Git Bash, very long `python - <<'EOF'` heredocs can fail to parse; write
  the patch script to a file and run it instead.

## Updates (`src/update.rs`)

Release flow the updater depends on: the GitHub release's latest tag `vX.Y.Z`,
an asset named exactly `xiaobaiyun.exe`, and notes written as `- 标题：说明`
lines (the cloud shows the part before `：`). A new version must keep
`--self-test` (exit right after `Hud::new()`) and `--updated-from`, or every
update to it fails and is rolled back.

Test without GitHub: debug builds read `XIAOBAIYUN_UPDATE_URL` (a JSON like
the releases API). Serve a fake release with `python -m http.server <port>`
(pick a free port; some are reserved on Windows and fail with WinError 10013),
copy the debug exe to a temp folder and run it with `--update-now`. For the
failure path, serve a program that exits at once (`fn main() {}`): the exe
must come back unchanged and `%APPDATA%\xiaobaiyun\update-failed.txt` hold the
version.
