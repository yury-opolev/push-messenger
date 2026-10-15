//! Compiles SQLite3 Multiple Ciphers, the SQLite that can encrypt the whole database file,
//! into a static library named `sqlite3` that `libsqlite3-sys` (not `bundled`) links to.
//! Only a C compiler is needed: MSVC on Windows, gcc or clang elsewhere.

use std::env;
use std::path::PathBuf;

fn main() {
    let source = sqlite3mc_src::source_dir();
    println!("cargo:rerun-if-changed={}", source.display());
    let mut build = cc::Build::new();
    build
        .file(source.join(sqlite3mc_src::SOURCE_FILE))
        .include(source)
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
