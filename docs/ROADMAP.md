# Roadmap

Estimativas em **semanas de uma pessoa** e assumindo um desenvolvedor sênior em Rust/Java. Com dois desenvolvedores em paralelo, o calendário encurta, mas as dependências entre marcos continuam.

## M0 — MVP nativo (concluído)

Escopo entregue:

- shell egui/eframe;
- editor com highlight Java, abas, save e cursor;
- descoberta de projeto Eclipse/Maven/Gradle/simples;
- índice sintático paralelo com tree-sitter;
- outline, problemas, quick open, command palette;
- go-to-definition sintático;
- ABI de plugins Rust v1 + plugin de exemplo;
- documentação de arquitetura, SDK e compatibilidade.

Critérios de aceite (todos atendidos):

- abrir `examples/demo-java` e ver 4 arquivos Java indexados;
- outline com classes, records, métodos e campos;
- `F3` sobre `User` navega para `User.java`;
- plugin `Hello` carregado e executável pela palette;
- build limpo em Windows/MSVC.

## M1 — Editor de produção (concluído)

Entregue:

- buffer rope (`ropey`) e edições com índices de caractere;
- editor virtualizado, desenhando apenas o viewport;
- highlight incremental por linha, com deslocamento de estado em inserção/remoção de linhas;
- undo/redo transacional, agrupando digitação no cursor único e rajadas de digitação em multi-cursor;
- autoindent, Tab/Shift+Tab, copiar/cortar/colar;
- folding por blocos com marcadores na gutter, atalhos (`Ctrl+Shift+[`/`]`/`0`/`9`) e persistência na sessão;
- multi-cursor (`Ctrl+Alt+↓/↑`, Alt+clique) e seleção por coluna (Alt+arrastar);
- busca/substituição no arquivo com case, palavra inteira e regex com expansão de grupos (`$1`, `${nome}`);
- busca e substituição no projeto, com confirmação, proteção de buffers sujos e expansão de grupos;
- file watcher com recarga automática e banner de conflito;
- recuperação de buffers sujos após fechar/crash;
- sessão persistente (projeto, até 100 abas, cursor, folds, painéis, fonte);
- keybindings configuráveis no padrão Eclipse (Open Resource, Open Type, Quick Access, Quick Outline, F3).

Aceite validado:

- 100k linhas: abertura 12 ms, lexer completo 152 ms, 1.000 edições 1 ms;
- 100 abas (3,58 MB) abertas em 18 ms e restauradas via sessão por screenshot;
- watcher recarregou arquivo limpo e mostrou conflito para buffer sujo;
- recuperação de buffer sujo exibida e exercitada;
- folding (mouse, teclado e sessão) e multi-cursor cobertos por UI/testes;
- frame benchmark: CPU p50 4,00 ms / p95 6,04 ms / p99 8,66 ms; wall p95 20,92 ms em 100k linhas;
- 58 testes no fechamento do M1 (78 na suíte atual com M2) e clippy limpo.

Limitações residuais (M1.1, sem bloquear M2):

- backspace/delete em multi-cursor ainda agrupam por tecla (digitação já agrupa por rajada);
- “select next occurrence” e column mode por teclado;
- replace no projeto ignora buffers sujos.

## M2 — Núcleo semântico Java (fechado no núcleo nativo)

Entregue:

- classpath Eclipse, Maven (direto + transitivo, parent, dependencyManagement/BOM, escopos, optional/exclusions) e Gradle (direto + transitivo, platform/BOM e version catalogs);
- JDK (`jmods`/`rt.jar`) escolhido pelo target do projeto (Maven/Gradle/Eclipse);
- watch de `pom.xml`/`.classpath`/`build.gradle`/version catalog para recarregar o classpath;
- índice nativo de classes de JARs/JMODs, sem JVM, com campos, métodos, superclasse e interfaces;
- fontes via `-sources.jar` e `src.zip`, abertas no visualizador de tipo externo;
- Open Type com tipos externos; F3 em tipo do classpath abre o visualizador de metadados;
- resolução de tipos por import, mesmo pacote, wildcard e `java.lang`;
- diagnóstico de imports, tipos e membros não resolvidos, `static imports` ambíguos e `@Override` inválido no painel Problemas;
- `javac` incremental em fundo ao salvar (com debounce e cancelamento);
- inferência de variáveis (linhas anteriores, generics e arrays);
- **completion de membros inline** por `.`/`Ctrl+Space`, com herança do projeto e do classpath e retorno encadeado com type arguments;
- overloads ranqueados por tipo com fallback de aridade;
- type variables com substituição (projeto e `Signature` de bytecode), bounds e `Object` como fallback conservador;
- static imports com precedência single-sobre-wildcard; ambiguidade genuína recusada e diagnosticada;
- **hover semântico** de tipos, variáveis e símbolos;
- **referências semânticas de tipos** e de **métodos/campos** (`Ctrl+Shift+G`), com method references e fallback textual;
- **rename conservador** de tipos, métodos, campos e variáveis locais (`Alt+Shift+R`), com rename de arquivo, imports e proteção de overrides/shadowing;
- inferência de `var` e hierarquia de tipos do projeto;
- **RECLIPSE Studio**: 3 temas comutáveis, activity bar, toolbar, breadcrumb e console de diagnóstico.

Pendente (refinamento, fora do núcleo):

- overloads finos por conversão de narrowing/varargs além das 3 fases JLS;
- move de um tipo individual entre pacotes e atualização de Javadoc/recursos no rename;
- JDT LS opt-in já existe; falta empacotamento/UX e matriz de projetos reais;
- Gradle Tooling API, bundles de version catalog e dependências de plugins;
- Javadoc HTML completo e download automático de sources.

Aceite final:

- abrir um projeto Maven real e navegar de `@Autowired`/interfaces até implementações;
- completion de membros em tipos de biblioteca;
- zero falsos positivos de sintaxe em Java 21;
- memória < 800 MB em projeto de 1M LOC.

## M3 — Build, run e testes (em andamento; compilação/execução entregues)

Entregue:

- compilação do projeto com `javac` usando o classpath resolvido e `--release` do target;
- diagnósticos do javac no painel Problemas e no console;
- descoberta de `main` e execução com `java`, com saída no console e `Ctrl+F11`;
- lifecycle Maven/Gradle (`compile`, `package`, `clean`) com output incremental e hyperlinks;
- JUnit 4/5 inline: descoberta, árvore classe → método, execução por classe/método e falhas clicáveis;
- formatter externo opt-in e organize imports nativo;
- console com ANSI, histórico persistido e limites explícitos;
- split editor com abas arrastáveis entre dois painéis.

Pendente:

- Debugger JDWP/JDI (M4).
- JUnit independente do runner Maven/Gradle (execução direta via launcher/JUnit Platform).
- Configurações de execução múltiplas e variáveis por plataforma.
- Formatter embutido (sem dependência externa).
- Problemas de compilação em tempo real durante a digitação (javac em fundo já existe).

Aceite:

- rodar `mvn test`/`gradle test` e navegar dos resultados ao código;
- reexecutar um teste individual em < 2 s de overhead da IDE.

## M4 — Debugger (4–6 semanas)

- Adaptador DAP em Rust e backend JDWP/JDI em processo separado.
- Breakpoints (linha, condicional, exceção), stepping, call stack, variáveis, watches.
- Evaluate expression e hot swap básico quando suportado.
- Launch/attach em JVM local e remota.

Aceite:

- depurar o `demo-java` e um projeto Spring Boot real;
- pausar em exceção e inspecionar frames sem congelar a UI.

## M5 — Plataforma de plugins (4–6 semanas)

- ABI v2 com comandos assíncronos, eventos e serviços de host (documento, workspace, UI).
- Manifesto, permissões e assinatura de plugins nativos.
- Runtime WASM com capabilities e limites.
- Hot reload seguro e diagnóstico de falhas.
- SDK com templates, testes e exemplos (formatter, linter, gerador).
- Marketplace simples (índice estático + verificação de integridade).

Aceite:

- plugin de terceiro instalado sem reiniciar;
- plugin WASM não consegue ler arquivos sem permissão;
- crash de plugin não derruba a IDE.

## M6 — Compatibilidade Eclipse headless (8–12 semanas)

- Sidecar Equinox com JDK embutido e bundle pool configurável.
- Descoberta de natures/builders a partir de `.project`.
- Ponte de extension registry → comandos/eventos Reclipse.
- Execução de builders/validadores JDT headless e publicação de markers.
- Adaptadores para contribuições de comandos e wizards não-SWT.
- Matriz de compatibilidade explícita: “funciona”, “parcial”, “não suportado (SWT)”.

Aceite:

- importar um projeto Eclipse existente e executar seus builders headless;
- comandos de plugins comuns (ex.: geradores) aparecem na palette;
- plugins SWT são rejeitados com explicação clara, sem tentar embutir um display.

## M7 — Performance e escala (4–6 semanas)

- Índice persistente em disco (mmap) e carregamento lazy.
- Atualização incremental por dependências (reverse imports).
- Perfis de memória e CPU no CI; benchmarks de 10k/100k/1M LOC.
- Thread pool com prioridades (interativo vs background).
- Medições públicas por release.

Aceite:

- cold start < 150 ms; projeto 1M LOC < 2 s; memória < 500 MB em repouso;
- reindexação de arquivo < 50 ms; UI sem frames > 33 ms em uso normal.

## M8 — Produto 1.0 (4–6 semanas)

- Instalador MSI e atualização automática assinada.
- Configurações visuais, temas, acessibilidade e i18n (pt-BR/en).
- Crash reporting opt-in, logs rotativos, diagnóstico de ambiente.
- Documentação de usuário, migração do Eclipse e guia de plugins.
- Política de segurança e processo de release.

## Resumo

| Marco | Status/semanas | Acumulado |
|---|---:|---:|
| M0 | concluído | — |
| M1 | concluído | 3 |
| M2 | em andamento (referências, rename e fontes entregues) | 15 |
| M3 | em andamento (javac/run/JUnit/lifecycle/console entregues) | 21 |
| M4 | 4–6 | 27 |
| M5 | 4–6 | 33 |
| M6 | 8–12 | 45 |
| M7 | 4–6 | 51 |
| M8 | 4–6 | 57 |

**1.0 realista: ~12–15 meses com uma pessoa, ~7–9 meses com duas**, sem contar imprevistos de compatibilidade. A parte mais arriscada não é a UI: é o motor semântico Java e a compatibilidade OSGi.

## Riscos

| Risco | Impacto | Mitigação |
|---|---|---|
| Subestimar resolução de tipos Java | Alto | Começar por classpath e imports; modo JDT LS opt-in; testes com projetos reais |
| Expectativa de rodar plugins SWT | Alto | Comunicação clara; matriz de compatibilidade; adaptadores só para headless |
| ABI de plugins instável | Médio | Versionamento explícito; congelar v1; testes de contrato |
| Sidecar JVM consumir memória | Médio | Processo separado, limites de heap, desligamento automático, métricas |
| Gramática tree-sitter divergir do javac | Médio | Golden tests; fallback semântico; atualizar gramática |
| Performance regredir | Médio | Benchmarks no CI e orçamento por release |
| Dependência de um único SO | Baixo | Manter crates portáveis; CI Linux/macOS |

## Critérios para 1.0

1. Abrir, compilar, testar e depurar um projeto Maven/Gradle real.
2. Navegação semântica confiável e refactors seguros.
3. Compatibilidade headless com um subconjunto útil de plugins Eclipse, documentada.
4. Ecossistema de plugins Rust com SDK e sandbox WASM.
5. Metas de performance medidas e publicadas.
6. Instalação, atualização e recuperação de falhas sem fricção.
