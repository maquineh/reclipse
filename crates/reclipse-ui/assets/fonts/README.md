# Fontes do RECLIPSE Studio

Fontes embutidas no binário via `include_bytes!` (SIL Open Font License 1.1).
Os arquivos estáticos foram instanciados a partir das fontes variáveis do
Google Fonts para garantir pesos consistentes no `ab_glyph`/egui.

| Arquivo | Uso | Origem |
|---|---|---|
| `CalSans.ttf` | display/headlines e marca | google/fonts `ofl/calsans` |
| `Commissioner-Regular.ttf` | UI, menus, árvores | google/fonts `ofl/commissioner` |
| `Commissioner-SemiBold.ttf` | fallback de display | google/fonts `ofl/commissioner` |
| `SourceCodePro-Regular.ttf` | editor, console, telemetria | google/fonts `ofl/sourcecodepro` |
| `SourceCodePro-SemiBold.ttf` | reservado para destaques de código | google/fonts `ofl/sourcecodepro` |

As licenças originais estão em `OFL-*.txt`. As fontes são carregadas em
`crates/reclipse-ui/src/theme.rs` (`install_fonts`) e o fallback para as fontes
padrão do egui permanece disponível para glifos não cobertos.
