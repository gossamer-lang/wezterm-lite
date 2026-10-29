# wezterm-lite

The terminal emulation core of [WezTerm](https://github.com/wezterm/wezterm),
without the GUI, multiplexer, SSH client, font stack, or image decoding. It
parses what a program writes to a terminal and keeps the screen, scrollback,
modes and cursor that a front end draws.

Scrollback lines are packed into their text plus runs of attributes as they
leave the screen, so a full history costs a small fraction of a cell grid.

## Crates

| Crate | Path | What it holds |
|---|---|---|
| `wezterm-term` | `term` | The terminal state machine: screen, scrollback, modes, replies |
| `wezterm-escape-parser` | `wezterm-escape-parser` | Escape sequences parsed into actions, and encoded back |
| `vtparse` | `vtparse` | The byte-level VT state machine |
| `wezterm-surface` | `wezterm-surface` | Lines, line storage and hyperlink rules |
| `wezterm-cell` | `wezterm-cell` | Cells and their attributes |
| `wezterm-char-props` | `wezterm-char-props` | Unicode width, grapheme and emoji properties |
| `wezterm-bidi` | `bidi` | The Unicode Bidirectional Algorithm |
| `wezterm-color-types` | `color-types` | Colors and color parsing |
| `wezterm-input-types` | `wezterm-input-types` | Key codes and modifiers |
| `wezterm-dynamic` | `wezterm-dynamic` | Configuration values and their derive macros |
| `wezterm-blob-leases` | `wezterm-blob-leases` | Storage for image data |

## Use

```toml
[dependencies]
wezterm-term = { git = "https://github.com/gossamer-lang/wezterm-lite", tag = "v0.1.0" }
```

Feed program output with `Terminal::advance_bytes`, read lines through
`Terminal::screen`, and implement `TerminalConfiguration` for scrollback size,
palette, and the terminfo capabilities `termcap` reports.

The `images` feature of `wezterm-term` turns on Sixel, kitty graphics and
iTerm2 images. Without it those sequences are parsed and ignored.

## Releases

Pushing a tag `vX.Y.Z` runs the checks on Linux, macOS and Windows and
publishes a GitHub release whose notes are the `## X.Y.Z` section of
`CHANGELOG.md`. The tag must match the version of `wezterm-term`.

## License

MIT, as WezTerm. See `LICENSE.md`; the copyright stays with Wez Furlong and
the WezTerm contributors.
