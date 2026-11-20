//! Con qué backend responder, según `vasak.conf`.
//!
//! La lógica vive en el crate compartido [`vasak_assistant_ipc::config`] porque
//! la necesitan los dos lados del servicio: el broker (el único que arma la URL
//! y abre la conexión) y el demonio, que sirve `info()` de su propia config y
//! lee la clave del llavero por la misma dirección base normalizada. Acá sólo se
//! reexporta para que el resto del demonio la use como `crate::config`.
pub use vasak_assistant_ipc::config::*;
