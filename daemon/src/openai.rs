//! El backend genérico compatible con la API de OpenAI.
//!
//! `GET {base}/models` y `POST {base}/chat/completions` sin streaming. Lo
//! hablan el propio OpenAI y la mayoría de los proveedores, y también
//! llama.cpp server y LM Studio en la máquina. Las defensas comunes —tiempos,
//! tope, sin redirecciones ni proxy— están en [`crate::http`].
//!
//! La clave sale del llavero en cada pedido ([`crate::keyring`]), así una clave
//! nueva sirve sin reiniciar. Si no hay, el pedido va sin `Authorization`: un
//! servidor local no la suele pedir, y uno que sí contesta 401, que se informa
//! como «no disponible» con el motivo.

use std::sync::Arc;

use reqwest::{Client, RequestBuilder, Response, StatusCode, Url};
use serde::{Deserialize, Serialize};
use vasak_assistant_protocol::{Message, ModelInfo};

use crate::backend::{BackendError, BoxFuture, ModelBackend};
use crate::config::Selection;
use crate::http::{self, CHAT_TIMEOUT, LIST_TIMEOUT};
use crate::keyring::{SecretSource, ITEM_SERVICE};

pub struct OpenAiBackend {
    client: Client,
    selection: Selection,
    secrets: Arc<dyn SecretSource>,
}

impl OpenAiBackend {
    pub fn new(client: Client, selection: Selection, secrets: Arc<dyn SecretSource>) -> Self {
        Self {
            client,
            selection,
            secrets,
        }
    }

    fn base(&self) -> &Url {
        &self.selection.url
    }

    /// Le pone la clave al pedido, si hay. `bearer_auth` marca la cabecera
    /// como sensible: no aparece en ningún `Debug`.
    async fn authorize(&self, request: RequestBuilder) -> RequestBuilder {
        match self.secrets.api_key(self.base().as_str()).await {
            Some(key) => request.bearer_auth(key.expose()),
            None => request,
        }
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response, BackendError> {
        let response = self
            .authorize(request)
            .await
            .send()
            .await
            .map_err(|error| http::map_error(self.base(), error))?;
        match response.status() {
            status if status.is_success() => Ok(response),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(self.key_missing()),
            status => Err(BackendError::Failed(format!(
                "{} respondió {status}",
                http::origin(self.base())
            ))),
        }
    }

    fn key_missing(&self) -> BackendError {
        BackendError::Unavailable(format!(
            "{} pide una clave y en el llavero no hay ninguna válida para «{}» (service={ITEM_SERVICE}, url={}).",
            http::origin(self.base()),
            self.base(),
            self.base(),
        ))
    }

    async fn list_models(&self) -> Result<Vec<String>, BackendError> {
        let request = self
            .client
            .get(http::endpoint(self.base(), "models")?)
            .timeout(LIST_TIMEOUT);
        let response = self.send(request).await?;
        parse_models(&http::read_capped(self.base(), response).await?)
    }

    /// El modelo configurado tal cual (un proveedor no siempre lista todos los
    /// que acepta), o el primero de la lista si no hay. Un remoto sin modelo ya
    /// lo rechazó `config::select_backend`.
    async fn resolve_model(&self) -> Result<String, BackendError> {
        if let Some(model) = &self.selection.model {
            return Ok(model.clone());
        }
        self.list_models().await?.into_iter().next().ok_or_else(|| {
            BackendError::Unavailable(format!(
                "{} no ofrece ningún modelo.",
                http::origin(self.base())
            ))
        })
    }

    async fn send_chat(&self, messages: &[Message]) -> Result<String, BackendError> {
        let model = self.resolve_model().await?;
        let request = self
            .client
            .post(http::endpoint(self.base(), "chat/completions")?)
            .timeout(CHAT_TIMEOUT)
            .json(&ChatRequest {
                model: &model,
                messages,
                stream: false,
            });
        let response = self.send(request).await?;
        parse_chat(&http::read_capped(self.base(), response).await?)
    }
}

impl ModelBackend for OpenAiBackend {
    fn info(&self) -> ModelInfo {
        self.selection.info()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(self.list_models())
    }

    /// Pregunta la lista aunque el modelo esté configurado: es lo que dice si
    /// el servidor responde y si acepta la clave.
    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move {
            let listed = self.list_models().await?;
            let name = match &self.selection.model {
                Some(model) => model.clone(),
                None => listed.into_iter().next().ok_or_else(|| {
                    BackendError::Unavailable(format!(
                        "{} no ofrece ningún modelo.",
                        http::origin(self.base())
                    ))
                })?,
            };
            Ok(ModelInfo {
                name,
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
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    /// Puede venir `null` (una llamada a herramienta, un rechazo).
    content: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// La respuesta de `/chat/completions`: el texto de la primera opción.
pub fn parse_chat(body: &[u8]) -> Result<String, BackendError> {
    let response = serde_json::from_slice::<ChatResponse>(body)
        .map_err(|error| BackendError::Failed(format!("respuesta del modelo ilegible: {error}")))?;
    response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .ok_or_else(|| BackendError::Failed("el modelo respondió sin texto".into()))
}

/// La respuesta de `/models`: los ids de los modelos.
pub fn parse_models(body: &[u8]) -> Result<Vec<String>, BackendError> {
    serde_json::from_slice::<ModelsResponse>(body)
        .map(|models| models.data.into_iter().map(|model| model.id).collect())
        .map_err(|error| BackendError::Failed(format!("lista de modelos ilegible: {error}")))
}
