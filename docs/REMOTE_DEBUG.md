# Debug remoto (JBoss, WildFly, Spring Boot)

O Reclipse depura servidores remotos como o Eclipse/WTP: o servidor expõe
JDWP (`server=y`) e a IDE faz **attach** em `host:porta`. Não há agente
próprio nem TLS — JDWP puro, como nas receitas abaixo.

## Na IDE

1. Suba o servidor com JDWP (receitas abaixo).
2. Em **Launch…**, preencha **Attach de debug** com `host:porta`
   (só a porta = `127.0.0.1:porta`; padrão JBoss/WildFly: `5005`).
3. `F11` ou **Debug**. A IDE tenta conectar por ~30 s (útil quando o
   servidor ainda está subindo) e mostra o progresso no painel.
4. Se a conexão cair, o botão **Reconectar** no painel DEBUG refaz o
   attach com os mesmos breakpoints e filtros de exceção.

Breakpoints, stepping, evaluator, watches, hot swap e filtros de exceção
funcionam igual ao launch local. Para redes não confiáveis, use túnel SSH
(`ssh -L 5005:localhost:5005 usuario@servidor`).

## JBoss EAP / WildFly

```sh
# porta 5005 (suspend=n: o servidor não espera o debugger)
standalone.sh --debug
# endereço/porta explícitos (WildFly 14+)
standalone.sh --debug *:5005
```

Equivalente manual via `JAVA_OPTS` (`bin/standalone.conf`):

```sh
JAVA_OPTS="$JAVA_OPTS -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005"
```

## Spring Boot (web)

```sh
# jar pronto
java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005 -jar app.jar

# Maven (suspende até o attach com suspend=y, se preferir)
mvn spring-boot:run -Dspring-boot.run.jvmArguments="-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005"

# Gradle
./gradlew bootRun --args='' -Dorg.gradle.jvmargs="-agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=*:5005"
```

Com `suspend=y`, o Boot espera a IDE antes de iniciar o contexto (bom para
pegar inicialização); com `suspend=n`, o servidor sobe e a IDE conecta quando
quiser — o retry de 30 s cobre a janela de startup.

## Limites

- Sem TLS/autenticação: restrinja a interface (`address=localhost:5005`)
  ou use túnel SSH.
- Sem reconexão automática: a queda de conexão exige o botão **Reconectar**
  (a sessão JDWP não sobrevive ao drop de socket).
- Hot swap segue as regras do `RedefineClasses` da JVM remota.
