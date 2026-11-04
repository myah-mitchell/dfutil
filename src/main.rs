use clap::Parser;

fn main() {
    let cli = dfapp::cli::Cli::parse();
    match dfapp::cli::run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("dfapp: {error:#}");
            std::process::exit(2);
        }
    }
}
