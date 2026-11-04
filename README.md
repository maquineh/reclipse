# Reclipse

**Uma IDE Java nativa em Rust.** O objetivo é ser a IDE Java mais rápida de todos os tempos: cold start baixo, memória previsível, indexação paralela e uma UI que não depende de SWT nem de um GC.

Este repositório contém o **MVP 0.1.0**, o **M1** do editor e a **fundação do M2** (classpath + semântica Java), funcionais e executáveis, além do plano técnico para chegar a uma IDE completa e a uma camada de compatibilidade com o ecossistema Eclipse.

> Projeto independente. Não é afiliado à Eclipse Foundation. Eclipse e JDT são marcas dos seus respectivos titulares.

![Reclipse MVP com o projeto demo aberto](docs/screenshot.png)

---

## O que já funciona

- Shell nativo em Rust com **egui/eframe** (GPU), tema escuro e layout de IDE.
- Abertura de pasta/projeto com descoberta de fontes Java:
  - Eclipse (`.project` + `.classpath`)
  - Maven (`pom.xml`, `src/main/java`, `src/test/java`)
  - Gradle (layout convencional)
  - projeto Java simples (fallback)
- Editor de código com:
  - buffer **rope** e renderização **virtualizada** (só linhas visíveis)
  - destaque de sintaxe Java incremental por linha
  - números de linha, linha atual, seleção, cursor piscante
  - **multi-cursor** (`Ctrl+Alt+↓/↑`, Alt+clique) e **seleção por coluna** (Alt+arrastar), com undo agrupado por rajada de digitação
  - **folding** por bloco, com marcador na gutter, atalhos e persistência na sessão
  - undo/redo agrupado, copiar/cortar/colar, autoindent
  - abas, indicador de alteração, salvar (`Ctrl+S`)
  - posição do cursor na barra de status
- **Busca e substituição** no arquivo (`Ctrl+F`/`Ctrl+H`) com regex (incluindo grupos `$1`), palavra inteira, destaques e navegação.
- **Busca no projeto** (`Ctrl+Shift+F`) em thread separada, com resultados clicáveis e substituição no projeto.
- **Classpath nativo** Eclipse/Maven/Gradle + JDK, com dependências transitivas e índice de classes de JARs/JMODs; o JDK é escolhido pelo target do projeto.
- **Open Type** inclui tipos do projeto e do classpath; **F3** em tipo externo abre o visualizador de metadados.
- **Completion inline** de membros (`Ctrl+Space` ou após `.`), com herança do classpath, filtro incremental e aceite por Enter/Tab.
- **Hover semântico** de tipos, variáveis e símbolos.
- **Referências textuais** no projeto (`Ctrl+Shift+G`).
- Diagnóstico de **imports não resolvidos** no painel de problemas.
- **File watcher**: recarga automática de arquivos limpos alterados no disco e banner de conflito para buffers sujos.
- **Recuperação de buffers sujos** após fechar/crash, com diálogo de recuperação.
- **Sessão persistente**: projeto, abas, cursor, folds, painéis, fonte e keybindings.
- **Keybindings no padrão Eclipse** (Open Resource, Open Type, Quick Access, Quick Outline, F3), configuráveis via `session.json`.
- Indexação paralela de arquivos Java com **tree-sitter-java + Rayon**:
  - pacote, imports, classes, interfaces, enums, records, anotações
  - métodos, construtores, campos, constantes de enum
  - erros de sintaxe
- Painéis de **Projeto**, **Estrutura (outline)** e **Problemas**.
- **Open Resource** (`Ctrl+Shift+R`) com busca fuzzy.
- **Quick Access** (`Ctrl+3`).
- **Ir para definição** (`F3`) no índice sintático e em tipos do classpath.
- **Plugins nativos em Rust** com ABI C versionada (`v1`) e comandos JSON.
- Plugin de exemplo (`hello-plugin`) que insere texto e mostra estatísticas do arquivo.

Busca no arquivo com destaque de todas as ocorrências:

![Busca no arquivo](docs/screenshot-search.png)

Conflito entre buffer sujo e alteração externa:

![Banner de conflito](docs/screenshot-conflict.png)

Código dobrado por bloco:

![Folding](docs/screenshot-folding.png)

Recuperação de buffers após uma sessão anterior:

![Recuperação de buffers](docs/screenshot-recovery.png)

100 abas abertas com sessão restaurada:

![100 abas](docs/screenshot-100tabs.png)

## O que ainda não existe

Este MVP é deliberadamente honesto sobre o escopo:

- Não compila, não roda testes e não depura projetos Java ainda.
- A semântica Java já cobre imports/tipos, completion de membros e hover, mas ainda não tem generics/overloads completos, find references semântico, rename ou refactors.
- Maven/Gradle resolvem dependências transitivas do repositório local; não há download, perfis Maven nem Tooling API/BOM completo do Gradle.
- Multi-cursor agrupa undo de digitação, mas backspace/delete multi ainda são por tecla.
- Não é um substituto completo do Eclipse para projetos corporativos.
- Plugins Eclipse/OSGi/SWT **não** são carregados ainda. A compatibilidade headless está no roadmap.
- Não há LSP embutido nesta versão.

Veja [docs/M1.md](docs/M1.md) para o detalhamento do editor, [docs/M2.md](docs/M2.md) para o classpath/semântica e [docs/ROADMAP.md](docs/ROADMAP.md) para os próximos marcos.

---

## Rodar

Pré-requisitos:

- Windows 10/11 (o alvo atual; o código é portável para Linux/macOS)
- Rust estável com toolchain MSVC
- Visual Studio Build Tools com “Desktop development with C++” (tree-sitter compila C)
- Nenhum JDK é obrigatório; se um for encontrado, o classpath do JDK é indexado automaticamente.

```powershell
cargo run -p reclipse-ui -- examples/demo-java
```

Build release:

```powershell
cargo build --release -p reclipse-ui
.\target\release\reclipse.exe examples\demo-java
```

Também é possível abrir a IDE sem argumentos e escolher **Arquivo → Abrir pasta…**.

## Atalhos (padrão Eclipse)

| Atalho | Ação |
|---|---|
| `Ctrl+Shift+R` | Open Resource (quick open de arquivos) |
| `Ctrl+Shift+T` | Open Type (tipos do índice) |
| `Ctrl+3` | Quick Access (command palette) |
| `Ctrl+O` | Quick Outline do arquivo ativo |
| `Ctrl+F` | Localizar no arquivo |
| `Ctrl+H` | Substituir no arquivo |
| `Ctrl+Shift+F` | Buscar no projeto |
| `Ctrl+K` / `Ctrl+Shift+K` | Próxima / anterior ocorrência |
| `Ctrl+Space` | Completion de tipos/membros |
| `Ctrl+Shift+G` | Referências textuais no projeto |
| `Ctrl+Shift+L` | Ajuda de atalhos |
| `F3` | Ir para definição |
| `F5` | Reindexar |
| `Ctrl+S` | Salvar |
| `Ctrl+W` | Fechar aba |
| `Ctrl+Alt+O` | Abrir arquivo |
| `Ctrl+Shift+O` | Abrir pasta/projeto |
| `Ctrl+Alt+↓/↑` | Adicionar cursor abaixo/acima |
| `Alt+clique` / `Alt+arrastar` | Adicionar cursor / seleção por coluna |
| `Ctrl+Z` / `Ctrl+Y` | Desfazer / refazer no editor |
| `Esc` | Fechar palette/busca ou reduzir cursores |

Os atalhos de arquivo, navegação, busca e projeto podem ser redefinidos em `%APPDATA%\Reclipse\session.json` (ou `~/.config/reclipse/session.json`). Atalhos de edição de texto permanecem fixos nesta versão.

## Sessão e configuração

A IDE salva projeto, abas, cursor, visibilidade dos painéis, tamanho da fonte e keybindings ao sair. Abra **Ajuda → Abrir configuração (session.json)** para editar. Alterações de keybinding exigem reinício; edições externas de keybindings são preservadas ao fechar a IDE.

Buffers sujos são gravados em backups para recuperação; a sessão guarda abas, cursor, folds, painéis e keybindings.

## Plugins Rust

O plugin de exemplo já é carregado automaticamente quando compilado:

```powershell
cargo build -p reclipse-plugin-hello
cargo run -p reclipse-ui -- examples/demo-java
```

O host procura por `reclipse_plugin_*.dll` (ou `.so`/`.dylib`) no diretório do executável, em `./plugins` e em `RECLIPSE_PLUGIN_PATH`. Abra `Ctrl+3` (Quick Access) e procure por:

- `Hello: inserir timestamp`
- `Hello: estatísticas do arquivo ativo`

![Command palette com comandos do hello-plugin](docs/screenshot-palette.png)

Detalhes do ABI, ownership de strings, ciclo de vida e versionamento: [docs/PLUGIN_SDK.md](docs/PLUGIN_SDK.md).

## Estrutura do repositório

```text
crates/
  reclipse-core/        descoberta de projetos e modelo de workspace
  reclipse-classpath/   classpath Eclipse/Maven/Gradle + índice de JARs/JMODs
  reclipse-java/        parser tree-sitter, índice de símbolos e semântica inicial
  reclipse-editor/      documento, cursor, lexer de destaque
  reclipse-plugin-api/  ABI C v1 e protocolo JSON
  reclipse-plugin-host/ carregamento e despacho de plugins
  reclipse-ui/          aplicativo egui/eframe (binário `reclipse`)
plugins/
  hello-plugin/         plugin nativo de exemplo (cdylib)
examples/
  demo-java/            projeto Java para experimentar
docs/
  ARCHITECTURE.md       arquitetura e metas de performance
  M1.md                 editor rope, busca, watcher, sessão e keybindings
  M2.md                 classpath nativo, índice de classes e semântica inicial
  ROADMAP.md            marcos, estimativas e critérios
  PLUGIN_SDK.md         guia do ABI de plugins
  ECLIPSE_COMPAT.md     estratégia realista para plugins Eclipse
```

## M1 — Editor de produção

O marco M1 substituiu o `TextEdit` por um editor próprio com rope, virtualização de linhas e lexer incremental. Também adicionou busca/substituição (literal, regex e palavra inteira), busca/substituição no projeto, watcher com conflitos, sessão persistente, keybindings no padrão Eclipse, folding, multi-cursor, seleção por coluna e recuperação de buffers sujos.

Detalhes técnicos, métricas e limitações: [docs/M1.md](docs/M1.md).

## M2 — Motor Java (completion e hover entregues)

O M2 cobre classpath Eclipse/Maven/Gradle/JDK com dependências transitivas, escolha de JDK pelo target, índice nativo de classes de JARs/JMODs, Open Type com tipos externos, F3 abrindo metadados do classpath e diagnóstico de imports não resolvidos.

A camada semântica adiciona inferência de variáveis (inclusive generics e arrays), completion de membros inline com herança do classpath, hover de tipos/variáveis e referências textuais. Ainda faltam rename seguro, referências semânticas, generics/overloads completos e diagnósticos do javac/JDT. Plano e limitações: [docs/M2.md](docs/M2.md).

![Classpath e índice de tipos](docs/screenshot-classpath.png)

Completion de membros de `MessageService` após `app.service.`:

![Completion de membros](docs/screenshot-completion.png)

Hover semântico de um tipo do projeto:

![Hover semântico](docs/screenshot-hover.png)

Open Type com a query `java.lang.String` e tipos do classpath:

![Open Type com classpath](docs/screenshot-opentype.png)

Visualizador de metadados de um tipo externo:

![Tipo externo java.lang.String](docs/screenshot-classpath-viewer.png)

## Performance

Metas de engenharia (a medir em cada release, não promessas do MVP):

| Métrica | Meta |
|---|---|
| Cold start (até primeira janela) | < 150 ms |
| Abrir projeto de 1M LOC | < 2 s para o índice sintático |
| Latência de tecla | < 16 ms (p95) |
| Reindexação incremental | < 50 ms por arquivo |
| Memória em repouso (projeto médio) | < 300 MB |
| Memória em projeto grande | < 800 MB |

O editor do M1 usa rope + widget virtualizado próprio, então apenas as linhas visíveis são layoutadas. O frame benchmark mede o `update` completo do egui; ainda faltam “select next occurrence” e column mode por teclado.

Medição local de referência (Windows 10, i5-4590, 4 núcleos, build release):

| Cenário | Resultado |
|---|---|
| Abrir arquivo de 100k linhas | 12 ms |
| Lexer completo das 100k linhas | 152 ms |
| 1.000 edições no arquivo de 100k linhas | 1 ms |
| Abrir 100 abas (1k linhas cada, 3,58 MB) | 18 ms |
| Indexar 2.000 arquivos Java / 26.000 símbolos | 212 ms |
| Frame CPU p95 com 100k linhas e digitação | 6,04 ms |
| Frame wall p95 com 100k linhas (vsync) | 20,92 ms |
| Índice do JDK 25 (70 JMODs) | 26.862 classes / ~1,7 s |

O frame benchmark usa uma janela focada e mede o `update` completo do egui; wall time inclui vsync. CPU p95 é a métrica de trabalho da UI.

Reproduza com `cargo run --release -p reclipse-editor --example bench_editor` e `cargo run --release -p reclipse-java --example bench_index -- <pasta>`. Esses números medem o núcleo (rope, lexer e documentos); o frame benchmark da UI está descrito em [docs/M1.md](docs/M1.md). Uma sessão com 100 abas foi aberta e validada por screenshot.

## Licença

MIT. Consulte [docs/ECLIPSE_COMPAT.md](docs/ECLIPSE_COMPAT.md) para as fronteiras legais e técnicas em relação ao código EPL do Eclipse.
