//! Dónde está Ollama y con qué modelo responder.
//!
//! Lo mínimo que sirve: dos variables de entorno, que se ponen en la unidad de
//! systemd (`systemctl --user edit vasak-assistant`) o al lanzarlo a mano. El
//! panel de Configuración (fase 3) traerá la configuración de verdad.
//!
//! - `VASAK_ASSISTANT_OLLAMA_URL`: por omisión `http://127.0.0.1:11434`.
//! - `VASAK_ASSISTANT_MODEL`: por omisión, el primero que Ollama tenga.

use std::net::IpAddr;

use reqwest::Url;

pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";

/// Valida la dirección de Ollama: tiene que ser HTTP y apuntar a la propia
/// máquina. En esta fase el modelo es local, y una dirección de afuera
/// mandaría la conversación fuera de la caja sin que nadie lo haya elegido.
pub fn parse_base_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|error| format!("dirección inválida: {error}"))?;
    if url.scheme() != "http" {
        return Err("sólo se admite http: Ollama corre en la máquina".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err("la dirección no puede llevar usuario ni contraseña".into());
    }
    // `host_str` da las IPv6 entre corchetes (`[::1]`).
    let host = url.host_str().unwrap_or_default();
    let local = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if !local {
        return Err("la dirección tiene que ser de la propia máquina".into());
    }
    Ok(url)
}

/// El modelo configurado, o `None` si no hay (vacío cuenta como no haber).
pub fn model_from(raw: Option<String>) -> Option<String> {
    raw.map(|model| model.trim().to_owned())
        .filter(|model| !model.is_empty())
}

/// Lee las dos variables. Una dirección inválida no deja al servicio sin
/// arrancar: se avisa en el registro y se usa la de siempre.
pub fn from_env() -> (Url, Option<String>) {
    let default =
        || parse_base_url(DEFAULT_OLLAMA_URL).expect("la dirección por omisión es válida");
    let url = match std::env::var("VASAK_ASSISTANT_OLLAMA_URL") {
        Ok(raw) if !raw.trim().is_empty() => parse_base_url(&raw).unwrap_or_else(|error| {
            tracing::warn!("VASAK_ASSISTANT_OLLAMA_URL: {error}; se usa {DEFAULT_OLLAMA_URL}");
            default()
        }),
        _ => default(),
    };
    (url, model_from(std::env::var("VASAK_ASSISTANT_MODEL").ok()))
}
