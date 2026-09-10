mod client;
mod daemon;
mod forge;
mod pilocal;
mod relay;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // daemon entry: `ranch daemon` (subcommand), `ranchd` argv[0], or --daemon
    let argv0 = args.first().and_then(|a| a.rsplit('/').next()).unwrap_or("ranch");
    if argv0 == "ranchd"
        || args.get(1).map(|a| a == "daemon").unwrap_or(false)
        || args.iter().any(|a| a == "--daemon")
    {
        // drop the `daemon` subcommand arg before handing off
        let is_sub = args.get(1).map(|a| a == "daemon").unwrap_or(false);
        if is_sub {
            daemon::run_daemon_with_args(args);
        } else {
            daemon::run_daemon();
        }
        return;
    }
    // client: pass through (client expects args AFTER the program name)
    client::main_client();
}
