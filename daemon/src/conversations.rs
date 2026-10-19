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
