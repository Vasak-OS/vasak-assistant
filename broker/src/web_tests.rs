//! Pruebas de la habilidad web del broker: SSRF, allowlist y las puertas de
//! `fetch` que cortan **antes** de la red (por eso son deterministas en el CI).
//! El fetch de punta a punta contra un servidor de mentira no se puede: el
//! servidor viviría en 127.0.0.1, que el SSRF bloquea a propósito.

use std::net::IpAddr;

use super::*;

fn ip(text: &str) -> IpAddr {
    text.parse().expect("IP de prueba válida")
}

fn list(hosts: &[&str]) -> Vec<String> {
    hosts.iter().map(|host| (*host).to_owned()).collect()
}

// ── SSRF ──────────────────────────────────────────────────────────────────────

#[test]
fn el_loopback_v6_esta_bloqueado() {
    // Regresión: `to_ipv4()` devuelve `Some(0.0.0.1)` para `::1` (rango
    // compatible `::/96`); juzgar antes por el v4 embebido lo dejaba pasar.
    assert!(is_blocked_ip(ip("::1")));
}

#[test]
fn las_direcciones_internas_estan_bloqueadas() {
    for text in [
        "127.0.0.1",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.1.1",
        "169.254.169.254", // la metadata de la nube
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "::",
        "::ffff:127.0.0.1", // v4 mapeada
        "::ffff:10.0.0.1",
        "fe80::1",
        "fc00::1",
        "fd12:3456::1",
        "ff02::1",
        "64:ff9b::7f00:1",    // NAT64 de 127.0.0.1
        "64:ff9b::a9fe:a9fe", // NAT64 de la metadata
        "100.64.0.1",         // CGNAT
        "100.127.255.254",
        "198.18.0.0", // benchmarking 198.18.0.0/15
        "198.19.255.255",
        "64:ff9b:1::a", // NAT64 de uso local (RFC 8215)
        "64:ff9b:1:ffff::1",
    ] {
        assert!(is_blocked_ip(ip(text)), "{text} debería estar bloqueada");
    }
}

#[test]
fn las_direcciones_publicas_estan_permitidas() {
    for text in [
        "1.1.1.1",
        "8.8.8.8",
        "100.63.255.255", // justo antes del CGNAT
        "100.128.0.1",    // justo después
        "198.17.255.255", // justo antes del benchmarking
        "198.20.0.0",     // justo después
        "2606:4700:4700::1111",
        "64:ff9b::808:808", // NAT64 de 8.8.8.8
        "::ffff:8.8.8.8",
    ] {
        assert!(!is_blocked_ip(ip(text)), "{text} debería estar permitida");
    }
}

// ── Allowlist ─────────────────────────────────────────────────────────────────

#[test]
fn el_allowlist_permite_el_host_y_sus_subdominios() {
    let allow = list(&["ejemplo.com"]);
    assert!(host_allowed(&allow, "ejemplo.com"));
    assert!(host_allowed(&allow, "www.ejemplo.com"));
    assert!(host_allowed(&allow, "a.b.ejemplo.com"));
}

#[test]
fn el_allowlist_no_se_deja_enganar_por_nombres_parecidos() {
    let allow = list(&["ejemplo.com"]);
    assert!(!host_allowed(&allow, "ejemplo.com.evil.com"));
    assert!(!host_allowed(&allow, "noejemplo.com"));
    assert!(!host_allowed(&allow, "evil.com"));
    assert!(!host_allowed(&allow, ""));
    assert!(!host_allowed(&allow, "."));
}

#[test]
fn un_allowlist_vacio_no_permite_nada() {
    assert!(!host_allowed(&[], "ejemplo.com"));
    // Una entrada en blanco tampoco abre la puerta.
    assert!(!host_allowed(&list(&["", "  ", "."]), "ejemplo.com"));
}

#[test]
fn el_allowlist_ignora_mayusculas_y_tolera_el_punto_final() {
    assert!(host_allowed(&list(&["Ejemplo.COM"]), "WWW.ejemplo.com"));
    assert!(host_allowed(&list(&["ejemplo.com."]), "ejemplo.com"));
    assert!(host_allowed(&list(&["ejemplo.com"]), "ejemplo.com."));
    assert!(host_allowed(&list(&[" ejemplo.com "]), "ejemplo.com"));
}

// ── fetch: puertas que cortan antes de la red ─────────────────────────────────

#[tokio::test]
async fn fetch_rechaza_un_esquema_que_no_es_http() {
    let allow = list(&["ejemplo.com"]);
    for url in [
        "ftp://ejemplo.com/x",
        "file:///etc/passwd",
        "gopher://ejemplo.com/",
        "ejemplo.com/sin-esquema",
    ] {
        let result = fetch(&allow, url).await;
        assert!(
            matches!(result, Err(BackendError::Failed(_))),
            "{url}: {result:?}"
        );
    }
}

#[tokio::test]
async fn fetch_rechaza_usuario_y_contrasena_en_la_url() {
    let result = fetch(&list(&["ejemplo.com"]), "https://u:p@ejemplo.com/").await;
    assert!(matches!(result, Err(BackendError::Failed(_))), "{result:?}");
}

#[tokio::test]
async fn fetch_rechaza_un_host_fuera_del_allowlist() {
    let result = fetch(&list(&["ejemplo.com"]), "https://otro.org/").await;
    assert!(
        matches!(result, Err(BackendError::NotAllowed(_))),
        "{result:?}"
    );
    // Con la lista vacía no sale nada.
    let result = fetch(&[], "https://ejemplo.com/").await;
    assert!(
        matches!(result, Err(BackendError::NotAllowed(_))),
        "{result:?}"
    );
}

#[tokio::test]
async fn fetch_no_repite_la_url_completa_en_el_error() {
    let Err(error) = fetch(&[], "https://otro.org/ruta?token=secreto").await else {
        panic!("tenía que rechazar");
    };
    assert!(!error.text().contains("secreto"), "{}", error.text());
}

#[tokio::test]
async fn fetch_rechaza_un_puerto_que_no_es_80_ni_443() {
    let allow = list(&["ejemplo.com"]);
    for url in [
        "http://ejemplo.com:6379/",
        "https://ejemplo.com:22/",
        "http://ejemplo.com:8080/",
    ] {
        let result = fetch(&allow, url).await;
        assert!(
            matches!(result, Err(BackendError::NotAllowed(_))),
            "{url}: {result:?}"
        );
    }
}

#[tokio::test]
async fn fetch_no_sale_a_un_host_del_allowlist_que_resuelve_a_una_ip_interna() {
    // `localhost` sale de /etc/hosts y la IP literal no necesita DNS: la
    // resolución es local y el rechazo ocurre antes de abrir la conexión.
    for (host, url) in [
        ("localhost", "http://localhost/"),
        ("127.0.0.1", "http://127.0.0.1/"),
        (
            "169.254.169.254",
            "http://169.254.169.254/latest/meta-data/",
        ),
    ] {
        let result = fetch(&list(&[host]), url).await;
        assert!(
            matches!(result, Err(BackendError::NotAllowed(_))),
            "{url}: {result:?}"
        );
    }
}

#[tokio::test]
async fn fetch_no_sale_a_un_loopback_v6_literal() {
    // `host_str()` devuelve `[::1]` con corchetes y el resolvedor no lo entiende:
    // falla cerrado (`Unavailable`) en vez de `NotAllowed`. Lo que importa acá es
    // que nunca salga; el motivo exacto no es parte del contrato.
    let result = fetch(&list(&["[::1]"]), "http://[::1]/").await;
    assert!(result.is_err(), "{result:?}");
}

// ── cap_body ──────────────────────────────────────────────────────────────────

#[test]
fn un_cuerpo_corto_pasa_intacto() {
    assert_eq!(cap_body("hola".into()), "hola");
    let justo = "a".repeat(MAX_BODY_CHARS);
    assert_eq!(cap_body(justo.clone()), justo);
}

#[test]
fn un_cuerpo_largo_se_recorta_y_se_marca() {
    let out = cap_body("a".repeat(MAX_BODY_CHARS + 500));
    assert!(out.ends_with("… [contenido recortado]"), "{out:?}");
    assert_eq!(
        out.trim_end_matches("… [contenido recortado]")
            .trim_end()
            .len(),
        MAX_BODY_CHARS
    );
}

#[test]
fn el_recorte_no_parte_un_caracter_multibyte() {
    // 'é' (2 bytes) y un emoji (4 bytes) ocupan el límite: nunca se corta a medias.
    for ch in ['é', '😀'] {
        let body: String = std::iter::repeat_n(ch, MAX_BODY_CHARS + 10).collect();
        let out = cap_body(body);
        let kept = out.trim_end_matches("… [contenido recortado]").trim_end();
        assert_eq!(kept.chars().count(), MAX_BODY_CHARS, "{ch}");
        assert!(kept.chars().all(|c| c == ch));
    }
}

// ── parse_web_url y pinned_client ─────────────────────────────────────────────

#[test]
fn parse_web_url_conserva_ruta_y_consulta_y_rechaza_lo_demas() {
    let url = parse_web_url("  https://ejemplo.com/a/b?q=1#x ").unwrap();
    assert_eq!(url.path(), "/a/b");
    assert_eq!(url.query(), Some("q=1"));
    for malo in [
        "",
        "no es url",
        "ftp://x.org/",
        "https://u@x.org/",
        "https://:p@x.org/",
    ] {
        assert!(
            matches!(parse_web_url(malo), Err(BackendError::Failed(_))),
            "{malo}"
        );
    }
}

#[test]
fn pinned_client_se_arma_con_las_ips_verificadas() {
    let addr: SocketAddr = "1.1.1.1:443".parse().unwrap();
    assert!(pinned_client("ejemplo.com", &[addr]).is_ok());
}

// ── fetch_with_client: la parte de red, contra un servidor local ──────────────
// Un cliente común, sin el guardia SSRF (que bloquea 127.0.0.1 a propósito).

use crate::test_support::{client, FakeServer, Reply};

async fn get(server: &FakeServer, path: &str) -> Result<WebContent, BackendError> {
    let url = Url::parse(&server.url(path)).unwrap();
    fetch_with_client(&client(), &url).await
}

#[tokio::test]
async fn una_respuesta_ok_trae_cuerpo_tipo_y_url_final() {
    let server = FakeServer::start(|_| {
        let mut reply = Reply::new(200, "<p>hola</p>");
        reply
            .headers
            .push(("content-type".into(), "text/html; charset=utf-8".into()));
        reply
    })
    .await;
    let content = get(&server, "/pagina?q=1").await.unwrap();
    assert_eq!(content.body, "<p>hola</p>");
    assert_eq!(content.content_type, "text/html; charset=utf-8");
    assert_eq!(content.final_url, server.url("/pagina?q=1"));
    // GET pelado: sin credenciales.
    let seen = server.captured();
    assert_eq!(seen[0].method, "GET");
    assert!(seen[0].header("authorization").is_none());
}

#[tokio::test]
async fn sin_content_type_el_tipo_queda_vacio() {
    let server = FakeServer::start(|_| Reply::new(200, "x")).await;
    assert_eq!(get(&server, "/").await.unwrap().content_type, "");
}

#[tokio::test]
async fn un_cuerpo_grande_llega_recortado() {
    let big = "a".repeat(MAX_BODY_CHARS + 1000);
    let server = FakeServer::start(move |_| Reply::new(200, big.clone())).await;
    let content = get(&server, "/").await.unwrap();
    assert!(content.body.ends_with("… [contenido recortado]"));
    assert!(content.body.len() < MAX_BODY_CHARS + 100);
}

#[tokio::test]
async fn un_cuerpo_binario_llega_como_texto_en_vez_de_fallar() {
    let server = FakeServer::start(|_| Reply::new(200, vec![0xff, 0xfe, b'o', b'k'])).await;
    assert!(get(&server, "/").await.unwrap().body.ends_with("ok"));
}

#[tokio::test]
async fn un_estado_de_error_da_fallo_sin_repetir_la_ruta() {
    let server = FakeServer::start(|_| Reply::status(404)).await;
    let Err(BackendError::Failed(text)) = get(&server, "/secreta?token=zzz").await else {
        panic!("tenía que fallar");
    };
    assert!(text.contains("404"), "{text}");
    assert!(!text.contains("zzz"), "{text}");
}

#[tokio::test]
async fn una_redireccion_no_se_sigue() {
    let server = FakeServer::start(|_| Reply::redirect("http://interno.local/")).await;
    assert!(matches!(
        get(&server, "/").await,
        Err(BackendError::Failed(_))
    ));
    assert_eq!(server.captured().len(), 1);
}

#[tokio::test]
async fn un_cuerpo_que_pasa_el_tope_de_bytes_da_fallo() {
    let huge = vec![b'a'; vasak_assistant_ipc::MAX_RESPONSE_BYTES + 1024];
    let server = FakeServer::start(move |_| Reply::new(200, huge.clone())).await;
    assert!(matches!(
        get(&server, "/").await,
        Err(BackendError::Failed(_))
    ));
}

#[tokio::test]
async fn un_servidor_caido_da_error_de_red() {
    let url = Url::parse(&crate::test_support::closed_port_url()).unwrap();
    assert!(fetch_with_client(&client(), &url).await.is_err());
}
