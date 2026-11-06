# JDT LS opt-in

O motor nativo continua sendo o padrão. O JDT Language Server é um **modo de
precisão opt-in**: ele adiciona diagnósticos do Eclipse/JDT sem substituir a
indexação, completion, hover, referências e rename nativos.

## Configuração

```text
RECLIPSE_JDTLS=C:\caminho\para\jdtls\bin\jdtls.cmd
RECLIPSE_JDTLS_WORKSPACE=C:\caminho\para\workspace-jdtls   # opcional
```

- O launcher precisa aceitar `-data <workspace>` e falar LSP por stdio.
- Sem workspace explícito, a IDE usa `<projeto>/.reclipse-jdtls`.
- A toolbar mostra `JDT off`, `JDT…`, `JDT on` ou `JDT falhou`.
- A palette tem **JDT: reiniciar servidor (opt-in)**.
- Diagnósticos do JDT aparecem no painel **Problemas** com cor própria e são
  recarregados ao salvar buffers Java.

## Matriz de validação

| Cenário | Estado | Observação |
|---|---|---|
| `examples/demo-java` | validado | inicialização, didOpen/didChange e diagnóstico |
| Spring Boot 3.5 (`examples/spring-boot-demo`) | validado | classpath Maven com 105 archives; debugger pausa no `main` |
| Projeto Maven multi-módulo | pendente | requer matriz com workspaces reais |
| Gradle | pendente | falta validação com Tooling API/launcher real |
| JDT empacotado com a IDE | pendente | hoje o usuário fornece o launcher |

## Limitações atuais

- O JDT LS não é baixado nem empacotado automaticamente; é necessário apontar
  `RECLIPSE_JDTLS` para uma instalação existente.
- Não há UX de primeira execução para escolher/baixar o servidor.
- Diagnósticos de fluxo/nullness só existem quando o JDT está ativo; o modo
  nativo continua conservador.
- O servidor roda no mesmo usuário e não é sandbox.
