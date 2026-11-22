//! La habilidad web: que Lince lea una página para responder.
//!
//! El demonio no tiene red (su unit lo deja con `RestrictAddressFamilies=AF_UNIX`),
//! así que la lectura la hace el **broker** —el único con red— por el IPC. La
//! habilidad sólo le pasa la URL al broker; la frontera dura (sólo esos hosts, sólo
//! IPs públicas, sólo GET sin credenciales) vive en el broker
//! ([`vasak_assistant_broker::web`]).
//!
//! Como `files`, el permiso `web` es **consentimiento y visibilidad, no una
//! frontera**: dice que la persona aceptó que Lince salga a la web y acota, con el
//! allowlist, a qué sitios. Sin hosts habilitados, la capacidad se ve no disponible.

use std::sync::Arc;

use serde_json::json;

use super::dbus::string_arg;
use super::{Skill, SkillCtx, ToolOutcome, PERMISSION_WEB};
use crate::backend::{BackendError, BoxFuture, ToolDef};
use vasak_assistant_ipc::WebContent;
use vasak_assistant_protocol::{Error, DOMAIN_WEB};

/// Lo que la habilidad web necesita del broker: traer una URL ya validada. Es un
/// rasgo angosto (no el `BrokerClient` entero) para que las pruebas pongan uno de
/// mentira sin un socket.
pub trait WebFetcher: Send + Sync {
    fn web_fetch(&self, url: String) -> BoxFuture<'_, Result<WebContent, BackendError>>;
}

/// La habilidad web. Guarda el handle al broker y el allowlist (sólo para informar
/// la capacidad en `probe`; la validación de verdad la hace el broker).
pub struct WebSkill {
    broker: Arc<dyn WebFetcher>,
    allowed_hosts: Vec<String>,
}

impl WebSkill {
    pub fn new(broker: Arc<dyn WebFetcher>, allowed_hosts: Vec<String>) -> Self {
        Self {
            broker,
            allowed_hosts,
        }
    }

    async fn fetch(&self, args: &serde_json::Value) -> Result<ToolOutcome, Error> {
        let url = string_arg(args, "url")?;
        match self.broker.web_fetch(url).await {
            Ok(content) => {
                let json = json!({
                    "url": content.final_url,
                    "content_type": content.content_type,
                    "content": content.body,
                })
                .to_string();
                Ok(ToolOutcome::Result(json))
            }
            // El broker ya da un motivo legible (host no habilitado, IP interna,
            // esquema inválido, el servidor falló); se lo devolvemos al modelo tal
            // cual, para que se lo explique a la persona.
            Err(error) => Err(Error::Backend(error.text().to_owned())),
        }
    }
}

impl Skill for WebSkill {
    fn domain(&self) -> &'static str {
        DOMAIN_WEB
    }

    fn permission(&self) -> &'static str {
        PERMISSION_WEB
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![ToolDef {
            name: "web_fetch".into(),
            description: "Lee el contenido de una página web por su URL, de los sitios que la \
                          persona habilitó. Sólo lectura: no inicia sesión ni envía nada. \
                          Devuelve la dirección final, el tipo de contenido y el texto de la página."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "La URL completa de la página (http o https)."}
                },
                "required": ["url"]
            }),
        }]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a serde_json::Value,
        _ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        Box::pin(async move {
            match name {
                "web_fetch" => self.fetch(args).await,
                other => Err(Error::InvalidArgs(format!(
                    "la web no tiene una herramienta «{other}»"
                ))),
            }
        })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        // En pie si hay al menos un host habilitado de verdad. Se normaliza como el
        // broker (`web::host_allowed`: recorta espacios y el punto final) antes de
        // decidir, así una entrada basura como «.» o « » cuenta como vacía y la
        // capacidad se ve no disponible; no se abre ninguna conexión.
        let alive = self
            .allowed_hosts
            .iter()
            .any(|host| !host.trim().trim_end_matches('.').is_empty());
        Box::pin(async move { alive })
    }
}
