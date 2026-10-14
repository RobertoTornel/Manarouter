//! Standalone entry point for the detached mock upstream.

use std::net::SocketAddr;

use mock_upstream::{app, parse_args, Exit};

fn main() {
    let options = match parse_args(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(Exit::Help(message)) => {
            println!("{message}");
            return;
        }
        Err(Exit::Usage(message)) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("mock-upstream: cannot start runtime: {error}");
            std::process::exit(2);
        });

    runtime.block_on(async move {
        let address = SocketAddr::new(options.bind, options.port);
        let listener = tokio::net::TcpListener::bind(address)
            .await
            .unwrap_or_else(|error| {
                eprintln!("mock-upstream: cannot bind {address}: {error}");
                std::process::exit(2);
            });
        let bound = listener.local_addr().expect("listener has local address");
        println!("mock-upstream listening on {bound}");
        let _ = std::io::Write::flush(&mut std::io::stdout());

        if let Err(error) = axum::serve(listener, app(options)).await {
            eprintln!("mock-upstream: server stopped: {error}");
            std::process::exit(1);
        }
    });
}
