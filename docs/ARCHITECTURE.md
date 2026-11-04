# Arquitetura do Reclipse

## 1. Visão

Reclipse é uma IDE Java nativa. O caminho crítico — abrir a janela, editar, indexar, navegar — roda em Rust, sem JVM no processo principal e sem SWT. O ecossistema Eclipse é tratado como uma **fronteira de compatibilidade opcional**, não como base da arquitetura.

Objetivos:

1. **Latência previsível**: sem pausas de GC no processo de UI.
2. **Paralelismo agressivo**: parsing e indexação em Rayon; I/O fora da thread de UI.
3. **Incrementalidade**: cada camada (editor, parser, índice, resolução) atualiza apenas o que mudou.
4. **Extensibilidade segura**: plugins nativos versionados hoje; WASM como evolução.
5. **Compatibilidade pragmática**: metadados de projeto Eclipse hoje; bundles headless via sidecar JVM no futuro.

Não objetivos do MVP: reimplementar o SWT, rodar plugins Eclipse de UI sem alteração, substituir o JDT em precisão semântica no dia 1.

## 2. Metas mensuráveis

| Métrica | Orçamento | Como medir |
|---|---|---|
| Cold start | < 150 ms | processo → primeira frame |
| Projeto 1M LOC, índice sintático | < 2 s | 8 cores, SSD, `demo` sintético |
| Latência de tecla p95 | < 16 ms | gravação de input + frame times |
| Reindexação de 1 arquivo | < 50 ms | edição + save em arquivo 1k LOC |
| Memória em repouso | < 300 MB | projeto médio, 50k LOC |
| Memória em projeto grande | < 800 MB | 1M LOC, índice em RAM |

Cada release deve publicar números; metas sem medição são apenas intenção.

## 3. Princípios

- **UI thread nunca bloqueia em I/O.** Descoberta, parsing e indexação vivem em threads de trabalho e conversam por canais.
- **Nenhum lock no caminho de digitação.** O editor é dono do buffer; o índice é uma cópia imutável consultável.
- **Formato de dados estável entre camadas.** O ABI de plugins usa C + JSON, não tipos Rust internos.
- **Falhas degradam, não derrubam.** Um arquivo inválido gera problema de sintaxe; um plugin malformado é rejeitado.
- **Processos isolam risco.** Código legado/JVM e, futuramente, plugins não confiáveis podem rodar fora do processo da IDE.

## 4. Mapa de crates

```text
reclipse-ui  ──────────────► reclipse-java ──► reclipse-classpath ──► reclipse-core
     │                            │                    │
     │                            └──────────► tree-sitter-java
     ├──────────────► reclipse-editor
     ├──────────────► reclipse-plugin-host ──► reclipse-plugin-api
     └──────────────► egui / eframe / rfd
```

| Crate | Responsabilidade | Depende de |
|---|---|---|
| `reclipse-core` | descoberta de fontes, modelo de projeto, arquivos | walkdir, dunce |
| `reclipse-classpath` | classpath Eclipse/Maven/Gradle/JDK, parser de `.class`, índice de JARs/JMODs | zip, core |
| `reclipse-java` | parse Java, extração de símbolos, índice paralelo, semântica de imports/tipos | tree-sitter, Rayon, core, classpath |
| `reclipse-editor` | documento, cursor, lexer de highlighting | std |
| `reclipse-plugin-api` | structs `#[repr(C)]`, protocolo JSON | serde |
| `reclipse-plugin-host` | dlopen/dlclose, validação de ABI, despacho | libloading, api |
| `reclipse-ui` | estado da aplicação, painéis, comandos | todos |
| `hello-plugin` | exemplo de cdylib | plugin-api |

## 5. Fluxo de dados

```text
                 ┌────────────────────────────┐
 Abrir pasta ───►│ discover_project (thread)  │
                 │  .classpath / pom / gradle │
                 └─────────────┬──────────────┘
                               │ Project { source_roots, java_files, all_files }
                               ▼
                 ┌────────────────────────────┐
                 │ build_index (Rayon)        │
                 │  parse_source_with(parser) │
                 └─────────────┬──────────────┘
                               │ IndexMessage::{Progress,Done,Failed}
                               ▼
┌──────────────┐    canal mpsc    ┌─────────────────────┐
│ UI thread    │◄─────────────────│ worker de indexação │
│ egui/eframe  │                  └─────────────────────┘
│              │    canal mpsc    ┌─────────────────────┐
│              │◄─────────────────│ worker de classpath │
└──────┬───────┘                  │ resolve + JAR/JMOD  │
       │                          └─────────────────────┘
       │ clique/atalho
       ▼
┌──────────────┐   Document::open   ┌──────────────┐
│ Tab/Editor   │───────────────────►│ reclipse-    │
│ TextEdit     │◄───────────────────│ editor       │
└──────┬───────┘   save/update_file └──────────────┘
       │
       ▼
┌──────────────────────┐  ABI C v1 + JSON  ┌──────────────────┐
│ PluginHost           │◄─────────────────►│ plugin cdylib    │
│ libloading + dispatch│                   │ (Rust)           │
└──────────────────────┘                   └──────────────────┘
```

### Ciclo de vida da indexação

1. `start_project` limpa o estado e dispara `spawn_index(root)`.
2. A thread descobre o projeto (sem JVM), cria o `Project` e chama `build_index`.
3. `build_index` divide `java_files` com Rayon; cada worker mantém um `Parser` tree-sitter reutilizável.
4. Progresso é enviado a cada 16 arquivos (throttle) para não inundar o canal.
5. A UI recebe `Done` e então:
   - publica o índice imutável;
   - mescla o conteúdo de abas Java sujas (para o outline refletir edições não salvas);
   - constrói `ProjectTypes` (FQN, localização e nomes simples);
   - dispara `spawn_classpath(project)` em outra thread;
   - abre o primeiro `.java` se não houver abas.
6. O worker de classpath resolve `.classpath`/POM/Gradle/JDK e indexa JARs/JMODs. Ao terminar, a UI calcula imports não resolvidos e habilita Open Type/F3 externos.

O índice sintático é reconstruído por completo nesta versão. A próxima evolução mantém um mapa de dependências e atualiza apenas o arquivo alterado e seus dependentes.

## 6. UI e editor

- **egui/eframe** com backend glow. A UI é imediata; o estado persistente fica em `ReclipseApp`.
- **Palette** usa ranking fuzzy próprio, sem dependências, com bônus para prefixos, limites de palavra e matches consecutivos.
- **Editor (M1)**: widget próprio desenhado com `Painter` e `Galley`. O buffer é um `Rope` (ropey), e apenas as linhas visíveis no viewport são tokenizadas, layoutadas e pintadas.
- **Gutter** é pintado diretamente, com posição fixa durante o scroll horizontal; a linha atual, a seleção, o cursor e os matches de busca são desenhados como retângulos/linhas.
- **Highlight incremental**: lexer por linha com estados `Normal`, `BlockComment` e `TextBlock`. Uma edição dentro de uma linha recalcula o estado dali até convergir; inserção/remoção de linhas reaproveita o prefixo e desloca o sufixo, com fallback para recálculo.
- **Undo/redo** é transacional, baseado em deltas de texto, com agrupamento de digitação/backspace no cursor único e de rajadas de digitação em multi-cursor.
- **Folding** por blocos: marcadores na gutter, clique e atalhos (`Ctrl+Shift+[`/`]`/`0`/`9`), ranges não sobrepostos e folds persistidos na sessão.
- **Multi-cursor** e **seleção por coluna**: cursores/âncoras adicionais no `Document`, edição composta com um único registro de undo, pintura e navegação por todos os cursores.
- **Busca/substituição** no arquivo com case, palavra inteira e regex com expansão de grupos (`$1`, `${nome}`); busca no projeto em thread, com substituição confirmada, expansão de grupos e proteção de buffers sujos.
- **Watcher** (`notify`) observa a raiz e reconcilia disco × buffer: recarga automática se limpo, banner de conflito se sujo.
- **Sessão** persiste projeto, até 100 abas, cursor/âncora, folds, painéis, fonte e keybindings em `session.json`.
- **Recuperação** grava backups de buffers sujos em `backups/` a cada ~700 ms e oferece recuperação no próximo início.

Limitações do M1:

- backspace/delete em multi-cursor ainda agrupam por tecla; a digitação já agrupa por rajada;
- “select next occurrence” e column mode por teclado não existem;
- folds usam heurística por chaves, não parsing completo;
- IME não foi testado a fundo.

O M1 está concluído; o foco atual é o motor semântico Java (M2).

## 7. Motor Java

### Níveis de inteligência

| Nível | Capacidade | Estado |
|---|---|---|
| L0 | lexer, highlight, erros de sintaxe | ✅ MVP |
| L1 | índice de pacotes/tipos/membros, outline, go-to-def sintático | ✅ MVP |
| L2 | resolução de imports, classpath Maven/Gradle/Eclipse, tipos | 🔶 fundação M2 |
| L3 | inferência de tipos, generics, overload resolution, completion | 🔶 completion/hover básicos |
| L4 | análise incremental de projeto, refactors seguros | 🔜 M4+ |

### Classpath e índice de classes (M2)

`reclipse-classpath` resolve o classpath fora da UI thread:

- **Eclipse**: `<classpathentry kind="src">`, `kind="lib"` (JAR ou diretório), container JRE detectado à parte e avisos para entradas não resolvidas.
- **Maven**: dependências diretas e transitivas do `pom.xml`, propriedades (`${...}`, `project.*`, parent), `dependencyManagement`/BOMs importados, escopos compile/runtime/provided, `optional` e exclusions; repositório local (`M2_REPO` ou `~/.m2/repository`); `sourceDirectory`/`testSourceDirectory`.
- **Gradle**: coordenadas `group:artifact:version` (incluindo sintaxe de mapa) em `build.gradle`/`.kts`, com transitivas via POMs do repositório Maven local, `platform(...)`/BOM e version catalogs (`libs.versions.toml`).
- **JDK**: `RECLIPSE_JDK`, `JAVA_HOME` e instalações comuns; JDK 9+ via `jmods/*.jmod`, JDK 8 via `rt.jar`; target detectado em Maven/Gradle/Eclipse e JDK exato (ou menor >= target) escolhido.
- **Watch**: mudanças em `pom.xml`, `.classpath`, `build.gradle(.kts)`, `libs.versions.toml` e prefs do JDT disparam recarga do classpath com debounce.
- **Fontes**: `source.rs` resolve `-sources.jar`/`src.zip` sob demanda; o visualizador lê o fonte do ZIP.
- **Fallback**: JARs em `lib/` e `libs/`.

O índice lê `.class` sem JVM (`classfile.rs`): constant pool, acesso, superclasse, interfaces, campos e métodos públicos/protegidos. Descritores são formatados para assinaturas legíveis e tipos FQN são expostos para o resolvedor. JARs usam `zip`; JMODs descartam o header `JM\x01\x00` e leem o ZIP interno. Classes duplicadas são deduplicadas, com preferência pela versão pública.

`reclipse-java::semantic` implementa a camada de consulta: `ProjectTypes`, resolução por import/pacote/wildcard/`java.lang`, diagnóstico de imports, inferência reversa de variáveis (inclusive generics/arrays), completion de membros com herança do classpath e hover de tipos/variáveis/símbolos. O parser também coleta `type_identifier` em uma segunda passada (inclusive dentro de corpos) para um índice de referências semânticas de tipos; a UI usa esse índice no `Ctrl+Shift+G` e cai para busca textual quando o símbolo não é um tipo.

`reclipse-java::semantic` consulta os dois índices:

- `ProjectTypes` indexa FQN, localização e nomes simples dos tipos do projeto;
- `resolve_type` tenta nome qualificado, import explícito, mesmo pacote, wildcard, `java.lang` e nome simples único;
- `unresolved_imports` gera diagnósticos com linha;
- `completion_type_names` alimenta o Open Type/completion de tipos.

Na UI, o status do classpath mostra archives/classes/tempo e o target; Open Type inclui tipos externos; F3 sobre um tipo do classpath abre um visualizador de metadados, com código-fonte quando `-sources.jar`/`src.zip` estão disponíveis.

Limitações: sem download, perfis Maven, Tooling API do Gradle, Javadoc HTML ou rename; referências semânticas cobrem tipos, não membros.

### Descoberta de fontes

`reclipse-core` entende:

- `.classpath` com `<classpathentry kind="src" path="...">`;
- Maven e Gradle por layout convencional;
- fallback para `src`, `source`, `java` ou a raiz;
- diretórios gerados/ignorados (`target`, `build`, `out`, `bin`, `node_modules`, `.git`, `.idea`, `.metadata`, `.gradle`).

Entradas de workspace como `/OutroProjeto/src` são ignoradas de propósito: abrir uma pasta não deve indexar projetos irmãos silenciosamente.

### Parsing e símbolos

- `tree-sitter-java` é tolerante a erros e incremental por natureza.
- Extraímos `package`, `import`, tipos (class/interface/enum/record/annotation), métodos, construtores, campos, constantes de enum e componentes de record.
- `container` guarda a cadeia de tipos (`Outer.Inner`), `depth` alimenta o outline.
- Nós `ERROR`/`MISSING` viram problemas de sintaxe com linha/coluna.
- O parser roda em paralelo com um `Parser` por worker (Rayon `map_init`).

### O que falta para “Java IDE de verdade”

1. **Classpath**: Eclipse/Maven/Gradle/JDK, JARs/JMODs e transitivas existem; faltam download, perfis Maven, Tooling API/version catalogs do Gradle e observação em tempo real.
2. **Resolução**: imports/tipos básicos existem; faltam escopos, herança, generics e overloads. É um compilador frontal, não um parser.
3. **Diagnósticos**: erros semânticos incrementais ou fallback controlado para `javac`/JDT.
4. **Completion/refactor**: completion de membros, hover e referências semânticas de tipos existem; rename e referências de membros ainda faltam.
5. **Build/test/debug**: integrar Maven/Gradle, JUnit e JDI/DAP.

A estratégia é construir L2/L3 nativos para o caminho quente e oferecer um **modo de precisão** opcional via JDT Language Server (LSP) enquanto o motor nativo amadurece. O modo de precisão é opt-in porque reidrata o custo de JVM que motivou o projeto.

## 8. Plugins

### Hoje: ABI C v1

- `#[repr(C)] PluginV1` com strings e function pointers.
- Comandos são JSON; o host não conhece tipos Rust do plugin.
- O plugin retorna strings alocadas por ele e o host chama `free_string` do próprio plugin — ownership simétrico, sem depender do alocador do processo.
- `init` recebe `HostApiV1` (log e free_string). Novos serviços entram em `HostApiV2`.
- O host valida `abi_version`, id, nome, `invoke` e `free_string` antes de ativar.
- Plugins rodam na thread de UI nesta versão. Comandos devem ser rápidos; trabalho longo deve ir para threads do plugin e retornar depois (API assíncrona é roadmap).

### Futuro: WASM + permissões

- Plugins de terceiros em WebAssembly (wasmtime/component model) com capabilities explícitas: ler arquivo, escrever arquivo, rede, processos.
- Plugins nativos assinados para integrações de alto desempenho.
- Manifesto (`reclipse-plugin.toml`) com id, versão, ABI, permissões e pontos de extensão.
- Hot reload seguro: desativar, descarregar, recarregar preservando comandos.

Detalhes: [PLUGIN_SDK.md](PLUGIN_SDK.md).

## 9. Compatibilidade Eclipse

### A verdade difícil

“Rodar plugins Eclipse sem mudanças” cobre universos diferentes:

1. **Metadados** (`.project`, `.classpath`, natures, builders) — barato e já parcialmente suportado.
2. **Plugins headless** (JDT core, builders, validadores, geradores) — viável via Equinox em uma JVM sidecar.
3. **Plugins de UI em SWT** (editores, views, wizards, perspectivas) — não viável de rodar dentro de uma UI egui sem reescrever a camada SWT ou embutir um display SWT completo. Isso anularia a proposta nativa.

A arquitetura recomendada é híbrida e opcional:

```text
┌──────────────────────────────┐        JSON-RPC/local socket
│ Reclipse (Rust)              │◄──────────────────────────────┐
│  UI, editor, índice nativo   │                               │
└──────────────┬───────────────┘                               │
               │ spawn (opcional)                              │
               ▼                                               │
┌──────────────────────────────┐        ┌──────────────────────┴─────┐
│ Equinox sidecar (JDK 21)     │◄──────►│ Eclipse 4.36 bundle pool    │
│  OSGi, extension registry    │        │ JDT core 3.42, builders    │
│  headless services only      │        └────────────────────────────┘
└──────────────────────────────┘
```

Fluxo:

1. Reclipse lê `.project`/`.classpath` e descobre natures/builders.
2. Se o usuário ativar “Compatibilidade Eclipse”, sobe um processo Java com Equinox e o bundle pool existente.
3. Um bundle-ponte (nosso) publica comandos/extension points via JSON-RPC.
4. A UI Rust mapeia editores/views para adaptadores nativos; plugins SWT puros são marcados como incompatíveis com explicação.
5. Falhas/OOM do sidecar derrubam apenas o sidecar; a IDE continua.

O JDT Language Server (LSP) é um atalho separado e mais simples para semântica: rode-o como processo e conecte um cliente LSP Rust. É útil como “modo de precisão”, mas não dá compatibilidade de plugins.

### Licença

O Eclipse é EPL-2.0 (copyleft fraco). A estratégia segura é:

- não copiar código EPL para crates MIT/Apache;
- distribuir JARs não modificados como componentes separados, com notices;
- comunicar por processo/protocolo, não por linking;
- revisar obrigações ao redistribuir um produto comercial.

Nada disso substitui aconselhamento jurídico.

## 10. Build, testes e debug (roadmap)

- **Build**: invocar wrappers Maven/Gradle com output estruturado; futuramente Tooling API via sidecar.
- **Testes**: descoberta JUnit 4/5, execução por arquivo/classe, console de resultados.
- **Debug**: JDWP/JDI atrás de um adaptador DAP; a UI fala DAP com o adaptador, mantendo o núcleo Rust limpo.
- **Formatting**: google-java-format/Eclipse formatter via sidecar ou implementação nativa futura.

## 11. Segurança

- Plugins nativos têm poder total do processo. O menu mostra a origem e a versão; instalação deve ser explícita.
- O sidecar JVM roda com o mesmo usuário; não é sandbox.
- WASM será o caminho para plugins não confiáveis, com capabilities e limites de memória/CPU.
- Nenhuma telemetria no MVP.

## 12. Estratégia de testes

- Unitários: descoberta de projeto, parser, índice, lexer, fuzzy match, protocolo de plugin.
- Integração: carregar o `hello-plugin` compilado e executar comandos de ponta a ponta.
- Golden tests: entradas Java representativas (records, sealed, text blocks, generics, anotações).
- Performance: benchmarks com projetos sintéticos de 10k, 100k e 1M LOC; orçamento no CI.
- Compatibilidade: fixtures Eclipse reais (`.classpath` com múltiplas fontes, output, containers).

## 13. Estado atual

| Camada | MVP |
|---|---|
| Janela, tema, menus, status | ✅ |
| Editor rope + virtualização + undo/redo | ✅ |
| Folding + multi-cursor + seleção por coluna | ✅ |
| Busca literal/regex/whole-word + substituição no projeto | ✅ |
| Watcher + conflito disco × buffer | ✅ |
| Recuperação de buffers sujos | ✅ |
| Sessão + keybindings padrão Eclipse | ✅ |
| Descoberta Eclipse/Maven/Gradle | ✅ |
| Índice sintático paralelo | ✅ |
| Outline, problemas, quick open | ✅ |
| Go-to-definition sintático | ✅ |
| Plugins Rust nativos (ABI v1) | ✅ |
| Classpath Eclipse/Maven/Gradle/JDK | 🔶 fundação |
| Índice de JARs/JMODs | ✅ |
| Resolução de imports/tipos + Open Type externo | ✅ |
| Inferência/completion/hover | 🔶 básicos |
| Referências semânticas de tipos | ✅ |
| Rename / referências de membros | ❌ |
| Fontes de JARs/JDK | ✅ |
| Build/test/debug | ❌ |
| LSP/JDT | ❌ |
| Equinox sidecar | ❌ |
| WASM plugins | ❌ |
