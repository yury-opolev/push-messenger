//! Compiles `tests/c/flow.c` against `include/push_sdk.h`, links it to the shared library
//! cargo built for these tests, and runs it against the SDK's fake auth-svc and gateway:
//! a C app signs in with its own secret store, connects, sends, reads events by polling and
//! through a callback, and opens an encrypted database, checking the error codes on the
//! way.

use std::path::{Path, PathBuf};
use std::process::Command;

use push_sdk::testing::{AutoGateway, FakeAuth};

/// target/<profile>, where cargo puts the shared library.
fn library_dir() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    test.parent().and_then(Path::parent).unwrap().to_path_buf()
}

/// Builds the shared library: cargo builds only the Rust library for integration tests.
fn build_library() {
    let library = library_dir();
    let mut command = Command::new(env!("CARGO"));
    command
        .args([
            "build",
            "--locked",
            "--lib",
            "-p",
            "push-sdk-c",
            "--target-dir",
        ])
        .arg(library.parent().unwrap());
    if library.ends_with("release") {
        command.arg("--release");
    }
    let built = command.output().expect("cargo runs");
    assert!(
        built.status.success(),
        "the shared library does not build:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
}

fn compile(scratch: &Path) -> PathBuf {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = env!("PUSH_SDK_C_TARGET");
    let compiler = cc::Build::new()
        .target(target)
        .host(target)
        .opt_level(0)
        .cargo_metadata(false)
        .cargo_warnings(false)
        .get_compiler();
    let source = crate_dir.join("tests").join("c").join("flow.c");
    let include = crate_dir.join("include");
    let library = library_dir();
    let program = scratch.join(if cfg!(windows) { "flow.exe" } else { "flow" });
    let mut command = compiler.to_command();
    if compiler.is_like_msvc() {
        command
            .arg("/nologo")
            .arg(format!("/I{}", include.display()))
            .arg(&source)
            .arg(format!("/Fo{}\\", scratch.display()))
            .arg(format!("/Fe{}", program.display()))
            .arg("/link")
            .arg(library.join("push_sdk_c.dll.lib"));
    } else {
        command
            .arg("-std=c99")
            .arg("-D_POSIX_C_SOURCE=200809L")
            .arg("-Wall")
            .arg(format!("-I{}", include.display()))
            .arg(&source)
            .arg("-o")
            .arg(&program)
            .arg(format!("-L{}", library.display()))
            .arg("-lpush_sdk_c")
            .arg(format!("-Wl,-rpath,{}", library.display()))
            .arg("-lpthread");
    }
    let compiled = command.output().expect("the C compiler runs");
    assert!(
        compiled.status.success(),
        "the C program does not compile:\n{}{}",
        String::from_utf8_lossy(&compiled.stdout),
        String::from_utf8_lossy(&compiled.stderr)
    );
    program
}

#[test]
fn a_c_program_signs_in_sends_and_reads_events() {
    let scratch = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("push-sdk-c-flow");
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).unwrap();
    build_library();
    let program = compile(&scratch);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (auth, gateway) = runtime.block_on(async {
        let auth = FakeAuth::start().await;
        let gateway = AutoGateway::start(auth.identity.clone()).await;
        (auth, gateway)
    });
    // The program finds the shared library next to it (Windows) or by its rpath.
    let mut path = std::ffi::OsString::from(library_dir());
    path.push(if cfg!(windows) { ";" } else { ":" });
    path.push(std::env::var_os("PATH").unwrap_or_default());
    // JSON in the program's config: forward slashes need no escaping.
    let scratch_arg = scratch.display().to_string().replace('\\', "/");
    let ran = Command::new(&program)
        .args([
            auth.url.as_str(),
            gateway.url.as_str(),
            scratch_arg.as_str(),
        ])
        .env("PATH", path)
        .output()
        .expect("the C program runs");
    let stdout = String::from_utf8_lossy(&ran.stdout);
    println!("{stdout}");
    assert!(
        ran.status.success() && stdout.contains("c flow: PASS"),
        "the C program failed:\n{stdout}{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    drop(runtime);
    let _ = std::fs::remove_dir_all(&scratch);
}
