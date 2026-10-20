# vasak-assistant

El asistente de IA residente de VasakOS — **Lince**. Un servicio local que
responde desde la máquina, con el modelo que elija cada persona —desde uno
totalmente local hasta el proveedor que prefiera— y **sin tocar ningún dato sin
permiso**: cada dominio que Lince puede leer —correo, calendario, contactos,
archivos, la web— es un permiso que se concede, igual que los tokens de
`vasak-accounts`.

El diseño completo, las fases y el porqué viven en el issue ancla **#1**.

## Lo que hay hoy (fases 1 a 4)

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
| `share_memory_with_remote` | `false`: con un backend remoto, la memoria no se lee ni se escribe |

**Local o remoto.** Lo decide la dirección, no una declaración: un host
`localhost` o una IP de loopback es **local**; cualquier otro es **remoto**. Un
remoto sólo se usa con `allow_remote: true`, sólo por `https` y con el modelo
elegido; si no, queda no disponible con el motivo. `Status` informa en
`model.local` y `model.host` si la conversación sale de la máquina y hacia
dónde.

**La clave del proveedor nunca va en la configuración.** Se lee de
`vasak-keyring` (el Secret Service de la sesión), del ítem con los atributos
`service=ar.net.vasak.os.Assistant`, `url=` la dirección base terminada en `/`,
`purpose=provider` y el esquema protegido `xdg:schema=ar.net.vasak.os.Assistant`.
Se guarda desde **Configuración → Asistente**: Configuración no toca el
llavero, le pasa la clave al servicio, que es quien crea el ítem.

El esquema protegido es la defensa: `vasak-keyring` deja leer, crear y
reemplazar esos ítems sólo a `/usr/bin/vasak-assistant` (vasak-keyring#55), y
el servicio busca con el mismo esquema, así que un ítem común plantado por otro
programa no se usa. Por eso, con esa restricción activa, `secret-tool` ya no
puede guardar la clave. Antes de eso, el equivalente a mano es:

```bash
secret-tool store --label='Lince' service ar.net.vasak.os.Assistant url https://api.openai.com/v1/ purpose provider xdg:schema ar.net.vasak.os.Assistant
```

Una clave guardada por una versión anterior (sin `purpose` ni `xdg:schema`) se
pasa sola al ítem protegido la primera vez que se la lee, si es la única para
esa dirección; con más de una no se usa ninguna (el registro lo dice) y hay que
volver a guardarla desde Configuración. Con el llavero bloqueado no se migra ni
se abre ningún diálogo.

| Método | Qué hace |
|---|---|
| `SetProviderKey(s url, s key) → ()` | Guarda la clave de esa dirección en la colección por omisión del llavero, reemplazando la anterior. La dirección se normaliza como la de `vasak.conf` (con `/` final); un host remoto tiene que ser `https`. La clave se recorta y va de 1 a 4096 bytes sin caracteres de control. `InvalidArgs` si algo no vale; `Unavailable` si el llavero está bloqueado (no abre ningún diálogo) o no responde. La clave nunca vuelve en un error ni va al registro. |
| `HasProviderKey(s url) → b` | Si hay una clave guardada para esa dirección (aunque el llavero esté bloqueado). Con más de una para la misma dirección da `false`, porque el servicio no usa ninguna. Nunca devuelve la clave. |

La clave queda atada a esa dirección: si la configuración cambia de servidor, la
clave de antes no viaja al nuevo.

## La memoria

Lince puede tener una memoria propia: una base de grafos
([Grafeo](https://github.com/GrafeoDB/grafeo), embebida en el servicio) en
`~/.local/state/vasak-assistant/memory/` (la carpeta de estado que le da
systemd, `$STATE_DIRECTORY`). No va en el almacén de cuentas.

- **Viene apagada.** Existe sólo desde que la persona la enciende; no hay nada
  que tocar en `vasak.conf`. Encendida o no lo dice el archivo `enabled` al lado
  de `memory/`.
- **Cifrada.** El archivo y su registro de escritura (WAL) van con AES-256-GCM,
  con una llave de 32 bytes que genera el servicio la primera vez que se
  enciende y guarda en el llavero (atributos `service=ar.net.vasak.os.Assistant`,
  `purpose=memory` y el esquema protegido `xdg:schema=ar.net.vasak.os.Assistant`).
  Sin la llave, lo que hay en disco no se puede leer.
- **Apagar no borra.** Deja de usarla y de escribirla; encenderla de nuevo
  reabre la misma base con su llave.
- **Borrar es `ClearMemory`.** Primero la llave del llavero y después los
  archivos; con el llavero bloqueado no se toca nada.
- Si la llave falta o no abre la base, la memoria queda **no disponible** con el
  motivo; el servicio nunca la regenera ni la borra solo. La salida es borrarla.
- **Qué guarda.** Lo que la persona dice de sí misma —preferencias, cómo
  quiere que le hablen, hábitos, temas que le interesan, correcciones y los
  nombres que usa («lo de la abuela» = la casa de su abuela)— y un resumen
  breve cada seis turnos. Lo aprende el propio modelo después de responder, en
  segundo plano, y sólo cuando el mensaje tiene señales de eso. Cada recuerdo
  pasa por filtros: nada con claves, contraseñas, tarjetas ni tokens, y nada
  con correos, teléfonos, rutas, cuentas bancarias o fechas. Un turno con un
  secreto no se aprende. Topes por clase y 4 MiB en total; lo más viejo y menos
  reforzado se poda primero.
- **Cómo lo usa.** Antes de cada mensaje, las notas que vienen al caso (hasta 4
  KiB) van al modelo como contexto de ese turno, marcadas como datos y no como
  instrucciones. Nunca quedan en el historial ni en `History`.
- **Sólo lo suyo.** No guarda correo, calendario, contactos, archivos, cuentas
  ni claves: eso se lee de su servicio en el momento y no se persiste.
- **Con un proveedor remoto**, la memoria no se lee ni se escribe salvo que la
  persona lo permita con `"share_memory_with_remote": true` en la sección
  `assistant` de `vasak.conf`; si no, `MemoryStatus` dice «La memoria no se
  comparte con un proveedor remoto».

| Método | Qué hace |
|---|---|
| `MemoryStatus() → (bbst)` | `enabled`, `available`, `reason` y `bytes` (lo que ocupa en disco, encendida o apagada, medido sin abrirla). Nunca falla: un problema llega como `available = false` con el motivo («La memoria está apagada», «El llavero está bloqueado», «La clave de la memoria no está en el llavero», «La memoria no se pudo abrir»). |
| `SetMemoryEnabled(b enabled) → ()` | Encender: reabre la base que haya o, si no hay, llave nueva al llavero y base vacía. `Unavailable` si el llavero está bloqueado (no abre ningún diálogo), no responde o la base no abre. Apagar: deja de usarla, sin borrar nada. |
| `ClearMemory() → ()` | La borra (llave y archivos) y, si estaba encendida, la recrea vacía con una llave nueva. `Unavailable` con el llavero bloqueado, sin tocar nada. |

`Capabilities` informa `memory` como disponible cuando está encendida y
abierta.

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
