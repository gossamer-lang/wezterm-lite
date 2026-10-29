# Changelog

## 0.1.0

- Initial release: WezTerm's terminal core (`wezterm-term` and the crates under it) as its own workspace, from WezTerm commit e68a8ec.
- Sixel, kitty graphics and iTerm2 images are behind the `images` feature, off by default; without it image sequences are parsed and ignored.
- Key encoding lives in `wezterm_term::keys`, so the core no longer depends on termwiz.
- XTGETTCAP answers come from `TerminalConfiguration::termcap`, so an embedder reports the terminfo entry it advertises.
- `TerminalState::get_current_dir` returns the OSC 7 location as the program sent it.
- Replies to the program (device attributes, status reports, mouse reports) are written in full.
- `CellAttributes::set_image` and `attach_image` take an `ImageCell` by value, and `Rule::new` returns a boxed regex error.
- `vtparse` reports a missing `std`, `alloc` or `no_std` feature at compile time.
- `wezterm-term` builds 65 crates where WezTerm's builds 194, and adds about 0.8 MB to a release binary where WezTerm's adds 4.3 MB.
