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
