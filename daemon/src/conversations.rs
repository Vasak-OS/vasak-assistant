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

use std::collections::HashMap;

use uuid::Uuid;
use vasak_assistant_protocol::{
    Error, Message, Role, MAX_CONVERSATIONS, MAX_HISTORY_BYTES, MAX_HISTORY_MESSAGES,
    MAX_MESSAGE_BYTES,
};

#[derive(Default)]
struct Conversation {
    messages: Vec<Message>,
    /// Hay un pedido al modelo en curso: un segundo `Chat` sobre la misma
    /// conversación se rechaza en vez de entreverar las respuestas.
    busy: bool,
}

#[derive(Default)]
pub struct Conversations {
    open: HashMap<String, Conversation>,
}

impl Conversations {
    pub fn len(&self) -> usize {
        self.open.len()
    }

    /// Abre una conversación y devuelve su id.
    pub fn create(&mut self) -> Result<String, Error> {
        if self.open.len() >= MAX_CONVERSATIONS {
            return Err(Error::LimitExceeded(format!(
                "ya hay {MAX_CONVERSATIONS} conversaciones abiertas; cerrá alguna"
            )));
        }
        let id = Uuid::new_v4().to_string();
        self.open.insert(id.clone(), Conversation::default());
        Ok(id)
    }

    /// Los ids abiertos, ordenados para que la respuesta no cambie de una
    /// llamada a otra.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.open.keys().cloned().collect();
        ids.sort();
        ids
    }

    pub fn history(&self, id: &str) -> Result<Vec<Message>, Error> {
        Ok(self.get(id)?.messages.clone())
    }

    pub fn delete(&mut self, id: &str) -> Result<(), Error> {
        validate_id(id)?;
        self.open.remove(id).map(drop).ok_or_else(not_found)
    }

    /// Empieza un turno: valida el mensaje, marca la conversación ocupada y
    /// devuelve lo que hay que mandarle al modelo (el historial más el mensaje
    /// nuevo). El mensaje todavía no se guarda: si el modelo falla, la
    /// conversación queda como estaba.
    pub fn begin_turn(&mut self, id: &str, content: &str) -> Result<Vec<Message>, Error> {
        validate_content(content)?;
        let conversation = self.get_mut(id)?;
        if conversation.busy {
            return Err(Error::Busy(
                "la conversación todavía está esperando la respuesta anterior".into(),
            ));
        }
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
    /// conversación se cerró mientras tanto, no hay nada que guardar.
    pub fn finish_turn(&mut self, id: &str, content: &str, reply: Option<&str>) {
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

    fn get(&self, id: &str) -> Result<&Conversation, Error> {
        validate_id(id)?;
        self.open.get(id).ok_or_else(not_found)
    }

    fn get_mut(&mut self, id: &str) -> Result<&mut Conversation, Error> {
        validate_id(id)?;
        self.open.get_mut(id).ok_or_else(not_found)
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

    fn message(role: Role, content: &str) -> Message {
        Message {
            role,
            content: content.to_owned(),
        }
    }

    /// Una conversación con `n` intercambios de mensajes cortos y distintos.
    fn with_turns(n: usize) -> (Conversations, String) {
        let mut conversations = Conversations::default();
        let id = conversations.create().unwrap();
        for i in 0..n {
            let content = format!("pregunta {i}");
            conversations.begin_turn(&id, &content).unwrap();
            conversations.finish_turn(&id, &content, Some(&format!("respuesta {i}")));
        }
        (conversations, id)
    }

    #[test]
    fn un_id_valido_es_un_uuid_con_guiones() {
        let mut conversations = Conversations::default();
        let id = conversations.create().unwrap();
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
        let mut conversations = Conversations::default();
        let ids: Vec<String> = (0..MAX_CONVERSATIONS)
            .map(|_| conversations.create().unwrap())
            .collect();
        assert_eq!(conversations.len(), MAX_CONVERSATIONS);
        assert!(matches!(
            conversations.create(),
            Err(Error::LimitExceeded(_))
        ));
        assert_eq!(conversations.len(), MAX_CONVERSATIONS);

        conversations.delete(&ids[0]).unwrap();
        assert!(conversations.create().is_ok());
    }

    #[test]
    fn los_ids_salen_ordenados_y_sin_repetirse() {
        let mut conversations = Conversations::default();
        for _ in 0..5 {
            conversations.create().unwrap();
        }
        let ids = conversations.ids();
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(ids.len(), 5);
        assert_eq!(ids, sorted);
    }

    #[test]
    fn una_conversacion_nueva_no_tiene_mensajes() {
        let mut conversations = Conversations::default();
        let id = conversations.create().unwrap();
        assert!(conversations.history(&id).unwrap().is_empty());
    }

    #[test]
    fn pedir_el_historial_de_un_id_inexistente_o_invalido_da_el_error_que_toca() {
        let (conversations, _) = with_turns(0);
        let otro = Uuid::new_v4().to_string();
        assert!(matches!(
            conversations.history(&otro),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            conversations.history("basura"),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn cerrar_una_conversacion_olvida_sus_mensajes_y_no_se_puede_cerrar_dos_veces() {
        let (mut conversations, id) = with_turns(1);
        conversations.delete(&id).unwrap();
        assert_eq!(conversations.len(), 0);
        assert!(matches!(
            conversations.history(&id),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(conversations.delete(&id), Err(Error::NotFound(_))));
        assert!(matches!(
            conversations.delete("basura"),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn el_turno_le_da_al_modelo_el_historial_mas_el_mensaje_nuevo_sin_guardarlo() {
        let (mut conversations, id) = with_turns(1);
        let context = conversations.begin_turn(&id, "hola de nuevo").unwrap();
        assert_eq!(
            context,
            vec![
                message(Role::User, "pregunta 0"),
                message(Role::Assistant, "respuesta 0"),
                message(Role::User, "hola de nuevo"),
            ]
        );
        assert_eq!(conversations.history(&id).unwrap().len(), 2);
    }

    #[test]
    fn un_turno_exitoso_guarda_el_mensaje_y_la_respuesta() {
        let (conversations, id) = with_turns(2);
        assert_eq!(
            conversations.history(&id).unwrap(),
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
        conversations.begin_turn(&id, "uno").unwrap();
        assert!(matches!(
            conversations.begin_turn(&id, "dos"),
            Err(Error::Busy(_))
        ));
    }

    #[test]
    fn el_turno_de_una_conversacion_no_ocupa_a_las_otras() {
        let mut conversations = Conversations::default();
        let a = conversations.create().unwrap();
        let b = conversations.create().unwrap();
        conversations.begin_turn(&a, "uno").unwrap();
        assert!(conversations.begin_turn(&b, "dos").is_ok());
    }

    #[test]
    fn un_turno_fallido_deja_la_conversacion_como_estaba_y_libre() {
        let (mut conversations, id) = with_turns(1);
        let antes = conversations.history(&id).unwrap();
        conversations.begin_turn(&id, "esto falla").unwrap();
        conversations.finish_turn(&id, "esto falla", None);
        assert_eq!(conversations.history(&id).unwrap(), antes);
        assert!(conversations.begin_turn(&id, "de nuevo").is_ok());
    }

    #[test]
    fn un_mensaje_invalido_no_deja_la_conversacion_ocupada() {
        let (mut conversations, id) = with_turns(0);
        assert!(matches!(
            conversations.begin_turn(&id, "  "),
            Err(Error::InvalidArgs(_))
        ));
        assert!(conversations.begin_turn(&id, "ahora sí").is_ok());
    }

    #[test]
    fn el_turno_valida_el_id_y_el_mensaje_antes_de_buscar() {
        let (mut conversations, _) = with_turns(0);
        let otro = Uuid::new_v4().to_string();
        assert!(matches!(
            conversations.begin_turn(&otro, "hola"),
            Err(Error::NotFound(_))
        ));
        assert!(matches!(
            conversations.begin_turn("basura", "hola"),
            Err(Error::InvalidArgs(_))
        ));
    }

    #[test]
    fn terminar_el_turno_de_una_conversacion_cerrada_no_la_resucita() {
        let (mut conversations, id) = with_turns(0);
        conversations.begin_turn(&id, "hola").unwrap();
        conversations.delete(&id).unwrap();
        conversations.finish_turn(&id, "hola", Some("respuesta"));
        assert_eq!(conversations.len(), 0);
    }

    #[test]
    fn el_historial_se_recorta_al_maximo_de_mensajes_y_se_van_los_mas_viejos() {
        let turnos = MAX_HISTORY_MESSAGES; // el doble de mensajes que el tope
        let (conversations, id) = with_turns(turnos);
        let history = conversations.history(&id).unwrap();
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
}
