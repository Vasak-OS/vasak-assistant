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

use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
pub const REASON_MODEL_UNREACHABLE: &str = "El modelo no responde";

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
    /// `share_memory_with_remote`: si la memoria se usa también con un modelo
    /// que no es local. Decide la puerta de cada pedido ([`Self::allowed_for`]).
    share_with_remote: bool,
    /// Una sola extracción a la vez, con `try_acquire`: si hay una en curso,
    /// el turno no se aprende. Propio, aparte de los permisos del chat.
    learning: Arc<Semaphore>,
    /// La extracción en vuelo (hay una sola a la vez, por `learning`): de qué
    /// conversación es y su marca de cancelada. `forget_conversation` la marca
    /// si el id coincide, y `store_at` la mira bajo el candado. No tiene tope
    /// que se pueda vaciar borrando otras conversaciones.
    in_flight: parking_lot::Mutex<Option<(String, Arc<AtomicBool>)>>,
    /// La época: sube con cada `set_enabled` y cada `clear`, bajo el candado.
    /// Una extracción lanzada en una época no se guarda en otra.
    epoch: AtomicU64,
    /// Por conversación, desde qué mensaje se puede aprender en la época
    /// actual: lo dicho antes de encender (o de borrar) no se aprende.
    windows: parking_lot::Mutex<HashMap<String, (u64, usize)>>,
    /// Si ya se podó en el episodio actual sobre [`MAX_MEMORY_DISK_BYTES`].
    /// Se limpia cuando la base vuelve a estar dentro del tope.
    pruned_over_limit: Arc<AtomicBool>,
    /// Las conversaciones borradas hace poco (`DeleteConversation`): lo que
    /// llegue de una extracción de alguna de ellas se descarta en
    /// [`Self::store_at`], bajo el candado. Hace de época por conversación:
    /// los ids son uuid y no vuelven, así que basta con recordar que se borró.
    forgotten: parking_lot::Mutex<std::collections::VecDeque<String>>,
}

/// Cuántas conversaciones borradas se recuerdan en `forgotten`. Vaciarlo no
/// deja pasar nada: la extracción en vuelo se cancela por `in_flight`, y el
/// servicio no lanza aprendizajes de una conversación que ya no existe
/// (`chat_with` lo mira con el candado de las conversaciones). `forgotten`
/// queda para quien llame a `learn_after` o `store_at` directamente.
const MAX_FORGOTTEN_CONVERSATIONS: usize = 256;

/// Al terminar la tarea de extracción, de cualquier forma, saca la marca de en
/// vuelo si sigue siendo la suya.
struct InFlight<'a> {
    memory: &'a Memory,
    cancelled: &'a Arc<AtomicBool>,
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let mut in_flight = self.memory.in_flight.lock();
        if in_flight
            .as_ref()
            .is_some_and(|(_, cancelled)| Arc::ptr_eq(cancelled, self.cancelled))
        {
            *in_flight = None;
        }
    }
}

/// Cuántas conversaciones se recuerdan en `windows` antes de olvidar las de
/// épocas viejas (y, si no alcanza, todas: olvidar sólo achica la ventana).
const MAX_TRACKED_CONVERSATIONS: usize = 256;

impl Memory {
    pub fn new(dir: Option<PathBuf>, secrets: Arc<dyn SecretSource>) -> Self {
        Self {
            dir,
            secrets,
            db: Mutex::new(None),
            remote_blocked: false,
            share_with_remote: false,
            learning: Arc::new(Semaphore::new(1)),
            in_flight: parking_lot::Mutex::default(),
            epoch: AtomicU64::new(0),
            windows: parking_lot::Mutex::default(),
            pruned_over_limit: Arc::default(),
            forgotten: parking_lot::Mutex::default(),
        }
    }

    /// Con un backend remoto que no tiene permiso de ver la memoria.
    pub fn with_remote_blocked(mut self, blocked: bool) -> Self {
        self.remote_blocked = blocked;
        self
    }

    /// Si `share_memory_with_remote` está en true.
    pub fn shares_with_remote(&self) -> bool {
        self.share_with_remote
    }

    /// Con `share_memory_with_remote` de `vasak.conf`.
    pub fn with_share_with_remote(mut self, share: bool) -> Self {
        self.share_with_remote = share;
        self
    }

    /// La puerta de cada pedido: si la memoria puede ir al modelo que va a
    /// responder. Se decide con el modelo **efectivo** (`active_model`, el que
    /// resuelve el backend en ese momento), no con el configurado: sin modelo
    /// en `vasak.conf`, Ollama toma el primero instalado, y puede ser uno
    /// `:cloud`. Si el backend no contesta, no (falla cerrado).
    pub async fn allowed_for(&self, backend: &dyn ModelBackend) -> bool {
        self.share_with_remote || backend.active_model().await.is_ok_and(|model| model.local)
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
        let epoch = self.epoch.load(Ordering::SeqCst);
        let Some(current) = conversation.iter().rposition(|m| m.role == Role::User) else {
            return;
        };
        let start = self.window_start(conversation_id, epoch, current);
        let conversation = conversation[start..].to_vec();
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
        // Sin ventana (todo dio positivo en los filtros) no hay nada que
        // pedirle al modelo.
        let Some((conversation, gate)) = learn::extraction_window(&conversation, gate) else {
            return;
        };
        // El permiso y la marca de en vuelo se toman acá, antes de lanzar la
        // tarea: un borrado que llegue después de esta llamada ya la encuentra.
        let Ok(permit) = self.learning.clone().try_acquire_owned() else {
            return;
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        *self.in_flight.lock() = Some((conversation_id.to_owned(), cancelled.clone()));
        let memory = self.clone();
        let conversation_id = conversation_id.to_owned();
        tokio::spawn(async move {
            let _permit = permit;
            let _in_flight = InFlight {
                memory: &memory,
                cancelled: &cancelled,
            };
            // Antes de mandarle nada al modelo: que la memoria se pueda usar
            // y que el modelo que va a extraer pueda verla.
            if memory.database().await.is_err() || !memory.allowed_for(&*backend).await {
                return;
            }
            // Si ya se borró, ni se le pide nada al modelo (`store_at` igual lo
            // descartaría).
            if memory
                .forgotten
                .lock()
                .iter()
                .any(|id| *id == conversation_id)
            {
                return;
            }
            let request = learn::extraction_request(&conversation, gate);
            // Sin permiso para el remoto, la extracción sólo va a un modelo
            // local, decidido con la misma resolución que manda el pedido.
            let sent = if memory.share_with_remote {
                backend.chat(&request).await
            } else {
                backend.chat_local_only(&request).await
            };
            let reply = match sent {
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
            if let Err(reason) = memory.store_at(extraction, &conversation_id, epoch).await {
                tracing::info!("no se guardaron los recuerdos: {reason}");
            }
        });
    }

    /// Guarda lo extraído, bajo el candado y volviendo a comprobar que la
    /// memoria siga encendida y permitida.
    // Sin llamador en el servicio (aprende por `store_at`); lo usan las pruebas.
    #[cfg(test)]
    pub async fn store(&self, extraction: Extraction, conversation_id: &str) -> Result<(), String> {
        let epoch = self.epoch.load(Ordering::SeqCst);
        self.store_at(extraction, conversation_id, epoch).await
    }

    /// [`Self::store`] de una extracción lanzada en la época `epoch`: si desde
    /// entonces se encendió, se apagó o se borró la memoria, se descarta.
    pub async fn store_at(
        &self,
        extraction: Extraction,
        conversation_id: &str,
        epoch: u64,
    ) -> Result<(), String> {
        let dir = self.dir.as_ref().ok_or(REASON_NO_PLACE)?;
        let mut guard = self.db.lock().await;
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return Err("la memoria cambió mientras se extraían los recuerdos".into());
        }
        let cancelled = matches!(
            &*self.in_flight.lock(),
            Some((id, cancelled)) if id == conversation_id && cancelled.load(Ordering::SeqCst)
        );
        if cancelled || self.forgotten.lock().iter().any(|id| id == conversation_id) {
            return Err("la conversación se borró mientras se extraían los recuerdos".into());
        }
        let db = self.usable(dir, &mut guard).await?;
        let (dir, conversation_id) = (dir.clone(), conversation_id.to_owned());
        let pruned = self.pruned_over_limit.clone();
        blocking(move || write(&db, &dir, &extraction, &conversation_id, &pruned).map_err(text))
            .await
    }

    /// Una conversación borrada (`DeleteConversation`): se descarta lo que
    /// esté extrayéndose de ella y se borran sus resúmenes. Nunca falla: si la
    /// memoria no se puede usar, sólo queda la cancelación.
    ///
    /// Con la memoria apagada no se toca la base (apagar no lee ni escribe,
    /// decisión 10), y el resumen de una conversación borrada mientras estaba
    /// apagada queda: no se anota en ningún lado para borrarlo al encender,
    /// porque eso sería escribir con la memoria apagada. La salida para eso es
    /// `ClearMemory`. Lo mismo si la memoria no se pudo abrir (llavero
    /// bloqueado, llave que falta): no se reintenta después.
    pub async fn forget_conversation(&self, conversation_id: &str) {
        if let Some((id, cancelled)) = &*self.in_flight.lock() {
            if id == conversation_id {
                cancelled.store(true, Ordering::SeqCst);
            }
        }
        {
            let mut forgotten = self.forgotten.lock();
            if forgotten.len() >= MAX_FORGOTTEN_CONVERSATIONS {
                forgotten.pop_front();
            }
            forgotten.push_back(conversation_id.to_owned());
        }
        self.windows.lock().remove(conversation_id);
        let Some(dir) = &self.dir else {
            return;
        };
        let mut db = self.db.lock().await;
        if !is_file(&flag(dir)).await {
            return;
        }
        // Borrar no le manda nada a ningún modelo: la puerta del remoto no
        // aplica, sólo que esté encendida y abierta.
        if let Err(reason) = self.ensure_open(dir, &mut db).await {
            tracing::info!("no se borraron los resúmenes de la conversación: {reason}");
            return;
        }
        let Some(db) = db.clone() else { return };
        let id = conversation_id.to_owned();
        if let Err(error) = blocking(move || schema::delete_summaries(&db, &id).map_err(text)).await
        {
            tracing::warn!("no se borraron los resúmenes de la conversación: {error}");
        }
    }

    /// La época actual (sube con cada encender, apagar y borrar). Sólo la
    /// usan las pruebas de `store_at`.
    #[cfg(test)]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// Desde qué mensaje de la conversación se puede aprender en `epoch`. La
    /// primera vez que se ve la conversación en una época, es el turno actual
    /// (`current`): lo anterior se dijo con la memoria apagada o antes de
    /// borrarla.
    fn window_start(&self, conversation_id: &str, epoch: u64, current: usize) -> usize {
        let mut windows = self.windows.lock();
        if windows.len() >= MAX_TRACKED_CONVERSATIONS {
            windows.retain(|_, (seen, _)| *seen == epoch);
            if windows.len() >= MAX_TRACKED_CONVERSATIONS {
                windows.clear();
            }
        }
        let entry = windows
            .entry(conversation_id.to_owned())
            .or_insert((epoch, current));
        if entry.0 != epoch {
            *entry = (epoch, current);
        }
        // El historial recorta lo más viejo: el índice guardado nunca pasa el
        // turno actual.
        entry.1.min(current)
    }

    /// Cierra la base si está abierta (vuelca el WAL al archivo). Se vuelve a
    /// abrir sola la próxima vez que se la necesite.
    // Sólo lo usan las pruebas: apagar y borrar cierran por dentro.
    #[cfg(test)]
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
        // Ya encendida y abierta: nada que hacer. Subir la época descartaría
        // las extracciones en curso y reiniciaría todas las ventanas.
        if enabled && db.is_some() && is_file(&flag(dir)).await {
            return Ok(());
        }
        self.epoch.fetch_add(1, Ordering::SeqCst);
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
        self.epoch.fetch_add(1, Ordering::SeqCst);
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
        let opened = blocking(move || open_existing(&path, &key)).await;
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
    tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(flag(dir))
        .await
        .map(drop)
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

/// Abre una base que ya existe. Nunca crea nada: Grafeo crearía una base
/// nueva donde no hay archivo, y migraría y adoptaría una base 0.5.x sin
/// cifrar. Antes de abrir se exige la cabecera de una base 0.6 cifrada
/// ([`is_encrypted_header`]) y que no haya restos de una migración de Grafeo.
fn open_existing(path: &Path, key: &[u8; MEMORY_KEY_BYTES]) -> Result<GrafeoDB, String> {
    for leftover in ["pre-0.6", "migrating"] {
        let leftover = path.with_extension(format!("grafeo.{leftover}"));
        if std::fs::symlink_metadata(&leftover).is_ok() {
            return Err(format!(
                "hay restos de una migración: {}",
                leftover.display()
            ));
        }
    }
    let mut header = [0u8; HEADER_PREFIX_BYTES];
    std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut header))
        .map_err(|error| format!("no se pudo leer {}: {error}", path.display()))?;
    if !is_encrypted_header(&header) {
        return Err(format!("{} no es una base 0.6 cifrada", path.display()));
    }
    let db = GrafeoDB::with_config(config(path, key)).map_err(text)?;
    schema::ensure_indexes(&db).map_err(text)?;
    Ok(db)
}

/// Cuántos bytes de la cabecera de `.grafeo` hacen falta para decidir.
pub const HEADER_PREFIX_BYTES: usize = 16;

/// Si esos bytes son el principio de la cabecera de un archivo de Grafeo 0.6
/// cifrado. El formato (`grafeo-storage` 0.6.0-beta.1, `file/v3/header.rs`),
/// en little-endian: la marca `GRAF` en el byte 0, la versión del formato
/// `u32` (3) en el 4, el tamaño de página en el 8 y las banderas `u32` en el
/// 12, donde el bit 0 dice que las secciones están cifradas.
pub fn is_encrypted_header(bytes: &[u8]) -> bool {
    let u32_at = |at: usize| {
        bytes
            .get(at..at + 4)
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
            .map(u32::from_le_bytes)
    };
    bytes.get(0..4) == Some(b"GRAF".as_slice())
        && u32_at(4) == Some(3)
        && u32_at(12).is_some_and(|flags| flags & 1 == 1)
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
    pruned: &AtomicBool,
) -> grafeo::Result<()> {
    let now = schema::now();
    if dir_size(dir) > MAX_MEMORY_DISK_BYTES {
        // Una sola poda por episodio sobre el tope: si el directorio sigue
        // pasado por otra causa, podar en cada escritura terminaría borrando
        // toda la memoria. Mientras siga pasado, no se escribe.
        if pruned.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        schema::prune_tenth(db, now)?;
        if dir_size(dir) > MAX_MEMORY_DISK_BYTES {
            tracing::warn!("la memoria llegó a su tope de espacio: no se guarda nada más");
            return Ok(());
        }
    }
    pruned.store(false, Ordering::SeqCst);
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

/// La cabecera de la base y el contador de época.
#[cfg(test)]
mod guard_tests {
    use super::learn::Item;
    use super::*;
    use crate::test_support::{FixedKey, TempDir};

    fn header(magic: &[u8; 4], version: u32, flags: u32) -> Vec<u8> {
        let mut bytes = magic.to_vec();
        bytes.extend(version.to_le_bytes());
        bytes.extend(4096u32.to_le_bytes());
        bytes.extend(flags.to_le_bytes());
        bytes
    }

    #[test]
    fn solo_la_cabecera_de_una_base_0_6_cifrada_se_acepta() {
        assert!(is_encrypted_header(&header(b"GRAF", 3, 1)));
        assert!(is_encrypted_header(&header(b"GRAF", 3, 0b101)));
        for (name, bytes) in [
            ("sin cifrar", header(b"GRAF", 3, 0)),
            ("otras banderas", header(b"GRAF", 3, 0b110)),
            ("versión vieja", header(b"GRAF", 2, 1)),
            ("versión nueva", header(b"GRAF", 4, 1)),
            ("otra marca", header(b"GRAG", 3, 1)),
            ("SQLite", b"SQLite format 3\0".to_vec()),
            ("vacía", Vec::new()),
            ("corta", header(b"GRAF", 3, 1)[..15].to_vec()),
            ("sólo la marca", b"GRAF".to_vec()),
        ] {
            assert!(!is_encrypted_header(&bytes), "{name}");
        }
    }

    /// Cada archivo bajo `path`, con su ruta y su contenido, en orden.
    fn files_under(path: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files_under(&path));
            } else {
                found.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
        found.sort();
        found
    }

    async fn enabled() -> (TempDir, Arc<FixedKey>, Arc<Memory>) {
        let tmp = TempDir::new();
        let keys = FixedKey::new(None);
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), keys.clone()));
        memory.set_enabled(true).await.unwrap();
        (tmp, keys, memory)
    }

    fn restarted(tmp: &TempDir, keys: &Arc<FixedKey>) -> Memory {
        Memory::new(Some(tmp.memory_dir()), keys.clone())
    }

    #[tokio::test]
    async fn la_base_que_crea_el_servicio_tiene_la_cabecera_cifrada_y_una_en_claro_no() {
        let (tmp, _keys, memory) = enabled().await;
        memory.close().await;
        let real = std::fs::read(tmp.memory_dir().join(DB_FILE)).unwrap();
        assert!(is_encrypted_header(&real), "{:?}", &real[..16]);

        // La misma base de Grafeo, sin cifrar, no pasa la prueba.
        let plain = tmp.path().join("plana.grafeo");
        let db = GrafeoDB::with_config(Config::persistent(&plain)).unwrap();
        db.create_node_with_props(&["Memory"], [("text", "hola")])
            .unwrap();
        db.close().unwrap();
        drop(db);
        let plain = std::fs::read(&plain).unwrap();
        assert!(
            !is_encrypted_header(&plain),
            "{:?}",
            &plain[..16.min(plain.len())]
        );
    }

    #[tokio::test]
    async fn una_base_en_claro_en_lugar_de_la_cifrada_no_se_abre_ni_se_toca() {
        let (tmp, keys, memory) = enabled().await;
        memory.close().await;
        let path = tmp.memory_dir().join(DB_FILE);
        std::fs::remove_file(&path).unwrap();
        let _ = std::fs::remove_dir_all(tmp.memory_dir().join("lince.grafeo.wal"));
        let db = GrafeoDB::with_config(Config::persistent(&path)).unwrap();
        db.create_node_with_props(&["Memory"], [("text", "en claro")])
            .unwrap();
        db.close().unwrap();
        drop(db);
        let before = std::fs::read(&path).unwrap();

        let status = restarted(&tmp, &keys).status().await;

        assert!(status.enabled && !status.available, "{status:?}");
        assert_eq!(status.reason, REASON_NOT_OPENED);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // Ni siquiera dejó restos de migración.
        for leftover in ["lince.grafeo.pre-0.6", "lince.grafeo.migrating"] {
            assert!(!tmp.memory_dir().join(leftover).exists(), "{leftover}");
        }
    }

    #[tokio::test]
    async fn una_base_con_la_marca_rota_o_un_archivo_cualquiera_no_se_abre() {
        for content in [&b"esto no es una base"[..], b"", b"GRAF"] {
            let (tmp, keys, memory) = enabled().await;
            memory.close().await;
            let path = tmp.memory_dir().join(DB_FILE);
            std::fs::write(&path, content).unwrap();

            let status = restarted(&tmp, &keys).status().await;

            assert!(!status.available, "{content:?}");
            assert_eq!(status.reason, REASON_NOT_OPENED, "{content:?}");
            assert_eq!(std::fs::read(&path).unwrap(), content);
        }
    }

    #[tokio::test]
    async fn una_cabecera_de_una_version_vieja_o_sin_cifrar_no_se_abre_y_no_se_migra() {
        for (name, version, flags) in [
            ("0.5 sin cifrar", 2, 0),
            ("0.6 sin cifrar", 3, 0),
            ("0.5 con la bandera", 2, 1),
        ] {
            let (tmp, keys, memory) = enabled().await;
            memory.close().await;
            let path = tmp.memory_dir().join(DB_FILE);
            let mut content = header(b"GRAF", version, flags);
            content.resize(16 * 1024, 0);
            std::fs::write(&path, &content).unwrap();
            let before = files_under(&tmp.memory_dir());

            let status = restarted(&tmp, &keys).status().await;

            assert!(!status.available, "{name}");
            assert_eq!(status.reason, REASON_NOT_OPENED, "{name}");
            assert_eq!(std::fs::read(&path).unwrap(), content, "{name}");
            // Grafeo no llegó a migrar nada: ningún archivo nuevo ni cambiado.
            assert_eq!(files_under(&tmp.memory_dir()), before, "{name}");
        }
    }

    #[tokio::test]
    async fn con_restos_de_una_migracion_de_grafeo_no_se_abre_aunque_la_base_este_bien() {
        for leftover in ["lince.grafeo.pre-0.6", "lince.grafeo.migrating"] {
            for as_dir in [false, true] {
                let (tmp, keys, memory) = enabled().await;
                memory.close().await;
                let path = tmp.memory_dir().join(leftover);
                if as_dir {
                    std::fs::create_dir(&path).unwrap();
                } else {
                    std::fs::write(&path, b"resto").unwrap();
                }

                let status = restarted(&tmp, &keys).status().await;

                assert!(status.enabled && !status.available, "{leftover} {as_dir}");
                assert_eq!(status.reason, REASON_NOT_OPENED, "{leftover} {as_dir}");
                assert!(path.exists(), "no se borra nada solo");
            }
        }
    }

    #[tokio::test]
    async fn un_enlace_roto_con_el_nombre_de_un_resto_tambien_impide_abrir() {
        let (tmp, keys, memory) = enabled().await;
        memory.close().await;
        std::os::unix::fs::symlink("/no/existe", tmp.memory_dir().join("lince.grafeo.pre-0.6"))
            .unwrap();

        let status = restarted(&tmp, &keys).status().await;

        assert!(!status.available);
        assert_eq!(status.reason, REASON_NOT_OPENED);
    }

    #[tokio::test]
    async fn borrar_deja_la_memoria_usable_aunque_hubiera_restos_de_una_migracion() {
        let (tmp, keys, memory) = enabled().await;
        memory.close().await;
        std::fs::write(tmp.memory_dir().join("lince.grafeo.pre-0.6"), b"resto").unwrap();
        let memory = restarted(&tmp, &keys);
        assert!(!memory.status().await.available);

        memory.clear().await.unwrap();

        let status = memory.status().await;
        assert!(status.enabled && status.available, "{status:?}");
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

    #[tokio::test]
    async fn cada_encender_apagar_y_borrar_sube_la_epoca_y_una_lectura_no() {
        let (_tmp, _keys, memory) = enabled().await;
        let start = memory.epoch();

        memory.status().await;
        memory.recall("hola", true).await;
        assert_eq!(memory.epoch(), start);

        memory.set_enabled(false).await.unwrap();
        assert_eq!(memory.epoch(), start + 1);
        memory.set_enabled(true).await.unwrap();
        assert_eq!(memory.epoch(), start + 2);
        memory.clear().await.unwrap();
        assert_eq!(memory.epoch(), start + 3);
    }

    #[tokio::test]
    async fn lo_aprendido_en_una_epoca_vieja_se_descarta_y_en_la_actual_se_guarda() {
        let (_tmp, _keys, memory) = enabled().await;
        let old = memory.epoch();
        memory.clear().await.unwrap();

        let stale = memory
            .store_at(preference("Prefiere el mate"), "c", old)
            .await;

        assert!(stale.is_err());
        assert_eq!(memory.recall("mate", true).await, None);
        memory
            .store_at(preference("Prefiere el mate"), "c", memory.epoch())
            .await
            .unwrap();
        assert!(memory.recall("mate", true).await.is_some());
    }

    /// Espera a que la extracción en segundo plano termine (suelta el permiso).
    async fn until_idle(memory: &Memory) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while memory.learning.available_permits() != 1 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "la extracción no terminó"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn turn() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: "prefiero el mate amargo".into(),
            },
            Message {
                role: Role::Assistant,
                content: "Anotado.".into(),
            },
        ]
    }

    const EXTRACTION: &str =
        r#"{"items":[{"kind":"preference","text":"Prefiere el mate amargo"}],"summary":null}"#;

    #[tokio::test]
    async fn un_borrado_en_medio_de_una_extraccion_hace_que_lo_aprendido_no_se_guarde() {
        let (_tmp, _keys, memory) = enabled().await;
        let (backend, gate) = crate::test_support::FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(backend);

        memory.learn_after(backend.clone(), "c1", turn());
        backend.entered.notified().await;
        memory.clear().await.unwrap();
        gate.notify_one();
        until_idle(&memory).await;

        // La memoria quedó encendida y vacía: lo del modelo llegó tarde.
        assert!(memory.status().await.available);
        assert_eq!(memory.recall("mate", true).await, None);
    }

    #[tokio::test]
    async fn apagar_y_volver_a_encender_en_medio_de_una_extraccion_tambien_la_descarta() {
        let (_tmp, _keys, memory) = enabled().await;
        let (backend, gate) = crate::test_support::FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(backend);

        memory.learn_after(backend.clone(), "c1", turn());
        backend.entered.notified().await;
        memory.set_enabled(false).await.unwrap();
        memory.set_enabled(true).await.unwrap();
        gate.notify_one();
        until_idle(&memory).await;

        assert_eq!(memory.recall("mate", true).await, None);
    }

    #[tokio::test]
    async fn sin_cambios_en_medio_la_misma_extraccion_si_se_guarda() {
        let (_tmp, _keys, memory) = enabled().await;
        let (backend, gate) = crate::test_support::FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(backend);

        memory.learn_after(backend.clone(), "c1", turn());
        backend.entered.notified().await;
        gate.notify_one();
        until_idle(&memory).await;

        assert!(memory
            .recall("mate", true)
            .await
            .unwrap()
            .contains("mate amargo"));
    }

    #[tokio::test]
    async fn lo_dicho_antes_de_encender_no_se_aprende_ni_se_manda_al_modelo() {
        let (_tmp, _keys, memory) = enabled().await;
        let backend = crate::test_support::ScriptedBackend::new([EXTRACTION]);
        let mut conversation = vec![
            Message {
                role: Role::User,
                content: "mi perro se llama Rex y es muy bueno".into(),
            },
            Message {
                role: Role::Assistant,
                content: "Qué lindo.".into(),
            },
        ];
        conversation.extend(turn());

        // Es la primera vez que la memoria ve esta conversación: la ventana
        // arranca en el turno actual.
        memory.learn_after(backend.clone(), "c1", conversation);
        backend.wait_for_calls(1).await;

        let request = backend.seen.lock()[0][1].content.clone();
        assert!(request.contains("prefiero el mate amargo"));
        assert!(!request.contains("Rex"), "{request}");
    }

    #[test]
    fn la_ventana_arranca_donde_se_vio_la_conversacion_y_se_reinicia_con_la_epoca() {
        let memory = Memory::new(None, FixedKey::new(None));

        assert_eq!(memory.window_start("c", 0, 4), 4);
        // Los turnos siguientes de la misma época conservan el arranque.
        assert_eq!(memory.window_start("c", 0, 6), 4);
        assert_eq!(memory.window_start("c", 0, 10), 4);
        // El historial recortó lo más viejo: nunca pasa del turno actual.
        assert_eq!(memory.window_start("c", 0, 2), 2);
        // Otra época: arranca de nuevo.
        assert_eq!(memory.window_start("c", 1, 8), 8);
        // Otra conversación, otra ventana.
        assert_eq!(memory.window_start("otra", 1, 0), 0);
    }

    #[test]
    fn la_cuenta_de_ventanas_tiene_tope_y_olvidar_solo_achica_la_ventana() {
        let memory = Memory::new(None, FixedKey::new(None));

        for n in 0..(MAX_TRACKED_CONVERSATIONS * 3) {
            memory.window_start(&format!("c{n}"), 0, 5);
        }

        assert!(memory.windows.lock().len() <= MAX_TRACKED_CONVERSATIONS);
        // Una conversación olvidada vuelve a arrancar en el turno actual.
        assert_eq!(memory.window_start("c0", 0, 9), 9);
    }
}

/// Los arreglos de la revisión: idempotencia de encender y poda una sola vez.
#[cfg(test)]
mod review_tests {
    use super::learn::Item;
    use super::*;
    use crate::test_support::{FakeBackend, FixedKey, TempDir};

    async fn enabled() -> (TempDir, Arc<Memory>) {
        let tmp = TempDir::new();
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), FixedKey::new(None)));
        memory.set_enabled(true).await.unwrap();
        (tmp, memory)
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

    #[tokio::test]
    async fn encender_dos_veces_no_sube_la_epoca_ni_descarta_una_extraccion_en_curso() {
        let (_tmp, memory) = enabled().await;
        let (backend, gate) = FakeBackend::gated(
            r#"{"items":[{"kind":"preference","text":"Prefiere el mate amargo"}],"summary":null}"#,
        );
        let backend = Arc::new(backend);
        let conversation = vec![
            Message {
                role: Role::User,
                content: "prefiero el mate amargo".into(),
            },
            Message {
                role: Role::Assistant,
                content: "Anotado.".into(),
            },
        ];
        let epoch = memory.epoch();

        memory.learn_after(backend.clone(), "c1", conversation);
        backend.entered.notified().await;
        memory.set_enabled(true).await.unwrap();
        memory.set_enabled(true).await.unwrap();
        assert_eq!(memory.epoch(), epoch);
        gate.notify_one();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while memory.recall("mate", true).await.is_none() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "se descartó la extracción"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(memory.epoch(), epoch);
    }

    #[tokio::test]
    async fn encender_dos_veces_conserva_la_ventana_de_la_conversacion() {
        let (_tmp, memory) = enabled().await;
        assert_eq!(memory.window_start("c", memory.epoch(), 4), 4);

        memory.set_enabled(true).await.unwrap();

        assert_eq!(memory.window_start("c", memory.epoch(), 9), 4);
    }

    #[tokio::test]
    async fn encender_una_memoria_apagada_o_con_la_base_cerrada_si_cuenta() {
        let (tmp, memory) = enabled().await;
        let epoch = memory.epoch();
        memory.close().await;

        // Encendida pero cerrada: se reabre (y es un cambio de estado).
        memory.set_enabled(true).await.unwrap();
        assert!(memory.status().await.available);

        memory.set_enabled(false).await.unwrap();
        memory.set_enabled(true).await.unwrap();
        assert_eq!(memory.epoch(), epoch + 3);
        assert!(tmp.path().join("enabled").is_file());
    }

    async fn preferences(memory: &Memory) -> usize {
        let db = memory.database().await.unwrap();
        blocking(move || Ok(schema::entries(&db, MemoryKind::Preference).len()))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn con_el_directorio_pasado_del_limite_se_poda_una_sola_vez_y_no_se_borra_todo() {
        let (tmp, memory) = enabled().await;
        for n in 0..10 {
            memory
                .store(preference(&format!("Prefiere la cosa número {n}")), "c")
                .await
                .unwrap();
        }
        std::fs::write(
            tmp.memory_dir().join("relleno"),
            vec![0u8; MAX_MEMORY_DISK_BYTES as usize + 1],
        )
        .unwrap();

        for n in 0..6 {
            memory
                .store(preference(&format!("Prefiere lo nuevo {n}")), "c")
                .await
                .unwrap();
        }

        // Una poda del 10 % (uno de diez) y nada más; lo nuevo no se escribió.
        assert_eq!(preferences(&memory).await, 9);
        assert_eq!(
            memory
                .recall("nuevo", false)
                .await
                .unwrap()
                .matches("Prefiere lo nuevo")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn al_volver_al_limite_se_escribe_y_un_episodio_nuevo_vuelve_a_podar_una_vez() {
        let (tmp, memory) = enabled().await;
        for n in 0..10 {
            memory
                .store(preference(&format!("Prefiere la cosa número {n}")), "c")
                .await
                .unwrap();
        }
        let filler = tmp.memory_dir().join("relleno");
        let fill =
            || std::fs::write(&filler, vec![0u8; MAX_MEMORY_DISK_BYTES as usize + 1]).unwrap();
        fill();
        memory
            .store(preference("Prefiere lo primero"), "c")
            .await
            .unwrap();
        memory
            .store(preference("Prefiere lo segundo"), "c")
            .await
            .unwrap();
        assert_eq!(preferences(&memory).await, 9);

        // Dentro del límite otra vez: se escribe y el estado se limpia.
        std::fs::remove_file(&filler).unwrap();
        memory
            .store(preference("Prefiere lo tercero"), "c")
            .await
            .unwrap();
        assert_eq!(preferences(&memory).await, 10);

        // Otro episodio: una poda más, y de nuevo sólo una.
        fill();
        memory
            .store(preference("Prefiere lo cuarto"), "c")
            .await
            .unwrap();
        memory
            .store(preference("Prefiere lo quinto"), "c")
            .await
            .unwrap();
        assert_eq!(preferences(&memory).await, 9);
    }
}

/// Olvidar una conversación borrada.
#[cfg(test)]
mod forget_tests {
    use super::learn::Item;
    use super::*;
    use crate::test_support::{FakeBackend, FixedKey, ScriptedBackend, TempDir};

    async fn enabled() -> (TempDir, Arc<Memory>) {
        let tmp = TempDir::new();
        let memory = Arc::new(Memory::new(Some(tmp.memory_dir()), FixedKey::new(None)));
        memory.set_enabled(true).await.unwrap();
        (tmp, memory)
    }

    fn summary(text: &str) -> Extraction {
        Extraction {
            items: Vec::new(),
            summary: Some(text.into()),
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

    async fn summaries(memory: &Memory) -> Vec<String> {
        let db = memory.database().await.unwrap();
        let mut all = blocking(move || {
            Ok(schema::entries(&db, MemoryKind::ConversationSummary)
                .into_iter()
                .map(|e| e.text)
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
        all.sort();
        all
    }

    fn turn() -> Vec<Message> {
        vec![
            Message {
                role: Role::User,
                content: "prefiero el mate amargo".into(),
            },
            Message {
                role: Role::Assistant,
                content: "Anotado.".into(),
            },
        ]
    }

    const EXTRACTION: &str =
        r#"{"items":[{"kind":"preference","text":"Prefiere el mate amargo"}],"summary":null}"#;

    #[tokio::test]
    async fn olvidar_una_conversacion_borra_su_resumen_y_deja_el_de_las_otras() {
        let (_tmp, memory) = enabled().await;
        memory.store(summary("Resumen de A"), "a").await.unwrap();
        memory.store(summary("Resumen de B"), "b").await.unwrap();

        memory.forget_conversation("a").await;

        assert_eq!(summaries(&memory).await, ["Resumen de B"]);
        let block = memory.recall("hola", true).await.unwrap();
        assert!(block.contains("Resumen de B") && !block.contains("Resumen de A"));
    }

    #[tokio::test]
    async fn lo_que_llega_de_una_extraccion_de_una_conversacion_borrada_se_descarta() {
        let (_tmp, memory) = enabled().await;
        let epoch = memory.epoch();
        memory.forget_conversation("a").await;

        let late = memory.store_at(summary("Resumen tardío"), "a", epoch).await;

        assert!(late.is_err());
        assert!(summaries(&memory).await.is_empty());
        // Las demás conversaciones siguen guardando.
        memory
            .store_at(summary("Resumen de B"), "b", epoch)
            .await
            .unwrap();
        assert_eq!(summaries(&memory).await, ["Resumen de B"]);
    }

    #[tokio::test]
    async fn borrar_la_conversacion_en_medio_de_una_extraccion_no_guarda_nada() {
        let (_tmp, memory) = enabled().await;
        let (backend, gate) = FakeBackend::gated(EXTRACTION);
        let backend = Arc::new(backend);

        memory.learn_after(backend.clone(), "a", turn());
        backend.entered.notified().await;
        memory.forget_conversation("a").await;
        gate.notify_one();
        // Espera a que la extracción termine (suelta el permiso).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while memory.learning.available_permits() != 1 {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        assert_eq!(memory.recall("mate", true).await, None);
    }

    #[tokio::test]
    async fn si_la_conversacion_ya_se_borro_la_extraccion_no_llama_al_modelo() {
        let (_tmp, memory) = enabled().await;
        let backend = ScriptedBackend::new([EXTRACTION]);
        memory.forget_conversation("a").await;

        memory.learn_after(backend.clone(), "a", turn());
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(backend.seen.lock().is_empty());
        assert_eq!(memory.recall("mate", true).await, None);
        // Otra conversación sí aprende.
        memory.learn_after(backend.clone(), "b", turn());
        backend.wait_for_calls(1).await;
    }

    #[tokio::test]
    async fn con_la_memoria_apagada_no_se_toca_la_base_y_el_resumen_queda() {
        let (tmp, memory) = enabled().await;
        memory.store(summary("Resumen de A"), "a").await.unwrap();
        memory.set_enabled(false).await.unwrap();
        let files = |dir: &Path| -> Vec<(PathBuf, Vec<u8>)> {
            fn walk(path: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
                for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
                    let p = entry.path();
                    if p.is_dir() {
                        walk(&p, out);
                    } else {
                        out.push((p.clone(), std::fs::read(&p).unwrap()));
                    }
                }
            }
            let mut out = Vec::new();
            walk(dir, &mut out);
            out.sort();
            out
        };
        let before = files(&tmp.memory_dir());

        memory.forget_conversation("a").await;

        assert_eq!(files(&tmp.memory_dir()), before);
        memory.set_enabled(true).await.unwrap();
        assert_eq!(summaries(&memory).await, ["Resumen de A"]);
    }

    #[tokio::test]
    async fn sin_carpeta_de_estado_olvidar_solo_cancela_y_no_falla() {
        let memory = Memory::new(None, FixedKey::new(None));
        memory.forget_conversation("a").await;
        assert!(memory
            .store_at(preference("x"), "a", memory.epoch())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn con_la_llave_ausente_olvidar_no_falla_ni_borra_nada() {
        let (tmp, memory) = enabled().await;
        memory.store(summary("Resumen de A"), "a").await.unwrap();
        memory.close().await;
        let keys = FixedKey::new(None);
        let broken = Memory::new(Some(tmp.memory_dir()), keys);

        broken.forget_conversation("a").await;

        assert!(!broken.status().await.available);
        assert!(tmp.memory_dir().join(DB_FILE).is_file());
    }

    #[tokio::test]
    async fn solo_se_recuerdan_las_ultimas_conversaciones_borradas_y_olvidar_libera_su_ventana() {
        let (_tmp, memory) = enabled().await;
        memory.window_start("primera", memory.epoch(), 4);

        for n in 0..(MAX_FORGOTTEN_CONVERSATIONS + 10) {
            memory.forget_conversation(&format!("c{n}")).await;
        }

        assert!(memory.forgotten.lock().len() <= MAX_FORGOTTEN_CONVERSATIONS);
        // La más vieja ya se olvidó del todo; la última sigue anotada.
        let last = format!("c{}", MAX_FORGOTTEN_CONVERSATIONS + 9);
        assert!(memory.forgotten.lock().iter().any(|id| *id == last));
        assert!(!memory.forgotten.lock().iter().any(|id| id == "c0"));
        memory.forget_conversation("primera").await;
        assert!(!memory.windows.lock().contains_key("primera"));
    }
}
