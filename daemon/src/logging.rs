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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::capture_logs;

    #[test]
    fn redact_url_deja_solo_esquema_host_y_puerto() {
        for (raw, esperado) in [
            ("https://api.openai.com/v1", "https://api.openai.com"),
            ("https://api.openai.com/v1/", "https://api.openai.com"),
            ("http://127.0.0.1:11434/", "http://127.0.0.1:11434"),
            ("http://localhost:8080/v1?x=1#frag", "http://localhost:8080"),
            (
                "https://usuario:clavesecreta@api.ejemplo.com/ruta/privada?token=abc#f",
                "https://api.ejemplo.com",
            ),
            ("https://solo@api.ejemplo.com", "https://api.ejemplo.com"),
            ("  https://api.ejemplo.com/v1  ", "https://api.ejemplo.com"),
            ("https://[::1]:8443/v1", "https://[::1]:8443"),
            ("HTTPS://API.Ejemplo.COM/V1", "https://api.ejemplo.com"),
        ] {
            let redacted = redact_url(raw);
            assert_eq!(redacted, esperado, "{raw}");
            for secret in [
                "usuario",
                "clavesecreta",
                "privada",
                "token",
                "abc",
                "frag",
                "solo@",
            ] {
                assert!(!redacted.contains(secret), "{raw}: {redacted}");
            }
        }
    }

    #[test]
    fn redact_url_con_algo_ilegible_o_sin_host_no_muestra_nada_de_lo_que_vino() {
        for raw in [
            "",
            "   ",
            "no es una dirección",
            "usuario:clavesecreta@ejemplo.com",
            "file:///etc/passwd",
            "mailto:ana@ejemplo.com?subject=secreto",
            "https://",
            "http://:clavesecreta@/?token=abc",
            "data:text/plain,clavesecreta",
        ] {
            let redacted = redact_url(raw);
            assert_eq!(redacted, "(dirección ilegible)", "{raw:?}");
        }
    }

    #[test]
    fn short_id_deja_ocho_caracteres_seguros_de_lo_que_llegue() {
        assert_eq!(short_id("123e4567-e89b-12d3-a456-426614174000"), "123e4567");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
        // Lo que llega por el bus puede ser cualquier cosa: nada de saltos ni comillas.
        assert_eq!(short_id("a\nb\"c d\u{202e}e"), "abcde");
        assert_eq!(short_id("\n\n\n"), "");
        assert_eq!(short_id("ñandú-12345678901"), "and-1234");
    }

    #[test]
    fn error_kind_nombra_cada_error_sin_su_texto() {
        for (error, nombre) in [
            (Error::Unavailable("secreto".into()), "Unavailable"),
            (Error::Backend("secreto".into()), "Backend"),
            (Error::InvalidArgs("secreto".into()), "InvalidArgs"),
            (Error::NotFound("secreto".into()), "NotFound"),
            (Error::LimitExceeded("secreto".into()), "LimitExceeded"),
            (Error::Busy("secreto".into()), "Busy"),
            (Error::AccessDenied("secreto".into()), "AccessDenied"),
        ] {
            assert_eq!(error_kind(&error), nombre);
        }
        assert_eq!(
            error_kind(&Error::ZBus(zbus::Error::Failure("x".into()))),
            "ZBus"
        );
    }

    #[test]
    fn el_filtro_de_bibliotecas_corta_en_info_lo_que_ven_los_cuerpos_y_deja_pasar_lo_propio() {
        let (log, _guard) = capture_logs();

        tracing::trace!(target: "zbus::connection", "cuerpo-del-bus-trace");
        tracing::debug!(target: "zbus::connection", "cuerpo-del-bus-debug");
        tracing::debug!(target: "hyper_util::client", "cabecera-con-la-clave");
        tracing::trace!(target: "reqwest::connect", "url-con-clave");
        tracing::debug!(target: "rustls::client", "handshake");
        tracing::info!(target: "zbus::connection", "aviso-del-bus-info");
        tracing::trace!(target: "vasak_assistant", "propio-trace");
        tracing::debug!(target: "otra_biblioteca", "otra-debug");

        let text = log.text();
        for hidden in [
            "cuerpo-del-bus-trace",
            "cuerpo-del-bus-debug",
            "cabecera-con-la-clave",
            "url-con-clave",
            "handshake",
        ] {
            assert!(!text.contains(hidden), "{hidden}\n{text}");
        }
        for shown in ["aviso-del-bus-info", "propio-trace", "otra-debug"] {
            assert!(text.contains(shown), "{shown}\n{text}");
        }
    }

    #[test]
    fn una_llamada_atendida_va_en_debug_y_un_rechazo_en_warn_con_sus_campos() {
        let (log, _guard) = capture_logs();

        let ok = Call::method("Models").finish(Ok(1));
        let denied: Result<(), Error> = Call::method("ClearMemory")
            .conversation("123e4567-e89b-12d3-a456-426614174000")
            .finish(Err(Error::AccessDenied("no se puede".into())));
        let bus: Result<(), Error> =
            Call::method("Chat").finish(Err(Error::ZBus(zbus::Error::Failure("roto".into()))));

        assert_eq!(ok.unwrap(), 1);
        assert!(denied.is_err() && bus.is_err());
        let text = log.text();
        let line = |needle: &str| {
            text.lines()
                .find(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("falta «{needle}»:\n{text}"))
                .to_owned()
        };
        let attended = line("llamada atendida");
        assert!(
            attended.contains("DEBUG") && attended.contains("method=\"Models\""),
            "{attended}"
        );
        let rejected = line("la llamada se rechazó");
        assert!(rejected.contains("WARN"), "{rejected}");
        assert!(rejected.contains("method=\"ClearMemory\""), "{rejected}");
        assert!(rejected.contains("conversation=\"123e4567\""), "{rejected}");
        assert!(rejected.contains("error=\"AccessDenied\""), "{rejected}");
        assert!(rejected.contains("no se puede"), "{rejected}");
        assert!(!rejected.contains("123e4567-e89b"), "{rejected}");
        let failed = line("la llamada falló en el bus");
        assert!(
            failed.contains("ERROR") && failed.contains("error=\"ZBus\""),
            "{failed}"
        );
    }
}
