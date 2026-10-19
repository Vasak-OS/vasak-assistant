//! La interfaz común de los backends de modelo.
//!
//! El motor de conversación y la interfaz D-Bus sólo conocen este rasgo. Lo
//! implementan Ollama ([`crate::ollama`]), cualquier servidor compatible con la
//! API de OpenAI ([`crate::openai`]) y [`RejectedBackend`], que es lo que queda
//! cuando la configuración no se puede usar. Las pruebas pueden poner uno de
//! mentira.

use std::future::Future;
use std::pin::Pin;

use vasak_assistant_protocol::{Message, ModelInfo};

/// Un futuro en caja: el rasgo se usa como `dyn`, y un `async fn` en un rasgo
/// no se puede.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Por qué el backend no respondió.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// No hay con quién hablar: el proveedor no corre, no tiene el modelo, pide
    /// una clave que no hay, o la configuración no se puede usar. El texto es
    /// para la persona.
    Unavailable(String),
    /// Respondió, pero algo que no sirve: un estado HTTP de error, un cuerpo
    /// ilegible o demasiado grande, o tardó más de la cuenta.
    Failed(String),
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
}

/// El backend que queda cuando la configuración no se puede usar (un remoto sin
/// permiso, una dirección inválida…): responde siempre «no disponible» con el
/// motivo, así el servicio sigue en pie y el estado lo explica.
pub struct RejectedBackend {
    pub info: ModelInfo,
    pub reason: String,
}

impl RejectedBackend {
    fn unavailable(&self) -> BackendError {
        BackendError::Unavailable(self.reason.clone())
    }
}

impl ModelBackend for RejectedBackend {
    fn info(&self) -> ModelInfo {
        self.info.clone()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(async move { Err(self.unavailable()) })
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move { Err(self.unavailable()) })
    }

    fn chat<'a>(&'a self, _: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move { Err(self.unavailable()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::model_info;

    fn rejected() -> RejectedBackend {
        RejectedBackend {
            info: model_info("gpt"),
            reason: "No se puede usar esta configuración.".into(),
        }
    }

    #[tokio::test]
    async fn el_backend_rechazado_contesta_no_disponible_con_el_motivo_a_todo() {
        let backend = rejected();
        let esperado = BackendError::Unavailable("No se puede usar esta configuración.".into());
        assert_eq!(backend.models().await.unwrap_err(), esperado);
        assert_eq!(backend.active_model().await.unwrap_err(), esperado);
        let mensaje = Message {
            role: vasak_assistant_protocol::Role::User,
            content: "hola".into(),
        };
        assert_eq!(backend.chat(&[mensaje]).await.unwrap_err(), esperado);
    }

    #[test]
    fn el_backend_rechazado_igual_dice_que_modelo_y_host_eran() {
        assert_eq!(rejected().info(), model_info("gpt"));
    }
}
