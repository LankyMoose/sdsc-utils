//! Center-top clock widget for immersive Start.
//!
//! On Windows the widget uses the user's system locale time format
//! (`GetTimeFormatW` with the user locale, no seconds), so 12h vs 24h,
//! leading zeros, separators, and AM/PM markers all follow the same settings
//! as the taskbar clock. Other platforms fall back to 12h am/pm.
//! Updates whenever immersive redraws (which ticks continuously).

use crate::ui::start::position::{local_utc_offset_secs, now_ms};

/// Format 24h hour/minute as 12h `h:mm am/pm` (no leading zero on the hour,
/// matching system clocks). Midnight and noon render as 12.
/// Fallback for non-Windows and when the locale API fails.
pub fn format_clock(hour: u8, minute: u8) -> String {
    let hour = hour.min(23);
    let minute = minute.min(59);
    let (hour12, suffix) = match hour {
        0 => (12, "am"),
        1..=11 => (hour, "am"),
        12 => (12, "pm"),
        _ => (hour - 12, "pm"),
    };
    format!("{hour12}:{minute:02} {suffix}")
}

/// Current local hour/minute from system time + local offset.
pub fn local_time_hm() -> (u8, u8) {
    let local_secs = (now_ms() / 1000) as i64 + i64::from(local_utc_offset_secs());
    let secs_of_day = local_secs.rem_euclid(86_400);
    let hour = (secs_of_day / 3600) as u8;
    let minute = ((secs_of_day % 3600) / 60) as u8;
    (hour, minute)
}

/// System locale time (e.g. `7:06 pm` or `19:06`), no seconds.
///
/// `None` when the locale API is unavailable or fails; callers fall back
/// to 12h am/pm [`format_clock`].
#[cfg(windows)]
fn windows_locale_time() -> Option<String> {
    use windows::Win32::Globalization::{GetTimeFormatW, LOCALE_USER_DEFAULT, TIME_NOSECONDS};
    use windows::core::PCWSTR;

    // Passing no time uses the current local time; a null format uses the
    // user's short-time locale format. First call queries the required length
    // (including the null terminator).
    let needed = unsafe {
        GetTimeFormatW(
            LOCALE_USER_DEFAULT,
            TIME_NOSECONDS.0,
            None,
            PCWSTR::null(),
            None,
        )
    };
    if needed <= 0 {
        return None;
    }
    let mut buf = vec![0u16; needed as usize];
    let written = unsafe {
        GetTimeFormatW(
            LOCALE_USER_DEFAULT,
            TIME_NOSECONDS.0,
            None,
            PCWSTR::null(),
            Some(&mut buf[..]),
        )
    };
    if written <= 0 {
        return None;
    }
    // `written` includes the null terminator; strip it before decoding.
    let len = (written as usize).saturating_sub(1);
    String::from_utf16(&buf[..len])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Well-formatted current system time for the clock widget.
pub fn clock_text() -> String {
    #[cfg(windows)]
    if let Some(locale) = windows_locale_time() {
        return locale;
    }
    let (hour, minute) = local_time_hm();
    format_clock(hour, minute)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_12h_am_pm() {
        assert_eq!(format_clock(0, 0), "12:00 am");
        assert_eq!(format_clock(9, 5), "9:05 am");
        assert_eq!(format_clock(11, 59), "11:59 am");
        assert_eq!(format_clock(12, 0), "12:00 pm");
        assert_eq!(format_clock(14, 32), "2:32 pm");
        assert_eq!(format_clock(23, 59), "11:59 pm");
    }

    #[test]
    fn clamps_out_of_range() {
        assert_eq!(format_clock(255, 255), "11:59 pm");
    }

    #[test]
    fn clock_text_looks_like_a_time() {
        // Locale-dependent on Windows (`7:06 PM` vs `19:06`); only assert shape.
        let text = clock_text();
        assert!(text.contains(':'), "unexpected clock text: {text:?}");
        assert!(!text.trim().is_empty());
    }
}
