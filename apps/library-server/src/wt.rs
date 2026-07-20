//! WebTransport side of the server: self-signed endpoint setup and the
//! per-session accept/request loop. Each session receives datagram queries
//! and answers them on `App`, streaming each response back on its own uni
//! stream (see the module doc on `main` for the wire protocol).

use std::sync::Arc;

use anyhow::Result;
use library_core::Query;
use wtransport::endpoint::endpoint_side::Server;
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::App;

/// Builds the self-signed WebTransport endpoint and returns it along with
/// the certificate hash (served over HTTP so clients can pin it).
pub(crate) fn build_endpoint(port: u16) -> Result<(Endpoint<Server>, Vec<u8>)> {
    let identity = Identity::self_signed(["localhost", "127.0.0.1", "::1"])?;
    let cert_hash: Vec<u8> = identity.certificate_chain().as_slice()[0]
        .hash()
        .as_ref()
        .to_vec();

    let wt_config = ServerConfig::builder()
        .with_bind_default(port)
        .with_identity(identity)
        .build();
    let endpoint = Endpoint::server(wt_config)?;
    Ok((endpoint, cert_hash))
}

pub(crate) async fn serve_session(
    incoming: wtransport::endpoint::IncomingSession,
    app: Arc<App>,
) -> Result<()> {
    let request = incoming.await?;
    let conn = request.accept().await?;
    println!("session from {}", conn.remote_address());

    loop {
        let dgram = conn.receive_datagram().await?;
        let q: Query = match serde_json::from_slice(&dgram) {
            Ok(q) => q,
            Err(_) => continue,
        };

        let resp = {
            let app = app.clone();
            // embedding + search are sync; keep the event loop clean
            tokio::task::spawn_blocking(move || app.answer(&q)).await?
        };

        let mut stream = conn.open_uni().await?.await?;
        stream.write_all(&serde_json::to_vec(&resp)?).await?;
        stream.finish().await?;
    }
}
