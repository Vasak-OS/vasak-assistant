//! El registro del broker: a stderr, que systemd manda al journal.
//!
//! El armado lo comparte con el demonio ([`vasak_assistant_ipc::logging`]). El
//! broker ve direcciones y respuestas del modelo, pero **nunca** las registra:
//! de un pedido deja la operación (`api/chat`, `chat/completions`) y el status,
//! no la ruta ni el cuerpo; las direcciones van recortadas a su origen
//! ([`crate::http::origin`]). Las bibliotecas que ven los cuerpos (HTTP, TLS) se
//! quedan en `info` aunque `RUST_LOG` pida más.

pub use vasak_assistant_ipc::logging::{install_panic_hook, panic_report};

/// Arma el registro del broker: `info` por omisión para este crate, las
/// bibliotecas HTTP/TLS fijas en `info`. El broker no habla con el bus, así que
/// no suma objetivos de biblioteca a los de [`vasak_assistant_ipc::logging`].
pub fn init() {
    vasak_assistant_ipc::logging::init(env!("CARGO_CRATE_NAME"), &[]);
}

/// El filtro de bibliotecas del broker (sólo las de HTTP/TLS). Lo usa la captura
/// del registro de las pruebas ([`crate::test_support::capture_logs`]).
#[cfg(any(test, feature = "test-support"))]
pub fn library_filter() -> tracing_subscriber::filter::Targets {
    vasak_assistant_ipc::logging::library_filter(&[])
}
