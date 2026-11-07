//! El motor de conversación: las conversaciones abiertas y su historial, en
//! memoria y nada más. Se pierden al cerrar el servicio; la memoria que
//! persiste es la fase 4, sobre el almacén cifrado.
//!
//! Acá no hay ninguna E/S: el turno se parte en dos ([`Conversations::begin_turn`]
//! y [`Conversations::finish_turn`]) para que el pedido al modelo, que tarda,
//! ocurra afuera y sin el candado tomado.
//!
//! Los límites del contrato (`MAX_*` de `vasak-assistant-protocol`) se aplican
//! acá, en la frontera: un cliente del bus no puede hacer crecer la memoria del
//! servicio sin techo.
//!
//! Cada conversación es de la conexión que la abrió (su nombre único en el bus,
//! `:1.42`). Nadie más la ve: para cualquier otro, una conversación ajena da el
//! mismo `NotFound` que una que no existe, así ni siquiera se confirma que el id
//! esté en uso. Cuando la conexión se va, sus conversaciones se liberan
//! ([`Conversations::release_owner`]).
//!
//! Los topes por dueño se cuentan por nombre único, y un proceso que abre
//! varias conexiones los esquiva. Por eso los llamadores confinados por
//! AppArmor cuentan además como un solo grupo (#32): entre todos, a lo sumo
//! `MAX_CONFINED_CONVERSATIONS` abiertas y `MAX_CONFINED_CHATS_IN_FLIGHT` en
//! vuelo. Un proceso sin confinar queda fuera de alcance: igual puede leer o
//! matar al servicio.

use std::collections::HashMap;

use uuid::Uuid;
use vasak_assistant_protocol::{
    Error, Message, Role, MAX_CHATS_IN_FLIGHT_PER_OWNER, MAX_CONFINED_CHATS_IN_FLIGHT,
    MAX_CONFINED_CONVERSATIONS, MAX_CONVERSATIONS, MAX_CONVERSATIONS_PER_OWNER, MAX_HISTORY_BYTES,
    MAX_HISTORY_MESSAGES, MAX_MESSAGE_BYTES,
};

struct Conversation {
    /// El nombre único de la conexión que la abrió.
    owner: String,
    /// La conexión que la abrió está confinada: cuenta en el cupo compartido
    /// de los confinados. La etiqueta de una conexión no cambia.
    confined: bool,
    messages: Vec<Message>,
    /// Hay un pedido al modelo en curso: un segundo `Chat` sobre la misma
    /// conversación se rechaza en vez de entreverar las respuestas.
    busy: bool,
}

struct InFlight {
    owner: String,
    confined: bool,
}

#[derive(Default)]
pub struct Conversations {
    open: HashMap<String, Conversation>,
    /// Los chats en vuelo: id de la conversación → su dueño y si está
    /// confinado. Se anota en
    /// `begin_turn` y se saca en `finish_turn` (que corre siempre, por el
    /// `Drop` del turno), exista o no la conversación: borrarla mientras espera
    /// al modelo no devuelve el lugar, porque el pedido sigue ocupando el
    /// backend. Sólo tiene los que están en vuelo, así que no acumula nada.
    in_flight: HashMap<String, InFlight>,
}

impl Conversations {
    /// Cuántas conversaciones hay abiertas en total, de todos los dueños.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Cuántas conversaciones tiene abiertas `owner`.
    pub fn count_for(&self, owner: &str) -> usize {
        self.open.values().filter(|c| c.owner == owner).count()
    }

    /// Abre una conversación para `owner` y devuelve su id. Hay un tope por
    /// dueño, para que un solo cliente no se quede con todas, además del total
    /// y, si `confined`, el compartido de los confinados.
    pub fn create(&mut self, owner: &str, confined: bool) -> Result<String, Error> {
        if self.count_for(owner) >= MAX_CONVERSATIONS_PER_OWNER {
            return Err(Error::LimitExceeded(format!(
                "ya tenés {MAX_CONVERSATIONS_PER_OWNER} conversaciones abiertas; cerrá alguna"
            )));
        }
        if confined
            && self.open.values().filter(|c| c.confined).count() >= MAX_CONFINED_CONVERSATIONS
        {
            return Err(Error::LimitExceeded(format!(
                "las aplicaciones confinadas ya tienen {MAX_CONFINED_CONVERSATIONS} conversaciones abiertas entre todas; cerrá alguna"
            )));
        }
        if self.open.len() >= MAX_CONVERSATIONS {
            return Err(Error::LimitExceeded(format!(
                "ya hay {MAX_CONVERSATIONS} conversaciones abiertas; cerrá alguna"
            )));
        }
        let id = Uuid::new_v4().to_string();
        self.open.insert(
            id.clone(),
            Conversation {
                owner: owner.to_owned(),
                confined,
                messages: Vec::new(),
                busy: false,
            },
        );
        Ok(id)
    }

    /// Los ids de `owner`, ordenados para que la respuesta no cambie de una
    /// llamada a otra.
    pub fn ids(&self, owner: &str) -> Vec<String> {
        let mut ids: Vec<String> = self
            .open
            .iter()
            .filter(|(_, c)| c.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    pub fn history(&self, owner: &str, id: &str) -> Result<Vec<Message>, Error> {
        Ok(self.get(owner, id)?.messages.clone())
    }

    pub fn delete(&mut self, owner: &str, id: &str) -> Result<(), Error> {
        self.get(owner, id)?;
        // El chat en vuelo, si hay, sigue anotado en `in_flight` hasta que
        // termine: sigue ocupando el backend.
        self.open.remove(id);
        Ok(())
    }

    /// Olvida todas las conversaciones de `owner`: la conexión se fue. Devuelve
    /// cuántas eran.
    pub fn release_owner(&mut self, owner: &str) -> usize {
        let before = self.open.len();
        self.open.retain(|_, c| c.owner != owner);
        // Los chats en vuelo no se tocan: el pedido sigue ocupando el backend
        // aunque la conexión se haya ido, y sacarlo acá dejaría a un confinado
        // esquivar el cupo compartido desconectándose a mitad de un chat. La
        // entrada la saca siempre `finish_turn`, por el `Drop` del turno.
        before - self.open.len()
    }

    /// Empieza un turno: valida el mensaje, marca la conversación ocupada y
    /// devuelve lo que hay que mandarle al modelo (el historial más el mensaje
    /// nuevo). El mensaje todavía no se guarda: si el modelo falla, la
    /// conversación queda como estaba.
    pub fn begin_turn(
        &mut self,
        owner: &str,
        id: &str,
        content: &str,
    ) -> Result<Vec<Message>, Error> {
        validate_content(content)?;
        let conversation = self.get(owner, id)?;
        let confined = conversation.confined;
        if conversation.busy {
            return Err(Error::Busy(
                "la conversación todavía está esperando la respuesta anterior".into(),
            ));
        }
        // El tope por dueño (#3): con los chats en vuelo de una conexión
        // contados acá, bajo el mismo candado que los marca, un cliente no
        // ocupa todo el cupo del backend. `Busy` y no `LimitExceeded`: es
        // pasajero, alcanza con esperar a que llegue una respuesta, mientras
        // que `LimitExceeded` es el tope de conversaciones abiertas, que no se
        // libera solo.
        let in_flight = self
            .in_flight
            .values()
            .filter(|chat| chat.owner == owner)
            .count();
        if in_flight >= MAX_CHATS_IN_FLIGHT_PER_OWNER {
            return Err(Error::Busy(format!(
                "ya hay {in_flight} pedidos esperando respuesta del modelo; esperá a que termine uno"
            )));
        }
        // El cupo compartido de los confinados (#32), aparte del de cada dueño:
        // abrir más conexiones no da más lugares.
        if confined
            && self.in_flight.values().filter(|chat| chat.confined).count()
                >= MAX_CONFINED_CHATS_IN_FLIGHT
        {
            return Err(Error::Busy(format!(
                "las aplicaciones confinadas ya tienen {MAX_CONFINED_CHATS_IN_FLIGHT} pedidos esperando respuesta del modelo; esperá a que termine uno"
            )));
        }
        self.in_flight.insert(
            id.to_owned(),
            InFlight {
                owner: owner.to_owned(),
                confined,
            },
        );
        let conversation = self.get_mut(owner, id)?;
        conversation.busy = true;
        let mut context = conversation.messages.clone();
        context.push(Message {
            role: Role::User,
            content: content.to_owned(),
        });
        Ok(context)
    }

    /// Termina un turno. Con respuesta, guarda el mensaje y la respuesta y
    /// recorta el historial; sin respuesta, sólo libera la conversación. Si la
    /// conversación se cerró mientras tanto, no hay nada que guardar. Sin
    /// dueño: sólo se llama después de un `begin_turn` que ya lo comprobó, y un
    /// id (UUID v4) no vuelve a darse.
    pub fn finish_turn(&mut self, id: &str, content: &str, reply: Option<&str>) {
        self.in_flight.remove(id);
        let Some(conversation) = self.open.get_mut(id) else {
            return;
        };
        conversation.busy = false;
        if let Some(reply) = reply {
            conversation.messages.push(Message {
                role: Role::User,
                content: content.to_owned(),
            });
            conversation.messages.push(Message {
                role: Role::Assistant,
                content: reply.to_owned(),
            });
            trim_history(&mut conversation.messages);
        }
    }

    /// Si `owner` tiene abierta la conversación `id`.
    pub fn contains(&self, owner: &str, id: &str) -> bool {
        self.get(owner, id).is_ok()
    }

    fn get(&self, owner: &str, id: &str) -> Result<&Conversation, Error> {
        validate_id(id)?;
        self.open
            .get(id)
            .filter(|c| c.owner == owner)
            .ok_or_else(not_found)
    }

    fn get_mut(&mut self, owner: &str, id: &str) -> Result<&mut Conversation, Error> {
        validate_id(id)?;
        self.open
            .get_mut(id)
            .filter(|c| c.owner == owner)
            .ok_or_else(not_found)
    }
}

/// Un id es un UUID con guiones, como los que da [`Conversations::create`]. El
/// error no repite lo recibido: podría ser cualquier cosa, de cualquier largo.
pub fn validate_id(id: &str) -> Result<(), Error> {
    if id.len() == 36 && Uuid::try_parse(id).is_ok() {
        Ok(())
    } else {
        Err(Error::InvalidArgs("id de conversación mal formado".into()))
    }
}

/// Un mensaje no puede estar vacío ni pasar [`MAX_MESSAGE_BYTES`].
pub fn validate_content(content: &str) -> Result<(), Error> {
    if content.trim().is_empty() {
        return Err(Error::InvalidArgs("el mensaje está vacío".into()));
    }
    if content.len() > MAX_MESSAGE_BYTES {
        return Err(Error::InvalidArgs(format!(
            "el mensaje pasa el máximo de {MAX_MESSAGE_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Olvida los mensajes más viejos hasta quedar dentro de
/// [`MAX_HISTORY_MESSAGES`] y [`MAX_HISTORY_BYTES`]. El último intercambio
/// (mensaje y respuesta) se conserva siempre, aunque solo pase el tope de bytes.
pub fn trim_history(messages: &mut Vec<Message>) {
    let mut bytes: usize = messages.iter().map(|message| message.content.len()).sum();
    let mut drop = 0;
    while messages.len() - drop > 2
        && (messages.len() - drop > MAX_HISTORY_MESSAGES || bytes > MAX_HISTORY_BYTES)
    {
        bytes -= messages[drop].content.len();
        drop += 1;
    }
    messages.drain(..drop);
}

fn not_found() -> Error {
    Error::NotFound("no hay ninguna conversación con ese id".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = ":1.1";

    fn message(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
        }
    }

    /// Una conversación con `n` intercambios de mensajes cortos y distintos.
    fn with_turns(n: usize) -> (Conversations, String) {
        let mut conversations = Conversations::default();
        let id = conversations.create(OWNER, false).unwrap();
        for i in 0..n {
            let content = format!("pregunta {i}");
            conversations.begin_turn(OWNER, &id, &content).unwrap();
            conversations.finish_turn(&id, &content, Some(&format!("respuesta {i}")));
        }
        (conversations, id)
    }

    #[test]
    fn un_id_valido_es_un_uuid_con_guiones() {
        let mut conversations = Conversations::default();
        let id = conversations.create(OWNER, false).unwrap();
        assert!(validate_id(&id).is_ok());
    }

    #[test]
    fn un_id_mal_formado_se_rechaza_sin_repetirlo() {
        let largo = "a".repeat(10_000);
        let casos = [
            "",
            "x",
            "no-es-un-uuid-pero-tiene-36-caracteres",
            "67e5504410b1426f9247bb680e5fe0c8",
            "{67e55044-10b1-426f-9247-bb680e5fe0c8}",
            "urn:uuid:67e55044-10b1-426f-9247-bb680e5fe0c8",
            "../../etc/passwd",
            &largo,
        ];
        for id in casos {
            match validate_id(id) {
                Err(Error::InvalidArgs(text)) => {
                    assert!(text.len() < 100, "el error no debe repetir lo recibido");
                }
                other => panic!("{id:?} debía ser InvalidArgs, dio {other:?}"),
            }
        }
    }

    #[test]
    fn un_mensaje_vacio_o_de_espacios_se_rechaza() {
        for content in ["", "   ", "\n\t "] {
            assert!(matches!(
                validate_content(content),
                Err(Error::InvalidArgs(_))
            ));
        }
    }

    #[test]
    fn un_mensaje_acepta_justo_el_maximo_y_rechaza_un_byte_mas() {
        assert!(validate_content(&"a".repeat(MAX_MESSAGE_BYTES)).is_ok());
        assert!(matches!(
            validate_content(&"a".repeat(MAX_MESSAGE_BYTES + 1)),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn el_maximo_del_mensaje_se_mide_en_bytes_y_no_en_caracteres() {
        // Menos caracteres que el máximo, más bytes.
        let content = "é".repeat(MAX_MESSAGE_BYTES / 2 + 1);
        assert!(content.chars().count() <= MAX_MESSAGE_BYTES);
        assert!(matches!(
            validate_content(&content),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn no_se_pasa_del_maximo_de_conversaciones_y_cerrar_una_deja_abrir_otra() {
        // Repartidas entre varios dueños, para no toparse antes con el tope por
        // dueño.
        let owner = |i: usize| format!(":1.{}", i / MAX_CONVERSATIONS_PER_OWNER);
        let mut conversations = Conversations::default();
        let ids: Vec<String> = (0..MAX_CONVERSATIONS)
            .map(|i| conversations.create(&owner(i), false).unwrap())
            .collect();
        assert_eq!(conversations.len(), MAX_CONVERSATIONS);
        assert!(matches!(
            conversations.create(":1.999", false),
            Err(Error::LimitExceeded(_))
        ));
        assert_eq!(conversations.len(), MAX_CONVERSATIONS);

        conversations.delete(&owner(0), &ids[0]).unwrap();
        assert!(conversations.create(":1.999", false).is_ok());
    }

    #[test]
    fn los_ids_salen_ordenados_y_sin_repetirse() {
        let mut conversations = Conversations::default();
        for _ in 0..5 {
            conversations.create(OWNER, false).unwrap();
        }
        let ids = conversations.ids(OWNER);
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids.len(), 5);
        assert_eq!(ids, sorted);
    }

    #[test]
    fn una_conversacion_nueva_no_tiene_mensajes() {
        let mut conversations = Conversations::default();
        let id = conversations.create(OWNER, false).unwrap();
        assert!(conversations.history(OWNER, &id).unwrap().is_empty());
    }

    #[test]
    fn pedir_el_historial_de_un_id_inexistente_o_invalido_da_el_error_que_toca() {
        let (conversations, _) = with_turns(0);
        let otro = Uuid::new_v4().to_string();
        assert!(matches!(
            conversations.history(OWNER, &otro),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            conversations.history(OWNER, "basura"),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn cerrar_una_conversacion_olvida_sus_mensajes_y_no_se_puede_cerrar_dos_veces() {
        let (mut conversations, id) = with_turns(1);
        conversations.delete(OWNER, &id).unwrap();
        assert_eq!(conversations.len(), 0);
        assert!(matches!(
            conversations.history(OWNER, &id),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            conversations.delete(OWNER, &id),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            conversations.delete(OWNER, "basura"),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn el_turno_le_da_al_modelo_el_historial_mas_el_mensaje_nuevo_sin_guardarlo() {
        let (mut conversations, id) = with_turns(1);
        let context = conversations
            .begin_turn(OWNER, &id, "hola de nuevo")
            .unwrap();
        assert_eq!(
            context,
            vec![
                message(Role::User, "pregunta 0"),
                message(Role::Assistant, "respuesta 0"),
                message(Role::User, "hola de nuevo"),
            ]
        );
        assert_eq!(conversations.history(OWNER, &id).unwrap().len(), 2);
    }

    #[test]
    fn un_turno_exitoso_guarda_el_mensaje_y_la_respuesta() {
        let (conversations, id) = with_turns(2);
        assert_eq!(
            conversations.history(OWNER, &id).unwrap(),
            vec![
                message(Role::User, "pregunta 0"),
                message(Role::Assistant, "respuesta 0"),
                message(Role::User, "pregunta 1"),
                message(Role::Assistant, "respuesta 1"),
            ]
        );
    }

    #[test]
    fn un_segundo_turno_sobre_la_misma_conversacion_da_busy() {
        let (mut conversations, id) = with_turns(0);
        conversations.begin_turn(OWNER, &id, "uno").unwrap();
        assert!(matches!(
            conversations.begin_turn(OWNER, &id, "dos"),
            Err(Error::Busy(_))
        ));
    }

    #[test]
    fn el_turno_de_una_conversacion_no_ocupa_a_las_otras() {
        let mut conversations = Conversations::default();
        let a = conversations.create(OWNER, false).unwrap();
        let b = conversations.create(OWNER, false).unwrap();
        conversations.begin_turn(OWNER, &a, "uno").unwrap();
        assert!(conversations.begin_turn(OWNER, &b, "dos").is_ok());
    }

    #[test]
    fn un_turno_fallido_deja_la_conversacion_como_estaba_y_libre() {
        let (mut conversations, id) = with_turns(1);
        let antes = conversations.history(OWNER, &id).unwrap();
        conversations.begin_turn(OWNER, &id, "esto falla").unwrap();
        conversations.finish_turn(&id, "esto falla", None);
        assert_eq!(conversations.history(OWNER, &id).unwrap(), antes);
        assert!(conversations.begin_turn(OWNER, &id, "de nuevo").is_ok());
    }

    #[test]
    fn un_mensaje_invalido_no_deja_la_conversacion_ocupada() {
        let (mut conversations, id) = with_turns(0);
        assert!(matches!(
            conversations.begin_turn(OWNER, &id, "  "),
            Err(Error::InvalidArgs(_))
        ));
        assert!(conversations.begin_turn(OWNER, &id, "ahora sí").is_ok());
    }

    #[test]
    fn el_turno_valida_el_id_y_el_mensaje_antes_de_buscar() {
        let (mut conversations, _) = with_turns(0);
        let otro = Uuid::new_v4().to_string();
        assert!(matches!(
            conversations.begin_turn(OWNER, &otro, "hola"),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            conversations.begin_turn(OWNER, "basura", "hola"),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn terminar_el_turno_de_una_conversacion_cerrada_no_la_resucita() {
        let (mut conversations, id) = with_turns(0);
        conversations.begin_turn(OWNER, &id, "hola").unwrap();
        conversations.delete(OWNER, &id).unwrap();
        conversations.finish_turn(&id, "hola", Some("respuesta"));
        assert_eq!(conversations.len(), 0);
    }

    #[test]
    fn el_historial_se_recorta_al_maximo_de_mensajes_y_se_van_los_mas_viejos() {
        let turnos = MAX_HISTORY_MESSAGES; // el doble de mensajes que el tope
        let (conversations, id) = with_turns(turnos);
        let history = conversations.history(OWNER, &id).unwrap();
        assert_eq!(history.len(), MAX_HISTORY_MESSAGES);
        assert_eq!(
            history[0],
            message(Role::User, &format!("pregunta {}", turnos / 2))
        );
        assert_eq!(
            history.last().unwrap(),
            &message(Role::Assistant, &format!("respuesta {}", turnos - 1))
        );
    }

    #[test]
    fn trim_history_no_toca_lo_que_ya_cabe() {
        let mut messages: Vec<Message> = (0..10)
            .map(|i| message(Role::User, &i.to_string()))
            .collect();
        let copy = messages.clone();
        trim_history(&mut messages);
        assert_eq!(messages, copy);
    }

    #[test]
    fn trim_history_recorta_por_bytes_dejando_el_total_dentro_del_tope() {
        let cien_k = "x".repeat(100 * 1024);
        let mut messages: Vec<Message> = (0..6)
            .map(|i| {
                message(
                    if i % 2 == 0 {
                        Role::User
                    } else {
                        Role::Assistant
                    },
                    &cien_k,
                )
            })
            .collect();
        trim_history(&mut messages);
        let bytes: usize = messages.iter().map(|m| m.content.len()).sum();
        assert_eq!(messages.len(), 5);
        assert!(bytes <= MAX_HISTORY_BYTES);
        // Se fue el más viejo, no otro.
        assert_eq!(messages[0].role, Role::Assistant);
    }

    #[test]
    fn trim_history_conserva_el_ultimo_intercambio_aunque_solo_pase_el_tope_de_bytes() {
        let mut messages = vec![
            message(Role::User, "viejo"),
            message(Role::Assistant, "viejo"),
            message(Role::User, &"a".repeat(MAX_HISTORY_BYTES)),
            message(Role::Assistant, &"b".repeat(MAX_HISTORY_BYTES)),
        ];
        trim_history(&mut messages);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content.len(), MAX_HISTORY_BYTES);
        assert_eq!(messages[1].role, Role::Assistant);
    }

    #[test]
    fn trim_history_no_deja_vacio_ni_con_un_solo_intercambio_enorme() {
        let mut messages = vec![
            message(Role::User, &"a".repeat(MAX_HISTORY_BYTES)),
            message(Role::Assistant, &"b".repeat(MAX_HISTORY_BYTES)),
        ];
        trim_history(&mut messages);
        assert_eq!(messages.len(), 2);
    }

    // ── Dueños ────────────────────────────────────────────────────────────────

    const OTHER: &str = ":1.2";

    #[test]
    fn otro_dueno_recibe_not_found_igual_que_con_un_id_que_no_existe() {
        let (mut conversations, id) = with_turns(1);
        let inexistente = Uuid::new_v4().to_string();
        let texto = |e: Error| match e {
            Error::NotFound(text) => text,
            other => panic!("se esperaba NotFound, dio {other:?}"),
        };

        let ajeno = texto(conversations.history(OTHER, &id).unwrap_err());
        assert_eq!(
            ajeno,
            texto(conversations.history(OTHER, &inexistente).unwrap_err())
        );
        assert_eq!(
            texto(conversations.begin_turn(OTHER, &id, "hola").unwrap_err()),
            ajeno
        );
        assert_eq!(texto(conversations.delete(OTHER, &id).unwrap_err()), ajeno);
    }

    #[test]
    fn lo_que_intenta_otro_dueno_no_cambia_la_conversacion() {
        let (mut conversations, id) = with_turns(1);
        let antes = conversations.history(OWNER, &id).unwrap();

        let _ = conversations.begin_turn(OTHER, &id, "intruso");
        let _ = conversations.delete(OTHER, &id);

        assert_eq!(conversations.history(OWNER, &id).unwrap(), antes);
        // Tampoco la dejó ocupada para su dueño.
        assert!(conversations.begin_turn(OWNER, &id, "yo").is_ok());
    }

    #[test]
    fn cada_dueno_lista_y_cuenta_solo_lo_suyo() {
        let mut conversations = Conversations::default();
        let a = conversations.create(OWNER, false).unwrap();
        let b1 = conversations.create(OTHER, false).unwrap();
        let b2 = conversations.create(OTHER, false).unwrap();

        assert_eq!(conversations.ids(OWNER), [a]);
        let mut de_b = vec![b1, b2];
        de_b.sort();
        assert_eq!(conversations.ids(OTHER), de_b);
        assert!(conversations.ids(":1.3").is_empty());
        assert_eq!(conversations.count_for(OWNER), 1);
        assert_eq!(conversations.count_for(OTHER), 2);
        assert_eq!(conversations.count_for(":1.3"), 0);
    }

    #[test]
    fn el_tope_por_dueno_no_bloquea_a_otro_dueno() {
        let mut conversations = Conversations::default();
        for _ in 0..MAX_CONVERSATIONS_PER_OWNER {
            conversations.create(OWNER, false).unwrap();
        }
        assert!(matches!(
            conversations.create(OWNER, false),
            Err(Error::LimitExceeded(_))
        ));
        assert!(conversations.create(OTHER, false).is_ok());
        assert_eq!(conversations.count_for(OWNER), MAX_CONVERSATIONS_PER_OWNER);
    }

    #[test]
    fn cerrar_una_propia_libera_cupo_del_dueno() {
        let mut conversations = Conversations::default();
        let ids: Vec<String> = (0..MAX_CONVERSATIONS_PER_OWNER)
            .map(|_| conversations.create(OWNER, false).unwrap())
            .collect();
        conversations.delete(OWNER, &ids[0]).unwrap();
        assert!(conversations.create(OWNER, false).is_ok());
    }

    #[test]
    fn al_irse_el_dueno_se_liberan_solo_sus_conversaciones() {
        let mut conversations = Conversations::default();
        for _ in 0..3 {
            conversations.create(OWNER, false).unwrap();
        }
        let ajena = conversations.create(OTHER, false).unwrap();

        assert_eq!(conversations.release_owner(OWNER), 3);

        assert_eq!(conversations.len(), 1);
        assert!(conversations.ids(OWNER).is_empty());
        assert!(conversations.history(OTHER, &ajena).is_ok());
        // Y su cupo queda libre.
        assert!(conversations.create(OWNER, false).is_ok());
        // Irse dos veces, o sin haber abierto nada, no libera nada más.
        assert_eq!(conversations.release_owner(":1.99"), 0);
    }

    #[test]
    fn terminar_el_turno_de_un_dueno_que_se_fue_no_resucita_nada() {
        let (mut conversations, id) = with_turns(0);
        conversations.begin_turn(OWNER, &id, "hola").unwrap();
        conversations.release_owner(OWNER);
        conversations.finish_turn(&id, "hola", Some("respuesta"));
        assert_eq!(conversations.len(), 0);
    }
}

/// El tope de chats en vuelo por dueño (#3).
#[cfg(test)]
mod owner_limit_tests {
    use super::*;

    const A: &str = ":1.1";
    const B: &str = ":1.2";

    fn open(conversations: &mut Conversations, owner: &str, n: usize) -> Vec<String> {
        (0..n)
            .map(|_| conversations.create(owner, false).unwrap())
            .collect()
    }

    fn busy_count(conversations: &Conversations, owner: &str) -> usize {
        conversations
            .open
            .values()
            .filter(|c| c.owner == owner && c.busy)
            .count()
    }

    #[test]
    fn con_dos_chats_en_vuelo_el_tercero_da_busy_y_no_queda_marcado_en_vuelo() {
        let mut conversations = Conversations::default();
        let ids = open(&mut conversations, A, 3);
        conversations.begin_turn(A, &ids[0], "uno").unwrap();
        conversations.begin_turn(A, &ids[1], "dos").unwrap();

        let third = conversations.begin_turn(A, &ids[2], "tres");

        assert_eq!(MAX_CHATS_IN_FLIGHT_PER_OWNER, 2);
        match third {
            Err(Error::Busy(text)) => {
                assert!(text.contains('2'), "{text}");
                assert!(!text.contains("tres"), "{text}");
            }
            other => panic!("se esperaba Busy, vino {other:?}"),
        }
        // El tercero no ocupó nada: ni el cupo del dueño ni la conversación.
        assert_eq!(busy_count(&conversations, A), 2);
        assert!(!conversations.open[&ids[2]].busy);
        // Y su mensaje no entró al historial.
        assert!(conversations.history(A, &ids[2]).unwrap().is_empty());
    }

    #[test]
    fn el_busy_del_tope_no_es_limit_exceeded() {
        let mut conversations = Conversations::default();
        let ids = open(&mut conversations, A, 3);
        conversations.begin_turn(A, &ids[0], "uno").unwrap();
        conversations.begin_turn(A, &ids[1], "dos").unwrap();
        assert!(!matches!(
            conversations.begin_turn(A, &ids[2], "tres"),
            Err(Error::LimitExceeded(_))
        ));
    }

    #[test]
    fn otro_duenio_chatea_normal_con_el_primero_en_su_tope() {
        let mut conversations = Conversations::default();
        let a = open(&mut conversations, A, 3);
        let b = open(&mut conversations, B, 3);
        conversations.begin_turn(A, &a[0], "uno").unwrap();
        conversations.begin_turn(A, &a[1], "dos").unwrap();
        assert!(conversations.begin_turn(A, &a[2], "tres").is_err());

        conversations.begin_turn(B, &b[0], "uno").unwrap();
        conversations.begin_turn(B, &b[1], "dos").unwrap();

        assert_eq!(busy_count(&conversations, B), 2);
        assert!(matches!(
            conversations.begin_turn(B, &b[2], "tres"),
            Err(Error::Busy(_))
        ));
    }

    #[test]
    fn terminar_un_chat_con_respuesta_o_sin_ella_devuelve_el_lugar() {
        for reply in [Some("respuesta"), None] {
            let mut conversations = Conversations::default();
            let ids = open(&mut conversations, A, 3);
            conversations.begin_turn(A, &ids[0], "uno").unwrap();
            conversations.begin_turn(A, &ids[1], "dos").unwrap();
            assert!(conversations.begin_turn(A, &ids[2], "tres").is_err());

            conversations.finish_turn(&ids[0], "uno", reply);

            conversations.begin_turn(A, &ids[2], "tres").unwrap();
            assert_eq!(busy_count(&conversations, A), 2, "{reply:?}");
        }
    }

    #[test]
    fn irse_el_duenio_devuelve_el_lugar() {
        let mut conversations = Conversations::default();
        let ids = open(&mut conversations, A, 2);
        conversations.begin_turn(A, &ids[0], "uno").unwrap();
        conversations.begin_turn(A, &ids[1], "dos").unwrap();

        conversations.release_owner(A);

        assert_eq!(busy_count(&conversations, A), 0);
        let fresh = conversations.create(A, false).unwrap();
        conversations.begin_turn(A, &fresh, "nuevo").unwrap();
    }

    // Un cliente esquivaba el tope borrando las conversaciones que esperan
    // respuesta: el chat seguía en vuelo en el backend pero ya no contaba.
    #[test]
    fn borrar_una_conversacion_en_vuelo_no_devuelve_el_lugar() {
        let mut conversations = Conversations::default();
        let ids = open(&mut conversations, A, 3);
        conversations.begin_turn(A, &ids[0], "uno").unwrap();
        conversations.begin_turn(A, &ids[1], "dos").unwrap();

        conversations.delete(A, &ids[0]).unwrap();

        // El pedido de «uno» sigue esperando al modelo: el dueño sigue en su tope.
        assert!(matches!(
            conversations.begin_turn(A, &ids[2], "tres"),
            Err(Error::Busy(_))
        ));
    }

    #[test]
    fn la_misma_conversacion_ocupada_sigue_dando_su_busy_y_no_cuenta_dos_veces() {
        let mut conversations = Conversations::default();
        let ids = open(&mut conversations, A, 2);
        conversations.begin_turn(A, &ids[0], "uno").unwrap();

        let again = conversations.begin_turn(A, &ids[0], "otra vez");

        let Err(Error::Busy(text)) = again else {
            panic!("se esperaba Busy");
        };
        assert!(text.contains("anterior"), "{text}");
        // Sigue habiendo un lugar para otra conversación.
        conversations.begin_turn(A, &ids[1], "dos").unwrap();
    }

    #[test]
    fn un_mensaje_invalido_ni_una_conversacion_ajena_ocupan_ni_gastan_lugar() {
        let mut conversations = Conversations::default();
        let ids = open(&mut conversations, A, 2);
        let theirs = conversations.create(B, false).unwrap();

        assert!(conversations.begin_turn(A, &ids[0], "").is_err());
        assert!(conversations.begin_turn(A, &theirs, "hola").is_err());

        assert_eq!(busy_count(&conversations, A), 0);
        assert_eq!(busy_count(&conversations, B), 0);
        // Ni el mapa de chats en vuelo anotó nada...
        assert!(conversations.in_flight.is_empty());
        // ...y el dueño conserva sus dos lugares enteros.
        conversations.begin_turn(A, &ids[0], "uno").unwrap();
        conversations.begin_turn(A, &ids[1], "dos").unwrap();
        let third = conversations.create(A, false).unwrap();
        assert!(matches!(
            conversations.begin_turn(A, &third, "tres"),
            Err(Error::Busy(_))
        ));
        assert_eq!(conversations.in_flight.len(), 2);
    }

    #[test]
    fn al_irse_el_duenio_con_dos_chats_colgados_se_liberan_sus_lugares_y_no_se_hereda_nada() {
        let mut conversations = Conversations::default();
        let old = open(&mut conversations, A, 2);
        let other = open(&mut conversations, B, 1);
        conversations.begin_turn(A, &old[0], "uno").unwrap();
        conversations.begin_turn(A, &old[1], "dos").unwrap();
        conversations.begin_turn(B, &other[0], "uno").unwrap();
        assert_eq!(conversations.in_flight.len(), 3);

        // La conexión se va con sus dos chats colgados: sólo el otro dueño queda anotado.
        conversations.release_owner(A);

        assert_eq!(conversations.in_flight.len(), 1);
        assert!(conversations.in_flight.values().all(|chat| chat.owner == B));
        // Otra conexión con el mismo nombre (aunque no debería repetirse) empieza de cero.
        let fresh = open(&mut conversations, A, 3);
        conversations.begin_turn(A, &fresh[0], "uno").unwrap();
        conversations.begin_turn(A, &fresh[1], "dos").unwrap();
        assert!(matches!(
            conversations.begin_turn(A, &fresh[2], "tres"),
            Err(Error::Busy(_))
        ));
        // Los pedidos viejos terminan después (su `Drop` llama a `finish_turn`):
        // no liberan los lugares nuevos ni rompen nada.
        conversations.finish_turn(&old[0], "uno", None);
        conversations.finish_turn(&old[1], "dos", Some("tarde"));
        assert_eq!(conversations.in_flight.len(), 3);
        assert!(matches!(
            conversations.begin_turn(A, &fresh[2], "tres"),
            Err(Error::Busy(_))
        ));
        // Y el otro dueño sigue con lo suyo.
        conversations.finish_turn(&other[0], "uno", None);
        assert_eq!(conversations.in_flight.len(), 2);
    }

    #[test]
    fn el_mapa_de_chats_en_vuelo_solo_tiene_los_que_estan_en_vuelo() {
        let mut conversations = Conversations::default();
        let ids = open(&mut conversations, A, 2);
        assert!(conversations.in_flight.is_empty());

        conversations.begin_turn(A, &ids[0], "uno").unwrap();
        assert!(conversations.begin_turn(A, &ids[0], "otra vez").is_err());
        assert_eq!(conversations.in_flight.len(), 1);
        conversations.delete(A, &ids[0]).unwrap();
        // Borrada, el pedido sigue ocupando el backend: sigue anotado.
        assert_eq!(conversations.in_flight.len(), 1);
        conversations.finish_turn(&ids[0], "uno", Some("respuesta"));

        assert!(conversations.in_flight.is_empty());
        // Terminar un turno que ya no existe o nunca empezó no rompe nada.
        conversations.finish_turn("no-existe", "x", None);
        conversations.finish_turn(&ids[1], "x", None);
        assert!(conversations.in_flight.is_empty());
    }
}

/// El cupo compartido de los confinados (#32).
#[cfg(test)]
mod confined_quota_tests {
    use super::*;

    const A: &str = ":1.1";
    const B: &str = ":1.2";
    const C: &str = ":1.3";

    // El cupo de los confinados deja lugar al resto, y el de chats no puede
    // llenar el del servicio (`backend::MAX_IN_FLIGHT`).
    const _: () = {
        assert!(MAX_CONFINED_CONVERSATIONS < MAX_CONVERSATIONS);
        assert!(MAX_CONFINED_CHATS_IN_FLIGHT < crate::backend::MAX_IN_FLIGHT);
        assert!(MAX_CONFINED_CONVERSATIONS > 0 && MAX_CONFINED_CHATS_IN_FLIGHT > 0);
    };

    #[test]
    fn entre_dos_duenios_confinados_la_conversacion_de_mas_da_limit_exceeded_y_un_duenio_sin_confinar_sigue_abriendo(
    ) {
        let mut conversations = Conversations::default();
        for _ in 0..MAX_CONFINED_CONVERSATIONS / 2 {
            conversations.create(A, true).unwrap();
            conversations.create(B, true).unwrap();
        }

        // Ni un dueño nuevo confinado ni uno que ya tenía entran.
        for owner in [A, B, C] {
            assert!(
                matches!(
                    conversations.create(owner, true),
                    Err(Error::LimitExceeded(_))
                ),
                "{owner}"
            );
        }
        // Sin confinar sigue abriendo, y lo suyo no gasta el cupo compartido.
        for _ in 0..MAX_CONVERSATIONS_PER_OWNER {
            conversations.create(C, false).unwrap();
        }
        assert_eq!(
            conversations.open.values().filter(|c| c.confined).count(),
            MAX_CONFINED_CONVERSATIONS
        );
    }

    #[test]
    fn cerrar_una_conversacion_confinada_devuelve_su_lugar_al_cupo_compartido() {
        let mut conversations = Conversations::default();
        let ids: Vec<String> = (0..MAX_CONFINED_CONVERSATIONS)
            .map(|_| conversations.create(A, true).unwrap())
            .collect();
        assert!(conversations.create(B, true).is_err());

        conversations.delete(A, &ids[0]).unwrap();

        assert!(conversations.create(B, true).is_ok());
    }

    #[test]
    fn con_un_chat_confinado_en_vuelo_el_de_otro_confinado_da_busy_y_el_sin_confinar_chatea() {
        let mut conversations = Conversations::default();
        let a = conversations.create(A, true).unwrap();
        let b = conversations.create(B, true).unwrap();
        let c = conversations.create(C, false).unwrap();
        conversations.begin_turn(A, &a, "uno").unwrap();

        let busy = conversations.begin_turn(B, &b, "dos");

        assert_eq!(MAX_CONFINED_CHATS_IN_FLIGHT, 1);
        assert!(matches!(busy, Err(Error::Busy(_))), "{busy:?}");
        // El rechazado no ocupó nada: ni la conversación ni su mensaje.
        assert!(!conversations.open[&b].busy);
        assert!(conversations.history(B, &b).unwrap().is_empty());
        // Sin confinar chatea al mismo tiempo, y no gasta el cupo compartido.
        conversations.begin_turn(C, &c, "tres").unwrap();

        // Al terminar el turno, el lugar vuelve.
        conversations.finish_turn(&a, "uno", Some("ok"));
        conversations.begin_turn(B, &b, "dos").unwrap();
    }

    #[test]
    fn un_chat_sin_confinar_en_vuelo_no_ocupa_el_lugar_de_los_confinados() {
        let mut conversations = Conversations::default();
        let c = conversations.create(C, false).unwrap();
        let a = conversations.create(A, true).unwrap();
        conversations.begin_turn(C, &c, "uno").unwrap();

        assert!(conversations.begin_turn(A, &a, "dos").is_ok());
    }

    #[test]
    fn al_irse_un_confinado_se_libera_solo_lo_suyo_y_lo_del_otro_sigue_ocupando() {
        let mut conversations = Conversations::default();
        let a = conversations.create(A, true).unwrap();
        let b = conversations.create(B, true).unwrap();
        conversations.begin_turn(A, &a, "uno").unwrap();

        // Se va B: el lugar en vuelo de A sigue ocupado y su conversación abierta.
        assert_eq!(conversations.release_owner(B), 1);
        assert!(conversations.history(B, &b).is_err());
        let b2 = conversations.create(B, true).unwrap();
        assert!(matches!(
            conversations.begin_turn(B, &b2, "dos"),
            Err(Error::Busy(_))
        ));
        assert!(conversations.history(A, &a).is_ok());

        // Se va A: ahora sí queda libre el lugar.
        assert_eq!(conversations.release_owner(A), 1);
        conversations.begin_turn(B, &b2, "dos").unwrap();
    }

    #[test]
    fn borrar_una_conversacion_confinada_con_su_chat_en_vuelo_no_libera_el_lugar_hasta_terminar_el_turno(
    ) {
        let mut conversations = Conversations::default();
        let a = conversations.create(A, true).unwrap();
        let b = conversations.create(B, true).unwrap();
        conversations.begin_turn(A, &a, "uno").unwrap();
        conversations.delete(A, &a).unwrap();

        assert!(matches!(
            conversations.begin_turn(B, &b, "dos"),
            Err(Error::Busy(_))
        ));

        conversations.finish_turn(&a, "uno", Some("ok"));
        conversations.begin_turn(B, &b, "dos").unwrap();
    }
}
