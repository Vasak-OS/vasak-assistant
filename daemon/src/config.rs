//! Con qué backend responder, según `vasak.conf`.
//!
//! La configuración vive en la misma `vasak.conf` que el resto del sistema
//! (`~/.config/vasak/vasak.conf`, o `VASAK_CONFIG_PATH`), en la sección
//! `assistant`. Ahí la va a escribir el panel de Configuración (fase 3); hasta
//! entonces se edita a mano. Se lee al arrancar: un cambio pide
//! `systemctl --user restart vasak-assistant`.
//!
//! ```json
//! {
//!   "assistant": {
//!     "backend": "openai",
//!     "url": "https://api.openai.com/v1",
//!     "model": "gpt-4o-mini",
//!     "allow_remote": true
//!   }
//! }
//! ```
//!
//! Sin la sección, o con un archivo ilegible, queda lo de siempre: Ollama en
//! `127.0.0.1:11434`, local. **La clave del proveedor nunca va acá**: se lee de
//! `vasak-keyring` (ver [`crate::keyring`]).

use std::net::IpAddr;
use std::path::PathBuf;

use reqwest::Url;
use serde::Deserialize;
use vasak_assistant_protocol::{ModelInfo, PROVIDER_OLLAMA, PROVIDER_OPENAI};

pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
/// llama.cpp server escucha ahí por omisión; es el compatible con OpenAI más
/// probable en la máquina.
pub const DEFAULT_OPENAI_URL: &str = "http://127.0.0.1:8080/v1";

/// La sección `assistant` de `vasak.conf`, tal cual. Lo que falta toma el valor
/// por omisión.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// `ollama` (por omisión) u `openai`.
    pub backend: Option<String>,
    /// La dirección base. Por omisión, la local de cada backend.
    pub url: Option<String>,
    /// El modelo. Por omisión, el primero que tenga el backend; un remoto lo
    /// exige.
    pub model: Option<String>,
    /// Si se permite un backend remoto (un host que no es de loopback). Por
    /// omisión no: la conversación no sale de la máquina sin que la persona lo
    /// haya elegido.
    pub allow_remote: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Ollama,
    OpenAi,
}

/// El backend elegido, ya validado.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub kind: BackendKind,
    /// La dirección base, terminada siempre en `/` para que `join` agregue la
    /// ruta en vez de reemplazar el último tramo (`.../v1` + `models`).
    pub url: Url,
    pub model: Option<String>,
    /// El host es de loopback. Sale de la dirección, no de lo que alguien
    /// declare.
    pub local: bool,
}

impl Selection {
    pub fn info(&self) -> ModelInfo {
        ModelInfo {
            provider: provider_name(self.kind).into(),
            name: self.model.clone().unwrap_or_default(),
            local: self.local,
            host: self.url.host_str().unwrap_or_default().into(),
        }
    }
}

/// Por qué la configuración no se puede usar, con lo que se sabe del backend
/// para que el estado lo muestre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub info: ModelInfo,
    pub reason: String,
}

pub fn provider_name(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Ollama => PROVIDER_OLLAMA,
        BackendKind::OpenAi => PROVIDER_OPENAI,
    }
}

/// Si el host de la dirección es la propia máquina: `localhost` o una IP de
/// loopback. Cualquier otro nombre cuenta como remoto, aunque resuelva a
/// loopback: ante la duda, se trata como que sale de la caja.
pub fn is_local(url: &Url) -> bool {
    // `host_str` da las IPv6 entre corchetes (`[::1]`).
    let host = url.host_str().unwrap_or_default();
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Decide el backend a partir de la configuración. Es pura: no lee archivos ni
/// habla con nadie.
///
/// - El backend tiene que ser `ollama` u `openai`.
/// - La dirección, `http` o `https`, sin usuario ni contraseña.
/// - Local (host de loopback): `http` o `https`; el modelo puede faltar.
/// - Remoto: sólo con `allow_remote`, sólo por `https` (la conversación y la
///   clave no viajan en claro) y con el modelo explícito (elegir solo el
///   primero de un proveedor remoto podría ser cualquiera, y cobrarse).
pub fn select_backend(settings: &Settings) -> Result<Selection, Rejection> {
    let backend = clean(settings.backend.as_deref()).unwrap_or(PROVIDER_OLLAMA);
    let model = clean(settings.model.as_deref()).map(str::to_owned);
    let kind = match backend {
        PROVIDER_OLLAMA => BackendKind::Ollama,
        PROVIDER_OPENAI => BackendKind::OpenAi,
        other => {
            return Err(Rejection {
                info: ModelInfo {
                    provider: other.into(),
                    name: model.unwrap_or_default(),
                    local: true,
                    host: String::new(),
                },
                reason: format!(
                    "El backend «{other}» no existe: tiene que ser «{PROVIDER_OLLAMA}» u «{PROVIDER_OPENAI}»."
                ),
            })
        }
    };
    let raw_url = clean(settings.url.as_deref()).unwrap_or(match kind {
        BackendKind::Ollama => DEFAULT_OLLAMA_URL,
        BackendKind::OpenAi => DEFAULT_OPENAI_URL,
    });
    // Mientras no haya una dirección legible no se habla con nadie: cuenta
    // como local.
    let reject_unparsed = |reason: String| Rejection {
        info: ModelInfo {
            provider: provider_name(kind).into(),
            name: model.clone().unwrap_or_default(),
            local: true,
            host: String::new(),
        },
        reason,
    };
    let mut url = Url::parse(raw_url).map_err(|error| {
        reject_unparsed(format!("La dirección del modelo es inválida: {error}."))
    })?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(reject_unparsed(
            "La dirección del modelo tiene que ser http o https.".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(reject_unparsed(
            "La dirección del modelo no puede llevar usuario ni contraseña: la clave va en el llavero."
                .into(),
        ));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(reject_unparsed(
            "La dirección del modelo no tiene host.".into(),
        ));
    }
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }

    let selection = Selection {
        kind,
        local: is_local(&url),
        url,
        model,
    };
    if selection.local {
        return Ok(selection);
    }
    let host = selection.url.host_str().unwrap_or_default().to_owned();
    let reason = if !settings.allow_remote {
        format!(
            "«{host}» es un proveedor remoto: la conversación saldría de la máquina. Para usarlo hay que permitirlo con «allow_remote»."
        )
    } else if selection.url.scheme() != "https" {
        format!("«{host}» es un proveedor remoto y sólo se le habla por https.")
    } else if selection.model.is_none() {
        format!("Con un proveedor remoto («{host}») hay que elegir el modelo.")
    } else {
        return Ok(selection);
    };
    Err(Rejection {
        info: selection.info(),
        reason,
    })
}

fn clean(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// La sección `assistant` del contenido de `vasak.conf`. Un archivo ilegible, sin
/// la sección o con tipos que no cuadran da lo de siempre (local), que es lo
/// seguro; `Err` lleva el motivo para el registro.
pub fn settings_from_json(content: &str) -> Result<Settings, String> {
    let config: serde_json::Value =
        serde_json::from_str(content).map_err(|error| format!("vasak.conf ilegible: {error}"))?;
    match config.get("assistant") {
        None => Ok(Settings::default()),
        Some(section) => Settings::deserialize(section)
            .map_err(|error| format!("la sección «assistant» de vasak.conf no cuadra: {error}")),
    }
}

/// La ruta de `vasak.conf`. `VASAK_CONFIG_PATH` la pisa, como en el resto del
/// sistema.
pub fn config_path() -> Option<PathBuf> {
    if let Some(set) = std::env::var_os("VASAK_CONFIG_PATH") {
        if !set.is_empty() {
            return Some(PathBuf::from(set));
        }
    }
    dirs::home_dir().map(|home| home.join(".config/vasak/vasak.conf"))
}

/// Lee `vasak.conf`. Si no está, o no se puede usar, lo de siempre.
pub fn read_settings() -> Settings {
    let Some(path) = config_path() else {
        return Settings::default();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Settings::default();
    };
    settings_from_json(&content).unwrap_or_else(|reason| {
        tracing::warn!("{reason}; se usa el backend local por omisión");
        Settings::default()
    })
}
