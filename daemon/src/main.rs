//! El asistente de IA residente de VasakOS (Lince, por omisión).
//!
//! Fase 1: el servicio y el contrato. Expone D-Bus en el bus de sesión y
//! conversa con un modelo local por Ollama, sin ninguna capacidad sobre los
//! datos del usuario. Las conversaciones viven en memoria; nada se guarda.
//!
//! No dibuja nada: el chat y el panel de Configuración son otras superficies
//! que le hablan por D-Bus. Por eso es un demonio de Rust y no una aplicación
//! de Tauri.

mod backend;
mod config;
mod conversations;
mod ollama;
mod service;

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::signal::unix::{signal, SignalKind};
use tracing::info;
use zbus::connection;

use conversations::Conversations;
use ollama::OllamaBackend;
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

    let (url, model) = config::from_env();
    info!(
        "backend: Ollama en {url}, modelo {}",
        model.as_deref().unwrap_or("(el primero instalado)")
    );

    let service = AssistantService {
        backend: Arc::new(OllamaBackend::new(url, model)?),
        conversations: Arc::new(Mutex::new(Conversations::default())),
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
