# Plugin SDK — ABI C v1

Plugins Reclipse são bibliotecas dinâmicas (`cdylib` em Rust) que exportam uma entrada C estável. O host não compartilha tipos Rust, traits ou allocators com o plugin: apenas structs `#[repr(C)]` e strings JSON.

## Por que C ABI?

O ABI do Rust não é estável entre versões do compilador. Um plugin compilado com outro rustc poderia corromper a memória se trocássemos `Box<T>`/traits. Uma fronteira C com ownership explícito é estável, testável e permite plugins em outras linguagens no futuro.

## Estruturas

```rust
pub const ABI_VERSION: u32 = 1;

#[repr(C)]
pub struct HostApiV1 {
    pub abi_version: u32,
    pub log: unsafe extern "C" fn(*const c_char),
    pub free_string: unsafe extern "C" fn(*mut c_char),
}

#[repr(C)]
pub struct PluginV1 {
    pub abi_version: u32,
    pub id: *const c_char,
    pub name: *const c_char,
    pub version: *const c_char,
    pub init: Option<unsafe extern "C" fn(*const HostApiV1) -> i32>,
    pub shutdown: Option<unsafe extern "C" fn()>,
    pub invoke: Option<unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_char>,
    pub free_string: Option<unsafe extern "C" fn(*mut c_char)>,
}

pub type PluginEntryFn = unsafe extern "C" fn() -> *const PluginV1;
```

Símbolo exportado obrigatório:

```rust
#[no_mangle]
pub extern "C" fn reclipse_plugin_entry_v1() -> *const PluginV1
```

## Ciclo de vida

1. O host varre `reclipse_plugin_*.dll` (ou `.so`/`.dylib`) em:
   - diretório do executável;
   - `./plugins` no diretório atual;
   - caminhos de `RECLIPSE_PLUGIN_PATH` separados por `;`.
2. `dlopen` + resolução de `reclipse_plugin_entry_v1`.
3. Validação: descritor não nulo, `abi_version == 1`, `id`, `invoke` e `free_string` presentes.
4. `init(&HostApiV1)` é chamado; retorno diferente de zero rejeita o plugin.
5. `invoke("list_commands", "{}")` deve retornar um JSON `{"commands":[...]}`.
6. Comandos aparecem na command palette.
7. No drop do host, `shutdown()` é chamado antes do `dlclose`.

Plugins duplicados por `id` são ignorados. Erros de carregamento ficam em `PluginHost::load_errors()`.

## Protocolo JSON

### Listagem

Requisição: `invoke("list_commands", "{}")`

Resposta:

```json
{
  "commands": [
    { "id": "hello.insert_timestamp", "title": "Hello: inserir timestamp" },
    { "id": "hello.file_stats", "title": "Hello: estatísticas do arquivo ativo" }
  ]
}
```

### Execução

Requisição: `invoke("hello.file_stats", context_json)`

Contexto:

```json
{
  "active_file": "C:\\projetos\\demo\\src\\Main.java",
  "project_root": "C:\\projetos\\demo",
  "selection": "texto selecionado ou null"
}
```

Resposta (`PluginResponse`):

```json
{
  "message": "Main.java: 120 linhas, 3400 bytes",
  "insert_text": null,
  "replace_selection": false,
  "open_file": null,
  "line": null
}
```

Campos:

| Campo | Tipo | Efeito |
|---|---|---|
| `message` | string? | toast na barra de status |
| `insert_text` | string? | inserido no cursor da aba ativa |
| `replace_selection` | bool | se true, substitui a seleção |
| `open_file` | string? | abre um caminho |
| `line` | number? | linha 1-based ao abrir |

Comandos desconhecidos devem retornar uma resposta com `message` explicativa, não entrar em pânico.

## Ownership de strings

Regra de ouro: **quem aloca, libera**.

- O plugin aloca o retorno de `invoke` (por exemplo, `CString::into_raw`) e o host o libera chamando o `free_string` **do próprio plugin**.
- O host aloca strings de serviços do host (futuro) e o plugin as libera com `HostApiV1::free_string`.
- Nunca use `CString::from_raw` em uma string alocada pelo outro lado.

O helper `reclipse_plugin_api::to_c_string` aloca; `read_c_string` lê; `take_c_string` assume ownership (use apenas quando o allocator for o mesmo).

## Segurança e pânicos

- `init`, `shutdown` e `invoke` são `extern "C"`: um panic que atravesse a fronteira aborta o processo. Plugins **devem** ser panic-free nesses pontos; use `catch_unwind` internamente se necessário.
- Não bloqueie a thread de UI em comandos demorados nesta versão.
- Plugins nativos têm poder total do processo. Instale apenas código confiável. WASM com capabilities é o caminho para isolamento.

## Exemplo mínimo

```rust
use std::ffi::{c_char, CString};
use reclipse_plugin_api::{
    read_c_string, to_c_string, CommandInfo, CommandList, PluginResponse, PluginV1, ABI_VERSION,
};

const ID: &[u8] = b"acme.hello\0";
const NAME: &[u8] = b"Acme Hello\0";
const VERSION: &[u8] = b"1.0.0\0";

struct Descriptor(PluginV1);
unsafe impl Sync for Descriptor {}

static PLUGIN: Descriptor = Descriptor(PluginV1 {
    abi_version: ABI_VERSION,
    id: ID.as_ptr() as *const c_char,
    name: NAME.as_ptr() as *const c_char,
    version: VERSION.as_ptr() as *const c_char,
    init: None,
    shutdown: None,
    invoke: Some(invoke),
    free_string: Some(free_string),
});

#[no_mangle]
pub extern "C" fn reclipse_plugin_entry_v1() -> *const PluginV1 {
    &PLUGIN.0
}

unsafe extern "C" fn invoke(command: *const c_char, _args: *const c_char) -> *mut c_char {
    let command = unsafe { read_c_string(command) }.unwrap_or_default();
    let json = if command == "list_commands" {
        serde_json::to_string(&CommandList {
            commands: vec![CommandInfo {
                id: "acme.hello".into(),
                title: "Acme: dizer olá".into(),
            }],
        }).unwrap()
    } else {
        serde_json::to_string(&PluginResponse {
            message: Some("Olá do plugin!".into()),
            ..Default::default()
        }).unwrap()
    };
    to_c_string(&json)
}

unsafe extern "C" fn free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}
```

`Cargo.toml`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
reclipse-plugin-api = { path = "../../crates/reclipse-plugin-api" }
serde_json = "1"
```

Compile com `cargo build -p seu-plugin`. O `.dll` resultante deve ficar ao lado de `reclipse.exe` (ou em `plugins/`).

## Versionamento

- `ABI_VERSION` só muda com quebra de layout ou semântica.
- Novos serviços entram em structs novas (`HostApiV2`) e o host negocia a maior versão suportada.
- Campos existentes nunca mudam de posição/tipo dentro da mesma versão.
- O descritor `PluginV1` fica congelado; extensões vão em `PluginV2`.

## Roadmap do SDK

- ABI v2 assíncrona (comandos com `request_id` e callback/evento).
- Serviços de host: documento ativo, workspace, edição, notificações, UI.
- Manifesto `reclipse-plugin.toml` com permissões.
- Templates `cargo generate` e testes de contrato no CI.
- Runtime WASM (wasmtime + component model) para plugins de terceiros.
- Assinatura e verificação de integridade para plugins nativos.
