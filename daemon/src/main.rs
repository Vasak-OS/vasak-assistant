//! El asistente de IA residente de VasakOS (Lince, por omisión).
//!
//! Fase 1: el servicio y el contrato. Expone D-Bus en el bus de sesión y
//! conversa con un modelo —local por omisión (Ollama), o un proveedor
//! compatible con OpenAI si la configuración lo elige— sin ninguna capacidad
//! sobre los datos del usuario. Las conversaciones viven en memoria.
//!
//! Fase 4: la memoria de Lince (`memory`), una base de grafos cifrada en la
//! carpeta de estado del servicio. Viene apagada; encendida, en esta fase
//! queda lista y vacía.
//!
//! No dibuja nada: el chat y el panel de Configuración son otras superficies
//! que le hablan por D-Bus. Por eso es un demonio de Rust y no una aplicación
//! de Tauri.

mod backend;
mod config;
mod conversations;
mod http;
mod keyring;
mod logging;
mod memory;
mod ollama;
mod openai;
mod service;
#[cfg(test)]
mod test_support;

use std::sync::Arc;

use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::names::BusName;
use zbus::{connection, Connection};

use backend::{Limited, ModelBackend, RejectedBackend};
use config::BackendKind;
use conversations::Conversations;
use keyring::SecretService;
use memory::Memory;
use ollama::OllamaBackend;
use openai::OpenAiBackend;
use service::AssistantService;
use vasak_assistant_protocol::{SERVICE_NAME, SERVICE_PATH};

/// El nombre tiene que quedar en nuestras manos. Con `DO_NOT_QUEUE` sólo somos
/// dueños si la respuesta es `PrimaryOwner` (o `AlreadyOwner`, que también somos
/// nosotros); `Exists` o `InQueue` significan que otro proceso lo tiene, y
/// seguir deja a un impostor recibiendo a los clientes —sobre todo la clave del
/// proveedor que manda Configuración—, así que se aborta el arranque (#47).
fn primary_owner_or_abort(reply: RequestNameReply) -> Result<(), String> {
    match reply {
        RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner => Ok(()),
        other => Err(format!(
            "el nombre {SERVICE_NAME} ya lo tiene otro proceso (respuesta {other}): \
             no se arranca para no dejar que un impostor reciba a los clientes"
        )),
    }
}

/// Sin cola, sin ofrecerse a reemplazo y sin reemplazar a nadie: las banderas
/// por omisión de zbus (`AllowReplacement | ReplaceExisting | DoNotQueue`)
/// dejan que un proceso posterior nos eche del nombre.
fn name_flags() -> zbus::fdo::RequestNameFlags {
    RequestNameFlags::DoNotQueue
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init();

    let settings = config::read_settings();
    let client = http::client()?;
    // El llavero lo usan el backend (para leer la clave) y el servicio (para
    // guardarla desde Configuración).
    let secrets = Arc::new(SecretService {
        connection: Connection::session().await?,
    });
    let backend: Arc<dyn ModelBackend> = match config::select_backend(&settings) {
        Ok(selection) => {
            info!("{}", logging::backend_summary(&selection));
            let backend: Arc<dyn ModelBackend> = match selection.kind {
                BackendKind::Ollama => {
                    Arc::new(OllamaBackend::new(client, selection, settings.allow_remote))
                }
                BackendKind::OpenAi => {
                    Arc::new(OpenAiBackend::new(client, selection, secrets.clone()))
                }
            };
            Arc::new(Limited::new(backend))
        }
        Err(rejection) => {
            warn!(
                backend = rejection.info.provider.as_str(),
                "backend no disponible: {}", rejection.reason
            );
            Arc::new(RejectedBackend {
                info: rejection.info,
                reason: rejection.reason,
            })
        }
    };

    let conversations = Arc::new(Mutex::new(Conversations::default()));
    // Con un backend remoto, la memoria no se lee ni se escribe salvo que la
    // persona lo haya permitido.
    let remote_blocked = !backend.info().local && !settings.share_memory_with_remote;
    // `remote_blocked` mira lo configurado; la puerta de cada pedido mira el
    // modelo efectivo (`Memory::allowed_for`).
    let memory = Arc::new(
        Memory::from_env(secrets.clone())
            .with_remote_blocked(remote_blocked)
            .with_share_with_remote(settings.share_memory_with_remote),
    );
    info!(
        "{}",
        logging::startup_summary(&settings, &backend.info(), memory.is_enabled().await)
    );
    let service = AssistantService {
        backend,
        conversations: conversations.clone(),
        remote_allowed: settings.allow_remote,
        secrets,
        memory,
        labels: Arc::new(service::BusLabels),
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
    // `DO_NOT_QUEUE` y sin `AllowReplacement`: si otro proceso de la sesión ya
    // tiene el nombre no nos encolamos, y una vez que es nuestro nadie nos lo
    // puede quitar con `ReplaceExisting`. zbus devuelve el `Exists` del bus
    // (nombre ya tomado) como `NameTaken`; lo encaminamos al mismo chequeo para
    // abortar en un solo lugar y con un log claro.
    let reply = match connection
        .request_name_with_flags(SERVICE_NAME, name_flags().into())
        .await
    {
        Ok(reply) => reply,
        Err(zbus::Error::NameTaken) => RequestNameReply::Exists,
        Err(error) => return Err(error.into()),
    };
    if let Err(message) = primary_owner_or_abort(reply) {
        error!("{message}");
        return Err(message.into());
    }
    info!(name = SERVICE_NAME, "servicio en el bus de sesión");

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    loop {
        tokio::select! {
            Some(change) = departures.next() => {
                let Ok(args) = change.args() else { continue };
                // Una conexión que se cerró: su nombre único se queda sin dueño.
                if let (BusName::Unique(name), None) = (args.name(), args.new_owner().as_ref()) {
                    // Olvidarlas al irse el dueño no es borrarlas: sus
                    // resúmenes en la memoria quedan, que para eso están
                    // (recordar conversaciones pasadas). Sólo
                    // `DeleteConversation`, que es pedirlo, los borra.
                    let released = conversations.lock().release_owner(name.as_str());
                    if released > 0 {
                        info!(sender = %name, released, "la conexión se fue: se liberan sus conversaciones");
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::PrivateBus;

    #[test]
    fn dueno_primario_o_ya_dueno_dejan_arrancar() {
        assert_eq!(
            primary_owner_or_abort(RequestNameReply::PrimaryOwner),
            Ok(())
        );
        assert_eq!(
            primary_owner_or_abort(RequestNameReply::AlreadyOwner),
            Ok(())
        );
    }

    #[test]
    fn nombre_ocupado_o_en_cola_abortan_nombrando_el_servicio() {
        for reply in [RequestNameReply::Exists, RequestNameReply::InQueue] {
            let message = primary_owner_or_abort(reply).unwrap_err();
            assert!(message.contains(SERVICE_NAME), "{message}");
        }
    }

    /// Lo que cierra el squat: con las banderas del demonio, otro proceso no
    /// puede echarlo del nombre pidiéndolo con `ReplaceExisting`, y quien llega
    /// segundo no se encola.
    #[tokio::test]
    async fn por_un_bus_real_nadie_le_quita_el_nombre_al_demonio_ni_se_encola() {
        let Some(bus) = PrivateBus::start_or_skip() else {
            return;
        };
        let daemon = bus.connect().await;
        let first = daemon
            .request_name_with_flags(SERVICE_NAME, name_flags().into())
            .await
            .unwrap();
        assert_eq!(primary_owner_or_abort(first), Ok(()));

        // Con `ReplaceExisting` y sin `DoNotQueue` el que llega queda en la cola
        // (no es dueño); con `DoNotQueue` zbus traduce `Exists` en `NameTaken`.
        let squatter = bus.connect().await;
        let replace = squatter
            .request_name_with_flags(SERVICE_NAME, RequestNameFlags::ReplaceExisting.into())
            .await;
        assert!(
            matches!(replace, Ok(RequestNameReply::InQueue)),
            "{replace:?}"
        );
        let second = bus
            .connect()
            .await
            .request_name_with_flags(SERVICE_NAME, name_flags().into())
            .await;
        assert!(matches!(second, Err(zbus::Error::NameTaken)), "{second:?}");

        let dbus = zbus::fdo::DBusProxy::new(&squatter).await.unwrap();
        let owner = dbus
            .get_name_owner(SERVICE_NAME.try_into().unwrap())
            .await
            .unwrap();
        assert_eq!(owner.as_str(), daemon.unique_name().unwrap().as_str());
    }
}
