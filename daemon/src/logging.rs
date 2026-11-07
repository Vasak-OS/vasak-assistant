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
//! direcciones pasan por [`redact_url`] (o `http::origin`), y de un pedido a un
//! backend se registra la operación (`chat/completions`, `api/chat`), no la
//! ruta. Los textos de los errores del contrato ya están escritos para
//! mostrarse y no llevan nada de eso. De un error de Grafeo se cuenta sólo su
//! código, de uno de E/S la ruta y la clase (`memory`), y de un error de serde
//! en `vasak.conf` la línea, la columna y la clase (`config`). Un pánico deja
//! sólo dónde ocurrió ([`panic_report`]), nunca su mensaje. Las bibliotecas
//! que ven los cuerpos (el bus, HTTP, TLS) no pasan de `info` aunque `RUST_LOG`
//! pida más.

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
    install_panic_hook();
}

/// Instala el aviso de pánico: una línea `error` sólo con dónde ocurrió
/// ([`panic_report`]), nunca con el mensaje. El de Rust por omisión escribe el
/// mensaje en stderr, que va al journal, y ese mensaje puede llevar un valor.
/// Si todavía no hay registro, la misma línea va directo a stderr.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let report = panic_report(info.location());
        if tracing::dispatcher::has_been_set() {
            tracing::error!("{report}");
        } else {
            eprintln!("{report}");
        }
    }));
}

/// Lo que se cuenta de un pánico: el archivo y la línea donde ocurrió.
pub fn panic_report(location: Option<&std::panic::Location<'_>>) -> String {
    match location {
        Some(location) => format!("pánico en {}:{}", location.file(), location.line()),
        None => "pánico en un lugar desconocido".to_owned(),
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

    /// Una llamada sin cabecera, para las pruebas: todos los métodos del
    /// servicio llevan la suya ([`Self::new`]).
    #[cfg(test)]
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

    /// Registra en `debug` una llamada que no puede fallar.
    pub fn done(self) {
        let ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::debug!(
            method = self.method,
            sender = self.sender.as_str(),
            conversation = self.conversation.as_str(),
            ms,
            "llamada atendida"
        );
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

/// La línea de arranque: la versión y un resumen de la configuración. Nunca
/// una dirección, una clave ni el modelo de un remoto con su ruta: sólo el
/// proveedor, si es local, los dos permisos y si la memoria está encendida.
pub fn startup_summary(
    settings: &crate::config::Settings,
    backend: &vasak_assistant_protocol::ModelInfo,
    memory_enabled: bool,
) -> String {
    format!(
        "arrancando vasak-assistant {}: backend {} ({}), allow_remote={}, share_memory_with_remote={}, memoria {}",
        env!("CARGO_PKG_VERSION"),
        backend.provider,
        // Con Ollama, un alias puede correr afuera: al arrancar sólo se sabe la
        // dirección, el modelo se confirma en cada pedido.
        match (backend.local, backend.provider.as_str()) {
            (false, _) => "remoto",
            (true, vasak_assistant_protocol::PROVIDER_OLLAMA) => "local según la dirección",
            (true, _) => "local",
        },
        settings.allow_remote,
        settings.share_memory_with_remote,
        if memory_enabled { "encendida" } else { "apagada" },
    )
}

/// La línea del backend elegido: proveedor, origen (esquema, host y puerto,
/// sin credenciales ni ruta), si es local y el modelo configurado.
pub fn backend_summary(selection: &crate::config::Selection) -> String {
    format!(
        "backend elegido: {} en {} ({}), modelo {}",
        crate::config::provider_name(selection.kind),
        crate::http::origin(&selection.url),
        if selection.local { "local" } else { "remoto" },
        selection
            .model
            .as_deref()
            .unwrap_or("(el primero que ofrezca)")
    )
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

/// Las líneas de arranque: contenido y nada de credenciales ni rutas.
#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::config::Settings;
    use crate::test_support::{local_selection, model_info};

    fn settings(allow_remote: bool, share: bool) -> Settings {
        Settings {
            backend: Some("openai".into()),
            url: Some("https://usuariocanario:clavecanaria@api.ejemplo.com/v1/cuentacanaria?token=consultacanaria".into()),
            model: Some("modelo-canario".into()),
            allow_remote,
            share_memory_with_remote: share,
        }
    }

    #[test]
    fn el_arranque_dice_version_proveedor_permisos_y_memoria() {
        let local = model_info("llama");
        let remote = vasak_assistant_protocol::ModelInfo {
            local: false,
            provider: "openai".into(),
            ..model_info("gpt")
        };
        for (settings, info, memory, pieces) in [
            (
                settings(false, false),
                local.clone(),
                false,
                vec![
                    "ollama (local según la dirección)",
                    "allow_remote=false",
                    "share_memory_with_remote=false",
                    "memoria apagada",
                ],
            ),
            (
                settings(true, true),
                remote.clone(),
                true,
                vec![
                    "openai (remoto)",
                    "allow_remote=true",
                    "share_memory_with_remote=true",
                    "memoria encendida",
                ],
            ),
            (
                settings(true, false),
                local,
                true,
                vec![
                    "(local según la dirección)",
                    "allow_remote=true",
                    "share_memory_with_remote=false",
                    "memoria encendida",
                ],
            ),
        ] {
            let line = startup_summary(&settings, &info, memory);
            assert!(
                line.starts_with(&format!(
                    "arrancando vasak-assistant {}",
                    env!("CARGO_PKG_VERSION")
                )),
                "{line}"
            );
            for piece in pieces {
                assert!(line.contains(piece), "{piece}: {line}");
            }
        }
    }

    #[test]
    fn solo_con_ollama_el_arranque_dice_local_segun_la_direccion() {
        let openai = vasak_assistant_protocol::ModelInfo {
            provider: "openai".into(),
            ..model_info("gpt")
        };
        let line = startup_summary(&settings(false, false), &openai, false);
        assert!(line.contains("openai (local)"), "{line}");
        assert!(!line.contains("según la dirección"), "{line}");
        let line = startup_summary(&settings(false, false), &model_info("llama"), false);
        assert!(line.contains("ollama (local según la dirección)"), "{line}");
    }

    #[test]
    fn el_arranque_no_repite_la_direccion_ni_el_usuario_ni_la_clave_ni_la_ruta_ni_el_modelo() {
        for share in [false, true] {
            let line = startup_summary(&settings(true, share), &model_info("modelo-canario"), true);
            for secret in [
                "usuariocanario",
                "clavecanaria",
                "cuentacanaria",
                "consultacanaria",
                "api.ejemplo.com",
                "modelo-canario",
                "https://",
                "/v1",
            ] {
                assert!(!line.contains(secret), "{secret}: {line}");
            }
        }
    }

    #[test]
    fn el_backend_elegido_dice_proveedor_origen_local_o_remoto_y_modelo() {
        let server = "http://127.0.0.1:11434/";
        let local = local_selection("ollama", server, Some("llama3.2"));
        let line = backend_summary(&local);
        assert_eq!(
            line,
            "backend elegido: ollama en http://127.0.0.1:11434 (local), modelo llama3.2"
        );

        let no_model = local_selection("ollama", server, None);
        assert!(backend_summary(&no_model).contains("modelo (el primero que ofrezca)"));

        let mut remote = local_selection("openai", server, Some("gpt"));
        remote.local = false;
        remote.url = reqwest::Url::parse("https://api.ejemplo.com/v1/").unwrap();
        let line = backend_summary(&remote);
        assert_eq!(
            line,
            "backend elegido: openai en https://api.ejemplo.com (remoto), modelo gpt"
        );
    }

    #[test]
    fn el_backend_elegido_no_repite_usuario_clave_ruta_ni_consulta() {
        let mut selection = local_selection("openai", "http://127.0.0.1:8080/v1/", Some("gpt"));
        selection.url = reqwest::Url::parse(
            "https://usuariocanario:clavecanaria@api.ejemplo.com:8443/v1/cuentacanaria?token=consultacanaria#frag",
        )
        .unwrap();
        selection.local = false;

        let line = backend_summary(&selection);

        assert!(line.contains("https://api.ejemplo.com:8443"), "{line}");
        for secret in [
            "usuariocanario",
            "clavecanaria",
            "cuentacanaria",
            "consultacanaria",
            "frag",
            "/v1",
            "@",
        ] {
            assert!(!line.contains(secret), "{secret}: {line}");
        }
    }
}

/// El aviso de pánico: dónde ocurrió y nunca el mensaje.
#[cfg(test)]
mod panic_tests {
    use super::*;
    use crate::test_support::capture_logs;
    use std::panic::{catch_unwind, panic_any, AssertUnwindSafe};

    /// El hook de pánico es de todo el proceso: una prueba lo cambia a la vez.
    static HOOK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn panic_report_con_ubicacion_da_archivo_y_linea_y_sin_ella_un_texto_fijo() {
        let here = std::panic::Location::caller();
        let report = panic_report(Some(here));
        assert_eq!(report, format!("pánico en {}:{}", here.file(), here.line()));
        assert!(report.contains("logging.rs"), "{report}");
        assert_eq!(panic_report(None), "pánico en un lugar desconocido");
        // No hay por dónde entrar un mensaje: sólo la ubicación.
        assert!(!report.contains("mensaje"));
    }

    #[test]
    fn un_panico_deja_en_el_registro_la_ubicacion_y_nunca_el_mensaje() {
        let _lock = HOOK.lock();
        let previous = std::panic::take_hook();
        install_panic_hook();
        let (log, _guard) = capture_logs();

        let by_macro = catch_unwind(|| panic!("mensajecanarioqzx con clave sk-valorcanarioqzx"));
        let by_unwrap = catch_unwind(|| {
            std::hint::black_box(Err::<(), String>("errorcanarioqzx".into())).unwrap();
        });
        let by_expect = catch_unwind(|| {
            std::hint::black_box(None::<u8>).expect("expectcanarioqzx");
        });
        let by_payload = catch_unwind(|| panic_any(String::from("payloadcanarioqzx")));
        let by_index = catch_unwind(AssertUnwindSafe(|| {
            let v = std::hint::black_box([1u8]);
            v[usize::from(v[0]) + 9]
        }));

        std::panic::set_hook(previous);
        for (name, result) in [
            ("macro", by_macro.is_err()),
            ("unwrap", by_unwrap.is_err()),
            ("expect", by_expect.is_err()),
            ("payload", by_payload.is_err()),
            ("índice", by_index.is_err()),
        ] {
            assert!(result, "{name} tenía que entrar en pánico");
        }
        let text = log.text();
        assert!(!text.contains("canarioqzx"), "{text}");
        assert!(!text.contains("index out of bounds"), "{text}");
        let lines: Vec<_> = text.lines().filter(|l| l.contains("pánico en")).collect();
        assert_eq!(lines.len(), 5, "{text}");
        for l in lines {
            assert!(l.contains("ERROR"), "{l}");
            assert!(l.contains("logging.rs:"), "{l}");
        }
    }

    /// Hijo del siguiente: sin registro, el aviso va a stderr. Sólo hace algo si
    /// el padre lo invoca con la variable puesta.
    #[test]
    fn hijo_que_entra_en_panico_sin_registro() {
        if std::env::var_os("VASAK_PRUEBA_PANICO").is_none() {
            return;
        }
        install_panic_hook();
        let _ = catch_unwind(|| panic!("mensajecanarioqzx"));
        let _ = catch_unwind(|| {
            std::hint::black_box(Err::<(), String>("errorcanarioqzx".into())).unwrap();
        });
    }

    #[test]
    fn sin_registro_el_aviso_va_a_stderr_con_la_ubicacion_y_sin_el_mensaje() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "logging::panic_tests::hijo_que_entra_en_panico_sin_registro",
                "--exact",
                "--nocapture",
            ])
            .env("VASAK_PRUEBA_PANICO", "1")
            .env_remove("RUST_BACKTRACE")
            .output()
            .unwrap();

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{stderr}");
        assert_eq!(stderr.matches("pánico en ").count(), 2, "{stderr}");
        assert!(stderr.contains("logging.rs:"), "{stderr}");
        assert!(!stderr.contains("canarioqzx"), "{stderr}");
        assert!(
            !stderr.contains("panicked at"),
            "el aviso de Rust no tiene que salir: {stderr}"
        );
    }
}
