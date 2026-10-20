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
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::signal::unix::{signal, SignalKind};
use tracing::level_filters::LevelFilter;
use tracing::{info, warn};
use tracing_subscriber::filter::{EnvFilter, FilterExt, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use zbus::names::BusName;
use zbus::{connection, Connection};

use backend::{Limited, ModelBackend, RejectedBackend};
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
    let env = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("{}=info", env!("CARGO_CRATE_NAME")).into());
    // Las bibliotecas que ven los cuerpos (el bus, HTTP, TLS) no pasan de
    // `info` aunque `RUST_LOG` pida más: en `debug` o `trace` vuelcan mensajes
    // del bus, conversaciones y la cabecera con la clave. `Targets` va aparte
    // del filtro del entorno, así que nada en `RUST_LOG` lo afloja.
    let libraries = Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_targets(
            [
                "zbus",
                "zvariant",
                "hyper",
                "hyper_util",
                "h2",
                "reqwest",
                "rustls",
                "tower",
            ]
            .map(|target| (target, LevelFilter::INFO)),
        );
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(env.and(libraries)))
        .init();

    let settings = config::read_settings();
    let client = http::client()?;
    // El llavero lo usan el backend (para leer la clave) y el servicio (para
    // guardarla desde Configuración).
    let secrets = Arc::new(SecretService {
        connection: Connection::session().await?,
    });
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
            let backend: Arc<dyn ModelBackend> = match selection.kind {
                BackendKind::Ollama => Arc::new(OllamaBackend::new(client, selection)),
                BackendKind::OpenAi => {
                    Arc::new(OpenAiBackend::new(client, selection, secrets.clone()))
                }
            };
            Arc::new(Limited::new(backend))
        }
        Err(rejection) => {
            warn!("backend no disponible: {}", rejection.reason);
            Arc::new(RejectedBackend {
                info: rejection.info,
                reason: rejection.reason,
            })
        }
    };

    let conversations = Arc::new(Mutex::new(Conversations::default()));
    let service = AssistantService {
        backend,
        conversations: conversations.clone(),
        remote_allowed: settings.allow_remote,
        secrets,
    };

    let connection = connection::Builder::session()?
        .serve_at(SERVICE_PATH, service)?
        .build()
        .await?;
    // Primero la suscripción y después el nombre: así no hay un cliente que
    // abra conversaciones y se vaya sin que nos enteremos.
    let mut departures = zbus::fdo::DBusProxy::new(&connection)
        .await?
        .receive_name_owner_changed()
        .await?;
    connection.request_name(SERVICE_NAME).await?;
    info!("servicio en {SERVICE_NAME}");

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            Some(change) = departures.next() => {
                let Ok(args) = change.args() else { continue };
                // Una conexión que se cerró: su nombre único se queda sin dueño.
                if let (BusName::Unique(name), None) = (args.name(), args.new_owner().as_ref()) {
                    let released = conversations.lock().release_owner(name.as_str());
                    if released > 0 {
                        info!("{name} se fue: se liberan {released} conversaciones");
                    }
                }
            }
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }
    }
    info!("saliendo");
    Ok(())
}
