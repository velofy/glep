mod cli;
mod index;
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
        Err(e) => {
            // rg parity: a closed stdout pipe (`| head`) is a clean exit.
            if e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            }) {
                std::process::exit(0);
            }
            eprintln!("glep: {e}");
            std::process::exit(2);
        }
    }
}
