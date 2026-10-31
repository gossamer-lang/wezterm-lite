# Changelog

## 0.1.0

- Initial release: WezTerm's terminal core (`wezterm-term` and the crates under it) as its own workspace, from WezTerm commit e68a8ec.
- Sixel, kitty graphics and iTerm2 images are behind the `images` feature, off by default; without it image sequences are parsed and ignored.
- With `images`, the `image` crate builds only its PNG, GIF, JPEG and WebP decoders, 18 crates where its defaults pull 71, including an AV1 encoder.
- Images from all three protocols share one memory limit, `TerminalConfiguration::image_memory_limit` (64 MiB by default): a new image detaches the oldest placements of other images until it fits, and one larger than the limit is refused. Kitty's store prunes to the same limit instead of a fixed 320 MB, and `image_memory()` reports what the images hold.
- The cache that shares a repeated image no longer keeps it alive once no cell shows it.
- iTerm2 images are decoded as they arrive, so the limit counts the pixels a renderer draws.
- Decoded pixels are `Arc<[u8]>`, so a renderer holds a frame without copying it; an in-place kitty frame edit copies only a frame something else still holds.
- An animation decodes at most 100 MB of frames; a longer one keeps its first frame.
- `CellAttributes::detach_image_data` removes the attachments that show one image.
- A kitty placement given only columns or only rows sets the other from the image's aspect ratio, as kitty does, where it kept the image's own height in pixels and stretched it.
- Key encoding lives in `wezterm_term::keys`, so the core no longer depends on termwiz.
- XTGETTCAP answers come from `TerminalConfiguration::termcap`, so an embedder reports the terminfo entry it advertises.
- `TerminalState::get_current_dir` returns the OSC 7 location as the program sent it.
- Replies to the program (device attributes, status reports, mouse reports) are written in full, on the thread that feeds the terminal, before `advance_bytes` returns; a terminal no longer starts a writer thread.
- A tab over blank cells leaves `\t` in the cell it began on, as xterm and VTE do, so copying a line gives its tabs back.
- A cell keeps at most 32 codepoints of a grapheme cluster, so a run of combining marks cannot grow one cell without bound.
- Runs of printable ASCII skip the escape parser and fill their cells in bulk.
- Synchronized output (DECSET 2026) is tracked: `synchronized_output()` answers whether the program has a frame open, DECRQM reports the mode, and RIS ends it.
- A combining mark that arrives in a later read than its base character joins that character, where it was dropped.
- `Screen::with_phys_lines` reads lines in order once the history ring wraps, where it read the wrong lines or panicked.
- `CellRef::str` and `CellRef::attrs` borrow from the line, not from the `CellRef`, so a caller can keep them while walking the line.
- `wezterm_term` re-exports `CursorShape` and `CursorVisibility`, the types of `CursorPosition`'s fields.
- `CellAttributes::set_image` and `attach_image` take an `ImageCell` by value, and `Rule::new` returns a boxed regex error.
- `vtparse` reports a missing `std`, `alloc` or `no_std` feature at compile time.
- `wezterm-term` builds 65 crates where WezTerm's builds 194, and adds about 0.8 MB to a release binary where WezTerm's adds 4.3 MB.
