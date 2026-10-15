//! `include/push_sdk.h` is generated from the crate by cbindgen and checked in, so C apps
//! need no Rust tooling; this test fails when it no longer matches the code. With
//! `PUSH_SDK_BLESS_HEADER=1` it writes the header instead.

use std::path::Path;

#[test]
fn the_checked_in_header_matches_the_code() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let config = cbindgen::Config::from_file(crate_dir.join("cbindgen.toml")).unwrap();
    let mut generated = Vec::new();
    cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
        .expect("cbindgen reads the crate")
        .write(&mut generated);
    let generated = String::from_utf8(generated).unwrap();
    let path = crate_dir.join("include").join("push_sdk.h");
    if std::env::var_os("PUSH_SDK_BLESS_HEADER").is_some() {
        std::fs::write(&path, &generated).unwrap();
        return;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == generated,
        "include/push_sdk.h is stale: regenerate it with PUSH_SDK_BLESS_HEADER=1 cargo test -p push-sdk-c --test header"
    );
}
