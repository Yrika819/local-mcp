// The Bubblewrap gate is Linux-only. Gating the module keeps it out of the
// build on other platforms rather than leaving it compiled but unused.
#[cfg(target_os = "linux")]
#[path = "../exec_ready.rs"]
mod exec_ready;

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../resource_limits.rs"]
mod resource_limits;

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../process_group.rs"]
mod process_group;

#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../process_blocking.rs"]
mod process_blocking;

#[cfg(target_os = "linux")]
#[path = "../bubblewrap_support.rs"]
mod bubblewrap_support;

#[cfg(target_os = "linux")]
use bubblewrap_support::{BubblewrapSupport, check as check_bubblewrap};

fn main() -> ! {
    #[cfg(target_os = "linux")]
    {
        if let Err(error) = process_group::normalize_child_signal_policy() {
            eprintln!("Linux sandbox could not establish child ownership policy: {error}");
            std::process::exit(126);
        }
        // The trusted parent already refuses to spawn this helper when the
        // ambient Bubblewrap is unusable, and it owns the sandbox lifecycle
        // decision. This gate is deliberately retained: the helper must not
        // depend on its caller having checked, and the two call sites share one
        // implementation of the minimum supported version.
        //
        // It runs before the outer setup path so an unsupported version fails
        // before the model-authored command can start.
        let is_inner_stage = std::env::args_os()
            .skip(1)
            .take_while(|arg| arg != "--")
            .any(|arg| arg == "--apply-seccomp-then-exec");
        if !is_inner_stage {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let support = check_bubblewrap(&path);
            if support != BubblewrapSupport::Supported {
                eprintln!(
                    "Linux sandbox setup rejected: {}",
                    bubblewrap_support::rejection_message(support)
                );
                std::process::exit(126);
            }
        }
        codex_linux_sandbox::run_main();
    }

    #[cfg(not(target_os = "linux"))]
    panic!("codex-linux-sandbox is only supported on Linux");
}
