mod cli;
mod dircache;
mod filters;
mod index;
mod messages;
mod plan;
mod search;
mod timing;
mod trigram;
mod walk;
#[cfg(target_os = "macos")]
mod walk_bulk;

/// Stack for the thread that runs the CLI. Windows gives the main thread
/// only 1 MiB, which the argument struct and the run path overflow in
/// debug builds; 8 MiB matches the Linux and macOS main-thread default.
const MAIN_STACK: usize = 8 * 1024 * 1024;

fn main() {
    let result = std::thread::Builder::new()
        .name("glep-main".into())
        .stack_size(MAIN_STACK)
        .spawn(cli::run)
        .expect("spawn the glep main thread")
        .join();
    let result = match result {
        Ok(r) => r,
        // A panic already printed its message; exit like an error.
        Err(_) => std::process::exit(2),
    };
    match result {
        Ok(code) => std::process::exit(code),
        // A reader that went away (`glep ... | head -1`) ends the run
        // quietly with success, as in rg. Rust ignores SIGPIPE, so a
        // closed pipe arrives as an EPIPE error instead of a signal.
        Err(e) if is_broken_pipe(&e) => std::process::exit(0),
        Err(e) => {
            eprintln!("glep: {e}");
            std::process::exit(2);
        }
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
    })
}
