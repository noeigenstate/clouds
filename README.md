<p align="center"><img src="assets/logo.svg" width="128" alt="小白云 logo"></p>

# 小白云 · XiaoBaiYun

![小白云](docs/preview.png)

一朵漂浮在屏幕上的小白云，中英互译。在任何软件里（包括远程桌面）选中文字，译文立刻以彩色糖果字出现在云朵上。默认免费可用，也可以接入 Claude、OpenAI 兼容接口或本地模型（Ollama / LM Studio）。

A little white cloud that floats on your screen and translates whatever you
select, Chinese ⇄ English. Select text in any app, even on a remote desktop,
and the translation appears on the cloud in candy letters. Free out of the box;
plug in Claude, any OpenAI-compatible API, or a local model (Ollama / LM Studio)
when you want.

Built with Rust + [Slint](https://slint.dev). Windows 10/11.

## Download

Get `xiaobaiyun.exe` from [Releases](../../releases) and run it; no install
needed. To start it with Windows, put a shortcut to it in `shell:startup`.

## Build

```
cargo build --release
target\release\xiaobaiyun.exe
```

The `.exe` is self-contained: copy it anywhere (also onto a remote machine) and
run it.

Start-up options, handy for trying things out: `--translate "some text"`
(start with that text translated), `--mini` (start minimized), `--settings`
(open the settings window), `--update-now` (update to the latest release now).

## Use

- **Select text**: drag over it or double-click a word. Chinese becomes English,
  everything else becomes Chinese. Single words also get dictionary entries.
- **Move**: drag the cloud anywhere. Its position is remembered
  (`%APPDATA%\xiaobaiyun\position.txt`).
- **Buttons** (they appear when the pointer is on the cloud): minimize ·
  copy translation · pause/resume · close. Minimized, the cloud becomes a small
  cloud puff; it opens again when you click it or select new text. Left alone
  for 30 seconds (no pointer on it, no new selection), the cloud minimizes
  itself.
- **Right-click** the cloud to choose the translation engine (see below).
- **Updates** come by themselves: every few hours the cloud looks for a new
  release, downloads it in the background and installs it while minimized,
  then lists what's new. If the new version doesn't start, the old one is put
  back without a word and that version is skipped. The ↑ button in the
  settings window updates right away.
- The cloud is painted procedurally (`src/cloud.rs`): sunlit billows, blue-grey
  shade underneath, translucent edges and faint wisps. It bobs gently, its
  fringe and wisps drift on their own, and it rises when the pointer rests on
  it. It just fits the text, widening for long translations (very long ones
  scroll); a new size fades in smoothly.
- Text is drawn in candy colours with two embedded fonts (SIL Open Font
  License): round Fredoka for English results and ZCOOL KuaiLe (站酷快乐体) for
  Chinese ones (`ui/fonts/OFL-*.txt`).
- Clicking the cloud never takes focus away from the app you are reading.

## Translation engines

Right-click the cloud to open the settings:

| Engine | Setup | Notes |
| --- | --- | --- |
| 免费 Google (default) | none | ~0.4 s; single words also get dictionary entries |
| 在线 API → Claude | API key, model (default `claude-opus-5-5`) | streams the answer; low effort for speed |
| 在线 API → OpenAI 兼容 | base URL, key, model | OpenAI, DeepSeek, Qwen, Moonshot, … |
| 本地模型 | base URL (default Ollama `http://localhost:11434/v1`), model | LM Studio works too (`http://localhost:1234/v1`); "thinking" is turned off for speed |

「刷新」lists the models the service offers, 「测试」translates a sample sentence
and shows the time, 「保存」applies. If an API or local model fails, the
cloud falls back to Google and says so under the result. Settings live in
`%APPDATA%\xiaobaiyun\settings.json`; the API key is encrypted for your
Windows account (DPAPI), never stored in plain text.

## Remote screens

Remote Desktop, ToDesk, TeamViewer, AnyDesk, RustDesk, Sunlogin, VNC, Parsec,
Citrix, VMware, VirtualBox and others (list: `REMOTE_APPS` in
`src/selection.rs`):

1. Ctrl+C is forwarded to the remote machine; with clipboard sharing on, the text
   comes back and is translated.
2. Without clipboard sharing, the selected area is read with the Windows OCR
   engine, keeping only the words the remote app painted with its selection
   highlight. After two clipboard misses in a row, a viewer goes straight to
   OCR for 5 minutes (no 1.5 s wait, no Ctrl+C sent into the remote session).
   Install the Chinese and English OCR languages
   (Settings → Time & Language → Language → add the language, include
   "Optical character recognition").

## How it works

| File | Role |
| --- | --- |
| `ui/hud.slint` | Layout, candy text, float/drift animation of the cloud layers |
| `src/cloud.rs` | Procedural cloud painting (body, fringe, wisps) and its click region |
| `src/main.rs` | Wiring: UI callbacks, translation threads, the 30 fps heartbeat |
| `src/selection.rs` | Global mouse hook, Ctrl+C capture, remote detection |
| `src/clipboard.rs` | Read/restore clipboard text |
| `src/ocr.rs` | Screen capture + Windows OCR fallback |
| `src/translate.rs` | Language detection; Google raced with MyMemory; Claude / OpenAI-compatible / local LLMs (streaming); cache |
| `src/settings.rs` | Engine settings, DPAPI-encrypted API key |
| `ui/settings.slint`, `src/settings_window.rs` | The settings window |
| `src/log.rs` | Diagnostic log |
| `src/window.rs` | Win32: no-activate tool window, outline region, position |

Notes:

- Capturing a selection sends Ctrl+C to the active app and then puts your
  previous clipboard **text** back. Images or files on the clipboard are not
  restored.
- Consoles are skipped (Ctrl+C would interrupt them), and so are drags that
  start on an arrow/hand/resize cursor (title bars, scrollbars).
- Apps running as administrator can't receive the Ctrl+C unless this one also
  runs as administrator.
- If a selection isn't translated, `%APPDATA%\xiaobaiyun\debug.log` says
  why (which app, whether anything was copied, how the engine answered). It
  never records the selected text.
- Translation uses the free Google endpoint; if it hasn't answered in 1.2 s,
  MyMemory is asked too and the first answer wins. One connection is kept warm
  (a tiny request every 2 minutes) and recent results are cached, so a typical
  translation takes ~0.4 s and a repeat is instant. It honours
  `HTTPS_PROXY`/`HTTP_PROXY` and the Windows system proxy.

## License

MIT (see `LICENSE`). The embedded fonts keep their own SIL Open Font License:
`ui/fonts/OFL-Fredoka.txt`, `ui/fonts/OFL-ZCOOLKuaiLe.txt`.
