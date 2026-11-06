---
name: "RECLIPSE Studio System"
description: "Design system dark industrial e focado em precisão para a IDE Java RECLIPSE baseada em Eclipse e construída em Rust."
---

## Overview

RECLIPSE é uma IDE de alta performance para engenharia Java, forjada sobre a robustez do Rust e inspirada na arquitetura consagrada do Eclipse. O design system traduz a fusão entre a identidade industrial do Rust (engrenagens, tons terrosos oxidantes, precisão mecânica) e o fenômeno astronômico do Eclipse (penumbra profunda, anel coronal, contraste cósmico de alto foco).

A experiência visual elimina o ruído e o excesso de bordas pesadas do Eclipse clássico dos anos 2000, preservando sua ergonomia comprovada de docking, visualização de pacotes e console, enquanto introduz uma camada moderna, densa e confortável para longas sessões de codificação.

## Colors

RECLIPSE disponibiliza 3 temas pré-configurados comutáveis na barra de ferramentas e nas preferências do usuário:

1. **Rust Oxide Industrial (Padrão):** Grafite profundo (#111418), ferrugem industrial (#E05A2B) e acentos âmbar solar (#F59E0B). Máximo contraste mecânico e foco no motor nativo.
2. **Eclipse Dark Moderno (Espacial & Neon):** Superfície azulada/índigo cósmica (#0d1117 / #161b22), violeta espacial (#8B5CF6) e acento neon cyan (#06B6D4) que moderniza o Eclipse clássico.
3. **Cyber / Solar Eclipse:** Carvão obsidiana (#0a0c10), âmbar de radiação solar (#F59E0B) e turquesa cibernético elétrico (#10B981 / #06B6D4) para sessões noturnas de depuração.

- **Primary:** Rust Oxide (#E05A2B), representando o motor Rust e as ações primárias de execução e build.
- **Secondary:** Corona Solar (#F59E0B), o anel de luz do eclipse, utilizado para seleção ativa, avisos e status.
- **Surface:** Níveis escuros balanceados para separar árvore de projeto, editor e abas de ferramenta sem linhas cortantes desnecessárias.

```yaml
colors:
  rust-core: "#E05A2B"
  eclipse-corona: "#F59E0B"
  java-cyan: "#38BDF8"
  java-magenta: "#EC4899"
  success: "#22C55E"
  warning: "#F59E0B"
  error: "#EF4444"
  info: "#38BDF8"
  light:
    primary: "#B93815"
    on-primary: "#FFFFFF"
    surface: "#F8FAFC"
    on-surface: "#0F172A"
    surface-container: "#F1F5F9"
    surface-container-high: "#E2E8F0"
    outline: "#94A3B8"
  dark:
    primary: "#FF7A45"
    on-primary: "#280A03"
    surface: "#111418"
    on-surface: "#E2E8F0"
    surface-container: "#181D24"
    surface-container-high: "#222832"
    outline: "#374151"
```

## Typography

A identidade tipográfica do RECLIPSE equilibra personalidade técnica e conforto extremo de leitura. **Cal Sans** traz uma geometria angular, sólida e moderna para títulos, logomarca e cabeçalhos de visualizações. **Commissioner** atua na interface geral com clareza ótica superior em rótulos densos e árvores de arquivos. **Source Code Pro** fornece a fundação monospaçada para o código Java, console de saída e métricas do compilador Rust.

```yaml
typography:
  display-lg:
    fontFamily: "Cal Sans"
    fontSize: 36px
    fontWeight: 700
    lineHeight: 44px
    letterSpacing: -0.5px
  display-lg-mobile:
    fontFamily: "Cal Sans"
    fontSize: 28px
    fontWeight: 700
    lineHeight: 34px
    letterSpacing: -0.25px
  headline-lg:
    fontFamily: "Cal Sans"
    fontSize: 24px
    fontWeight: 600
    lineHeight: 32px
    letterSpacing: -0.2px
  headline-md:
    fontFamily: "Cal Sans"
    fontSize: 20px
    fontWeight: 600
    lineHeight: 28px
    letterSpacing: -0.1px
  title-md:
    fontFamily: "Commissioner"
    fontSize: 15px
    fontWeight: 600
    lineHeight: 22px
    letterSpacing: 0px
  body-md:
    fontFamily: "Commissioner"
    fontSize: 13px
    fontWeight: 400
    lineHeight: 18px
    letterSpacing: 0px
  label-md:
    fontFamily: "Commissioner"
    fontSize: 12px
    fontWeight: 500
    lineHeight: 16px
    letterSpacing: 0.1px
  label-sm:
    fontFamily: "Commissioner"
    fontSize: 11px
    fontWeight: 500
    lineHeight: 14px
    letterSpacing: 0.2px
  code-md:
    fontFamily: "Source Code Pro"
    fontSize: 13px
    fontWeight: 400
    lineHeight: 20px
    letterSpacing: 0px
  code-sm:
    fontFamily: "Source Code Pro"
    fontSize: 11px
    fontWeight: 400
    lineHeight: 16px
    letterSpacing: 0px
```

## Layout & Spacing

A IDE opera com uma grade de docking flexível dividida em quadrantes canônicos do Eclipse: barra de atividades lateral, painel de navegação de pacotes Java (Package Explorer), editor central multi-abas e console inferior/painel de problemas.

O espaçamento interno adota múltiplos de 4px para densidade compacta de ferramentas de desenvolvimento, priorizando a visibilidade do código.

```yaml
spacing:
  gutter-xs: 4px
  gutter-sm: 8px
  gutter-md: 12px
  gutter-lg: 16px
  gutter-xl: 24px
  section: 32px
  page-margin: 16px
```

## Elevation & Depth

RECLIPSE adota um modelo de profundidade por camadas tonais sem sombras espessas difusas que poluem o foco visual. As janelas ativas e abas recebem elevação através de contraste sutil entre `surface-container` e `surface-container-high` com bordas discretas em tom grafite oxidado. Tooltips e menus suspensos recebem elevação suave com bordas iluminadas pelo acento solar.

## Shapes

As formas são precisas e industriais, com cantos ligeiramente arredondados (4px a 6px) para botões de controle, abas chanfradas ou arredondadas sutilmente e badges em formato de pílula compacta.

```yaml
rounded:
  sm: 3px
  DEFAULT: 5px
  md: 8px
  lg: 12px
  xl: 16px
  full: 9999px
```

## Motion & Micro-interaction

As transições são ultra-rápidas para preservar a percepção de performance incomparável que o Rust oferece (menos de 100ms em trocas de aba, execução e expansão de árvore).

```yaml
motion:
  state-change:
    duration: 80ms
    delay: 0ms
    timingFunction: cubic-bezier(0.2, 0, 0, 1)
  panel-toggle:
    duration: 160ms
    delay: 0ms
    timingFunction: cubic-bezier(0.16, 1, 0.3, 1)
  toast-fade:
    duration: 120ms
    delay: 0ms
    timingFunction: ease-out
```

## Voice & Content Strategy

A linguagem de RECLIPSE é técnica, objetiva e orientada a produtividade de engenharia de software:

- **Precisão terminológica:** Uso de termos nativos de Java e Rust claros (`Build Workspace`, `Run Application`, `Hot Reload`, `Rust Core Engine`, `Garbage Collector Metrics`).
- **Feedback instantâneo:** Diagnósticos com identificação de arquivo e linha clara, diferenciando avisos do compilador Rust e do analisador semântico Java (JDT).
- **Ações diretas:** Verbos no imperativo para ações rápidas (`Run`, `Debug`, `Refactor`, `Clean & Build`).

## Components

Component implementations live as individual HTML sticker sheet files in
`components/`. Each file contains the canonical, styled HTML components with
`<!-- COMPONENT: name -->` comment markers. Read each file before use.

- **Badge** → `@components/badge.html`: `badge-numeric`, `badge-dot`
- **Button** → `@components/button.html`: `button-primary`, `button-icon`, `button-with-icon`
- **Card** → `@components/card.html`: `card-elevated`, `card-interactive`, `card-interactive-2`
- **Tabs** → `@components/tabs.html`: `tabs-horizontal`, `tabs-with-icon`

## Ícones de ação

A toolbar, o painel DEBUG e a árvore JUnit usam ícones vetoriais originais
desenhados em `crates/reclipse-ui/src/icons.rs` com `egui::Painter`:

- **Executar/Continuar**: triângulo play; **Debug**: besouro;
- **Compilar**: marreta; **Build**: prancheta com check;
- **Testar**: frasco com líquido e check; **Launch**: play + sliders de configuração;
- **Step over/into/return**: arcos e setas amarelas; **Pausar**: barras; **Parar**: quadrado;
- **Hot swap/Reindexar**: setas circulares; **Salvar/Buscar/Referências**: glifos de chrome.

Os desenhos são inspirados nas metáforas do Eclipse/WTP, mas **não copiam**
assets EPL-2.0; permanecem sob a licença MIT do projeto. Cada ícone carrega a
cor semântica do tema (verde para run/test, amarelo para step/pause, vermelho
para stop, accent para build/debug) e o texto mantém a cor de primeiro plano.
Os botões icon-only usam tooltip com o atalho e a descrição da ação.

## Implementação atual no Reclipse

O design system está implementado nativamente no shell egui, sem HTML/CSS:

- `crates/reclipse-ui/src/theme.rs`: `ThemeKind` (`RustOxide`, `EclipseCosmic`, `CyberSolar`), as três paletas, o emblema vetorial e os acessores dinâmicos de cor (`accent()`, `panel()`, `clr_keyword()`, ...).
- `crates/reclipse-ui/src/icons.rs`: ícones vetoriais originais de build, test, launch e debug, com tint semântico por tema e botões com/sem label.
- `crates/reclipse-ui/src/app.rs`: title bar com emblema, seletor de tema, quick action toolbar, activity bar, breadcrumb, abas com indicador de alteração, console de diagnóstico (`Console`/`Rust Engine`) e status bar.
- `crates/reclipse-ui/src/editor_view.rs`: linha atual, seleção e destaques de busca derivam da paleta ativa.
- `session.json`: o campo `theme` persiste `rust-oxide`, `eclipse-cosmic` ou `cyber-solar`; `RECLIPSE_THEME` permite override no startup.

As fontes **Cal Sans**, **Commissioner** e **Source Code Pro** são embutidas via `include_bytes!` a partir de `crates/reclipse-ui/assets/fonts/` (OFL 1.1), com fallback para as fontes do egui em glifos não cobertos.
