//! Contrato interno del asistente entre el demonio y el broker de red.
//!
//! El demonio (con los datos de la persona) no puede abrir un socket IP: su
//! unit lo deja con `RestrictAddressFamilies=AF_UNIX` (barrera del kernel). Para
//! hablar con el modelo le pide al broker por un socket de dominio Unix. El
//! broker es el único proceso con `AF_INET`, el único que lee la dirección de la
//! configuración y el único que puede nombrar un host.
//!
//! Este crate tiene el rasgo [`backend::ModelBackend`] y sus tipos (el contrato
//! que ambos lados comparten), el protocolo del puente ([`wire`]) y el
//! enmarcado del socket. No va en `protocol` —que se publica y es mínimo—
//! porque nada de afuera del servicio lo necesita.

pub mod backend;
// La política de destino: con qué backend y en qué dirección responder, según
// `vasak.conf`. Vive acá porque la necesitan los dos lados: el broker (el único
// que arma la URL y abre el socket) y el demonio (que sirve `info()` de su
// propia config y lee la clave por la misma dirección base normalizada).
pub mod config;
// El armado del registro y las piezas del llavero (la clave y de dónde sale),
// compartidos por el demonio y el broker con el mismo código.
pub mod keyring;
pub mod logging;
pub mod wire;

// Ayudantes de prueba compartidos (la captura del registro). Sólo con la bandera
// `test-support`, que las pruebas de los dos crates encienden.
#[cfg(feature = "test-support")]
pub mod test_support;

use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub use backend::{
    flatten_messages, BackendError, BoxFuture, ChatMessage, ModelBackend, ModelTurn, ToolCall,
    ToolDef, METADATA_BUSY, NOT_LOCAL, REMOTE_AFTER_SEND,
};
pub use wire::{BrokerEnvelope, BrokerRequest, BrokerResponse, WebContent};

/// El subdirectorio del asistente dentro de `$XDG_RUNTIME_DIR`. Lo crea systemd
/// con `RuntimeDirectory=vasak-assistant` (modo `0700`): escribible aun con
/// `ProtectHome=read-only`, que deja `/run/user/<uid>` de sólo lectura.
pub const RUNTIME_SUBDIR: &str = "vasak-assistant";

/// El nombre del socket del broker dentro de [`RUNTIME_SUBDIR`]. En
/// `$XDG_RUNTIME_DIR`, y no en `/tmp`, porque esa carpeta la crea systemd con
/// permisos sólo para el usuario (`0700`) y se limpia al cerrar la sesión.
pub const SOCKET_NAME: &str = "vasak-assistant-broker.sock";

/// El tope del cuerpo de cualquier respuesta de un backend. El broker corta la
/// lectura del HTTP en este tamaño: un servidor que manda de más no hace crecer
/// la memoria.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// El tope de un marco del IPC. Acota la memoria del enmarcado igual que
/// [`MAX_RESPONSE_BYTES`] acota la lectura HTTP; se le deja margen porque la
/// respuesta (ya recortada a 256 KiB) viaja envuelta en JSON, que escapa y
/// agrega llaves. Un marco más grande se rechaza antes de reservar la memoria.
pub const MAX_FRAME_BYTES: usize = MAX_RESPONSE_BYTES * 2;

/// La ruta del socket del broker:
/// `$XDG_RUNTIME_DIR/vasak-assistant/vasak-assistant-broker.sock`. `None` si no
/// hay `XDG_RUNTIME_DIR` (fuera de una sesión de usuario). El subdirectorio lo
/// crea systemd (`RuntimeDirectory=`) para el broker; el demonio sólo conecta.
pub fn socket_path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    if dir.is_empty() {
        return None;
    }
    Some(PathBuf::from(dir).join(RUNTIME_SUBDIR).join(SOCKET_NAME))
}

/// Escribe un valor como un marco: `u32` big-endian con el largo del cuerpo,
/// seguido del cuerpo JSON. Un cuerpo que pasa [`MAX_FRAME_BYTES`] no se escribe
/// (el otro lado lo rechazaría igual).
pub async fn write_frame<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let body = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::other(format!(
            "el marco del IPC pasa el tope de {MAX_FRAME_BYTES} bytes"
        )));
    }
    let len = u32::try_from(body.len()).map_err(std::io::Error::other)?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await
}

/// Lee un marco escrito por [`write_frame`]. Un largo declarado que pasa
/// [`MAX_FRAME_BYTES`] se rechaza sin reservar la memoria.
pub async fn read_frame<R, T>(reader: &mut R) -> std::io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::other(format!(
            "el marco del IPC declara {len} bytes, más que el tope de {MAX_FRAME_BYTES}"
        )));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    serde_json::from_slice(&body).map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn un_marco_va_y_vuelve_igual() {
        let envelope = BrokerEnvelope {
            api_key: Some("sk-x".into()),
            key_base_url: None,
            request: BrokerRequest::Chat {
                messages: vec![vasak_assistant_protocol::Message {
                    role: vasak_assistant_protocol::Role::User,
                    content: "hola".into(),
                }],
            },
        };
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &envelope).await.unwrap();
        let mut slice = buffer.as_slice();
        let read: BrokerEnvelope = read_frame(&mut slice).await.unwrap();
        assert_eq!(read, envelope);
    }

    #[tokio::test]
    async fn un_marco_mas_grande_que_el_tope_se_rechaza_al_leer() {
        // Un largo declarado enorme no reserva la memoria.
        let mut framed = ((MAX_FRAME_BYTES as u32) + 1).to_be_bytes().to_vec();
        framed.extend_from_slice(b"{}");
        let mut slice = framed.as_slice();
        let result: std::io::Result<BrokerResponse> = read_frame(&mut slice).await;
        assert!(result.is_err());
    }
}
