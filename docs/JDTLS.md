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
- A palette tem **JDT: reiniciar servidor (opt-in)** e
  **JDT: instalar servidor (download)** (one-click, ver abaixo).
- Diagnósticos do JDT aparecem no painel **Problemas** com cor própria e são
  recarregados ao salvar buffers Java.

## Instalação one-click (empacotamento)

A IDE não embute o JDT LS (EPL, específico por plataforma). Em vez disso, a
palette oferece **JDT: instalar servidor (download)**:

1. resolve o tarball do último release em
   `eclipse-jdtls/eclipse.jdt.ls` (API pública do GitHub);
2. baixa com `curl` e extrai com `tar` para `~/.reclipse/jdtls`
   (`%USERPROFILE%\.reclipse\jdtls` no Windows);
3. grava o launcher (`jdtls.cmd`/`jdtls`) apontando para o equinox launcher
   e o `config_<so>` detectados, e o usa automaticamente.

Tudo é explícito e cancelável; `RECLIPSE_JDTLS` continua vencendo quando
definido. Para instalar manualmente, baixe qualquer release em
<https://github.com/eclipse-jdtls/eclipse.jdt.ls/releases> e aponte a
variável para o launcher.

## Matriz de validação

| Cenário | Estado | Observação |
|---|---|---|
| `examples/demo-java` | validado | inicialização, didOpen/didChange e diagnóstico |
| Spring Boot 3.5 (`examples/spring-boot-demo`) | validado | classpath Maven com 105 archives; debugger pausa no `main` |
| Projeto Maven multi-módulo (`examples/multi-module-demo`) | validado (nativo) | debugger cross-module + teste `steps_across_maven_modules`; JDT pendente |
| Gradle | pendente | modelo via init script entregue; falta JDT com launcher real |
| JDT instalado pela IDE | implementado | download/extração/launcher testados; falta matriz com projetos reais |

## Limitações atuais

- O JDT LS não vem embutido no instalador; a primeira ativação baixa ~100 MB
  do GitHub (explícito, via palette).
- Diagnósticos de fluxo/nullness só existem quando o JDT está ativo; o modo
  nativo continua conservador.
- O servidor roda no mesmo usuário e não é sandbox.
