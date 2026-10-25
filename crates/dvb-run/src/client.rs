//! Blocking worker calls use the same Hyper transport as owner and desk calls.

use dvb_cli::client::{CliError, Client, Method, RUN_REQUEST_TIMEOUT, Reply};
use std::path::Path;

pub fn post(sock: &Path, path: &str, body: &str) -> Result<Reply, CliError> {
    // Provider sessions are synchronous, and the driver may already be in Tokio.
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(CliError::Connection)?
                    .block_on(Client::new(sock.to_owned()).raw(
                        Method::POST,
                        path,
                        body.as_bytes().to_vec(),
                        RUN_REQUEST_TIMEOUT,
                    ))
            })
            .join()
            .map_err(|_| CliError::Message("run socket thread panicked".into()))?
    })
}
