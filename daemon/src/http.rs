//! Lo común a los backends que hablan HTTP: el cliente, los tiempos máximos y
//! la lectura con tope.
//!
//! Todo lo que llega del backend es entrada de afuera: cada pedido tiene su
//! tiempo máximo, el cuerpo se lee con un tope, no se siguen redirecciones (un
//! servidor no puede mandar la conversación, ni la clave, a otro lado) y no se
//! pasa por ningún proxy del entorno (`HTTP_PROXY` mandaría la conversación a
//! otra máquina sin que nadie lo haya elegido).

use std::time::Duration;

use reqwest::{redirect, Client, Response, Url};

use crate::backend::BackendError;

/// Cuánto se espera para abrir la conexión.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Cuánto se espera la lista de modelos.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// Cuánto se espera una respuesta del modelo. Generoso: un modelo local en una
/// máquina modesta, cargándose por primera vez, tarda.
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(300);
/// El tope del cuerpo de cualquier respuesta de un backend.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// El cliente que comparten todos los backends.
pub fn client() -> reqwest::Result<Client> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect::Policy::none())
        .no_proxy()
        .build()
}

/// `path` relativo a la dirección base, que termina siempre en `/` (ver
/// `config::select_backend`).
pub fn endpoint(base: &Url, path: &str) -> Result<Url, BackendError> {
    base.join(path)
        .map_err(|error| BackendError::Failed(format!("dirección inválida: {error}")))
}

pub fn map_error(base: &Url, error: reqwest::Error) -> BackendError {
    if error.is_connect() {
        BackendError::Unavailable(format!(
            "El modelo no responde en {}. ¿Está instalado y corriendo?",
            origin(base)
        ))
    } else if error.is_timeout() {
        BackendError::Failed("el modelo tardó demasiado en responder".into())
    } else {
        // Sin `{error}`: el texto de reqwest trae la dirección completa.
        BackendError::Failed(format!("error hablando con {}", origin(base)))
    }
}

/// Esquema, host y puerto: lo que se puede mostrar de una dirección.
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Lee el cuerpo entero, cortando en cuanto pasa [`MAX_RESPONSE_BYTES`]: un
/// servidor que manda de más no hace crecer la memoria del servicio.
pub async fn read_capped(base: &Url, mut response: Response) -> Result<Vec<u8>, BackendError> {
    let too_large = || {
        BackendError::Failed(format!(
            "la respuesta pasa el tope de {MAX_RESPONSE_BYTES} bytes"
        ))
    };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| map_error(base, error))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
