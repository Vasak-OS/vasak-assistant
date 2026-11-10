# Changelog

Todas las novedades de este repositorio, por paquete.

## vasak-assistant 0.3.14

El borrado de una conversación ya no se pierde si la memoria está apagada o el
llavero bloqueado (#28).

- **`daemon`** — `forget_conversation`, cuando no puede borrar los resúmenes
  porque la memoria está apagada o la base no se puede abrir (llavero bloqueado
  o llave ausente), anota la conversación en una cola en RAM de borrados
  pendientes (`pending_deletes`, con tope y sin duplicados) en vez de responder
  como si hubiera borrado y olvidarlo. `flush_pending_deletes` la vacía en la
  próxima apertura de la base (al desbloquear el llavero o al volver a encender
  la memoria). Apagada no escribe nada, sólo acumula en RAM (decisión 10); la
  cola vive mientras el demonio corre. Antes el resumen quedaba guardado y no se
  reintentaba.

## vasak-assistant 0.3.11

La memoria no guarda datos de categoría especial ni domicilios (#11).

- **`daemon`** — `filter::mentions_sensitive` descarta los recuerdos que nombran
  salud, religión, orientación sexual o afiliación política, o un domicilio
  (tipo de vía + nombre + altura). Se aplica como `is_user_data`: en la puerta
  de la nota (`is_clean_note`) y antes de extraer (`extraction_window`), con el
  mismo plegado NFKC / sin tildes / homoglifos cirílicos que el resto de los
  filtros. Ante la duda se descarta.

## vasak-assistant 0.3.10

Lo de un turno no se aprende en una memoria que se borró mientras se respondía
(#20).

- **`daemon`** — `chat_with` toma la época de la memoria al comienzo del turno,
  antes de `recall`, y se la pasa a `learn_after`. Si mientras el modelo
  respondía se borró (`ClearMemory`), se encendió o se apagó la memoria, de ese
  turno no se aprende nada: ni recuerdos ni resumen, no se lanza la extracción,
  no se anota en vuelo y no se toca la ventana de la conversación. Antes la
  época se tomaba recién con la respuesta, y lo dicho en ese turno podía
  guardarse en la memoria nueva. `store_at` sigue comparando contra la misma
  época del comienzo del turno.

## vasak-assistant 0.3.9

Un modelo de Ollama que corre afuera se reconoce por `remote_host`, no sólo
por el nombre (#23).

- **`daemon`** — un alias de `ollama create` (`FROM gpt-oss:120b-cloud`, o con
  `remote_host`) corre en la nube con un nombre sin sufijo, y se trataba como
  local: la conversación y la memoria salían de la máquina sin `allow_remote`
  ni `share_memory_with_remote`. Ahora el modelo efectivo es remoto si su fila
  de `/api/tags` trae `remote_host`, o si el nombre termina en `:cloud` o
  `-cloud`. Vale para el chat, `chat_local_only`, la puerta de la memoria y
  `model.local` de `Status`.
- **`daemon`** — falla cerrado: sin `/api/tags` no se manda nada, y un modelo
  que no figura en la lista cuenta como remoto. Un `:cloud`/`-cloud`
  configurado se usa aunque no figure (el servidor lo resuelve contra
  ollama.com sin haberlo bajado), como remoto.
- **`daemon`** — si la respuesta de un chat trae `remote_host` (el nombre se
  reapuntó a un remoto durante el pedido), sin `allow_remote` se descarta, da
  error y no se reintenta. Con `allow_remote` y sin
  `share_memory_with_remote`, la de un chat con notas de la memoria se descarta
  y se repite sin ellas; con los dos permisos, el chat con notas acepta la
  respuesta remota. Lo descartado no se aprende y queda un aviso en el
  registro. `BackendError` suma `RemoteAfterSend` para ese descarte: a
  diferencia de `NotLocal`, el pedido ya había salido.
- **`daemon`** — con un modelo remoto rechazado por falta de `allow_remote`,
  `Status` informa `model.local = false` en vez de `true`.
- **`daemon`** — la línea de arranque dice «local según la dirección» con
  Ollama: un alias se confirma en cada pedido.

## vasak-assistant 0.3.8

La memoria, más dura frente a una base cambiada y a las notas con órdenes
escondidas (#18, #19).

- **`daemon`** — la base guarda en su único `:LinceMeta` un canario atado a la
  llave (`SHA-256(llave ‖ "lince-canary")`, formato 2). Al abrir, si no hay
  exactamente un `:LinceMeta` o el canario falta o no coincide (comparado en
  tiempo constante), la base se cierra, no se usa y queda una línea en el
  registro, sin la ruta ni el texto de Grafeo: si alguien cambió el archivo por
  una base 0.5.x en claro entre mirar la cabecera y abrirla, Grafeo la migra con
  nuestra llave pero no puede tener el canario. Una base sin canario se rechaza
  y nunca se le agrega. Una de formato 1 no se migra: `MemoryStatus` dice «La
  memoria no es de esta versión: borrala para empezar de nuevo», y `ClearMemory`
  la borra y crea una nueva. Después de abrir se vuelven a mirar además los
  restos de una migración (`lince.grafeo.pre-0.6`, `.migrating`) (#18). Abrir
  por un descriptor ya validado no se puede con Grafeo 0.6.0-beta.1, que sólo
  abre por ruta.
- **`daemon`** — más marcadores de inyección: «olvidá lo anterior», «a partir
  de ahora», «actuá como», «disregard», «forget», «developer» y las comillas
  `'‘’‹›„`. Se buscan en NFKC, con los homoglifos cirílicos llevados a su letra
  latina, palabra por palabra sin lo que no es letra o dígito (`i.g.n.o.r.a`) y
  con las letras sueltas juntadas (`i g n o r a`), sin juntar palabras
  distintas. Las raíces y palabras que no aparecen dentro de una palabra
  legítima (`ignor`, `instrucci`, `instruct`, `prompt`, `disregard`, `forget`,
  `developer`, `assistant`) se buscan en cualquier lugar de una palabra, así
  `systemprompt` o `developer_mode` caen; `system`, `sistema` y `regla`, como
  palabra entera (`systemd`, `filesystem`, `arreglar` y «toma notas» ya no se
  descartan); las frases, seguidas o todo junto en una palabra
  (`olvida-lo-anterior`); los signos (`<|`, `###`, `user:`), como subcadena. Los
  selectores de variación y los rellenos de Hangul cuentan como caracteres
  prohibidos, y una nota sólo puede tener letras latinas (ASCII, Latin-1 y Latin
  Extended-A, sin `ı`) que sin tildes queden en ASCII (afuera `ø`, `ł`, `ß`,
  `æ`, `œ`), dígitos, el espacio, la puntuación básica y `$ € ª º`. `recall`
  aplica al leer los mismos filtros que al escribir, antes del corte a las 5 de
  mayor puntaje, así las notas que guardó una versión anterior y hoy no
  entrarían ya no llegan al modelo ni le quitan el lugar a una limpia (#19).

## vasak-assistant 0.3.7

La puerta de AppArmor alcanza a `MemoryStatus` y los errores del backend dejan
de reconocerse por el texto (#22, #24).

- **`daemon`** — `MemoryStatus` contesta `AccessDenied` a un llamador
  confinado, igual que `SetMemoryEnabled` y `ClearMemory`: lo que ocupa la
  memoria crece con el uso y dejaba inferir cuánto se usa el asistente (#22).
- **`daemon`** — a un llamador confinado, `Capabilities` le informa `memory`
  como no disponible con un motivo fijo («No disponible para aplicaciones
  confinadas»), sin abrir la base ni preguntar al llavero: antes le decía si
  estaba encendida y cómo estaba el llavero.
- **`daemon`** — con el modelo por omisión de Ollama en la nube y sin
  `allow_remote`, `MemoryStatus` dice «El modelo no está permitido: corre en la
  nube y falta «allow_remote»» en lugar de «El modelo no responde» (#24).
  También con `share_memory_with_remote`: antes ese caso la daba por
  disponible aunque el chat rechazara el modelo; ahora siempre se mira el
  modelo efectivo, y si no responde, «El modelo no responde».
- **`daemon`** — `BackendError` suma `NotAllowed`, `NotLocal` y `Busy`: el
  rechazo de `chat_local_only` y el cupo de modelos lleno ya no se reconocen
  comparando el texto del error. En el bus no cambia nada (#24).
- **`protocol`** — el contrato de `memory_status` documenta el
  `AccessDenied`.

## vasak-assistant 0.3.5

Un solo cliente ya no acapara los pedidos al modelo (#3).

- **`protocol`** — `MAX_CHATS_IN_FLIGHT_PER_OWNER` (2): cuántos `Chat` puede
  tener una conexión esperando respuesta a la vez. Pasado eso, `Chat` da
  `Busy`.
- **`daemon`** — el tope por dueño se cuenta con los chats en vuelo, bajo el
  mismo candado que las conversaciones: borrar una conversación que espera
  respuesta no devuelve el lugar, porque el pedido sigue en el backend. El
  rechazo no ocupa lugar en el backend y queda en el registro como cualquier
  llamada rechazada. `models` y `active_model` tienen su propio cupo, aparte
  del de los chats y sin fila: sin lugar libre, `Models` da `Busy` y
  `active_model` devuelve lo último que se supo aunque esté vencido. La
  extracción de recuerdos en segundo plano no cuenta para el tope del dueño:
  hay una sola a la vez para todo el servicio.

## vasak-assistant 0.3.4

Un registro de errores completo y que no filtra datos (#29).

- **`daemon`** — cada llamada D-Bus que termina en error deja una línea con el
  método, quien llama, la conversación en corto (8 caracteres) y el tipo de
  error; en `debug`, también las que salen bien, con su duración. Los pedidos a
  los backends, la extracción y la poda de la memoria, el llavero y la puerta de
  AppArmor registran sus fallas y su ciclo de vida (arranque con la versión y un
  resumen de la configuración, parada, encender, apagar y borrar la memoria).
  Nunca se registra el contenido de los mensajes ni de las respuestas, las
  notas, los recuerdos, las claves ni una dirección con credenciales o
  consulta (`logging::redact_url`); de un pedido a un backend va la operación y
  no la ruta, de un error de la base de la memoria sólo su código y de un
  `vasak.conf` ilegible la línea, la columna y la clase. Si la memoria no se
  puede abrir (llave ausente, bloqueada, duplicada o inválida, llavero caído),
  queda una línea la primera vez y cada vez que cambia el motivo. Sin colores ni
  hora propia cuando la salida va al journal.
- **`packaging`** — la unidad dice `StandardError=journal`.

## vasak-assistant 0.3.3

- **`daemon`** — borrar una conversación (`DeleteConversation`) borra su resumen
  de la memoria y cancela la extracción que tuviera en curso (#12), si la
  memoria está encendida y se puede abrir; apagada o con el llavero bloqueado,
  el resumen queda (#28). La clave del resumen incluye la conversación, así dos
  resúmenes iguales no se pisan.

## vasak-assistant 0.3.2

- **`daemon`** — tokio 1.54.1, que corrige una carrera en `tokio::time::Interval`
  (alerta de Dependabot).

## vasak-assistant 0.3.1

- **`daemon`** — una llamada sin remitente se rechaza con `AccessDenied` en vez
  de quedar con el dueño vacío `""` (#4): con varios pares punto a punto, todos
  habrían compartido las mismas conversaciones. La puerta de AppArmor también
  falla cerrado sin remitente o sin bus que lo respalde.

## vasak-assistant 0.3.0

La memoria de Lince (fase 4 de #1), apagada por omisión, y la clave del
proveedor en el esquema protegido del llavero.

- **`protocol`** — `MemoryStatus` (`(bbst)`: encendida, disponible, motivo y
  bytes) y tres métodos nuevos en el proxy: `MemoryStatus()`,
  `SetMemoryEnabled(enabled)` y `ClearMemory()`. `Capabilities` informa
  `memory` disponible cuando está encendida y abierta.
- **`daemon`** — la memoria (`memory`): una base de grafos Grafeo embebida,
  cifrada con AES-256-GCM (archivo y WAL) en `$STATE_DIRECTORY/memory/`, con la
  llave de 32 bytes en el llavero. Encender reabre la base que haya o, si no
  hay, genera la llave sólo con el llavero desbloqueado, la relee y crea la base
  en `memory.tmp/` antes de renombrarla. Apagar no borra nada: deja de usarla.
  Borrar (`ClearMemory`) saca primero la llave y después los archivos, y con el
  llavero bloqueado no toca nada. Al arrancar nunca se genera ni se borra nada
  solo. No guarda datos que no son suyos (correo, calendario, contactos,
  archivos, cuentas, claves).
  - El esquema (`memory/schema`): nodos `:Memory` de siete clases
    (preferencias, estilo, hábitos, temas, correcciones, alias y resúmenes de
    conversación), con topes por clase, peso que se refuerza y decae, aristas
    `ABOUT` a temas, índices de propiedad (`id`, `key`) y de texto (`terms`).
  - La lectura (`memory/recall`): antes de cada mensaje, hasta 4 KiB de notas
    como mensaje `system` en el contexto de ese turno, nunca en el historial.
  - La escritura (`memory/learn`): después de responder, si el turno tiene
    señales, el modelo extrae recuerdos en JSON en segundo plano; se validan y
    se guardan con deduplicación. Topes de 4 MiB con poda.
  - Los filtros (`memory/filter`): nada con secretos ni con datos de la persona.
  - Con un modelo remoto, la memoria no se lee ni se escribe salvo
    `share_memory_with_remote` en `vasak.conf`. Se decide en cada pedido con
    el modelo efectivo, así que un `:cloud` que Ollama elige por omisión
    también cuenta como remoto.
- **`daemon/keyring`** — busca y guarda por un mapa de atributos y suma el
  borrado de ítems. La clave del proveedor y la llave de la memoria se buscan y
  se crean con el mismo mapa, con `xdg:schema=ar.net.vasak.os.Assistant`
  (protegido por vasak-keyring#55) y `purpose`. No hay migración: un ítem sin
  el esquema no se cuenta ni se usa.
- **`daemon/service`** — puerta de AppArmor (#7): un llamador confinado recibe
  `AccessDenied` en `SetMemoryEnabled`, `ClearMemory`, `SetProviderKey` y
  `HasProviderKey`, y chatea sin memoria. `protocol` suma
  `Error::AccessDenied`.
  Una etiqueta vacía cuenta como confinada. Límite: la puerta mira sólo a
  quien llama, y el perfil `vasak-appimage` todavía deja lanzar un proceso sin
  confinar que la pasa (se cierra en Vasak-OS/vasak-desktop-settings#13).
- **`daemon/memory`, endurecimiento** — no se abre una base sin cifrar ni con
  restos de una migración de Grafeo; las notas se filtran contra inyección
  (NFKC, caracteres de formato, marcadores de rol, comillas, credenciales) y
  van escapadas entre delimitadores con nonce; la ventana de extracción
  descarta mensajes con secretos o datos y el mensaje de la persona tiene
  prioridad en el presupuesto; una época descarta lo extraído si la memoria
  se encendió, apagó o borró en el medio, y no se aprende de lo anterior al
  último encendido.
- El binario se compila con `lto = "fat"` y `codegen-units = 1`.
- **`packaging`** — la unidad tiene `StateDirectory=vasak-assistant` (modo
  0700), `UMask=0077` y `MemoryMax=512M`.

## vasak-assistant 0.2.1

Un modelo «cloud» de Ollama se trata como remoto (#14).

- **`daemon`** — un modelo de Ollama con sufijo `:cloud` o `-cloud` corre en
  ollama.com aunque se le hable al Ollama local, así que la conversación sale de
  la máquina. Ahora cuenta como remoto y necesita `allow_remote`; no se le exige
  https ni al loopback, que el salto a la nube lo cifra Ollama. Vale también para
  el modelo elegido por omisión cuando resulta ser uno `cloud`: sin
  `allow_remote` no se usa, y el estado lo muestra como remoto.

## vasak-assistant 0.2.0

La clave del proveedor se guarda por el servicio, para la pantalla «Asistente»
de Configuración (fase 3 de #1).

- **`protocol`** — dos métodos nuevos en el proxy: `SetProviderKey(url, key)` y
  `HasProviderKey(url)`.
- **`daemon`** — `SetProviderKey` valida la dirección (`normalize_url`, la misma
  forma con la que después se busca la clave; un remoto sólo por https) y la
  clave, y la guarda en el llavero con `CreateItem`, reemplazando la anterior.
  Con el llavero bloqueado contesta `Unavailable` y no abre ningún diálogo.
  `HasProviderKey` dice si hay una clave utilizable para esa dirección (con más
  de una, `false`, igual que la lectura). La clave nunca va al registro ni a un
  mensaje de error.

## vasak-assistant 0.1.0

Primera versión: el servicio y el contrato (fase 1 de #1), con parte de la
fase 2. Conversación con un modelo —local por omisión, o un proveedor remoto si
la persona lo permite— sin ninguna capacidad sobre datos del usuario.

- **`protocol`** — el contrato D-Bus (`ar.net.vasak.os.Assistant`): las
  direcciones del bus, los tipos que viajan (`Message`, `Role`, `ModelInfo`,
  `ServiceStatus`, `Capability`), los proveedores, los dominios, los límites,
  los errores con nombre propio (`ar.net.vasak.os.Assistant.Error.*`) y el
  proxy `Assistant`.
- **`daemon`** — el servicio:
  - El motor de conversación (`conversations`): historial en memoria por
    conversación, con topes de tamaño de mensaje, de cantidad de conversaciones
    y de historial.
  - La interfaz común de los backends de modelo (`backend`) y dos backends:
    Ollama (`ollama`) y uno genérico compatible con la API de OpenAI
    (`openai`). Lo común de HTTP (`http`): tiempos máximos, tope de tamaño de
    respuesta, sin redirecciones y sin proxy; https con rustls.
  - La configuración (`config`), en la sección `assistant` de `vasak.conf`:
    backend, dirección, modelo y `allow_remote`. Local o remoto sale de la
    dirección; un remoto exige permiso explícito, https y modelo.
  - La clave del proveedor (`keyring`), leída de `vasak-keyring` por la API del
    Secret Service y atada a la dirección del backend. Nunca en disco, en el
    entorno ni en el registro.
  - La interfaz D-Bus (`service`): `Status` (con `model.local` y
    `model.host` para el indicador de «esto sale de la caja»), `Capabilities`
    (las de datos del usuario, no disponibles; `remote_backends` según
    `allow_remote`), `Models`, `NewConversation`, `Conversations`,
    `History`, `Chat` y `DeleteConversation`.
- **`packaging`** — la unidad de usuario de systemd y la activación por D-Bus.
