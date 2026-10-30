//! Strings, formatting, small Win32 helpers.

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::Globalization::{GetDateFormatEx, GetTimeFormatEx, DATE_SHORTDATE, TIME_NOSECONDS};
use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

/// NUL-terminated UTF-16.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(w: &[u16]) -> String {
    let n = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..n])
}

/// 1407908 -> "1,407,908"
pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Everything-style size column: whole KB, rounded up, with separators.
pub fn size_kb(bytes: u64) -> String {
    format!("{} KB", thousands(bytes.div_ceil(1024)))
}

pub fn human_bytes(b: u64) -> String {
    const U: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} bytes")
    } else if v >= 100.0 {
        format!("{v:.0} {}", U[i])
    } else {
        format!("{v:.1} {}", U[i])
    }
}

/// FILETIME (UTC) -> "10/30/2026 5:51 AM" in the user's locale and time zone.
pub fn date(ft: u64) -> String {
    if ft == 0 {
        return String::new();
    }
    unsafe {
        let f = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        let mut local: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&f, &mut utc) == 0 || SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return String::new();
        }
        let mut d = [0u16; 64];
        let mut t = [0u16; 64];
        let dn = GetDateFormatEx(std::ptr::null(), DATE_SHORTDATE, &local, std::ptr::null(), d.as_mut_ptr(), 64, std::ptr::null());
        let tn = GetTimeFormatEx(std::ptr::null(), TIME_NOSECONDS, &local, std::ptr::null(), t.as_mut_ptr(), 64);
        if dn <= 0 || tn <= 0 {
            return String::new();
        }
        format!("{} {}", from_wide(&d), from_wide(&t))
    }
}

/// Full path of a result row: "<folder>\<name>", or "C:\" for a volume root.
pub fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        format!("{name}\\")
    } else {
        format!("{dir}\\{name}")
    }
}

#[inline]
pub fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

#[inline]
pub fn rgb(r: u8, g: u8, b: u8) -> u32 {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(thousands(1407908), "1,407,908");
        assert_eq!(thousands(999), "999");
        assert_eq!(size_kb(1), "1 KB");
        assert_eq!(size_kb(85_899_345_920), "83,886,080 KB");
        assert_eq!(human_bytes(535 * (1 << 30)), "535 GB");
        assert_eq!(join_path("", "C:"), "C:\\");
        assert_eq!(join_path("C:", "pagefile.sys"), "C:\\pagefile.sys");
    }
}
