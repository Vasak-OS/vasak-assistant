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

#[cfg(test)]
mod tests {
    use super::learn::Item;
    use super::*;
    use crate::test_support::{FixedKey, ScriptedBackend, TempDir};

    /// Una memoria de prueba: su carpeta, su llavero de mentira y la memoria.
    struct Fixture {
        tmp: TempDir,
        keys: Arc<FixedKey>,
        memory: Arc<Memory>,
    }

    impl Fixture {
        fn new() -> Self {
            let tmp = TempDir::new();
            let keys = FixedKey::new(None);
            let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
            Self { tmp, keys, memory }
        }

        /// Una memoria nueva sobre la misma carpeta y el mismo llavero: como
        /// si el servicio se hubiera reiniciado.
        fn restarted(&self) -> Arc<Memory> {
            Arc::new(Memory::new(Some(self.tmp.memory_dir()), self.keys.clone()))
        }

        async fn enabled() -> Self {
            let fixture = Self::new();
            fixture.memory.set_enabled(true).await.unwrap();
            fixture
        }

        fn files(&self) -> PathBuf {
            self.tmp.memory_dir()
        }

        fn key_is_stored(&self) -> bool {
            self.keys.memory_key.lock().is_some()
        }

        fn calls(&self) -> Vec<String> {
            self.keys.calls.lock().clone()
        }
    }

    fn preference(text: &str) -> Extraction {
        Extraction {
            items: vec![Item {
                kind: MemoryKind::Preference,
                text: text.into(),
                phrase: None,
                topics: Vec::new(),
            }],
            summary: None,
        }
    }

    /// Todos los bytes de los archivos bajo `path`.
    fn bytes_under(path: &Path) -> Vec<Vec<u8>> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(bytes_under(&path));
            } else {
                found.push(std::fs::read(&path).unwrap());
            }
        }
        found
    }

    fn contains(haystack: &[u8], needle: &str) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    }

    #[test]
    fn la_carpeta_de_la_memoria_sale_del_estado_del_servicio_y_se_rechaza_una_relativa() {
        let xdg = Some(PathBuf::from("/home/yo/.local/state"));
        for (state_directory, state_dir, esperado) in [
            (Some("/var/lib/va"), xdg.clone(), Some("/var/lib/va/memory")),
            (
                None,
                xdg.clone(),
                Some("/home/yo/.local/state/vasak-assistant/memory"),
            ),
            (
                Some(""),
                xdg.clone(),
                Some("/home/yo/.local/state/vasak-assistant/memory"),
            ),
            (Some("relativa"), xdg.clone(), None),
            (None, Some(PathBuf::from("relativa")), None),
            (None, None, None),
        ] {
            assert_eq!(
                memory_dir(state_directory.map(OsString::from), state_dir),
                esperado.map(PathBuf::from),
                "{state_directory:?}"
            );
        }
    }

    #[tokio::test]
    async fn viene_apagada_y_no_toca_el_disco_ni_el_llavero() {
        let fixture = Fixture::new();

        let status = fixture.memory.status().await;

        assert!(!status.enabled && !status.available);
        assert_eq!(status.reason, REASON_OFF);
        assert_eq!(status.bytes, 0);
        assert!(!fixture.files().exists());
        assert!(fixture.calls().is_empty());
        assert_eq!(fixture.memory.database().await.err(), Some(REASON_OFF));
        assert_eq!(fixture.memory.recall("hola", true).await, None);
    }

    #[tokio::test]
    async fn sin_carpeta_de_estado_no_se_puede_encender_pero_apagar_no_falla() {
        let memory = Memory::new(None, FixedKey::new(None));

        let status = memory.status().await;
        assert!(!status.enabled && !status.available);
        assert_eq!(status.reason, REASON_NO_PLACE);
        assert!(matches!(
            memory.set_enabled(true).await,
            Err(Error::Unavailable(text)) if text == REASON_NO_PLACE
        ));
        memory.set_enabled(false).await.unwrap();
        memory.clear().await.unwrap();
        assert_eq!(memory.database().await.err(), Some(REASON_NO_PLACE));
        assert!(memory.store(preference("x"), "c").await.is_err());
    }

    #[tokio::test]
    async fn encender_crea_la_base_cifrada_con_una_llave_nueva_en_el_llavero() {
        let fixture = Fixture::new();

        fixture.memory.set_enabled(true).await.unwrap();

        let status = fixture.memory.status().await;
        assert!(status.enabled && status.available, "{status:?}");
        assert!(status.reason.is_empty());
        assert!(status.bytes > 0);
        assert!(fixture.files().join(DB_FILE).is_file());
        assert!(fixture.tmp.path().join("enabled").is_file());
        assert!(!fixture.tmp.path().join("memory.tmp").exists());
        assert_eq!(fixture.keys.memory_key.lock().as_ref().unwrap().len(), 32);
        // Antes de generar una llave se mira que el llavero esté abierto.
        let calls = fixture.calls();
        let locked = calls.iter().position(|c| c == "is_locked").unwrap();
        let stored = calls.iter().position(|c| c == "store_memory_key").unwrap();
        assert!(locked < stored, "{calls:?}");
    }

    #[tokio::test]
    async fn encender_con_el_llavero_bloqueado_no_genera_llave_ni_crea_nada() {
        let fixture = Fixture::new();
        *fixture.keys.locked.lock() = true;

        let result = fixture.memory.set_enabled(true).await;

        assert!(matches!(result, Err(Error::Unavailable(text)) if text.contains("bloqueado")));
        assert!(!fixture.key_is_stored());
        assert!(!fixture.files().exists());
        assert!(!fixture.tmp.path().join("enabled").exists());
        assert!(!fixture.calls().iter().any(|c| c == "store_memory_key"));
        assert!(!fixture.memory.status().await.enabled);
    }

    #[tokio::test]
    async fn encender_dos_veces_no_cambia_la_llave_ni_la_base() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere el mate"), "c")
            .await
            .unwrap();
        let key = fixture.keys.memory_key.lock().clone();

        fixture.memory.set_enabled(true).await.unwrap();

        assert_eq!(*fixture.keys.memory_key.lock(), key);
        assert!(fixture
            .memory
            .recall("mate", false)
            .await
            .unwrap()
            .contains("mate"));
    }

    #[tokio::test]
    async fn una_llave_que_no_se_puede_releer_deja_la_memoria_apagada_y_sin_carpeta() {
        let fixture = Fixture::new();
        // El llavero responde bien al guardar pero devuelve otra cosa al leer.
        *fixture.keys.duplicated.lock() = true;

        let result = fixture.memory.set_enabled(true).await;

        assert!(matches!(result, Err(Error::Unavailable(_))));
        assert!(!fixture.files().exists());
        assert!(!fixture.tmp.path().join("enabled").exists());
    }

    #[tokio::test]
    async fn lo_que_se_aprende_se_encuentra_despues_de_reiniciar_con_la_misma_llave() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere respuestas cortas"), "c")
            .await
            .unwrap();
        fixture.memory.close().await;

        let restarted = fixture.restarted();
        let status = restarted.status().await;
        assert!(status.enabled && status.available, "{status:?}");
        // Por el bloque de recuperación y por la búsqueda de texto del índice.
        assert!(restarted
            .recall("otra cosa", false)
            .await
            .unwrap()
            .contains("Prefiere respuestas cortas"));
        let db = restarted.database().await.unwrap();
        let found = blocking(move || {
            Ok(db
                .text_search(schema::LABEL, "terms", "respuestas", 5, None)
                .map(|hits| hits.len()))
        })
        .await
        .unwrap();
        assert_eq!(found.unwrap(), 1);
    }

    #[tokio::test]
    async fn el_texto_aprendido_no_aparece_en_ningun_byte_de_los_archivos() {
        let fixture = Fixture::enabled().await;
        let canary = "canariodelamemoria";
        fixture
            .memory
            .store(preference(&format!("Prefiere el {canary}")), "c")
            .await
            .unwrap();

        // Con la base todavía abierta y después de cerrarla.
        for closed in [false, true] {
            if closed {
                fixture.memory.close().await;
            }
            let files = bytes_under(&fixture.files());
            assert!(
                files.iter().any(|bytes| !bytes.is_empty()),
                "no hay nada en disco"
            );
            for bytes in &files {
                assert!(!contains(bytes, canary), "cerrada: {closed}");
                assert!(!contains(bytes, "Prefiere"), "cerrada: {closed}");
            }
        }
    }

    #[tokio::test]
    async fn con_otra_llave_la_base_no_abre_y_el_motivo_lo_dice() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere el mate"), "c")
            .await
            .unwrap();
        fixture.memory.close().await;
        *fixture.keys.memory_key.lock() = Some(vec![7u8; 32]);

        let restarted = fixture.restarted();
        let status = restarted.status().await;

        assert!(status.enabled);
        assert!(!status.available);
        assert_eq!(status.reason, REASON_NOT_OPENED);
        assert_eq!(restarted.recall("mate", true).await, None);
        assert!(restarted.store(preference("otro"), "c").await.is_err());
    }

    #[tokio::test]
    async fn cada_problema_con_la_llave_tiene_su_motivo_y_no_borra_nada() {
        for (name, setup, esperado) in [
            ("falta", 0, REASON_MISSING_KEY),
            ("duplicada", 1, REASON_DUPLICATED_KEY),
            ("bloqueada", 2, REASON_LOCKED),
            ("invalida", 3, REASON_NOT_OPENED),
            ("llavero caído", 4, REASON_NO_KEYRING),
        ] {
            let fixture = Fixture::enabled().await;
            fixture.memory.close().await;
            match setup {
                0 => *fixture.keys.memory_key.lock() = None,
                1 => *fixture.keys.duplicated.lock() = true,
                2 => *fixture.keys.locked.lock() = true,
                3 => *fixture.keys.memory_key.lock() = Some(vec![1, 2, 3]),
                _ => *fixture.keys.unreachable.lock() = true,
            }

            let status = fixture.restarted().status().await;

            assert!(status.enabled && !status.available, "{name}");
            assert_eq!(status.reason, esperado, "{name}");
            assert!(fixture.files().join(DB_FILE).is_file(), "{name}");
        }
    }

    #[tokio::test]
    async fn encender_sobre_una_base_que_no_abre_es_unavailable_y_no_la_pisa() {
        let fixture = Fixture::enabled().await;
        fixture.memory.close().await;
        *fixture.keys.memory_key.lock() = None;
        let before = bytes_under(&fixture.files());

        let result = fixture.restarted().set_enabled(true).await;

        assert!(matches!(result, Err(Error::Unavailable(text)) if text == REASON_MISSING_KEY));
        assert_eq!(bytes_under(&fixture.files()), before);
    }

    #[tokio::test]
    async fn apagar_no_borra_nada_y_al_volver_a_encender_el_recuerdo_esta() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere el té de jengibre"), "c")
            .await
            .unwrap();
        let key = fixture.keys.memory_key.lock().clone();

        fixture.memory.set_enabled(false).await.unwrap();

        assert!(fixture.files().join(DB_FILE).is_file());
        assert_eq!(*fixture.keys.memory_key.lock(), key);
        assert!(!fixture
            .calls()
            .iter()
            .any(|c| c.starts_with("delete_memory_key")));
        assert!(!fixture.tmp.path().join("enabled").exists());
        let status = fixture.memory.status().await;
        assert!(!status.enabled && !status.available);
        assert_eq!(status.reason, REASON_OFF);
        // Los bytes se informan también apagada.
        assert!(status.bytes > 0);
        assert_eq!(fixture.memory.recall("té", true).await, None);
        assert!(fixture.memory.store(preference("otro"), "c").await.is_err());

        fixture.memory.set_enabled(true).await.unwrap();

        assert_eq!(*fixture.keys.memory_key.lock(), key);
        assert!(fixture
            .memory
            .recall("té", false)
            .await
            .unwrap()
            .contains("jengibre"));
    }

    #[tokio::test]
    async fn apagar_sin_haber_encendido_nunca_no_falla() {
        let fixture = Fixture::new();
        fixture.memory.set_enabled(false).await.unwrap();
        assert!(!fixture.files().exists());
    }

    #[tokio::test]
    async fn borrar_encendida_vacia_la_base_con_una_llave_nueva_y_sigue_encendida() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere el mate"), "c")
            .await
            .unwrap();
        let old_key = fixture.keys.memory_key.lock().clone().unwrap();

        fixture.memory.clear().await.unwrap();

        let new_key = fixture.keys.memory_key.lock().clone().unwrap();
        assert_ne!(new_key, old_key);
        let status = fixture.memory.status().await;
        assert!(status.enabled && status.available, "{status:?}");
        assert_eq!(fixture.memory.recall("mate", true).await, None);
        assert!(!fixture.tmp.path().join("memory.deleting").exists());
        assert!(!fixture.tmp.path().join("memory.tmp").exists());
        // La base nueva se puede escribir con la llave nueva.
        fixture
            .memory
            .store(preference("Prefiere el café"), "c")
            .await
            .unwrap();
        fixture.memory.close().await;
        assert!(fixture.restarted().recall("café", false).await.is_some());
    }

    #[tokio::test]
    async fn borrar_apagada_borra_la_llave_y_todos_los_archivos() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere el mate"), "c")
            .await
            .unwrap();
        fixture.memory.set_enabled(false).await.unwrap();
        assert!(fixture.files().exists());

        fixture.memory.clear().await.unwrap();

        assert!(!fixture.key_is_stored());
        assert!(!fixture.files().exists());
        let status = fixture.memory.status().await;
        assert!(!status.enabled);
        assert_eq!(status.bytes, 0);
        assert_eq!(status.reason, REASON_OFF);
    }

    #[tokio::test]
    async fn borrar_con_el_llavero_bloqueado_no_toca_ni_la_llave_ni_los_archivos() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere el mate"), "c")
            .await
            .unwrap();
        fixture.memory.close().await;
        let key = fixture.keys.memory_key.lock().clone();
        let before = bytes_under(&fixture.files());
        *fixture.keys.locked.lock() = true;

        let result = fixture.memory.clear().await;

        assert!(matches!(result, Err(Error::Unavailable(text)) if text.contains("bloqueado")));
        assert_eq!(*fixture.keys.memory_key.lock(), key);
        assert_eq!(bytes_under(&fixture.files()), before);
        assert!(fixture.tmp.path().join("enabled").is_file());
        assert!(!fixture.calls().iter().any(|c| c == "delete_memory_key"));
        // Desbloqueado, el recuerdo sigue ahí.
        *fixture.keys.locked.lock() = false;
        assert!(fixture.restarted().recall("mate", false).await.is_some());
    }

    #[tokio::test]
    async fn borrar_con_el_llavero_caido_no_toca_nada() {
        let fixture = Fixture::enabled().await;
        let before = bytes_under(&fixture.files());
        *fixture.keys.unreachable.lock() = true;

        assert!(matches!(
            fixture.memory.clear().await,
            Err(Error::Unavailable(_))
        ));

        assert!(fixture.key_is_stored());
        assert_eq!(bytes_under(&fixture.files()).len(), before.len());
    }

    #[tokio::test]
    async fn borrar_saca_primero_la_llave_y_despues_los_archivos() {
        let fixture = Fixture::enabled().await;
        fixture.memory.set_enabled(false).await.unwrap();
        *fixture.keys.watch.lock() = Some(fixture.files());

        fixture.memory.clear().await.unwrap();

        // `delete_memory_key(true)`: cuando se borró la llave, la carpeta seguía.
        assert!(
            fixture
                .calls()
                .iter()
                .any(|c| c == "delete_memory_key(true)"),
            "{:?}",
            fixture.calls()
        );
        assert!(!fixture.files().exists());
    }

    #[tokio::test]
    async fn borrar_limpia_lo_que_dejo_un_corte_anterior() {
        let fixture = Fixture::enabled().await;
        fixture.memory.set_enabled(false).await.unwrap();
        for leftover in ["memory.deleting", "memory.tmp"] {
            let path = fixture.tmp.path().join(leftover);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("resto"), b"x").unwrap();
        }

        fixture.memory.clear().await.unwrap();

        assert!(!fixture.tmp.path().join("memory.deleting").exists());
        assert!(!fixture.tmp.path().join("memory.tmp").exists());
    }

    #[tokio::test]
    async fn con_un_backend_remoto_sin_permiso_la_memoria_no_se_lee_ni_se_escribe() {
        let fixture = Fixture::enabled().await;
        fixture
            .memory
            .store(preference("Prefiere el mate"), "c")
            .await
            .unwrap();
        fixture.memory.close().await;
        let remote = Arc::new(
            Memory::new(Some(fixture.files()), fixture.keys.clone()).with_remote_blocked(true),
        );

        let status = remote.status().await;

        assert!(status.enabled && !status.available);
        assert_eq!(status.reason, REASON_REMOTE);
        assert_eq!(remote.database().await.err(), Some(REASON_REMOTE));
        assert_eq!(remote.recall("mate", true).await, None);
        assert!(matches!(
            remote.store(preference("otro"), "c").await,
            Err(text) if text == REASON_REMOTE
        ));
        // Con permiso, el mismo recuerdo está.
        let allowed = Memory::new(Some(fixture.files()), fixture.keys.clone());
        assert!(allowed.recall("mate", true).await.is_some());
    }

    #[tokio::test]
    async fn pasado_el_tope_de_espacio_se_poda_y_si_sigue_pasado_no_se_escribe() {
        let fixture = Fixture::enabled().await;
        for n in 0..10 {
            fixture
                .memory
                .store(preference(&format!("Prefiere la cosa número {n}")), "c")
                .await
                .unwrap();
        }
        let count = |memory: Arc<Memory>| async move {
            let db = memory.database().await.unwrap();
            blocking(move || Ok(schema::entries(&db, MemoryKind::Preference).len()))
                .await
                .unwrap()
        };
        assert_eq!(count(fixture.memory.clone()).await, 10);
        // Un archivo que por sí solo pasa el tope.
        std::fs::write(
            fixture.files().join("relleno"),
            vec![0u8; MAX_MEMORY_DISK_BYTES as usize + 1],
        )
        .unwrap();

        fixture
            .memory
            .store(preference("Prefiere lo nuevo"), "c")
            .await
            .unwrap();

        // Podó el 10 % (uno), y como sigue pasada, no guardó lo nuevo.
        assert_eq!(count(fixture.memory.clone()).await, 9);
        assert!(!fixture
            .memory
            .recall("nuevo", false)
            .await
            .unwrap()
            .contains("lo nuevo"));
        // Sin el relleno vuelve a escribir.
        std::fs::remove_file(fixture.files().join("relleno")).unwrap();
        fixture
            .memory
            .store(preference("Prefiere lo nuevo"), "c")
            .await
            .unwrap();
        assert!(fixture
            .memory
            .recall("nuevo", false)
            .await
            .unwrap()
            .contains("lo nuevo"));
    }

    #[tokio::test]
    async fn la_extraccion_en_segundo_plano_se_guarda_y_una_en_curso_hace_que_el_turno_no_se_aprenda(
    ) {
        let fixture = Fixture::enabled().await;
        let backend = ScriptedBackend::new([
            r#"{"items":[{"kind":"preference","text":"Prefiere el mate"}],"summary":null}"#,
        ]);
        let conversation = vec![
            Message {
                role: Role::User,
                content: "prefiero el mate al café".into(),
            },
            Message {
                role: Role::Assistant,
                content: "Anotado.".into(),
            },
        ];

        // Con el permiso tomado por otra extracción, el turno se saltea.
        let held = fixture.memory.learning.try_acquire().unwrap();
        fixture
            .memory
            .learn_after(backend.clone(), "c1", conversation.clone());
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(backend.seen.lock().is_empty());
        drop(held);

        fixture
            .memory
            .learn_after(backend.clone(), "c1", conversation);
        backend.wait_for_calls(1).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while fixture.memory.recall("mate", false).await.is_none() {
            assert!(tokio::time::Instant::now() < deadline, "no se guardó");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
