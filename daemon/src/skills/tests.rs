//! Pruebas de la habilidad de correo y del motor de turnos con herramientas.
//!
//! Nada toca un bus ni un modelo de verdad: el modelo es un guion de turnos
//! ([`ToolBackend`]), la habilidad cuenta lo que le piden ([`FakeSkill`]) y el
//! permiso responde lo que se le dice ([`FakePerm`]). El cable hacia
//! `AccountsStore`/`AccountsSync` y `vasak-permissions` se prueba contra
//! servicios de mentira servidos en una conexión punto a punto.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::{json, Value};
use vasak_assistant_protocol::{
    Error, ModelInfo, PendingAction, Role, DOMAIN_MAIL, MAX_TOOL_ITERATIONS,
};

use super::*;
use crate::backend::{BackendError, ChatMessage, ModelBackend, ModelTurn, ToolCall};
use crate::conversations::Conversations;
use crate::memory::Memory;
use crate::service::AssistantService;
use crate::test_support::{model_info, FixedKey, FixedLabel};

const OWNER: &str = ":1.7";

// ── Mentiras ────────────────────────────────────────────────────────────────────

/// Un modelo con turnos guionados. Anota cada hilo que ve y cuántas veces lo
/// usaron en texto plano o con herramientas.
struct ToolBackend {
    turns: Mutex<VecDeque<Result<ModelTurn, BackendError>>>,
    local: bool,
    supports: bool,
    /// Si está, `chat_tools_local_only` contesta este error sin consumir turnos.
    reject_local_only: Mutex<Option<BackendError>>,
    threads: Mutex<Vec<Vec<ChatMessage>>>,
    offered_tools: Mutex<Vec<usize>>,
    plain_chats: AtomicUsize,
    tools_chats: AtomicUsize,
    local_only_chats: AtomicUsize,
}

impl ToolBackend {
    fn new(turns: Vec<Result<ModelTurn, BackendError>>) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.into()),
            local: true,
            supports: true,
            reject_local_only: Mutex::default(),
            threads: Mutex::default(),
            offered_tools: Mutex::default(),
            plain_chats: AtomicUsize::new(0),
            tools_chats: AtomicUsize::new(0),
            local_only_chats: AtomicUsize::new(0),
        })
    }

    fn remote(turns: Vec<Result<ModelTurn, BackendError>>) -> Arc<Self> {
        let mut backend = Arc::try_unwrap(Self::new(turns)).ok().unwrap();
        backend.local = false;
        Arc::new(backend)
    }

    fn without_tools() -> Arc<Self> {
        let mut backend = Arc::try_unwrap(Self::new(vec![])).ok().unwrap();
        backend.supports = false;
        Arc::new(backend)
    }
}

impl ModelBackend for ToolBackend {
    fn info(&self) -> ModelInfo {
        ModelInfo {
            local: self.local,
            ..model_info("guionado")
        }
    }

    fn models(&self) -> crate::backend::BoxFuture<'_, Result<Vec<String>, BackendError>> {
        Box::pin(async { Ok(vec!["guionado".into()]) })
    }

    fn active_model(&self) -> crate::backend::BoxFuture<'_, Result<ModelInfo, BackendError>> {
        let info = self.info();
        Box::pin(async move { Ok(info) })
    }

    fn chat<'a>(
        &'a self,
        _messages: &'a [vasak_assistant_protocol::Message],
    ) -> crate::backend::BoxFuture<'a, Result<String, BackendError>> {
        self.plain_chats.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok("texto plano".to_owned()) })
    }

    fn supports_tools(&self) -> bool {
        self.supports
    }

    fn chat_tools<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolDef],
    ) -> crate::backend::BoxFuture<'a, Result<ModelTurn, BackendError>> {
        self.tools_chats.fetch_add(1, Ordering::SeqCst);
        self.threads.lock().push(messages.to_vec());
        self.offered_tools.lock().push(tools.len());
        let turn = self
            .turns
            .lock()
            .pop_front()
            .unwrap_or(Ok(ModelTurn::Text("fin".into())));
        Box::pin(async move { turn })
    }

    fn chat_tools_local_only<'a>(
        &'a self,
        messages: &'a [ChatMessage],
        tools: &'a [ToolDef],
    ) -> crate::backend::BoxFuture<'a, Result<ModelTurn, BackendError>> {
        self.local_only_chats.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if let Some(error) = self.reject_local_only.lock().clone() {
                return Err(error);
            }
            if !self.info().local {
                return Err(BackendError::NotLocal);
            }
            self.chat_tools(messages, tools).await
        })
    }
}

/// Una habilidad de correo de mentira: cuenta lo que se ejecutó y cuántos
/// correos salieron.
struct FakeSkill {
    calls: Mutex<Vec<(String, Value)>>,
    sent: Arc<AtomicUsize>,
    send_fails: bool,
    alive: bool,
}

impl FakeSkill {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            sent: Arc::default(),
            send_fails: false,
            alive: true,
        })
    }

    fn calls_named(&self, name: &str) -> usize {
        self.calls.lock().iter().filter(|(n, _)| n == name).count()
    }
}

fn tool(name: &str) -> ToolDef {
    ToolDef {
        name: name.into(),
        description: String::new(),
        parameters: json!({"type": "object"}),
    }
}

impl Skill for FakeSkill {
    fn domain(&self) -> &'static str {
        DOMAIN_MAIL
    }

    fn permission(&self) -> &'static str {
        PERMISSION_EMAIL
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![tool("search_mail"), tool("send_mail"), tool("boom_mail")]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        args: &'a Value,
        ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        self.calls.lock().push((name.to_owned(), args.clone()));
        Box::pin(async move {
            match name {
                "send_mail" => {
                    let sent = self.sent.clone();
                    let fails = self.send_fails;
                    let action = PendingAction {
                        id: uuid::Uuid::new_v4().to_string(),
                        conversation_id: ctx.conversation_id.clone(),
                        kind: "send_mail".into(),
                        summary: "Enviar un correo".into(),
                        to: args["to"].as_str().unwrap_or_default().into(),
                        subject: args["subject"].as_str().unwrap_or_default().into(),
                        body_preview: args["body"].as_str().unwrap_or_default().into(),
                    };
                    let execute: SendAction = Box::new(move || {
                        Box::pin(async move {
                            sent.fetch_add(1, Ordering::SeqCst);
                            if fails {
                                Err(Error::Backend("no se pudo encolar".into()))
                            } else {
                                Ok(())
                            }
                        })
                    });
                    Ok(ToolOutcome::Pending { action, execute })
                }
                "boom_mail" => Err(Error::Backend("se rompió".into())),
                _ => Ok(ToolOutcome::Result(
                    "[{\"id\":7,\"asunto\":\"Hola\"}]".into(),
                )),
            }
        })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        let alive = self.alive;
        Box::pin(async move { alive })
    }
}

/// Un verificador de permisos que anota qué se le preguntó.
struct FakePerm {
    grant: bool,
    asked: Mutex<Vec<(String, String)>>,
}

impl FakePerm {
    fn new(grant: bool) -> Arc<Self> {
        Arc::new(Self {
            grant,
            asked: Mutex::default(),
        })
    }
}

impl PermissionCheck for FakePerm {
    fn check<'a>(
        &'a self,
        resource: &'a str,
        detail: &'a str,
    ) -> BoxFuture<'a, Result<bool, Error>> {
        self.asked
            .lock()
            .push((resource.to_owned(), detail.to_owned()));
        let grant = self.grant;
        Box::pin(async move { Ok(grant) })
    }
}

// ── Armado ──────────────────────────────────────────────────────────────────────

fn mail_chat(skill: Option<Arc<FakeSkill>>, perm: Arc<FakePerm>) -> Arc<MailChat> {
    Arc::new(MailChat {
        skills: SkillRegistry::new(
            skill
                .into_iter()
                .map(|skill| skill as Arc<dyn Skill>)
                .collect(),
        ),
        permission: perm,
        pending: Mutex::new(PendingActions::default()),
    })
}

fn build(backend: Arc<ToolBackend>, mail: Arc<MailChat>, share: bool) -> AssistantService {
    AssistantService {
        backend,
        conversations: Arc::new(Mutex::new(Conversations::default())),
        remote_allowed: share,
        secrets: FixedKey::new(None),
        memory: Arc::new(Memory::new(None, FixedKey::new(None)).with_share_with_remote(share)),
        labels: FixedLabel::none(),
        mail,
    }
}

struct Rig {
    service: AssistantService,
    backend: Arc<ToolBackend>,
    skill: Arc<FakeSkill>,
    perm: Arc<FakePerm>,
    conversation: String,
}

fn rig_with(backend: Arc<ToolBackend>, skill: Arc<FakeSkill>, grant: bool, share: bool) -> Rig {
    let perm = FakePerm::new(grant);
    let service = build(
        backend.clone(),
        mail_chat(Some(skill.clone()), perm.clone()),
        share,
    );
    let conversation = service.conversations.lock().create(OWNER, false).unwrap();
    Rig {
        service,
        backend,
        skill,
        perm,
        conversation,
    }
}

fn rig(turns: Vec<Result<ModelTurn, BackendError>>, grant: bool) -> Rig {
    rig_with(ToolBackend::new(turns), FakeSkill::new(), grant, false)
}

fn call(name: &str, args: Value) -> Result<ModelTurn, BackendError> {
    Ok(ModelTurn::Calls(vec![ToolCall {
        id: format!("id-{name}"),
        name: name.into(),
        arguments: args,
    }]))
}

fn text(text: &str) -> Result<ModelTurn, BackendError> {
    Ok(ModelTurn::Text(text.into()))
}

fn send_args() -> Value {
    json!({"account_id": "a1", "to": "ana@x.org", "subject": "Hola", "body": "Cuerpo"})
}

impl Rig {
    async fn turn(&self, content: &str) -> Result<vasak_assistant_protocol::ChatReply, Error> {
        self.service
            .chat_turn_with(OWNER, &self.conversation, content, true)
            .await
    }

    fn history(&self) -> Vec<vasak_assistant_protocol::Message> {
        self.service
            .conversations
            .lock()
            .history(OWNER, &self.conversation)
            .unwrap()
    }
}

// ── 1. Sin habilidades ──────────────────────────────────────────────────────────

#[tokio::test]
async fn sin_habilidades_registradas_el_turno_responde_texto_sin_acciones() {
    let backend = ToolBackend::new(vec![]);
    let service = build(backend.clone(), mail_chat(None, FakePerm::new(true)), false);
    let id = service.conversations.lock().create(OWNER, false).unwrap();
    let reply = service
        .chat_turn_with(OWNER, &id, "hola", true)
        .await
        .unwrap();
    assert_eq!(reply.text, "texto plano");
    assert!(reply.pending.is_empty());
    assert_eq!(backend.tools_chats.load(Ordering::SeqCst), 0);
    assert_eq!(backend.plain_chats.load(Ordering::SeqCst), 1);
    // El intercambio queda en el historial como en un chat normal.
    let history = service.conversations.lock().history(OWNER, &id).unwrap();
    assert_eq!(history.len(), 2);
}

#[tokio::test]
async fn un_llamador_confinado_chatea_sin_herramientas_aunque_haya_habilidades() {
    let r = rig(vec![call("search_mail", json!({}))], true);
    let reply = r
        .service
        .chat_turn_with(OWNER, &r.conversation, "hola", false)
        .await
        .unwrap();
    assert_eq!(reply.text, "texto plano");
    assert!(reply.pending.is_empty());
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 0);
    assert!(r.skill.calls.lock().is_empty());
    assert!(r.perm.asked.lock().is_empty());
}

#[tokio::test]
async fn un_modelo_sin_soporte_de_herramientas_chatea_en_texto() {
    let r = rig_with(ToolBackend::without_tools(), FakeSkill::new(), true, false);
    let reply = r.turn("hola").await.unwrap();
    assert_eq!(reply.text, "texto plano");
    assert!(reply.pending.is_empty());
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 0);
}

// ── 2. Lectura con permiso ──────────────────────────────────────────────────────

#[tokio::test]
async fn una_herramienta_de_lectura_corre_con_permiso_y_su_resultado_vuelve_al_modelo() {
    let r = rig(
        vec![
            call(
                "search_mail",
                json!({"account_id": "a1", "query": "factura"}),
            ),
            text("Encontré un correo: Hola"),
        ],
        true,
    );
    let reply = r.turn("¿qué correos hay?").await.unwrap();
    assert_eq!(reply.text, "Encontré un correo: Hola");
    assert!(reply.pending.is_empty());

    // La herramienta se ejecutó una vez con los argumentos del modelo.
    assert_eq!(r.skill.calls_named("search_mail"), 1);
    assert_eq!(r.skill.calls.lock()[0].1["query"], "factura");
    // El permiso se preguntó por el recurso de correo, con la cuenta como detalle.
    assert_eq!(
        *r.perm.asked.lock(),
        vec![(PERMISSION_EMAIL.to_owned(), "a1".to_owned())]
    );
    // El modelo vio el resultado como mensaje de herramienta en el segundo paso.
    let threads = r.backend.threads.lock();
    assert_eq!(threads.len(), 2);
    assert!(matches!(&threads[0][0], ChatMessage::System(_)));
    assert!(threads[1].iter().any(|m| matches!(
        m,
        ChatMessage::Tool { id, content } if id == "id-search_mail" && content.contains("Hola")
    )));
    // Se le ofrecieron las tres herramientas.
    assert_eq!(r.backend.offered_tools.lock()[0], 3);
}

#[tokio::test]
async fn los_mensajes_de_herramienta_no_entran_al_historial_visible() {
    let r = rig(
        vec![
            call("search_mail", json!({"account_id": "a1"})),
            text("listo"),
        ],
        true,
    );
    r.turn("buscá").await.unwrap();
    let history = r.history();
    assert_eq!(
        history.len(),
        2,
        "sólo la pregunta y la respuesta: {history:?}"
    );
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[1].content, "listo");
    assert!(history.iter().all(|m| !m.content.contains("asunto")));
}

#[tokio::test]
async fn con_el_permiso_negado_la_herramienta_no_se_ejecuta_y_el_modelo_lo_sabe() {
    let r = rig(
        vec![
            call("search_mail", json!({"account_id": "a1", "query": "x"})),
            text("No tengo permiso"),
        ],
        false,
    );
    let reply = r.turn("buscá").await.unwrap();
    assert_eq!(reply.text, "No tengo permiso");
    assert!(r.skill.calls.lock().is_empty(), "no debió ejecutarse nada");
    let threads = r.backend.threads.lock();
    assert!(threads[1].iter().any(|m| matches!(
        m,
        ChatMessage::Tool { content, .. } if content.contains("no concedió el permiso")
    )));
}

#[tokio::test]
async fn con_el_permiso_negado_un_envio_tampoco_deja_una_accion_pendiente() {
    let r = rig(vec![call("send_mail", send_args()), text("no")], false);
    let reply = r.turn("mandá").await.unwrap();
    assert!(reply.pending.is_empty());
    assert!(r.service.mail.pending.lock().by_id.is_empty());
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn una_herramienta_inexistente_vuelve_al_modelo_como_resultado_y_no_rompe_el_turno() {
    let r = rig(vec![call("borrar_todo", json!({})), text("ok")], true);
    let reply = r.turn("hacé algo").await.unwrap();
    assert_eq!(reply.text, "ok");
    assert!(r.skill.calls.lock().is_empty());
    assert!(r.perm.asked.lock().is_empty(), "ni se pregunta el permiso");
    let threads = r.backend.threads.lock();
    assert!(threads[1].iter().any(|m| matches!(
        m,
        ChatMessage::Tool { content, .. } if content.contains("no existe una herramienta")
    )));
}

#[tokio::test]
async fn si_la_herramienta_falla_el_error_vuelve_al_modelo_y_el_turno_sigue() {
    let r = rig(
        vec![call("boom_mail", json!({})), text("hubo un error")],
        true,
    );
    let reply = r.turn("probá").await.unwrap();
    assert_eq!(reply.text, "hubo un error");
    let threads = r.backend.threads.lock();
    assert!(threads[1].iter().any(|m| matches!(
        m,
        ChatMessage::Tool { content, .. } if content.contains("no se pudo ejecutar") && content.contains("se rompió")
    )));
}

#[tokio::test]
async fn un_error_del_modelo_corta_el_turno_y_deja_la_conversacion_libre() {
    let r = rig(
        vec![
            Err(BackendError::Failed("se cayó".into())),
            text("ya volvió"),
        ],
        true,
    );
    assert!(r.turn("hola").await.is_err());
    assert!(r.history().is_empty(), "un turno fallido no se guarda");
    // Quedó libre: el siguiente turno no da Busy.
    assert_eq!(r.turn("hola").await.unwrap().text, "ya volvió");
}

// ── Tope de pasos ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn si_el_modelo_pide_herramientas_sin_parar_el_turno_se_corta_con_una_respuesta_segura() {
    let turns = (0..MAX_TOOL_ITERATIONS + 3)
        .map(|_| call("search_mail", json!({"account_id": "a1"})))
        .collect();
    let r = rig(turns, true);
    let reply = r.turn("buscá").await.unwrap();
    assert!(reply.pending.is_empty());
    assert!(reply.text.contains("No pude completar"), "{}", reply.text);
    assert_eq!(
        r.backend.tools_chats.load(Ordering::SeqCst),
        MAX_TOOL_ITERATIONS
    );
    assert_eq!(r.skill.calls_named("search_mail"), MAX_TOOL_ITERATIONS);
}

// ── 3. send_mail devuelve una acción pendiente ──────────────────────────────────

#[tokio::test]
async fn send_mail_no_envia_devuelve_una_accion_pendiente_con_el_resumen() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let reply = r.turn("mandale un correo a Ana").await.unwrap();
    assert_eq!(reply.pending.len(), 1);
    let action = &reply.pending[0];
    assert_eq!(action.kind, "send_mail");
    assert_eq!(action.to, "ana@x.org");
    assert_eq!(action.subject, "Hola");
    assert_eq!(action.body_preview, "Cuerpo");
    assert_eq!(action.conversation_id, r.conversation);
    assert_eq!(reply.text, action.summary);
    // No salió nada, y el modelo no volvió a ser llamado: decide la persona.
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 1);
    assert_eq!(r.service.mail.pending.lock().by_id.len(), 1);
}

#[tokio::test]
async fn confirmar_la_accion_envia_recien_ahi() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
    let result = r.service.confirm_pending(OWNER, &id).await.unwrap();
    assert_eq!(result, "Enviado");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

// ── 4. Anti-doble-envío ─────────────────────────────────────────────────────────

#[tokio::test]
async fn una_segunda_confirmacion_da_not_found_y_no_reenvia() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    r.service.confirm_pending(OWNER, &id).await.unwrap();
    let second = r.service.confirm_pending(OWNER, &id).await;
    assert!(matches!(second, Err(Error::NotFound(_))), "{second:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dos_confirmaciones_a_la_vez_envian_una_sola_vez() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    let (a, b) = tokio::join!(
        r.service.confirm_pending(OWNER, &id),
        r.service.confirm_pending(OWNER, &id)
    );
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1, "{a:?} {b:?}");
    let loser = if a.is_err() { a } else { b };
    assert!(matches!(loser, Err(Error::NotFound(_))), "{loser:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn si_el_envio_falla_el_error_sale_y_la_accion_no_se_puede_reintentar() {
    let mut skill = Arc::try_unwrap(FakeSkill::new()).ok().unwrap();
    skill.send_fails = true;
    let r = rig_with(
        ToolBackend::new(vec![call("send_mail", send_args())]),
        Arc::new(skill),
        true,
        false,
    );
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    let first = r.service.confirm_pending(OWNER, &id).await;
    assert!(matches!(first, Err(Error::Backend(_))), "{first:?}");
    let retry = r.service.confirm_pending(OWNER, &id).await;
    assert!(matches!(retry, Err(Error::NotFound(_))), "{retry:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

// ── 5. Dueño y vencimiento ──────────────────────────────────────────────────────

#[tokio::test]
async fn otra_conexion_no_puede_confirmar_ni_cancelar_una_accion_ajena() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    let confirm = r.service.confirm_pending(":1.99", &id).await;
    assert!(matches!(confirm, Err(Error::NotFound(_))), "{confirm:?}");
    let cancel = r.service.cancel_pending(":1.99", &id);
    assert!(matches!(cancel, Err(Error::NotFound(_))), "{cancel:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
    // El intento ajeno no la consumió: su dueño todavía puede confirmarla.
    r.service.confirm_pending(OWNER, &id).await.unwrap();
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn un_id_inventado_da_not_found() {
    let r = rig(vec![], true);
    let confirm = r.service.confirm_pending(OWNER, "no-existe").await;
    assert!(matches!(confirm, Err(Error::NotFound(_))));
    assert!(matches!(
        r.service.cancel_pending(OWNER, "no-existe"),
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn una_accion_vencida_no_se_confirma_ni_se_envia() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    age(&r.service.mail, &id, PENDING_TTL + Duration::from_secs(1));
    let confirm = r.service.confirm_pending(OWNER, &id).await;
    assert!(matches!(confirm, Err(Error::NotFound(_))), "{confirm:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn una_accion_justo_dentro_del_vencimiento_todavia_se_confirma() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    age(&r.service.mail, &id, PENDING_TTL - Duration::from_secs(5));
    r.service.confirm_pending(OWNER, &id).await.unwrap();
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

/// Envejece una acción pendiente, como si hubiera pasado `by` desde que se creó.
fn age(mail: &MailChat, id: &str, by: Duration) {
    let mut pending = mail.pending.lock();
    let entry = pending.by_id.get_mut(id).expect("la acción existe");
    entry.created = Instant::now().checked_sub(by).unwrap();
}

// ── 6. Cancelar ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancelar_descarta_la_accion_sin_enviar() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    r.service.cancel_pending(OWNER, &id).unwrap();
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
    assert!(r.service.mail.pending.lock().by_id.is_empty());
    let confirm = r.service.confirm_pending(OWNER, &id).await;
    assert!(matches!(confirm, Err(Error::NotFound(_))), "{confirm:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
}

// ── 7. Una acción pendiente por conversación ────────────────────────────────────

#[tokio::test]
async fn un_turno_nuevo_reemplaza_la_accion_pendiente_de_la_conversacion() {
    let r = rig(
        vec![
            call("send_mail", send_args()),
            call(
                "send_mail",
                json!({"account_id": "a1", "to": "bea@x.org", "subject": "Otro", "body": "B"}),
            ),
        ],
        true,
    );
    let first = r.turn("mandá uno").await.unwrap().pending[0].id.clone();
    let second = r.turn("mejor a Bea").await.unwrap().pending[0].id.clone();
    assert_ne!(first, second);
    assert_eq!(r.service.mail.pending.lock().by_id.len(), 1);
    let old = r.service.confirm_pending(OWNER, &first).await;
    assert!(matches!(old, Err(Error::NotFound(_))), "{old:?}");
    r.service.confirm_pending(OWNER, &second).await.unwrap();
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn las_acciones_de_conversaciones_distintas_conviven() {
    let r = rig(
        vec![
            call("send_mail", send_args()),
            call("send_mail", send_args()),
        ],
        true,
    );
    let other = r.service.conversations.lock().create(OWNER, false).unwrap();
    let a = r.turn("uno").await.unwrap().pending[0].id.clone();
    let b = r
        .service
        .chat_turn_with(OWNER, &other, "dos", true)
        .await
        .unwrap()
        .pending[0]
        .id
        .clone();
    assert_eq!(r.service.mail.pending.lock().by_id.len(), 2);
    r.service.confirm_pending(OWNER, &a).await.unwrap();
    r.service.confirm_pending(OWNER, &b).await.unwrap();
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 2);
}

// ── 8. Sólo local ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn con_un_modelo_remoto_y_sin_compartir_no_se_ofrecen_herramientas_ni_se_toca_el_correo() {
    let r = rig_with(
        ToolBackend::remote(vec![call("search_mail", json!({"account_id": "a1"}))]),
        FakeSkill::new(),
        true,
        false,
    );
    let reply = r.turn("¿qué correos tengo?").await.unwrap();
    assert_eq!(reply.text, "texto plano");
    assert!(reply.pending.is_empty());
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 0);
    assert!(r.skill.calls.lock().is_empty());
    assert!(r.perm.asked.lock().is_empty());
}

#[tokio::test]
async fn si_el_modelo_pasa_a_remoto_en_el_medio_la_respuesta_es_segura_y_no_sale_nada() {
    for error in [BackendError::NotLocal, BackendError::RemoteAfterSend] {
        let backend = ToolBackend::new(vec![call("search_mail", json!({"account_id": "a1"}))]);
        *backend.reject_local_only.lock() = Some(error);
        let r = rig_with(backend, FakeSkill::new(), true, false);
        let reply = r.turn("¿qué correos tengo?").await.unwrap();
        assert!(reply.pending.is_empty());
        assert!(
            reply.text.contains("No pude usar el correo"),
            "{}",
            reply.text
        );
        assert!(r.skill.calls.lock().is_empty());
        assert_eq!(r.backend.local_only_chats.load(Ordering::SeqCst), 1);
        assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn sin_compartir_el_turno_usa_la_variante_solo_local_del_modelo() {
    let r = rig(vec![text("hola")], true);
    r.turn("hola").await.unwrap();
    assert_eq!(r.backend.local_only_chats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compartiendo_con_remoto_el_modelo_remoto_si_ve_las_herramientas() {
    let r = rig_with(
        ToolBackend::remote(vec![
            call("search_mail", json!({"account_id": "a1"})),
            text("listo"),
        ]),
        FakeSkill::new(),
        true,
        true,
    );
    let reply = r.turn("buscá").await.unwrap();
    assert_eq!(reply.text, "listo");
    assert_eq!(r.backend.local_only_chats.load(Ordering::SeqCst), 0);
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 2);
    assert_eq!(r.skill.calls_named("search_mail"), 1);
}

// ── 10. Capacidad DOMAIN_MAIL ───────────────────────────────────────────────────

async fn mail_capability(
    backend: Arc<ToolBackend>,
    skill: Option<Arc<FakeSkill>>,
    share: bool,
    confined: bool,
) -> vasak_assistant_protocol::Capability {
    let service = build(backend, mail_chat(skill, FakePerm::new(true)), share);
    service.mail_capability(confined).await
}

#[tokio::test]
async fn la_capacidad_de_correo_esta_disponible_con_habilidad_almacen_y_modelo_con_herramientas() {
    let cap = mail_capability(
        ToolBackend::new(vec![]),
        Some(FakeSkill::new()),
        false,
        false,
    )
    .await;
    assert_eq!(cap.domain, DOMAIN_MAIL);
    assert!(cap.available, "{}", cap.reason);
    assert!(cap.reason.is_empty());
    // Un modelo remoto también sirve si la persona permite compartir.
    let shared = mail_capability(
        ToolBackend::remote(vec![]),
        Some(FakeSkill::new()),
        true,
        false,
    )
    .await;
    assert!(shared.available, "{}", shared.reason);
}

#[tokio::test]
async fn la_capacidad_de_correo_no_esta_disponible_con_motivo_en_cada_caso_que_falta() {
    let mut dead = Arc::try_unwrap(FakeSkill::new()).ok().unwrap();
    dead.alive = false;
    let cases = [
        (
            "confinado",
            mail_capability(
                ToolBackend::new(vec![]),
                Some(FakeSkill::new()),
                false,
                true,
            )
            .await,
        ),
        (
            "sin habilidad",
            mail_capability(ToolBackend::new(vec![]), None, false, false).await,
        ),
        (
            "sin herramientas",
            mail_capability(
                ToolBackend::without_tools(),
                Some(FakeSkill::new()),
                false,
                false,
            )
            .await,
        ),
        (
            "remoto sin compartir",
            mail_capability(
                ToolBackend::remote(vec![]),
                Some(FakeSkill::new()),
                false,
                false,
            )
            .await,
        ),
        (
            "almacén caído",
            mail_capability(ToolBackend::new(vec![]), Some(Arc::new(dead)), false, false).await,
        ),
    ];
    let mut reasons = std::collections::HashSet::new();
    for (name, cap) in cases {
        assert_eq!(cap.domain, DOMAIN_MAIL, "{name}");
        assert!(!cap.available, "{name}");
        assert!(!cap.reason.is_empty(), "{name}");
        reasons.insert(cap.reason);
    }
    assert_eq!(reasons.len(), 5, "cada caso explica su propio motivo");
}

// ── Registro de acciones pendientes ─────────────────────────────────────────────

fn action(id: &str) -> PendingAction {
    PendingAction {
        id: id.into(),
        conversation_id: "c".into(),
        kind: "send_mail".into(),
        summary: String::new(),
        to: String::new(),
        subject: String::new(),
        body_preview: String::new(),
    }
}

fn noop() -> SendAction {
    Box::new(|| Box::pin(async { Ok(()) }))
}

#[test]
fn el_registro_reemplaza_por_conversacion_del_mismo_dueno_y_no_toca_las_ajenas() {
    let mut pending = PendingActions::default();
    pending.insert("a", "c1", action("1"), noop());
    pending.insert("a", "c1", action("2"), noop());
    pending.insert("b", "c1", action("3"), noop());
    pending.insert("a", "c2", action("4"), noop());
    assert!(pending.take("1", "a").is_err(), "la primera se reemplazó");
    assert!(pending.take("2", "a").is_ok());
    assert!(
        pending.take("3", "b").is_ok(),
        "otro dueño, misma conversación"
    );
    assert!(pending.take("4", "a").is_ok());
}

#[test]
fn el_registro_descarta_las_vencidas_al_insertar() {
    let mut pending = PendingActions::default();
    pending.insert("a", "c1", action("viejo"), noop());
    pending.by_id.get_mut("viejo").unwrap().created = Instant::now()
        .checked_sub(PENDING_TTL + Duration::from_secs(1))
        .unwrap();
    pending.insert("a", "c2", action("nuevo"), noop());
    assert!(!pending.by_id.contains_key("viejo"));
    assert!(pending.by_id.contains_key("nuevo"));
}

#[test]
fn take_de_otro_dueno_no_la_saca_del_registro() {
    let mut pending = PendingActions::default();
    pending.insert("a", "c", action("1"), noop());
    assert!(pending.take("1", "b").is_err());
    let entry = pending.take("1", "a").unwrap();
    assert_eq!(entry.owner, "a");
    assert_eq!(entry.conversation_id, "c");
}

#[tokio::test]
async fn sin_permiso_de_mentira_el_verificador_por_omision_niega() {
    assert!(!DenyPermission.check("store.email", "").await.unwrap());
    let default = MailChat::default();
    assert!(default.skills.is_empty());
    assert!(!default.permission.check("store.email", "").await.unwrap());
}

#[test]
fn el_registro_de_habilidades_encuentra_por_herramienta_y_por_dominio() {
    let registry = SkillRegistry::new(vec![FakeSkill::new()]);
    assert!(!registry.is_empty());
    assert_eq!(registry.tools().len(), 3);
    assert!(registry.find("send_mail").is_some());
    assert!(registry.find("nada").is_none());
    assert!(registry.for_domain(DOMAIN_MAIL).is_some());
    assert!(registry.for_domain("calendar").is_none());
}

// ── Por el bus (punto a punto): ChatTurn, ConfirmAction, CancelAction ───────────

mod por_el_bus {
    use super::*;
    use vasak_assistant_protocol::{AssistantProxy, SERVICE_PATH};

    async fn connected(r: Rig) -> (AssistantProxy<'static>, zbus::Connection, Arc<FakeSkill>) {
        let skill = r.skill.clone();
        let service = r.service;
        let (server, client) = crate::test_support::p2p_pair_as(OWNER, |builder| {
            builder.serve_at(SERVICE_PATH, service)
        })
        .await
        .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        (proxy, server, skill)
    }

    #[tokio::test]
    async fn chat_turn_confirm_y_cancel_funcionan_de_punta_a_punta_por_el_bus() {
        let r = rig(
            vec![
                call("send_mail", send_args()),
                call("send_mail", send_args()),
                text("hola"),
            ],
            true,
        );
        let (proxy, _server, skill) = connected(r).await;

        // Confirmar: el envío recién sale acá.
        let id = proxy.new_conversation().await.unwrap();
        let reply = proxy.chat_turn(&id, "mandá").await.unwrap();
        assert_eq!(reply.pending.len(), 1);
        assert_eq!(skill.sent.load(Ordering::SeqCst), 0);
        proxy.confirm_action(&reply.pending[0].id).await.unwrap();
        assert_eq!(skill.sent.load(Ordering::SeqCst), 1);
        assert!(proxy.confirm_action(&reply.pending[0].id).await.is_err());

        // Cancelar: no sale nada.
        let reply = proxy.chat_turn(&id, "mandá otro").await.unwrap();
        proxy.cancel_action(&reply.pending[0].id).await.unwrap();
        assert_eq!(skill.sent.load(Ordering::SeqCst), 1);

        // Un turno de texto sin acción.
        let reply = proxy.chat_turn(&id, "hola").await.unwrap();
        assert_eq!((reply.text.as_str(), reply.pending.len()), ("hola", 0));
    }

    #[tokio::test]
    async fn un_llamador_confinado_por_el_bus_chatea_sin_habilidades() {
        let mut r = rig(vec![call("send_mail", send_args())], true);
        r.service.labels = FixedLabel::of(b"vasak-appimage");
        let (proxy, _server, skill) = connected(r).await;
        let id = proxy.new_conversation().await.unwrap();
        let reply = proxy.chat_turn(&id, "mandá").await.unwrap();
        assert!(reply.pending.is_empty());
        assert_eq!(reply.text, "texto plano");
        assert!(skill.calls.lock().is_empty());
        let capabilities = proxy.capabilities().await.unwrap();
        let mail = capabilities
            .iter()
            .find(|c| c.domain == DOMAIN_MAIL)
            .unwrap();
        assert!(!mail.available);
    }

    #[tokio::test]
    async fn las_capacidades_por_el_bus_informan_el_correo_disponible() {
        let r = rig(vec![], true);
        let (proxy, _server, _skill) = connected(r).await;
        let capabilities = proxy.capabilities().await.unwrap();
        let mail = capabilities
            .iter()
            .find(|c| c.domain == DOMAIN_MAIL)
            .unwrap();
        assert!(mail.available, "{}", mail.reason);
    }
}
