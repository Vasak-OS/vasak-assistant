//! La política de destino (con qué backend y en qué dirección responder) vive en
//! el crate compartido `ipc`: la necesitan los dos lados. El broker es el único
//! que la usa para abrir una conexión.
pub use vasak_assistant_ipc::config::*;
