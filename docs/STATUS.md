# Estado atual do Reclipse

Atualizado após a validação com Spring Boot e os refinamentos do M2.

## Entregue

### Base (M1/M2)

- IDE nativa em Rust/egui com três temas, editor virtualizado, multi-cursor, folding, abas e split editor.
- Classpath Eclipse/Maven/Gradle + JDK, índice nativo de JARs/JMODs e resolução de tipos/imports.
- Completion, hover, referências semânticas, static imports e overloads com especificidade de tipos.
- Rename conservador de tipos, métodos, campos e locais, com atualização de Javadoc e `module-info`.
- Move de tipo top-level entre pacotes.
- Javadoc completo (`F2`), download de sources/Javadoc via Maven e JDT LS opt-in com status/reinício.
- Toolbar, painel DEBUG e JUnit com ícones vetoriais próprios (sem assets EPL do Eclipse).

### Build, testes e debug (M3/M4)

- `javac` com `--release`, lifecycle Maven/Gradle (`compile`, `package`, `clean`) e output incremental.
- JUnit 4/5 inline com árvore classe → método, execução filtrada e falhas clicáveis.
- Formatter externo opt-in e organize imports nativo.
- Console ANSI com histórico persistido e limites explícitos.
- Debugger JDWP nativo:
  - launch/attach, breakpoints de linha e condicionais;
  - stepping (`F5`/`F6`/`F7`/`F8`), pausa e encerramento;
  - call stack, threads, variáveis, campos de `this`, watches e evaluator
    (índices de array e chamadas de método via JDWP);
  - hot swap básico e pausa em exceções;
  - breakpoints por arquivo-fonte para classes anônimas/lambdas e tipos aninhados.

## Validação

- `cargo test --workspace`: **173 testes** aprovados.
- `cargo clippy --workspace --all-targets`: sem warnings.
- `cargo fmt --all -- --check`: limpo.
- Integração real com JVM: breakpoint, step into/out, exceções, attach, hot swap, classes anônimas e expressões.
- Spring Boot 3.5 real: Tomcat iniciado e debugger pausado em `DemoApplication.java:13`.
- Evidências: `docs/screenshot-spring-boot.png`, `docs/screenshot-javadoc.png`, `docs/screenshot-debug.png`, `docs/screenshot-toolbar-icons.png`.

## Pendente

- Modelagem de tipos anônimos com ordinais por método (hoje mesclados por método) e `this`/`super` em corpos anônimos.
- Rename/move atualizando referências por reflexão e nomes em arquivos de recursos não-textuais.
- Bundles de version catalog e dependências de plugins Gradle; Tooling API JVM-side.
- Adaptador DAP: variáveis de frames não-selecionados, selectFrame e pontos de log.
- TLS/autenticação no attach remoto; matriz de JVMs na validação.
- Variáveis de ambiente por plataforma nas launch configs.

## Como verificar

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets
cargo run -p reclipse-ui -- examples/demo-java
```
