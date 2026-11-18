//! Las habilidades del asistente: lo que Lince puede hacer sobre los datos de
//! la persona cuando el modelo las pide como herramientas.
//!
//! Cada habilidad ([`Skill`]) declara su dominio (`DOMAIN_MAIL`, …), el permiso
//! que exige (`store.email`) y las herramientas que ofrece. El motor de
//! conversación ([`crate::service`]) junta las herramientas de todas las
//! habilidades registradas, se las ofrece al modelo y, por cada llamada,
//! comprueba el permiso y ejecuta la habilidad.
//!
//! # Lo que nunca hace una habilidad por su cuenta
//!
//! Una acción con efecto (mandar un correo) **no la ejecuta el modelo**: la
//! habilidad devuelve [`ToolOutcome::Pending`] con una [`PendingAction`] que la
//! persona confirma mirando destinatarios, asunto y cuerpo reales. El borrador
//! completo se queda en el servicio; sólo viaja el resumen.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::backend::{BoxFuture, ToolDef};
use vasak_assistant_protocol::{
    Error, PendingAction, DOMAIN_CALENDAR, DOMAIN_CONTACTS, DOMAIN_FILES, DOMAIN_MAIL,
};

pub mod calendar;
pub mod contacts;
pub mod dbus;
pub mod files;
pub mod mail;

/// El recurso de permiso del correo, el mismo que exige `AccountsStore`.
pub const PERMISSION_EMAIL: &str = "store.email";

/// El recurso de permiso del calendario, el mismo que exige `AccountsStore`.
pub const PERMISSION_CALENDAR: &str = "store.calendar";

/// El recurso de permiso de los contactos, el mismo que exige `AccountsStore`.
pub const PERMISSION_CONTACTS: &str = "store.contacts";

/// El recurso de permiso de los archivos. A diferencia de los `store.*`, no lo
/// sirve ningún demonio: es consentimiento y visibilidad, no una frontera.
pub const PERMISSION_FILES: &str = "files";

/// Cuánto vive una acción pendiente antes de vencer. Pasado eso, confirmarla da
/// `NotFound`: un borrador viejo no se manda por una confirmación tardía.
pub const PENDING_TTL: Duration = Duration::from_secs(180);

/// La ejecución diferida de una acción pendiente (encolar el correo). Se guarda
/// en el registro y se corre recién al confirmar. Es `FnOnce` porque una acción
/// se ejecuta una sola vez.
pub type SendAction = Box<dyn FnOnce() -> BoxFuture<'static, Result<(), Error>> + Send>;

/// Lo que produce una herramienta: un resultado textual para devolverle al
/// modelo, o una acción que la persona tiene que confirmar antes de que ocurra.
pub enum ToolOutcome {
    /// Un texto para que el modelo lo lea como resultado de la herramienta.
    Result(String),
    /// Una acción con efecto, a la espera de que la persona la confirme.
    Pending {
        /// El resumen que viaja a la persona.
        action: PendingAction,
        /// Qué hacer si la confirma.
        execute: SendAction,
    },
}

/// El contexto de una llamada a herramienta: lo que la habilidad necesita saber
/// del turno en curso sin acoplarse al servicio.
pub struct SkillCtx {
    /// La conversación en la que se llamó, para atar la [`PendingAction`].
    pub conversation_id: String,
}

/// Una habilidad del asistente.
pub trait Skill: Send + Sync {
    /// El dominio que cubre (`DOMAIN_MAIL`).
    fn domain(&self) -> &'static str;

    /// El recurso de permiso que exige antes de ejecutar cualquiera de sus
    /// herramientas (`store.email`).
    fn permission(&self) -> &'static str;

    /// Las herramientas que ofrece al modelo.
    fn tools(&self) -> Vec<ToolDef>;

    /// Ejecuta la herramienta `name` con `args`. El permiso ya se comprobó.
    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a serde_json::Value,
        ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>>;

    /// Si el servicio detrás responde, para informar la capacidad. Barato y sin
    /// abrir diálogos; ante cualquier duda, `false` (la capacidad se ve no
    /// disponible, nunca rota).
    fn probe(&self) -> BoxFuture<'_, bool>;
}

/// El registro de habilidades: por nombre de herramienta, cuál la atiende.
#[derive(Default)]
pub struct SkillRegistry {
    skills: Vec<Arc<dyn Skill>>,
    by_tool: HashMap<String, usize>,
}

impl SkillRegistry {
    pub fn new(skills: Vec<Arc<dyn Skill>>) -> Self {
        let mut by_tool = HashMap::new();
        for (index, skill) in skills.iter().enumerate() {
            for tool in skill.tools() {
                by_tool.insert(tool.name, index);
            }
        }
        Self { skills, by_tool }
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Las herramientas de las habilidades cuyo dominio acepta `allow`. Se usa
    /// para no ofrecerle a un modelo remoto las de un dominio que no se consintió
    /// compartir.
    pub fn tools_for(&self, mut allow: impl FnMut(&str) -> bool) -> Vec<ToolDef> {
        self.skills
            .iter()
            .filter(|skill| allow(skill.domain()))
            .flat_map(|skill| skill.tools())
            .collect()
    }

    /// La habilidad que atiende la herramienta `name`, si hay alguna.
    pub fn find(&self, name: &str) -> Option<&Arc<dyn Skill>> {
        self.by_tool.get(name).map(|&index| &self.skills[index])
    }

    /// La habilidad de un dominio, para informar su capacidad.
    pub fn for_domain(&self, domain: &str) -> Option<&Arc<dyn Skill>> {
        self.skills.iter().find(|skill| skill.domain() == domain)
    }
}

/// Comprueba un permiso como lo haría Lince: pregunta la primera vez y recuerda
/// la respuesta. Es un rasgo para que las pruebas pongan uno de mentira.
pub trait PermissionCheck: Send + Sync {
    /// `Ok(true)` si está concedido. `detail` va al diálogo (de qué cuenta se
    /// trata) y no cambia la decisión guardada.
    fn check<'a>(
        &'a self,
        resource: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<bool, Error>>;
}

/// Un verificador que niega siempre, sin preguntar nada. Es el que lleva un
/// servicio sin habilidades (las pruebas que no tocan el correo): no hay
/// herramienta que gatear.
pub struct DenyPermission;

impl PermissionCheck for DenyPermission {
    fn check<'a>(
        &'a self,
        _resource: &'a str,
        _detail: &'a str,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        Box::pin(async { Ok(false) })
    }
}

/// Una acción pendiente guardada en el servicio: quién la creó, en qué
/// conversación, cuándo y cómo ejecutarla.
pub struct PendingEntry {
    pub owner: String,
    pub conversation_id: String,
    pub execute: SendAction,
    created: Instant,
}

/// El registro de acciones pendientes de confirmación. A lo sumo una por
/// conversación; cada una vence a los [`PENDING_TTL`].
#[derive(Default)]
pub struct PendingActions {
    by_id: HashMap<String, PendingEntry>,
}

impl PendingActions {
    /// Descarta los drafts de una conversación (de su dueño). Se llama al
    /// cerrarla, bajo el candado de las conversaciones, para que un draft no
    /// sobreviva a la conversación que lo propuso.
    pub fn remove_conversation(&mut self, owner: &str, conversation_id: &str) {
        self.by_id
            .retain(|_, entry| !(entry.owner == owner && entry.conversation_id == conversation_id));
    }

    /// Guarda una acción, reemplazando la que hubiera en la misma conversación
    /// del mismo dueño (un turno nuevo deja una sola pendiente). De paso
    /// descarta las vencidas.
    pub fn insert(
        &mut self,
        owner: &str,
        conversation_id: &str,
        action: PendingAction,
        execute: SendAction,
    ) {
        self.purge_expired();
        self.by_id
            .retain(|_, entry| !(entry.owner == owner && entry.conversation_id == conversation_id));
        self.by_id.insert(
            action.id,
            PendingEntry {
                owner: owner.to_owned(),
                conversation_id: conversation_id.to_owned(),
                execute,
                created: Instant::now(),
            },
        );
    }

    /// Saca una acción del registro: la devuelve si existe, es de `owner` y no
    /// venció. Sacarla antes de ejecutarla es lo que evita que un correo salga
    /// dos veces: una segunda confirmación ya no la encuentra. `NotFound` en
    /// cualquier otro caso, sin distinguirlos (no se confirma si un id existe).
    pub fn take(&mut self, id: &str, owner: &str) -> Result<PendingEntry, Error> {
        let not_found =
            || Error::NotFound("no hay una acción pendiente con ese identificador".into());
        let is_owner = self
            .by_id
            .get(id)
            .is_some_and(|entry| entry.owner == owner && entry.created.elapsed() <= PENDING_TTL);
        if !is_owner {
            return Err(not_found());
        }
        self.by_id.remove(id).ok_or_else(not_found)
    }

    fn purge_expired(&mut self) {
        self.by_id
            .retain(|_, entry| entry.created.elapsed() <= PENDING_TTL);
    }
}

/// Lo que el servicio necesita para los turnos con herramientas: las
/// habilidades registradas, el verificador de permisos y las acciones
/// pendientes. Va como un solo campo de `AssistantService` para no multiplicar
/// los campos; por omisión, sin habilidades y negando todo.
pub struct MailChat {
    pub skills: SkillRegistry,
    pub permission: Arc<dyn PermissionCheck>,
    pub pending: parking_lot::Mutex<PendingActions>,
    /// `share_mail_with_remote` de `vasak.conf`: si el correo se comparte con un
    /// modelo que no es local. Decisión propia, aparte de la memoria.
    pub share_mail_with_remote: bool,
    /// `share_calendar_with_remote` de `vasak.conf`: lo mismo para el calendario.
    /// Cada dominio decide por separado; compartir el correo no comparte el
    /// calendario. Por omisión `false`: con un remoto, el dominio no sale ni se
    /// ofrecen sus herramientas.
    pub share_calendar_with_remote: bool,
    /// `share_contacts_with_remote` de `vasak.conf`: lo mismo para los contactos.
    pub share_contacts_with_remote: bool,
    /// `share_files_with_remote` de `vasak.conf`: lo mismo para los archivos.
    pub share_files_with_remote: bool,
}

impl MailChat {
    /// Si el dominio `domain` se comparte con un modelo remoto. Un dominio
    /// desconocido, no.
    pub fn shares_with_remote(&self, domain: &str) -> bool {
        match domain {
            DOMAIN_MAIL => self.share_mail_with_remote,
            DOMAIN_CALENDAR => self.share_calendar_with_remote,
            DOMAIN_CONTACTS => self.share_contacts_with_remote,
            DOMAIN_FILES => self.share_files_with_remote,
            _ => false,
        }
    }
}

impl Default for MailChat {
    fn default() -> Self {
        Self {
            skills: SkillRegistry::default(),
            permission: Arc::new(DenyPermission),
            pending: parking_lot::Mutex::new(PendingActions::default()),
            share_mail_with_remote: false,
            share_calendar_with_remote: false,
            share_contacts_with_remote: false,
            share_files_with_remote: false,
        }
    }
}

#[cfg(test)]
mod tests;
