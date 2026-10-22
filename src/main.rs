fn main() {
    // Installed as berthd too: that name runs the daemon.
    let argv0 = std::env::args().next().unwrap_or_default();
    let result =
        if argv0.rsplit('/').next() == Some("berthd") { berth::daemon::main().map(|_| 0) } else { berth::cli::main() };
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("berth: {e:#}");
            std::process::exit(1);
        }
    }
}
