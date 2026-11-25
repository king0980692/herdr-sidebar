//! herdr-sidebar — the VS Code sidebar for herdr. The program lives in the
//! library (`herdr_sidebar::cli`) so a host such as herdr-native can run it
//! as one of its own commands.

fn main() -> std::io::Result<()> {
    herdr_sidebar::cli::main_with_args(std::env::args().skip(1).collect())
}
