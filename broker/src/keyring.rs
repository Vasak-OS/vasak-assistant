//! La clave del proveedor, del lado del broker.
//!
//! En esta PR el broker **no** lee el llavero: el demonio lee la clave (es el
//! dueño natural: `SetProviderKey`, su `SecretSource` sobre el Secret Service) y
//! se la pasa al broker en el sobre del IPC. Los tipos que los dos lados usan
//! con el mismo código —[`ApiKey`], el rasgo angosto [`SecretSource`],
//! [`ITEM_SERVICE`] y [`RequestKey`] (la clave que llegó en el sobre)— viven en
//! el crate compartido ([`vasak_assistant_ipc::keyring`]); acá sólo se
//! reexportan. (PR2 mueve la lectura del llavero a este proceso.)

pub use vasak_assistant_ipc::keyring::{ApiKey, RequestKey, SecretSource, ITEM_SERVICE};
