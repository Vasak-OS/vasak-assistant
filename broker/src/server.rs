//! El servidor IPC: estado compartido, despacho de los pedidos y el bucle de
//! aceptación. Vive en la biblioteca (no en `main.rs`) para que tenga pruebas y
//! su cobertura se mida; `main.rs` sólo arma el socket y llama a [`run`].

use std::sync::Arc;
use std::time::Duration;

use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::config::{BackendKind, Rejection, Selection, Settings};
use crate::keyring::RequestKey;
use crate::ollama::OllamaBackend;
use crate::openai::OpenAiBackend;
use vasak_assistant_ipc::{
    read_frame, write_frame, BackendError, BrokerEnvelope, BrokerRequest, BrokerResponse,
    ModelBackend,
};

/// Cuánto se espera el marco del pedido antes de cerrar la conexión. Acota al
/// cliente que abre, manda el largo y se calla: sin esto, su task (y hasta
/// `MAX_FRAME_BYTES`) quedarían colgadas. No cubre la llamada al modelo, que
/// puede tardar: envuelve sólo la lectura del sobre, no `handle`.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Tope de conexiones a la vez. El cliente legítimo (el demonio) hace pocas; el
/// tope frena a un proceso del mismo usuario que intente abrir muchas de golpe.
const MAX_CONNECTIONS: usize = 32;

/// Lo que el broker decidió de la configuración al arrancar: el backend elegido
/// (con el permiso de remoto) o el rechazo, que se responde «no disponible».
pub enum Resolved {
    Ready {
        selection: Selection,
        allow_remote: bool,
    },
    Rejected(Rejection),
}

/// Lo que comparten todas las conexiones: el cliente HTTP (uno solo) y lo que se
/// resolvió de la configuración.
pub struct State {
    client: reqwest::Client,
    resolved: Resolved,
}

impl State {
    /// Arma el estado desde la configuración: elige el backend (y registra cuál)
    /// o guarda el rechazo para contestar «no disponible».
    pub fn from_settings(client: reqwest::Client, settings: &Settings) -> Self {
        let resolved = match crate::config::select_backend(settings) {
            Ok(selection) => {
                let info = selection.info();
                info!(
                    backend = info.provider.as_str(),
                    host = info.host.as_str(),
                    local = info.local,
                    "backend elegido"
                );
                Resolved::Ready {
                    selection,
                    allow_remote: settings.allow_remote,
                }
            }
            Err(rejection) => {
                warn!(
                    backend = rejection.info.provider.as_str(),
                    "backend no disponible: {}", rejection.reason
                );
                Resolved::Rejected(rejection)
            }
        };
        Self { client, resolved }
    }

    /// Para las pruebas: arma el estado con un `Resolved` ya decidido.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_resolved(client: reqwest::Client, resolved: Resolved) -> Self {
        Self { client, resolved }
    }

    /// El backend para un pedido. Se arma por pedido porque la clave de OpenAI
    /// llega en el sobre; la dirección sale siempre de la configuración.
    fn backend(&self, api_key: Option<String>) -> Option<Arc<dyn ModelBackend>> {
        let Resolved::Ready {
            selection,
            allow_remote,
        } = &self.resolved
        else {
            return None;
        };
        Some(match selection.kind {
            BackendKind::Ollama => Arc::new(OllamaBackend::new(
                self.client.clone(),
                selection.clone(),
                *allow_remote,
            )),
            BackendKind::OpenAi => Arc::new(OpenAiBackend::new(
                self.client.clone(),
                selection.clone(),
                Arc::new(RequestKey::new(api_key)),
            )),
        })
    }

    pub async fn handle(&self, request: BrokerRequest, api_key: Option<String>) -> BrokerResponse {
        match self.backend(api_key) {
            Some(backend) => dispatch(backend.as_ref(), request).await,
            None => {
                let Resolved::Rejected(rejection) = &self.resolved else {
                    unreachable!("sin backend sólo si la config se rechazó");
                };
                rejected_response(&request, rejection)
            }
        }
    }
}

/// Despacha el pedido al método del rasgo que le toca.
async fn dispatch(backend: &dyn ModelBackend, request: BrokerRequest) -> BrokerResponse {
    match request {
        BrokerRequest::Info => BrokerResponse::Info(backend.info()),
        BrokerRequest::Models => BrokerResponse::Models(backend.models().await),
        BrokerRequest::ActiveModel => BrokerResponse::ActiveModel(backend.active_model().await),
        BrokerRequest::SupportsTools => BrokerResponse::SupportsTools(backend.supports_tools()),
        BrokerRequest::ActiveSupportsTools => {
            BrokerResponse::Bool(backend.active_supports_tools().await)
        }
        BrokerRequest::Chat { messages } => BrokerResponse::Text(backend.chat(&messages).await),
        BrokerRequest::ChatLocalOnly { messages } => {
            BrokerResponse::Text(backend.chat_local_only(&messages).await)
        }
        BrokerRequest::ChatTools { messages, tools } => {
            BrokerResponse::Turn(backend.chat_tools(&messages, &tools).await)
        }
        BrokerRequest::ChatToolsLocalOnly { messages, tools } => {
            BrokerResponse::Turn(backend.chat_tools_local_only(&messages, &tools).await)
        }
    }
}

/// La respuesta cuando la configuración no se puede usar: «no disponible» con el
/// motivo, igual que el `RejectedBackend` del demonio. El demonio ya sirve este
/// caso de su lado; esto es la red de seguridad si igual llega un pedido.
fn rejected_response(request: &BrokerRequest, rejection: &Rejection) -> BrokerResponse {
    let unavailable = || BackendError::Unavailable(rejection.reason.clone());
    match request {
        BrokerRequest::Info => BrokerResponse::Info(rejection.info.clone()),
        BrokerRequest::Models => BrokerResponse::Models(Err(unavailable())),
        BrokerRequest::ActiveModel => BrokerResponse::ActiveModel(Err(unavailable())),
        BrokerRequest::SupportsTools => BrokerResponse::SupportsTools(false),
        BrokerRequest::ActiveSupportsTools => BrokerResponse::Bool(Err(unavailable())),
        BrokerRequest::Chat { .. } | BrokerRequest::ChatLocalOnly { .. } => {
            BrokerResponse::Text(Err(unavailable()))
        }
        BrokerRequest::ChatTools { .. } | BrokerRequest::ChatToolsLocalOnly { .. } => {
            BrokerResponse::Turn(Err(unavailable()))
        }
    }
}

/// Una conexión: un pedido y una respuesta. Un error de E/S acá es el demonio
/// cortando, o un sobre ilegible: la conexión se cierra sin más.
pub async fn serve(mut stream: UnixStream, state: &State) -> std::io::Result<()> {
    let envelope: BrokerEnvelope =
        match tokio::time::timeout(READ_TIMEOUT, read_frame(&mut stream)).await {
            Ok(result) => result?,
            // El cliente abrió y no mandó el marco a tiempo: se cierra sin dejar la
            // task colgada. La llamada al modelo, abajo, no tiene este plazo.
            Err(_elapsed) => return Ok(()),
        };
    let response = state.handle(envelope.request, envelope.api_key).await;
    write_frame(&mut stream, &response).await
}

/// El bucle de aceptación: una task por conexión, acotada por [`MAX_CONNECTIONS`],
/// hasta un SIGTERM/SIGINT. El dueño del socket (su creación y su borrado) es
/// `main`.
pub async fn run(listener: UnixListener, state: Arc<State>) -> std::io::Result<()> {
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        error!("no se pudo aceptar una conexión: {error}");
                        continue;
                    }
                };
                // Al tope, se descarta la conexión en vez de encolarla: así un
                // proceso del mismo usuario no acumula tasks ni fds.
                let Ok(permit) = limit.clone().try_acquire_owned() else {
                    debug!("demasiadas conexiones a la vez; se descarta una");
                    continue;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve(stream, &state).await {
                        debug!("la conexión del IPC terminó: {error}");
                    }
                });
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
