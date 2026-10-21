//! Reads selected text off the screen with the OCR engine built into Windows.
//! Used for remote screens whose clipboard does not reach this machine.
//!
//! The area around the drag is captured and read. Of the words found, only the
//! ones the remote app painted with its selection highlight are kept; if no
//! highlight shows up, the words between the drag's ends are taken instead.

use std::collections::HashMap;

use windows::Foundation::Rect;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, SRCCOPY,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, GetWindowRect, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use crate::log;
use crate::selection::Gesture;
use crate::translate::is_cjk;

/// A word counts as selected when this share of its box is highlight colour
/// (the rest is the letters drawn on top of it).
const HIGHLIGHTED_SHARE: f32 = 0.25;

#[derive(Clone)]
struct Word {
    text: String,
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl Word {
    fn center_x(&self) -> f32 {
        (self.left + self.right) / 2.0
    }

    /// From OCR image coordinates (scaled up by `scale`, origin at the captured
    /// area's corner) to screen pixels.
    fn into_screen(self, left: i32, top: i32, scale: f32) -> Word {
        Word {
            left: left as f32 + self.left / scale,
            top: top as f32 + self.top / scale,
            right: left as f32 + self.right / scale,
            bottom: top as f32 + self.bottom / scale,
            text: self.text,
        }
    }
}

/// How far `v` lies outside the range `lo..=hi` (0 inside it).
fn gap(v: f32, lo: f32, hi: f32) -> f32 {
    (lo - v).max(v - hi).max(0.0)
}

/// Reads the text a drag or double-click selected on the foreground window.
pub fn read_selection(gesture: Gesture) -> Option<String> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let scale = unsafe { GetDpiForWindow(GetForegroundWindow()) }.max(96) as f32 / 96.0;
    let px = |v: f32| (v * scale) as i32;

    // Generous area around the gesture; the words are filtered afterwards.
    let (left, top, right, bottom) = match gesture {
        Gesture::Drag { from, to } => {
            let multi_line = (from.y - to.y).abs() > px(14.0);
            let spread = if multi_line { px(700.0) } else { px(8.0) };
            (
                from.x.min(to.x) - spread,
                from.y.min(to.y) - px(24.0),
                from.x.max(to.x) + spread,
                from.y.max(to.y) + px(24.0),
            )
        }
        Gesture::DoubleClick { at } => (at.x - px(260.0), at.y - px(30.0), at.x + px(260.0), at.y + px(30.0)),
    };
    // Stay inside the remote viewer's window so nothing behind it is read.
    let mut viewer = RECT::default();
    unsafe {
        let _ = GetWindowRect(GetForegroundWindow(), &mut viewer);
    }
    let shot =
        Shot::grab(left.max(viewer.left), top.max(viewer.top), right.min(viewer.right), bottom.min(viewer.bottom))?;
    let lines = recognize(&shot)?;

    // Best: exactly the words the app highlighted.
    if let Some(color) = shot.highlight_color(gesture) {
        let text = join(lines.iter().flatten().filter(|w| shot.share_of(w, color) >= HIGHLIGHTED_SHARE));
        if !text.trim().is_empty() {
            log!("OCR: kept the highlighted words");
            return Some(text.trim().to_string());
        }
    }
    log!("OCR: no selection highlight found, using the drag's ends");
    let text = match gesture {
        Gesture::Drag { from, to } => pick_range(&lines, from, to),
        Gesture::DoubleClick { at } => pick_word(&lines, at),
    };
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// A captured screen area: top-down BGRA pixels, with its screen position.
struct Shot {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
    pixels: Vec<u8>,
}

impl Shot {
    /// Captures a screen rectangle, clipped to the screens.
    fn grab(left: i32, top: i32, right: i32, bottom: i32) -> Option<Shot> {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let (left, top) = (left.max(vx), top.max(vy));
        let (width, height) = (right.min(vx + vw) - left, bottom.min(vy + vh) - top);
        if width < 8 || height < 8 {
            return None;
        }
        Some(Shot { left, top, width, height, pixels: grab_screen(left, top, width, height)? })
    }

    /// The colour at a screen point, if it was captured.
    fn rgb(&self, x: i32, y: i32) -> Option<[u8; 3]> {
        let (x, y) = (x - self.left, y - self.top);
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return None;
        }
        let i = ((y * self.width + x) * 4) as usize;
        Some([self.pixels[i + 2], self.pixels[i + 1], self.pixels[i]])
    }

    /// The colour the app paints behind selected text, found along the drag:
    /// the commonest colour there that is not the page background (the
    /// commonest colour on the captured area's edge). None if no colour
    /// dominates, e.g. nothing is highlighted.
    fn highlight_color(&self, gesture: Gesture) -> Option<[u8; 3]> {
        // Colours are compared in 16 steps per channel, so anti-aliasing and
        // gradients don't split them.
        let bucket = |c: [u8; 3]| [c[0] >> 4, c[1] >> 4, c[2] >> 4];

        let mut edge: HashMap<[u8; 3], u32> = HashMap::new();
        let (l, t, r, b) = (self.left, self.top, self.left + self.width - 1, self.top + self.height - 1);
        let border = (l..=r).flat_map(|x| [(x, t), (x, b)]).chain((t..=b).flat_map(|y| [(l, y), (r, y)]));
        for (x, y) in border.step_by(2) {
            if let Some(c) = self.rgb(x, y) {
                *edge.entry(bucket(c)).or_default() += 1;
            }
        }
        let background = edge.into_iter().max_by_key(|&(_, n)| n)?.0;
        let near_background = |k: [u8; 3]| k.iter().zip(background).all(|(a, b)| a.abs_diff(b) <= 1);

        let (a, z) = match gesture {
            Gesture::Drag { from, to } => (from, to),
            Gesture::DoubleClick { at } => (POINT { x: at.x - 6, y: at.y }, POINT { x: at.x + 6, y: at.y }),
        };
        const SAMPLES: i32 = 200;
        let mut along: HashMap<[u8; 3], (u32, [u32; 3])> = HashMap::new();
        for i in 0..=SAMPLES {
            let x = a.x + (z.x - a.x) * i / SAMPLES;
            let y = a.y + (z.y - a.y) * i / SAMPLES;
            let Some(c) = self.rgb(x, y) else { continue };
            if near_background(bucket(c)) {
                continue;
            }
            let entry = along.entry(bucket(c)).or_default();
            entry.0 += 1;
            for (total, channel) in entry.1.iter_mut().zip(c) {
                *total += channel as u32;
            }
        }
        let (count, sum) = along.into_values().max_by_key(|&(n, _)| n)?;
        // Letters drawn on the highlight take part of the path; the highlight
        // itself should still be the clear majority of what isn't background.
        (count as i32 * 4 >= SAMPLES).then(|| sum.map(|s| (s / count) as u8))
    }

    /// The share of a word's box painted in `color`.
    fn share_of(&self, word: &Word, color: [u8; 3]) -> f32 {
        let (mut matching, mut total) = (0u32, 0u32);
        for y in word.top as i32..word.bottom.ceil() as i32 {
            for x in word.left as i32..word.right.ceil() as i32 {
                if let Some(c) = self.rgb(x, y) {
                    total += 1;
                    if c.iter().zip(color).all(|(a, b)| a.abs_diff(b) <= 28) {
                        matching += 1;
                    }
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            matching as f32 / total as f32
        }
    }
}

/// Words in reading order between the two drag points.
fn pick_range(lines: &[Vec<Word>], from: POINT, to: POINT) -> String {
    let (a, b) = if (from.y, from.x) <= (to.y, to.x) { (from, to) } else { (to, from) };
    let (Some(first), Some(last)) = (line_at(lines, a.y as f32), line_at(lines, b.y as f32)) else {
        return String::new();
    };
    let mut picked: Vec<&Word> = Vec::new();
    for (i, line) in lines.iter().enumerate().take(last + 1).skip(first) {
        for word in line {
            let cx = word.center_x();
            let keep = if first == last {
                cx >= a.x.min(b.x) as f32 && cx <= a.x.max(b.x) as f32
            } else if i == first {
                cx >= a.x as f32
            } else if i == last {
                cx <= b.x as f32
            } else {
                true
            };
            if keep {
                picked.push(word);
            }
        }
    }
    join(picked.into_iter())
}

/// The word under the pointer; for Chinese (one character per OCR word) the whole line.
fn pick_word(lines: &[Vec<Word>], at: POINT) -> String {
    let Some(i) = line_at(lines, at.y as f32) else { return String::new() };
    let line = &lines[i];
    let x = at.x as f32;
    let distance = |w: &Word| gap(x, w.left, w.right);
    let Some(word) = line.iter().min_by(|p, q| distance(p).total_cmp(&distance(q))) else {
        return String::new();
    };
    if word.text.chars().any(is_cjk) {
        join(line.iter())
    } else {
        word.text.trim_matches(|c: char| !c.is_alphanumeric()).to_string()
    }
}

/// The line nearest to height `y` (within 30 px).
fn line_at(lines: &[Vec<Word>], y: f32) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let top = line.iter().map(|w| w.top).fold(f32::MAX, f32::min);
            let bottom = line.iter().map(|w| w.bottom).fold(f32::MIN, f32::max);
            (i, gap(y, top, bottom))
        })
        .filter(|&(_, d)| d < 30.0)
        .min_by(|p, q| p.1.total_cmp(&q.1))
        .map(|(i, _)| i)
}

/// Joins words with spaces, except between characters that are not ASCII (Chinese).
fn join<'a>(words: impl Iterator<Item = &'a Word>) -> String {
    let mut out = String::new();
    for word in words {
        let text = word.text.trim();
        if let (Some(prev), Some(next)) = (out.chars().last(), text.chars().next()) {
            if prev.is_ascii() || next.is_ascii() {
                out.push(' ');
            }
        }
        out.push_str(text);
    }
    out
}

/// OCRs a captured area and returns its words line by line, in screen pixels.
fn recognize(shot: &Shot) -> Option<Vec<Vec<Word>>> {
    let (left, top, w, h) = (shot.left, shot.top, shot.width, shot.height);
    // Windows OCR misses small text; doubling the image helps a lot.
    let max = OcrEngine::MaxImageDimension().ok()? as i32;
    let up = if w * 2 <= max && h * 2 <= max { 2 } else { 1 };
    let (iw, ih) = (w * up, h * up);
    let pixels = if up == 1 { shot.pixels.clone() } else { upscale(&shot.pixels, w as usize, h as usize, up as usize) };

    let buffer = CryptographicBuffer::CreateFromByteArray(&pixels).ok()?;
    let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        iw,
        ih,
        BitmapAlphaMode::Premultiplied,
    )
    .ok()?;

    let lines = match (run_engine("zh", &bitmap), run_engine("en", &bitmap)) {
        (Some(zh), Some(en)) => prefer_english_lines(zh, &en),
        (zh, en) => zh.or(en)?,
    };
    let scale = up as f32;
    Some(lines.into_iter().map(|line| line.into_iter().map(|w| w.into_screen(left, top, scale)).collect()).collect())
}

/// The Chinese engine also reads Latin text, but splits English words ("yo u"):
/// lines that are mostly Latin are taken from the English engine's reading of
/// the same height instead.
fn prefer_english_lines(zh: Vec<Vec<Word>>, en: &[Vec<Word>]) -> Vec<Vec<Word>> {
    zh.into_iter()
        .map(|line| {
            let chars: Vec<char> = line.iter().flat_map(|w| w.text.chars()).collect();
            let cjk = chars.iter().filter(|&&c| is_cjk(c)).count();
            if cjk * 3 >= chars.len() {
                return line;
            }
            let (top, bottom) = (line[0].top, line[0].bottom);
            let overlap = |l: &Vec<Word>| l[0].bottom.min(bottom) - l[0].top.max(top);
            en.iter()
                .filter(|l| overlap(l) > 0.0)
                .max_by(|a, b| overlap(a).total_cmp(&overlap(b)))
                .cloned()
                .unwrap_or(line)
        })
        .collect()
}

/// Runs the installed OCR engine whose language tag starts with `lang_prefix`
/// ("zh", "en"); its lines come back top to bottom, in image coordinates.
fn run_engine(lang_prefix: &str, bitmap: &SoftwareBitmap) -> Option<Vec<Vec<Word>>> {
    let language: Language = OcrEngine::AvailableRecognizerLanguages()
        .ok()?
        .into_iter()
        .find(|l| l.LanguageTag().is_ok_and(|t| t.to_string().to_lowercase().starts_with(lang_prefix)))?;
    let engine = OcrEngine::TryCreateFromLanguage(&language).ok()?;
    let result = engine.RecognizeAsync(bitmap).ok()?.join().ok()?;
    let mut lines = Vec::new();
    for line in result.Lines().ok()? {
        let mut words = Vec::new();
        for word in line.Words().ok()? {
            let Rect { X, Y, Width, Height } = word.BoundingRect().ok()?;
            words.push(Word {
                text: word.Text().ok()?.to_string(),
                left: X,
                top: Y,
                right: X + Width,
                bottom: Y + Height,
            });
        }
        if !words.is_empty() {
            lines.push(words);
        }
    }
    lines.sort_by(|a, b| a[0].top.total_cmp(&b[0].top));
    Some(lines)
}

/// Copies a screen rectangle as top-down BGRA pixels.
fn grab_screen(left: i32, top: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    unsafe {
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let result = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0).ok().and_then(|dib| {
            let old = SelectObject(mem, dib.into());
            let copied = BitBlt(mem, 0, 0, w, h, Some(screen), left, top, SRCCOPY | CAPTUREBLT).is_ok();
            let pixels = copied.then(|| {
                let mut data = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize).to_vec();
                // The screen has no alpha channel: make every pixel opaque.
                for px in data.chunks_exact_mut(4) {
                    px[3] = 255;
                }
                data
            });
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            pixels
        });
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        result
    }
}

/// Enlarges a BGRA image `k` times by repeating pixels.
fn upscale(src: &[u8], w: usize, h: usize, k: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * k * h * k * 4];
    for y in 0..h * k {
        for x in 0..w * k {
            let s = ((y / k) * w + x / k) * 4;
            let d = (y * w * k + x) * 4;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::HiDpi::{SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};

    /// A white page with a blue selection over the first two of three words,
    /// each word's letters drawn as dark strokes.
    fn page() -> (Shot, Vec<Word>) {
        let (w, h) = (300, 40);
        let mut pixels = vec![255u8; (w * h * 4) as usize];
        let mut paint = |x0: i32, x1: i32, y0: i32, y1: i32, bgr: [u8; 3]| {
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = ((y * w + x) * 4) as usize;
                    pixels[i..i + 3].copy_from_slice(&bgr);
                }
            }
        };
        paint(10, 190, 10, 30, [0xD7, 0x78, 0x00]); // selection: #0078D7
        let words: Vec<Word> = [("hello", 20), ("cloud", 110), ("sky", 210)]
            .iter()
            .map(|&(text, x)| Word {
                text: text.into(),
                left: x as f32,
                top: 12.0,
                right: x as f32 + 60.0,
                bottom: 28.0,
            })
            .collect();
        for word in &words {
            // Vertical letter strokes, a quarter of the word's box.
            for x in (word.left as i32..word.right as i32).step_by(8) {
                paint(x, x + 2, 12, 28, [0x20, 0x20, 0x20]);
            }
        }
        (Shot { left: 0, top: 0, width: w, height: h, pixels }, words)
    }

    #[test]
    fn keeps_only_highlighted_words() {
        let (shot, words) = page();
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        let color = shot.highlight_color(drag).expect("highlight found");
        assert_eq!(color, [0x00, 0x78, 0xD7]);
        let kept: Vec<&str> =
            words.iter().filter(|w| shot.share_of(w, color) >= HIGHLIGHTED_SHARE).map(|w| w.text.as_str()).collect();
        assert_eq!(kept, ["hello", "cloud"]);
    }

    #[test]
    fn no_highlight_on_a_plain_page() {
        let (mut shot, _) = page();
        shot.pixels.iter_mut().for_each(|p| *p = 255);
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert_eq!(shot.highlight_color(drag), None);
    }

    /// Manual check against whatever is on screen, in physical pixels:
    /// OCR_GESTURE="drag x1 y1 x2 y2" or "dbl x y" cargo test reads_screen -- --ignored --nocapture
    #[test]
    #[ignore]
    fn reads_screen() {
        unsafe {
            let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        let spec = std::env::var("OCR_GESTURE").expect("set OCR_GESTURE");
        let parts: Vec<&str> = spec.split_whitespace().collect();
        let n: Vec<i32> = parts[1..].iter().map(|v| v.parse().unwrap()).collect();
        let gesture = match parts[0] {
            "drag" => Gesture::Drag { from: POINT { x: n[0], y: n[1] }, to: POINT { x: n[2], y: n[3] } },
            _ => Gesture::DoubleClick { at: POINT { x: n[0], y: n[1] } },
        };
        println!("OCR: {:?}", read_selection(gesture));
    }
}
