//! Run a parsed macro: focus the game window and inject keyboard input (Windows).

use crate::games::macro_text::{Key, KeyChord, Step};
use crate::platform::app_log;
use std::collections::HashSet;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const FOCUS_POLL_CAP: Duration = Duration::from_millis(400);
const FOCUS_SETTLE: Duration = Duration::from_millis(80);
const KEY_GAP: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    NoWindow,
    FocusFailed,
    LostFocus,
    #[allow(dead_code)] // non-Windows builds
    Unsupported,
    SendFailed(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoWindow => write!(f, "no visible game window"),
            Self::FocusFailed => write!(f, "could not focus game window"),
            Self::LostFocus => write!(f, "game lost foreground before keys"),
            Self::Unsupported => write!(f, "macros are Windows-only"),
            Self::SendFailed(s) => write!(f, "send failed: {s}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOk {
    pub focused: bool,
}

/// Execute `steps` against processes matching `match_paths`.
///
/// Call from a worker thread (blocks on waits / focus polls).
pub fn run(match_paths: &[PathBuf], steps: &[Step]) -> Result<RunOk, RunError> {
    #[cfg(windows)]
    {
        run_windows(match_paths, steps)
    }
    #[cfg(not(windows))]
    {
        let _ = (match_paths, steps);
        Err(RunError::Unsupported)
    }
}

/// Focus the game only (largest visible non-tool window).
pub fn focus_game(match_paths: &[PathBuf]) -> Result<(), RunError> {
    #[cfg(windows)]
    {
        let pids = matching_pids(match_paths);
        if pids.is_empty() {
            return Err(RunError::NoWindow);
        }
        let hwnd = pick_hwnd(&pids).ok_or(RunError::NoWindow)?;
        focus_hwnd(hwnd, &pids)
    }
    #[cfg(not(windows))]
    {
        let _ = match_paths;
        Err(RunError::Unsupported)
    }
}

/// Execute steps, treating `focus` as a no-op (caller already focused).
pub fn run_keys(match_paths: &[PathBuf], steps: &[Step]) -> Result<RunOk, RunError> {
    let filtered: Vec<Step> = steps
        .iter()
        .filter(|s| !matches!(s, Step::Focus))
        .cloned()
        .collect();
    if filtered.is_empty() {
        return Ok(RunOk {
            focused: foreground_matches(match_paths),
        });
    }
    run(match_paths, &filtered)
}

/// Whether the foreground window belongs to one of `match_paths`.
#[allow(dead_code)] // used by run_keys empty-path and diagnostics
pub fn foreground_matches(match_paths: &[PathBuf]) -> bool {
    #[cfg(windows)]
    {
        let pids = matching_pids(match_paths);
        foreground_pid_in(&pids)
    }
    #[cfg(not(windows))]
    {
        let _ = match_paths;
        false
    }
}

#[cfg(windows)]
fn run_windows(match_paths: &[PathBuf], steps: &[Step]) -> Result<RunOk, RunError> {
    let started = Instant::now();
    crate::controller::hid::diag::diag_info(format!(
        "macro: begin steps={} paths={}",
        steps.len(),
        match_paths.len()
    ));

    let pids = matching_pids(match_paths);
    if pids.is_empty() {
        app_log::warn("macro: no matching process");
        return Err(RunError::NoWindow);
    }

    let mut held: Vec<KeyChord> = Vec::new();
    let mut did_focus = false;

    let result = (|| {
        for step in steps {
            match step {
                Step::Focus => {
                    let t0 = Instant::now();
                    let hwnd = pick_hwnd(&pids).ok_or(RunError::NoWindow)?;
                    focus_hwnd(hwnd, &pids)?;
                    did_focus = true;
                    crate::controller::hid::diag::diag_info(format!(
                        "macro: focus ok ms={}",
                        t0.elapsed().as_millis()
                    ));
                    thread::sleep(FOCUS_SETTLE);
                }
                Step::Wait(ms) => {
                    thread::sleep(Duration::from_millis(u64::from(*ms)));
                }
                Step::Press(chord) => {
                    ensure_foreground(&pids)?;
                    press_chord(chord)?;
                    thread::sleep(KEY_GAP);
                }
                Step::Type(text) => {
                    ensure_foreground(&pids)?;
                    send_unicode(text)?;
                }
                Step::KeyDown(chord) => {
                    ensure_foreground(&pids)?;
                    send_chord(chord, true)?;
                    held.push(chord.clone());
                }
                Step::KeyUp(chord) => {
                    ensure_foreground(&pids)?;
                    send_chord(chord, false)?;
                    held.retain(|h| h != chord);
                }
            }
        }
        Ok(RunOk {
            focused: did_focus || foreground_pid_in(&pids),
        })
    })();

    for chord in held.into_iter().rev() {
        let _ = send_chord(&chord, false);
    }

    match &result {
        Ok(_) => {
            app_log::info(format!(
                "macro: ok steps={} ms={}",
                steps.len(),
                started.elapsed().as_millis()
            ));
        }
        Err(err) => {
            app_log::warn(format!(
                "macro: stop reason={err} ms={}",
                started.elapsed().as_millis()
            ));
        }
    }
    result
}

#[cfg(windows)]
fn ensure_foreground(pids: &HashSet<u32>) -> Result<(), RunError> {
    if foreground_pid_in(pids) {
        Ok(())
    } else {
        Err(RunError::LostFocus)
    }
}

#[cfg(windows)]
fn matching_pids(match_paths: &[PathBuf]) -> HashSet<u32> {
    use crate::games::process_match;
    let images = process_match::list_process_images();
    images
        .into_iter()
        .filter_map(|(pid, path)| {
            process_match::any_matching_in_images(match_paths, &[(pid, path.clone())])
                .then_some(pid)
        })
        .collect()
}

#[cfg(windows)]
fn pick_hwnd(pids: &HashSet<u32>) -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GWL_EXSTYLE, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible, WS_EX_TOOLWINDOW,
    };

    struct State {
        pids: HashSet<u32>,
        best: Option<(i64, HWND)>,
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        let state = unsafe { &mut *(lparam.0 as *mut State) };
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return true.into();
        }
        if unsafe { IsIconic(hwnd) }.as_bool() {
            return true.into();
        }
        let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        if ex & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
        }
        if !state.pids.contains(&pid) {
            return true.into();
        }
        let mut rect = RECT::default();
        if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
            return true.into();
        }
        let area = i64::from(rect.right - rect.left) * i64::from(rect.bottom - rect.top);
        if area <= 0 {
            return true.into();
        }
        match state.best {
            Some((best_area, _)) if best_area >= area => {}
            _ => state.best = Some((area, hwnd)),
        }
        true.into()
    }

    let mut state = State {
        pids: pids.clone(),
        best: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut state as *mut _ as isize));
    }
    state.best.map(|(_, hwnd)| hwnd)
}

#[cfg(windows)]
fn focus_hwnd(hwnd: windows::Win32::Foundation::HWND, pids: &HashSet<u32>) -> Result<(), RunError> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };

    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        if SetForegroundWindow(hwnd).as_bool() && foreground_pid_in(pids) {
            return wait_foreground(pids);
        }

        let fg = GetForegroundWindow();
        if fg != HWND::default() && fg != hwnd {
            let mut fg_pid = 0u32;
            let fg_tid = GetWindowThreadProcessId(fg, Some(&mut fg_pid));
            let mut target_pid = 0u32;
            let target_tid = GetWindowThreadProcessId(hwnd, Some(&mut target_pid));
            let our_tid = GetCurrentThreadId();
            let _ = AttachThreadInput(our_tid, fg_tid, true);
            let _ = AttachThreadInput(our_tid, target_tid, true);
            let _ = SetForegroundWindow(hwnd);
            let _ = AttachThreadInput(our_tid, fg_tid, false);
            let _ = AttachThreadInput(our_tid, target_tid, false);
        } else {
            let _ = SetForegroundWindow(hwnd);
        }
    }
    wait_foreground(pids)
}

#[cfg(windows)]
fn wait_foreground(pids: &HashSet<u32>) -> Result<(), RunError> {
    let deadline = Instant::now() + FOCUS_POLL_CAP;
    while Instant::now() < deadline {
        if foreground_pid_in(pids) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Err(RunError::FocusFailed)
}

#[cfg(windows)]
fn foreground_pid_in(pids: &HashSet<u32>) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pids.contains(&pid)
    }
}

#[cfg(windows)]
fn press_chord(chord: &KeyChord) -> Result<(), RunError> {
    send_chord(chord, true)?;
    send_chord(chord, false)
}

#[cfg(windows)]
fn send_chord(chord: &KeyChord, down: bool) -> Result<(), RunError> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        VIRTUAL_KEY, VK_CONTROL, VK_LWIN, VK_MENU, VK_SHIFT,
    };

    let mut events: Vec<(VIRTUAL_KEY, bool)> = Vec::new();
    if down {
        if chord.ctrl {
            events.push((VK_CONTROL, true));
        }
        if chord.alt {
            events.push((VK_MENU, true));
        }
        if chord.shift {
            events.push((VK_SHIFT, true));
        }
        if chord.win {
            events.push((VK_LWIN, true));
        }
    }

    if let Some(vk) = key_vk(chord.key) {
        events.push((vk, down));
    } else if let Key::Char(ch) = chord.key {
        // No fixed VK — inject as Unicode when pressing.
        if down {
            send_vk_events(&events)?;
            send_unicode(&ch.to_string())?;
            return Ok(());
        }
    } else {
        return Err(RunError::SendFailed(format!("no vk for {:?}", chord.key)));
    }

    if !down {
        if chord.win {
            events.push((VK_LWIN, false));
        }
        if chord.shift {
            events.push((VK_SHIFT, false));
        }
        if chord.alt {
            events.push((VK_MENU, false));
        }
        if chord.ctrl {
            events.push((VK_CONTROL, false));
        }
    }

    send_vk_events(&events)
}

#[cfg(windows)]
fn send_vk_events(
    events: &[(
        windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY,
        bool,
    )],
) -> Result<(), RunError> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    };

    if events.is_empty() {
        return Ok(());
    }
    let mut inputs: Vec<INPUT> = Vec::with_capacity(events.len());
    for (vk, down) in events {
        let flags = if *down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: *vk,
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(RunError::SendFailed(format!(
            "SendInput returned {sent}/{}",
            inputs.len()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn send_unicode(text: &str) -> Result<(), RunError> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, SendInput,
    };

    let mut inputs: Vec<INPUT> = Vec::new();
    for unit in text.encode_utf16() {
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
    }
    if inputs.is_empty() {
        return Ok(());
    }
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        return Err(RunError::SendFailed(format!(
            "SendInput unicode returned {sent}/{}",
            inputs.len()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn key_vk(key: Key) -> Option<windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    Some(match key {
        Key::Enter => VK_RETURN,
        Key::Esc => VK_ESCAPE,
        Key::Tab => VK_TAB,
        Key::Space => VK_SPACE,
        Key::Backspace => VK_BACK,
        Key::Delete => VK_DELETE,
        Key::Up => VK_UP,
        Key::Down => VK_DOWN,
        Key::Left => VK_LEFT,
        Key::Right => VK_RIGHT,
        Key::Home => VK_HOME,
        Key::End => VK_END,
        Key::PageUp => VK_PRIOR,
        Key::PageDown => VK_NEXT,
        Key::F(n) => match n {
            1 => VK_F1,
            2 => VK_F2,
            3 => VK_F3,
            4 => VK_F4,
            5 => VK_F5,
            6 => VK_F6,
            7 => VK_F7,
            8 => VK_F8,
            9 => VK_F9,
            10 => VK_F10,
            11 => VK_F11,
            12 => VK_F12,
            _ => return None,
        },
        Key::Char(c) => {
            let u = c.to_ascii_uppercase() as u8;
            if u.is_ascii_alphanumeric() {
                VIRTUAL_KEY(u16::from(u))
            } else {
                return None;
            }
        }
    })
}
