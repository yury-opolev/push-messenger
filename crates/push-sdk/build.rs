//! Compiles SQLite3 Multiple Ciphers, the SQLite that can encrypt the whole database file,
//! into a static library named `sqlite3` that `libsqlite3-sys` (not `bundled`) links to.
//! Only a C compiler is needed: MSVC on Windows, gcc or clang elsewhere.
//!
//! The amalgamation is vendored in `vendor/sqlite3mc`, byte for byte as released upstream;
//! `scripts/update-sqlite3mc.ps1` (or `.sh`) verifies a release and replaces it. Every
//! build first checks each vendored file against the SHA-256 recorded in `VERSION`, so a
//! changed byte fails the build.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const SOURCE_FILE: &str = "sqlite3mc_amalgamation.c";
const REQUIRED_FILES: [&str; 3] = [SOURCE_FILE, "sqlite3mc_amalgamation.h", "LICENSE"];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let source = manifest.join("vendor").join("sqlite3mc");
    println!("cargo:rerun-if-changed={}", source.display());
    verify_vendored(&source);

    let mut build = cc::Build::new();
    build
        .file(source.join(SOURCE_FILE))
        .include(&source)
        .define("SQLITE_CORE", None)
        .define("SQLITE_THREADSAFE", "1")
        .define("SQLITE_DEFAULT_FOREIGN_KEYS", "1")
        .define("SQLITE_DEFAULT_MEMSTATUS", "0")
        .define("SQLITE_DQS", "0")
        .define("SQLITE_ENABLE_API_ARMOR", None)
        .define("SQLITE_OMIT_DEPRECATED", None)
        .define("SQLITE_OMIT_LOAD_EXTENSION", None)
        .define("SQLITE_USE_URI", "1")
        // 3 is always memory: no PRAGMA can send temporary data to a plain file.
        .define("SQLITE_TEMP_STORE", "3")
        .define("SQLITE_SECURE_DELETE", None)
        .define("HAVE_ISNAN", None)
        .warnings(false)
        // libsqlite3-sys asks the linker for `sqlite3`; only the search path is given.
        .cargo_metadata(false);
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        build
            .define("HAVE_USLEEP", "1")
            .define("HAVE_LOCALTIME_R", None);
    }
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
        && env::var("CARGO_CFG_TARGET_FEATURE")
            .is_ok_and(|features| features.contains("crt-static"))
    {
        build.static_crt(true);
    }
    build.compile("sqlite3");
    let out = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    println!("cargo:rustc-link-search=native={}", out.display());
}

/// Panics, failing the build, unless every `file:` line of `VERSION` names a vendored file
/// with exactly that SHA-256, and the source, the header and the license are among them.
fn verify_vendored(source: &Path) {
    let version_path = source.join("VERSION");
    let version = fs::read_to_string(&version_path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", version_path.display()));
    let mut checked = Vec::new();
    for line in version.lines() {
        let Some(entry) = line.strip_prefix("file:") else {
            continue;
        };
        let (expected, name) = entry
            .trim()
            .split_once("  ")
            .unwrap_or_else(|| panic!("malformed line in VERSION: {line}"));
        let name = name.trim();
        assert!(
            !name.contains(['/', '\\']) && name != "." && name != "..",
            "VERSION names a file outside vendor/sqlite3mc: {name}"
        );
        let bytes = fs::read(source.join(name))
            .unwrap_or_else(|error| panic!("cannot read vendor/sqlite3mc/{name}: {error}"));
        let actual = hex(&Sha256::digest(&bytes));
        assert!(
            actual.eq_ignore_ascii_case(expected),
            "vendor/sqlite3mc/{name} has SHA-256 {actual}, but VERSION pins {expected}: \
             the vendored SQLite3 Multiple Ciphers was changed; restore it or run \
             scripts/update-sqlite3mc.ps1"
        );
        checked.push(name.to_owned());
    }
    for required in REQUIRED_FILES {
        assert!(
            checked.iter().any(|name| name == required),
            "VERSION pins no SHA-256 for {required}"
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
