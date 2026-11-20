//! El cliente del puente de red: implementa [`ModelBackend`] conectándose, por
//! pedido, al socket del broker.
//!
//! El demonio no puede abrir un socket IP (su unit lo deja con
//! `RestrictAddressFamilies=AF_UNIX`). Para hablar con el modelo serializa el
//! método del rasgo y se lo manda al broker por el socket; el broker, el único
//! con la dirección, decide el destino. **El pedido nunca nombra un host**: la
//! única cosa que viaja aparte es la clave del proveedor, que el demonio lee del
//! llavero (su dueño natural) y mete en el sobre sólo cuando el backend es
//! OpenAI.
//!
//! Ante un socket caído o un broker que falla, todo da
//! [`BackendError::Unavailable`]: el servicio ya lo convierte en «modelo no
//! disponible» ([`crate::service::to_dbus`]), nunca en un crash (decisión 2).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixStream;

use vasak_assistant_ipc::{read_frame, write_frame, BrokerEnvelope, BrokerRequest, BrokerResponse};
use vasak_assistant_protocol::{Message, ModelInfo};

use crate::backend::{BackendError, BoxFuture, ChatMessage, ModelBackend, ModelTurn, ToolDef};
use crate::config::{BackendKind, Selection};
use crate::keyring::SecretSource;

/// El plazo del intercambio con el broker. Cubre `CHAT_TIMEOUT` (300 s, el tope
/// de una respuesta del modelo en el broker) con margen: si el broker queda
/// conectado pero no responde, el `read` no vuelve nunca y el permiso de
/// `Limited` se retendría. Al expirar es «puente no disponible»; no se reintenta,
/// porque el pedido ya pudo haber salido hacia el modelo.
const IPC_TIMEOUT: Duration = Duration::from_secs(330);

/// El puente al broker. `info()` sale de la propia configuración (sin ir al
/// broker); todo lo demás es un viaje de ida y vuelta por el socket.
pub struct BrokerClient {
    /// La configuración propia del demonio: para `info()` y para saber si el
    /// backend pide clave (OpenAI) y por qué dirección leerla.
    selection: Selection,
    /// El llavero: la clave del proveedor la lee el demonio y la pasa en el
    /// sobre. (PR2 mueve esta lectura al broker.)
    secrets: Arc<dyn SecretSource>,
    /// El socket del broker (`$XDG_RUNTIME_DIR/vasak-assistant-broker.sock`).
    socket: PathBuf,
}

impl BrokerClient {
    pub fn new(selection: Selection, secrets: Arc<dyn SecretSource>, socket: PathBuf) -> Self {
        Self {
            selection,
            secrets,
            socket,
        }
    }

    /// La clave a mandar en el sobre y la dirección para la que se resolvió: sólo
    /// para OpenAI, leída por la dirección base ya normalizada (la misma con que
    /// se guardó). El broker compara esa dirección con la suya antes de usar la
    /// clave, así la clave del proveedor nuevo no viaja a la URL del viejo si la
    /// config cambió y reinició uno solo de los dos procesos.
    async fn envelope_key(&self) -> (Option<String>, Option<String>) {
        if self.selection.kind != BackendKind::OpenAi {
            return (None, None);
        }
        let key = self
            .secrets
            .api_key(self.selection.url.as_str())
            .await
            .map(|key| key.expose().to_owned());
        let base = key.as_ref().map(|_| self.selection.url.as_str().to_owned());
        (key, base)
    }

    /// Un viaje de ida y vuelta: conecta, manda el sobre, devuelve la respuesta.
    /// Cualquier falla de E/S es el puente caído. El intercambio entero va con un
    /// plazo ([`IPC_TIMEOUT`]): un broker conectado que no responde no cuelga la
    /// llamada ni retiene el permiso de `Limited`.
    async fn request(&self, request: BrokerRequest) -> Result<BrokerResponse, BackendError> {
        let (api_key, key_base_url) = self.envelope_key().await;
        let envelope = BrokerEnvelope {
            api_key,
            key_base_url,
            request,
        };
        let exchange = async {
            let mut stream = UnixStream::connect(&self.socket)
                .await
                .map_err(|_| bridge_down())?;
            write_frame(&mut stream, &envelope)
                .await
                .map_err(|_| bridge_down())?;
            read_frame(&mut stream).await.map_err(|_| bridge_down())
        };
        match tokio::time::timeout(IPC_TIMEOUT, exchange).await {
            Ok(result) => result,
            // No se reintenta: el pedido ya pudo haber llegado al modelo.
            Err(_elapsed) => Err(bridge_down()),
        }
    }
}

/// El puente no está: el broker no corre, o el socket no responde. El servicio
/// lo muestra como «modelo no disponible».
fn bridge_down() -> BackendError {
    BackendError::Unavailable(
        "El puente de red del asistente no está disponible: el modelo no se puede usar ahora."
            .into(),
    )
}

/// La respuesta llegó, pero no es la variante que este método esperaba: se trata
/// como puente caído (nunca roto).
fn mismatched() -> BackendError {
    bridge_down()
}

impl ModelBackend for BrokerClient {
    /// Síncrono: sale de la `Selection` propia (local según la configuración). El
    /// `info().local` puede no reflejar un modelo que en runtime resultó `:cloud`
    /// (cosmético); la localidad **dura** la decide el `chat_local_only` del
    /// broker, y la efectiva se consulta por `active_model`.
    fn info(&self) -> ModelInfo {
        self.selection.info()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(async move {
            match self.request(BrokerRequest::Models).await? {
                BrokerResponse::Models(result) => result,
                _ => Err(mismatched()),
            }
        })
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move {
            match self.request(BrokerRequest::ActiveModel).await? {
                BrokerResponse::ActiveModel(result) => result,
                _ => Err(mismatched()),
            }
        })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        let messages = messages.to_vec();
        Box::pin(async move {
            match self.request(BrokerRequest::Chat { messages }).await? {
                BrokerResponse::Text(result) => result,
                _ => Err(mismatched()),
            }
        })
    }

    fn chat_local_only<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        let messages = messages.to_vec();
        Box::pin(async move {
            match self
                .request(BrokerRequest::ChatLocalOnly { messages })
                .await?
            {
                BrokerResponse::Text(result) => result,
                _ => Err(mismatched()),
            }
        })
    }

    /// Ambos backends del broker aceptan herramientas; el que no sabe usarlas
    /// responde texto (nunca roto). Síncrono: no va al broker.
    fn supports_tools(&self) -> bool {
        matches!(
            self.selection.kind,
            BackendKind::Ollama | BackendKind::OpenAi
        )
    }

    fn active_supports_tools(&self) -> BoxFuture<'_, Result<bool, BackendError>> {
        Box::pin(async move {
            match self.request(BrokerRequest::ActiveSupportsTools).await? {
                BrokerResponse::Bool(result) => result,
                _ => Err(mismatched()),
            }
        })
    }

    fn chat_tools<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolDef],
    ) -> BoxFuture<'a, Result<ModelTurn, BackendError>> {
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            match self
                .request(BrokerRequest::ChatTools { messages, tools })
                .await?
            {
                BrokerResponse::Turn(result) => result,
                _ => Err(mismatched()),
            }
        })
    }

    fn chat_tools_local_only<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolDef],
    ) -> BoxFuture<'a, Result<ModelTurn, BackendError>> {
        let messages = messages.to_vec();
        let tools = tools.to_vec();
        Box::pin(async move {
            match self
                .request(BrokerRequest::ChatToolsLocalOnly { messages, tools })
                .await?
            {
                BrokerResponse::Turn(result) => result,
                _ => Err(mismatched()),
            }
        })
    }
}

#[cfg(test)]
#[path = "broker_client_tests.rs"]
mod tests;
