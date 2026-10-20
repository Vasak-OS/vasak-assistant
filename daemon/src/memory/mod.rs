//! La memoria de Lince: una base de grafos propia del asistente (Grafeo,
//! embebida en este mismo proceso), cifrada con AES-256-GCM —el archivo y su
//! WAL— con una llave cruda de 32 bytes que vive en el llavero de la sesión
//! (`crate::keyring`). No va en el almacén de cuentas.
//!
//! # Qué guarda y qué no
//!
//! **La memoria no guarda datos sensibles que no son suyos**: correo,
//! calendario, contactos, archivos, cuentas ni claves. Esos se leen de su
//! servicio en el momento de usarlos y no se persisten acá. Lo que sí va
//! (preferencias, tono, hábitos, temas, correcciones, nombres, contexto) lleva
//! el esquema de [`schema`]. Lo que pasa por la memoria se filtra dos veces
//! ([`filter`]): un turno con un secreto no se aprende, y ningún recuerdo con
//! un secreto o un dato de la persona entra en la base.
//!
//! # Lectura y escritura
//!
//! - [`recall`]: antes de mandar un mensaje al modelo, un bloque de notas
//!   relevantes va como mensaje `system` al principio del contexto de ese
//!   turno (nunca al historial).
//! - [`learn`]: después de responder, si el turno tiene señales de algo que
//!   recordar, el modelo extrae recuerdos en segundo plano, se validan y se
//!   guardan.
//! - **Remoto**: con un backend remoto, la memoria no se lee ni se escribe salvo
//!   que la persona lo permita (`share_memory_with_remote` en `vasak.conf`).
//! - **Espacio**: la base no pasa de [`MAX_MEMORY_DISK_BYTES`]; si se pasa se
//!   poda el 10 % de cada clase, y si sigue pasada no se escribe más.
//!
//! # Encendida, apagada y borrada
//!
//! Todo vive en la carpeta de estado del servicio (`$STATE_DIRECTORY`, que pone
//! systemd, o `~/.local/state/vasak-assistant`); nada en `vasak.conf`.
//!
//! - **Viene apagada.** «Encendida» quiere decir que existe el archivo
//!   `enabled` al lado de `memory/`. La base y la llave son otra cosa: pueden
//!   estar con la memoria apagada.
//! - **Encender**: si ya hay base, se reabre con la llave que está en el
//!   llavero. Si no, con el llavero desbloqueado, una llave nueva al llavero,
//!   se la relee, y la base se crea en `memory.tmp/` y se renombra a
//!   `memory/` ya completa, con un nodo de metadatos con la versión del
//!   formato. El indicador se escribe sólo si la base quedó abierta.
//! - **Apagar no borra nada**: se cierra la base y se saca el indicador; los
//!   archivos y la llave quedan, y encender de nuevo los reabre.
//! - **Borrar** es sólo [`Memory::clear`]: primero la llave del llavero (sin
//!   ella lo que quede en disco no se puede leer) y después los archivos, por
//!   `memory.deleting/`; si estaba encendida, se recrea vacía con una llave
//!   nueva. Con el llavero bloqueado no se toca nada.
//! - **Al arrancar** no se genera ni se borra nada solo: si la llave falta o no
//!   abre la base, la memoria queda no disponible con el motivo, y la salida es
//!   borrarla. La base se abre la primera vez que alguien la necesita, así un
//!   llavero que se desbloquea después alcanza.

use std::ffi::OsString;
use std::fmt::Display;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grafeo::{Config, EncryptionConfig, GrafeoDB, KeyChain};
use tokio::sync::{Mutex, Semaphore};
use vasak_assistant_protocol::{Error, MemoryStatus, Message, Role};
use zeroize::Zeroizing;

use crate::backend::ModelBackend;
use crate::keyring::{MemoryKey, SecretSource, MEMORY_KEY_BYTES};

pub mod filter;
pub mod learn;
pub mod recall;
pub mod schema;

use learn::Extraction;
use schema::{MemoryKind, NewMemory};

pub const REASON_OFF: &str = "La memoria está apagada";
pub const REASON_LOCKED: &str = "El llavero está bloqueado";
pub const REASON_MISSING_KEY: &str = "La clave de la memoria no está en el llavero";
pub const REASON_DUPLICATED_KEY: &str = "Hay más de una clave de la memoria en el llavero";
pub const REASON_NOT_OPENED: &str = "La memoria no se pudo abrir";
pub const REASON_NO_KEYRING: &str = "El llavero no responde";
pub const REASON_NO_PLACE: &str = "No hay una carpeta de estado donde guardar la memoria";
pub const REASON_REMOTE: &str = "La memoria no se comparte con un proveedor remoto";

/// El tope de lo que ocupa la base en disco.
pub const MAX_MEMORY_DISK_BYTES: u64 = 4 << 20;

/// La versión del formato de la base, en el nodo `:LinceMeta`.
pub const FORMAT_VERSION: i64 = 1;

/// El archivo de la base dentro de `memory/` (Grafeo le agrega al lado su WAL,
/// `lince.grafeo.wal/`).
const DB_FILE: &str = "lince.grafeo";

/// El presupuesto de memoria del motor, muy por debajo del `MemoryMax=512M` de
/// la unidad: la base no pasa de unos cientos de nodos y 4 MiB. Una base
/// cifrada no vuelca nada a disco (Grafeo no cifra los archivos de desborde),
/// así que este tope no mueve datos: los acota.
const MEMORY_LIMIT: usize = 64 << 20;

/// La carpeta `memory/`. `state_directory` es `$STATE_DIRECTORY` (systemd,
/// con un solo `StateDirectory=`) y manda si está; si no, `state_dir` (la de
/// XDG, que `dirs` ya da absoluta) más `vasak-assistant`. Una base relativa se
/// rechaza: `None`, y la memoria no se puede encender.
pub fn memory_dir(
    state_directory: Option<OsString>,
    state_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    let base = match state_directory.filter(|value| !value.is_empty()) {
        Some(value) => PathBuf::from(value),
        None => state_dir?.join("vasak-assistant"),
    };
    base.is_absolute().then(|| base.join("memory"))
}

pub struct Memory {
    /// `None` si no hay carpeta de estado válida.
    dir: Option<PathBuf>,
    secrets: Arc<dyn SecretSource>,
    /// La base abierta. Un candado de Tokio: encender, apagar y borrar lo
    /// sostienen de punta a punta, con esperas al llavero y al disco.
    db: Mutex<Option<Arc<GrafeoDB>>>,
    /// Con un backend remoto y sin `share_memory_with_remote`: ni se lee ni se
    /// escribe.
    remote_blocked: bool,
    /// Una sola extracción a la vez, con `try_acquire`: si hay una en curso,
    /// el turno no se aprende. Propio, aparte de los permisos del chat.
    learning: Semaphore,
}

impl Memory {
    pub fn new(dir: Option<PathBuf>, secrets: Arc<dyn SecretSource>) -> Self {
        Self {
            dir,
            secrets,
            db: Mutex::new(None),
            remote_blocked: false,
            learning: Semaphore::new(1),
        }
    }

    /// Con un backend remoto que no tiene permiso de ver la memoria.
    pub fn with_remote_blocked(mut self, blocked: bool) -> Self {
        self.remote_blocked = blocked;
        self
    }

    /// Con la carpeta del entorno del proceso.
    pub fn from_env(secrets: Arc<dyn SecretSource>) -> Self {
        let dir = memory_dir(std::env::var_os("STATE_DIRECTORY"), dirs::state_dir());
        if dir.is_none() {
            tracing::warn!("no hay carpeta de estado absoluta: la memoria no se podrá encender");
        }
        Self::new(dir, secrets)
    }

    /// Nunca falla: cualquier problema va como `available: false` y el motivo.
    /// `bytes` se informa también con la memoria apagada.
    pub async fn status(&self) -> MemoryStatus {
        let Some(dir) = &self.dir else {
            return MemoryStatus {
                enabled: false,
                available: false,
                reason: REASON_NO_PLACE.to_owned(),
                bytes: 0,
            };
        };
        let mut db = self.db.lock().await;
        let enabled = is_file(&flag(dir)).await;
        let reason = if !enabled {
            REASON_OFF
        } else if self.remote_blocked {
            REASON_REMOTE
        } else {
            self.ensure_open(dir, &mut db).await.err().unwrap_or("")
        };
        MemoryStatus {
            enabled,
            available: reason.is_empty(),
            reason: reason.to_owned(),
            bytes: disk_usage(dir.clone()).await,
        }
    }

    /// La base abierta, para quien guarde o lea recuerdos. `Err` con el
    /// motivo si está apagada, el remoto no la puede ver o no se puede abrir.
    pub async fn database(&self) -> Result<Arc<GrafeoDB>, &'static str> {
        let dir = self.dir.as_ref().ok_or(REASON_NO_PLACE)?;
        let mut db = self.db.lock().await;
        self.usable(dir, &mut db).await
    }

    /// Con el candado tomado: encendida, permitida y abierta.
    async fn usable(
        &self,
        dir: &Path,
        db: &mut Option<Arc<GrafeoDB>>,
    ) -> Result<Arc<GrafeoDB>, &'static str> {
        if !is_file(&flag(dir)).await {
            return Err(REASON_OFF);
        }
        if self.remote_blocked {
            return Err(REASON_REMOTE);
        }
        self.ensure_open(dir, db).await?;
        db.clone().ok_or(REASON_NOT_OPENED)
    }

    /// El bloque de notas para este mensaje ([`recall`]). Nunca falla: sin
    /// memoria utilizable, o si algo sale mal, `None` y la conversación sigue
    /// sin notas.
    pub async fn recall(&self, message: &str, first_turn: bool) -> Option<String> {
        let db = self.database().await.ok()?;
        let message = message.to_owned();
        blocking(move || Ok(recall::build(&db, &message, first_turn, schema::now())))
            .await
            .ok()
            .flatten()
    }

    /// Después de responder: si el turno pasa la puerta ([`learn::worth_learning`])
    /// y no hay otra extracción en curso, extrae y guarda recuerdos en segundo
    /// plano. No espera ni falla: lo que salga mal queda en el registro.
    ///
    /// `conversation` son los mensajes del turno (historial, mensaje y
    /// respuesta), sin el bloque de la memoria.
    pub fn learn_after(
        self: &Arc<Self>,
        backend: Arc<dyn ModelBackend>,
        conversation_id: &str,
        conversation: Vec<Message>,
    ) {
        let user_turns = conversation
            .iter()
            .filter(|message| message.role == Role::User)
            .count();
        let (Some(user), Some(reply)) = (
            conversation.iter().rev().find(|m| m.role == Role::User),
            conversation.last().filter(|m| m.role == Role::Assistant),
        ) else {
            return;
        };
        let Some(gate) = learn::worth_learning(&user.content, &reply.content, user_turns) else {
            return;
        };
        let memory = self.clone();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            let Ok(_permit) = memory.learning.try_acquire() else {
                return;
            };
            // Antes de mandarle nada al modelo: que la memoria se pueda usar.
            if memory.database().await.is_err() {
                return;
            }
            let request = learn::extraction_request(&conversation, gate);
            let reply = match backend.chat(&request).await {
                Ok(reply) => reply,
                Err(error) => {
                    tracing::info!("la extracción de recuerdos no respondió: {error:?}");
                    return;
                }
            };
            let extraction = learn::parse_extraction(&reply);
            if extraction.items.is_empty() && extraction.summary.is_none() {
                return;
            }
            if let Err(reason) = memory.store(extraction, &conversation_id).await {
                tracing::info!("no se guardaron los recuerdos: {reason}");
            }
        });
    }

    /// Guarda lo extraído, bajo el candado y volviendo a comprobar que la
    /// memoria siga encendida y permitida.
    pub async fn store(&self, extraction: Extraction, conversation_id: &str) -> Result<(), String> {
        let dir = self.dir.as_ref().ok_or(REASON_NO_PLACE)?;
        let mut guard = self.db.lock().await;
        let db = self.usable(dir, &mut guard).await?;
        let (dir, conversation_id) = (dir.clone(), conversation_id.to_owned());
        blocking(move || write(&db, &dir, &extraction, &conversation_id).map_err(text)).await
    }

    /// Cierra la base si está abierta (vuelca el WAL al archivo). Se vuelve a
    /// abrir sola la próxima vez que se la necesite.
    // Sin llamador en el servicio todavía: apagar y borrar cierran por dentro.
    #[allow(dead_code)]
    pub async fn close(&self) {
        close(self.db.lock().await.take()).await;
    }

    pub async fn set_enabled(&self, enabled: bool) -> Result<(), Error> {
        let Some(dir) = &self.dir else {
            return if enabled {
                Err(Error::Unavailable(REASON_NO_PLACE.into()))
            } else {
                Ok(())
            };
        };
        let mut db = self.db.lock().await;
        if !enabled {
            // Apagar no borra nada: deja de usarla y de escribirla.
            close(db.take()).await;
            return remove_flag(dir).await;
        }
        if is_dir(dir).await {
            self.ensure_open(dir, &mut db)
                .await
                .map_err(|reason| Error::Unavailable(reason.into()))?;
        } else {
            self.create(dir, &mut db).await?;
        }
        write_flag(dir).await
    }

    /// Borra la llave y los archivos; si estaba encendida, la recrea vacía con
    /// una llave nueva. Con el llavero bloqueado no toca nada.
    pub async fn clear(&self) -> Result<(), Error> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        let mut db = self.db.lock().await;
        if self.secrets.is_locked().await? {
            return Err(locked());
        }
        // Primero la llave: si no se pudo borrar, los archivos quedan con ella.
        self.secrets.delete_memory_key().await?;
        close(db.take()).await;
        let target = dir.to_owned();
        blocking(move || remove_files(&target).map_err(text))
            .await
            .map_err(|error| {
                Error::Unavailable(format!("No se pudo borrar la memoria: {error}"))
            })?;
        if is_file(&flag(dir)).await {
            self.create(dir, &mut db).await?;
        }
        Ok(())
    }

    /// Abre la base si no lo está. Nunca crea ni borra nada.
    async fn ensure_open(
        &self,
        dir: &Path,
        db: &mut Option<Arc<GrafeoDB>>,
    ) -> Result<(), &'static str> {
        if db.is_some() {
            return Ok(());
        }
        let key = match self.secrets.memory_key().await {
            Ok(MemoryKey::Found(key)) => key,
            Ok(MemoryKey::Missing) => return Err(REASON_MISSING_KEY),
            Ok(MemoryKey::Locked) => return Err(REASON_LOCKED),
            Ok(MemoryKey::Duplicated) => return Err(REASON_DUPLICATED_KEY),
            Ok(MemoryKey::Invalid) => return Err(REASON_NOT_OPENED),
            Err(_) => return Err(REASON_NO_KEYRING),
        };
        let path = dir.join(DB_FILE);
        let opened = blocking(move || {
            // Grafeo crearía una base nueva donde no hay archivo: al abrir, eso
            // no se hace nunca.
            if !path.is_file() {
                return Err(format!("falta {}", path.display()));
            }
            let db = GrafeoDB::with_config(config(&path, &key)).map_err(text)?;
            schema::ensure_indexes(&db).map_err(text)?;
            Ok(db)
        })
        .await;
        match opened {
            Ok(opened) => {
                *db = Some(Arc::new(opened));
                Ok(())
            }
            Err(error) => {
                tracing::warn!("no se pudo abrir la memoria: {error}");
                Err(REASON_NOT_OPENED)
            }
        }
    }

    /// Crea la base vacía con una llave nueva. Con `memory/` ausente.
    async fn create(&self, dir: &Path, db: &mut Option<Arc<GrafeoDB>>) -> Result<(), Error> {
        // 1. Nunca se genera una llave sin saber que el llavero está abierto.
        if self.secrets.is_locked().await? {
            return Err(locked());
        }
        // 2.
        let mut key = Zeroizing::new([0u8; MEMORY_KEY_BYTES]);
        getrandom::fill(key.as_mut()).map_err(|error| {
            Error::Unavailable(format!(
                "No se pudo generar la llave de la memoria: {error}"
            ))
        })?;
        // 3.
        self.secrets.store_memory_key(&key).await?;
        // 4. Se relee: la base se cifra con lo que de verdad quedó guardado.
        let stored = match self.secrets.memory_key().await? {
            MemoryKey::Found(stored) if *stored == *key => stored,
            other => {
                tracing::warn!("la llave de la memoria no se pudo releer: {other:?}");
                return Err(Error::Unavailable(format!(
                    "{REASON_MISSING_KEY}: no se pudo releer después de guardarla."
                )));
            }
        };
        // 5 y 6.
        let dir = dir.to_owned();
        let created = blocking(move || create_files(&dir, &stored)).await;
        match created {
            Ok(opened) => {
                *db = Some(Arc::new(opened));
                Ok(())
            }
            Err(error) => {
                tracing::warn!("no se pudo crear la memoria: {error}");
                Err(Error::Unavailable(format!(
                    "No se pudo crear la memoria: {error}"
                )))
            }
        }
    }
}

/// El indicador de encendida, al lado de `memory/`.
fn flag(dir: &Path) -> PathBuf {
    dir.with_file_name("enabled")
}

async fn write_flag(dir: &Path) -> Result<(), Error> {
    let path = flag(dir);
    blocking(move || {
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map(drop)
            .map_err(text)
    })
    .await
    .map_err(|error| Error::Unavailable(format!("No se pudo encender la memoria: {error}")))
}

async fn remove_flag(dir: &Path) -> Result<(), Error> {
    match tokio::fs::remove_file(flag(dir)).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(Error::Unavailable(format!(
            "No se pudo apagar la memoria: {error}"
        ))),
        _ => Ok(()),
    }
}

async fn is_file(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file())
}

fn locked() -> Error {
    Error::Unavailable(format!(
        "{REASON_LOCKED}: desbloquealo (al iniciar sesión, o desde el llavero) y volvé a intentarlo."
    ))
}

fn text(error: impl Display) -> String {
    error.to_string()
}

/// La configuración de la base: cifrada, con WAL y con el tope de memoria. Los
/// índices son de la base ([`schema::ensure_indexes`]); cada escritura termina
/// con `wal_checkpoint`, así que no hace falta `with_checkpoint_interval`.
fn config(path: &Path, key: &[u8; MEMORY_KEY_BYTES]) -> Config {
    Config::persistent(path)
        .with_encryption(EncryptionConfig::new(Arc::new(KeyChain::new(*key))))
        .with_memory_limit(MEMORY_LIMIT)
}

/// Crea la base en `memory.tmp/`, la cierra, la renombra a `memory/` y la
/// devuelve abierta. Si algo falla no queda `memory/`.
fn create_files(dir: &Path, key: &[u8; MEMORY_KEY_BYTES]) -> Result<GrafeoDB, String> {
    let tmp = dir.with_file_name("memory.tmp");
    let parent = dir
        .parent()
        .ok_or("la carpeta de la memoria no tiene padre")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(text)?;
    remove_dir_if_exists(&tmp).map_err(text)?;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&tmp)
        .map_err(text)?;
    let built = (|| {
        let db = GrafeoDB::with_config(config(&tmp.join(DB_FILE), key)).map_err(text)?;
        db.create_node_with_props(&["LinceMeta"], [("format", FORMAT_VERSION)])
            .map_err(text)?;
        schema::ensure_indexes(&db).map_err(text)?;
        db.close().map_err(text)?;
        drop(db);
        std::fs::rename(&tmp, dir).map_err(text)
    })();
    if let Err(error) = built {
        let _ = remove_dir_if_exists(&tmp);
        return Err(error);
    }
    GrafeoDB::with_config(config(&dir.join(DB_FILE), key)).map_err(text)
}

/// Borra `memory/` por `memory.deleting/`, y lo que haya quedado de un corte
/// anterior (`memory.deleting/`, `memory.tmp/`).
fn remove_files(dir: &Path) -> io::Result<()> {
    let deleting = dir.with_file_name("memory.deleting");
    remove_dir_if_exists(&deleting)?;
    match std::fs::rename(dir, &deleting) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    remove_dir_if_exists(&deleting)?;
    remove_dir_if_exists(&dir.with_file_name("memory.tmp"))
}

fn remove_dir_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
}

/// Lo que ocupan los archivos bajo `path`, sin seguir enlaces ni abrir la base.
fn dir_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    std::fs::read_dir(path)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| dir_size(&entry.path()))
        .sum()
}

async fn disk_usage(dir: PathBuf) -> u64 {
    tokio::task::spawn_blocking(move || dir_size(&dir))
        .await
        .unwrap_or(0)
}

/// La escritura de lo extraído, fuera del runtime. Antes de escribir mira el
/// espacio: pasado [`MAX_MEMORY_DISK_BYTES`] poda el 10 % de cada clase, y si
/// sigue pasado no escribe. Termina con `wal_checkpoint`.
fn write(
    db: &GrafeoDB,
    dir: &Path,
    extraction: &Extraction,
    conversation_id: &str,
) -> grafeo::Result<()> {
    let now = schema::now();
    if dir_size(dir) > MAX_MEMORY_DISK_BYTES {
        schema::prune_tenth(db, now)?;
        if dir_size(dir) > MAX_MEMORY_DISK_BYTES {
            tracing::warn!("la memoria llegó a su tope de espacio: no se guarda nada más");
            return Ok(());
        }
    }
    for item in &extraction.items {
        let node = schema::upsert(
            db,
            &NewMemory {
                kind: item.kind,
                text: &item.text,
                phrase: item.phrase.as_deref(),
                conversation: None,
            },
            now,
        )?;
        for topic in &item.topics {
            schema::link_topic(db, node, topic, now)?;
        }
    }
    if let Some(summary) = &extraction.summary {
        schema::upsert(
            db,
            &NewMemory {
                kind: MemoryKind::ConversationSummary,
                text: summary,
                phrase: None,
                conversation: Some(conversation_id),
            },
            now,
        )?;
    }
    db.wal_checkpoint()
}

async fn close(db: Option<Arc<GrafeoDB>>) {
    let Some(db) = db else { return };
    if let Err(error) = blocking(move || db.close().map_err(text)).await {
        tracing::warn!("no se pudo cerrar la memoria: {error}");
    }
}

/// Grafeo lee y escribe archivos: fuera del runtime.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| Err(error.to_string()))
}
