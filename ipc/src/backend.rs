//! La interfaz común de los backends de modelo.
//!
//! El motor de conversación y la interfaz D-Bus sólo conocen este rasgo. Lo
//! implementan Ollama y cualquier servidor compatible con la API de OpenAI (en
//! el broker), el cliente IPC (en el demonio) y `RejectedBackend` (en el
//! demonio), que es lo que queda cuando la configuración no se puede usar. Las
//! pruebas pueden poner uno de mentira.
//!
//! Vive en el crate compartido porque el demonio (que llama al rasgo) y el
//! broker (que lo implementa sobre HTTP) tienen que hablar exactamente el mismo
//! contrato. Ninguna de sus firmas lleva URL ni host: el único que puede
//! nombrar un destino es el broker, desde su propia configuración.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use vasak_assistant_protocol::{Message, ModelInfo};

/// Un futuro en caja: el rasgo se usa como `dyn`, y un `async fn` en un rasgo
/// no se puede.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Por qué el backend no respondió.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendError {
    /// No hay con quién hablar: el proveedor no corre, no tiene el modelo, pide
    /// una clave que no hay, o la configuración no se puede usar. El texto es
    /// para la persona.
    Unavailable(String),
    /// Respondió, pero algo que no sirve: un estado HTTP de error, un cuerpo
    /// ilegible o demasiado grande, o tardó más de la cuenta.
    Failed(String),
    /// El modelo responde, pero la configuración no deja usarlo: el modelo por
    /// omisión de Ollama corre en la nube y no hay `allow_remote`. El texto es
    /// para la persona.
    NotAllowed(String),
    /// El rechazo de [`ModelBackend::chat_local_only`]: el modelo que
    /// respondería no es local, y no se mandó nada. El texto es [`NOT_LOCAL`].
    NotLocal,
    /// Como [`Self::NotLocal`], pero ya se mandó: la respuesta del chat dijo
    /// que la respondió un modelo remoto (Ollama, `remote_host`) y se
    /// descartó. El texto es [`REMOTE_AFTER_SEND`].
    RemoteAfterSend,
    /// El cupo de `models` está lleno. El servicio lo devuelve como `Busy`; el
    /// texto es [`METADATA_BUSY`].
    Busy,
}

impl BackendError {
    /// El texto para la persona.
    pub fn text(&self) -> &str {
        match self {
            Self::Unavailable(text) | Self::Failed(text) | Self::NotAllowed(text) => text,
            Self::NotLocal => NOT_LOCAL,
            Self::RemoteAfterSend => REMOTE_AFTER_SEND,
            Self::Busy => METADATA_BUSY,
        }
    }
}

// ── Las herramientas (tool calling) ─────────────────────────────────────────────
//
// El modelo puede pedir que se ejecute una herramienta (leer el correo,
// redactar un envío) en vez de contestar con texto. Estos tipos son el contrato
// entre el motor de conversación y los backends que lo admiten (Ollama). Un
// backend que no lo admite recae en [`ModelBackend::chat`] por el método con
// `default` más abajo.

/// La definición de una herramienta que se le ofrece al modelo: su nombre, para
/// qué sirve y el esquema JSON de sus argumentos.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Una llamada a una herramienta que pidió el modelo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// El identificador de la llamada, para aparear el resultado. Si el backend
    /// no lo da (Ollama no lo manda), el backend genera uno.
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Lo que devuelve un turno del modelo: o texto, o un pedido de ejecutar
/// herramientas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ModelTurn {
    Text(String),
    Calls(Vec<ToolCall>),
}

/// Un mensaje del hilo que ve el modelo cuando hay herramientas. A diferencia de
/// [`Message`] distingue el rol `tool` (el resultado de una herramienta) y deja
/// que el mensaje del asistente lleve las llamadas que pidió.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ChatMessage {
    System(String),
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    Tool { id: String, content: String },
}

/// Aplana el hilo con herramientas a [`Message`] para los backends que no las
/// admiten: el resultado de una herramienta entra como un turno del usuario
/// (no hay rol `tool` en [`Message`]).
pub fn flatten_messages(messages: &[ChatMessage]) -> Vec<Message> {
    use vasak_assistant_protocol::Role;
    messages
        .iter()
        .map(|message| match message {
            ChatMessage::System(content) => Message {
                role: Role::System,
                content: content.clone(),
            },
            ChatMessage::User(content) => Message {
                role: Role::User,
                content: content.clone(),
            },
            ChatMessage::Assistant { text, .. } => Message {
                role: Role::Assistant,
                content: text.clone(),
            },
            ChatMessage::Tool { content, .. } => Message {
                role: Role::User,
                content: content.clone(),
            },
        })
        .collect()
}

pub trait ModelBackend: Send + Sync {
    /// Lo que se sabe sin preguntarle nada a nadie: proveedor, host, si es
    /// local, y el modelo configurado (vacío si se elige solo).
    fn info(&self) -> ModelInfo;

    /// Los modelos que el proveedor tiene. Sirve también para saber si está.
    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>>;

    /// El modelo con el que se va a responder: el configurado o, si no hay, uno
    /// que el proveedor tenga.
    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>>;

    /// La respuesta del modelo a la conversación, que termina con el mensaje de
    /// la persona.
    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>>;

    /// Como [`Self::chat`], pero sólo si el modelo que responde es local: lo
    /// decide la misma resolución que manda el pedido, así nada que haya
    /// pasado la puerta de la memoria sale por un cambio de modelo en el
    /// medio. Si no es local, o no se pudo saber, contesta
    /// [`BackendError::NotLocal`] sin mandar nada. Por omisión decide
    /// [`Self::info`], que vale para un backend cuya dirección y modelo son
    /// fijos (el de OpenAI); el de Ollama lo resuelve por pedido.
    fn chat_local_only<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            if !self.info().local {
                return Err(BackendError::NotLocal);
            }
            self.chat(messages).await
        })
    }

    /// Si el backend admite herramientas. Por omisión no: recae en
    /// [`Self::chat`] e ignora las que se le ofrezcan.
    fn supports_tools(&self) -> bool {
        false
    }

    /// Si el **modelo efectivo** admite herramientas: no alcanza con que el
    /// backend las hable (Ollama), el modelo instalado tiene que tener la
    /// capacidad `tools`. Por omisión responde [`Self::supports_tools`], para
    /// el backend que no puede mirar el modelo; Ollama lo consulta por
    /// `/api/show`. `Err` si no se pudo saber (el modelo no responde): quien
    /// llama decide (cae en el camino de chat plano, nunca roto).
    fn active_supports_tools(&self) -> BoxFuture<'_, Result<bool, BackendError>> {
        let supports = self.supports_tools();
        Box::pin(async move { Ok(supports) })
    }

    /// Un turno del modelo con herramientas disponibles. Por omisión **ignora**
    /// las herramientas: aplana el hilo a [`Message`], llama a [`Self::chat`] y
    /// devuelve texto. Así un backend sin soporte (y `RejectedBackend`) sigue
    /// respondiendo, y el motor de conversación no las ofrece.
    fn chat_tools<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        _tools: &'a [ToolDef],
    ) -> BoxFuture<'a, Result<ModelTurn, BackendError>> {
        Box::pin(async move {
            let flat = flatten_messages(messages);
            self.chat(&flat).await.map(ModelTurn::Text)
        })
    }

    /// Como [`Self::chat_tools`], pero sólo si el modelo que responde es local:
    /// lo que vuelve de una herramienta (correo, por ejemplo) no sale de la
    /// máquina por un modelo remoto. Decide igual que [`Self::chat_local_only`].
    fn chat_tools_local_only<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolDef],
    ) -> BoxFuture<'a, Result<ModelTurn, BackendError>> {
        Box::pin(async move {
            if !self.info().local {
                return Err(BackendError::NotLocal);
            }
            self.chat_tools(messages, tools).await
        })
    }
}

/// El motivo de [`ModelBackend::chat_local_only`] cuando el modelo no es local.
pub const NOT_LOCAL: &str =
    "El modelo que respondería no es local: lo que viene de la memoria no se le manda.";

/// El motivo de [`BackendError::RemoteAfterSend`].
pub const REMOTE_AFTER_SEND: &str =
    "El modelo que respondió no era local: el pedido ya había salido y la respuesta se descartó.";

/// El motivo de `models` cuando el cupo de modelos está lleno.
pub const METADATA_BUSY: &str =
    "Hay demasiados pedidos de la lista de modelos a la vez: volvé a intentarlo en un momento.";

#[cfg(test)]
mod tests {
    use super::*;
    use vasak_assistant_protocol::Role;

    #[test]
    fn el_texto_de_cada_variante_es_el_que_le_toca() {
        for (error, text) in [
            (BackendError::Unavailable("a".into()), "a"),
            (BackendError::Failed("b".into()), "b"),
            (BackendError::NotAllowed("c".into()), "c"),
            (BackendError::NotLocal, NOT_LOCAL),
            (BackendError::RemoteAfterSend, REMOTE_AFTER_SEND),
            (BackendError::Busy, METADATA_BUSY),
        ] {
            assert_eq!(error.text(), text, "{error:?}");
        }
    }

    #[test]
    fn aplanar_el_hilo_pasa_el_resultado_de_una_herramienta_como_turno_del_usuario() {
        let thread = vec![
            ChatMessage::System("sistema".into()),
            ChatMessage::User("hola".into()),
            ChatMessage::Assistant {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "1".into(),
                    name: "search_mail".into(),
                    arguments: serde_json::json!({}),
                }],
            },
            ChatMessage::Tool {
                id: "1".into(),
                content: "[{\"id\":1}]".into(),
            },
        ];
        let flat = flatten_messages(&thread);
        let roles: Vec<_> = flat.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            [Role::System, Role::User, Role::Assistant, Role::User]
        );
        assert_eq!(flat[3].content, "[{\"id\":1}]");
    }
}
