//! El binario del broker de red.
//!
//! Arma el socket en
//! `$XDG_RUNTIME_DIR/vasak-assistant/vasak-assistant-broker.sock` (modo 0600) y
//! delega el servicio en [`vasak_assistant_broker::server::run`]. La lógica
//! (estado, despacho, bucle de aceptación) vive en la biblioteca para que tenga
//! pruebas y se mida su cobertura; acá sólo queda el dueño del socket.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use tokio::net::UnixListener;
use tracing::info;

use vasak_assistant_broker::server::{run, State};
use vasak_assistant_broker::{config, http, logging};
use vasak_assistant_ipc::socket_path;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init();

    let settings = config::read_settings();
    let client = http::client()?;
    let state = Arc::new(State::from_settings(client, &settings));

    let path =
        socket_path().ok_or("sin XDG_RUNTIME_DIR: no hay dónde abrir el socket del broker")?;
    // El subdirectorio lo crea systemd con `RuntimeDirectory=vasak-assistant`
    // (escribible aun con `ProtectHome=read-only`); lo creamos igual por si el
    // broker corre fuera de systemd. El error real, si lo hubiera, lo da el bind.
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Un socket de un arranque anterior impide el bind. Borrarlo es seguro:
    // systemd corre una sola instancia del broker (la unit no es templada), así
    // que no hay otro broker vivo cuyo socket se pudiera estar pisando.
    let _ = tokio::fs::remove_file(&path).await;
    let listener = UnixListener::bind(&path)?;
    // Sólo el usuario: el socket encamina pedidos al modelo con su clave.
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).await?;
    info!(socket = %path.display(), "broker de red escuchando");

    run(listener, state).await?;

    info!("saliendo");
    let _ = tokio::fs::remove_file(&path).await;
    Ok(())
}
