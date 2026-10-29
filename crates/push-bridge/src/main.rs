//! `push-agent-bridge`: the agent bridge's binary. The commands come in a later change.

const USAGE: &str = "usage: push-agent-bridge [--dir <folder>] <keygen | thumbprint | enroll --code <code> | run | status | help>";

fn main() {
    println!("{USAGE}");
}
