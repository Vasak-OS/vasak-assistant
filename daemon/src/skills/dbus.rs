//! Lo que las habilidades comparten para hablarle al bus de cuentas y a
//! `vasak-permissions`: las direcciones del almacén, los plazos, el armado de un
//! proxy y el parseo de los argumentos que manda el modelo. Vivía duplicado en
//! cada habilidad; acá se escribe una sola vez.

use std::time::Duration;

use serde_json::Value;
use zbus::Connection;

use vasak_assistant_protocol::Error;

// ── Direcciones del bus de cuentas ──────────────────────────────────────────────
//
// El almacén local lo sirve el servicio dueño del nombre
// `ar.net.vasak.os.AccountsSync`, en el objeto `AccountsStore`, por el bus de
// **sesión**: es un servicio del usuario.

pub(crate) const ACCOUNTS_SERVICE: &str = "ar.net.vasak.os.AccountsSync";
pub(crate) const STORE_PATH: &str = "/ar/net/vasak/os/AccountsStore";
pub(crate) const STORE_INTERFACE: &str = "ar.net.vasak.os.AccountsStore";

/// Plazo de una lectura o de `GetStatus`: zbus 5 no tiene timeout por método, y
/// un servicio conectado pero que no responde dejaría la llamada —y el lugar de
/// conversación del turno— esperando para siempre. **No** se usa para una
/// llamada que puede abrir un diálogo y esperar a la persona (el permiso): esa
/// no lleva plazo.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// El bus donde está `vasak-permissions`.
///
/// En producción, siempre el del **sistema**: ahí publica el servicio de
/// permisos, que identifica a quien llama por su pid y binario. En una
/// compilación de depuración con `VASAK_PERMISSIONS_TEST_ROOT` —el mismo
/// disparador con el que el servicio se corre en el bus de sesión— el de
/// sesión. Fuera de la compilación de producción por completo, no detrás de un
/// `if`: un agente al que se pudiera apuntar a un bus que controla el usuario no
/// estaría preguntándole al servicio de permisos.
#[cfg(debug_assertions)]
pub(crate) async fn permission_bus() -> zbus::Result<Connection> {
    if std::env::var_os("VASAK_PERMISSIONS_TEST_ROOT").is_some() {
        return Connection::session().await;
    }
    Connection::system().await
}

#[cfg(not(debug_assertions))]
pub(crate) async fn permission_bus() -> zbus::Result<Connection> {
    Connection::system().await
}

/// Un proxy al almacén de cuentas sobre `connection` (el bus de sesión). Se arma
/// por llamada, que es barato.
pub(crate) async fn store_proxy(connection: &Connection) -> Result<zbus::Proxy<'_>, Error> {
    zbus::Proxy::new(connection, ACCOUNTS_SERVICE, STORE_PATH, STORE_INTERFACE)
        .await
        .map_err(backend_error)
}

/// El `execute` de una creación por el almacén: arma un proxy nuevo, llama
/// `method` con `args` —los cuatro textos que toman `PutEvent`/`PutContact`:
/// cuenta, colección, payload e `if_match`— y descarta el JSON de vuelta. Lo
/// comparten crear un evento y crear un contacto; se corre recién al confirmar.
pub(crate) fn store_create(
    connection: Connection,
    method: &'static str,
    args: (String, String, String, String),
) -> super::SendAction {
    Box::new(
        move || -> crate::backend::BoxFuture<'static, Result<(), Error>> {
            Box::pin(async move {
                let proxy = store_proxy(&connection).await?;
                let _created: String =
                    call_with_timeout(CALL_TIMEOUT, proxy.call(method, &args)).await?;
                Ok(())
            })
        },
    )
}

/// Si el almacén de cuentas responde a `GetStatus`: para informar la capacidad.
/// `GetStatus` no abre ningún diálogo ni pide permiso: si contesta, el almacén
/// está en pie. Ante cualquier duda, `false` (la capacidad se ve no disponible,
/// nunca rota).
pub(crate) async fn store_alive(connection: &Connection) -> bool {
    let Ok(proxy) = store_proxy(connection).await else {
        return false;
    };
    let status = proxy.call::<_, _, String>("GetStatus", &());
    matches!(tokio::time::timeout(CALL_TIMEOUT, status).await, Ok(Ok(_)))
}

/// Envuelve una llamada D-Bus en un plazo y traduce su error: zbus 5 no tiene
/// timeout por método. Si se vence, el servicio está conectado pero no
/// responde, y la llamada no espera para siempre.
pub(crate) async fn call_with_timeout<T>(
    timeout: Duration,
    call: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T, Error> {
    match tokio::time::timeout(timeout, call).await {
        Ok(result) => result.map_err(backend_error),
        Err(_) => Err(Error::Backend(
            "el servicio de cuentas no respondió a tiempo".into(),
        )),
    }
}

/// El valor de un argumento de texto, o `InvalidArgs` si falta o no es texto.
pub(crate) fn string_arg(args: &Value, key: &str) -> Result<String, Error> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Error::InvalidArgs(format!("falta el argumento «{key}»")))
}

/// El valor de un argumento de texto opcional: vacío si falta o es nulo.
/// `InvalidArgs` si está pero no es texto —un número no es "el evento entero":
/// el modelo tiene que corregirlo, no caer en silencio en el valor por omisión—.
pub(crate) fn opt_string_arg(args: &Value, key: &str) -> Result<String, Error> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(value)) => Ok(value.clone()),
        Some(_) => Err(Error::InvalidArgs(format!("«{key}» no es texto"))),
    }
}

/// Un argumento entero, o `InvalidArgs` si falta o no es un número entero.
pub(crate) fn int_arg(args: &Value, key: &str) -> Result<i64, Error> {
    args.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| Error::InvalidArgs(format!("falta el argumento «{key}» o no es un número")))
}

/// Un argumento booleano opcional, con su valor por omisión si falta o es nulo.
/// `InvalidArgs` si está pero no es booleano —`"true"` como texto no cuenta: el
/// modelo tiene que corregirlo, no caer en silencio en el valor por omisión—.
pub(crate) fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool, Error> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err(Error::InvalidArgs(format!("«{key}» no es booleano"))),
    }
}

/// Un argumento de lista de textos opcional: vacío si falta. `InvalidArgs` si
/// está pero no es una lista, o si tiene un elemento que no es texto.
pub(crate) fn string_array_arg(args: &Value, key: &str) -> Result<Vec<String>, Error> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str().map(str::to_owned).ok_or_else(|| {
                    Error::InvalidArgs(format!("«{key}» tiene un elemento que no es texto"))
                })
            })
            .collect(),
        Some(_) => Err(Error::InvalidArgs(format!("«{key}» no es una lista"))),
    }
}

/// Un error del bus de cuentas o permisos, para la persona, sin filtrar nada
/// delicado del cable.
pub(crate) fn backend_error(error: impl std::fmt::Display) -> Error {
    Error::Backend(format!("el servicio de cuentas no respondió: {error}"))
}

#[cfg(test)]
#[path = "dbus_tests.rs"]
mod tests;
