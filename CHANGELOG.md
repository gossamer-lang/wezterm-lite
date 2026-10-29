# Changelog

## 0.1.0

- Initial release: WezTerm's terminal core (`wezterm-term` and the crates under it) as its own workspace, from WezTerm commit e68a8ec.
- Sixel, kitty graphics and iTerm2 images are behind the `images` feature, off by default; without it image sequences are parsed and ignored.
- Key encoding lives in `wezterm_term::keys`, so the core no longer depends on termwiz.
- XTGETTCAP answers come from `TerminalConfiguration::termcap`, so an embedder reports the terminfo entry it advertises.
- `TerminalState::get_current_dir` returns the OSC 7 location as the program sent it.
- Replies to the program (device attributes, status reports, mouse reports) are written in full, on the thread that feeds the terminal, before `advance_bytes` returns; a terminal no longer starts a writer thread.
- A tab over blank cells leaves `\t` in the cell it began on, as xterm and VTE do, so copying a line gives its tabs back.
- A cell keeps at most 32 codepoints of a grapheme cluster, so a run of combining marks cannot grow one cell without bound.
- A combining mark that arrives in a later read than its base character joins that character, where it was dropped.
- `Screen::with_phys_lines` reads lines in order once the history ring wraps, where it read the wrong lines or panicked.
- `CellRef::str` and `CellRef::attrs` borrow from the line, not from the `CellRef`, so a caller can keep them while walking the line.
- `wezterm_term` re-exports `CursorShape` and `CursorVisibility`, the types of `CursorPosition`'s fields.
- `CellAttributes::set_image` and `attach_image` take an `ImageCell` by value, and `Rule::new` returns a boxed regex error.
- `vtparse` reports a missing `std`, `alloc` or `no_std` feature at compile time.
- `wezterm-term` builds 65 crates where WezTerm's builds 194, and adds about 0.8 MB to a release binary where WezTerm's adds 4.3 MB.
