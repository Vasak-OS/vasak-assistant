//! La interfaz D-Bus: lo que el escritorio, Configuración, la terminal y las
//! aplicaciones pueden pedirle al asistente.
//!
//! Los nombres de los métodos en el bus son los de Rust en PascalCase
//! (`new_conversation` → `NewConversation`), y están declarados una sola vez,
//! igual, en el proxy del crate `vasak-assistant-protocol`.
//!
//! El contenido de las conversaciones nunca va al registro.

use std::sync::Arc;

use parking_lot::Mutex;
use zbus::interface;

use vasak_assistant_protocol::{Capability, Error, Message, ModelInfo, ServiceStatus, DOMAINS};

use crate::backend::{BackendError, ModelBackend};
use crate::conversations::Conversations;

pub struct AssistantService {
    pub backend: Arc<dyn ModelBackend>,
    pub conversations: Arc<Mutex<Conversations>>,
}

#[interface(name = "ar.net.vasak.os.Assistant")]
impl AssistantService {
    /// Cómo está el servicio: si hay modelo, cuál, y por qué no si no.
    async fn status(&self) -> ServiceStatus {
        let conversations = u32::try_from(self.conversations.lock().len()).unwrap_or(u32::MAX);
        match self.backend.active_model().await {
            Ok(model) => ServiceStatus {
                backend_available: true,
                reason: String::new(),
                model,
                conversations,
            },
            Err(error) => ServiceStatus {
                backend_available: false,
                reason: reason(&error).to_owned(),
                model: ModelInfo {
                    provider: self.backend.provider().to_owned(),
                    name: String::new(),
                    local: self.backend.is_local(),
                },
                conversations,
            },
        }
    }

    /// Las capacidades sobre cada dominio. En esta fase ninguna está: se dicen
    /// no disponibles, nunca rotas.
    fn capabilities(&self) -> Vec<Capability> {
        unavailable_capabilities()
    }

    /// Los modelos instalados en el backend.
    async fn models(&self) -> Result<Vec<String>, Error> {
        self.backend.models().await.map_err(to_dbus)
    }

    /// Abre una conversación nueva y devuelve su id.
    fn new_conversation(&self) -> Result<String, Error> {
        self.conversations.lock().create()
    }

    /// Las conversaciones abiertas.
    fn conversations(&self) -> Vec<String> {
        self.conversations.lock().ids()
    }

    /// Los mensajes de una conversación.
    fn history(&self, conversation_id: &str) -> Result<Vec<Message>, Error> {
        self.conversations.lock().history(conversation_id)
    }

    /// Manda un mensaje y devuelve la respuesta del modelo.
    async fn chat(&self, conversation_id: &str, content: &str) -> Result<String, Error> {
        let context = self
            .conversations
            .lock()
            .begin_turn(conversation_id, content)?;
        let mut turn = Turn {
            conversations: &self.conversations,
            id: conversation_id,
            content,
            reply: None,
        };
        let reply = self.backend.chat(&context).await.map_err(to_dbus)?;
        turn.reply = Some(reply.clone());
        Ok(reply)
    }

    /// Cierra una conversación y olvida sus mensajes.
    fn delete_conversation(&self, conversation_id: &str) -> Result<(), Error> {
        self.conversations.lock().delete(conversation_id)
    }
}

/// Cierra el turno pase lo que pase: con la respuesta si la hubo, y si no —un
/// error del modelo, o el futuro abandonado a mitad de camino— liberando la
/// conversación, para que no quede ocupada para siempre.
struct Turn<'a> {
    conversations: &'a Mutex<Conversations>,
    id: &'a str,
    content: &'a str,
    reply: Option<String>,
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.conversations
            .lock()
            .finish_turn(self.id, self.content, self.reply.as_deref());
    }
}

/// Todas las capacidades de [`DOMAINS`], no disponibles.
pub fn unavailable_capabilities() -> Vec<Capability> {
    DOMAINS
        .iter()
        .map(|domain| Capability {
            domain: (*domain).to_owned(),
            available: false,
            reason: "Todavía no está disponible en esta versión del asistente.".into(),
        })
        .collect()
}

fn reason(error: &BackendError) -> &str {
    match error {
        BackendError::Unavailable(text) | BackendError::Failed(text) => text,
    }
}

pub fn to_dbus(error: BackendError) -> Error {
    match error {
        BackendError::Unavailable(text) => Error::Unavailable(text),
        BackendError::Failed(text) => Error::Backend(text),
    }
}
