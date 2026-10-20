//! La memoria de Lince: una base de grafos propia del asistente (Grafeo,
//! embebida en este mismo proceso), cifrada con AES-256-GCM —el archivo y su
//! WAL— con una llave cruda de 32 bytes que vive en el llavero de la sesión
//! (`crate::keyring`). No va en el almacén de cuentas.
//!
//! **Viene apagada.** «Encendida» quiere decir que existe la carpeta
//! `memory/` dentro de la carpeta de estado del servicio
//! (`$STATE_DIRECTORY`, que pone systemd, o `~/.local/state/vasak-assistant`);
//! no hay nada en `vasak.conf`.
//!
//! - **Encender**: con el llavero desbloqueado, una llave nueva al llavero, se
//!   la relee, y la base se crea en `memory.tmp/` y se renombra a `memory/`
//!   ya completa, con un nodo de metadatos con la versión del formato.
//! - **Apagar es borrar**: primero la llave del llavero (sin ella lo que quede
//!   en disco no se puede leer) y después los archivos, por
//!   `memory.deleting/`. Con el llavero bloqueado los archivos se borran igual.
//! - **Al arrancar** no se genera ni se borra nada solo: si la llave falta o no
//!   abre la base, la memoria queda no disponible con el motivo, y la salida es
//!   apagarla (borrarla). La base se abre la primera vez que alguien pregunta
//!   por ella, así un llavero que se desbloquea después alcanza.
//!
//! En esta fase no se guarda nada de las conversaciones: el almacén queda
//! listo y vacío.

use std::ffi::OsString;
use std::fmt::Display;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grafeo::{Config, EncryptionConfig, GrafeoDB, KeyChain};
use tokio::sync::Mutex;
use vasak_assistant_protocol::{Error, MemoryStatus};
use zeroize::Zeroizing;

use crate::keyring::{MemoryKey, SecretSource, MEMORY_KEY_BYTES};

pub const REASON_OFF: &str = "La memoria está apagada";
pub const REASON_LOCKED: &str = "El llavero está bloqueado";
pub const REASON_MISSING_KEY: &str = "La clave de la memoria no está en el llavero";
pub const REASON_DUPLICATED_KEY: &str = "Hay más de una clave de la memoria en el llavero";
pub const REASON_NOT_OPENED: &str = "La memoria no se pudo abrir";
pub const REASON_NO_KEYRING: &str = "El llavero no responde";
pub const REASON_NO_PLACE: &str = "No hay una carpeta de estado donde guardar la memoria";

/// La versión del formato de la base, en el nodo `:LinceMeta`.
pub const FORMAT_VERSION: i64 = 1;

/// El archivo de la base dentro de `memory/` (Grafeo le agrega al lado su WAL,
/// `lince.grafeo.wal/`).
const DB_FILE: &str = "lince.grafeo";

/// El presupuesto de memoria del motor, por debajo del `MemoryMax=512M` de la
/// unidad. Una base cifrada no vuelca nada a disco (Grafeo no cifra los
/// archivos de desborde), así que este tope no mueve datos: los acota.
const MEMORY_LIMIT: usize = 256 << 20;

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
    /// La base abierta. Un candado de Tokio: encender, apagar y vaciar lo
    /// sostienen de punta a punta, con esperas al llavero y al disco.
    db: Mutex<Option<Arc<GrafeoDB>>>,
}

impl Memory {
    pub fn new(dir: Option<PathBuf>, secrets: Arc<dyn SecretSource>) -> Self {
        Self {
            dir,
            secrets,
            db: Mutex::new(None),
        }
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
    pub async fn status(&self) -> MemoryStatus {
        let Some(dir) = &self.dir else {
            return off(REASON_NO_PLACE);
        };
        let mut db = self.db.lock().await;
        if !is_dir(dir).await {
            // Alguien la borró por fuera con la base abierta.
            close(db.take()).await;
            return off(REASON_OFF);
        }
        let bytes = disk_usage(dir.clone()).await;
        let reason = match self.ensure_open(dir, &mut db).await {
            Ok(()) => "",
            Err(reason) => reason,
        };
        MemoryStatus {
            enabled: true,
            available: reason.is_empty(),
            reason: reason.to_owned(),
            bytes,
        }
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
            return self.erase(dir, &mut db).await;
        }
        if is_dir(dir).await {
            return self
                .ensure_open(dir, &mut db)
                .await
                .map_err(|reason| Error::Unavailable(reason.into()));
        }
        self.create(dir, &mut db).await
    }

    /// Encendida: se borra y se recrea vacía con una llave nueva. Apagada: nada.
    pub async fn clear(&self) -> Result<(), Error> {
        let Some(dir) = &self.dir else {
            return Ok(());
        };
        let mut db = self.db.lock().await;
        if !is_dir(dir).await {
            return Ok(());
        }
        // Antes de borrar nada: con el llavero bloqueado no se podría recrear,
        // y vaciar no puede terminar en apagar.
        if self.secrets.is_locked().await? {
            return Err(locked());
        }
        self.erase(dir, &mut db).await?;
        self.create(dir, &mut db).await
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
            GrafeoDB::with_config(config(&path, &key)).map_err(text)
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

    /// Los pasos de encender, en orden. Con `memory/` ausente.
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

    /// Apagar: la llave primero, después la base cerrada y los archivos.
    async fn erase(&self, dir: &Path, db: &mut Option<Arc<GrafeoDB>>) -> Result<(), Error> {
        if let Err(error) = self.secrets.delete_memory_key().await {
            // Los archivos se borran igual; una llave que quedó no cifra nada
            // y la próxima vez que se encienda se reemplaza.
            tracing::warn!("no se pudo borrar la llave de la memoria: {error}");
        }
        close(db.take()).await;
        let dir = dir.to_owned();
        blocking(move || remove_files(&dir).map_err(text))
            .await
            .map_err(|error| Error::Unavailable(format!("No se pudo borrar la memoria: {error}")))
    }
}

fn off(reason: &str) -> MemoryStatus {
    MemoryStatus {
        enabled: false,
        available: false,
        reason: reason.to_owned(),
        bytes: 0,
    }
}

fn locked() -> Error {
    Error::Unavailable(format!(
        "{REASON_LOCKED}: desbloquealo (al iniciar sesión, o desde el llavero) y volvé a intentarlo."
    ))
}

fn text(error: impl Display) -> String {
    error.to_string()
}

/// La configuración de la base: cifrada, con WAL y con el tope de memoria.
///
/// Cuando haya contenido (todavía no hay esquema), lo que se ajustaría acá y
/// en la base: un índice de propiedad (`CREATE INDEX`) sobre la clave por la
/// que se busque cada recuerdo, y `with_checkpoint_interval` para no esperar
/// al cierre para volcar el WAL. El caché de planes de consulta de Grafeo ya
/// viene encendido.
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
        db.execute(&format!("INSERT (:LinceMeta {{format: {FORMAT_VERSION}}})"))
            .map_err(text)?;
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

/// Lo que ocupan los archivos bajo `dir`, sin seguir enlaces ni abrir la base.
async fn disk_usage(dir: PathBuf) -> u64 {
    fn walk(path: &Path) -> u64 {
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
            .map(|entry| walk(&entry.path()))
            .sum()
    }
    tokio::task::spawn_blocking(move || walk(&dir))
        .await
        .unwrap_or(0)
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
