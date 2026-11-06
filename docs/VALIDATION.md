# Matriz de validação do debugger

Cenários validados contra JVMs reais, manuais ou via
`crates/reclipse-debug/tests/debug_session.rs` (`cargo test -p reclipse-debug`).

## Como rodar

```sh
cargo test -p reclipse-debug
```

Os testes de integração lançam uma JVM real por caso (serializados por
`Mutex`, pois cada um reserva uma porta JDWP efêmera). Sob carga paralela
(`cargo test --workspace` junto à indexação do JDK), os deadlines estouram de
forma intermitente — para a matriz, rode o crate isolado.

## Projetos

| Projeto | Tipo | Como reproduzir |
|---|---|---|
| `examples/demo-java` | `javac` direto | `cargo run -p reclipse-ui -- examples/demo-java`, `F11` |
| `examples/multi-module-demo` | Maven multi-módulo (parent + `core` + `app`, sem deps externas) | `mvn -o compile` no diretório; teste `steps_across_maven_modules` |
| `examples/spring-boot-demo` | Spring Boot 3.5 real | `docs/screenshot-spring-boot.png`; Tomcat iniciado, parada em `DemoApplication.java:13` |

## Cenários

| Cenário | demo-java | multi-module-demo | spring-boot-demo | Evidência |
|---|---|---|---|---|
| Launch + breakpoint de linha | ✅ | ✅ | ✅ | testes + screenshots |
| Step into/out/over | ✅ | ✅ (into `core` a partir de `app`) | ✅ | teste `steps_across_maven_modules`, `steps_into_and_out` |
| Inspecionar locais/campos | ✅ | ✅ | ✅ | testes |
| Evaluator (`1+2*3`, campos) | ✅ | ✅ (`name` no frame de `core`) | ✅ (watch `args.length`) | testes + screenshot |
| Índices de array (`arr[i]`) | ✅ | — | — | teste `evaluates_expressions_and_watches` |
| Chamadas (`obj.m()`, `Classe.stat()`) | ✅ | — | — | teste `evaluates_expressions_and_watches` |
| Exceção não capturada | ✅ | — | — | teste `pauses_on_uncaught_exception` |
| Filtro por tipo/capturada | ✅ | — | — | teste `filters_exceptions_by_type_and_caught_flag` |
| Condicional (`i == 3`) | ✅ | — | ✅ (`App.java:15`) | teste + screenshot |
| Hot swap | ✅ | — | — | teste `hot_swaps_a_loaded_class` |
| Attach | ✅ | — | — | teste `attaches_to_a_suspended_jvm` |
| Anônimas/lambdas/aninhados | ✅ | — | — | testes |

## Limites conhecidos

- Multi-módulo na IDE: abrir no diretório do módulo e resolver deps via
  repositório local (`mvn install` nos módulos) ou `mvn compile` antes; o
  reator Maven (`<modules>`) não é resolvido pelo classpath.
- Gradle real ainda sem validação de ponta a ponta no debugger.
- Remoto sem TLS/autenticação; attach remoto sem reconexão.
- Evaluator: sem `new`, campos estáticos, casts, lambdas; sem chamadas em condições.
