//! La habilidad web del broker: traer una página con defensa.
//!
//! El broker es el único proceso con red; el demonio le pide una lectura web por
//! el IPC ([`vasak_assistant_ipc::BrokerRequest::WebFetch`]) y acá se decide si
//! sale y adónde. Dos puertas, las dos del lado del broker (nunca del demonio, que
//! con datos de la persona a mano podría codificarlos en una URL):
//!
//! 1. **Allowlist de hosts** (de `vasak.conf`, el dueño del destino es el broker):
//!    sólo salen los hosts que la persona habilitó, por nombre exacto o subdominio.
//!    Lista vacía = no sale nada.
//! 2. **SSRF**: sólo `http`/`https`; se **resuelve** el host y se rechaza si
//!    **cualquiera** de sus IPs es interna (loopback, privada, link-local —incluida
//!    `169.254.169.254`, la metadata—, ULA, no especificada, multicast); y se
//!    **fija** la conexión a las IPs ya verificadas, para que un cambio de DNS entre
//!    la comprobación y la conexión (rebinding) no la desvíe a una IP interna.
//!
//! Es **sólo lectura**: GET, sin cabeceras de autorización, sin cuerpo, sin seguir
//! redirecciones y sin pasar por ningún proxy del entorno. Así la vía web no puede
//! iniciar sesión, mandar credenciales ni escribir nada; el diálogo del permiso lo
//! promete y esto lo honra. El cuerpo se lee con el tope de `MAX_RESPONSE_BYTES`
//! (`read_capped`): un servidor que manda de más no hace crecer la memoria.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use reqwest::{redirect, Client, Url};

use vasak_assistant_ipc::{BackendError, WebContent};

use crate::http::{self, read_capped};

/// Cuánto se espera abrir la conexión web.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Cuánto se espera una lectura web entera. Más corto que `CHAT_TIMEOUT` (que
/// cubre la carga de un modelo): una página que no llega en este tiempo no vale la
/// espera.
pub const WEB_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Si `host` cae dentro del allowlist: por nombre exacto o como subdominio de una
/// entrada (la entrada `ejemplo.com` permite `www.ejemplo.com`, pero no
/// `noejemplo.com` ni `ejemplo.com.evil.com`). Sin distinguir mayúsculas. Un
/// allowlist vacío no permite nada.
pub fn host_allowed(allowlist: &[String], host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    allowlist.iter().any(|raw| {
        let entry = raw.trim().trim_end_matches('.').to_ascii_lowercase();
        if entry.is_empty() {
            return false;
        }
        host == entry || host.ends_with(&format!(".{entry}"))
    })
}

/// Si `ip` es una dirección a la que la habilidad web **no** puede salir: la propia
/// máquina o la red interna. Cubre v4 y v6, y una v6 que mapea o encapsula una v4
/// se juzga por esa v4 (`::ffff:127.0.0.1` es loopback).
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            // Primero las propiedades del propio v6. `to_ipv4()` devuelve `Some`
            // también para el rango compatible `::/96` —que incluye `::1`—, así
            // que juzgar antes por el v4 embebido dejaría pasar el loopback v6.
            if is_blocked_v6(v6) {
                return true;
            }
            // Una v6 que encapsula una v4 se juzga por esa v4: mapeada
            // `::ffff:a.b.c.d`, compatible `::a.b.c.d` (`to_ipv4`), y el prefijo
            // NAT64 `64:ff9b::/96`.
            if let Some(v4) = v6.to_ipv4().or_else(|| nat64_embedded(v6)) {
                return is_blocked_v4(v4);
            }
            false
        }
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()            // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254.0.0/16 (incluye .169.254, la metadata)
        || ip.is_unspecified()  // 0.0.0.0
        || ip.is_multicast()    // 224.0.0.0/4
        || ip.is_broadcast()    // 255.255.255.255
        || is_cgnat(ip) // 100.64.0.0/10 (RFC 6598, NAT de carrier)
}

/// 100.64.0.0/10 (RFC 6598): el segundo octeto va de 64 a 127.
fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (o[1] & 0xc0) == 0x40
}

/// La v4 embebida en el prefijo NAT64 `64:ff9b::/96` (RFC 6052), o `None`. En una
/// red con gateway NAT64 esto se traduce a esa v4, que puede ser interna.
fn nat64_embedded(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return Some(Ipv4Addr::new(
            (s[6] >> 8) as u8,
            (s[6] & 0xff) as u8,
            (s[7] >> 8) as u8,
            (s[7] & 0xff) as u8,
        ));
    }
    None
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    // `is_unique_local` y `is_unicast_link_local` son API inestable, así que los
    // rangos se comprueban a mano sobre el primer segmento.
    let first = ip.segments()[0];
    let unique_local = (first & 0xfe00) == 0xfc00; // fc00::/7
    let link_local = (first & 0xffc0) == 0xfe80; // fe80::/10
    ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || unique_local || link_local
}

/// Trae `raw_url` con las dos puertas (allowlist + SSRF) ya aplicadas. `Err` lleva
/// un motivo para la persona que **no** repite la URL completa (podría llevar un
/// token en la ruta): sólo el host o el origen.
pub async fn fetch(allowlist: &[String], raw_url: &str) -> Result<WebContent, BackendError> {
    let url = parse_web_url(raw_url)?;
    let host = url
        .host_str()
        .ok_or_else(|| BackendError::Failed("la dirección web no tiene host".into()))?
        .to_owned();

    if !host_allowed(allowlist, &host) {
        return Err(BackendError::NotAllowed(format!(
            "«{host}» no está en la lista de sitios que habilitaste para Lince."
        )));
    }

    // Puerto efectivo (el que usará reqwest): el de la URL o el del esquema.
    let port = url
        .port_or_known_default()
        .ok_or_else(|| BackendError::Failed("la dirección web no tiene puerto".into()))?;

    // Defensa en profundidad: la web se lee por 80/443. Un host del allowlist no
    // debe poder usarse para tocar otro servicio suyo (SSH 22, SMTP 25, Redis
    // 6379, …) por un puerto cualquiera.
    if port != 80 && port != 443 {
        return Err(BackendError::NotAllowed(format!(
            "Lince sólo sale a la web por los puertos 80 y 443, no el {port}."
        )));
    }

    // Resolver el host y comprobar **todas** sus IPs. Si cualquiera es interna, se
    // rechaza entero: un host no puede mezclar una IP pública y una interna para
    // colarse.
    let resolved = resolve(&host, port).await?;
    for addr in &resolved {
        if is_blocked_ip(addr.ip()) {
            return Err(BackendError::NotAllowed(format!(
                "«{host}» resuelve a una dirección interna: la habilidad web no sale a la red local."
            )));
        }
    }

    // Fijar la conexión a las IPs ya verificadas: reqwest no vuelve a resolver, así
    // que un cambio de DNS entre la comprobación y la conexión (rebinding) no puede
    // desviarla a una IP interna. El puerto del SocketAddr lo ignora reqwest (usa el
    // de la URL); las IPs son las que mandan.
    let client = pinned_client(&host, &resolved)?;

    // GET pelado: sin cabeceras de autorización, sin cuerpo. `redirect::none` y
    // `no_proxy` ya van en el cliente; el tope de cuerpo lo pone `read_capped`.
    let response = client
        .get(url.clone())
        .timeout(WEB_FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|error| http::map_error(&url, error))?;

    if !response.status().is_success() {
        return Err(BackendError::Failed(format!(
            "{} respondió {}",
            http::origin(&url),
            response.status()
        )));
    }

    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = read_capped(&url, response).await?;
    // El cuerpo es texto; un binario (o un corte en medio de un carácter) llega como
    // su mejor interpretación UTF-8 en vez de fallar.
    let body = String::from_utf8_lossy(&body).into_owned();

    Ok(WebContent {
        final_url,
        content_type,
        body,
    })
}

/// Parsea y valida la URL web: `http`/`https`, con host, sin usuario ni contraseña.
/// A diferencia de `config::normalize_url` **no** recorta la consulta ni la ruta:
/// una página las necesita. `Err` sin la URL completa.
fn parse_web_url(raw: &str) -> Result<Url, BackendError> {
    let url = Url::parse(raw.trim())
        .map_err(|_| BackendError::Failed("la dirección web es inválida".into()))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(BackendError::Failed(
            "la dirección web tiene que ser http o https".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(BackendError::Failed(
            "la dirección web no puede llevar usuario ni contraseña".into(),
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(BackendError::Failed(
            "la dirección web no tiene host".into(),
        ));
    }
    Ok(url)
}

/// Resuelve `host:port` a sus direcciones. Usa el resolvedor del sistema por el
/// camino asíncrono (no bloquea el runtime). `Err` si no resuelve a nada.
async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>, BackendError> {
    // Un host que ya es una IP literal también pasa por acá: `lookup_host` lo
    // devuelve tal cual, y la comprobación de IP interna lo alcanza igual.
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| BackendError::Unavailable(format!("no se pudo resolver «{host}».")))?
        .collect();
    if addrs.is_empty() {
        return Err(BackendError::Unavailable(format!(
            "«{host}» no resuelve a ninguna dirección."
        )));
    }
    Ok(addrs)
}

/// Un cliente HTTP fijado a `resolved` para `host`: reqwest no vuelve a resolver,
/// no sigue redirecciones y no usa proxy del entorno. Mismo criterio que
/// `http::client`, con el destino ya verificado clavado.
fn pinned_client(host: &str, resolved: &[SocketAddr]) -> Result<Client, BackendError> {
    Client::builder()
        .resolve_to_addrs(host, resolved)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| BackendError::Failed("no se pudo preparar la lectura web".into()))
}

#[cfg(test)]
#[path = "web_tests.rs"]
mod tests;
