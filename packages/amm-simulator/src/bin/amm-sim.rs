fn main() {
    if let Err(err) = soroban_amm_simulator::run() {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
