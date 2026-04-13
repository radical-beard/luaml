use clap::Parser;

/// Standalone luaml script engine service.
///
/// Exposes a JSON-RPC 2.0 interface over TCP or Unix sockets.
/// Consumers connect, register scripts and API namespaces, then
/// dispatch events. When a Lua script calls an API function, the
/// service sends an `api_call` request back to the consumer and
/// waits for the response.
#[derive(Parser)]
#[command(name = "luaml-service", version)]
struct Cli {
    /// Listen address.
    ///
    /// Formats: tcp:HOST:PORT, unix:PATH, or HOST:PORT (defaults to TCP).
    #[arg(short, long, default_value = "tcp:127.0.0.1:9100")]
    listen: String,
}

fn main() {
    let cli = Cli::parse();

    let addr = match luaml_service::server::ListenAddr::parse(&cli.listen) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("invalid listen address: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = luaml_service::server::serve(&addr) {
        eprintln!("server error: {e}");
        std::process::exit(1);
    }
}
