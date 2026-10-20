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
//!     "allow_remote": true,
//!     "share_memory_with_remote": false
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
    /// Si la memoria del asistente se usa también con un backend remoto (se
    /// manda en el contexto y se aprende de esas conversaciones). Por omisión
    /// no: con un remoto, la memoria no se lee ni se escribe.
    pub share_memory_with_remote: bool,
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
    let url = normalize_url(raw_url).map_err(reject_unparsed)?;

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

/// La dirección base de un backend, normalizada: `http` o `https`, con host,
/// sin usuario ni contraseña, sin consulta ni fragmento y terminada en `/`
/// para que `join` agregue la ruta en vez de reemplazar el último tramo
/// (`.../v1` + `models`).
///
/// Es también el atributo `url` del ítem de la clave en el llavero
/// ([`crate::keyring`]): quien guarda la clave y quien la lee tienen que
/// normalizar igual, o no se encuentran. `Err` lleva el motivo para la
/// persona, sin la dirección (podría traer una contraseña).
pub fn normalize_url(raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw.trim())
        .map_err(|error| format!("La dirección del modelo es inválida: {error}."))?;
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err("La dirección del modelo tiene que ser http o https.".into());
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "La dirección del modelo no puede llevar usuario ni contraseña: la clave va en el llavero."
                .into(),
        );
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err("La dirección del modelo no tiene host.".into());
    }
    url.set_query(None);
    url.set_fragment(None);
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    Ok(url)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(backend: &str, url: &str, model: &str, allow_remote: bool) -> Settings {
        let some = |text: &str| (!text.is_empty()).then(|| text.to_owned());
        Settings {
            backend: some(backend),
            url: some(url),
            model: some(model),
            allow_remote,
            share_memory_with_remote: false,
        }
    }

    fn rejected(settings: &Settings) -> Rejection {
        select_backend(settings).expect_err("la configuración se rechaza")
    }

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn sin_configuracion_queda_ollama_local_en_el_puerto_de_siempre() {
        let selection = select_backend(&Settings::default()).unwrap();
        assert_eq!(selection.kind, BackendKind::Ollama);
        assert_eq!(selection.url.as_str(), "http://127.0.0.1:11434/");
        assert_eq!(selection.model, None);
        assert!(selection.local);
    }

    #[test]
    fn openai_sin_direccion_usa_la_local_de_llama_cpp() {
        let selection = select_backend(&settings("openai", "", "", false)).unwrap();
        assert_eq!(selection.kind, BackendKind::OpenAi);
        assert_eq!(selection.url.as_str(), "http://127.0.0.1:8080/v1/");
        assert!(selection.local);
    }

    #[test]
    fn los_campos_en_blanco_cuentan_como_omitidos() {
        let selection = select_backend(&settings("  ", "   ", "  ", false)).unwrap();
        assert_eq!(selection.kind, BackendKind::Ollama);
        assert_eq!(selection.url.as_str(), "http://127.0.0.1:11434/");
        assert_eq!(selection.model, None);
    }

    #[test]
    fn el_modelo_se_recorta() {
        let selection = select_backend(&settings("", "", "  llama3.2  ", false)).unwrap();
        assert_eq!(selection.model.as_deref(), Some("llama3.2"));
    }

    #[test]
    fn un_backend_desconocido_se_rechaza_nombrandolo() {
        let rejection = rejected(&settings("claude", "", "m", false));
        assert!(rejection.reason.contains("«claude» no existe"));
        assert_eq!(rejection.info.provider, "claude");
        assert_eq!(rejection.info.name, "m");
        assert!(rejection.info.local);
        assert!(rejection.info.host.is_empty());
    }

    #[test]
    fn un_esquema_que_no_es_http_se_rechaza() {
        for raw in ["ftp://127.0.0.1/", "file:///etc/passwd", "ws://127.0.0.1/"] {
            let rejection = rejected(&settings("ollama", raw, "", true));
            assert!(rejection.reason.contains("http o https"), "{raw}");
            assert!(rejection.info.local, "{raw}");
        }
    }

    #[test]
    fn una_direccion_ilegible_se_rechaza() {
        let rejection = rejected(&settings("ollama", "esto no es una url", "", false));
        assert!(rejection.reason.contains("inválida"));
        assert_eq!(rejection.info.provider, "ollama");
    }

    #[test]
    fn una_direccion_con_usuario_o_contrasena_se_rechaza() {
        for raw in [
            "http://usuario@127.0.0.1:11434",
            "http://usuario:clave@127.0.0.1:11434",
            "https://:clave@api.openai.com/v1",
        ] {
            let rejection = rejected(&settings("openai", raw, "gpt", true));
            assert!(rejection.reason.contains("usuario ni contraseña"), "{raw}");
            // La clave de la dirección no debe quedar en lo que se muestra.
            assert!(!rejection.reason.contains("clave@"), "{raw}");
            assert!(rejection.info.host.is_empty(), "{raw}");
        }
    }

    #[test]
    fn un_remoto_sin_allow_remote_se_rechaza_y_dice_adonde_iria() {
        let rejection = rejected(&settings(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
            false,
        ));
        assert!(rejection.reason.contains("allow_remote"));
        assert!(rejection.reason.contains("api.openai.com"));
        assert!(!rejection.info.local);
        assert_eq!(rejection.info.host, "api.openai.com");
        assert_eq!(rejection.info.name, "gpt-4o-mini");
    }

    #[test]
    fn un_remoto_por_http_se_rechaza_aunque_este_permitido() {
        let rejection = rejected(&settings("openai", "http://api.ejemplo.com/v1", "m", true));
        assert!(rejection.reason.contains("https"));
        assert!(!rejection.info.local);
    }

    #[test]
    fn un_remoto_sin_modelo_se_rechaza_aunque_este_permitido() {
        for model in ["", "   "] {
            let rejection = rejected(&settings(
                "openai",
                "https://api.openai.com/v1",
                model,
                true,
            ));
            assert!(rejection.reason.contains("elegir el modelo"), "{model:?}");
        }
    }

    #[test]
    fn sin_allow_remote_el_rechazo_pesa_mas_que_http_y_que_el_modelo() {
        let rejection = rejected(&settings("openai", "http://api.ejemplo.com/v1", "", false));
        assert!(rejection.reason.contains("allow_remote"));
    }

    #[test]
    fn un_remoto_permitido_por_https_y_con_modelo_se_acepta() {
        let selection = select_backend(&settings(
            "openai",
            "https://api.openai.com/v1",
            "gpt-4o-mini",
            true,
        ))
        .unwrap();
        assert!(!selection.local);
        assert_eq!(selection.url.as_str(), "https://api.openai.com/v1/");
        let info = selection.info();
        assert_eq!(info.provider, "openai");
        assert_eq!(info.name, "gpt-4o-mini");
        assert_eq!(info.host, "api.openai.com");
        assert!(!info.local);
    }

    #[test]
    fn un_local_no_necesita_allow_remote_ni_modelo_y_acepta_https() {
        let selection =
            select_backend(&settings("openai", "https://localhost:8443/v1", "", false)).unwrap();
        assert!(selection.local);
        assert_eq!(selection.model, None);
    }

    #[test]
    fn un_nombre_que_resuelve_a_loopback_igual_cuenta_como_remoto() {
        let rejection = rejected(&settings(
            "ollama",
            "http://localhost.evil.com:11434",
            "",
            false,
        ));
        assert!(rejection.reason.contains("allow_remote"));
    }

    #[test]
    fn la_direccion_se_normaliza_sin_consulta_ni_fragmento_y_con_barra_final() {
        let casos = [
            ("http://127.0.0.1:8080/v1", "http://127.0.0.1:8080/v1/"),
            ("http://127.0.0.1:8080/v1/", "http://127.0.0.1:8080/v1/"),
            ("http://localhost:1234", "http://localhost:1234/"),
            (
                "http://127.0.0.1:8080/v1?token=1#parte",
                "http://127.0.0.1:8080/v1/",
            ),
        ];
        for (raw, expected) in casos {
            let selection = select_backend(&settings("openai", raw, "", false)).unwrap();
            assert_eq!(selection.url.as_str(), expected, "{raw}");
        }
    }

    #[test]
    fn la_ruta_se_agrega_a_la_direccion_base_sin_pisar_el_ultimo_tramo() {
        let selection =
            select_backend(&settings("openai", "http://127.0.0.1:8080/v1", "", false)).unwrap();
        assert_eq!(
            selection.url.join("models").unwrap().as_str(),
            "http://127.0.0.1:8080/v1/models"
        );
    }

    #[test]
    fn is_local_reconoce_el_loopback_y_nada_mas() {
        for raw in [
            "http://localhost",
            "http://LOCALHOST:11434",
            "http://127.0.0.1",
            "http://127.5.5.5:80",
            "http://[::1]:11434",
            "https://[::1]/v1",
        ] {
            assert!(is_local(&url(raw)), "{raw} debería ser local");
        }
        for raw in [
            "https://api.openai.com/v1",
            "http://192.168.1.5:11434",
            "http://0.0.0.0:11434",
            "http://[2001:db8::1]",
            "http://localhost.evil.com",
            "http://127.0.0.1.evil.com",
            "http://milocalhost",
        ] {
            assert!(!is_local(&url(raw)), "{raw} debería ser remoto");
        }
    }

    #[test]
    fn el_nombre_del_proveedor_sale_del_contrato() {
        assert_eq!(provider_name(BackendKind::Ollama), PROVIDER_OLLAMA);
        assert_eq!(provider_name(BackendKind::OpenAi), PROVIDER_OPENAI);
    }

    #[test]
    fn el_json_sin_la_seccion_assistant_da_lo_de_siempre() {
        assert_eq!(
            settings_from_json(r#"{"theme": "dark"}"#).unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn la_seccion_assistant_se_lee_y_lo_que_falta_toma_el_valor_por_omision() {
        let settings = settings_from_json(
            r#"{"theme":"dark","assistant":{"backend":"openai","model":"m","otra_cosa":1}}"#,
        )
        .unwrap();
        assert_eq!(settings.backend.as_deref(), Some("openai"));
        assert_eq!(settings.model.as_deref(), Some("m"));
        assert_eq!(settings.url, None);
        assert!(!settings.allow_remote);
    }

    #[test]
    fn un_json_ilegible_o_con_tipos_que_no_cuadran_da_error() {
        assert!(settings_from_json("{no es json")
            .unwrap_err()
            .contains("ilegible"));
        let error = settings_from_json(r#"{"assistant":{"allow_remote":"si"}}"#).unwrap_err();
        assert!(error.contains("no cuadra"));
    }

    #[test]
    fn una_configuracion_rota_cae_en_el_backend_local_por_omision() {
        let _env = crate::test_support::ENV_LOCK.lock();
        let dir = std::env::temp_dir().join(format!("vasak-assistant-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, content: &str| {
            let path = dir.join(name);
            std::fs::write(&path, content).unwrap();
            path
        };

        let valid = write(
            "valida.conf",
            r#"{"assistant":{"backend":"openai","allow_remote":true}}"#,
        );
        std::env::set_var("VASAK_CONFIG_PATH", &valid);
        assert_eq!(config_path(), Some(valid));
        let read = read_settings();
        assert_eq!(read.backend.as_deref(), Some("openai"));
        assert!(read.allow_remote);

        for broken in [
            write("rota.conf", "{esto no es json"),
            write("tipos.conf", r#"{"assistant":{"allow_remote":"si"}}"#),
            dir.join("no-existe.conf"),
        ] {
            std::env::set_var("VASAK_CONFIG_PATH", &broken);
            let settings = read_settings();
            assert_eq!(settings, Settings::default(), "{}", broken.display());
            let selection = select_backend(&settings).unwrap();
            assert_eq!(selection.kind, BackendKind::Ollama);
            assert!(selection.local);
        }

        // Vacía, la variable no cuenta: se usa la ruta de siempre.
        std::env::set_var("VASAK_CONFIG_PATH", "");
        if let Some(path) = config_path() {
            assert!(path.ends_with(".config/vasak/vasak.conf"));
        }
        std::env::remove_var("VASAK_CONFIG_PATH");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn normalize_url_acepta_http_y_https_y_deja_la_barra_final_sin_consulta_ni_fragmento() {
        let casos = [
            ("http://127.0.0.1:8080/v1", "http://127.0.0.1:8080/v1/"),
            ("https://api.openai.com/v1/", "https://api.openai.com/v1/"),
            ("https://api.openai.com", "https://api.openai.com/"),
            (
                "  https://api.openai.com/v1  ",
                "https://api.openai.com/v1/",
            ),
            (
                "https://api.openai.com/v1?k=1#x",
                "https://api.openai.com/v1/",
            ),
            ("http://[::1]:11434", "http://[::1]:11434/"),
        ];
        for (raw, expected) in casos {
            assert_eq!(normalize_url(raw).unwrap().as_str(), expected, "{raw:?}");
        }
    }

    #[test]
    fn normalize_url_rechaza_lo_que_no_sirve_de_direccion_base() {
        let casos = [
            ("esto no es una url", "inválida"),
            ("", "inválida"),
            ("ftp://127.0.0.1/", "http o https"),
            ("file:///etc/passwd", "http o https"),
            ("http://usuario@127.0.0.1", "usuario ni contraseña"),
            ("https://:clave@api.openai.com/v1", "usuario ni contraseña"),
            ("http://", "inválida"),
        ];
        for (raw, motivo) in casos {
            let error = normalize_url(raw).expect_err(raw);
            assert!(error.contains(motivo), "{raw:?}: {error}");
        }
    }

    #[test]
    fn el_error_de_normalize_url_no_repite_la_contrasena_de_la_direccion() {
        let error = normalize_url("https://usuario:SECRETO-123@api.openai.com/v1").unwrap_err();
        assert!(!error.contains("SECRETO-123"), "{error}");
        assert!(!error.contains("usuario:"), "{error}");
    }

    #[test]
    fn select_backend_y_normalize_url_dan_la_misma_direccion() {
        for raw in [
            "http://127.0.0.1:8080/v1",
            "https://localhost:8443/v1?x=1#y",
        ] {
            let selection = select_backend(&settings("openai", raw, "", false)).unwrap();
            assert_eq!(selection.url, normalize_url(raw).unwrap(), "{raw}");
        }
    }

    #[test]
    fn la_seccion_assistant_con_el_nombre_del_asistente_no_hace_rechazar_la_configuracion() {
        let settings = settings_from_json(
            r#"{"assistant":{"name":"Lince","backend":"openai","url":"http://127.0.0.1:8080/v1"}}"#,
        )
        .unwrap();
        assert_eq!(settings.backend.as_deref(), Some("openai"));
        assert_eq!(settings.url.as_deref(), Some("http://127.0.0.1:8080/v1"));
        assert!(select_backend(&settings).is_ok());
    }
}
