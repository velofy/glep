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

fn main() {
    match cli::run() {
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
