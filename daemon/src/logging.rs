//! El registro: a stderr, que systemd manda al journal.
//!
//! Qué se registra: el ciclo de vida (arranque con la versión y un resumen de
//! la configuración, parada, encender, apagar y borrar la memoria), cada
//! llamada D-Bus que termina en error (con el método, quien llama, la
//! conversación en corto y el tipo de error) y lo que falla en segundo plano.
//! En `debug`, además, cada llamada que sale bien y los pedidos a los backends
//! con su status HTTP y su duración.
//!
//! **Qué no se registra nunca**: el contenido de los mensajes ni de las
//! respuestas, las notas, los recuerdos ni los resúmenes, las claves ni los
//! tokens, ni una dirección con usuario, contraseña, ruta o consulta. Las
//! direcciones pasan por [`redact_url`] (o `http::origin`); los textos de los
//! errores del contrato ya están escritos para mostrarse y no llevan nada de
//! eso; y las bibliotecas que ven los cuerpos (el bus, HTTP, TLS) no pasan de
//! `info` aunque `RUST_LOG` pida más.

use std::io::IsTerminal;
use std::time::Instant;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::{EnvFilter, FilterExt, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;
use vasak_assistant_protocol::Error;
use zbus::message::Header;
use zbus::DBusError;

/// Arma el registro del proceso.
///
/// - Nivel `info` por omisión para este crate; `RUST_LOG` lo sube o lo baja.
/// - Las bibliotecas que ven los cuerpos se quedan en `info` pase lo que pase.
/// - Sin colores si stderr no es una terminal (el journal), y sin la hora
///   propia si stderr va al journal (`JOURNAL_STREAM`): journald pone la suya.
pub fn init() {
    // El nombre del crate lo pone el compilador: el binario es
    // `vasak-assistant` pero el paquete es `vasak-assistant-daemon`.
    let env = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("{}=info", env!("CARGO_CRATE_NAME")).into());
    let libraries = library_filter();
    let ansi = std::io::stderr().is_terminal();
    let journal = std::env::var_os("JOURNAL_STREAM").is_some();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(ansi);
    let filter = env.and(libraries);
    let registry = tracing_subscriber::registry();
    if journal {
        registry
            .with(layer.without_time().with_filter(filter))
            .init();
    } else {
        registry.with(layer.with_filter(filter)).init();
    }
}

/// Las bibliotecas que ven los cuerpos (el bus, HTTP, TLS) no pasan de `info`:
/// en `debug` o `trace` vuelcan mensajes del bus, conversaciones y la cabecera
/// con la clave. Va aparte del filtro del entorno, así que nada en `RUST_LOG`
/// lo afloja; las pruebas lo usan igual (`test_support::capture_logs`).
pub fn library_filter() -> Targets {
    Targets::new()
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
        )
}

/// Lo que se puede registrar de una dirección: esquema, host y puerto, sin
/// usuario, contraseña, ruta, consulta ni fragmento. Una dirección que no se
/// entiende, o que no tiene host, no se muestra.
pub fn redact_url(raw: &str) -> String {
    match reqwest::Url::parse(raw.trim()) {
        Ok(url) if url.host_str().is_some() => crate::http::origin(&url),
        _ => "(dirección ilegible)".into(),
    }
}

/// Los primeros 8 caracteres de un id de conversación, para reconocerla en el
/// registro sin el id entero. Sólo letras, dígitos y `-`: lo que llega en una
/// llamada puede ser cualquier cosa.
pub fn short_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(8)
        .collect()
}

/// El nombre del error del contrato, sin su texto.
pub fn error_kind(error: &Error) -> &'static str {
    match error {
        Error::ZBus(_) => "ZBus",
        Error::Unavailable(_) => "Unavailable",
        Error::Backend(_) => "Backend",
        Error::InvalidArgs(_) => "InvalidArgs",
        Error::NotFound(_) => "NotFound",
        Error::LimitExceeded(_) => "LimitExceeded",
        Error::Busy(_) => "Busy",
        Error::AccessDenied(_) => "AccessDenied",
    }
}

/// Una llamada D-Bus en curso, para registrar cómo terminó con los mismos
/// campos en todos los métodos: `method`, `sender`, `conversation` (corto),
/// `error` (el tipo) y `ms`.
pub struct Call {
    method: &'static str,
    sender: String,
    conversation: String,
    started: Instant,
}

impl Call {
    pub fn new(method: &'static str, header: &Header<'_>) -> Self {
        Self {
            method,
            sender: header
                .sender()
                .map_or_else(|| "(sin remitente)".to_owned(), |s| s.to_string()),
            conversation: String::new(),
            started: Instant::now(),
        }
    }

    /// Una llamada de un método que no mira quién llama.
    pub fn method(method: &'static str) -> Self {
        Self {
            method,
            sender: String::new(),
            conversation: String::new(),
            started: Instant::now(),
        }
    }

    /// La conversación de la llamada, en corto.
    pub fn conversation(mut self, id: &str) -> Self {
        self.conversation = short_id(id);
        self
    }

    /// Registra el resultado y lo devuelve. Un error del bus es inesperado
    /// (`error`); los demás son rechazos esperables o degradaciones (`warn`),
    /// con su texto, que está escrito para mostrarse y no lleva contenido.
    /// Lo que sale bien va en `debug`.
    pub fn finish<T>(self, result: Result<T, Error>) -> Result<T, Error> {
        let ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (method, sender, conversation) = (
            self.method,
            self.sender.as_str(),
            self.conversation.as_str(),
        );
        match &result {
            Ok(_) => tracing::debug!(method, sender, conversation, ms, "llamada atendida"),
            Err(error @ Error::ZBus(_)) => tracing::error!(
                method,
                sender,
                conversation,
                ms,
                error = error_kind(error),
                "la llamada falló en el bus: {error}"
            ),
            Err(error) => tracing::warn!(
                method,
                sender,
                conversation,
                ms,
                error = error_kind(error),
                "la llamada se rechazó: {}",
                error.description().unwrap_or_default()
            ),
        }
        result
    }
}
