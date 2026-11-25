//! Where the notification-area items (clock, bell...) start on a Windows 11 taskbar that has no
//! window for them. Windows 11 draws the secondary taskbars' clock in XAML, so there is no
//! `ClockButton` window to place the widget against; UI Automation still exposes those items
//! (automation id `SystemTrayIcon`) with their screen rectangles.
//!
//! UI Automation is asked from a worker thread (the widget's own window is a child of the taskbar,
//! so asking from the UI thread could wait on itself), and only when a taskbar is new, moved or
//! resized, when `taskbars_changed` says the main taskbar's notification area changed (the
//! notification bell appears on every taskbar at once and moves the clock), and periodically to track open app buttons. The answer is
//! kept as a distance from the taskbar's right end, so it stays right while the taskbar is checked
//! again after a move or resize. When an answer arrives, `notify` gets `msg` so the widget can be
//! placed at once.
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, TreeScope_Descendants};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

const REFRESH: Duration = Duration::from_secs(60);
const TASK_BUTTON_REFRESH: Duration = Duration::from_secs(1);
/// A first answer normally takes well under a second; after this, give up waiting for it.
const FIRST_WAIT: Duration = Duration::from_secs(2);
const TRAY_ID: &str = "SystemTrayIcon";

/// Where a taskbar's notification area begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEdge {
    /// Screen x of the first notification-area item (screen y on a vertical taskbar).
    Known(i32),
    /// First check still running: keep the widget hidden for now.
    Pending,
    /// UI Automation did not tell: use an estimate.
    Unknown,
}

struct Entry {
    bar: (i32, i32, i32, i32),
    /// Distance of the first item from the taskbar's right end (bottom end when vertical).
    from_right: Option<i32>,
    buttons_right: Option<i32>,
    xaml: Option<bool>,
    at: Instant,
    /// Check again at the next call (the answer is kept until then).
    dirty: bool,
}

#[derive(Default)]
struct Cache {
    known: HashMap<isize, Entry>,
    /// First request for a taskbar not answered yet.
    asked: HashMap<isize, Instant>,
    in_flight: HashSet<isize>,
}

static CACHE: Mutex<Option<Cache>> = Mutex::new(None);

/// Rightmost app button in the last background measurement.
pub fn app_layout(bar: HWND) -> Option<(bool, Option<i32>)> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let entry = cache.as_ref()?.known.get(&(bar.0 as isize))?;
    Some((entry.xaml?, entry.buttons_right))
}

/// Starts or refreshes the background measurement and returns the notification-area edge.
pub fn tray_left(bar: HWND, bar_rect: RECT, notify: HWND, msg: u32) -> TrayEdge {
    let key = bar.0 as isize;
    let r = (bar_rect.left, bar_rect.top, bar_rect.right, bar_rect.bottom);
    let vertical = r.3 - r.1 > r.2 - r.0;
    // The axis along the taskbar: (start, end) of the bar on it.
    let (lo, hi) = if vertical { (r.1, r.3) } else { (r.0, r.2) };
    let (edge, start) = {
        let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let c = g.get_or_insert_with(Cache::default);
        let (edge, stale) = match c.known.get(&key) {
            Some(e) => (e.from_right.map_or(TrayEdge::Unknown, |d| TrayEdge::Known(hi - d)), e.dirty || e.bar != r || e.at.elapsed() >= if !vertical && e.xaml != Some(false) { TASK_BUTTON_REFRESH } else { REFRESH }),
            None => {
                let since = *c.asked.entry(key).or_insert_with(Instant::now);
                (if since.elapsed() < FIRST_WAIT { TrayEdge::Pending } else { TrayEdge::Unknown }, true)
            }
        };
        (edge, stale && c.in_flight.insert(key))
    };
    if start {
        let notify = notify.0 as isize;
        std::thread::spawn(move || {
            let (tray_edge, buttons_right, xaml) = probe(key, vertical).map_or((None, None, None), |(tray, buttons, xaml)| (tray, buttons, Some(xaml)));
            let from_right = tray_edge.filter(|x| *x > lo && *x < hi).map(|x| hi - x);
            let buttons_right = buttons_right.filter(|x| *x > lo && *x <= hi);
            {
                let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                let c = g.get_or_insert_with(Cache::default);
                // A transient COM error should not discard the last measurement for the same rectangle.
                if xaml.is_none() && let Some(previous) = c.known.get_mut(&key) && previous.bar == r {
                    previous.at = Instant::now();
                    previous.dirty = false;
                } else {
                    c.known.insert(key, Entry { bar: r, from_right, buttons_right, xaml, at: Instant::now(), dirty: false });
                }
                c.in_flight.remove(&key);
                c.known.retain(|_, e| e.at.elapsed() < REFRESH * 4);
                c.asked.remove(&key);
            }
            unsafe {
                let _ = PostMessageW(Some(HWND(notify as *mut _)), msg, WPARAM(0), LPARAM(0));
            }
        });
    }
    edge
}

/// The main taskbar's notification area changed size (e.g. the notification bell appeared or the
/// date got longer): check every taskbar again, keeping the current answers until then.
pub fn taskbars_changed() {
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        for e in c.known.values_mut() {
            e.dirty = true;
        }
    }
}

/// Leftmost (topmost when `vertical`) `SystemTrayIcon` element under the taskbar window `bar`.
fn probe(bar: isize, vertical: bool) -> Option<(Option<i32>, Option<i32>, bool)> {
    unsafe {
        let init = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
        let found = (|| -> windows::core::Result<(Option<i32>, Option<i32>, bool)> {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let root = uia.ElementFromHandle(HWND(bar as *mut _))?;
            let all = root.FindAll(TreeScope_Descendants, &uia.CreateTrueCondition()?)?;
            let mut best: Option<i32> = None;
            let mut buttons: Option<i32> = None;
            let mut xaml = false;
            for i in 0..all.Length()? {
                let e = all.GetElement(i)?;
                let id = e.CurrentAutomationId().map(|id| id.to_string()).unwrap_or_default();
                let tray = id == TRAY_ID;
                xaml |= id == "TaskbarFrame";
                let button = !vertical && e.CurrentClassName().is_ok_and(|class| {
                    let class = class.to_string();
                    class == "Taskbar.TaskListButtonAutomationPeer" || class == "Taskbar.TaskListButton"
                });
                xaml |= button;
                if !tray && !button {
                    continue;
                }
                let Ok(b) = e.CurrentBoundingRectangle() else { continue };
                if button && !e.CurrentIsOffscreen().is_ok_and(|offscreen| offscreen.as_bool()) && b.right > b.left && b.bottom > b.top {
                    buttons = Some(buttons.map_or(b.right, |x| x.max(b.right)));
                }
                if !tray {
                    continue;
                }
                if b.right > b.left && b.bottom > b.top {
                    let edge = if vertical { b.top } else { b.left };
                    best = Some(best.map_or(edge, |x| x.min(edge)));
                }
            }
            Ok((best, buttons, xaml))
        })();
        if init {
            CoUninitialize();
        }
        found.ok()
    }
}
