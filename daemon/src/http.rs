//! Lo común a los backends que hablan HTTP: el cliente, los tiempos máximos y
//! la lectura con tope.
//!
//! Todo lo que llega del backend es entrada de afuera: cada pedido tiene su
//! tiempo máximo, el cuerpo se lee con un tope, no se siguen redirecciones (un
//! servidor no puede mandar la conversación, ni la clave, a otro lado) y no se
//! pasa por ningún proxy del entorno (`HTTP_PROXY` mandaría la conversación a
//! otra máquina sin que nadie lo haya elegido).

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::{redirect, Client, Response, Url};

use crate::backend::BackendError;

/// Cuánto se espera para abrir la conexión.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Cuánto se espera la lista de modelos.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(10);
/// Cuánto se espera una respuesta del modelo. Generoso: un modelo local en una
/// máquina modesta, cargándose por primera vez, tarda.
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(300);
/// El tope del cuerpo de cualquier respuesta de un backend.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// El cliente que comparten todos los backends.
pub fn client() -> reqwest::Result<Client> {
    // `localhost` cuenta como local (`config::is_local`), así que no puede
    // depender del resolvedor: va directo al loopback. El puerto lo pone la URL.
    let loopback = [
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
    ];
    Client::builder()
        .resolve_to_addrs("localhost", &loopback)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect::Policy::none())
        .no_proxy()
        .build()
}

/// `path` relativo a la dirección base, que termina siempre en `/` (ver
/// `config::select_backend`).
pub fn endpoint(base: &Url, path: &str) -> Result<Url, BackendError> {
    base.join(path)
        .map_err(|error| BackendError::Failed(format!("dirección inválida: {error}")))
}

pub fn map_error(base: &Url, error: reqwest::Error) -> BackendError {
    if error.is_connect() {
        BackendError::Unavailable(format!(
            "El modelo no responde en {}. ¿Está instalado y corriendo?",
            origin(base)
        ))
    } else if error.is_timeout() {
        BackendError::Failed("el modelo tardó demasiado en responder".into())
    } else {
        // Sin `{error}`: el texto de reqwest trae la dirección completa.
        BackendError::Failed(format!("error hablando con {}", origin(base)))
    }
}

/// Esquema, host y puerto: lo que se puede mostrar de una dirección.
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Lee el cuerpo entero, cortando en cuanto pasa [`MAX_RESPONSE_BYTES`]: un
/// servidor que manda de más no hace crecer la memoria del servicio.
pub async fn read_capped(base: &Url, mut response: Response) -> Result<Vec<u8>, BackendError> {
    let too_large = || {
        BackendError::Failed(format!(
            "la respuesta pasa el tope de {MAX_RESPONSE_BYTES} bytes"
        ))
    };
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| map_error(base, error))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    /// El tope del contrato, escrito a mano: si alguien lo sube, estas pruebas lo notan.
    const TOPE: usize = 256 * 1024;

    use std::time::Duration;

    use super::*;
    use crate::test_support::{client, closed_port_url, parse_url, FakeServer, Reply, ENV_LOCK};

    async fn get(server: &FakeServer, path: &str) -> Response {
        client().get(server.url(path)).send().await.unwrap()
    }

    #[test]
    fn la_ruta_se_agrega_a_la_base_sin_pisar_el_ultimo_tramo() {
        let base = parse_url("http://127.0.0.1:8080/v1/");
        assert_eq!(
            endpoint(&base, "chat/completions").unwrap().as_str(),
            "http://127.0.0.1:8080/v1/chat/completions"
        );
    }

    #[test]
    fn el_origen_muestra_esquema_host_y_puerto_y_nada_mas() {
        let url = parse_url("https://api.openai.com/v1/models?x=1");
        assert_eq!(origin(&url), "https://api.openai.com");
        assert_eq!(
            origin(&parse_url("http://127.0.0.1:11434/")),
            "http://127.0.0.1:11434"
        );
    }

    #[tokio::test]
    async fn no_se_sigue_una_redireccion_y_el_otro_servidor_no_recibe_nada() {
        let destino = FakeServer::start(|_| Reply::json("{}")).await;
        let url_destino = destino.url("/robado");
        let origen = FakeServer::start(move |_| Reply::redirect(&url_destino)).await;

        let response = get(&origen, "/v1/models").await;

        assert_eq!(response.status(), 302);
        assert_eq!(origen.captured().len(), 1);
        assert!(destino.captured().is_empty());
    }

    #[tokio::test]
    async fn no_se_usa_el_proxy_del_entorno() {
        let proxy = FakeServer::start(|_| Reply::json("{}")).await;
        let destino = FakeServer::start(|_| Reply::json("{}")).await;
        let client = {
            let _env = ENV_LOCK.lock();
            for var in ["HTTP_PROXY", "http_proxy"] {
                std::env::set_var(var, format!("http://{}", proxy.addr));
            }
            let client = crate::http::client().unwrap();
            for var in ["HTTP_PROXY", "http_proxy"] {
                std::env::remove_var(var);
            }
            client
        };

        client.get(destino.url("/api/tags")).send().await.unwrap();

        assert_eq!(destino.captured().len(), 1);
        assert!(proxy.captured().is_empty(), "el pedido pasó por el proxy");
    }

    #[tokio::test]
    async fn un_cuerpo_que_cabe_se_lee_entero() {
        let server = FakeServer::start(|_| Reply::new(200, "hola")).await;
        let body = read_capped(&parse_url(&server.url("/")), get(&server, "/").await)
            .await
            .unwrap();
        assert_eq!(body, b"hola");
    }

    #[tokio::test]
    async fn un_cuerpo_de_justo_el_tope_se_lee_con_y_sin_largo_declarado() {
        let exacto = vec![b'a'; TOPE];
        let con_largo = exacto.clone();
        let server = FakeServer::start(move |pedido| {
            if pedido.path == "/con-largo" {
                Reply::new(200, con_largo.clone())
            } else {
                Reply::new(200, con_largo.clone()).without_length()
            }
        })
        .await;
        let base = parse_url(&server.url("/"));
        for path in ["/con-largo", "/sin-largo"] {
            let body = read_capped(&base, get(&server, path).await).await.unwrap();
            assert_eq!(body.len(), exacto.len(), "{path}");
        }
    }

    #[tokio::test]
    async fn un_cuerpo_de_un_byte_mas_que_el_tope_se_corta_con_y_sin_largo_declarado() {
        let grande = vec![b'a'; TOPE + 1];
        let server = FakeServer::start(move |pedido| {
            if pedido.path == "/con-largo" {
                Reply::new(200, grande.clone())
            } else {
                Reply::new(200, grande.clone()).without_length()
            }
        })
        .await;
        let base = parse_url(&server.url("/"));
        for path in ["/con-largo", "/sin-largo"] {
            match read_capped(&base, get(&server, path).await).await {
                Err(BackendError::Failed(text)) => assert!(text.contains("tope"), "{path}: {text}"),
                other => panic!("{path}: se esperaba Failed, dio {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn un_largo_declarado_enorme_se_rechaza_sin_esperar_el_cuerpo() {
        let server = FakeServer::start(|_| Reply::new(200, "poco").declaring(10 * TOPE)).await;
        let result = read_capped(&parse_url(&server.url("/")), get(&server, "/").await).await;
        assert!(matches!(result, Err(BackendError::Failed(text)) if text.contains("tope")));
    }

    #[tokio::test]
    async fn un_servidor_que_no_esta_da_no_disponible_con_la_direccion() {
        let base = parse_url(&closed_port_url());
        let error = client().get(base.clone()).send().await.unwrap_err();
        match map_error(&base, error) {
            BackendError::Unavailable(text) => assert!(text.contains(&origin(&base))),
            other => panic!("se esperaba Unavailable, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn un_servidor_que_no_contesta_a_tiempo_da_error_de_tiempo_agotado() {
        let server = FakeServer::silent().await;
        let base = parse_url(&server.url("/"));
        let error = client()
            .get(base.clone())
            .timeout(Duration::from_millis(100))
            .send()
            .await
            .unwrap_err();
        match map_error(&base, error) {
            BackendError::Failed(text) => assert!(text.contains("tardó demasiado")),
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn el_error_de_otro_tipo_no_repite_la_direccion_completa() {
        let server = FakeServer::start(|_| Reply::new(200, "x")).await;
        let base = parse_url(&server.url("/"));
        // Un esquema que el cliente no sabe hablar da un error que no es de conexión.
        let error = client()
            .get("ftp://127.0.0.1/secreto?token=1")
            .send()
            .await
            .unwrap_err();
        match map_error(&base, error) {
            BackendError::Failed(text) => {
                assert!(
                    !text.contains("secreto") && !text.contains("token"),
                    "{text}"
                );
            }
            other => panic!("se esperaba Failed, dio {other:?}"),
        }
    }

    #[tokio::test]
    async fn localhost_va_al_loopback_con_el_puerto_de_la_direccion() {
        let server = FakeServer::start(|_| Reply::new(200, "hola")).await;
        let port = server.addr.port();
        let response = client()
            .get(format!("http://localhost:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(server.captured().len(), 1);
    }
}
