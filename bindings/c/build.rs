fn main() {
    // tests/c_program.rs compiles a C program for the same target.
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo:rustc-env=PUSH_SDK_C_TARGET={target}");
}
