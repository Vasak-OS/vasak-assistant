//! Lo que comparten las pruebas del demonio: un servidor HTTP de mentira, un
//! backend de mentira y una conexión D-Bus punto a punto.
//!
//! Nada acá toca la red de verdad ni el bus de sesión: el servidor escucha en
//! `127.0.0.1:0` y la conexión D-Bus es un par de sockets de la misma prueba.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::{Client, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use vasak_assistant_protocol::{Error, Message, ModelInfo};
use zbus::{connection, Connection};

use crate::backend::{BackendError, BoxFuture, ModelBackend};
use crate::config::{select_backend, Selection, Settings};
use crate::keyring::{ApiKey, MemoryKey, SecretSource, MEMORY_KEY_BYTES};
use crate::memory::Memory;

/// Quien lee o cambia variables de entorno que `reqwest` mira al armar el
/// cliente toma este candado: las pruebas corren en paralelo y el entorno es
/// de todo el proceso.
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// El cliente HTTP real del demonio, armado sin que otra prueba esté tocando
/// el entorno.
pub fn client() -> Client {
    let _env = ENV_LOCK.lock();
    crate::http::client().expect("el cliente HTTP se arma")
}

// ── Servidor HTTP de mentira ──────────────────────────────────────────────────

/// Un pedido tal como llegó al servidor.
#[derive(Debug, Clone)]
pub struct Captured {
    pub method: String,
    pub path: String,
    /// Nombres en minúsculas.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("el cuerpo del pedido es JSON")
    }
}

/// Lo que contesta el servidor.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// El `Content-Length` a declarar; por omisión el largo real del cuerpo.
    pub declared_length: Option<usize>,
    /// Si es `false` no se declara largo y el cuerpo termina al cerrar la
    /// conexión.
    pub declare_length: bool,
}

impl Reply {
    pub fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.into(),
            declared_length: None,
            declare_length: true,
        }
    }

    pub fn json(body: &str) -> Self {
        let mut reply = Self::new(200, body);
        reply
            .headers
            .push(("content-type".into(), "application/json".into()));
        reply
    }

    pub fn status(status: u16) -> Self {
        Self::new(status, "")
    }

    pub fn redirect(location: &str) -> Self {
        let mut reply = Self::new(302, "");
        reply.headers.push(("location".into(), location.into()));
        reply
    }

    /// Sin `Content-Length`: el cliente tiene que leer hasta el cierre.
    pub fn without_length(mut self) -> Self {
        self.declare_length = false;
        self
    }

    /// Declara un largo que no es el del cuerpo.
    pub fn declaring(mut self, length: usize) -> Self {
        self.declared_length = Some(length);
        self
    }
}

pub struct FakeServer {
    pub addr: SocketAddr,
    pub requests: Arc<Mutex<Vec<Captured>>>,
}

impl FakeServer {
    /// Un servidor que contesta con lo que devuelva `handler`.
    pub async fn start(handler: impl Fn(&Captured) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("el puerto de prueba se abre");
        let addr = listener.local_addr().expect("tiene dirección");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler = Arc::new(handler);
        let seen = requests.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = handler.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    // Los errores de E/S acá son el cliente cortando a propósito.
                    let _ = serve(stream, &*handler, &seen).await;
                });
            }
        });
        Self { addr, requests }
    }

    /// Un servidor que acepta la conexión y nunca contesta.
    pub async fn silent() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("el puerto de prueba se abre");
        let addr = listener.local_addr().expect("tiene dirección");
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        Self {
            addr,
            requests: Arc::default(),
        }
    }

    /// La dirección base de la API (`http://127.0.0.1:PUERTO/` + `path`).
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn captured(&self) -> Vec<Captured> {
        self.requests.lock().clone()
    }
}

/// Un puerto en el que no escucha nadie.
pub fn closed_port_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("puerto libre");
    let addr = listener.local_addr().expect("tiene dirección");
    drop(listener);
    format!("http://{addr}/")
}

async fn serve(
    mut stream: TcpStream,
    handler: &(dyn Fn(&Captured) -> Reply + Send + Sync),
    seen: &Mutex<Vec<Captured>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break at;
        }
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(key, _)| key == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    let captured = Captured {
        method,
        path,
        headers,
        body,
    };
    let reply = handler(&captured);
    seen.lock().push(captured);

    let mut response = format!("HTTP/1.1 {} X\r\nconnection: close\r\n", reply.status);
    for (key, value) in &reply.headers {
        response.push_str(&format!("{key}: {value}\r\n"));
    }
    if reply.declare_length {
        response.push_str(&format!(
            "content-length: {}\r\n",
            reply.declared_length.unwrap_or(reply.body.len())
        ));
    }
    response.push_str("\r\n");
    stream.write_all(response.as_bytes()).await?;
    stream.write_all(&reply.body).await?;
    stream.shutdown().await
}

/// La selección de un backend local en `url`, por el mismo camino que la
/// configuración real.
pub fn local_selection(backend: &str, url: &str, model: Option<&str>) -> Selection {
    select_backend(&Settings {
        backend: Some(backend.into()),
        url: Some(url.into()),
        model: model.map(str::to_owned),
        allow_remote: false,
        share_memory_with_remote: false,
    })
    .unwrap_or_else(|rejection| panic!("la selección local se acepta: {}", rejection.reason))
}

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
// Infraestructura para las pruebas de la puerta de AppArmor, que escribe qa.
#[allow(dead_code)]
pub struct FixedLabel {
    pub label: Result<Option<Vec<u8>>, ()>,
    pub asked: Mutex<usize>,
}

#[allow(dead_code)]
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
}

impl ScriptedBackend {
    pub fn new(replies: impl IntoIterator<Item = &'static str>) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().map(|r| Ok(r.to_owned())).collect()),
            seen: Mutex::default(),
            local: true,
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
        let info = self.info();
        Box::pin(async move { Ok(info) })
    }

    fn chat<'a>(&'a self, messages: &'a [Message]) -> BoxFuture<'a, Result<String, BackendError>> {
        Box::pin(async move {
            let reply = self.replies.lock().pop_front().unwrap_or(Ok(String::new()));
            self.seen.lock().push(messages.to_vec());
            reply
        })
    }
}

// ── D-Bus punto a punto ───────────────────────────────────────────────────────

/// Dos conexiones D-Bus unidas por un par de sockets, sin bus de por medio: la
/// primera sirve lo que se le pida, la segunda es el cliente.
pub async fn p2p_pair(
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
    tokio::try_join!(server, client)
}

pub fn parse_url(text: &str) -> Url {
    Url::parse(text).expect("la dirección de prueba es válida")
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
