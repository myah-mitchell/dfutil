use clap::Parser;

fn main() {
    let cli = dfutil::cli::Cli::parse();
    match dfutil::cli::run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("dfutil: {error:#}");
            std::process::exit(2);
        }
    }
}
