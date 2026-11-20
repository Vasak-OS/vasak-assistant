//! Lo que comparten las pruebas del broker (y las de integración del demonio,
//! con la bandera `test-support`): un servidor HTTP de mentira, un llavero de
//! mentira y la captura del registro.
//!
//! Nada acá toca la red de verdad: el servidor escucha en `127.0.0.1:0`.

use std::net::SocketAddr;
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::{Client, Url};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::{select_backend, Selection, Settings};
use crate::keyring::{ApiKey, SecretSource};
use crate::ollama::OllamaBackend;
use vasak_assistant_ipc::BoxFuture;

/// Quien lee o cambia variables de entorno que `reqwest` mira al armar el
/// cliente toma este candado: las pruebas corren en paralelo y el entorno es
/// de todo el proceso.
pub static ENV_LOCK: Mutex<()> = Mutex::new(());

/// El cliente HTTP real del broker, armado sin que otra prueba esté tocando el
/// entorno.
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

pub fn parse_url(text: &str) -> Url {
    Url::parse(text).expect("la dirección de prueba es válida")
}

/// La selección de un backend local en `url`, por el mismo camino que la
/// configuración real.
pub fn local_selection(backend: &str, url: &str, model: Option<&str>) -> Selection {
    select_backend(&Settings {
        backend: Some(backend.into()),
        url: Some(url.into()),
        model: model.map(str::to_owned),
        ..Settings::default()
    })
    .unwrap_or_else(|rejection| panic!("la selección local se acepta: {}", rejection.reason))
}

/// Un `remote_host` de Ollama como el que trae un modelo que corre afuera.
pub const REMOTE_HOST: &str = "https://ollama.com:443";

/// Un `/api/tags` con un solo modelo, `name`, que corre en [`REMOTE_HOST`].
pub fn remote_tags(name: &str) -> String {
    serde_json::json!({"models": [{"name": name, "remote_host": REMOTE_HOST}]}).to_string()
}

/// Un backend de Ollama en `url`. La selección se acepta con `allow_remote` (el
/// arranque no mira la red, así que con un nombre sin sufijo da lo mismo); el
/// backend recibe el `allow_remote` pedido, que es el que decide.
pub fn ollama_backend(url: &str, model: Option<&str>, allow_remote: bool) -> OllamaBackend {
    let selection = select_backend(&Settings {
        backend: Some("ollama".into()),
        url: Some(url.into()),
        model: model.map(str::to_owned),
        allow_remote: true,
        ..Settings::default()
    })
    .unwrap_or_else(|rejection| panic!("{}", rejection.reason));
    OllamaBackend::new(client(), selection, allow_remote)
}

// ── Llavero de mentira ────────────────────────────────────────────────────────

/// Una clave fija; guarda las direcciones por las que le preguntaron.
pub struct FixedKey {
    pub key: Option<ApiKey>,
    pub asked: Mutex<Vec<String>>,
}

impl FixedKey {
    pub fn new(key: Option<&str>) -> Arc<Self> {
        Arc::new(Self {
            key: key.and_then(|key| ApiKey::parse(key.as_bytes())),
            asked: Mutex::default(),
        })
    }
}

impl SecretSource for FixedKey {
    fn api_key<'a>(&'a self, url: &'a str) -> BoxFuture<'a, Option<ApiKey>> {
        self.asked.lock().push(url.to_owned());
        let key = self.key.clone();
        Box::pin(async move { key })
    }
}

// ── Captura del registro ──────────────────────────────────────────────────────

// La infraestructura de la captura la comparten el broker y el demonio
// ([`vasak_assistant_ipc::test_support`]); acá sólo se le pasa el filtro de
// bibliotecas del broker.
pub use vasak_assistant_ipc::test_support::{CaptureGuard, LogCapture};

/// Captura el registro del hilo actual hasta que se suelta el guardián, con el
/// mismo filtro de bibliotecas que el broker ([`crate::logging::library_filter`])
/// y todo lo del broker hasta `trace`.
pub fn capture_logs() -> (LogCapture, CaptureGuard) {
    vasak_assistant_ipc::test_support::capture_logs(crate::logging::library_filter())
}
