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
    /// Si está, lo que contesta `active_supports_tools` (el modelo efectivo);
    /// si no, lo que dice `supports_tools`.
    active_tools: Mutex<Option<Result<bool, BackendError>>>,
    /// Si está, `active_model` falla con este error (modelo inalcanzable).
    active_model_error: Mutex<Option<BackendError>>,
    threads: Mutex<Vec<Vec<ChatMessage>>>,
    offered_tools: Mutex<Vec<usize>>,
    plain_chats: AtomicUsize,
    active_model_calls: AtomicUsize,
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
            active_tools: Mutex::default(),
            active_model_error: Mutex::default(),
            threads: Mutex::default(),
            offered_tools: Mutex::default(),
            plain_chats: AtomicUsize::new(0),
            active_model_calls: AtomicUsize::new(0),
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
        self.active_model_calls.fetch_add(1, Ordering::SeqCst);
        let info = self.info();
        let error = self.active_model_error.lock().clone();
        Box::pin(async move { error.map_or(Ok(info), Err) })
    }

    fn active_supports_tools(&self) -> crate::backend::BoxFuture<'_, Result<bool, BackendError>> {
        let answer = self
            .active_tools
            .lock()
            .clone()
            .unwrap_or(Ok(self.supports));
        Box::pin(async move { answer })
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
    /// Lo que devuelven las lecturas; por omisión un correo chico.
    read_result: String,
    /// Si está, el envío avisa que empezó y espera a que lo suelten.
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
    /// Se corre al armar el draft de `send_mail`, antes de devolverlo.
    on_send_mail: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

impl FakeSkill {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            sent: Arc::default(),
            send_fails: false,
            alive: true,
            read_result: "[{\"id\":7,\"asunto\":\"Hola\"}]".into(),
            gate: None,
            on_send_mail: Mutex::default(),
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
                    if let Some(hook) = self.on_send_mail.lock().as_ref() {
                        hook();
                    }
                    let sent = self.sent.clone();
                    let fails = self.send_fails;
                    let gate = self.gate.clone();
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
                            if let Some((started, release)) = gate {
                                started.notify_one();
                                release.notified().await;
                            }
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
                _ => Ok(ToolOutcome::Result(self.read_result.clone())),
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

fn mail_chat(skill: Option<Arc<FakeSkill>>, perm: Arc<FakePerm>, share: bool) -> Arc<MailChat> {
    Arc::new(MailChat {
        skills: SkillRegistry::new(
            skill
                .into_iter()
                .map(|skill| skill as Arc<dyn Skill>)
                .collect(),
        ),
        permission: perm,
        pending: Mutex::new(PendingActions::default()),
        share_mail_with_remote: share,
        share_calendar_with_remote: share,
        share_contacts_with_remote: share,
        share_files_with_remote: false,
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
        mail_chat(Some(skill.clone()), perm.clone(), share),
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
    let service = build(
        backend.clone(),
        mail_chat(None, FakePerm::new(true), false),
        false,
    );
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
async fn un_turno_con_datos_de_correo_devuelve_el_texto_pero_no_lo_deja_en_el_historial() {
    let r = rig(
        vec![
            call("search_mail", json!({"account_id": "a1"})),
            text("Tenés un correo: Hola"),
        ],
        true,
    );
    // Quien llama recibe la respuesta...
    assert_eq!(r.turn("buscá").await.unwrap().text, "Tenés un correo: Hola");
    // ...pero ni la pregunta ni la respuesta (que puede citar el correo) quedan
    // en el historial reutilizable.
    assert!(r.history().is_empty(), "{:?}", r.history());
}

#[tokio::test]
async fn un_turno_sin_datos_de_correo_conserva_su_historial_normal() {
    // Sin llamadas a herramienta.
    let r = rig(vec![text("hola, ¿qué tal?")], true);
    r.turn("hola").await.unwrap();
    let history = r.history();
    assert_eq!(history.len(), 2, "{history:?}");
    assert_eq!(history[0].role, Role::User);
    assert_eq!(history[1].role, Role::Assistant);
    assert_eq!(history[1].content, "hola, ¿qué tal?");

    // Con una herramienta que no trajo datos (permiso negado): tampoco hay
    // correo que proteger.
    let denied = rig(
        vec![
            call("search_mail", json!({"account_id": "a1"})),
            text("No tengo permiso"),
        ],
        false,
    );
    denied.turn("buscá").await.unwrap();
    assert_eq!(denied.history().len(), 2);
    assert!(denied
        .history()
        .iter()
        .all(|m| !m.content.contains("asunto")));
}

#[tokio::test]
async fn una_accion_pendiente_tras_leer_correo_no_deja_su_resumen_en_el_historial() {
    let r = rig(
        vec![
            call("search_mail", json!({"account_id": "a1"})),
            call("send_mail", send_args()),
        ],
        true,
    );
    let reply = r.turn("respondele a Ana").await.unwrap();
    assert_eq!(reply.pending.len(), 1);
    // El resumen puede venir de datos del correo leído: no se reenvía en un
    // turno posterior.
    assert!(
        r.history().iter().all(|m| !m.content.contains(&reply.text)),
        "{:?}",
        r.history()
    );
}

#[tokio::test]
async fn una_accion_pendiente_sin_datos_de_usuario_conserva_su_historial() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let reply = r.turn("mandale un correo a Ana").await.unwrap();
    assert_eq!(reply.pending.len(), 1);
    let history = r.history();
    assert_eq!(history.len(), 2, "{history:?}");
    assert_eq!(history[1].content, reply.text);
}

#[tokio::test]
async fn un_chat_en_texto_plano_no_espera_al_modelo_activo() {
    // Sin habilidades, confinado y sin soporte de herramientas: ninguno
    // resuelve `active_model`.
    let sin_habilidades = ToolBackend::new(vec![]);
    let service = build(
        sin_habilidades.clone(),
        mail_chat(None, FakePerm::new(true), false),
        false,
    );
    let id = service.conversations.lock().create(OWNER, false).unwrap();
    service
        .chat_turn_with(OWNER, &id, "hola", true)
        .await
        .unwrap();
    assert_eq!(sin_habilidades.active_model_calls.load(Ordering::SeqCst), 0);

    let r = rig(vec![], true);
    r.service
        .chat_turn_with(OWNER, &r.conversation, "hola", false)
        .await
        .unwrap();
    assert_eq!(r.backend.active_model_calls.load(Ordering::SeqCst), 0);

    let r = rig_with(ToolBackend::without_tools(), FakeSkill::new(), true, false);
    r.turn("hola").await.unwrap();
    assert_eq!(r.backend.active_model_calls.load(Ordering::SeqCst), 0);
    assert_eq!(r.backend.plain_chats.load(Ordering::SeqCst), 1);
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
    // `SendMessage` acepta el draft en la cola; no confirma la entrega.
    assert_eq!(result, "Puesto en cola para enviar");
    assert_ne!(result, "Enviado");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn confirmar_un_movimiento_informa_que_el_archivo_se_movio() {
    let r = rig(vec![], true);
    let moved = Arc::new(AtomicUsize::new(0));
    let counter = moved.clone();
    let action = PendingAction {
        id: "mover-1".into(),
        conversation_id: r.conversation.clone(),
        kind: "move_file".into(),
        summary: "Mover «a» a «b»".into(),
        to: String::new(),
        subject: String::new(),
        body_preview: String::new(),
    };
    let execute: SendAction = Box::new(move || {
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    });
    r.service
        .mail
        .pending
        .lock()
        .insert(OWNER, &r.conversation, action, execute);
    let result = r.service.confirm_pending(OWNER, "mover-1").await.unwrap();
    assert_eq!(result, "Archivo movido");
    assert_eq!(moved.load(Ordering::SeqCst), 1);
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
async fn la_segunda_confirmacion_da_not_found_mientras_el_primer_envio_sigue_en_curso() {
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut skill = Arc::try_unwrap(FakeSkill::new()).ok().unwrap();
    skill.gate = Some((started.clone(), release.clone()));
    let r = rig_with(
        ToolBackend::new(vec![call("send_mail", send_args())]),
        Arc::new(skill),
        true,
        false,
    );
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();

    let first = r.service.confirm_pending(OWNER, &id);
    tokio::pin!(first);
    // El primer envío arranca y queda esperando en la compuerta.
    tokio::select! {
        done = &mut first => panic!("el envío no debía terminar todavía: {done:?}"),
        () = started.notified() => {}
    }
    // Con el primero a medio camino, la acción ya no está: no hay segundo envío.
    let second = r.service.confirm_pending(OWNER, &id).await;
    assert!(matches!(second, Err(Error::NotFound(_))), "{second:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);

    release.notify_one();
    assert_eq!(first.await.unwrap(), "Puesto en cola para enviar");
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
        assert!(reply.text.contains("No pude"), "{}", reply.text);
        assert!(r.skill.calls.lock().is_empty());
        assert_eq!(r.backend.local_only_chats.load(Ordering::SeqCst), 1);
        assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn el_cambio_a_remoto_antes_de_mandar_dice_que_nada_salio_y_despues_de_mandar_no() {
    let mut texts = Vec::new();
    for error in [BackendError::NotLocal, BackendError::RemoteAfterSend] {
        let backend = ToolBackend::new(vec![call("search_mail", json!({"account_id": "a1"}))]);
        *backend.reject_local_only.lock() = Some(error);
        let r = rig_with(backend, FakeSkill::new(), true, false);
        let reply = r.turn("¿qué correos tengo?").await.unwrap();
        assert!(reply.pending.is_empty());
        texts.push(reply.text);
    }
    let (before, after) = (&texts[0], &texts[1]);
    assert_ne!(before, after, "los dos casos no pueden decir lo mismo");
    // Antes de mandar: nada salió, y lo dice.
    assert!(before.contains("no se comparten"), "{before}");
    // Después de mandar: pudo haber salido; no afirma lo contrario.
    assert!(after.contains("pudo haber salido"), "{after}");
    // El aviso cubre también el calendario, no sólo el correo.
    assert!(after.contains("calendario"), "{after}");
    assert!(before.contains("tus datos"), "{before}");
    assert!(!after.contains("no se comparten"), "{after}");
}

#[tokio::test]
async fn un_cambio_a_remoto_tampoco_deja_el_texto_seguro_en_el_historial_de_un_turno_de_correo() {
    let backend = ToolBackend::new(vec![call("search_mail", json!({"account_id": "a1"}))]);
    *backend.reject_local_only.lock() = Some(BackendError::RemoteAfterSend);
    let r = rig_with(backend, FakeSkill::new(), true, false);
    let reply = r.turn("hola").await.unwrap();
    // La respuesta segura se guarda como cualquier turno sin datos de correo.
    assert_eq!(r.history().last().unwrap().content, reply.text);
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

// ── Modelo efectivo sin herramientas (Ollama /api/show) ─────────────────────────

#[tokio::test]
async fn un_modelo_efectivo_sin_herramientas_chatea_en_texto_aunque_el_backend_las_hable() {
    let backend = ToolBackend::new(vec![call("search_mail", json!({"account_id": "a1"}))]);
    assert!(backend.supports_tools(), "el backend sí habla herramientas");
    *backend.active_tools.lock() = Some(Ok(false));
    let r = rig_with(backend, FakeSkill::new(), true, false);
    let reply = r.turn("hola").await.unwrap();
    assert_eq!(reply.text, "texto plano");
    assert!(reply.pending.is_empty());
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 0);
    assert_eq!(r.backend.plain_chats.load(Ordering::SeqCst), 1);
    assert!(r.skill.calls.lock().is_empty());
    // Es un chat normal: el intercambio queda en el historial.
    assert_eq!(r.history().len(), 2);
}

#[tokio::test]
async fn si_no_se_pudo_saber_si_el_modelo_admite_herramientas_cae_en_chat_plano() {
    let backend = ToolBackend::new(vec![]);
    *backend.active_tools.lock() = Some(Err(BackendError::Failed("no responde".into())));
    let r = rig_with(backend, FakeSkill::new(), true, false);
    assert_eq!(r.turn("hola").await.unwrap().text, "texto plano");
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn la_capacidad_de_correo_no_esta_disponible_si_el_modelo_efectivo_no_admite_herramientas() {
    let backend = ToolBackend::new(vec![]);
    *backend.active_tools.lock() = Some(Ok(false));
    let cap = mail_capability(backend, Some(FakeSkill::new()), false, false).await;
    assert!(!cap.available);
    assert!(
        cap.reason.contains("no admite herramientas"),
        "{}",
        cap.reason
    );
}

// ── Opt-in propio del correo (share_mail_with_remote) ───────────────────────────

#[test]
fn el_correo_no_se_comparte_con_un_remoto_por_omision() {
    assert!(!crate::config::Settings::default().share_mail_with_remote);
    assert!(!MailChat::default().share_mail_with_remote);
    assert!(!MailChat::default().share_calendar_with_remote);
    let leida =
        crate::config::settings_from_json(r#"{"assistant":{"share_memory_with_remote":true}}"#)
            .unwrap();
    assert!(leida.share_memory_with_remote);
    assert!(
        !leida.share_mail_with_remote,
        "consentir la memoria no es consentir el correo"
    );
    let ambos =
        crate::config::settings_from_json(r#"{"assistant":{"share_mail_with_remote":true}}"#)
            .unwrap();
    assert!(ambos.share_mail_with_remote && !ambos.share_memory_with_remote);
}

#[tokio::test]
async fn compartir_la_memoria_con_el_remoto_no_comparte_el_correo() {
    // Memoria compartida (share=true), correo no (mail_chat con false).
    let backend = ToolBackend::remote(vec![call("search_mail", json!({"account_id": "a1"}))]);
    let skill = FakeSkill::new();
    let service = build(
        backend.clone(),
        mail_chat(Some(skill.clone()), FakePerm::new(true), false),
        true,
    );
    let id = service.conversations.lock().create(OWNER, false).unwrap();
    let reply = service
        .chat_turn_with(OWNER, &id, "¿qué correos tengo?", true)
        .await
        .unwrap();
    assert_eq!(reply.text, "texto plano");
    assert_eq!(backend.tools_chats.load(Ordering::SeqCst), 0);
    assert!(skill.calls.lock().is_empty());

    let cap = service.mail_capability(false).await;
    assert!(!cap.available);
    assert!(cap.reason.contains("remoto"), "{}", cap.reason);
}

#[tokio::test]
async fn compartir_el_correo_alcanza_para_un_remoto_aunque_la_memoria_no_se_comparta() {
    let backend = ToolBackend::remote(vec![
        call("search_mail", json!({"account_id": "a1"})),
        text("listo"),
    ]);
    let skill = FakeSkill::new();
    // Correo compartido (true), memoria no (build recibe false).
    let service = build(
        backend.clone(),
        mail_chat(Some(skill.clone()), FakePerm::new(true), true),
        false,
    );
    let id = service.conversations.lock().create(OWNER, false).unwrap();
    let reply = service
        .chat_turn_with(OWNER, &id, "buscá", true)
        .await
        .unwrap();
    assert_eq!(reply.text, "listo");
    assert_eq!(skill.calls_named("search_mail"), 1);
    assert_eq!(backend.local_only_chats.load(Ordering::SeqCst), 0);
    assert!(service.mail_capability(false).await.available);
}

// ── Presupuesto del turno ───────────────────────────────────────────────────────

fn batch(n: usize) -> Result<ModelTurn, BackendError> {
    Ok(ModelTurn::Calls(
        (0..n)
            .map(|i| ToolCall {
                id: format!("id-{i}"),
                name: "search_mail".into(),
                arguments: json!({"account_id": "a1"}),
            })
            .collect(),
    ))
}

#[tokio::test]
async fn una_tanda_con_mas_llamadas_que_el_tope_se_corta_sin_ejecutar_ninguna() {
    let r = rig(vec![batch(13), text("no debió llegar")], true);
    let reply = r.turn("buscá").await.unwrap();
    assert!(reply.text.contains("No pude completar"), "{}", reply.text);
    assert!(
        r.skill.calls.lock().is_empty(),
        "se chequea antes de la tanda"
    );
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn doce_llamadas_en_un_turno_entran_y_la_numero_trece_no() {
    let r = rig(vec![batch(12), text("listo")], true);
    assert_eq!(r.turn("buscá").await.unwrap().text, "listo");
    assert_eq!(r.skill.calls_named("search_mail"), 12);

    // El tope es del turno, no de cada respuesta: 8 + 8 se pasa.
    let r = rig(vec![batch(8), batch(8), text("no debió llegar")], true);
    let reply = r.turn("buscá").await.unwrap();
    assert!(reply.text.contains("No pude completar"), "{}", reply.text);
    assert_eq!(
        r.skill.calls_named("search_mail"),
        8,
        "la segunda tanda no corre"
    );
}

#[tokio::test]
async fn los_resultados_que_pasan_el_tope_de_bytes_cortan_el_turno() {
    let mut big = Arc::try_unwrap(FakeSkill::new()).ok().unwrap();
    big.read_result = "x".repeat(70 * 1024);
    // Un resultado de 70 KiB entra; el segundo suma 140 KiB y pasa los 128 KiB.
    let r = rig_with(
        ToolBackend::new(vec![batch(2), text("no debió llegar")]),
        Arc::new(big),
        true,
        false,
    );
    let reply = r.turn("buscá").await.unwrap();
    assert!(reply.text.contains("No pude completar"), "{}", reply.text);
    assert!(reply.pending.is_empty());
    assert_eq!(r.backend.tools_chats.load(Ordering::SeqCst), 1);

    let mut ok = Arc::try_unwrap(FakeSkill::new()).ok().unwrap();
    ok.read_result = "x".repeat(100 * 1024);
    let r = rig_with(
        ToolBackend::new(vec![batch(1), text("listo")]),
        Arc::new(ok),
        true,
        false,
    );
    assert_eq!(r.turn("buscá").await.unwrap().text, "listo");
}

// ── Cerrar la conversación y los drafts ─────────────────────────────────────────

#[tokio::test]
async fn cerrar_la_conversacion_descarta_su_draft_y_una_confirmacion_tardia_no_envia() {
    let r = rig(vec![call("send_mail", send_args())], true);
    let id = r.turn("mandá").await.unwrap().pending[0].id.clone();
    r.service
        .delete_conversation_as(OWNER, &r.conversation)
        .await
        .unwrap();
    assert!(r.service.mail.pending.lock().by_id.is_empty());
    let late = r.service.confirm_pending(OWNER, &id).await;
    assert!(matches!(late, Err(Error::NotFound(_))), "{late:?}");
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cerrar_una_conversacion_no_toca_los_drafts_de_otra() {
    let r = rig(
        vec![
            call("send_mail", send_args()),
            call("send_mail", send_args()),
        ],
        true,
    );
    let other = r.service.conversations.lock().create(OWNER, false).unwrap();
    r.turn("uno").await.unwrap();
    let kept = r
        .service
        .chat_turn_with(OWNER, &other, "dos", true)
        .await
        .unwrap()
        .pending[0]
        .id
        .clone();
    r.service
        .delete_conversation_as(OWNER, &r.conversation)
        .await
        .unwrap();
    assert_eq!(r.service.mail.pending.lock().by_id.len(), 1);
    r.service.confirm_pending(OWNER, &kept).await.unwrap();
}

#[tokio::test]
async fn un_draft_que_llega_cuando_la_conversacion_ya_se_cerro_no_se_registra() {
    let r = rig(vec![call("send_mail", send_args())], true);
    // La conversación se cierra mientras la herramienta arma el draft.
    let conversations = r.service.conversations.clone();
    let id = r.conversation.clone();
    *r.skill.on_send_mail.lock() = Some(Box::new(move || {
        conversations.lock().delete(OWNER, &id).unwrap();
    }));
    let reply = r.turn("mandá").await.unwrap();
    // Se le muestra el resumen a quien llama, pero sin acción confirmable.
    assert!(!reply.text.is_empty());
    assert!(reply.pending.is_empty(), "{:?}", reply.pending);
    assert!(r.service.mail.pending.lock().by_id.is_empty());
    assert_eq!(r.skill.sent.load(Ordering::SeqCst), 0);
}

#[test]
fn remove_conversation_saca_solo_los_drafts_de_esa_conversacion_y_dueno() {
    let mut pending = PendingActions::default();
    pending.insert("a", "c1", action("1"), noop());
    pending.insert("a", "c2", action("2"), noop());
    pending.insert("b", "c1", action("3"), noop());
    pending.remove_conversation("a", "c1");
    assert!(pending.take("1", "a").is_err());
    assert!(pending.take("2", "a").is_ok());
    assert!(pending.take("3", "b").is_ok());
}

// ── 10. Capacidad DOMAIN_MAIL ───────────────────────────────────────────────────

async fn mail_capability(
    backend: Arc<ToolBackend>,
    skill: Option<Arc<FakeSkill>>,
    share: bool,
    confined: bool,
) -> vasak_assistant_protocol::Capability {
    let service = build(backend, mail_chat(skill, FakePerm::new(true), share), share);
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

#[tokio::test]
async fn la_capacidad_distingue_un_modelo_inalcanzable_de_uno_remoto() {
    let unreachable = ToolBackend::new(vec![]);
    *unreachable.active_model_error.lock() = Some(BackendError::Unavailable("caído".into()));
    // Aun compartiendo con remotos, un modelo que no responde no es «remoto».
    let down = mail_capability(unreachable, Some(FakeSkill::new()), true, false).await;
    let remote = mail_capability(
        ToolBackend::remote(vec![]),
        Some(FakeSkill::new()),
        false,
        false,
    )
    .await;
    assert!(!down.available && !remote.available);
    assert!(down.reason.contains("no responde"), "{}", down.reason);
    assert!(!down.reason.contains("remoto"), "{}", down.reason);
    assert!(remote.reason.contains("remoto"), "{}", remote.reason);

    let not_allowed = ToolBackend::new(vec![]);
    *not_allowed.active_model_error.lock() = Some(BackendError::NotAllowed("nube".into()));
    let cap = mail_capability(not_allowed, Some(FakeSkill::new()), false, false).await;
    assert!(cap.reason.contains("allow_remote"), "{}", cap.reason);
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
    assert_eq!(registry.tools_for(|_| true).len(), 3);
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
    async fn las_capacidades_por_el_bus_informan_los_contactos_disponibles() {
        let d = pair(
            ToolBackend::new(vec![]),
            true,
            false,
            false,
            FakeContacts::new(),
        );
        let (server, client) = crate::test_support::p2p_pair_as(OWNER, |builder| {
            builder.serve_at(SERVICE_PATH, d.service)
        })
        .await
        .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let capabilities = proxy.capabilities().await.unwrap();
        let contacts = capabilities
            .iter()
            .find(|c| c.domain == DOMAIN_CONTACTS)
            .unwrap();
        assert!(contacts.available, "{}", contacts.reason);
        drop(server);
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

// ── 11. Calendario: puerta por dominio y capacidad ──────────────────────────────

/// Una habilidad de calendario de mentira: anota lo que se ejecutó.
struct FakeCalendar {
    calls: Mutex<Vec<String>>,
    alive: bool,
}

impl FakeCalendar {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            alive: true,
        })
    }
}

impl Skill for FakeCalendar {
    fn domain(&self) -> &'static str {
        DOMAIN_CALENDAR
    }

    fn permission(&self) -> &'static str {
        PERMISSION_CALENDAR
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![tool("list_events")]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        _args: &'a Value,
        _ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        self.calls.lock().push(name.to_owned());
        Box::pin(async { Ok(ToolOutcome::Result("[{\"evento\":1}]".into())) })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        let alive = self.alive;
        Box::pin(async move { alive })
    }
}

/// Un servicio con correo y calendario, cada uno con su propio consentimiento.
struct Duo {
    service: AssistantService,
    backend: Arc<ToolBackend>,
    mail: Arc<FakeSkill>,
    calendar: Arc<FakeCalendar>,
    perm: Arc<FakePerm>,
    conversation: String,
}

fn duo(
    backend: Arc<ToolBackend>,
    grant: bool,
    share_mail: bool,
    share_calendar: bool,
    calendar: Arc<FakeCalendar>,
) -> Duo {
    let mail = FakeSkill::new();
    let perm = FakePerm::new(grant);
    let chat = Arc::new(MailChat {
        skills: SkillRegistry::new(vec![mail.clone(), calendar.clone()]),
        permission: perm.clone(),
        pending: Mutex::new(PendingActions::default()),
        share_mail_with_remote: share_mail,
        share_calendar_with_remote: share_calendar,
        share_contacts_with_remote: false,
        share_files_with_remote: false,
    });
    let service = build(backend.clone(), chat, false);
    let conversation = service.conversations.lock().create(OWNER, false).unwrap();
    Duo {
        service,
        backend,
        mail,
        calendar,
        perm,
        conversation,
    }
}

impl Duo {
    async fn turn(&self) {
        self.service
            .chat_turn_with(OWNER, &self.conversation, "hola", true)
            .await
            .unwrap();
    }

    /// Lo que el modelo vio como resultado de sus herramientas.
    fn tool_results(&self) -> String {
        format!("{:?}", self.backend.threads.lock())
    }
}

#[test]
fn cada_dominio_decide_por_separado_si_se_comparte_con_un_remoto() {
    let chat = |mail, calendar| MailChat {
        share_mail_with_remote: mail,
        share_calendar_with_remote: calendar,
        ..MailChat::default()
    };
    assert!(chat(true, false).shares_with_remote(DOMAIN_MAIL));
    assert!(!chat(true, false).shares_with_remote(DOMAIN_CALENDAR));
    assert!(chat(false, true).shares_with_remote(DOMAIN_CALENDAR));
    assert!(!chat(false, true).shares_with_remote(DOMAIN_MAIL));
    // Un dominio desconocido nunca se comparte.
    assert!(!chat(true, true).shares_with_remote("contactos"));
}

#[test]
fn el_registro_filtra_las_herramientas_por_dominio() {
    let registry = SkillRegistry::new(vec![FakeSkill::new(), FakeCalendar::new()]);
    let names = |allow: &dyn Fn(&str) -> bool| -> Vec<String> {
        registry
            .tools_for(|domain| allow(domain))
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    assert_eq!(names(&|_| true).len(), 4);
    assert_eq!(names(&|d| d == DOMAIN_CALENDAR), ["list_events"]);
    assert_eq!(names(&|d| d == DOMAIN_MAIL).len(), 3);
    assert!(names(&|_| false).is_empty());
}

#[tokio::test]
async fn con_el_modelo_local_se_ofrecen_las_herramientas_de_los_dos_dominios() {
    let d = duo(
        ToolBackend::new(vec![
            call("list_events", json!({"from": "a", "to": "b"})),
            call("search_mail", json!({"account_id": "a1"})),
            text("listo"),
        ]),
        true,
        false,
        false,
        FakeCalendar::new(),
    );
    d.turn().await;
    assert_eq!(d.backend.offered_tools.lock()[0], 4);
    assert_eq!(d.calendar.calls.lock().as_slice(), ["list_events"]);
    assert_eq!(d.mail.calls_named("search_mail"), 1);
    let asked: Vec<_> = d.perm.asked.lock().iter().map(|(r, _)| r.clone()).collect();
    assert_eq!(asked, [PERMISSION_CALENDAR, PERMISSION_EMAIL]);
}

#[tokio::test]
async fn con_un_remoto_y_solo_el_correo_compartido_el_calendario_no_se_ofrece_ni_se_ejecuta() {
    let d = duo(
        ToolBackend::remote(vec![
            call("list_events", json!({"from": "a", "to": "b"})),
            call("search_mail", json!({"account_id": "a1"})),
            text("listo"),
        ]),
        true,
        true,
        false,
        FakeCalendar::new(),
    );
    d.turn().await;
    // Sólo las 3 de correo se le ofrecieron al modelo remoto.
    assert_eq!(d.backend.offered_tools.lock()[0], 3);
    // Aunque el modelo pida el calendario igual, no corre ni pide permiso.
    assert!(d.calendar.calls.lock().is_empty());
    assert!(d
        .tool_results()
        .contains("no está disponible con un modelo remoto"));
    assert!(d
        .perm
        .asked
        .lock()
        .iter()
        .all(|(r, _)| r != PERMISSION_CALENDAR));
    // El correo, compartido, sigue funcionando.
    assert_eq!(d.mail.calls_named("search_mail"), 1);
}

#[tokio::test]
async fn con_un_remoto_y_solo_el_calendario_compartido_el_correo_no_se_ofrece_ni_se_ejecuta() {
    let d = duo(
        ToolBackend::remote(vec![
            call("search_mail", json!({"account_id": "a1"})),
            call("list_events", json!({"from": "a", "to": "b"})),
            text("listo"),
        ]),
        true,
        false,
        true,
        FakeCalendar::new(),
    );
    d.turn().await;
    assert_eq!(d.backend.offered_tools.lock()[0], 1);
    assert!(d.mail.calls.lock().is_empty());
    assert!(d
        .tool_results()
        .contains("no está disponible con un modelo remoto"));
    assert!(d
        .perm
        .asked
        .lock()
        .iter()
        .all(|(r, _)| r != PERMISSION_EMAIL));
    assert_eq!(d.calendar.calls.lock().as_slice(), ["list_events"]);
}

#[tokio::test]
async fn con_un_remoto_sin_compartir_nada_el_turno_es_texto_plano() {
    let d = duo(
        ToolBackend::remote(vec![call("list_events", json!({"from": "a", "to": "b"}))]),
        true,
        false,
        false,
        FakeCalendar::new(),
    );
    d.turn().await;
    assert_eq!(d.backend.tools_chats.load(Ordering::SeqCst), 0);
    assert_eq!(d.backend.plain_chats.load(Ordering::SeqCst), 1);
    assert!(d.calendar.calls.lock().is_empty());
}

#[tokio::test]
async fn con_store_calendar_negado_ninguna_herramienta_de_calendario_se_ejecuta() {
    let d = duo(
        ToolBackend::new(vec![
            call("list_events", json!({"from": "a", "to": "b"})),
            text("listo"),
        ]),
        false,
        false,
        false,
        FakeCalendar::new(),
    );
    d.turn().await;
    assert!(d.calendar.calls.lock().is_empty());
    assert_eq!(
        d.perm.asked.lock().as_slice(),
        [(PERMISSION_CALENDAR.to_owned(), String::new())]
    );
    assert!(d
        .tool_results()
        .contains("no concedió el permiso «store.calendar»"));
}

async fn calendar_capability(
    backend: Arc<ToolBackend>,
    calendar: Option<Arc<FakeCalendar>>,
    (share_mail, share_calendar): (bool, bool),
    confined: bool,
) -> vasak_assistant_protocol::Capability {
    let skills: Vec<Arc<dyn Skill>> = [Some(FakeSkill::new() as Arc<dyn Skill>)]
        .into_iter()
        .chain([calendar.map(|c| c as Arc<dyn Skill>)])
        .flatten()
        .collect();
    let chat = Arc::new(MailChat {
        skills: SkillRegistry::new(skills),
        permission: FakePerm::new(true),
        pending: Mutex::new(PendingActions::default()),
        share_mail_with_remote: share_mail,
        share_calendar_with_remote: share_calendar,
        share_contacts_with_remote: false,
        share_files_with_remote: false,
    });
    build(backend, chat, false)
        .calendar_capability(confined)
        .await
}

#[tokio::test]
async fn la_capacidad_de_calendario_esta_disponible_en_el_caso_bueno() {
    let local = calendar_capability(
        ToolBackend::new(vec![]),
        Some(FakeCalendar::new()),
        (false, false),
        false,
    )
    .await;
    assert_eq!(local.domain, DOMAIN_CALENDAR);
    assert!(
        local.available && local.reason.is_empty(),
        "{}",
        local.reason
    );
    let remote = calendar_capability(
        ToolBackend::remote(vec![]),
        Some(FakeCalendar::new()),
        (false, true),
        false,
    )
    .await;
    assert!(remote.available, "{}", remote.reason);
}

#[tokio::test]
async fn la_capacidad_de_calendario_no_esta_disponible_con_motivo_en_cada_caso_que_falta() {
    let mut dead = Arc::try_unwrap(FakeCalendar::new()).ok().unwrap();
    dead.alive = false;
    let none = (false, false);
    let cases = [
        (
            "confinado",
            calendar_capability(
                ToolBackend::new(vec![]),
                Some(FakeCalendar::new()),
                none,
                true,
            )
            .await,
        ),
        (
            "sin habilidad",
            calendar_capability(ToolBackend::new(vec![]), None, none, false).await,
        ),
        (
            "sin herramientas",
            calendar_capability(
                ToolBackend::without_tools(),
                Some(FakeCalendar::new()),
                none,
                false,
            )
            .await,
        ),
        (
            "remoto sin compartir",
            calendar_capability(
                ToolBackend::remote(vec![]),
                Some(FakeCalendar::new()),
                // Compartir el correo no comparte el calendario.
                (true, false),
                false,
            )
            .await,
        ),
        (
            "almacén caído",
            calendar_capability(ToolBackend::new(vec![]), Some(Arc::new(dead)), none, false).await,
        ),
    ];
    let mut reasons = std::collections::HashSet::new();
    for (name, cap) in cases {
        assert_eq!(cap.domain, DOMAIN_CALENDAR, "{name}");
        assert!(!cap.available, "{name}");
        assert!(!cap.reason.is_empty(), "{name}");
        reasons.insert(cap.reason);
    }
    assert_eq!(reasons.len(), 5, "cada caso explica su propio motivo");

    let unreachable = ToolBackend::new(vec![]);
    *unreachable.active_model_error.lock() = Some(BackendError::Unavailable("caído".into()));
    let down =
        calendar_capability(unreachable, Some(FakeCalendar::new()), (true, true), false).await;
    assert!(down.reason.contains("no responde"), "{}", down.reason);
    let denied = ToolBackend::new(vec![]);
    *denied.active_model_error.lock() = Some(BackendError::NotAllowed("nube".into()));
    let cap = calendar_capability(denied, Some(FakeCalendar::new()), none, false).await;
    assert!(cap.reason.contains("allow_remote"), "{}", cap.reason);
}

#[tokio::test]
async fn las_capacidades_de_correo_y_calendario_se_deciden_por_separado() {
    // Remoto con sólo el correo compartido: el correo sí, el calendario no.
    let d = duo(
        ToolBackend::remote(vec![]),
        true,
        true,
        false,
        FakeCalendar::new(),
    );
    assert!(d.service.mail_capability(false).await.available);
    let calendar = d.service.calendar_capability(false).await;
    assert!(!calendar.available);
    assert!(
        calendar.reason.contains("calendario"),
        "{}",
        calendar.reason
    );
    // Y al revés.
    let d = duo(
        ToolBackend::remote(vec![]),
        true,
        false,
        true,
        FakeCalendar::new(),
    );
    assert!(!d.service.mail_capability(false).await.available);
    assert!(d.service.calendar_capability(false).await.available);
}

// ── 12. Contactos: puerta por dominio y capacidad ───────────────────────────────

/// Una habilidad de contactos de mentira: anota lo que se ejecutó.
struct FakeContacts {
    calls: Mutex<Vec<String>>,
    alive: bool,
}

impl FakeContacts {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            alive: true,
        })
    }
}

impl Skill for FakeContacts {
    fn domain(&self) -> &'static str {
        DOMAIN_CONTACTS
    }

    fn permission(&self) -> &'static str {
        PERMISSION_CONTACTS
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![tool("search_contacts")]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        _args: &'a Value,
        _ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        self.calls.lock().push(name.to_owned());
        Box::pin(async { Ok(ToolOutcome::Result("[{\"contacto\":1}]".into())) })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        let alive = self.alive;
        Box::pin(async move { alive })
    }
}

/// Un servicio con correo y contactos, cada uno con su propio consentimiento.
struct Pair {
    service: AssistantService,
    backend: Arc<ToolBackend>,
    mail: Arc<FakeSkill>,
    contacts: Arc<FakeContacts>,
    perm: Arc<FakePerm>,
    conversation: String,
}

fn pair(
    backend: Arc<ToolBackend>,
    grant: bool,
    share_mail: bool,
    share_contacts: bool,
    contacts: Arc<FakeContacts>,
) -> Pair {
    let mail = FakeSkill::new();
    let perm = FakePerm::new(grant);
    let chat = Arc::new(MailChat {
        skills: SkillRegistry::new(vec![mail.clone(), contacts.clone()]),
        permission: perm.clone(),
        pending: Mutex::new(PendingActions::default()),
        share_mail_with_remote: share_mail,
        share_calendar_with_remote: false,
        share_contacts_with_remote: share_contacts,
        share_files_with_remote: false,
    });
    let service = build(backend.clone(), chat, false);
    let conversation = service.conversations.lock().create(OWNER, false).unwrap();
    Pair {
        service,
        backend,
        mail,
        contacts,
        perm,
        conversation,
    }
}

impl Pair {
    async fn turn(&self) {
        self.service
            .chat_turn_with(OWNER, &self.conversation, "hola", true)
            .await
            .unwrap();
    }

    fn tool_results(&self) -> String {
        format!("{:?}", self.backend.threads.lock())
    }
}

#[test]
fn los_contactos_se_comparten_con_un_remoto_solo_si_se_consintio_ese_dominio() {
    let chat = |mail, contacts| MailChat {
        share_mail_with_remote: mail,
        share_contacts_with_remote: contacts,
        share_files_with_remote: false,
        ..MailChat::default()
    };
    assert!(!MailChat::default().shares_with_remote(DOMAIN_CONTACTS));
    assert!(!chat(true, false).shares_with_remote(DOMAIN_CONTACTS));
    assert!(chat(false, true).shares_with_remote(DOMAIN_CONTACTS));
    assert!(!chat(false, true).shares_with_remote(DOMAIN_MAIL));
    assert!(!MailChat::default().share_contacts_with_remote);
}

#[test]
fn el_registro_ofrece_las_herramientas_de_contactos_solo_con_su_dominio() {
    let registry = SkillRegistry::new(vec![FakeSkill::new(), FakeContacts::new()]);
    let names: Vec<String> = registry
        .tools_for(|d| d == DOMAIN_CONTACTS)
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["search_contacts"]);
}

#[tokio::test]
async fn con_el_modelo_local_se_ejecutan_los_contactos_y_su_resultado_llega_al_modelo() {
    let d = pair(
        ToolBackend::new(vec![
            call("search_contacts", json!({"account_id": "a", "query": "q"})),
            text("listo"),
        ]),
        true,
        false,
        false,
        FakeContacts::new(),
    );
    d.turn().await;
    assert_eq!(d.backend.offered_tools.lock()[0], 4);
    assert_eq!(d.contacts.calls.lock().as_slice(), ["search_contacts"]);
    assert!(d.tool_results().contains("contacto"));
    let asked: Vec<_> = d.perm.asked.lock().iter().map(|(r, _)| r.clone()).collect();
    assert_eq!(asked, [PERMISSION_CONTACTS]);
}

#[tokio::test]
async fn con_un_remoto_y_solo_el_correo_compartido_los_contactos_no_se_ofrecen_ni_se_ejecutan() {
    let d = pair(
        ToolBackend::remote(vec![
            call("search_contacts", json!({"account_id": "a", "query": "q"})),
            call("search_mail", json!({"account_id": "a1"})),
            text("listo"),
        ]),
        true,
        true,
        false,
        FakeContacts::new(),
    );
    d.turn().await;
    assert_eq!(d.backend.offered_tools.lock()[0], 3);
    assert!(d.contacts.calls.lock().is_empty());
    let results = d.tool_results();
    assert!(results.contains("no está disponible con un modelo remoto"));
    // Su resultado nunca entró al contexto del modelo remoto.
    assert!(!results.contains("contacto\\\":1"), "{results}");
    assert!(d
        .perm
        .asked
        .lock()
        .iter()
        .all(|(r, _)| r != PERMISSION_CONTACTS));
    // El correo, compartido, sigue funcionando.
    assert_eq!(d.mail.calls_named("search_mail"), 1);
}

#[tokio::test]
async fn con_un_remoto_y_solo_los_contactos_compartidos_el_correo_se_rechaza_y_los_contactos_corren(
) {
    let d = pair(
        ToolBackend::remote(vec![
            call("search_mail", json!({"account_id": "a1"})),
            call("search_contacts", json!({"account_id": "a", "query": "q"})),
            text("listo"),
        ]),
        true,
        false,
        true,
        FakeContacts::new(),
    );
    d.turn().await;
    assert_eq!(d.backend.offered_tools.lock()[0], 1);
    assert!(d.mail.calls.lock().is_empty());
    assert_eq!(d.contacts.calls.lock().as_slice(), ["search_contacts"]);
}

#[tokio::test]
async fn con_un_remoto_sin_compartir_nada_los_contactos_no_arrancan_un_turno_con_herramientas() {
    let d = pair(
        ToolBackend::remote(vec![call(
            "search_contacts",
            json!({"account_id": "a", "query": "q"}),
        )]),
        true,
        false,
        false,
        FakeContacts::new(),
    );
    d.turn().await;
    assert_eq!(d.backend.tools_chats.load(Ordering::SeqCst), 0);
    assert!(d.contacts.calls.lock().is_empty());
}

#[tokio::test]
async fn con_store_contacts_negado_ninguna_herramienta_de_contactos_se_ejecuta() {
    let d = pair(
        ToolBackend::new(vec![
            call("search_contacts", json!({"account_id": "a", "query": "q"})),
            text("listo"),
        ]),
        false,
        false,
        false,
        FakeContacts::new(),
    );
    d.turn().await;
    assert!(d.contacts.calls.lock().is_empty());
    assert_eq!(
        d.perm.asked.lock().as_slice(),
        [(PERMISSION_CONTACTS.to_owned(), "a".to_owned())]
    );
    assert!(d
        .tool_results()
        .contains("no concedió el permiso «store.contacts»"));
}

async fn contacts_capability(
    backend: Arc<ToolBackend>,
    contacts: Option<Arc<FakeContacts>>,
    (share_mail, share_contacts): (bool, bool),
    confined: bool,
) -> vasak_assistant_protocol::Capability {
    let skills: Vec<Arc<dyn Skill>> = [Some(FakeSkill::new() as Arc<dyn Skill>)]
        .into_iter()
        .chain([contacts.map(|c| c as Arc<dyn Skill>)])
        .flatten()
        .collect();
    let chat = Arc::new(MailChat {
        skills: SkillRegistry::new(skills),
        permission: FakePerm::new(true),
        pending: Mutex::new(PendingActions::default()),
        share_mail_with_remote: share_mail,
        share_calendar_with_remote: false,
        share_contacts_with_remote: share_contacts,
        share_files_with_remote: false,
    });
    build(backend, chat, false)
        .contacts_capability(confined)
        .await
}

#[tokio::test]
async fn la_capacidad_de_contactos_esta_disponible_en_el_caso_bueno() {
    let local = contacts_capability(
        ToolBackend::new(vec![]),
        Some(FakeContacts::new()),
        (false, false),
        false,
    )
    .await;
    assert_eq!(local.domain, DOMAIN_CONTACTS);
    assert!(
        local.available && local.reason.is_empty(),
        "{}",
        local.reason
    );
    let remote = contacts_capability(
        ToolBackend::remote(vec![]),
        Some(FakeContacts::new()),
        (false, true),
        false,
    )
    .await;
    assert!(remote.available, "{}", remote.reason);
}

#[tokio::test]
async fn la_capacidad_de_contactos_no_esta_disponible_con_motivo_en_cada_caso_que_falta() {
    let mut dead = Arc::try_unwrap(FakeContacts::new()).ok().unwrap();
    dead.alive = false;
    let none = (false, false);
    let cases = [
        (
            "confinado",
            contacts_capability(
                ToolBackend::new(vec![]),
                Some(FakeContacts::new()),
                none,
                true,
            )
            .await,
        ),
        (
            "sin habilidad",
            contacts_capability(ToolBackend::new(vec![]), None, none, false).await,
        ),
        (
            "sin herramientas",
            contacts_capability(
                ToolBackend::without_tools(),
                Some(FakeContacts::new()),
                none,
                false,
            )
            .await,
        ),
        (
            "remoto sin compartir",
            contacts_capability(
                ToolBackend::remote(vec![]),
                Some(FakeContacts::new()),
                // Compartir el correo no comparte los contactos.
                (true, false),
                false,
            )
            .await,
        ),
        (
            "almacén caído",
            contacts_capability(ToolBackend::new(vec![]), Some(Arc::new(dead)), none, false).await,
        ),
    ];
    let mut reasons = std::collections::HashSet::new();
    for (name, cap) in cases {
        assert_eq!(cap.domain, DOMAIN_CONTACTS, "{name}");
        assert!(!cap.available, "{name}");
        assert!(!cap.reason.is_empty(), "{name}");
        reasons.insert(cap.reason);
    }
    assert_eq!(reasons.len(), 5, "cada caso explica su propio motivo");

    let unreachable = ToolBackend::new(vec![]);
    *unreachable.active_model_error.lock() = Some(BackendError::Unavailable("caído".into()));
    let down =
        contacts_capability(unreachable, Some(FakeContacts::new()), (true, true), false).await;
    assert!(down.reason.contains("no responde"), "{}", down.reason);
    let denied = ToolBackend::new(vec![]);
    *denied.active_model_error.lock() = Some(BackendError::NotAllowed("nube".into()));
    let cap = contacts_capability(denied, Some(FakeContacts::new()), none, false).await;
    assert!(cap.reason.contains("allow_remote"), "{}", cap.reason);
}

#[tokio::test]
async fn la_capacidad_de_contactos_se_decide_aparte_de_la_del_correo() {
    let d = pair(
        ToolBackend::remote(vec![]),
        true,
        true,
        false,
        FakeContacts::new(),
    );
    assert!(d.service.mail_capability(false).await.available);
    let contacts = d.service.contacts_capability(false).await;
    assert!(!contacts.available);
    assert!(contacts.reason.contains("contactos"), "{}", contacts.reason);
}

// ── 12. Archivos: puerta por dominio y capacidad ────────────────────────────────

/// Una habilidad de archivos de mentira: anota lo que se ejecutó.
struct FakeFiles {
    calls: Mutex<Vec<String>>,
    alive: bool,
}

impl FakeFiles {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            alive: true,
        })
    }
}

impl Skill for FakeFiles {
    fn domain(&self) -> &'static str {
        DOMAIN_FILES
    }

    fn permission(&self) -> &'static str {
        PERMISSION_FILES
    }

    fn tools(&self) -> Vec<ToolDef> {
        vec![tool("search_files"), tool("read_file")]
    }

    fn call<'a>(
        &'a self,
        name: &'a str,
        _args: &'a Value,
        _ctx: &'a SkillCtx,
    ) -> BoxFuture<'a, Result<ToolOutcome, Error>> {
        self.calls.lock().push(name.to_owned());
        Box::pin(async { Ok(ToolOutcome::Result("{\"archivo\":1}".into())) })
    }

    fn probe(&self) -> BoxFuture<'_, bool> {
        let alive = self.alive;
        Box::pin(async move { alive })
    }
}

/// Un servicio con correo y archivos, cada uno con su propio consentimiento.
struct FilesPair {
    service: AssistantService,
    backend: Arc<ToolBackend>,
    mail: Arc<FakeSkill>,
    files: Arc<FakeFiles>,
    perm: Arc<FakePerm>,
    conversation: String,
}

fn files_pair(
    backend: Arc<ToolBackend>,
    grant: bool,
    share_mail: bool,
    share_files: bool,
) -> FilesPair {
    let mail = FakeSkill::new();
    let files = FakeFiles::new();
    let perm = FakePerm::new(grant);
    let chat = Arc::new(MailChat {
        skills: SkillRegistry::new(vec![mail.clone(), files.clone()]),
        permission: perm.clone(),
        pending: Mutex::new(PendingActions::default()),
        share_mail_with_remote: share_mail,
        share_files_with_remote: share_files,
        ..MailChat::default()
    });
    let service = build(backend.clone(), chat, false);
    let conversation = service.conversations.lock().create(OWNER, false).unwrap();
    FilesPair {
        service,
        backend,
        mail,
        files,
        perm,
        conversation,
    }
}

impl FilesPair {
    async fn turn(&self) {
        self.service
            .chat_turn_with(OWNER, &self.conversation, "hola", true)
            .await
            .unwrap();
    }

    fn tool_results(&self) -> String {
        format!("{:?}", self.backend.threads.lock())
    }
}

#[test]
fn los_archivos_se_comparten_con_un_remoto_solo_si_se_consintio_ese_dominio() {
    let chat = |others: bool, files: bool| MailChat {
        share_mail_with_remote: others,
        share_calendar_with_remote: others,
        share_contacts_with_remote: others,
        share_files_with_remote: files,
        ..MailChat::default()
    };
    assert!(!MailChat::default().shares_with_remote(DOMAIN_FILES));
    assert!(!MailChat::default().share_files_with_remote);
    // Compartir los demás dominios no comparte los archivos.
    assert!(!chat(true, false).shares_with_remote(DOMAIN_FILES));
    assert!(chat(false, true).shares_with_remote(DOMAIN_FILES));
    assert!(!chat(false, true).shares_with_remote(DOMAIN_MAIL));
}

#[test]
fn el_registro_ofrece_las_herramientas_de_archivos_solo_con_su_dominio() {
    let registry = SkillRegistry::new(vec![FakeSkill::new(), FakeFiles::new()]);
    let names: Vec<String> = registry
        .tools_for(|d| d == DOMAIN_FILES)
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["search_files", "read_file"]);
}

#[tokio::test]
async fn con_el_modelo_local_se_ejecutan_los_archivos_y_se_pide_el_permiso_files() {
    let d = files_pair(
        ToolBackend::new(vec![
            call("search_files", json!({"query": "q"})),
            text("listo"),
        ]),
        true,
        false,
        false,
    );
    d.turn().await;
    assert_eq!(d.backend.offered_tools.lock()[0], 5);
    assert_eq!(d.files.calls.lock().as_slice(), ["search_files"]);
    assert!(d.tool_results().contains("archivo"));
    let asked: Vec<_> = d.perm.asked.lock().iter().map(|(r, _)| r.clone()).collect();
    assert_eq!(asked, [PERMISSION_FILES]);
}

#[tokio::test]
async fn con_un_remoto_sin_compartir_nada_los_archivos_no_arrancan_un_turno_con_herramientas() {
    let d = files_pair(
        ToolBackend::remote(vec![call("search_files", json!({"query": "q"}))]),
        true,
        false,
        false,
    );
    d.turn().await;
    assert_eq!(d.backend.tools_chats.load(Ordering::SeqCst), 0);
    assert!(d.files.calls.lock().is_empty());
}

#[tokio::test]
async fn con_un_remoto_y_solo_el_correo_compartido_los_archivos_no_se_ofrecen_ni_se_ejecutan() {
    let d = files_pair(
        ToolBackend::remote(vec![
            call("read_file", json!({"path": "/x"})),
            call("search_mail", json!({"account_id": "a1"})),
            text("listo"),
        ]),
        true,
        true,
        false,
    );
    d.turn().await;
    // Primera capa: sólo se ofrecen las tres del correo.
    assert_eq!(d.backend.offered_tools.lock()[0], 3);
    // Segunda capa: aunque el modelo la pida, no se ejecuta.
    assert!(d.files.calls.lock().is_empty());
    let results = d.tool_results();
    assert!(results.contains("no está disponible con un modelo remoto"));
    assert!(!results.contains("archivo\\\":1"), "{results}");
    assert!(d
        .perm
        .asked
        .lock()
        .iter()
        .all(|(r, _)| r != PERMISSION_FILES));
    assert_eq!(d.mail.calls_named("search_mail"), 1);
}

#[tokio::test]
async fn con_un_remoto_y_solo_los_archivos_compartidos_el_correo_se_rechaza_y_los_archivos_corren()
{
    let d = files_pair(
        ToolBackend::remote(vec![
            call("search_mail", json!({"account_id": "a1"})),
            call("search_files", json!({"query": "q"})),
            text("listo"),
        ]),
        true,
        false,
        true,
    );
    d.turn().await;
    assert_eq!(d.backend.offered_tools.lock()[0], 2);
    assert_eq!(d.mail.calls_named("search_mail"), 0);
    assert_eq!(d.files.calls.lock().as_slice(), ["search_files"]);
}

#[tokio::test]
async fn con_files_negado_ninguna_herramienta_de_archivos_se_ejecuta() {
    let d = files_pair(
        ToolBackend::new(vec![
            call("search_files", json!({"query": "q"})),
            text("listo"),
        ]),
        false,
        false,
        false,
    );
    d.turn().await;
    assert!(d.files.calls.lock().is_empty());
    assert_eq!(
        d.perm.asked.lock().as_slice(),
        [(PERMISSION_FILES.to_owned(), String::new())]
    );
    assert!(d.tool_results().contains("no concedió el permiso «files»"));
}

async fn files_capability(
    backend: Arc<ToolBackend>,
    files: Option<Arc<dyn Skill>>,
    share_files: bool,
    confined: bool,
) -> vasak_assistant_protocol::Capability {
    let skills: Vec<Arc<dyn Skill>> = [Some(FakeSkill::new() as Arc<dyn Skill>), files]
        .into_iter()
        .flatten()
        .collect();
    let chat = Arc::new(MailChat {
        skills: SkillRegistry::new(skills),
        permission: FakePerm::new(true),
        pending: Mutex::new(PendingActions::default()),
        share_files_with_remote: share_files,
        // Los demás dominios compartidos no habilitan los archivos.
        share_mail_with_remote: true,
        share_calendar_with_remote: true,
        share_contacts_with_remote: true,
    });
    build(backend, chat, false).files_capability(confined).await
}

#[tokio::test]
async fn la_capacidad_de_archivos_esta_disponible_en_el_caso_bueno() {
    let local = files_capability(
        ToolBackend::new(vec![]),
        Some(FakeFiles::new()),
        false,
        false,
    )
    .await;
    assert_eq!(local.domain, DOMAIN_FILES);
    assert!(
        local.available && local.reason.is_empty(),
        "{}",
        local.reason
    );
    let remote = files_capability(
        ToolBackend::remote(vec![]),
        Some(FakeFiles::new()),
        true,
        false,
    )
    .await;
    assert!(remote.available, "{}", remote.reason);
}

#[tokio::test]
async fn la_capacidad_de_archivos_no_esta_disponible_con_motivo_en_cada_caso_que_falta() {
    let mut dead = Arc::try_unwrap(FakeFiles::new()).ok().unwrap();
    dead.alive = false;
    let fake = || Some(FakeFiles::new() as Arc<dyn Skill>);
    let cases = [
        (
            "confinado",
            files_capability(ToolBackend::new(vec![]), fake(), false, true).await,
        ),
        (
            "sin habilidad",
            files_capability(ToolBackend::new(vec![]), None, false, false).await,
        ),
        (
            "sin herramientas",
            files_capability(ToolBackend::without_tools(), fake(), false, false).await,
        ),
        (
            // Compartir el correo, el calendario y los contactos no comparte los archivos.
            "remoto sin compartir",
            files_capability(ToolBackend::remote(vec![]), fake(), false, false).await,
        ),
        (
            "sin carpetas",
            files_capability(ToolBackend::new(vec![]), Some(Arc::new(dead)), false, false).await,
        ),
        (
            "la habilidad real sin raíces",
            files_capability(
                ToolBackend::new(vec![]),
                Some(Arc::new(super::files::FilesSkill::new(vec![]))),
                false,
                false,
            )
            .await,
        ),
    ];
    let mut reasons = std::collections::HashSet::new();
    for (name, cap) in cases {
        assert_eq!(cap.domain, DOMAIN_FILES, "{name}");
        assert!(!cap.available, "{name}");
        assert!(!cap.reason.is_empty(), "{name}");
        reasons.insert(cap.reason);
    }
    // Las dos últimas comparten motivo: sin carpetas.
    assert_eq!(reasons.len(), 5, "cada caso explica su propio motivo");

    let unreachable = ToolBackend::new(vec![]);
    *unreachable.active_model_error.lock() = Some(BackendError::Unavailable("caído".into()));
    let down = files_capability(unreachable, fake(), true, false).await;
    assert!(down.reason.contains("no responde"), "{}", down.reason);
    let denied = ToolBackend::new(vec![]);
    *denied.active_model_error.lock() = Some(BackendError::NotAllowed("nube".into()));
    let cap = files_capability(denied, fake(), false, false).await;
    assert!(cap.reason.contains("allow_remote"), "{}", cap.reason);
}

#[tokio::test]
async fn la_capacidad_de_archivos_se_decide_aparte_de_la_del_correo() {
    let d = files_pair(ToolBackend::remote(vec![]), true, true, false);
    assert!(d.service.mail_capability(false).await.available);
    let files = d.service.files_capability(false).await;
    assert!(!files.available);
    assert!(files.reason.contains("archivos"), "{}", files.reason);
}

#[cfg(test)]
mod files_bus {
    use super::*;
    use vasak_assistant_protocol::{AssistantProxy, SERVICE_PATH};

    #[tokio::test]
    async fn las_capacidades_por_el_bus_informan_los_archivos_disponibles() {
        let d = files_pair(ToolBackend::new(vec![]), true, false, false);
        let (server, client) = crate::test_support::p2p_pair_as(OWNER, |builder| {
            builder.serve_at(SERVICE_PATH, d.service)
        })
        .await
        .unwrap();
        let proxy = AssistantProxy::builder(&client)
            .path(SERVICE_PATH)
            .unwrap()
            .build()
            .await
            .unwrap();
        let capabilities = proxy.capabilities().await.unwrap();
        let files = capabilities
            .iter()
            .find(|c| c.domain == DOMAIN_FILES)
            .unwrap();
        assert!(files.available, "{}", files.reason);
        drop(server);
    }
}
