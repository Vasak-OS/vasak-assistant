//! Lo que comparten las pruebas del demonio: un backend de mentira, un llavero
//! de mentira y una conexión D-Bus punto a punto.
//!
//! Los ayudantes de HTTP (el servidor de mentira, el cliente, la selección
//! local, el backend de Ollama de prueba) viven ahora en el broker
//! ([`vasak_assistant_broker::test_support`]), que es quien tiene los backends;
//! las pruebas de integración del demonio los toman de ahí.
//!
//! Nada acá toca la red de verdad ni el bus de sesión: la conexión D-Bus es un
//! par de sockets de la misma prueba.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::sync::Notify;
use vasak_assistant_protocol::{Error, Message, ModelInfo};
use zbus::{connection, Connection};

use crate::backend::{BackendError, BoxFuture, ModelBackend};
use crate::keyring::{ApiKey, MemoryKey, SecretSource, MEMORY_KEY_BYTES};
use crate::memory::Memory;

// ── Llavero de mentira ────────────────────────────────────────────────────────

/// Una clave fija; guarda las direcciones por las que le preguntaron y lo que
/// le pidieron guardar.
///
/// También hace de llavero para la llave de la memoria: la guarda en
/// `memory_key`, se puede bloquear (`locked`) o dejar sin responder
/// (`unreachable`), y anota cada llamada en `calls`, en orden.
pub struct FixedKey {
    pub key: Option<ApiKey>,
    pub asked: Mutex<Vec<String>>,
    /// Cada `store_key`: la dirección y la clave.
    pub stored: Mutex<Vec<(String, ApiKey)>>,
    /// La llave de la memoria tal como está en el llavero (cualquier largo,
    /// para probar una inválida). `None`: no hay ítem.
    pub memory_key: Mutex<Option<Vec<u8>>>,
    /// Si es `true`, el llavero está bloqueado: `is_locked` da `true`,
    /// `memory_key` da `Locked` si hay llave, y guardar o borrar dan
    /// `Unavailable` sin tocarla.
    pub locked: Mutex<bool>,
    /// Si es `true`, el llavero no responde: todo lo de la memoria da
    /// `Unavailable`.
    pub unreachable: Mutex<bool>,
    /// Si es `true`, `memory_key` da `Duplicated` (dos ítems).
    pub duplicated: Mutex<bool>,
    /// Las llamadas sobre la memoria en orden (`is_locked`, `memory_key`,
    /// `store_memory_key`, `delete_memory_key`). Con `watch` puesto, cada una
    /// lleva entre paréntesis si esa ruta existía en ese momento:
    /// `delete_memory_key(true)` quiere decir que la llave se borró con la
    /// carpeta todavía en disco.
    pub calls: Mutex<Vec<String>>,
    pub watch: Mutex<Option<PathBuf>>,
}

impl FixedKey {
    pub fn new(key: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            key: key.and_then(|key| ApiKey::parse(key.as_bytes())),
            asked: Mutex::default(),
            stored: Mutex::default(),
            memory_key: Mutex::default(),
            locked: Mutex::default(),
            unreachable: Mutex::default(),
            duplicated: Mutex::default(),
            calls: Mutex::default(),
            watch: Mutex::default(),
        })
    }

    /// Anota la llamada y dice si el llavero responde.
    fn call(&self, name: &str) -> Result<(), Error> {
        let entry = match &*self.watch.lock() {
            Some(path) => format!("{name}({})", path.exists()),
            None => name.to_owned(),
        };
        self.calls.lock().push(entry);
        if *self.unreachable.lock() {
            return Err(Error::Unavailable("el llavero no responde".into()));
        }
        Ok(())
    }

    fn locked_error(&self) -> Result<(), Error> {
        if *self.locked.lock() {
            return Err(Error::Unavailable("El llavero está bloqueado".into()));
        }
        Ok(())
    }
}

impl SecretSource for FixedKey {
    fn api_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<ApiKey>> {
        self.asked.lock().push(url.to_owned());
        let key = self.key.clone();
        Box::pin(async move { key })
    }

    fn store_key<'a>(&'a self, url: &'a str, key: &'a ApiKey) -> BoxFuture<'a, Result<(), Error>> {
        self.stored.lock().push((url.to_owned(), key.clone()));
        Box::pin(async { Ok(()) })
    }

    /// Hay clave si la fija existe o si se guardó una para esa dirección.
    fn has_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Result<bool, Error>> {
        let has = self.key.is_some() || self.stored.lock().iter().any(|(u, _)| u == url);
        Box::pin(async move { Ok(has) })
    }

    fn is_locked(&self) -> BoxFuture<'_, Result<bool, Error>> {
        let result = self.call("is_locked").map(|()| *self.locked.lock());
        Box::pin(async move { result })
    }

    fn memory_key(&self) -> BoxFuture<'_, Result<MemoryKey, Error>> {
        let result = self.call("memory_key").map(|()| {
            let stored = self.memory_key.lock().clone();
            match stored {
                None => MemoryKey::Missing,
                Some(_) if *self.duplicated.lock() => MemoryKey::Duplicated,
                Some(_) if *self.locked.lock() => MemoryKey::Locked,
                Some(bytes) => <[u8; MEMORY_KEY_BYTES]>::try_from(bytes.as_slice())
                    .map(|key| MemoryKey::Found(zeroize::Zeroizing::new(key)))
                    .unwrap_or(MemoryKey::Invalid),
            }
        });
        Box::pin(async move { result })
    }

    fn store_memory_key<'a>(
        &'a self,
        key: &'a [u8; MEMORY_KEY_BYTES],
    ) -> BoxFuture<'a, Result<(), Error>> {
        let result = self
            .call("store_memory_key")
            .and_then(|()| self.locked_error())
            .map(|()| *self.memory_key.lock() = Some(key.to_vec()));
        Box::pin(async move { result })
    }

    fn delete_memory_key(&self) -> BoxFuture<'_, Result<(), Error>> {
        let result = self
            .call("delete_memory_key")
            .and_then(|()| self.locked_error())
            .map(|()| *self.memory_key.lock() = None);
        Box::pin(async move { result })
    }
}

// ── Etiqueta de AppArmor de mentira ───────────────────────────────────────────

/// Una etiqueta de seguridad fija para quien llame, en lugar de preguntarle al
/// bus: `FixedLabel::of(b"vasak-appimage (enforce)\0")` es un llamador
/// confinado, `FixedLabel::none()` uno sin AppArmor, y `FixedLabel::failing()`
/// un bus que no contesta (se trata como confinado). Anota cuántas veces se
/// preguntó.
pub struct FixedLabel {
    pub label: Result<Option<Vec<u8>>, ()>,
    pub asked: Mutex<usize>,
}

impl FixedLabel {
    pub fn of(label: &[u8]) -> Arc<Self> {
        Arc::new(Self {
            label: Ok(Some(label.to_vec())),
            asked: Mutex::default(),
        })
    }

    pub fn none() -> Arc<Self> {
        Arc::new(Self {
            label: Ok(None),
            asked: Mutex::default(),
        })
    }

    pub fn failing() -> Arc<Self> {
        Arc::new(Self {
            label: Err(()),
            asked: Mutex::default(),
        })
    }
}

impl crate::service::CallerLabels for FixedLabel {
    fn label<'a>(
        &'a self,
        _connection: &'a Connection,
        _sender: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Option<Vec<u8>>, ()>> {
        *self.asked.lock() += 1;
        let label = self.label.clone();
        Box::pin(async move { label })
    }
}

// ── Memoria de prueba ─────────────────────────────────────────────────────────

/// Una memoria sin carpeta de estado: apagada y sin poder encenderse. La de
/// las pruebas que no son de la memoria.
pub fn memory_off() -> Arc<Memory> {
    Arc::new(Memory::new(None, FixedKey::new(None)))
}

/// El estado de una memoria apagada, para `service::capabilities`.
pub fn memory_status_off() -> vasak_assistant_protocol::MemoryStatus {
    vasak_assistant_protocol::MemoryStatus {
        enabled: false,
        available: false,
        reason: crate::memory::REASON_OFF.into(),
        bytes: 0,
    }
}

/// Una carpeta temporal propia de la prueba, que se borra al soltarla. La
/// memoria de una prueba va en `TempDir::new().memory_dir()`.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("vasak-assistant-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("se crea la carpeta temporal");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Donde va `memory/`, como si esta carpeta fuera `$STATE_DIRECTORY`.
    pub fn memory_dir(&self) -> PathBuf {
        self.0.join("memory")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ── Backend de mentira ────────────────────────────────────────────────────────

pub fn model_info(name: &str) -> ModelInfo {
    ModelInfo {
        provider: "ollama".into(),
        name: name.into(),
        local: true,
        host: "127.0.0.1".into(),
    }
}

/// Un backend que contesta lo que se le dice y recuerda lo que le mandaron.
pub struct FakeBackend {
    pub chat_result: Mutex<Result<String, BackendError>>,
    pub models_result: Result<Vec<String>, BackendError>,
    pub active_result: Result<ModelInfo, BackendError>,
    /// Si está, `chat` avisa que entró y espera a que lo liberen.
    pub gate: Option<Arc<Notify>>,
    pub entered: Arc<Notify>,
    pub seen: Mutex<Vec<Vec<Message>>>,
}

impl FakeBackend {
    pub fn replying(text: &str) -> Self {
        Self {
            chat_result: Mutex::new(Ok(text.into())),
            models_result: Ok(vec!["modelo-de-prueba".into()]),
            active_result: Ok(model_info("modelo-de-prueba")),
            gate: None,
            entered: Arc::default(),
            seen: Mutex::default(),
        }
    }

    pub fn failing(error: BackendError) -> Self {
        let backend = Self::replying("");
        *backend.chat_result.lock() = Err(error);
        backend
    }

    /// `chat` espera a que alguien llame a `notify_one` sobre la compuerta.
    pub fn gated(text: &str) -> (Self, Arc<Notify>) {
        let gate = Arc::new(Notify::new());
        let mut backend = Self::replying(text);
        backend.gate = Some(gate.clone());
        (backend, gate)
    }
}

impl ModelBackend for FakeBackend {
    fn info(&self) -> ModelInfo {
        model_info("")
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(async move { self.models_result.clone() })
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        Box::pin(async move { self.active_result.clone() })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            self.seen.lock().push(messages.to_vec());
            self.entered.notify_one();
            if let Some(gate) = &self.gate {
                gate.notified().await;
            }
            self.chat_result.lock().clone()
        })
    }
}

/// Un backend con respuestas programadas, para las pruebas de la memoria:
/// contesta cada `chat` con la siguiente de `replies` (el chat de la persona y
/// después la extracción de recuerdos en JSON, por ejemplo) y anota cada
/// pedido en `seen`. Sin respuestas programadas contesta `""`. `local` decide
/// si se presenta como local o remoto.
pub struct ScriptedBackend {
    pub replies: Mutex<std::collections::VecDeque<Result<String, BackendError>>>,
    pub seen: Mutex<Vec<Vec<Message>>>,
    pub local: bool,
    /// Si el modelo que resuelve cada pedido (`active_model`) es local, cuando
    /// difiere de lo configurado (`info`). `Some(false)` con `local: true`
    /// simula Ollama sin modelo configurado cuyo primer instalado es `:cloud`.
    pub effective_local: Option<bool>,
    /// Si está, `chat_local_only` contesta este error sin mandar nada, sea
    /// cual sea el modelo.
    pub reject_local_only: Mutex<Option<BackendError>>,
}

impl ScriptedBackend {
    pub fn new(replies: impl IntoIterator<Item = &'static str>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().map(|r| Ok(r.to_owned())).collect()),
            seen: Mutex::default(),
            local: true,
            effective_local: None,
            reject_local_only: Mutex::default(),
        })
    }

    /// Como [`Self::new`], pero sin modelo configurado y con el primero
    /// instalado `:cloud`: `info` dice local, `active_model` dice remoto.
    pub fn cloud_by_default(replies: impl IntoIterator<Item = &'static str>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().map(|r| Ok(r.to_owned())).collect()),
            seen: Mutex::default(),
            local: true,
            effective_local: Some(false),
            reject_local_only: Mutex::default(),
        })
    }

    /// Espera hasta que haya `n` pedidos (la extracción corre en otra tarea).
    /// Falla la prueba a los 5 segundos.
    pub async fn wait_for_calls(&self, n: usize) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while self.seen.lock().len() < n {
            assert!(
                tokio::time::Instant::now() < deadline,
                "el backend recibió {} pedidos de {n}",
                self.seen.lock().len()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

impl ModelBackend for ScriptedBackend {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            local: self.local,
            ..model_info("guionado")
        }
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(async { Ok(vec!["guionado".into()]) })
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        let mut info = self.info();
        if let Some(local) = self.effective_local {
            info.local = local;
            info.name = "gpt-oss:120b-cloud".into();
        }
        Box::pin(async move { Ok(info) })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            let reply = self.replies.lock().pop_front().unwrap_or(Ok(String::new()));
            self.seen.lock().push(messages.to_vec());
            reply
        })
    }

    /// Decide con el modelo efectivo (`effective_local`, o `local`); si no es
    /// local no anota ni consume ninguna respuesta: no se mandó nada.
    fn chat_local_only<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            if let Some(error) = self.reject_local_only.lock().clone() {
                return Err(error);
            }
            if !self.effective_local.unwrap_or(self.local) {
                return Err(crate::backend::BackendError::NotLocal);
            }
            self.chat(messages).await
        })
    }
}

/// La carrera entre la puerta y el envío: el modelo resuelto es local la
/// primera vez que se resuelve (`active_model` o `chat_local_only`) y `:cloud`
/// de ahí en adelante, como si en el medio se hubiera hecho un
/// `ollama pull x:cloud`. Responde y anota con el `ScriptedBackend` de
/// adentro; `resolutions` cuenta cuántas veces se resolvió.
// Infraestructura para las pruebas de la carrera, que escribe qa.
#[allow(dead_code)]
pub struct FlippingBackend {
    pub inner: Arc<ScriptedBackend>,
    pub resolutions: std::sync::atomic::AtomicUsize,
}

#[allow(dead_code)]
impl FlippingBackend {
    pub fn new(replies: impl IntoIterator<Item = &'static str>) -> Arc<Self> {
        Arc::new(Self {
            inner: ScriptedBackend::new(replies),
            resolutions: Default::default(),
        })
    }

    /// Resuelve el modelo: local la primera vez, `:cloud` después.
    fn resolve_local(&self) -> bool {
        self.resolutions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
    }
}

impl ModelBackend for FlippingBackend {
    fn info(&self) -> ModelInfo {
        self.inner.info()
    }

    fn models(&self) -> BoxFuture<'_, Result<Vec<String>, BackendError>> {
        self.inner.models()
    }

    fn active_model(&self) -> BoxFuture<'_, Result<ModelInfo, BackendError>> {
        let mut info = self.inner.info();
        if !self.resolve_local() {
            info.local = false;
            info.name = "gpt-oss:120b-cloud".into();
        }
        Box::pin(async move { Ok(info) })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        self.resolve_local();
        self.inner.chat(messages)
    }

    fn chat_local_only<'a>(
        &'a self,
        messages: &'a [Message],
    ) -> BoxFuture<'a, Result<String, BackendError>> {
        if !self.resolve_local() {
            return Box::pin(async { Err(crate::backend::BackendError::NotLocal) });
        }
        self.inner.chat(messages)
    }
}

// ── D-Bus punto a punto ───────────────────────────────────────────────────────

/// Dos conexiones D-Bus unidas por un par de sockets, sin bus de por medio: la
/// primera sirve lo que se le pida, la segunda es el cliente.
pub async fn p2p_pair(
    serve: impl FnOnce(connection::Builder<'static>) -> zbus::Result<connection::Builder<'static>>,
) -> zbus::Result<(Connection, Connection)> {
    p2p(None, serve).await
}

/// Como [`p2p_pair`], pero el cliente lleva el nombre único `sender` y cada
/// llamada sale con ese remitente, como en el bus: el servicio rechaza las
/// llamadas sin remitente (#4). `sender` tiene que ser un nombre único
/// (`:1.1`).
pub async fn p2p_pair_as(
    sender: &str,
    serve: impl FnOnce(connection::Builder<'static>) -> zbus::Result<connection::Builder<'static>>,
) -> zbus::Result<(Connection, Connection)> {
    p2p(Some(sender), serve).await
}

async fn p2p(
    sender: Option<&str>,
    serve: impl FnOnce(connection::Builder<'static>) -> zbus::Result<connection::Builder<'static>>,
) -> zbus::Result<(Connection, Connection)> {
    let (server_end, client_end) = tokio::net::UnixStream::pair().expect("par de sockets");
    let server = serve(
        connection::Builder::unix_stream(server_end)
            .server(zbus::Guid::generate())?
            .p2p(),
    )?
    .build();
    let client = connection::Builder::unix_stream(client_end).p2p().build();
    let (server, client) = tokio::try_join!(server, client)?;
    if let Some(sender) = sender {
        // El constructor sólo pone el nombre único del lado servidor: el
        // cliente lo recibe después.
        client.set_unique_name(sender.to_owned())?;
    }
    Ok((server, client))
}

// ── Habilidades: contexto y llamadas de prueba ──────────────────────────────────

/// El contexto de turno que usan las pruebas de habilidades.
pub fn skill_ctx() -> crate::skills::SkillCtx {
    crate::skills::SkillCtx {
        conversation_id: "conv-1".into(),
    }
}

/// Llama una herramienta de lectura y devuelve su texto. Falla si deja una
/// acción pendiente. Lo comparten las pruebas de todas las habilidades.
pub async fn tool_result<S: crate::skills::Skill>(
    skill: &S,
    name: &str,
    args: serde_json::Value,
) -> Result<String, Error> {
    match skill.call(name, &args, &skill_ctx()).await? {
        crate::skills::ToolOutcome::Result(text) => Ok(text),
        crate::skills::ToolOutcome::Pending { .. } => {
            panic!("la herramienta no debía dejar una acción pendiente")
        }
    }
}

/// Llama una herramienta confirmable y devuelve la acción y su ejecución. Falla
/// si devuelve un resultado directo.
pub async fn tool_pending<S: crate::skills::Skill>(
    skill: &S,
    name: &str,
    args: serde_json::Value,
) -> Result<
    (
        vasak_assistant_protocol::PendingAction,
        crate::skills::SendAction,
    ),
    Error,
> {
    match skill.call(name, &args, &skill_ctx()).await? {
        crate::skills::ToolOutcome::Pending { action, execute } => Ok((action, execute)),
        crate::skills::ToolOutcome::Result(_) => {
            panic!("la herramienta debía dejar una acción pendiente")
        }
    }
}

// ── Un bus de verdad, privado ─────────────────────────────────────────────────

/// Un `dbus-daemon` propio de la prueba, en un socket temporal: dos conexiones
/// a él son dos remitentes distintos (`:1.x`), lo que una conexión punto a
/// punto no puede dar. Nunca toca el bus de sesión de quien corre las pruebas.
pub struct PrivateBus {
    child: std::process::Child,
    pub address: String,
}

impl PrivateBus {
    /// `None` si en la máquina no hay `dbus-daemon`.
    pub fn start() -> Option<Self> {
        use std::io::{BufRead, BufReader, Write};
        let dir =
            std::env::temp_dir().join(format!("vasak-assistant-bus-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).ok()?;
        let config = dir.join("bus.conf");
        let mut file = std::fs::File::create(&config).ok()?;
        write!(
            file,
            r#"<busconfig>
  <type>session</type>
  <listen>unix:dir={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*"/>
    <allow receive_sender="*"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
            dir.display()
        )
        .ok()?;
        let mut child = std::process::Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address=1"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let mut address = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut address)
            .ok()?;
        let address = address.trim().to_owned();
        if address.is_empty() {
            let _ = child.kill();
            return None;
        }
        Some(Self { child, address })
    }

    pub async fn connect(&self) -> Connection {
        connection::Builder::address(self.address.as_str())
            .expect("la dirección del bus privado es válida")
            .build()
            .await
            .expect("se conecta al bus privado")
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl PrivateBus {
    /// Para las pruebas que necesitan un bus: `None` (y la prueba se salta)
    /// sólo fuera del CI. En el CI, un `dbus-daemon` ausente es un fallo y no
    /// un salto en verde.
    pub fn start_or_skip() -> Option<Self> {
        let bus = Self::start();
        if bus.is_none() {
            assert!(
                std::env::var_os("CI").is_none(),
                "en el CI tiene que haber dbus-daemon: la prueba del bus privado no puede saltarse"
            );
            eprintln!("sin dbus-daemon: se salta la prueba del bus privado");
        }
        bus
    }
}

// ── Captura del registro ──────────────────────────────────────────────────────

// La infraestructura de la captura la comparten el demonio y el broker
// ([`vasak_assistant_ipc::test_support`]); acá sólo se le pasa el filtro de
// bibliotecas del demonio (con el bus).
pub use vasak_assistant_ipc::test_support::{
    current_capture, with_capture, CaptureGuard, LogCapture,
};

/// Captura el registro del hilo actual hasta que se suelta el guardián, con el
/// mismo filtro de bibliotecas que el servicio
/// ([`crate::logging::library_filter`]) y todo lo del servicio hasta `trace`. Con
/// `#[tokio::test]` (un solo hilo) cubre también las tareas lanzadas, y lo que
/// corre en `spawn_blocking` a través de `memory::blocking`, que le pasa al hilo
/// de trabajo la captura de quien lo lanza (con [`with_capture`]). Con un runtime
/// de varios hilos, sólo lo del hilo que llama.
///
/// ```ignore
/// let (log, _guard) = capture_logs();
/// // …
/// assert!(!log.text().contains("secreto"));
/// ```
// Infraestructura para las pruebas del registro, que escribe qa.
#[allow(dead_code)]
pub fn capture_logs() -> (LogCapture, CaptureGuard) {
    vasak_assistant_ipc::test_support::capture_logs(crate::logging::library_filter())
}
