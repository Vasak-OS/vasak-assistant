//! El contrato entre el asistente de IA residente de VasakOS y quien le habla:
//! el escritorio, Configuración, la terminal y las aplicaciones.
//!
//! Todo lo que viaja por D-Bus se define acá una sola vez, así los lados no se
//! separan: un campo renombrado en una punta deja de compilar en la otra en lugar
//! de volverse, en silencio, una respuesta que nadie entiende.
//!
//! # Lo que hay en esta fase
//!
//! Conversación con un modelo, local por omisión (Ollama en la máquina) o, si la
//! persona lo permitió explícitamente, un proveedor remoto compatible con la API
//! de OpenAI. Ninguna capacidad sobre datos del usuario:
//! [`Assistant::capabilities`] las lista como **no disponibles**, nunca como un
//! error ni como un método que falla (decisión de producto,
//! vasak-accounts#24). Lo que falta se ve que falta.
//!
//! # La memoria
//!
//! El asistente puede tener una memoria propia: una base de grafos cifrada en
//! la carpeta de estado del servicio, con la llave en el llavero de la sesión.
//! **Viene apagada**: existe sólo desde que la persona la enciende
//! ([`Assistant::set_memory_enabled`]). Apagarla no borra nada: deja de usarla
//! y encenderla de nuevo la reabre. Borrar es sólo [`Assistant::clear_memory`].
//! [`Assistant::memory_status`] dice si está encendida, si se puede usar y
//! cuánto ocupa.
//!
//! Qué guarda: lo que la persona dice de sí misma —preferencias, cómo quiere
//! que le hablen, hábitos, temas, correcciones, nombres que usa— y resúmenes
//! breves de conversaciones. Lo aprende solo después de responder, y lo usa
//! como notas en el contexto del modelo, nunca en el historial. **No guarda
//! datos sensibles que no son suyos** (correo, calendario, contactos,
//! archivos, cuentas, claves): esos se leen de su servicio en el momento. Con
//! un proveedor remoto, la memoria no se lee ni se escribe salvo que la
//! persona lo permita (`share_memory_with_remote` en `vasak.conf`); si no,
//! `MemoryStatus` y `Capabilities` dicen «La memoria no se comparte con un
//! proveedor remoto». La persona puede ver qué recuerda
//! ([`Assistant::list_memories`]) y borrar un recuerdo equivocado por su id
//! ([`Assistant::delete_memory`]), pero no editarlo: para cambiarlo se borra
//! y el asistente lo vuelve a aprender.

use serde::{Deserialize, Serialize};
use zbus::zvariant::Type;

// ── Direcciones del bus ───────────────────────────────────────────────────────

/// El servicio corre en el bus de **sesión**, como el usuario con la sesión
/// abierta: las conversaciones son de quien está sentado en la máquina.
pub const SERVICE_NAME: &str = "ar.net.vasak.os.Assistant";
pub const SERVICE_PATH: &str = "/ar/net/vasak/os/Assistant";
pub const SERVICE_INTERFACE: &str = "ar.net.vasak.os.Assistant";

/// El unit de systemd que provee el servicio. La activación por D-Bus mapea
/// [`SERVICE_NAME`] a este unit (`packaging/ar.net.vasak.os.Assistant.service`,
/// `SystemdService=`), así que el proceso que de verdad es dueño del nombre
/// corre dentro de su cgroup. Es el ancla con la que un cliente comprueba que
/// le está hablando al demonio real y no a un impostor que reclamó el nombre
/// antes ([`verify_service_owner`], [`cgroup_is_service`]).
pub const SERVICE_UNIT: &str = "vasak-assistant.service";

// ── Límites ───────────────────────────────────────────────────────────────────
//
// Están en el contrato y no escondidos en el servicio para que un cliente pueda
// cortar antes de mandar, en vez de enterarse por el error.

/// El tamaño máximo, en bytes UTF-8, de un mensaje que se le manda a `Chat`.
pub const MAX_MESSAGE_BYTES: usize = 32 * 1024;

/// Cuántas conversaciones abiertas puede haber a la vez, entre todos los
/// clientes.
pub const MAX_CONVERSATIONS: usize = 32;

/// Cuántas conversaciones abiertas puede tener una misma conexión.
pub const MAX_CONVERSATIONS_PER_OWNER: usize = 8;

/// Cuántos `Chat` puede tener una misma conexión esperando respuesta a la vez,
/// entre todas sus conversaciones. Pasado eso, `Chat` da `Busy` sin ocupar
/// lugar en el backend: así un solo cliente no acapara los pedidos al modelo
/// y los demás no esperan hasta el plazo de D-Bus.
pub const MAX_CHATS_IN_FLIGHT_PER_OWNER: usize = 2;

// El cupo de las aplicaciones confinadas (AppArmor) es **uno solo y compartido**
// entre todas, cualquiera sea su perfil y cuántas conexiones al bus abran: el
// cupo se cuenta por nombre único de conexión, y una app podría abrir varias
// para multiplicarlo. Con un bucket común y chico, siempre queda lugar en el
// total ([`MAX_CONVERSATIONS`]) para el chat de la persona, que corre sin
// confinar. La **propiedad** de cada conversación sigue por conexión: una app
// confinada no ve ni toca las de otra; lo único compartido es el conteo.

/// Cuántas conversaciones pueden tener abiertas, entre **todas** las
/// aplicaciones confinadas a la vez. Compartido, no por aplicación.
pub const MAX_CONVERSATIONS_CONFINED: usize = 3;

/// Cuántos `Chat` pueden tener en vuelo, entre **todas** las aplicaciones
/// confinadas a la vez. Compartido, no por aplicación.
pub const MAX_CHATS_IN_FLIGHT_CONFINED: usize = 1;

/// Cuántos mensajes guarda una conversación. Pasado eso se olvidan los más
/// viejos; el último intercambio se conserva siempre.
pub const MAX_HISTORY_MESSAGES: usize = 100;

/// Cuántos bytes de texto guarda una conversación entre todos sus mensajes.
/// Pasado eso se olvidan los más viejos, igual que con [`MAX_HISTORY_MESSAGES`].
pub const MAX_HISTORY_BYTES: usize = 512 * 1024;

// ── Proveedores ───────────────────────────────────────────────────────────────

/// Ollama, con su API propia (`/api/chat`, `/api/tags`).
pub const PROVIDER_OLLAMA: &str = "ollama";
/// Cualquier servidor compatible con la API de OpenAI (`/chat/completions`,
/// `/models`): el de OpenAI y la mayoría de los proveedores, pero también
/// llama.cpp server o LM Studio en la máquina.
pub const PROVIDER_OPENAI: &str = "openai";

// ── Dominios ──────────────────────────────────────────────────────────────────

/// Los dominios sobre los que el asistente puede llegar a tener una capacidad.
/// Viajan como texto y no como enumeración para que agregar uno en una fase
/// nueva no rompa a un cliente viejo que no lo conoce.
pub const DOMAIN_MAIL: &str = "mail";
pub const DOMAIN_CALENDAR: &str = "calendar";
pub const DOMAIN_CONTACTS: &str = "contacts";
pub const DOMAIN_FILES: &str = "files";
pub const DOMAIN_WEB: &str = "web";
pub const DOMAIN_MEMORY: &str = "memory";
pub const DOMAIN_REMOTE_BACKENDS: &str = "remote_backends";

/// Todos los dominios, en el orden en que se informan.
pub const DOMAINS: [&str; 7] = [
    DOMAIN_MAIL,
    DOMAIN_CALENDAR,
    DOMAIN_CONTACTS,
    DOMAIN_FILES,
    DOMAIN_WEB,
    DOMAIN_MEMORY,
    DOMAIN_REMOTE_BACKENDS,
];

// ── La conversación ───────────────────────────────────────────────────────────

/// Quién dijo un mensaje. Viaja como texto: `system`, `user` o `assistant`,
/// los mismos nombres que usan los backends de modelo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "lowercase")]
#[zvariant(signature = "s")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// Un mensaje de una conversación.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

// ── El estado ─────────────────────────────────────────────────────────────────

/// El modelo con el que responde el asistente.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ModelInfo {
    /// El proveedor: uno de los `PROVIDER_*`.
    pub provider: String,
    /// El nombre del modelo, tal como lo conoce el proveedor
    /// (`llama3.2:latest`). Vacío si todavía no hay ninguno.
    pub name: String,
    /// Si corre en la máquina: nada de la conversación sale de la caja. Sale de
    /// la dirección efectiva del backend (el host es de loopback), no de nada
    /// que alguien haya declarado. Es el dato para el indicador de «esto sale de
    /// la caja»: `false` quiere decir que cada mensaje viaja a [`Self::host`].
    pub local: bool,
    /// El host al que se le habla (`127.0.0.1`, `api.openai.com`). Vacío si la
    /// dirección configurada no se pudo leer.
    pub host: String,
}

/// Cómo está el servicio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct ServiceStatus {
    /// Si hay un modelo listo para responder.
    pub backend_available: bool,
    /// Por qué no, en palabras para mostrarle a la persona. Vacío si está
    /// disponible.
    pub reason: String,
    /// El modelo activo.
    pub model: ModelInfo,
    /// Cuántas conversaciones tiene abiertas quien pregunta.
    pub conversations: u32,
}

/// Una capacidad del asistente sobre un dominio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct Capability {
    /// Uno de los `DOMAIN_*`.
    pub domain: String,
    /// Si el asistente la tiene.
    pub available: bool,
    /// Por qué no, en palabras para mostrarle a la persona. Vacío si está
    /// disponible.
    pub reason: String,
}

/// Cómo está la memoria del asistente. Firma `(bbst)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Type)]
pub struct MemoryStatus {
    /// Si la persona la encendió, se pueda abrir o no.
    pub enabled: bool,
    /// Si está encendida y abierta, lista para usarse.
    pub available: bool,
    /// Por qué no está disponible, en palabras para mostrarle a la persona
    /// («La memoria está apagada», «El llavero está bloqueado», …). Vacío si
    /// está disponible.
    pub reason: String,
    /// Cuánto ocupa en disco, en bytes, encendida o apagada. Se mide sin
    /// abrirla; 0 si no hay base.
    pub bytes: u64,
}

/// Un recuerdo suelto, para mostrárselo a la persona y que pueda borrar el que
/// esté equivocado. Firma `(sssd)`. No lleva `Eq` porque `weight` es `f64`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Type)]
pub struct Memory {
    /// El identificador estable del recuerdo, el que toma
    /// [`Assistant::delete_memory`].
    pub id: String,
    /// La clase del recuerdo (`Preference`, `Habit`, `Correction`, …), para
    /// agrupar o filtrar en la interfaz.
    pub kind: String,
    /// El texto del recuerdo, tal como lo guardó el asistente.
    pub text: String,
    /// El peso con el que pesa al elegir qué recordar: entre 0 y 1, más alto
    /// cuanto más se reforzó. Es la relevancia que puede ver la persona.
    pub weight: f64,
}

// ── Los errores ───────────────────────────────────────────────────────────────

/// Los errores que devuelve el servicio, con nombre propio en el bus
/// (`ar.net.vasak.os.Assistant.Error.Unavailable`, …) para que quien llama
/// distinga «no hay modelo» de «mandaste algo inválido» sin leer el texto.
#[derive(Debug, zbus::DBusError)]
#[zbus(prefix = "ar.net.vasak.os.Assistant.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    /// No hay un modelo que pueda responder: el backend no corre, no tiene el
    /// modelo, pide una clave que no está en el llavero, o la configuración
    /// eligió un remoto sin permitirlo. El texto lo explica para la persona.
    Unavailable(String),
    /// El modelo respondió algo que no se pudo usar, o tardó demasiado.
    Backend(String),
    /// Un argumento inválido: id mal formado, mensaje vacío o demasiado largo.
    InvalidArgs(String),
    /// No hay ninguna conversación con ese id.
    NotFound(String),
    /// Se llegó a [`MAX_CONVERSATIONS`] o a [`MAX_CONVERSATIONS_PER_OWNER`].
    LimitExceeded(String),
    /// La conversación ya está esperando una respuesta, o quien llama ya tiene
    /// [`MAX_CHATS_IN_FLIGHT_PER_OWNER`] chats esperando la suya. Es
    /// pasajero: se puede volver a intentar cuando llegue una respuesta.
    Busy(String),
    /// Quien llama no puede hacer eso: una aplicación confinada (AppArmor) no
    /// enciende, apaga ni borra la memoria, ni guarda o consulta claves.
    AccessDenied(String),
}

// ── Verificar al dueño del nombre ─────────────────────────────────────────────
//
// El demonio pide [`SERVICE_NAME`] sin reemplazo ni cola: si un proceso de la
// sesión reclama ese nombre antes de que el servicio arranque, el servicio no
// levanta y los clientes le mandarían las conversaciones al impostor. Antes de
// hablarle, un cliente resuelve quién es el dueño del nombre y comprueba que su
// proceso corre dentro del cgroup de [`SERVICE_UNIT`]: sólo el unit de systemd
// activado por D-Bus está ahí.

/// Decide, a partir del contenido de `/proc/<pid>/cgroup`, si el proceso corre
/// dentro del cgroup del unit `unit` (p. ej. [`SERVICE_UNIT`]).
///
/// Es una función **pura y sin dependencias de zbus** a propósito: el escritorio
/// está clavado en zbus 4 y replica el proxy a mano (ver el doc del proxy más
/// abajo), así que puede reusar esta misma lógica resolviendo el dueño y su pid
/// con su propia versión de zbus y pasándole acá el contenido del cgroup.
///
/// # Qué acepta y por qué (esto es de seguridad)
///
/// Formato de `/proc/<pid>/cgroup`: una línea por jerarquía,
/// `jerarquía:controladores:/ruta`; en cgroup v2 una sola línea `0::/ruta`.
/// Devuelve `true` si **el último segmento** de alguna de esas rutas es
/// exactamente `unit`. El proceso principal de un unit simple (el demonio es
/// `Type=exec`) vive directamente en el cgroup del unit, así que su ruta termina
/// en `/<unit>` —por ejemplo `/user.slice/user-1000.slice/user@1000.service/
/// app.slice/vasak-assistant.service`—.
///
/// Se compara el **segmento completo**, no un prefijo ni «contiene»: así
/// `…/vasak-assistant.service.evil` no pasa, y tampoco un unit distinto que lo
/// tenga como ancestro. Ante cualquier entrada rara (vacío, línea sin `:`, ruta
/// sin ese segmento) devuelve `false`: **falla cerrado**, no confía.
///
/// # Límites conocidos (para la revisión de seguridad)
///
/// - **No distingue el unit real de un cgroup forjado con el mismo nombre.**
///   Bajo el gestor de usuario de systemd (cgroup v2), el subárbol
///   `user@<uid>.service` está delegado y es propiedad del UID: cualquier
///   proceso del mismo usuario puede crear un cgroup hijo llamado
///   `vasak-assistant.service` y escribir su pid en `cgroup.procs` —sin
///   `systemd-run` y sin ser un unit—, y su última componente queda idéntica.
///   El cgroup no lleva la procedencia (activado por D-Bus vs forjado), así que
///   esto no se cierra sólo con la ruta. Sube la barrera frente a un squat
///   accidental o un atacante ingenuo, no frente a un atacante local que
///   controla su propio cgroup. El cierre real es del lado del demonio: pedir
///   el nombre con `DO_NOT_QUEUE` y abortar ruidoso si ya está tomado, con esta
///   verificación como defensa en capas.
/// - **Anidamiento.** Si systemd alguna vez deja el proceso principal en un
///   sub-cgroup por debajo del unit (p. ej. `Delegate=yes`), esto lo rechaza
///   (falla cerrado, rompe la verificación en vez de aceptar de más). El unit
///   actual no delega, así que no pasa; si cambiara, se relaja acá.
pub fn cgroup_is_service(cgroup_file: &str, unit: &str) -> bool {
    if unit.is_empty() {
        return false;
    }
    cgroup_file.lines().any(|line| {
        // La ruta es todo lo que sigue al segundo ':'. `splitn(3, ':')` deja el
        // resto intacto aunque la ruta tuviera algún ':'. Sin dos ':' la línea
        // no es del formato esperado.
        let Some(path) = line.splitn(3, ':').nth(2) else {
            return false;
        };
        path.rsplit('/').next() == Some(unit)
    })
}

/// Comprueba que el dueño de [`SERVICE_NAME`] en el bus es el demonio real
/// —su proceso corre dentro del cgroup de [`SERVICE_UNIT`]— antes de confiarle
/// una conversación.
///
/// Resuelve el dueño único del nombre (`GetNameOwner`), obtiene su pid
/// (`GetConnectionUnixProcessID`), lee `/proc/<pid>/cgroup` y le aplica
/// [`cgroup_is_service`]. `Ok(())` si coincide.
///
/// # Cuándo llamarla
///
/// Una vez, al construir el cliente del asistente o justo antes de abrir la
/// primera conversación. No hace falta repetirla en cada llamada.
///
/// # Límites
///
/// Es una comprobación **puntual**: entre que se verifica y que se le habla, el
/// dueño podría cambiar, o el pid podría reusarse (TOCTOU). Se acepta como
/// límite conocido. Para los otros, ver [`cgroup_is_service`].
///
/// **Fuera de systemd (desarrollo):** si el demonio se corrió a mano desde una
/// terminal, su cgroup no es el del unit y esto falla —correctamente: reporta
/// la verdad—. No hay ningún bypass por variable de entorno.
///
/// # Falla cerrado
///
/// Si no se puede resolver el dueño (nadie tiene el nombre), leer su pid o leer
/// el cgroup, devuelve `Err` ([`Error::AccessDenied`]) con el motivo: ante la
/// duda, no se confía. La variante es `AccessDenied` también cuando el dueño
/// existe pero no es el unit: el cliente no debe hablarle en ningún caso.
pub async fn verify_service_owner(connection: &zbus::Connection) -> Result<(), Error> {
    use zbus::names::BusName;

    let deny = |motivo: String| Error::AccessDenied(motivo);

    let bus = zbus::fdo::DBusProxy::new(connection)
        .await
        .map_err(|error| deny(format!("no se pudo contactar al bus de sesión: {error}")))?;

    let service = BusName::try_from(SERVICE_NAME)
        .map_err(|error| deny(format!("nombre de servicio inválido: {error}")))?;

    let owner = bus.get_name_owner(service).await.map_err(|error| {
        deny(format!(
            "nadie es dueño de {SERVICE_NAME}: el asistente no está en el bus ({error})"
        ))
    })?;

    let pid = bus
        .get_connection_unix_process_id(BusName::Unique(owner.into()))
        .await
        .map_err(|error| {
            deny(format!(
                "no se pudo leer el pid del dueño de {SERVICE_NAME}: {error}"
            ))
        })?;

    owner_runs_service(pid, read_proc_cgroup)
}

/// Lee `/proc/<pid>/cgroup`. `std::fs` síncrono a propósito, y aparte de la
/// `async fn`: es un pseudo-archivo del kernel, la lectura no toca disco ni
/// bloquea, y así el crate no arrastra un runtime async (lo usan clientes de
/// cualquier versión de zbus).
fn read_proc_cgroup(pid: u32) -> std::io::Result<String> {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
}

/// Con el pid del dueño del nombre ya resuelto: lee su cgroup con `read_cgroup`
/// y comprueba que corre dentro de [`SERVICE_UNIT`]. La decisión está separada
/// de la resolución por el bus para poder probarla con un lector de prueba.
/// Falla cerrado: si el cgroup no se puede leer o no es el unit, `AccessDenied`.
fn owner_runs_service(
    pid: u32,
    read_cgroup: impl FnOnce(u32) -> std::io::Result<String>,
) -> Result<(), Error> {
    let cgroup = read_cgroup(pid).map_err(|error| {
        Error::AccessDenied(format!("no se pudo leer el cgroup del pid {pid}: {error}"))
    })?;
    if cgroup_is_service(&cgroup, SERVICE_UNIT) {
        Ok(())
    } else {
        Err(Error::AccessDenied(format!(
            "el dueño de {SERVICE_NAME} (pid {pid}) no corre dentro de {SERVICE_UNIT}: \
             no es el servicio del asistente"
        )))
    }
}

// ── El proxy, para quien esté en zbus 5 ───────────────────────────────────────

/// El lado cliente del servicio.
///
/// Lo usa quien esté en la misma versión de zbus que el servicio. Quien esté
/// clavado en zbus 4 (el escritorio) replica estas firmas a mano.
///
/// # Las conversaciones son de la conexión que las abrió
///
/// `NewConversation` deja como dueña a la conexión que llama (su nombre único
/// en el bus). Sólo ella la ve en `Conversations`, y sólo ella puede usar
/// `History`, `Chat` y `DeleteConversation` con ese id; para cualquier otra, el
/// id da `NotFound`, igual que uno que no existe. Cuando la conexión se cierra,
/// sus conversaciones se olvidan. `Status` cuenta sólo las de quien pregunta.
/// Una llamada sin remitente (una conexión punto a punto, por ejemplo) a
/// cualquiera de esos métodos da `AccessDenied`: sin remitente no hay dueño.
#[zbus::proxy(
    interface = "ar.net.vasak.os.Assistant",
    default_service = "ar.net.vasak.os.Assistant",
    default_path = "/ar/net/vasak/os/Assistant",
    gen_blocking = false
)]
pub trait Assistant {
    /// Cómo está el servicio: si hay modelo, cuál, y por qué no si no.
    fn status(&self) -> Result<ServiceStatus, Error>;

    /// Las capacidades sobre cada dominio de [`DOMAINS`]. En esta fase las de
    /// datos del usuario vienen como no disponibles; `remote_backends` dice si
    /// la configuración permite un proveedor remoto, y `memory` está
    /// disponible si la memoria está encendida y abierta (si no, el motivo es
    /// el de [`Self::memory_status`], o «La memoria está apagada»).
    ///
    /// A una aplicación confinada (AppArmor), `memory` llega siempre no
    /// disponible con el motivo «No disponible para aplicaciones confinadas»,
    /// sin abrir la base ni preguntar al llavero.
    fn capabilities(&self) -> Result<Vec<Capability>, Error>;

    /// Los modelos instalados en el backend. `Unavailable` si no responde.
    /// `Busy` si hay demasiados pedidos de la lista a la vez; es pasajero.
    fn models(&self) -> Result<Vec<String>, Error>;

    /// Abre una conversación nueva, de quien llama, y devuelve su id.
    fn new_conversation(&self) -> Result<String, Error>;

    /// Las conversaciones abiertas de quien llama.
    fn conversations(&self) -> Result<Vec<String>, Error>;

    /// Los mensajes de una conversación, del más viejo al más nuevo.
    fn history(&self, conversation_id: &str) -> Result<Vec<Message>, Error>;

    /// Le manda un mensaje a la conversación y devuelve la respuesta del
    /// modelo. Si falla, la conversación queda como estaba.
    fn chat(&self, conversation_id: &str, content: &str) -> Result<String, Error>;

    /// Cierra una conversación y olvida sus mensajes.
    fn delete_conversation(&self, conversation_id: &str) -> Result<(), Error>;

    /// Guarda en el llavero de la sesión la clave del proveedor de `url`,
    /// reemplazando la que hubiera. Es lo que usa el panel de Configuración:
    /// así sólo el servicio escribe el ítem del llavero.
    ///
    /// `url` se normaliza igual que la de `vasak.conf` (`http` o `https`, sin
    /// consulta ni fragmento, con `/` final), así que
    /// `https://api.openai.com/v1` y `https://api.openai.com/v1/` son la misma
    /// clave. La clave se recorta y tiene que tener de 1 a 4096 bytes sin
    /// caracteres de control.
    ///
    /// - `InvalidArgs`: la dirección no se puede usar (ilegible, otro esquema,
    ///   sin host, con usuario o contraseña, o un host remoto que no es
    ///   `https`) o la clave no es válida.
    /// - `Unavailable`: el llavero está bloqueado o no responde. No se abre
    ///   ningún diálogo de desbloqueo: el motivo dice qué hacer.
    ///
    /// La clave nunca vuelve en un error ni va al registro.
    ///
    /// `AccessDenied` si quien llama es una aplicación confinada (AppArmor).
    fn set_provider_key(&self, url: &str, key: &str) -> Result<(), Error>;

    /// Si hay una clave guardada en el llavero para `url` (normalizada igual
    /// que en [`Self::set_provider_key`]), aunque el llavero esté bloqueado.
    /// Si hay más de una para la misma dirección da `false`: el servicio no usa
    /// ninguna hasta que quede una sola. Nunca devuelve la clave. `InvalidArgs` si la dirección no se puede usar,
    /// `Unavailable` si el llavero no responde.
    /// `AccessDenied` si quien llama es una aplicación confinada (AppArmor).
    fn has_provider_key(&self, url: &str) -> Result<bool, Error>;

    /// Cómo está la memoria. Para quien puede preguntar no falla (salvo el
    /// bus): cualquier problema llega como `available: false` con el motivo
    /// (también el de un proveedor remoto sin permiso, o el de un modelo por
    /// omisión en la nube sin `allow_remote`). `bytes` se mide sin abrir la
    /// base.
    ///
    /// `AccessDenied` si quien llama es una aplicación confinada (AppArmor):
    /// lo que ocupa crece con el uso.
    fn memory_status(&self) -> Result<MemoryStatus, Error>;

    /// Enciende o apaga la memoria.
    ///
    /// - Encender reabre la base que haya, con la llave que está en el
    ///   llavero; si no hay, genera una llave nueva, la guarda en el llavero y
    ///   crea la base vacía y cifrada. `Unavailable` si el llavero está
    ///   bloqueado o no responde (nunca se abre un diálogo), si la llave falta
    ///   o no abre la base existente, o si no se pudo crear. Si ya estaba
    ///   encendida y abierta, no hace nada.
    /// - Apagar **no borra nada**: deja de usarla y de escribirla; los archivos
    ///   y la llave quedan.
    ///
    /// `AccessDenied` si quien llama es una aplicación confinada (AppArmor).
    fn set_memory_enabled(&self, enabled: bool) -> Result<(), Error>;

    /// Borra la memoria: primero la llave del llavero y después los archivos.
    /// Si estaba encendida, la recrea vacía con una llave nueva. `Unavailable`
    /// si el llavero está bloqueado (entonces no se toca nada) o no responde.
    /// Es la salida cuando la llave falta o no abre la base.
    /// `AccessDenied` si quien llama es una aplicación confinada (AppArmor).
    fn clear_memory(&self) -> Result<(), Error>;

    /// Los recuerdos que guarda, para que la persona vea qué recuerda el
    /// asistente y pueda borrar el que esté equivocado. Lista vacía si la
    /// memoria está apagada, no hay base o no se comparte con el remoto;
    /// `Unavailable` si el llavero está bloqueado (no se puede abrir la base).
    /// `AccessDenied` si quien llama es una aplicación confinada (AppArmor).
    fn list_memories(&self) -> Result<Vec<Memory>, Error>;

    /// Borra un recuerdo por su `id` (el de [`Self::list_memories`]). Sólo ese:
    /// no toca el resto de la memoria ni el aprendizaje en curso. Borrar uno
    /// que ya no existe no es error, termina en `Ok`. Apagada, `Ok` sin hacer
    /// nada (encender crea la base, así que encendida siempre la hay);
    /// `Unavailable` si el llavero está bloqueado o la base no abre (la llave
    /// falta o está dañada): el borrado no ocurrió y no se finge un `Ok`.
    /// `AccessDenied` si quien llama es una aplicación confinada (AppArmor).
    fn delete_memory(&self, id: &str) -> Result<(), Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{serialized::Context, to_bytes, LE};
    use zbus::DBusError;

    fn signature<T: Type>() -> String {
        T::SIGNATURE.to_string()
    }

    /// Ida y vuelta por la codificación de D-Bus, que es la del cable.
    fn round_trip<T>(value: &T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de> + Type,
    {
        let context = Context::new_dbus(LE, 0);
        let bytes = to_bytes(context, value).unwrap();
        bytes.deserialize().unwrap().0
    }

    #[test]
    fn la_firma_de_cada_tipo_es_la_del_contrato() {
        // Quien replica estas firmas a mano (el escritorio, en zbus 4) depende
        // de que no cambien sin avisar.
        assert_eq!(signature::<Role>(), "s");
        assert_eq!(signature::<Message>(), "(ss)");
        assert_eq!(signature::<ModelInfo>(), "(ssbs)");
        assert_eq!(signature::<ServiceStatus>(), "(bs(ssbs)u)");
        assert_eq!(signature::<Capability>(), "(sbs)");
        assert_eq!(signature::<MemoryStatus>(), "(bbst)");
    }

    #[test]
    fn el_estado_de_la_memoria_hace_ida_y_vuelta_por_dbus() {
        for status in [
            MemoryStatus {
                enabled: true,
                available: true,
                reason: String::new(),
                bytes: 4 << 20,
            },
            MemoryStatus {
                enabled: false,
                available: false,
                reason: "La memoria está apagada".into(),
                bytes: u64::MAX,
            },
        ] {
            assert_eq!(round_trip(&status), status);
        }
    }

    #[test]
    fn los_roles_viajan_como_texto_en_minuscula() {
        for (role, name) in [
            (Role::System, "system"),
            (Role::User, "user"),
            (Role::Assistant, "assistant"),
        ] {
            let bytes = to_bytes(Context::new_dbus(LE, 0), &role).unwrap();
            // Un string D-Bus: largo (u32), texto y NUL final.
            let mut expected = (name.len() as u32).to_le_bytes().to_vec();
            expected.extend_from_slice(name.as_bytes());
            expected.push(0);
            assert_eq!(&*bytes, expected.as_slice(), "{role:?}");
            assert_eq!(round_trip(&role), role);
        }
    }

    #[test]
    fn los_tipos_sobreviven_a_la_codificacion_de_dbus() {
        let message = Message {
            role: Role::Assistant,
            content: "Hola, ¿qué tal? 👋".into(),
        };
        assert_eq!(round_trip(&message), message);

        let status = ServiceStatus {
            backend_available: true,
            reason: String::new(),
            model: ModelInfo {
                provider: PROVIDER_OLLAMA.into(),
                name: "llama3.2:latest".into(),
                local: true,
                host: "127.0.0.1".into(),
            },
            conversations: 3,
        };
        assert_eq!(round_trip(&status), status);

        let capabilities: Vec<Capability> = DOMAINS
            .iter()
            .map(|domain| Capability {
                domain: (*domain).into(),
                available: false,
                reason: "todavía no".into(),
            })
            .collect();
        assert_eq!(round_trip(&capabilities), capabilities);
    }

    #[test]
    fn cada_error_tiene_su_nombre_propio_en_el_bus() {
        let prefix = "ar.net.vasak.os.Assistant.Error";
        let casos = [
            (Error::Unavailable(String::new()), "Unavailable"),
            (Error::Backend(String::new()), "Backend"),
            (Error::InvalidArgs(String::new()), "InvalidArgs"),
            (Error::NotFound(String::new()), "NotFound"),
            (Error::LimitExceeded(String::new()), "LimitExceeded"),
            (Error::Busy(String::new()), "Busy"),
            (Error::AccessDenied(String::new()), "AccessDenied"),
        ];
        for (error, name) in casos {
            assert_eq!(error.name().as_str(), format!("{prefix}.{name}"));
        }
    }

    #[test]
    fn el_texto_del_error_viaja_como_descripcion() {
        assert_eq!(
            Error::Busy("esperando".into()).description(),
            Some("esperando")
        );
    }

    #[test]
    fn las_direcciones_del_servicio_son_coherentes_entre_si() {
        assert_eq!(SERVICE_INTERFACE, SERVICE_NAME);
        assert_eq!(SERVICE_PATH, format!("/{}", SERVICE_NAME.replace('.', "/")));
    }

    #[test]
    fn los_dominios_son_siete_sin_repetirse_y_terminan_en_remote_backends() {
        assert_eq!(DOMAINS.len(), 7);
        let mut sorted = DOMAINS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), DOMAINS.len());
        assert_eq!(DOMAINS.last(), Some(&DOMAIN_REMOTE_BACKENDS));
        assert!(DOMAINS.contains(&DOMAIN_MAIL));
    }

    #[test]
    fn los_limites_son_coherentes_para_que_un_mensaje_siempre_quepa_en_el_historial() {
        const {
            assert!(MAX_MESSAGE_BYTES * 2 <= MAX_HISTORY_BYTES);
            assert!(MAX_HISTORY_MESSAGES >= 2);
            assert!(MAX_CONVERSATIONS > 0);
        }
    }

    mod dueno_del_nombre {
        use super::*;

        const UNIT: &str = "vasak-assistant.service";
        const V2: &str =
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/vasak-assistant.service";

        #[test]
        fn el_unit_del_contrato_es_el_del_demonio() {
            assert_eq!(SERVICE_UNIT, UNIT);
        }

        #[test]
        fn cgroup_v2_real_es_el_servicio_con_y_sin_salto_final() {
            assert!(cgroup_is_service(&format!("{V2}\n"), UNIT));
            assert!(cgroup_is_service(V2, UNIT));
        }

        #[test]
        fn cgroup_v1_con_varias_lineas_acepta_si_una_termina_en_el_unit() {
            let v1 = "12:pids:/user.slice/user@1000.service/app.slice/vasak-assistant.service\n\
                      4:cpu:/user.slice\n\
                      0::/user.slice/user@1000.service/app.slice/vasak-assistant.service\n";
            assert!(cgroup_is_service(v1, UNIT));
            // Basta una línea buena aunque las otras no lo sean.
            assert!(cgroup_is_service(
                "4:cpu:/user.slice\n0::/x/vasak-assistant.service",
                UNIT
            ));
        }

        #[test]
        fn un_sufijo_pegado_al_unit_no_pasa() {
            for evil in [
                "0::/user.slice/vasak-assistant.service.evil",
                "0::/user.slice/vasak-assistant.service-evil",
                "0::/user.slice/xvasak-assistant.service",
                "0::/user.slice/evil-vasak-assistant.service",
            ] {
                assert!(!cgroup_is_service(evil, UNIT), "{evil}");
            }
        }

        #[test]
        fn el_unit_en_un_segmento_intermedio_no_pasa() {
            // Anidamiento: el unit no es el último segmento, falla cerrado.
            assert!(!cgroup_is_service(
                "0::/a/vasak-assistant.service/algo",
                UNIT
            ));
            assert!(!cgroup_is_service("0::/a/vasak-assistant.service/", UNIT));
        }

        #[test]
        fn otro_unit_no_pasa() {
            assert!(!cgroup_is_service("0::/user.slice/otra-cosa.service", UNIT));
        }

        #[test]
        fn entrada_vacia_o_malformada_no_pasa() {
            assert!(!cgroup_is_service("", UNIT));
            assert!(!cgroup_is_service("\n\n", UNIT));
            assert!(!cgroup_is_service("vasak-assistant.service", UNIT));
            assert!(!cgroup_is_service("0:/vasak-assistant.service", UNIT));
            assert!(!cgroup_is_service("basura sin dos puntos", UNIT));
        }

        #[test]
        fn un_unit_vacio_no_pasa_contra_ningun_cgroup() {
            assert!(!cgroup_is_service(V2, ""));
            assert!(!cgroup_is_service("0::/", ""));
            assert!(!cgroup_is_service("0::", ""));
            assert!(!cgroup_is_service("", ""));
        }

        #[test]
        fn una_ruta_sin_segmentos_no_pasa() {
            assert!(!cgroup_is_service("0::/", UNIT));
            assert!(!cgroup_is_service("0::", UNIT));
            assert!(!cgroup_is_service("0::///", UNIT));
        }
    }

    mod owner_runs_service {
        use super::*;
        use std::io::{Error as IoError, ErrorKind};

        #[test]
        fn el_duenio_que_corre_en_el_unit_pasa() {
            let leer = |_| Ok("0::/user.slice/app.slice/vasak-assistant.service\n".to_string());
            assert!(owner_runs_service(123, leer).is_ok());
        }

        #[test]
        fn un_proceso_que_no_es_el_unit_se_niega() {
            let leer = |_| Ok("0::/user.slice/app.slice/impostor.service\n".to_string());
            match owner_runs_service(123, leer) {
                Err(Error::AccessDenied(motivo)) => {
                    assert!(motivo.contains("123"), "{motivo}");
                    assert!(!motivo.contains("impostor"), "{motivo}");
                }
                otro => panic!("se esperaba AccessDenied, vino {otro:?}"),
            }
        }

        #[test]
        fn si_no_se_puede_leer_el_cgroup_falla_cerrado() {
            let leer = |_| Err(IoError::new(ErrorKind::NotFound, "no such process"));
            match owner_runs_service(456, leer) {
                Err(Error::AccessDenied(motivo)) => assert!(motivo.contains("456"), "{motivo}"),
                otro => panic!("se esperaba AccessDenied, vino {otro:?}"),
            }
        }
    }
    mod lectura_real {
        use super::*;

        #[test]
        fn el_cgroup_del_propio_proceso_se_lee() {
            let cgroup = read_proc_cgroup(std::process::id()).unwrap();
            assert!(cgroup.contains('/'), "{cgroup}");
        }

        #[test]
        fn el_proceso_de_prueba_no_es_el_unit() {
            let pid = std::process::id();
            assert!(matches!(
                owner_runs_service(pid, read_proc_cgroup),
                Err(Error::AccessDenied(_))
            ));
        }

        #[test]
        fn un_pid_inexistente_no_se_puede_leer() {
            assert!(read_proc_cgroup(u32::MAX).is_err());
        }

        /// Un `org.freedesktop.DBus` de mentira: lo que responde cada método
        /// lo decide la prueba.
        struct FakeBus {
            owner: Option<&'static str>,
            pid: Option<u32>,
        }

        #[zbus::interface(name = "org.freedesktop.DBus")]
        impl FakeBus {
            fn get_name_owner(&self, _name: String) -> zbus::fdo::Result<String> {
                self.owner
                    .map(String::from)
                    .ok_or_else(|| zbus::fdo::Error::NameHasNoOwner("sin dueño".into()))
            }

            #[zbus(name = "GetConnectionUnixProcessID")]
            fn get_connection_unix_process_id(&self, _name: String) -> zbus::fdo::Result<u32> {
                self.pid
                    .ok_or_else(|| zbus::fdo::Error::Failed("sin pid".into()))
            }
        }

        async fn verificar_contra(bus: FakeBus) -> Result<(), Error> {
            let (a, b) = tokio::net::UnixStream::pair().unwrap();
            let server = zbus::connection::Builder::unix_stream(a)
                .server(zbus::Guid::generate())
                .unwrap()
                .p2p()
                .serve_at("/org/freedesktop/DBus", bus)
                .unwrap()
                .build();
            let client = zbus::connection::Builder::unix_stream(b).p2p().build();
            let (_server, client) = tokio::try_join!(server, client).unwrap();
            verify_service_owner(&client).await
        }

        fn motivo(resultado: Result<(), Error>) -> String {
            match resultado {
                Err(Error::AccessDenied(motivo)) => motivo,
                otro => panic!("se esperaba AccessDenied, vino {otro:?}"),
            }
        }

        #[tokio::test]
        async fn si_nadie_es_duenio_del_nombre_falla_cerrado() {
            let bus = FakeBus {
                owner: None,
                pid: None,
            };
            let m = motivo(verificar_contra(bus).await);
            assert!(m.contains("nadie es dueño"), "{m}");
        }

        #[tokio::test]
        async fn si_no_se_puede_leer_el_pid_falla_cerrado() {
            let bus = FakeBus {
                owner: Some(":1.9"),
                pid: None,
            };
            let m = motivo(verificar_contra(bus).await);
            assert!(m.contains("pid del dueño"), "{m}");
        }

        #[tokio::test]
        async fn un_duenio_que_no_corre_en_el_unit_se_niega() {
            let bus = FakeBus {
                owner: Some(":1.9"),
                pid: Some(std::process::id()),
            };
            let m = motivo(verificar_contra(bus).await);
            assert!(m.contains("no es el servicio del asistente"), "{m}");
        }
    }
}
