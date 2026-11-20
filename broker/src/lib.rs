//! El puente de red del asistente.
//!
//! Es el único proceso con `AF_INET`: tiene el cliente HTTP y los backends de
//! modelo, lee la dirección de `vasak.conf` y la expone al demonio por un socket
//! de dominio Unix. El demonio (con los datos de la persona, sin `AF_INET` por
//! el unit) le pide por ese socket; nunca le nombra un destino.
//!
//! La lógica vive en la biblioteca para que las pruebas de integración del
//! demonio puedan manejar un backend real (con la bandera `test-support`); el
//! binario ([`crate::main`] en `main.rs`) es sólo el servidor IPC.

pub mod config;
pub mod http;
pub mod keyring;
pub mod logging;
pub mod ollama;
pub mod openai;
pub mod server;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
