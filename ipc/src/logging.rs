//! El armado del registro, compartido por el demonio y el broker.
//!
//! Los dos procesos registran a stderr (que systemd manda al journal), con el
//! mismo trato de las bibliotecas que ven los cuerpos (HTTP, TLS y —en el
//! demonio— el bus): nunca pasan de `info`, aunque `RUST_LOG` pida más, porque
//! en `debug`/`trace` vuelcan conversaciones y la cabecera con la clave. Cada
//! proceso le suma a [`library_filter`] los objetivos propios (el demonio,
//! `zbus` y `zvariant`) y le pasa a [`init`] el nombre de su crate para el
//! filtro por omisión.
//!
//! Un pánico deja sólo dónde ocurrió ([`panic_report`]), nunca su mensaje, que
//! podría llevar un valor.

use std::io::IsTerminal;

use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::{EnvFilter, FilterExt, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Arma el registro del proceso: `info` por omisión para `crate_name`
/// (`RUST_LOG` lo mueve), las bibliotecas que ven los cuerpos fijas en `info`
/// (las de [`library_filter`] más `extra_library_targets`), sin colores fuera
/// de una terminal y sin hora propia si va al journal.
pub fn init(crate_name: &str, extra_library_targets: &[&str]) {
    let env =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| format!("{crate_name}=info").into());
    let ansi = std::io::stderr().is_terminal();
    let journal = std::env::var_os("JOURNAL_STREAM").is_some();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(ansi);
    let filter = env.and(library_filter(extra_library_targets));
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

/// Las bibliotecas que ven los cuerpos (HTTP, TLS y las que pasen en
/// `extra_targets`, como el bus en el demonio) no pasan de `info`: en
/// `debug`/`trace` vuelcan conversaciones y la cabecera con la clave. Va aparte
/// del filtro del entorno, así que nada en `RUST_LOG` lo afloja; las pruebas lo
/// usan igual (la captura del registro de [`crate::test_support`]).
pub fn library_filter(extra_targets: &[&str]) -> Targets {
    const BASE: [&str; 6] = ["hyper", "hyper_util", "h2", "reqwest", "rustls", "tower"];
    Targets::new()
        .with_default(LevelFilter::TRACE)
        .with_targets(
            BASE.iter()
                .chain(extra_targets.iter())
                .map(|target| (*target, LevelFilter::INFO)),
        )
}
