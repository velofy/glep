mod cli;
mod index;
mod plan;
mod search;
mod timing;
mod trigram;
mod walk;

fn main() {
    match cli::run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("glep: {e}");
            std::process::exit(2);
        }
    }
}
