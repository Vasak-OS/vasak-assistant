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

Todavía no tiene ninguna capacidad sobre datos del usuario —correo, calendario,
contactos, archivos, web—: `Capabilities` las informa como **no disponibles**.
La excepción es su propia memoria (`memory`), que figura disponible cuando está
encendida y abierta (ver más abajo). Si el backend no responde, no tiene el modelo,
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
programa no se usa. Por eso `secret-tool` no puede guardar la clave: se guarda
desde Configuración. No hay migración: un ítem sin el esquema, aunque tenga los
mismos `service` y `url`, no se cuenta ni se usa. Es a propósito: adoptar un
ítem viejo dejaría usar una clave que plantó otro programa. Quien haya guardado
la clave con la 0.2.x (sin el esquema) la tiene que volver a guardar desde
Configuración después de actualizar.

Por la misma razón, el binario de desarrollo (`cargo run`, en
`target/debug/vasak-assistant`) no es `/usr/bin/vasak-assistant` y el llavero no
lo deja pasar: ahí `SetProviderKey` falla, no se lee la clave del proveedor y la
memoria no se puede encender (no hay llave). Para probar eso hace falta el
paquete instalado; no hay una excepción de desarrollo en el llavero.

**Aplicaciones confinadas.** La memoria y las claves son de la persona, no de
cada aplicación. Si quien llama está confinado por AppArmor (el bus informa su
`LinuxSecurityLabel` y no es `unconfined`), `SetProviderKey`, `HasProviderKey`,
`SetMemoryEnabled` y `ClearMemory` contestan
`ar.net.vasak.os.Assistant.Error.AccessDenied`, y su `Chat` va sin memoria: no
se le pasan notas ni se aprende de lo que dice. `MemoryStatus` y
`Capabilities` siguen abiertos. Sin AppArmor no hay etiqueta y se deja pasar;
una etiqueta vacía cuenta como confinada.

Cuenta como confinado todo lo que tenga perfil, aunque esté en modo complain
(por ejemplo `busctl` y `qdbus`) o sea un perfil `(unconfined)` con nombre
(Flatpak, Firefox): `Chat` va sin memoria y los métodos de control dan
`AccessDenied`. Sólo pasa la etiqueta `unconfined` a secas. Configuración, el
chat y la CLI propia del asistente no tienen perfil.

**El límite.** La puerta mira a quien hace el llamado, y nada más. El perfil
`vasak-appimage` permite `dbus,`, `file,` y `change_profile,`: con eso, un
AppImage hostil puede lanzar un proceso sin confinar —con
`systemd-run --user`, con `change_profile` o escribiendo unidades o archivos
de arranque— y ese proceso pasa la puerta. Así, con la memoria encendida, la
puerta por etiqueta **no** protege contra un AppImage que escapa de su perfil:
puede encenderla, borrarla, leer las notas por `Chat` o plantar recuerdos.
Cerrarlo es trabajo del perfil, en Vasak-OS/vasak-desktop-settings#13. Hasta
que ese issue esté cerrado, la memoria no se publica en una ISO ni se promueve:
viene apagada y queda para quien la encienda sabiendo este límite.

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
- **Sólo cifrada.** Antes de abrir la base se comprueba su cabecera: tiene
  que ser una base de Grafeo 0.6 marcada como cifrada, y no puede haber restos
  de una migración de Grafeo (`lince.grafeo.pre-0.6`, `lince.grafeo.migrating`).
  Una base sin cifrar o de otra versión no se abre ni se migra: queda no
  disponible, y la salida es `ClearMemory`.
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
- **Las notas son datos.** Cada nota se filtra al guardarla (sin caracteres de
  formato ni de uso privado, sin marcadores de rol o de instrucciones, sin
  comillas ni palabras de credenciales) y al usarla va escapada como texto JSON
  entre delimitadores `<notas-…>` con un valor nuevo en cada pedido.
- **Sólo desde que se encendió.** No se aprende de lo dicho antes de encender
  la memoria ni de antes de borrarla, y lo que se estaba extrayendo cuando se
  apagó o se borró se descarta.
- **Borrar una conversación borra su resumen.** `DeleteConversation` cancela lo
  que se esté extrayendo de ella y borra sus resúmenes de la memoria. Lo que se
  aprendió de ella antes (preferencias, estilo, temas) queda: no está atado a
  una conversación. El borrado es lógico: la base sigue cifrada con la llave
  vigente, y borrar del todo es `ClearMemory`, que destruye la llave.
  - Con la memoria apagada no se toca la base, así que el resumen de una
    conversación borrada mientras estaba apagada queda.
  - Si la memoria está encendida pero no se puede abrir (llavero bloqueado o
    llave que falta), tampoco se borra y no se reintenta después: para eso,
    `ClearMemory` (#28).
  - Los resúmenes de conversaciones de antes de reiniciar el servicio (las
    conversaciones no sobreviven al reinicio, así que ya no se pueden borrar
    una por una) no se borran a pedido: se van con `ClearMemory`, o solos
    cuando la poda los desplaza porque se llegó al tope de resúmenes.
  - Una conversación que se olvida porque su aplicación se cerró no cuenta como
    borrada: su resumen queda, que para eso está.
- **Sólo lo suyo.** No guarda correo, calendario, contactos, archivos, cuentas
  ni claves: eso se lee de su servicio en el momento y no se persiste.
- **Con un modelo remoto**, la memoria no se lee ni se escribe salvo que la
  persona lo permita con `"share_memory_with_remote": true` en la sección
  `assistant` de `vasak.conf`; si no, `MemoryStatus` dice «La memoria no se
  comparte con un proveedor remoto». Se decide en cada pedido con el modelo que
  de verdad va a responder: sin modelo configurado, Ollama toma el primero
  instalado, y si es uno `:cloud` cuenta como remoto aunque se le hable al
  Ollama local.

| Método | Qué hace |
|---|---|
| `MemoryStatus() → (bbst)` | `enabled`, `available`, `reason` y `bytes` (lo que ocupa en disco, encendida o apagada, medido sin abrirla). Nunca falla: un problema llega como `available = false` con el motivo («La memoria está apagada», «El llavero está bloqueado», «La clave de la memoria no está en el llavero», «La memoria no se pudo abrir»). |
| `SetMemoryEnabled(b enabled) → ()` | Encender: reabre la base que haya o, si no hay, llave nueva al llavero y base vacía. `Unavailable` si el llavero está bloqueado (no abre ningún diálogo), no responde o la base no abre. Apagar: deja de usarla, sin borrar nada. |
| `ClearMemory() → ()` | La borra (llave y archivos) y, si estaba encendida, la recrea vacía con una llave nueva. `Unavailable` con el llavero bloqueado, sin tocar nada. |

`Capabilities` informa `memory` como disponible cuando está encendida y
abierta.

## Ver los errores

El servicio escribe su registro en el journal:

```bash
journalctl --user -u vasak-assistant -f
```

Por omisión registra el nivel `info`: el arranque (versión y un resumen de la
configuración: backend, local o remoto, memoria encendida o apagada), la
parada, encender, apagar o borrar la memoria, y cada llamada que termina en
error o cada falla en segundo plano (con `warn` o `error`). Para ver también el
detalle (cada llamada que sale bien, los pedidos a los backends con su status y
su duración, la puerta de AppArmor):

```bash
systemctl --user edit vasak-assistant
# en el editor:
# [Service]
# Environment=RUST_LOG=debug
systemctl --user restart vasak-assistant
```

Cada línea lleva campos: `method` (el método D-Bus), `sender` (quién llama en el
bus), `conversation` (los primeros 8 caracteres del id), `error` (el tipo),
`backend`, `operation` (lo que se le pidió: `models`, `chat/completions`,
`api/chat`…, nunca la ruta), `status` y `ms`.

**Nunca se registra**: el contenido de los mensajes ni de las respuestas, las
notas, los recuerdos ni los resúmenes, las claves ni los tokens, ni una
dirección con usuario, contraseña, ruta o consulta (se muestra sólo esquema,
host y puerto). De un error de la base de la memoria se registra sólo su código
(`GRAFEO-…`), de uno de archivos la ruta y la clase, y de un `vasak.conf` que no
se puede leer, la línea, la columna y la clase del error, nunca el valor. Los
errores del bus y del llavero (zbus) sí se registran con su texto: viene del
demonio del bus o de `vasak-keyring` y no trae secretos. Las
bibliotecas que ven los cuerpos (el bus, HTTP, TLS) no pasan de `info` aunque
`RUST_LOG` pida más.

## Correrlo

```bash
ollama pull llama3.2          # cualquier modelo sirve
cargo run -p vasak-assistant-daemon
```

Así corre el binario de desarrollo, no `/usr/bin/vasak-assistant`: alcanza para
conversar con un modelo local, pero el llavero protegido no lo deja pasar, así
que la clave del proveedor y la memoria no funcionan (ver «La clave del
proveedor»).

En otra terminal:

```bash
d=ar.net.vasak.os.Assistant p=/ar/net/vasak/os/Assistant
busctl --user call $d $p $d Status
id=$(busctl --user call $d $p $d NewConversation | cut -d'"' -f2)
busctl --user call $d $p $d Chat ss "$id" "Hola, ¿quién sos?"
```

Con AppArmor, `busctl` tiene perfil (en modo complain), así que cuenta como
confinado: el `Chat` de arriba va **sin memoria** y `SetMemoryEnabled`,
`ClearMemory`, `SetProviderKey` y `HasProviderKey` le contestan
`AccessDenied`. Para la memoria y las claves, Configuración.

Instalado, lo arranca systemd como servicio de usuario
(`packaging/vasak-assistant.service`, parte de `graphical-session.target`) o
D-Bus la primera vez que alguien le habla
(`packaging/ar.net.vasak.os.Assistant.service`).
