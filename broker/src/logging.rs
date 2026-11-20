//! El registro del broker: a stderr, que systemd manda al journal.
//!
//! El broker ve direcciones y respuestas del modelo, pero **nunca** las
//! registra: de un pedido deja la operación (`api/chat`, `chat/completions`) y
//! el status, no la ruta ni el cuerpo; las direcciones van recortadas a su
//! origen ([`crate::http::origin`]). Las bibliotecas que ven los cuerpos (HTTP,
//! TLS) se quedan en `info` aunque `RUST_LOG` pida más.

use std::io::IsTerminal;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::{EnvFilter, FilterExt, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Arma el registro del proceso: `info` por omisión para este crate (`RUST_LOG`
/// lo mueve), las bibliotecas que ven los cuerpos fijas en `info`, sin colores
/// fuera de una terminal y sin hora propia si va al journal.
pub fn init() {
    let env = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| format!("{}=info", env!("CARGO_CRATE_NAME")).into());
    let ansi = std::io::stderr().is_terminal();
    let journal = std::env::var_os("JOURNAL_STREAM").is_some();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(ansi);
    let filter = env.and(library_filter());
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

/// El aviso de pánico: una línea `error` sólo con dónde ocurrió, nunca con el
/// mensaje (que podría llevar un valor).
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

/// Las bibliotecas que ven los cuerpos (HTTP, TLS) no pasan de `info`: en
/// `debug`/`trace` vuelcan conversaciones y la cabecera con la clave. Va aparte
/// del filtro del entorno, así que nada en `RUST_LOG` lo afloja; las pruebas lo
/// usan igual ([`crate::test_support::capture_logs`]).
pub fn library_filter() -> Targets {
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_targets(
            ["hyper", "hyper_util", "h2", "reqwest", "rustls", "tower"]
                .map(|target| (target, LevelFilter::INFO)),
        )
}
