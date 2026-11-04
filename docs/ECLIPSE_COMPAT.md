# Compatibilidade com Eclipse

Este documento separa o que é tecnicamente viável do que soa bonito em um pitch. “Compatível com plugins Eclipse” pode significar coisas muito diferentes.

## O alvo observado

A instalação usada como referência nesta máquina:

| Item | Valor |
|---|---|
| Produto | Eclipse Platform / EPP JEE |
| Versão | 4.36.0 (build `4.36.0.20250605-1300`) |
| JDT Core | 3.42.0 |
| Equinox OSGi | 3.23.100 |
| JVM embutida | Temurin 21.0.7 |
| Bundles | 1.254 JARs |
| Caminho | `C:\Infra\eclipse` |

Os logs `hs_err_pid*.log` mostram falhas de alocação nativa em language servers JVM (por exemplo, Qute) em sessões longas. Isso reforça a tese central do Reclipse: o processo de UI e o índice quente não devem depender de uma JVM. Também reforça que a compatibilidade JVM, quando existir, deve ser **opcional e isolada**.

## Quatro camadas de compatibilidade

### Camada 1 — Metadados de projeto (✅ MVP)

Reclipse já entende:

- `.project` para detectar um projeto Eclipse;
- `.classpath` com `<classpathentry kind="src" path="...">`;
- diretórios de saída e containers são ignorados com segurança;
- entradas de workspace `/OutroProjeto/src` não são indexadas silenciosamente.

Falta: JRE container, libraries, project references, natures e builders.

### Camada 2 — Plugins headless via Equinox (🔜 M6)

Esta é a parte realmente valiosa e viável. Muitos plugins Eclipse não têm UI:

- builders e validadores;
- geradores de código;
- natures;
- extensões de JDT core;
- serviços de modelagem.

Plano:

1. Subir um processo JVM separado com a JVM embutida do Eclipse (JDK 21) e Equinox.
2. Reutilizar o bundle pool existente do Eclipse (`plugins/`, `configuration/`), sem copiar código.
3. Instalar um bundle-ponte nosso que:
   - registra um `BundleActivator`;
   - descobre extension points relevantes;
   - expõe comandos e eventos por JSON-RPC em um socket local.
4. Reclipse (Rust) descobre `.project`, pergunta ao sidecar quais natures/builders existem e dispara execuções.
5. Markers produzidos pelo JDT/builders são convertidos em problemas da IDE, com arquivo/linha/coluna.
6. Se o sidecar morrer (OOM, crash), a IDE continua; o usuário pode reiniciá-lo.

O sidecar deve ter:

- heap limitado e configurável (`-Xmx`);
- timeout por operação;
- desligamento automático após inatividade;
- logs próprios;
- nenhuma dependência de SWT/Display.

### Camada 3 — Contribuições de UI não-SWT (🔜 M6)

Comandos, wizards simples, páginas de preferência e views declarativas podem ser adaptados:

- extension registry → catálogo de comandos do Reclipse;
- `IHandler` → comando nativo com contexto;
- wizards → formulários egui gerados a partir de metadata;
- views → painéis nativos.

Isso não é automático para todo plugin. Cada extension point precisa de um adaptador e de testes.

### Camada 4 — SWT (❌ não suportado)

Plugins que dependem de `Display`, `Shell`, `Composite`, editores ou views SWT **não rodam** dentro de uma UI egui. As alternativas seriam:

1. Embutir um display SWT completo — destrói a proposta nativa, adiciona JVM, GTK/Win32 e complexidade de threading.
2. Rodar o plugin em uma janela SWT separada — experiência fragmentada e ainda acopla a IDE ao SWT.
3. Reescrever o plugin — não é compatibilidade.

A posição do projeto: **não suportar SWT**. Adaptar casos de uso comuns com extensões nativas e informar claramente os limites.

## JDT Language Server: atalho de precisão

Existe um caminho mais simples para semântica Java de alta qualidade: rodar o **Eclipse JDT Language Server** como processo e falar LSP.

Prós:

- resolução de tipos, completion, refactors e diagnósticos maduros;
- implementação rápida via cliente LSP;
- compatível com Maven/Gradle na maior parte dos casos.

Contras:

- reidrata uma JVM e o consumo de memória que motivou o Reclipse;
- latência de startup e indexing;
- não dá compatibilidade de plugins;
- o índice nativo continuaria necessário para o caminho rápido.

Recomendação: **modo de precisão opt-in**, com o motor nativo como padrão. A UI deve indicar qual motor respondeu e permitir alternar por projeto.

## Arquitetura do sidecar

```text
┌──────────────────────────────┐
│ Reclipse (Rust)              │
│  workspace, UI, editor       │
└──────────────┬───────────────┘
               │  JSON-RPC (localhost, token por sessão)
               ▼
┌──────────────────────────────┐
│ reclipse-eclipse-bridge      │
│  (bundle OSGi nosso)         │
│  extension registry adapter  │
└──────────────┬───────────────┘
               │  chamadas OSGi/JDT
               ▼
┌──────────────────────────────┐
│ Equinox + bundles do Eclipse │
│  JDT core, builders, natures │
│  sem SWT                     │
└──────────────────────────────┘
```

Regras:

- o socket escuta apenas em loopback e exige token efêmero;
- nenhum comando arbitrário de shell;
- serialização JSON com limites de tamanho;
- cancelamento e timeout em todas as chamadas;
- versões de ABI e de Eclipse reportadas no handshake.

## Estratégia de validação

1. Criar um projeto Eclipse de fixture com `.project`, `.classpath`, nature Java e um builder JDT.
2. Executar o build headless no sidecar e comparar markers com o Eclipse real.
3. Medir:
   - tempo de start do sidecar;
   - memória RSS/heap;
   - tempo por build incremental;
   - taxa de markers idênticos.
4. Matriz de plugins:
   - `org.eclipse.jdt.core` (builders) — alvo prioritário;
   - plugins de geração de código — alvo;
   - plugins de UI — explicitamente fora.
5. Testar falha: matar o sidecar no meio de um build e verificar recuperação da IDE.

## Licença e distribuição

O Eclipse é distribuído sob EPL-2.0. A estratégia de engenharia:

- não copiar código EPL para crates MIT;
- não fazer linking estático em código proprietário;
- distribuir bundles não modificados como componentes separados, preservando notices;
- comunicar por processo/protocolo;
- revisar obrigações antes de distribuir comercialmente.

O bundle-ponte original do Reclipse pode ser licenciado separadamente, mas deve ser escrito sem copiar implementações EPL.

## O que prometemos (e o que não prometemos)

Prometemos:

- ler projetos Eclipse existentes;
- executar um subconjunto headless de plugins Eclipse via sidecar, com matriz de compatibilidade;
- oferecer um SDK nativo em Rust para novos plugins;
- não fingir que plugins SWT funcionam.

Não prometemos:

- rodar qualquer plugin Eclipse sem mudanças;
- carregar SWT dentro da UI Rust;
- compatibilidade binária com APIs internas do JDT;
- paridade imediata com o Eclipse 4.36 em todos os extension points.

## Alternativas consideradas

| Alternativa | Veredito |
|---|---|
| Reimplementar todas as APIs Eclipse em Rust | Anos de trabalho; inviável e frágil |
| Rodar Eclipse inteiro e embutir a janela | Não é uma IDE Rust; mantém o custo que queremos eliminar |
| Sidecar Equinox headless | Melhor relação valor/esforço |
| Apenas LSP/JDT LS | Ótimo para semântica, não para plugins |
| SDK nativo + adaptadores | Caminho para o ecossistema novo |

A combinação recomendada é **motor nativo + SDK Rust + sidecar headless opcional + LSP opt-in**.
