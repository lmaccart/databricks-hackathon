//! Baseline x86-64 launcher for the paired feature-build experiment.
//! SS_EXEC_DIR is a build-time path; deployments use the fixed libexec path.
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

fn eligible() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        is_x86_feature_detected!("sse3")
            && is_x86_feature_detected!("ssse3")
            && is_x86_feature_detected!("sse4.1")
            && is_x86_feature_detected!("sse4.2")
            && is_x86_feature_detected!("popcnt")
            && is_x86_feature_detected!("avx")
            && is_x86_feature_detected!("avx2")
            && is_x86_feature_detected!("bmi1")
            && is_x86_feature_detected!("bmi2")
            && is_x86_feature_detected!("aes")
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

fn main() {
    let dir =
        Path::new(option_env!("SS_EXEC_DIR").unwrap_or("/usr/local/libexec/shopping_sessions"));
    let run = |name: &str| {
        Command::new(dir.join(name))
            .args(std::env::args_os().skip(1))
            .exec()
    };
    if eligible() && std::env::var_os("SS_ISA").as_deref() != Some(std::ffi::OsStr::new("base")) {
        let _ = run("isa");
    }
    let e = run("base");
    eprintln!("shopping_sessions: cannot execute baseline: {e}");
    std::process::exit(127);
}
