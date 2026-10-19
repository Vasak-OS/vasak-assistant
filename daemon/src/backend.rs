//! La interfaz común de los backends de modelo.
//!
//! El motor de conversación y la interfaz D-Bus sólo conocen este rasgo: hoy lo
//! implementa Ollama ([`crate::ollama`]); un proveedor remoto (fase 2) será otra
//! implementación, y las pruebas pueden poner una de mentira.

use std::future::Future;
use std::pin::Pin;

use vasak_assistant_protocol::{Message, ModelInfo};

/// Un futuro en caja: el rasgo se usa como `dyn`, y un `async fn` en un rasgo
/// no se puede.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Por qué el backend no respondió.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendError {
    /// No hay con quién hablar: el proveedor no corre o no tiene el modelo. El
    /// texto es para la persona.
    Unavailable(String),
    /// Respondió, pero algo que no sirve: un estado HTTP de error, un cuerpo
    /// ilegible o demasiado grande, o tardó más de la cuenta.
    Failed(String),
}

pub trait ModelBackend: Send + Sync {
    /// El proveedor y si es local, sin preguntarle nada a nadie.
    fn provider(&self) -> &str;
    fn is_local(&self) -> bool;

    /// Los modelos que el proveedor tiene. Sirve también para saber si está.
    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>>;

    /// El modelo con el que se va a responder: el configurado o, si no hay, uno
    /// que el proveedor tenga.
    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>>;

    /// La respuesta del modelo a la conversación, que termina con el mensaje de
    /// la persona.
    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>>;
}
