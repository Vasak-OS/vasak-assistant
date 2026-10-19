//! El asistente de IA residente de VasakOS (Lince, por omisión).
//!
//! Fase 1: el servicio y el contrato. Expone D-Bus en el bus de sesión y
//! conversa con un modelo —local por omisión (Ollama), o un proveedor
//! compatible con OpenAI si la configuración lo elige— sin ninguna capacidad
//! sobre los datos del usuario. Las conversaciones viven en memoria; nada se
//! guarda.
//!
//! No dibuja nada: el chat y el panel de Configuración son otras superficies
//! que le hablan por D-Bus. Por eso es un demonio de Rust y no una aplicación
//! de Tauri.

mod backend;
mod config;
mod conversations;
mod http;
mod keyring;
mod ollama;
mod openai;
mod service;

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{info, warn};
use zbus::{connection, Connection};

use backend::{ModelBackend, RejectedBackend};
use config::BackendKind;
use conversations::Conversations;
use keyring::SecretService;
use ollama::OllamaBackend;
use openai::OpenAiBackend;
use service::AssistantService;
use vasak_assistant_protocol::{SERVICE_NAME, SERVICE_PATH};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // El nombre del crate lo pone el compilador: el binario es
    // `vasak-assistant` pero el paquete es `vasak-assistant-daemon`.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("{}=info", env!("CARGO_CRATE_NAME")).into()),
        )
        .init();

    let settings = config::read_settings();
    let client = http::client()?;
    let backend: Arc<dyn ModelBackend> = match config::select_backend(&settings) {
        Ok(selection) => {
            info!(
                "backend: {} en {} ({}), modelo {}",
                config::provider_name(selection.kind),
                http::origin(&selection.url),
                if selection.local { "local" } else { "remoto" },
                selection
                    .model
                    .as_deref()
                    .unwrap_or("(el primero que ofrezca)")
            );
            match selection.kind {
                BackendKind::Ollama => Arc::new(OllamaBackend::new(client, selection)),
                BackendKind::OpenAi => {
                    let secrets = SecretService {
                        connection: Connection::session().await?,
                    };
                    Arc::new(OpenAiBackend::new(client, selection, Arc::new(secrets)))
                }
            }
        }
        Err(rejection) => {
            warn!("backend no disponible: {}", rejection.reason);
            Arc::new(RejectedBackend {
                info: rejection.info,
                reason: rejection.reason,
            })
        }
    };

    let service = AssistantService {
        backend,
        conversations: Arc::new(Mutex::new(Conversations::default())),
        remote_allowed: settings.allow_remote,
    };

    let _connection = connection::Builder::session()?
        .name(SERVICE_NAME)?
        .serve_at(SERVICE_PATH, service)?
        .build()
        .await?;
    info!("servicio en {SERVICE_NAME}");

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => {}
        _ = sigint.recv() => {}
    }
    info!("saliendo");
    Ok(())
}
