//! The Windows file version of a program, from its version resource.

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
};

/// `VS_FIXEDFILEINFO.dwSignature`.
const FIXED_FILE_INFO_SIGNATURE: u32 = 0xFEEF_04BD;

/// The file version (`VS_FIXEDFILEINFO`) as `a.b.c`, with a fourth part only when it is
/// not 0; `None` when the file has no version resource.
pub fn file_version(path: &Path) -> Option<String> {
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut handle = 0u32;
    // SAFETY: `wide` is NUL-terminated and outlives the call; `handle` is a valid u32 to
    // write to.
    let size = unsafe { GetFileVersionInfoSizeW(wide.as_ptr(), &mut handle) };
    if size == 0 {
        return None;
    }
    let mut data = vec![0u8; usize::try_from(size).ok()?];
    // SAFETY: `wide` is NUL-terminated; `data` has exactly `size` writable bytes and
    // outlives the call.
    let read = unsafe { GetFileVersionInfoW(wide.as_ptr(), 0, size, data.as_mut_ptr().cast()) };
    if read == 0 {
        return None;
    }
    let root = [u16::from(b'\\'), 0];
    let mut info: *mut c_void = std::ptr::null_mut();
    let mut len = 0u32;
    // SAFETY: `data` holds the version block GetFileVersionInfoW filled; `root` is
    // NUL-terminated; `info` and `len` are valid places to write the answer to.
    let found = unsafe { VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut info, &mut len) };
    let big_enough =
        usize::try_from(len).is_ok_and(|len| len >= std::mem::size_of::<VS_FIXEDFILEINFO>());
    if found == 0 || info.is_null() || !big_enough {
        return None;
    }
    // SAFETY: VerQueryValueW pointed `info` into `data` (still alive and unchanged) at a
    // VS_FIXEDFILEINFO of at least its size; read_unaligned needs no alignment.
    let fixed = unsafe { info.cast::<VS_FIXEDFILEINFO>().read_unaligned() };
    if fixed.dwSignature != FIXED_FILE_INFO_SIGNATURE {
        return None;
    }
    let high = |part: u32| u16::try_from(part >> 16).unwrap_or(0);
    let low = |part: u32| u16::try_from(part & 0xFFFF).unwrap_or(0);
    Some(version_text([
        high(fixed.dwFileVersionMS),
        low(fixed.dwFileVersionMS),
        high(fixed.dwFileVersionLS),
        low(fixed.dwFileVersionLS),
    ]))
}

fn version_text(parts: [u16; 4]) -> String {
    let [major, minor, build, revision] = parts;
    if revision == 0 {
        format!("{major}.{minor}.{build}")
    } else {
        format!("{major}.{minor}.{build}.{revision}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TestDir;

    #[test]
    fn notepad_has_a_file_version() {
        let notepad = Path::new(r"C:\Windows\System32\notepad.exe");
        if !notepad.exists() {
            return;
        }
        let version = file_version(notepad).expect("a version");
        assert!(version.contains('.'), "{version}");
    }

    #[test]
    fn a_file_without_a_version_resource_has_none() {
        let dir = TestDir::new();
        let file = dir.path().join("plain.exe");
        std::fs::write(&file, b"no resources here").unwrap();
        assert_eq!(file_version(&file), None);
        assert_eq!(file_version(&dir.path().join("missing.exe")), None);
    }

    #[test]
    fn the_fourth_part_shows_only_when_set() {
        assert_eq!(version_text([0, 1, 176, 0]), "0.1.176");
        assert_eq!(version_text([10, 0, 22621, 4]), "10.0.22621.4");
        assert_eq!(version_text([1, 0, 0, 0]), "1.0.0");
    }
}
