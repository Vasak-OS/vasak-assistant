//! El backend de Ollama, con su API propia.
//!
//! Usa dos rutas: `/api/tags` (qué modelos hay, y de paso si Ollama está) y
//! `/api/chat` sin streaming (la respuesta entera de una vez). Las defensas
//! comunes —tiempos, tope, sin redirecciones ni proxy— están en [`crate::http`].

use reqwest::{Client, StatusCode, Url};
use serde::{Deserialize, Serialize};
use vasak_assistant_protocol::{Message, ModelInfo};

use crate::backend::{BackendError, BoxFuture, ModelBackend};
use crate::config::Selection;
use crate::http::{self, CHAT_TIMEOUT, LIST_TIMEOUT};

pub struct OllamaBackend {
    client: Client,
    selection: Selection,
}

impl OllamaBackend {
    pub fn new(client: Client, selection: Selection) -> Self {
        Self { client, selection }
    }

    fn base(&self) -> &Url {
        &self.selection.url
    }

    async fn installed_models(&self) -> Result<Vec<String>, BackendError> {
        let response = self
            .client
            .get(http::endpoint(self.base(), "api/tags")?)
            .timeout(LIST_TIMEOUT)
            .send()
            .await
            .map_err(|error| http::map_error(self.base(), error))?;
        if !response.status().is_success() {
            return Err(BackendError::Failed(format!(
                "Ollama respondió {} a la lista de modelos",
                response.status()
            )));
        }
        parse_tags(&http::read_capped(self.base(), response).await?)
    }

    async fn resolve_model(&self) -> Result<String, BackendError> {
        let installed = self.installed_models().await?;
        pick_model(self.selection.model.as_deref(), &installed)
    }

    async fn send_chat(&self, messages: &[Message]) -> Result<String, BackendError> {
        let model = self.resolve_model().await?;
        let response = self
            .client
            .post(http::endpoint(self.base(), "api/chat")?)
            .timeout(CHAT_TIMEOUT)
            .json(&ChatRequest {
                model: &model,
                messages,
                stream: false,
            })
            .send()
            .await
            .map_err(|error| http::map_error(self.base(), error))?;
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
        parse_chat(&http::read_capped(self.base(), response).await?)
    }
}

impl ModelBackend for OllamaBackend {
    fn info(&self) -> ModelInfo {
        self.selection.info()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(self.installed_models())
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move {
            Ok(ModelInfo {
                name: self.resolve_model().await?,
                ..self.info()
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
