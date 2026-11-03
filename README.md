# Yeison

Dock de chat para macOS. Se queda junto al borde superior de la pantalla, habla con los CLIs de IA que ya tengas instalados y arma un índice del proyecto (el Brain) para que cada pregunta lleve el contexto que hace falta.

El paquete de Cargo se llama `hola_mundo`.

## Qué hace

- **Isla del notch.** Mientras Yeison está abierto, un robot pequeño queda debajo del notch. Un clic abre el chat. El robot cambia de estado: espera, escaneando, pensando, trabajando, sincronizando, alerta, error y listo.
- **Dock.** Un panel de unos 8,5 × 11 cm. Pasa el cursor por la franja superior de la pantalla para abrirlo. Un clic afuera lo cierra. Puede ocupar toda la pantalla; el contenido queda centrado.
- **Proyectos.** Eliges la carpeta del repositorio y, si quieres, otra carpeta para el Brain. Puedes tener **dos proyectos abiertos a la vez**, cada uno con su Brain y sus chats. Un botón **Transferir contexto** copia documentos precargados del Brain de uno al `context/` del otro, sin llamar a la IA ni gastar tokens.
- **Chats.** Varias conversaciones a la vez por proyecto, cada una con su modelo. Hasta cinco se ven como pestañas. A partir de la sexta, una flecha abre la lista. Las respuestas siguen llegando aunque estés en otra pestaña o en el otro proyecto.
- **Planes.** El botón Plan pide un plan de trabajo estructurado. Se muestra en pantalla con pasos marcables. Después podés elegir con qué IA o agente desarrollarlo.
- **Markdown.** Las respuestas se muestran con títulos, listas, código, citas, tablas y tareas.
- **Brain.** Índice incremental del código, sincronizado con Git. Al empezar una sesión revisa la rama y el último escaneo. Si hubo cambios, actualiza solo lo que cambió. El overview es documentación general. El código en disco manda sobre cualquier descripción guardada.

## Requisitos

- macOS
- Herramientas de línea de comandos de Xcode (`clang`, `git` y el SDK)
- Rust 1.85 o más nuevo, con Cargo (el proyecto usa la edición 2024)
- Los CLIs de IA que quieras usar, ya instalados y con su propia sesión. Yeison los detecta en el PATH. Entre los conocidos están Cursor, OpenCode, Claude Code, Gemini CLI, Codex CLI, Aider, Ollama y `llm`.

## Instalación

Desde la raíz del repositorio:

```bash
./instalar.sh
```

El script lista lo que ya está y lo que falta, e instala en orden solo lo que no encuentra: herramientas de Xcode y después Rust. Si ya está todo, termina con:

```text
Ya puede ejecutar el proyecto con el comando cargo run
```

## Uso

```bash
cargo run
```

Pruebas:

```bash
cargo test --bin hola_mundo
```

Al abrirlo, elige o crea un proyecto, espera el escaneo de las IA instaladas y entra al chat. Cambiar de modelo vale solo para la conversación activa. Podés abrir un segundo proyecto sin cerrar el primero; cada uno conserva sus chats. Con dos abiertos, **Transferir contexto** pasa overview, índice, mapa y fichas asentadas al Brain activo como archivos locales.

## Dónde guarda datos

| Qué | Dónde |
| --- | --- |
| Lista de proyectos | `~/Library/Application Support/hola_mundo/projects.json` |
| Catálogo de IA detectadas | `~/Library/Caches/hola_mundo/ai_catalog.json` |
| Brain de un proyecto | carpeta `brain_<nombre>` dentro del repo, o la que elijas al crearlo |

El Brain escribe, entre otros, `00_overview.md`, `BRAIN_INDEX.md`, `08_codebase_map.md` y una ficha por módulo. El estado del índice queda en `.brain_state.json`.

## Código

```text
src/main.rs           dock, chats, isla y pantalla
src/brain.rs          índice incremental y el prompt que se arma al preguntar
src/ai/               detección de CLIs y envío de mensajes
src/robot.rs          los ocho estados del robot
src/markdown.rs       cómo se pinta el Markdown
src/macos_overlay.rs  ventana por encima del resto, al nivel del notch
src/project.rs        registro de proyectos
instalar.sh           comprobación e instalación del entorno
```
