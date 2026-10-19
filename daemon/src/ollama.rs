//! El backend local: Ollama por HTTP en la propia máquina.
//!
//! Usa dos rutas de su API: `/api/tags` (qué modelos hay, y de paso si Ollama
//! está) y `/api/chat` sin streaming (la respuesta entera de una vez).
//!
//! Todo lo que llega de Ollama es entrada de afuera: cada pedido tiene su
//! tiempo máximo, el cuerpo se lee con un tope, no se siguen redirecciones y no
//! se pasa por ningún proxy del entorno (`HTTP_PROXY` mandaría la conversación a
//! otra máquina).

use std::time::Duration;

use reqwest::{redirect, Client, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use vasak_assistant_protocol::{Message, ModelInfo};

use crate::backend::{BackendError, BoxFuture, ModelBackend};

pub const PROVIDER: &str = "ollama";

/// Cuánto se espera para abrir la conexión. Es la máquina misma: si no
/// conecta enseguida, no está.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// Cuánto se espera la lista de modelos.
const TAGS_TIMEOUT: Duration = Duration::from_secs(5);
/// Cuánto se espera una respuesta del modelo. Generoso: un modelo local en una
/// máquina modesta, cargándose por primera vez, tarda.
const CHAT_TIMEOUT: Duration = Duration::from_secs(300);
/// El tope del cuerpo de cualquier respuesta de Ollama.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

pub struct OllamaBackend {
    client: Client,
    base_url: Url,
    configured_model: Option<String>,
}

impl OllamaBackend {
    pub fn new(base_url: Url, configured_model: Option<String>) -> reqwest::Result<Self> {
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .redirect(redirect::Policy::none())
            .no_proxy()
            .build()?;
        Ok(Self {
            client,
            base_url,
            configured_model,
        })
    }

    fn endpoint(&self, path: &str) -> Result<Url, BackendError> {
        self.base_url
            .join(path)
            .map_err(|error| BackendError::Failed(format!("dirección de Ollama inválida: {error}")))
    }

    fn unreachable(&self) -> BackendError {
        BackendError::Unavailable(format!(
            "Ollama no responde en {}. ¿Está instalado y corriendo?",
            self.base_url
        ))
    }

    fn map_error(&self, error: reqwest::Error) -> BackendError {
        if error.is_connect() {
            self.unreachable()
        } else if error.is_timeout() {
            BackendError::Failed("Ollama tardó demasiado en responder".into())
        } else {
            BackendError::Failed(format!("error hablando con Ollama: {error}"))
        }
    }

    async fn installed_models(&self) -> Result<Vec<String>, BackendError> {
        let response = self
            .client
            .get(self.endpoint("api/tags")?)
            .timeout(TAGS_TIMEOUT)
            .send()
            .await
            .map_err(|error| self.map_error(error))?;
        if !response.status().is_success() {
            return Err(BackendError::Failed(format!(
                "Ollama respondió {} a la lista de modelos",
                response.status()
            )));
        }
        let body = read_capped(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| self.map_error_or(error))?;
        parse_tags(&body)
    }

    async fn resolve_model(&self) -> Result<String, BackendError> {
        let installed = self.installed_models().await?;
        pick_model(self.configured_model.as_deref(), &installed)
    }

    async fn send_chat(&self, messages: &[Message]) -> Result<String, BackendError> {
        let model = self.resolve_model().await?;
        let response = self
            .client
            .post(self.endpoint("api/chat")?)
            .timeout(CHAT_TIMEOUT)
            .json(&ChatRequest {
                model: &model,
                messages,
                stream: false,
            })
            .send()
            .await
            .map_err(|error| self.map_error(error))?;
        match response.status() {
            status if status.is_success() => {}
            // Ollama contesta 404 cuando el modelo no está: se pudo borrar entre
            // la lista y el pedido.
            StatusCode::NOT_FOUND => return Err(model_missing(&model)),
            status => {
                return Err(BackendError::Failed(format!(
                    "Ollama respondió {status} al pedido de chat"
                )))
            }
        }
        let body = read_capped(response, MAX_RESPONSE_BYTES)
            .await
            .map_err(|error| self.map_error_or(error))?;
        parse_chat(&body)
    }

    fn map_error_or(&self, error: ReadError) -> BackendError {
        match error {
            ReadError::Http(error) => self.map_error(error),
            ReadError::TooLarge => BackendError::Failed(format!(
                "la respuesta de Ollama pasa el tope de {MAX_RESPONSE_BYTES} bytes"
            )),
        }
    }
}

impl ModelBackend for OllamaBackend {
    fn provider(&self) -> &str {
        PROVIDER
    }

    fn is_local(&self) -> bool {
        true
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(self.installed_models())
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move {
            Ok(ModelInfo {
                provider: PROVIDER.into(),
                name: self.resolve_model().await?,
                local: true,
            })
        })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(self.send_chat(messages))
    }
}

// ── Lo que va y viene ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatResponseMessage,
}

#[derive(Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    models: Vec<TagsModel>,
}

#[derive(Deserialize)]
struct TagsModel {
    name: String,
}

/// La respuesta de `/api/chat`: el texto del modelo.
pub fn parse_chat(body: &[u8]) -> Result<String, BackendError> {
    serde_json::from_slice::<ChatResponse>(body)
        .map(|response| response.message.content)
        .map_err(|error| BackendError::Failed(format!("respuesta de Ollama ilegible: {error}")))
}

/// La respuesta de `/api/tags`: los nombres de los modelos instalados.
pub fn parse_tags(body: &[u8]) -> Result<Vec<String>, BackendError> {
    serde_json::from_slice::<TagsResponse>(body)
        .map(|tags| tags.models.into_iter().map(|model| model.name).collect())
        .map_err(|error| BackendError::Failed(format!("lista de modelos ilegible: {error}")))
}

/// Con qué modelo responder. Si hay uno configurado tiene que estar instalado;
/// Ollama le agrega `:latest` al nombre sin etiqueta, así que `llama3.2`
/// encuentra a `llama3.2:latest`. Si no hay ninguno configurado, el primero
/// instalado.
pub fn pick_model(configured: Option<&str>, installed: &[String]) -> Result<String, BackendError> {
    match configured {
        Some(wanted) => installed
            .iter()
            .find(|name| *name == wanted || name.strip_suffix(":latest") == Some(wanted))
            .cloned()
            .ok_or_else(|| model_missing(wanted)),
        None => installed.first().cloned().ok_or_else(|| {
            BackendError::Unavailable(
                "Ollama no tiene ningún modelo instalado. Instalá uno con `ollama pull <modelo>`."
                    .into(),
            )
        }),
    }
}

fn model_missing(model: &str) -> BackendError {
    BackendError::Unavailable(format!(
        "El modelo «{model}» no está instalado en Ollama. Instalalo con `ollama pull {model}`."
    ))
}

// ── Leer con tope ─────────────────────────────────────────────────────────────

enum ReadError {
    Http(reqwest::Error),
    TooLarge,
}

/// Lee el cuerpo entero, cortando en cuanto pasa `cap` bytes: un servidor que
/// manda de más no hace crecer la memoria del servicio.
async fn read_capped(mut response: Response, cap: usize) -> Result<Vec<u8>, ReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err(ReadError::TooLarge);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(ReadError::Http)? {
        if body.len() + chunk.len() > cap {
            return Err(ReadError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}
