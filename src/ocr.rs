//! Reads selected text off the screen with the OCR engine built into Windows.
//! Used for remote screens whose clipboard does not reach this machine.
//!
//! The area around the drag is captured and read. Of the words found, only the
//! ones inside the gesture that the remote app painted with its selection
//! highlight are kept. No highlight means nothing was selected.

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use windows::Foundation::Rect;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Security::Cryptography::CryptographicBuffer;
use windows::Win32::Foundation::{HWND, POINT, RECT};
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
/// Minimum difference from the page in at least one colour channel.
const HIGHLIGHT_CONTRAST: u8 = 40;
/// Allows anti-aliasing and the colour shifts introduced by remote codecs.
const COLOR_TOLERANCE: u8 = 28;
/// One flat patch can justify waiting; recognition needs a more settled colour.
const HIGHLIGHT_PATCHES: u32 = 3;
/// About one letter at normal text sizes, in logical pixels.
const LETTER_WIDTH: f32 = 12.0;
/// Whole-line fill may reach the crop's edge only when its text spans this share.
const WIDE_LINE_SHARE: f32 = 0.6;
/// Remote viewers may paint the selection several frames after button-up.
const HIGHLIGHT_WAIT: Duration = Duration::from_millis(400);
const HIGHLIGHT_POLL: Duration = Duration::from_millis(50);
/// A queued button-down must not capture an already updated remote screen.
const BASELINE_DELAY: Duration = Duration::from_millis(50);
/// Bounds the pairwise colour merge even on a busy, multicoloured desktop.
const COLOR_BUCKETS: usize = 64;
/// Also covers the wide first/last-line search used by a multi-line drag.
const CAPTURE_SPREAD: f32 = 700.0;
const CLICK_HEIGHT: f32 = 30.0;

/// A band around button-down, retained only until its release.
pub struct Baseline {
    shot: Shot,
    window: isize,
    ready: Instant,
}

impl Baseline {
    /// Runs on the selection worker, never on the low-level hook thread.
    pub fn capture(window: HWND, point: POINT, pressed: Instant) -> Option<Self> {
        if pressed.elapsed() > BASELINE_DELAY {
            return None;
        }
        let mut rect = RECT::default();
        unsafe { GetWindowRect(window, &mut rect).ok()? };
        let scale = unsafe { GetDpiForWindow(window) }.max(96) as f32 / 96.0;
        let rect = baseline_area(rect, point, scale);
        let shot = Shot::grab(rect.left, rect.top, rect.right, rect.bottom, false)?;
        let ready = Instant::now();
        (ready.duration_since(pressed) <= BASELINE_DELAY).then_some(Self { shot, window: window.0 as isize, ready })
    }

    pub fn ready_by(&self, released: Instant) -> bool {
        self.ready <= released
    }

    #[cfg(test)]
    pub fn for_test(ready: Instant) -> Self {
        Self { shot: Shot { left: 0, top: 0, width: 0, height: 0, pixels: Vec::new() }, window: 0, ready }
    }
}

fn baseline_area(viewer: RECT, at: POINT, scale: f32) -> RECT {
    let (spread, height) = ((CAPTURE_SPREAD * scale) as i32, (CLICK_HEIGHT * scale) as i32);
    RECT {
        left: viewer.left.max(at.x - spread),
        top: viewer.top.max(at.y - height),
        right: viewer.right.min(at.x + spread),
        bottom: viewer.bottom.min(at.y + height),
    }
}

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
pub fn read_selection(gesture: Gesture, baseline: Option<&Baseline>) -> Option<String> {
    let window = unsafe { GetForegroundWindow() };
    let scale = unsafe { GetDpiForWindow(window) }.max(96) as f32 / 96.0;
    // Stay inside the remote viewer's window so nothing behind it is read.
    let mut viewer = RECT::default();
    unsafe {
        GetWindowRect(window, &mut viewer).ok()?;
    }
    let baseline = baseline.filter(|b| b.window == window.0 as isize).map(|b| &b.shot);
    let RECT { left, top, right, bottom } = selection_area(gesture, scale, baseline);
    let mut next = Instant::now();
    let deadline = next + HIGHLIGHT_WAIT;
    let shots = std::iter::from_fn(|| {
        if next > deadline || Instant::now() > deadline {
            return None;
        }
        thread::sleep(next.saturating_duration_since(Instant::now()));
        next += HIGHLIGHT_POLL;
        // Stop if the user has switched out of the viewer during the wait.
        if unsafe { GetForegroundWindow() } != window {
            return None;
        }
        Some(Shot::grab(
            left.max(viewer.left),
            top.max(viewer.top),
            right.min(viewer.right),
            bottom.min(viewer.bottom),
            true,
        ))
    });
    let Some(shot) = highlighted_shot(shots, gesture, scale) else {
        log!("OCR: no selection highlight, nothing selected");
        return None;
    };
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    let lines = recognize(&shot)?;

    let text = pick_selection(&shot, &lines, gesture, scale, baseline);
    if text.is_some() {
        log!("OCR: kept the highlighted words");
    } else {
        log!("OCR: no selection highlight, nothing selected");
    }
    text
}

/// Generous area around the gesture; the words are filtered afterwards.
fn selection_area(gesture: Gesture, scale: f32, baseline: Option<&Shot>) -> RECT {
    let px = |v: f32| (v * scale) as i32;
    let mut area = match gesture {
        Gesture::Drag { from, to } => {
            let multi_line = (from.y - to.y).abs() > px(14.0);
            let spread = if multi_line { px(CAPTURE_SPREAD) } else { px(LETTER_WIDTH) };
            RECT {
                left: from.x.min(to.x) - spread,
                top: from.y.min(to.y) - px(24.0),
                right: from.x.max(to.x) + spread,
                bottom: from.y.max(to.y) + px(24.0),
            }
        }
        Gesture::DoubleClick { at } => RECT {
            left: at.x - px(260.0),
            top: at.y - px(CLICK_HEIGHT),
            right: at.x + px(260.0),
            bottom: at.y + px(CLICK_HEIGHT),
        },
    };
    // A narrower drag must still show the old selection in the press band.
    if let (Gesture::Drag { .. }, Some(before)) = (gesture, baseline) {
        area.left = area.left.min(before.left);
        area.top = area.top.min(before.top);
        area.right = area.right.max(before.left + before.width);
        area.bottom = area.bottom.max(before.top + before.height);
    }
    area
}

/// The retry decision is independent of the clock, capture API and OCR engines.
fn highlighted_shot(shots: impl Iterator<Item = Option<Shot>>, gesture: Gesture, scale: f32) -> Option<Shot> {
    for (i, shot) in shots.flatten().enumerate() {
        let Some(background) = shot.background_color(scale) else { continue };
        let mut candidate = false;
        for (patches, color) in shot.highlight_candidates(gesture, scale) {
            let ends = match gesture {
                Gesture::Drag { from, to } => {
                    let forward =
                        if (from.y - to.y).abs() <= (14.0 * scale) as i32 { from.x <= to.x } else { from.y < to.y };
                    let direction = if forward { 1 } else { -1 };
                    (
                        shot.covers_pixel_end(from, direction, color, scale, background),
                        shot.covers_pixel_end(to, -direction, color, scale, background),
                    )
                }
                Gesture::DoubleClick { at } => {
                    let near = shot.covers_pixel_end(at, -1, color, scale, background)
                        || shot.covers_pixel_end(at, 1, color, scale, background);
                    (near, near)
                }
            };
            if patches >= HIGHLIGHT_PATCHES && ends.0 && ends.1 {
                return Some(shot);
            }
            candidate |= ends.0 || ends.1;
        }
        if i == 0 && !candidate && matches!(gesture, Gesture::Drag { .. }) {
            return None;
        }
    }
    None
}

/// Keeps highlighted words within the gesture's reading-order range.
fn pick_selection(
    shot: &Shot,
    lines: &[Vec<Word>],
    gesture: Gesture,
    scale: f32,
    baseline: Option<&Shot>,
) -> Option<String> {
    for color in shot.highlight_colors(gesture, scale) {
        let words = match gesture {
            Gesture::Drag { from, to } => {
                let (Some(first), Some(last)) =
                    (line_at(lines, from.y as f32, scale), line_at(lines, to.y as f32, scale))
                else {
                    continue;
                };
                let forward = if first == last { from.x <= to.x } else { first < last };
                let direction = if forward { 1.0 } else { -1.0 };
                if !shot.covers_end(&lines[first], from, direction, color, scale)
                    || !shot.covers_end(&lines[last], to, -direction, color, scale)
                {
                    continue;
                }
                pick_range(lines, from, to, scale)
            }
            Gesture::DoubleClick { at } => {
                let Some(words) = clicked_run(shot, lines, at, color, scale) else { continue };
                words
            }
        };
        let words: Vec<_> = words.into_iter().filter(|w| shot.share_of(w, color) >= HIGHLIGHTED_SHARE).collect();
        if baseline.is_some_and(|b| {
            words.iter().all(|w| b.contains(w) && b.share_of(w, color) >= HIGHLIGHTED_SHARE)
                && !shot.highlight_shrank(b, color)
        }) {
            continue;
        }
        let text = join(words.into_iter());
        let text = text.trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
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
    fn grab(left: i32, top: i32, right: i32, bottom: i32, layered: bool) -> Option<Shot> {
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
        Some(Shot { left, top, width, height, pixels: grab_screen(left, top, width, height, layered)? })
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

    /// Plausible selection colours on the endpoints' lines. OCR later checks
    /// their nearest words; empty margins need not be highlight-coloured.
    fn highlight_colors(&self, gesture: Gesture, scale: f32) -> Vec<[u8; 3]> {
        self.highlight_candidates(gesture, scale)
            .into_iter()
            .filter(|&(count, _)| count >= HIGHLIGHT_PATCHES)
            .map(|(_, color)| color)
            .collect()
    }

    fn highlight_candidates(&self, gesture: Gesture, scale: f32) -> Vec<(u32, [u8; 3])> {
        let (l, r) = (self.left, self.left + self.width - 1);
        let Some(background) = self.background_color(scale) else { return Vec::new() };
        let contrasts = |c: [u8; 3]| c.iter().zip(background).any(|(a, b)| a.abs_diff(b) >= HIGHLIGHT_CONTRAST);

        let letter = (LETTER_WIDTH * scale).ceil() as i32;
        let bands = match gesture {
            Gesture::Drag { from, to } if (from.y - to.y).abs() > (14.0 * scale) as i32 => {
                let (a, z) = if from.y < to.y { (from, to) } else { (to, from) };
                vec![(a.x - letter, r, a.y), (l, z.x + letter, z.y)]
            }
            Gesture::Drag { from, to } => {
                vec![(from.x.min(to.x) - letter, from.x.max(to.x) + letter, (from.y + to.y) / 2)]
            }
            Gesture::DoubleClick { at } => vec![(at.x - letter, at.x + letter, at.y)],
        };
        let step = (3.0 * scale).ceil() as usize;
        let mut samples = Vec::new();
        for (left, right, y) in bands {
            let (left, right) = (left.max(l), right.min(r));
            let horizontal_step = step.max(((right - left).max(0) / 200) as usize);
            for y in (y - letter..=y + letter).step_by(step) {
                for x in (left..=right).step_by(horizontal_step) {
                    let Some(color) = self.rgb(x, y) else { continue };
                    if contrasts(color) && self.flat_patch(x, y, color, scale) {
                        samples.push(color);
                    }
                }
            }
        }
        common_colors(samples.into_iter()).into_iter().filter(|&(_, color)| contrasts(color)).collect()
    }

    fn background_color(&self, scale: f32) -> Option<[u8; 3]> {
        let (l, t, r, b) = (self.left, self.top, self.left + self.width - 1, self.top + self.height - 1);
        let border = (l..=r).flat_map(|x| [(x, t), (x, b)]).chain((t..=b).flat_map(|y| [(l, y), (r, y)]));
        let edge = border.step_by((2.0 * scale).ceil() as usize).filter_map(|(x, y)| self.rgb(x, y));
        common_colors(edge).first().map(|&(_, color)| color)
    }

    /// Flat background patches distinguish a highlight from thin letter strokes.
    fn flat_patch(&self, x: i32, y: i32, color: [u8; 3], scale: f32) -> bool {
        let size = (3.0 * scale).ceil() as i32;
        let mut matching = 0;
        for y in y - size / 2..y - size / 2 + size {
            for x in x - size / 2..x - size / 2 + size {
                let Some(c) = self.rgb(x, y) else { return false };
                matching += u32::from(similar(c, color));
            }
        }
        matching * 5 >= (size * size * 4) as u32
    }

    /// Find the nearest painted content inward from an end, skipping an empty
    /// margin. Its edge must have highlight within one letter, before OCR runs.
    fn covers_pixel_end(&self, at: POINT, direction: i32, color: [u8; 3], scale: f32, background: [u8; 3]) -> bool {
        let letter = (LETTER_WIDTH * scale).ceil() as i32;
        let step = scale.ceil() as usize;
        let top = (at.y - letter).max(self.top);
        let bottom = (at.y + letter).min(self.top + self.height - 1);
        let mut x = at.x.clamp(self.left, self.left + self.width - 1);
        while x >= self.left && x < self.left + self.width {
            if (top..=bottom).step_by(step).any(|y| {
                self.rgb(x, y)
                    .is_some_and(|c| c.iter().zip(background).any(|(a, b)| a.abs_diff(b) >= HIGHLIGHT_CONTRAST))
            }) {
                return (0..=letter).step_by(step).any(|d| {
                    (top..=bottom).step_by(step).any(|y| self.flat_patch(x + d * direction, y, color, scale))
                });
            }
            x += direction * step as i32;
        }
        false
    }

    /// Look inward by a letter, or at the near edge of the nearest OCR word.
    /// This permits a margin start or an overshoot without accepting a panel
    /// that only crosses the middle of the gesture.
    fn covers_end(&self, line: &[Word], at: POINT, direction: f32, color: [u8; 3], scale: f32) -> bool {
        let distance = |w: &Word| gap(at.x as f32, w.left, w.right);
        let Some(word) = line.iter().min_by(|a, b| distance(a).total_cmp(&distance(b))) else { return false };
        let letter = LETTER_WIDTH * scale;
        let x = at.x as f32;
        let inward = x + letter * direction;
        let mut probe = word.clone();
        probe.left = x.min(inward);
        probe.right = x.max(inward);
        if self.share_of(&probe, color) >= HIGHLIGHTED_SHARE {
            return true;
        }
        if self.share_of(word, color) < HIGHLIGHTED_SHARE {
            return false;
        }
        if direction > 0.0 {
            probe.left = word.left;
            probe.right = word.right.min(word.left + letter);
        } else {
            probe.left = word.left.max(word.right - letter);
            probe.right = word.right;
        }
        self.share_of(&probe, color) >= HIGHLIGHTED_SHARE
    }

    fn contains(&self, word: &Word) -> bool {
        word.left >= self.left as f32
            && word.top >= self.top as f32
            && word.right <= (self.left + self.width) as f32
            && word.bottom <= (self.top + self.height) as f32
    }

    /// Compare the press band by filled columns, so moving letter strokes do not
    /// look like a smaller selection on an unchanged title bar or solid block.
    fn highlight_shrank(&self, before: &Shot, color: [u8; 3]) -> bool {
        let (left, top) = (self.left.max(before.left), self.top.max(before.top));
        let (right, bottom) = (
            (self.left + self.width).min(before.left + before.width),
            (self.top + self.height).min(before.top + before.height),
        );
        let columns = |shot: &Shot| {
            (left..right).filter(|&x| (top..bottom).any(|y| shot.rgb(x, y).is_some_and(|c| similar(c, color)))).count()
        };
        columns(self) < columns(before)
    }

    /// The share of a word's box painted in `color`.
    fn share_of(&self, word: &Word, color: [u8; 3]) -> f32 {
        let (mut matching, mut total) = (0u32, 0u32);
        for y in word.top as i32..word.bottom.ceil() as i32 {
            for x in word.left as i32..word.right.ceil() as i32 {
                if let Some(c) = self.rgb(x, y) {
                    total += 1;
                    if similar(c, color) {
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

fn similar(a: [u8; 3], b: [u8; 3]) -> bool {
    a.into_iter().zip(b).all(|(a, b)| a.abs_diff(b) <= COLOR_TOLERANCE)
}

/// Merge neighbouring buckets before counting, so codec noise at a bucket
/// boundary cannot split one selection colour below the threshold.
fn common_colors(samples: impl Iterator<Item = [u8; 3]>) -> Vec<(u32, [u8; 3])> {
    let mut buckets: HashMap<[u8; 3], (u32, [u32; 3])> = HashMap::new();
    for c in samples {
        let entry = buckets.entry(c.map(|v| v >> 4)).or_default();
        entry.0 += 1;
        for (total, channel) in entry.1.iter_mut().zip(c) {
            *total += channel as u32;
        }
    }
    let mut buckets: Vec<_> = buckets.into_values().collect();
    buckets.sort_unstable_by(|a, b| b.cmp(a));
    buckets.truncate(COLOR_BUCKETS);
    let mut colors = Vec::new();
    for &(n, sum) in &buckets {
        let seed = sum.map(|s| (s / n) as u8);
        let (mut count, mut total) = (0, [0; 3]);
        for &(n, sum) in &buckets {
            if similar(seed, sum.map(|s| (s / n) as u8)) {
                count += n;
                for (total, value) in total.iter_mut().zip(sum) {
                    *total += value;
                }
            }
        }
        colors.push((count, total.map(|s| (s / count) as u8)));
    }
    colors.sort_unstable_by(|a, b| b.cmp(a));
    let mut merged = Vec::new();
    for color in colors {
        if !merged.iter().any(|&(_, c)| similar(color.1, c)) {
            merged.push(color);
        }
    }
    merged
}

/// A double-click selects a word or Chinese run; a triple-click selects a line.
fn clicked_run<'a>(
    shot: &Shot,
    lines: &'a [Vec<Word>],
    at: POINT,
    color: [u8; 3],
    scale: f32,
) -> Option<Vec<&'a Word>> {
    let line = &lines[line_at(lines, at.y as f32, scale)?];
    let distance = |w: &Word| gap(at.x as f32, w.left, w.right);
    let clicked = (0..line.len()).min_by(|&a, &b| distance(&line[a]).total_cmp(&distance(&line[b])))?;
    let word = &line[clicked];
    let selected = |w: &Word| shot.share_of(w, color) >= HIGHLIGHTED_SHARE;
    let letter = LETTER_WIDTH * scale;
    if distance(word) > letter || !selected(word) {
        return None;
    }
    let (mut first, mut last) = (clicked, clicked);
    while first > 0 && selected(&line[first - 1]) {
        first -= 1;
    }
    while last + 1 < line.len() && selected(&line[last + 1]) {
        last += 1;
    }
    let run = &line[first..=last];
    let height = word.bottom - word.top;
    if run.windows(2).any(|pair| pair[1].left - pair[0].right > letter.max(height)) {
        return None;
    }
    let whole_line = first == 0 && last + 1 == line.len();
    let punctuation = |c: char| !c.is_alphanumeric() && !c.is_whitespace();
    let chinese_run = run.iter().all(|w| !w.text.is_empty() && w.text.chars().all(is_cjk))
        && (first == 0 || line[first - 1].text.chars().last().is_some_and(punctuation))
        && (last + 1 == line.len() || line[last + 1].text.chars().next().is_some_and(punctuation));
    if !whole_line && !chinese_run {
        if word.text.chars().any(is_cjk) {
            if run.iter().map(|w| w.text.chars().count()).sum::<usize>() > 8
                || line[last].right - line[first].left > 8.0 * height.max(16.0 * scale)
            {
                return None;
            }
        } else if word.left - line[first].left > letter || line[last].right - word.right > letter {
            return None;
        }
    }

    // A selected row or button also colours the empty space well beyond its
    // words. Only a wide text line may fill all the way to the crop's edge.
    let wide_line = whole_line && line[last].right - line[first].left > shot.width as f32 * WIDE_LINE_SHARE;
    let padding = letter.max(height * 1.5).ceil() as i32;
    let step = (2.0 * scale).ceil() as i32;
    for (edge, direction) in [(line[first].left, -1), (line[last].right, 1)] {
        let mut d = 0;
        loop {
            let x = edge + (d * direction) as f32;
            let mut probe = word.clone();
            probe.left = x - scale;
            probe.right = x + 2.0 * scale;
            if !shot.contains(&probe) {
                if !wide_line {
                    return None;
                }
                break;
            }
            if shot.share_of(&probe, color) < HIGHLIGHTED_SHARE {
                if d > padding + step {
                    return None;
                }
                break;
            }
            if d >= padding + step && !wide_line {
                return None;
            }
            d += step;
        }
    }
    Some(run.iter().collect())
}

/// Words in reading order between the two drag points.
fn pick_range(lines: &[Vec<Word>], from: POINT, to: POINT, scale: f32) -> Vec<&Word> {
    let (a, b) = if (from.y, from.x) <= (to.y, to.x) { (from, to) } else { (to, from) };
    let (Some(first), Some(last)) = (line_at(lines, a.y as f32, scale), line_at(lines, b.y as f32, scale)) else {
        return Vec::new();
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
    picked
}

/// The line nearest to height `y` (within 30 logical pixels).
fn line_at(lines: &[Vec<Word>], y: f32, scale: f32) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let top = line.iter().map(|w| w.top).fold(f32::MAX, f32::min);
            let bottom = line.iter().map(|w| w.bottom).fold(f32::MIN, f32::max);
            (i, gap(y, top, bottom))
        })
        .filter(|&(_, d)| d < 30.0 * scale)
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
fn grab_screen(left: i32, top: i32, w: i32, h: i32, layered: bool) -> Option<Vec<u8>> {
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
            let mode = if layered { SRCCOPY | CAPTUREBLT } else { SRCCOPY };
            let copied = BitBlt(mem, 0, 0, w, h, Some(screen), left, top, mode).is_ok();
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

    fn pick_selection(shot: &Shot, lines: &[Vec<Word>], gesture: Gesture) -> Option<String> {
        super::pick_selection(shot, lines, gesture, 1.0, None)
    }

    fn pixel_ready(shot: &Shot, gesture: Gesture, scale: f32) -> bool {
        let copy = Shot { pixels: shot.pixels.clone(), ..*shot };
        highlighted_shot([Some(copy)].into_iter(), gesture, scale).is_some()
    }

    fn blank(width: i32, height: i32, color: [u8; 3]) -> Shot {
        let mut shot = Shot { left: 0, top: 0, width, height, pixels: vec![255; (width * height * 4) as usize] };
        paint(&mut shot, [0, 0, width, height], color);
        shot
    }

    fn noise(shot: &mut Shot, mut seed: u32) {
        for pixel in shot.pixels.chunks_exact_mut(4) {
            for channel in &mut pixel[..3] {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let shift = ((seed >> 16) % 13) as i16 - 6;
                *channel = (*channel as i16 + shift).clamp(0, 255) as u8;
            }
        }
    }

    fn scaled_page(mut shot: Shot, words: Vec<Word>, scale: usize) -> (Shot, Vec<Word>) {
        shot.pixels = upscale(&shot.pixels, shot.width as usize, shot.height as usize, scale);
        shot.width *= scale as i32;
        shot.height *= scale as i32;
        let words = words.into_iter().map(|w| w.into_screen(0, 0, 1.0 / scale as f32)).collect();
        (shot, words)
    }

    /// A white page with a blue selection over the first two of three words,
    /// each word's letters drawn as dark strokes.
    fn page() -> (Shot, Vec<Word>) {
        page_with_colors([255; 3], [0x00, 0x78, 0xD7], [0x20; 3])
    }

    fn paint(shot: &mut Shot, [left, top, right, bottom]: [i32; 4], [r, g, b]: [u8; 3]) {
        for y in top..bottom {
            for x in left..right {
                let i = ((y * shot.width + x) * 4) as usize;
                shot.pixels[i..i + 3].copy_from_slice(&[b, g, r]);
            }
        }
    }

    fn page_with_colors(background: [u8; 3], highlight: [u8; 3], ink: [u8; 3]) -> (Shot, Vec<Word>) {
        let (w, h) = (300, 40);
        let mut shot = Shot { left: 0, top: 0, width: w, height: h, pixels: vec![255u8; (w * h * 4) as usize] };
        paint(&mut shot, [0, 0, w, h], background);
        paint(&mut shot, [10, 10, 190, 30], highlight);
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
                paint(&mut shot, [x, 12, x + 2, 28], ink);
            }
        }
        (shot, words)
    }

    fn chinese_page(background: [u8; 3], highlight: [u8; 3], ink: [u8; 3]) -> (Shot, Vec<Word>) {
        let mut shot = blank(160, 40, background);
        paint(&mut shot, [10, 8, 49, 32], highlight);
        let words: Vec<_> = ["你", "好", "白", "云"]
            .into_iter()
            .enumerate()
            .map(|(i, text)| Word {
                text: text.into(),
                left: 14.0 + i as f32 * 18.0,
                top: 12.0,
                right: 30.0 + i as f32 * 18.0,
                bottom: 28.0,
            })
            .collect();
        for word in &words {
            for y in 12..28 {
                for x in word.left as i32..word.right as i32 {
                    // Dense intersecting strokes cover 62.5% of each glyph.
                    if (x - word.left as i32) % 4 < 2 || (y - 12) % 4 == 0 {
                        paint(&mut shot, [x, y, x + 1, y + 1], ink);
                    }
                }
            }
        }
        (shot, words)
    }

    fn file_row() -> (Shot, Vec<Word>) {
        let mut shot = blank(900, 50, [255; 3]);
        paint(&mut shot, [0, 10, 900, 34], [0xCC, 0xE8, 0xFF]);
        let words: Vec<_> =
            [("notes.txt", 20, 100), ("2026-10-29", 250, 370), ("Text Document", 480, 650), ("2 KB", 780, 830)]
                .into_iter()
                .map(|(text, left, right)| Word {
                    text: text.into(),
                    left: left as f32,
                    top: 12.0,
                    right: right as f32,
                    bottom: 28.0,
                })
                .collect();
        for word in &words {
            for x in (word.left as i32..word.right as i32).step_by(8) {
                paint(&mut shot, [x, 12, x + 2, 28], [0x20; 3]);
            }
        }
        (shot, words)
    }

    #[test]
    fn keeps_only_highlighted_words() {
        let (shot, words) = page();
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        let color = shot.highlight_colors(drag, 1.0).into_iter().next().expect("highlight found");
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
        assert!(shot.highlight_colors(drag, 1.0).is_empty());
    }

    #[test]
    fn no_highlight_means_nothing_selected() {
        let (shot, words) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
        let lines = vec![words];
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, drag), None);
        for x in 20..80 {
            let click = Gesture::DoubleClick { at: POINT { x, y: 20 } };
            assert_eq!(pick_selection(&shot, &lines, click), None, "plain text at x={x}");
        }
    }

    #[test]
    fn ignores_a_panel_crossed_by_the_drag() {
        let (mut shot, words) = page_with_colors([0x1E; 3], [0x1E; 3], [0xD0; 3]);
        paint(&mut shot, [40, 10, 150, 30], [0x65, 0x7F, 0xA0]);
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert!(!shot.highlight_colors(drag, 1.0).is_empty(), "panel passes the contrast check");
        assert_eq!(pick_selection(&shot, &[words], drag), None);
    }

    #[test]
    fn ignores_a_low_contrast_panel() {
        let (shot, words) = page_with_colors([0x1E; 3], [0x2B; 3], [0xD0; 3]);
        let lines = vec![words];
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, drag), None);
        let click = Gesture::DoubleClick { at: POINT { x: 48, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, click), None);
    }

    #[test]
    fn ignores_a_colour_missing_at_the_drag_ends() {
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        for (left, right) in [(50, 150), (10, 150), (50, 190)] {
            let (mut shot, words) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
            paint(&mut shot, [left, 10, right, 30], [0x00, 0x78, 0xD7]);
            assert_eq!(pick_selection(&shot, &[words], drag), None);
        }
    }

    #[test]
    fn drops_highlighted_words_outside_the_drag() {
        let (mut shot, words) = page();
        shot.pixels = shot.pixels.repeat(5);
        shot.height *= 5;
        let mut lines = Vec::new();
        for i in 0..5 {
            let offset = i * 40;
            paint(&mut shot, [10, 10 + offset, 280, 30 + offset], [0x00, 0x78, 0xD7]);
            lines.push(
                words
                    .iter()
                    .cloned()
                    .map(|mut word| {
                        word.text = format!("{i}:{}", word.text);
                        word.top += offset as f32;
                        word.bottom += offset as f32;
                        word
                    })
                    .collect(),
            );
        }
        let (from, to) = (POINT { x: 115, y: 60 }, POINT { x: 75, y: 140 });
        let expected = Some("1:cloud 1:sky 2:hello 2:cloud 2:sky 3:hello".into());
        assert_eq!(pick_selection(&shot, &lines, Gesture::Drag { from, to }), expected);
        assert_eq!(pick_selection(&shot, &lines, Gesture::Drag { from: to, to: from }), expected);
        let drag = Gesture::Drag { from: POINT { x: 115, y: 100 }, to: POINT { x: 185, y: 100 } };
        assert_eq!(pick_selection(&shot, &lines, drag), Some("2:cloud".into()));
    }

    #[test]
    fn keeps_a_dark_theme_selection() {
        let (shot, words) = page_with_colors([0x1E; 3], [0x26, 0x4F, 0x78], [0xD0; 3]);
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert_eq!(shot.highlight_colors(drag, 1.0).first().copied(), Some([0x26, 0x4F, 0x78]));
        assert_eq!(pick_selection(&shot, &[words], drag), Some("hello cloud".into()));
    }

    #[test]
    fn double_click_keeps_the_highlighted_word() {
        let (mut shot, words) = page();
        paint(&mut shot, [10, 10, 100, 30], [255; 3]);
        shot.pixels = shot.pixels.repeat(2);
        shot.height *= 2;
        let next_line = words.iter().cloned().map(|w| w.into_screen(0, 40, 1.0)).collect();
        let lines = vec![words, next_line];
        for x in [99, 118, 140] {
            let click = Gesture::DoubleClick { at: POINT { x, y: 20 } };
            assert!(pixel_ready(&shot, click, 1.0));
            assert_eq!(pick_selection(&shot, &lines, click), Some("cloud".into()));
        }
        let click = Gesture::DoubleClick { at: POINT { x: 220, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, click), None);
    }

    #[test]
    fn keeps_light_and_grey_selections() {
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        for color in [[0xB4, 0xD7, 0xFF], [0x00, 0x78, 0xD7], [0xC6; 3]] {
            let (shot, words) = page_with_colors([255; 3], color, [0x20; 3]);
            assert_eq!(pick_selection(&shot, &[words], drag), Some("hello cloud".into()));
        }
    }

    #[test]
    fn keeps_chinese_characters() {
        let (shot, words) = chinese_page([255; 3], [0x00, 0x78, 0xD7], [0x20; 3]);
        let lines = vec![words];
        let drag = Gesture::Drag { from: POINT { x: 10, y: 20 }, to: POINT { x: 49, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, drag), Some("你好".into()));
        let click = Gesture::DoubleClick { at: POINT { x: 22, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, click), Some("你好".into()));
    }

    #[test]
    fn keeps_highlight_at_edges_and_under_letters() {
        let (shot, words) = page();
        let lines = vec![words];
        for (from, to) in [(10, 190), (20, 167)] {
            let drag = Gesture::Drag { from: POINT { x: from, y: 20 }, to: POINT { x: to, y: 20 } };
            assert_eq!(pick_selection(&shot, &lines, drag), Some("hello cloud".into()));
        }
    }

    #[test]
    fn highlight_contrast_requires_forty_levels() {
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        let (shot, words) = page_with_colors([30; 3], [69; 3], [0xD0; 3]);
        assert_eq!(pick_selection(&shot, &[words], drag), None);
        let (shot, words) = page_with_colors([30; 3], [70; 3], [0xD0; 3]);
        assert_eq!(pick_selection(&shot, &[words], drag), Some("hello cloud".into()));
    }

    #[test]
    fn no_highlighted_words_means_nothing_selected() {
        let (shot, words) = page();
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert_eq!(pick_selection(&shot, &[vec![words[2].clone()]], drag), None);
        assert_eq!(pick_selection(&shot, &[], drag), None);
    }

    #[test]
    fn keeps_a_drag_that_overshoots_the_line_end() {
        let (mut shot, mut words) = page();
        words.truncate(2);
        paint(&mut shot, [200, 0, 300, 40], [255; 3]);
        let (from, to) = (POINT { x: 15, y: 20 }, POINT { x: 295, y: 20 });
        let lines = vec![words];
        for gesture in [Gesture::Drag { from, to }, Gesture::Drag { from: to, to: from }] {
            assert!(pixel_ready(&shot, gesture, 1.0));
            assert_eq!(pick_selection(&shot, &lines, gesture), Some("hello cloud".into()));
        }
    }

    #[test]
    fn keeps_a_drag_started_in_the_margin() {
        let (page, words) = page();
        let mut shot = blank(400, 40, [255; 3]);
        for y in 0..40 {
            let start = ((y * 400 + 80) * 4) as usize;
            let source = (y * page.width * 4) as usize;
            shot.pixels[start..start + 1200].copy_from_slice(&page.pixels[source..source + 1200]);
        }
        let lines = vec![words.into_iter().map(|w| w.into_screen(80, 0, 1.0)).collect()];
        let (from, to) = (POINT { x: 10, y: 20 }, POINT { x: 265, y: 20 });
        for gesture in [Gesture::Drag { from, to }, Gesture::Drag { from: to, to: from }] {
            assert!(pixel_ready(&shot, gesture, 1.0));
            assert_eq!(pick_selection(&shot, &lines, gesture), Some("hello cloud".into()));
        }
    }

    #[test]
    fn keeps_a_multi_line_drag_ending_past_a_short_line() {
        let (mut shot, words) = page();
        shot.pixels = shot.pixels.repeat(2);
        shot.height *= 2;
        paint(&mut shot, [10, 10, 280, 30], [0x00, 0x78, 0xD7]);
        paint(&mut shot, [90, 40, 300, 80], [255; 3]);
        let short_line = vec![words[0].clone().into_screen(0, 40, 1.0)];
        let lines = vec![words, short_line];
        let (from, to) = (POINT { x: 15, y: 20 }, POINT { x: 290, y: 60 });
        for gesture in [Gesture::Drag { from, to }, Gesture::Drag { from: to, to: from }] {
            assert!(pixel_ready(&shot, gesture, 1.0));
            assert_eq!(pick_selection(&shot, &lines, gesture), Some("hello cloud sky hello".into()));
        }
    }

    #[test]
    fn keeps_a_selection_at_200_percent_scale() {
        let (mut shot, words) = page();
        // The press is four logical pixels before the next letter boundary.
        paint(&mut shot, [10, 10, 30, 30], [255; 3]);
        let (mut shot, words) = scaled_page(shot, words, 2);
        let lines = vec![words];
        let drag = Gesture::Drag { from: POINT { x: 52, y: 40 }, to: POINT { x: 370, y: 40 } };
        assert!(pixel_ready(&shot, drag, 2.0));
        assert_eq!(super::pick_selection(&shot, &lines, drag, 2.0, None), Some("hello cloud".into()));
        paint(&mut shot, [20, 20, 200, 60], [255; 3]);
        let click = Gesture::DoubleClick { at: POINT { x: 198, y: 40 } };
        assert!(pixel_ready(&shot, click, 2.0));
        assert_eq!(super::pick_selection(&shot, &lines, click, 2.0, None), Some("cloud".into()));
        let (shot, words) = chinese_page([255; 3], [0x00, 0x78, 0xD7], [0x20; 3]);
        let (shot, words) = scaled_page(shot, words, 2);
        let drag = Gesture::Drag { from: POINT { x: 20, y: 40 }, to: POINT { x: 98, y: 40 } };
        assert_eq!(super::pick_selection(&shot, &[words], drag, 2.0, None), Some("你好".into()));
        let (shot, words) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
        let (shot, words) = scaled_page(shot, words, 2);
        assert_eq!(super::pick_selection(&shot, &[words], drag, 2.0, None), None);
    }

    #[test]
    fn double_click_rejects_a_whole_selected_row() {
        let (shot, words) = file_row();
        let click = Gesture::DoubleClick { at: POINT { x: 50, y: 20 } };
        // Still reject if the other columns are outside the OCR crop or missed.
        assert_eq!(pick_selection(&shot, &[vec![words[0].clone()]], click), None);
        assert_eq!(pick_selection(&shot, &[words], click), None);
    }

    #[test]
    fn triple_click_keeps_the_whole_line() {
        let (mut shot, mut words) = page();
        // A text line has ordinary spaces, unlike the gaps in file_row().
        words[1] = words[1].clone().into_screen(-20, 0, 1.0);
        words[2] = words[2].clone().into_screen(-50, 0, 1.0);
        for right in [230, 300] {
            paint(&mut shot, [0, 0, 300, 40], [255; 3]);
            paint(&mut shot, [10, 10, right, 30], [0x00, 0x78, 0xD7]);
            for word in &words {
                for x in (word.left as i32..word.right as i32).step_by(8) {
                    paint(&mut shot, [x, 12, x + 2, 28], [0x20; 3]);
                }
            }
            for scale in [1, 2] {
                let (shot, words) = scaled_page(Shot { pixels: shot.pixels.clone(), ..shot }, words.clone(), scale);
                let click = Gesture::DoubleClick { at: POINT { x: 115 * scale as i32, y: 20 * scale as i32 } };
                assert_eq!(
                    super::pick_selection(&shot, &[words], click, scale as f32, None),
                    Some("hello cloud sky".into())
                );
                // Both the pixel gate and the OCR decision accept the full line.
                assert!(highlighted_shot([Some(shot)].into_iter(), click, scale as f32).is_some());
            }
        }
    }

    #[test]
    fn double_click_rejects_a_title_bar_without_baseline() {
        let (_, mut words) = page();
        words[1] = words[1].clone().into_screen(-20, 0, 1.0);
        words[2] = words[2].clone().into_screen(-50, 0, 1.0);
        let mut shot = blank(520, 40, [255; 3]);
        paint(&mut shot, [0, 4, 520, 36], [0x00, 0x78, 0xD7]);
        for scale in [1, 2] {
            let (shot, words) = scaled_page(Shot { pixels: shot.pixels.clone(), ..shot }, words.clone(), scale);
            let click = Gesture::DoubleClick { at: POINT { x: 115 * scale as i32, y: 20 * scale as i32 } };
            assert!(pixel_ready(&shot, click, scale as f32));
            assert_eq!(super::pick_selection(&shot, &[words], click, scale as f32, None), None);
        }
    }

    #[test]
    fn double_click_rejects_a_button_without_baseline() {
        let (_, mut words) = page();
        words[1] = words[1].clone().into_screen(-20, 0, 1.0);
        words.truncate(2);
        for rect in [[5, 4, 200, 36], [0, 4, 160, 36]] {
            let mut shot = blank(300, 40, [255; 3]);
            paint(&mut shot, rect, [0x00, 0x78, 0xD7]);
            for scale in [1, 2] {
                let (shot, words) = scaled_page(Shot { pixels: shot.pixels.clone(), ..shot }, words.clone(), scale);
                let click = Gesture::DoubleClick { at: POINT { x: 115 * scale as i32, y: 20 * scale as i32 } };
                assert!(pixel_ready(&shot, click, scale as f32));
                assert_eq!(super::pick_selection(&shot, &[words], click, scale as f32, None), None);
            }
        }
    }

    #[test]
    fn double_click_rejects_a_chat_bubble_without_baseline() {
        let (_, mut words) = page();
        words[1] = words[1].clone().into_screen(-20, 0, 1.0);
        words[2] = words[2].clone().into_screen(-50, 0, 1.0);
        let mut shot = blank(300, 40, [255; 3]);
        // Even a wide text run must not excuse padding that ends inside the crop.
        paint(&mut shot, [5, 4, 280, 36], [0x00, 0x78, 0xD7]);
        for scale in [1, 2] {
            let (shot, words) = scaled_page(Shot { pixels: shot.pixels.clone(), ..shot }, words.clone(), scale);
            let click = Gesture::DoubleClick { at: POINT { x: 115 * scale as i32, y: 20 * scale as i32 } };
            assert!(pixel_ready(&shot, click, scale as f32));
            assert_eq!(super::pick_selection(&shot, &[words], click, scale as f32, None), None);
        }
    }

    #[test]
    fn double_click_allows_edge_fill_only_for_a_wide_line() {
        let (_, mut words) = page();
        words[1] = words[1].clone().into_screen(-20, 0, 1.0);
        words.truncate(2);
        let mut shot = blank(300, 40, [255; 3]);
        paint(&mut shot, [0, 8, 300, 32], [0x00, 0x78, 0xD7]);
        for width in [179, 180, 181] {
            words[1].right = words[0].left + width as f32;
            for scale in [1, 2] {
                let (shot, words) = scaled_page(Shot { pixels: shot.pixels.clone(), ..shot }, words.clone(), scale);
                let click = Gesture::DoubleClick { at: POINT { x: 115 * scale as i32, y: 20 * scale as i32 } };
                assert_eq!(
                    super::pick_selection(&shot, &[words], click, scale as f32, None),
                    (width > 180).then(|| "hello cloud".into()),
                    "line width {width} at {scale}x"
                );
            }
        }
    }

    #[test]
    fn double_click_keeps_a_long_chinese_run() {
        let phrase = "欢迎来到这片自由云端世界";
        for (prefix, suffix, expected) in [
            ("", "", true),
            ("你好，", "。再见", true),
            ("", "。再见", true),
            ("你好，", "", true),
            ("你好", "。再见", false),
        ] {
            let text = format!("{prefix}{phrase}{suffix}");
            let start = prefix.chars().count();
            let end = start + phrase.chars().count();
            assert!(end - start > 8);
            let mut shot = blank(text.chars().count() as i32 * 18 + 28, 40, [255; 3]);
            let words: Vec<_> = text
                .chars()
                .enumerate()
                .map(|(i, c)| Word {
                    text: c.to_string(),
                    left: 14.0 + i as f32 * 18.0,
                    top: 12.0,
                    right: 30.0 + i as f32 * 18.0,
                    bottom: 28.0,
                })
                .collect();
            paint(
                &mut shot,
                [words[start].left as i32 - 4, 8, words[end - 1].right as i32 + 4, 32],
                [0x00, 0x78, 0xD7],
            );
            for word in &words {
                for y in 12..28 {
                    for x in word.left as i32..word.right as i32 {
                        if (x - word.left as i32) % 4 < 2 || (y - 12) % 4 == 0 {
                            paint(&mut shot, [x, y, x + 1, y + 1], [0x20; 3]);
                        }
                    }
                }
            }
            for scale in [1, 2] {
                let (shot, words) = scaled_page(Shot { pixels: shot.pixels.clone(), ..shot }, words.clone(), scale);
                let click =
                    Gesture::DoubleClick { at: POINT { x: words[start + 5].center_x() as i32, y: 20 * scale as i32 } };
                assert_eq!(
                    super::pick_selection(&shot, &[words], click, scale as f32, None),
                    expected.then(|| phrase.to_string()),
                    "{text} at {scale}x"
                );
            }
        }
    }

    #[test]
    fn reselecting_over_an_old_selection_keeps_all_words() {
        let (before, words) = page();
        let (mut shot, _) = page();
        paint(&mut shot, [190, 10, 280, 30], [0x00, 0x78, 0xD7]);
        let (from, to) = (POINT { x: 15, y: 20 }, POINT { x: 275, y: 20 });
        let lines = vec![words];
        for gesture in [Gesture::Drag { from, to }, Gesture::Drag { from: to, to: from }] {
            assert_eq!(
                super::pick_selection(&shot, &lines, gesture, 1.0, Some(&before)),
                Some("hello cloud sky".into())
            );
            assert_eq!(super::pick_selection(&shot, &lines, gesture, 1.0, Some(&shot)), None);
        }
    }

    #[test]
    fn narrowing_a_selection_keeps_the_new_words() {
        let (mut before, words) = page();
        paint(&mut before, [190, 10, 280, 30], [0x00, 0x78, 0xD7]);
        let (mut shot, _) = page();
        paint(&mut shot, [10, 10, 100, 30], [255; 3]);
        for scale in [1, 2] {
            let (before, _) = scaled_page(Shot { pixels: before.pixels.clone(), ..before }, Vec::new(), scale);
            let (shot, words) = scaled_page(Shot { pixels: shot.pixels.clone(), ..shot }, words.clone(), scale);
            let lines = vec![words];
            let (from, to) = (
                POINT { x: 115 * scale as i32, y: 20 * scale as i32 },
                POINT { x: 165 * scale as i32, y: 20 * scale as i32 },
            );
            for gesture in [Gesture::Drag { from, to }, Gesture::Drag { from: to, to: from }] {
                let area = selection_area(gesture, scale as f32, Some(&before));
                assert!(area.left <= before.left && area.right >= before.left + before.width);
                assert!(area.top <= before.top && area.bottom >= before.top + before.height);
                assert_eq!(
                    super::pick_selection(&shot, &lines, gesture, scale as f32, Some(&before)),
                    Some("cloud".into())
                );
                assert_eq!(super::pick_selection(&shot, &lines, gesture, scale as f32, Some(&shot)), None);
            }
        }
    }

    #[test]
    fn rejects_a_solid_block_that_was_already_there() {
        let (mut shot, mut words) = page();
        words[1] = words[1].clone().into_screen(-20, 0, 1.0);
        words.truncate(2);
        paint(&mut shot, [0, 8, 300, 32], [0x00, 0x78, 0xD7]);
        let mut before = blank(300, 40, [255; 3]);
        paint(&mut before, [0, 8, 300, 32], [0x00, 0x78, 0xD7]);
        let lines = vec![words];
        let click = Gesture::DoubleClick { at: POINT { x: 115, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, click), None);
        assert_eq!(super::pick_selection(&shot, &lines, click, 1.0, Some(&before)), None);
        let drag = Gesture::Drag { from: POINT { x: 25, y: 20 }, to: POINT { x: 145, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, drag), Some("hello cloud".into()));
        assert_eq!(super::pick_selection(&shot, &lines, drag, 1.0, Some(&before)), None);
        // Expanding a drag keeps its old words as well as the new one.
        paint(&mut before, [85, 8, 300, 32], [255; 3]);
        assert_eq!(super::pick_selection(&shot, &lines, click, 1.0, Some(&before)), None);
        assert_eq!(super::pick_selection(&shot, &lines, drag, 1.0, Some(&before)), Some("hello cloud".into()));
    }

    #[test]
    fn baseline_band_covers_the_press_area_and_scales() {
        let viewer = RECT { left: -2000, top: -1000, right: 4000, bottom: 2000 };
        let at = POINT { x: 1000, y: 500 };
        for scale in [1.0, 2.0] {
            let band = baseline_area(viewer, at, scale);
            assert_eq!(band.right - band.left, (1400.0 * scale) as i32);
            assert_eq!(band.bottom - band.top, (60.0 * scale) as i32);
            let area = selection_area(Gesture::DoubleClick { at }, scale, None);
            assert!(
                area.left >= band.left
                    && area.top >= band.top
                    && area.right <= band.right
                    && area.bottom <= band.bottom
            );
            let area = selection_area(
                Gesture::Drag { from: at, to: POINT { x: at.x, y: at.y + (100.0 * scale) as i32 } },
                scale,
                None,
            );
            assert_eq!(area.left, band.left);
            assert_eq!(area.right, band.right);
            assert!(area.top >= band.top);
            assert!(area.bottom > band.bottom);
            let edge = baseline_area(viewer, POINT { x: -1990, y: -990 }, scale);
            assert_eq!(edge.left, viewer.left);
            assert_eq!(edge.top, viewer.top);
        }
    }

    #[test]
    fn common_colors_caps_the_bucket_merge() {
        let all_buckets =
            (0..4096).map(|i| [((i >> 8) * 16) as u8, (((i >> 4) & 15) * 16) as u8, ((i & 15) * 16) as u8]);
        let color = [0x26, 0x4F, 0x78];
        let colors = common_colors(all_buckets.chain(std::iter::repeat_n(color, 1000)));
        assert!(colors.len() <= COLOR_BUCKETS);
        assert!(colors[0].0 >= 1000);
        assert!(similar(colors[0].1, color));
    }

    #[test]
    fn rejects_a_title_bar_that_was_already_there() {
        let (mut shot, words) = page();
        paint(&mut shot, [10, 10, 280, 30], [0x00, 0x78, 0xD7]);
        for word in &words {
            for x in (word.left as i32..word.right as i32).step_by(8) {
                paint(&mut shot, [x, 12, x + 2, 28], [0x20; 3]);
            }
        }
        let mut before = blank(300, 40, [255; 3]);
        paint(&mut before, [10, 10, 280, 30], [0x00, 0x78, 0xD7]);
        // The title moved: old ink exposes new blue pixels, but its words'
        // screen positions were already on the blue bar before the gesture.
        paint(&mut before, [20, 14, 170, 26], [0x20; 3]);
        let lines = vec![words];
        let drag = Gesture::Drag { from: POINT { x: 25, y: 20 }, to: POINT { x: 165, y: 20 } };
        assert_eq!(pick_selection(&shot, &lines, drag), Some("hello cloud".into()));
        assert!(!shot.highlight_colors(drag, 1.0).is_empty());
        assert_eq!(super::pick_selection(&shot, &lines, drag, 1.0, Some(&before)), None);
    }

    #[test]
    fn keeps_a_highlight_that_appeared_since_button_down() {
        let (shot, words) = page();
        let (before, _) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert_eq!(super::pick_selection(&shot, &[words], drag, 1.0, Some(&before)), Some("hello cloud".into()));
    }

    #[test]
    fn missing_baseline_pixels_do_not_reject_a_selection() {
        let (shot, words) = page();
        let mut before = blank(50, 40, [0x00, 0x78, 0xD7]);
        before.left = 500;
        let lines = vec![words];
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        assert_eq!(super::pick_selection(&shot, &lines, drag, 1.0, Some(&before)), Some("hello cloud".into()));
        // Only the old first word is covered by this small baseline.
        let before = blank(100, 40, [0x00, 0x78, 0xD7]);
        assert_eq!(super::pick_selection(&shot, &lines, drag, 1.0, Some(&before)), Some("hello cloud".into()));
    }

    #[test]
    fn ignores_a_baseline_that_arrived_after_button_up() {
        let released = Instant::now();
        let before = Baseline { shot: blank(300, 40, [255; 3]), window: 0, ready: released + Duration::from_millis(1) };
        assert!(!before.ready_by(released));
        assert!(before.ready_by(released + Duration::from_millis(2)));
    }

    #[test]
    fn survives_codec_noise() {
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        for (background, color, ink) in [
            ([0x1E; 3], [0x26, 0x4F, 0x78], [0xD0; 3]),
            ([255; 3], [0x00, 0x78, 0xD7], [0x20; 3]),
            ([255; 3], [0xB4, 0xD7, 0xFF], [0x20; 3]),
            ([255; 3], [0xC6; 3], [0x20; 3]),
        ] {
            let (mut shot, words) = page_with_colors(background, color, ink);
            noise(&mut shot, 1);
            assert!(pixel_ready(&shot, drag, 1.0), "colour {color:?}");
            assert_eq!(pick_selection(&shot, &[words], drag), Some("hello cloud".into()), "colour {color:?}");
            let (mut shot, words) = chinese_page(background, color, ink);
            noise(&mut shot, 2);
            let cjk_drag = Gesture::Drag { from: POINT { x: 10, y: 20 }, to: POINT { x: 49, y: 20 } };
            assert!(pixel_ready(&shot, cjk_drag, 1.0));
            assert_eq!(pick_selection(&shot, &[words], cjk_drag), Some("你好".into()));
        }
        for (background, panel, ink) in [([255; 3], [255; 3], [0x20; 3]), ([0x1E; 3], [0x2B; 3], [0xD0; 3])] {
            let (mut shot, words) = page_with_colors(background, panel, ink);
            noise(&mut shot, 3);
            let lines = vec![words];
            assert_eq!(pick_selection(&shot, &lines, drag), None);
            assert_eq!(pick_selection(&shot, &lines, Gesture::DoubleClick { at: POINT { x: 48, y: 20 } }), None);
            let (mut shot, words) = chinese_page(background, panel, ink);
            noise(&mut shot, 4);
            assert_eq!(pick_selection(&shot, &[words], drag), None);
        }
        let (mut shot, words) = page_with_colors([0x1E; 3], [0x1E; 3], [0xD0; 3]);
        paint(&mut shot, [40, 10, 150, 30], [0x65, 0x7F, 0xA0]);
        noise(&mut shot, 5);
        assert_eq!(pick_selection(&shot, &[words], drag), None);
        let (mut shot, words) = file_row();
        noise(&mut shot, 6);
        assert_eq!(pick_selection(&shot, &[words], Gesture::DoubleClick { at: POINT { x: 50, y: 20 } }), None);
        let (mut shot, words) = page();
        let (mut before, _) = page();
        noise(&mut shot, 7);
        noise(&mut before, 8);
        assert_eq!(super::pick_selection(&shot, &[words], drag, 1.0, Some(&before)), None);
    }

    #[test]
    fn waits_for_a_late_highlight() {
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        for gesture in [drag, Gesture::DoubleClick { at: POINT { x: 140, y: 20 } }] {
            let (mut arriving, _) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
            let (mut selected, words) = page();
            let expected = match gesture {
                Gesture::Drag { .. } => {
                    // A drag must already show some colour at one end.
                    paint(&mut arriving, [14, 19, 17, 22], [0x00, 0x78, 0xD7]);
                    assert!(!arriving.highlight_candidates(gesture, 1.0).is_empty());
                    "hello cloud"
                }
                Gesture::DoubleClick { .. } => {
                    assert!(arriving.highlight_candidates(gesture, 1.0).is_empty());
                    paint(&mut selected, [10, 10, 100, 30], [255; 3]);
                    "cloud"
                }
            };
            assert!(arriving.highlight_colors(gesture, 1.0).is_empty());
            let mut captures = 0;
            let shots = [Some(arriving), None, Some(selected), None].into_iter().inspect(|_| captures += 1);
            let shot = highlighted_shot(shots, gesture, 1.0).expect("the later selection is used");
            assert_eq!(captures, 3);
            assert_eq!(pick_selection(&shot, &[words], gesture), Some(expected.into()));
        }
    }

    #[test]
    fn waits_for_a_late_drag_end() {
        for scale in [1, 2] {
            for reverse in [false, true] {
                let (mut arriving, _) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
                paint(&mut arriving, [10, 10, 150, 30], [0x00, 0x78, 0xD7]);
                let (selected, words) = page();
                let (arriving, _) = scaled_page(arriving, Vec::new(), scale);
                let (selected, words) = scaled_page(selected, words, scale);
                let (from, to) = (
                    POINT { x: 15 * scale as i32, y: 20 * scale as i32 },
                    POINT { x: 185 * scale as i32, y: 20 * scale as i32 },
                );
                let drag = if reverse { Gesture::Drag { from: to, to: from } } else { Gesture::Drag { from, to } };
                let mut captures = 0;
                let shots = [Some(arriving), None, Some(selected), None].into_iter().inspect(|_| captures += 1);
                let shot = highlighted_shot(shots, drag, scale as f32).expect("waited for both ends");
                assert_eq!(captures, 3);
                assert_eq!(
                    super::pick_selection(&shot, &[words], drag, scale as f32, None),
                    Some("hello cloud".into())
                );
            }
        }
    }

    #[test]
    fn drag_without_highlight_stops_after_the_first_capture() {
        let drag = Gesture::Drag { from: POINT { x: 15, y: 20 }, to: POINT { x: 185, y: 20 } };
        for middle_panel in [false, true] {
            let mut captures = 0;
            let (mut plain, _) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
            if middle_panel {
                paint(&mut plain, [90, 10, 150, 30], [0x00, 0x78, 0xD7]);
            }
            let (selected, _) = page();
            let shots = [Some(plain), None, Some(selected)].into_iter().inspect(|_| captures += 1);
            assert!(highlighted_shot(shots, drag, 1.0).is_none());
            assert_eq!(captures, 1);
        }
    }

    #[test]
    fn double_click_keeps_a_highlight_after_a_plain_capture() {
        let (plain, _) = page_with_colors([255; 3], [255; 3], [0x20; 3]);
        let (mut selected, words) = page();
        paint(&mut selected, [10, 10, 100, 30], [255; 3]);
        let click = Gesture::DoubleClick { at: POINT { x: 140, y: 20 } };
        let mut captures = 0;
        let shots = [Some(plain), Some(selected), None].into_iter().inspect(|_| captures += 1);
        let shot = highlighted_shot(shots, click, 1.0).expect("a plain first capture does not stop a double-click");
        assert_eq!(captures, 2);
        assert_eq!(pick_selection(&shot, &[words], click), Some("cloud".into()));
    }

    #[test]
    fn double_click_without_highlight_polls_the_full_window() {
        let click = Gesture::DoubleClick { at: POINT { x: 140, y: 20 } };
        let attempts = (HIGHLIGHT_WAIT.as_millis() / HIGHLIGHT_POLL.as_millis()) as usize + 1;
        let mut captures = 0;
        let shots = std::iter::repeat_with(|| Some(page_with_colors([255; 3], [255; 3], [0x20; 3]).0))
            .take(attempts)
            .inspect(|_| captures += 1);
        assert!(highlighted_shot(shots, click, 1.0).is_none());
        assert_eq!(captures, attempts);
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
        println!("OCR: {:?}", read_selection(gesture, None));
    }
}
