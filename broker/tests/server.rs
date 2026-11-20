//! El servidor IPC del broker, de punta a punta: se lanza el binario de verdad
//! con un `XDG_RUNTIME_DIR` y un `vasak.conf` propios, y se le habla por el
//! socket con el enmarcado del crate `ipc`. El «modelo» es un servidor TCP de
//! mentira en 127.0.0.1.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixStream};
use tokio::process::{Child, Command};

use vasak_assistant_ipc::{
    read_frame, write_frame, BrokerEnvelope, BrokerRequest, BrokerResponse, MAX_FRAME_BYTES,
};
use vasak_assistant_protocol::{Message, Role};

/// Un «modelo» OpenAI de mentira: contesta `texto` a todo y guarda la cabecera
/// y la ruta de cada pedido que le llega.
struct FakeModel {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

impl FakeModel {
    async fn start(text: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4096];
                    // Cabeceras y cuerpo llegan juntos para estos pedidos chicos.
                    loop {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..read]);
                        if buffer.windows(4).any(|w| w == b"\r\n\r\n") && buffer.ends_with(b"}") {
                            break;
                        }
                    }
                    log.lock()
                        .push(String::from_utf8_lossy(&buffer).into_owned());
                    let body = format!(r#"{{"choices":[{{"message":{{"content":"{text}"}}}}]}}"#);
                    let reply = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(reply.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Self { port, seen }
    }

    fn requests(&self) -> Vec<String> {
        self.seen.lock().clone()
    }
}

/// El broker real, lanzado como proceso. Se mata al soltarlo.
struct Broker {
    _child: Child,
    dir: PathBuf,
    socket: PathBuf,
    /// La dirección base normalizada de la config, para marcar `key_base_url` en
    /// el sobre igual que lo hace el demonio.
    base_url: String,
}

impl Broker {
    /// Lo configura contra el endpoint `url` (backend OpenAI, que fija dirección
    /// y modelo) y espera a que el socket acepte.
    async fn start(url: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("vasak-broker-it-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("vasak.conf");
        std::fs::write(
            &conf,
            serde_json::json!({"assistant": {"backend": "openai", "url": url, "model": "m"}})
                .to_string(),
        )
        .unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_vasak-assistant-broker"))
            .env("XDG_RUNTIME_DIR", &dir)
            .env("VASAK_CONFIG_PATH", &conf)
            .env_remove("HTTP_PROXY")
            .env_remove("http_proxy")
            .env_remove("ALL_PROXY")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("se lanza el broker");
        let socket = dir
            .join(vasak_assistant_ipc::RUNTIME_SUBDIR)
            .join(vasak_assistant_ipc::SOCKET_NAME);
        // Espera acotada a que el proceso abra el socket: no hay otra señal.
        let ready = tokio::time::timeout(Duration::from_secs(10), async {
            while UnixStream::connect(&socket).await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(ready.is_ok(), "el broker no abrió su socket");
        let base_url = vasak_assistant_ipc::config::normalize_url(url)
            .expect("la dirección de prueba es válida")
            .to_string();
        Self {
            _child: child,
            dir,
            socket,
            base_url,
        }
    }

    async fn connect(&self) -> UnixStream {
        UnixStream::connect(&self.socket).await.unwrap()
    }

    /// Un viaje completo: un sobre, una respuesta.
    async fn ask(
        &self,
        key: Option<&str>,
        request: BrokerRequest,
    ) -> std::io::Result<BrokerResponse> {
        let mut stream = self.connect().await;
        let envelope = BrokerEnvelope {
            api_key: key.map(str::to_owned),
            // La clave se resolvió para la dirección de la config, igual que en
            // el demonio: el broker la usa sólo si coincide con la suya.
            key_base_url: key.map(|_| self.base_url.clone()),
            request,
        };
        write_frame(&mut stream, &envelope).await?;
        read_frame(&mut stream).await
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn hello() -> Vec<Message> {
    vec![Message {
        role: Role::User,
        content: "hola".into(),
    }]
}

fn text_of(response: BrokerResponse) -> String {
    match response {
        BrokerResponse::Text(Ok(text)) => text,
        other => panic!("se esperaba texto, llegó {other:?}"),
    }
}

#[tokio::test]
async fn el_chat_va_al_destino_de_la_config_y_lleva_la_clave_del_sobre() {
    let model = FakeModel::start("desde X").await;
    let broker = Broker::start(&format!("http://127.0.0.1:{}/v1", model.port)).await;

    let response = broker
        .ask(
            Some("sk-del-sobre"),
            BrokerRequest::Chat { messages: hello() },
        )
        .await
        .unwrap();

    assert_eq!(text_of(response), "desde X");
    let seen = model.requests();
    assert_eq!(seen.len(), 1, "un pedido, una conexión al destino");
    assert!(
        seen[0].starts_with("POST /v1/chat/completions"),
        "{}",
        seen[0]
    );
    assert!(seen[0].to_ascii_lowercase().contains("bearer sk-del-sobre"));
}

#[tokio::test]
async fn un_campo_de_destino_colado_en_el_pedido_se_ignora() {
    let model = FakeModel::start("desde X").await;
    let decoy = FakeModel::start("desde el señuelo").await;
    let broker = Broker::start(&format!("http://127.0.0.1:{}/v1", model.port)).await;

    // JSON a mano: un demonio comprometido intenta nombrar un host.
    let raw = serde_json::json!({
        "api_key": null,
        "request": {"Chat": {
            "messages": [{"role": "user", "content": "hola"}],
            "url": format!("http://127.0.0.1:{}/v1", decoy.port),
            "host": "127.0.0.1",
        }},
    });
    let mut stream = broker.connect().await;
    write_frame(&mut stream, &raw).await.unwrap();
    let response: BrokerResponse = read_frame(&mut stream).await.unwrap();

    assert_eq!(text_of(response), "desde X");
    assert_eq!(model.requests().len(), 1);
    assert!(
        decoy.requests().is_empty(),
        "el broker conectó a un destino del pedido"
    );
}

#[tokio::test]
async fn un_marco_incompleto_o_enorme_no_tumba_al_broker() {
    let model = FakeModel::start("sigue vivo").await;
    let broker = Broker::start(&format!("http://127.0.0.1:{}/v1", model.port)).await;

    // Dos bytes del largo y se corta.
    let mut half = broker.connect().await;
    half.write_all(&[0, 0]).await.unwrap();
    drop(half);

    // Declara 100 bytes, manda 3 y se corta.
    let mut short = broker.connect().await;
    short
        .write_all(&[0, 0, 0, 100, b'{', b'"', b'a'])
        .await
        .unwrap();
    drop(short);

    // Largo declarado más allá del tope: se rechaza sin reservar.
    let mut huge = broker.connect().await;
    huge.write_all(&((MAX_FRAME_BYTES as u32) + 1).to_be_bytes())
        .await
        .unwrap();
    let mut rest = Vec::new();
    // El broker cierra: la lectura termina (sin respuesta) en vez de colgarse.
    huge.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());

    // JSON ilegible con largo correcto.
    let mut junk = broker.connect().await;
    junk.write_all(&[0, 0, 0, 4, b'n', b'o', b'p', b'e'])
        .await
        .unwrap();
    let mut rest = Vec::new();
    junk.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());

    // Y sigue sirviendo.
    let response = broker
        .ask(None, BrokerRequest::Chat { messages: hello() })
        .await
        .unwrap();
    assert_eq!(text_of(response), "sigue vivo");
}

#[tokio::test]
async fn al_tope_de_conexiones_se_descartan_las_que_sobran_sin_tumbar_al_broker() {
    const OPEN: usize = 40;
    const CAP: usize = 32;
    let model = FakeModel::start("ok").await;
    let broker = Broker::start(&format!("http://127.0.0.1:{}/v1", model.port)).await;

    // 40 conexiones que no mandan nada, todas abiertas a la vez. Como mucho 32
    // pueden estar aceptadas (cada una retiene su cupo hasta el plazo de
    // lectura), así que al menos 8 se descartan, pase lo que pase con el cupo
    // de la sonda de arranque. Se cuentan los EOF de todas en paralelo: no
    // depende de cuál conexión cae en cuál cupo.
    let mut reads = tokio::task::JoinSet::new();
    for _ in 0..OPEN {
        let mut stream = broker.connect().await;
        reads.spawn(async move {
            let mut byte = [0u8; 1];
            let read = stream.read(&mut byte).await;
            matches!(read, Ok(0) | Err(_))
        });
    }
    let mut dropped = 0;
    let counted = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(Ok(closed)) = reads.join_next().await {
            if closed {
                dropped += 1;
            }
            if dropped >= OPEN - CAP {
                break;
            }
        }
    })
    .await;
    assert!(
        counted.is_ok() && dropped >= OPEN - CAP,
        "se esperaban {} descartadas y hubo {dropped}",
        OPEN - CAP
    );
    // Las aceptadas siguen esperando el marco: el broker no se cayó.
    assert!(!reads.is_empty());
    reads.abort_all();
}
