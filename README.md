# vasak-assistant

El asistente de IA residente de VasakOS — **Lince**. Un servicio local que
responde desde la máquina, con el modelo que elija cada persona —desde uno
totalmente local hasta el proveedor que prefiera— y **sin tocar ningún dato sin
permiso**: cada dominio que Lince puede leer —correo, calendario, contactos,
archivos, la web— es un permiso que se concede, igual que los tokens de
`vasak-accounts`.

El diseño completo, las fases y el porqué viven en el issue ancla **#1**.

## Lo que hay hoy (fase 1, y parte de la 2)

El servicio y el contrato:

- **`protocol/`** (`vasak-assistant-protocol`): el contrato D-Bus
  (`ar.net.vasak.os.Assistant` en `/ar/net/vasak/os/Assistant`), los tipos que
  viajan, los límites y el proxy de zbus 5.
- **`daemon/`** (`vasak-assistant-daemon`, binario `vasak-assistant`): conversa
  con un modelo por uno de dos backends intercambiables:
  - **`ollama`** (por omisión): [Ollama](https://ollama.com) en
    `http://127.0.0.1:11434`.
  - **`openai`**: cualquier servidor compatible con la API de OpenAI —el propio
    OpenAI y la mayoría de los proveedores, o llama.cpp server y LM Studio en la
    máquina—.

  Las conversaciones viven en memoria y se pierden al cerrar el servicio.

Todavía no tiene ninguna capacidad sobre datos del usuario: `Capabilities` las
informa como **no disponibles**. Si el backend no responde, no tiene el modelo,
pide una clave que no está o la configuración no se puede usar, `Status` lo dice
con un motivo legible y `Chat` devuelve el error
`ar.net.vasak.os.Assistant.Error.Unavailable`; el servicio sigue en pie.

## Configuración

En la sección `assistant` de `~/.config/vasak/vasak.conf` (o la ruta de
`VASAK_CONFIG_PATH`). Todo es opcional; sin la sección, Ollama local. Se lee al
arrancar: después de cambiarla, `systemctl --user restart vasak-assistant`.

```json
{
  "assistant": {
    "backend": "openai",
    "url": "https://api.openai.com/v1",
    "model": "gpt-4o-mini",
    "allow_remote": true
  }
}
```

| Clave | Por omisión |
|---|---|
| `backend` | `ollama`; también `openai` |
| `url` | `http://127.0.0.1:11434` con `ollama`, `http://127.0.0.1:8080/v1` (llama.cpp server) con `openai` |
| `model` | el primero que ofrezca el backend; con un remoto es obligatorio |
| `allow_remote` | `false` |

**Local o remoto.** Lo decide la dirección, no una declaración: un host
`localhost` o una IP de loopback es **local**; cualquier otro es **remoto**. Un
remoto sólo se usa con `allow_remote: true`, sólo por `https` y con el modelo
elegido; si no, queda no disponible con el motivo. `Status` informa en
`model.local` y `model.host` si la conversación sale de la máquina y hacia
dónde.

**La clave del proveedor nunca va en la configuración.** Se lee de
`vasak-keyring` (el Secret Service de la sesión), del ítem con los atributos
`service=ar.net.vasak.os.Assistant` y `url=` la dirección base terminada en
`/`. Se guarda desde **Configuración → Asistente**, o con `secret-tool`:

```bash
secret-tool store --label='Lince' service ar.net.vasak.os.Assistant url https://api.openai.com/v1/
```

Configuración no toca el llavero: le pasa la clave al servicio, que es quien
crea el ítem.

| Método | Qué hace |
|---|---|
| `SetProviderKey(s url, s key) → ()` | Guarda la clave de esa dirección en la colección por omisión del llavero, reemplazando la anterior. La dirección se normaliza como la de `vasak.conf` (con `/` final); un host remoto tiene que ser `https`. La clave se recorta y va de 1 a 4096 bytes sin caracteres de control. `InvalidArgs` si algo no vale; `Unavailable` si el llavero está bloqueado (no abre ningún diálogo) o no responde. La clave nunca vuelve en un error ni va al registro. |
| `HasProviderKey(s url) → b` | Si hay una clave guardada para esa dirección (aunque el llavero esté bloqueado). Nunca devuelve la clave. |

La clave queda atada a esa dirección: si la configuración cambia de servidor, la
clave de antes no viaja al nuevo.

## Correrlo

```bash
ollama pull llama3.2          # cualquier modelo sirve
cargo run -p vasak-assistant-daemon
```

En otra terminal:

```bash
d=ar.net.vasak.os.Assistant p=/ar/net/vasak/os/Assistant
busctl --user call $d $p $d Status
id=$(busctl --user call $d $p $d NewConversation | cut -d'"' -f2)
busctl --user call $d $p $d Chat ss "$id" "Hola, ¿quién sos?"
```

Instalado, lo arranca systemd como servicio de usuario
(`packaging/vasak-assistant.service`, parte de `graphical-session.target`) o
D-Bus la primera vez que alguien le habla
(`packaging/ar.net.vasak.os.Assistant.service`).
