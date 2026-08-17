# Third-party licenses

Osmium itself is MIT (see [LICENSE](LICENSE)). It links third-party crates and
embeds font glyph data; this file records their licenses, as those licenses
require when the work is redistributed — including the prebuilt images on the
releases page.

## Crates linked into the kernel

Every crate compiled into the kernel is under a permissive license — each is
`MIT`, `Apache-2.0`, `MIT OR Apache-2.0`, or `Unlicense OR MIT`. The direct
dependencies are: `bootloader_api`, `x86_64`, `spin`, `uart_16550`, `pic8259`,
`pc-keyboard`, `linked_list_allocator`, `noto-sans-mono-bitmap`, `log`,
`futures-util`, and `crossbeam-queue`, together with their transitive
dependencies (`bitflags`, `bit_field`, `lock_api`, `volatile`, `rustversion`,
and others). The `bootloader`-side image tooling (`fatfs`, `gpt`, `mbrman`,
`bincode`, and so on) runs only on the host during the build and is not linked
into the shipped image.

Each of these licenses permits redistribution provided the copyright notice and
permission notice are preserved. Those notices live in each crate's own source
(fetched to `~/.cargo/registry` at build time); the full resolved list for a
given build is `cargo tree -p kernel --target x86_64-unknown-none`.

## Embedded font — Noto Sans Mono (SIL Open Font License 1.1)

The framebuffer console renders glyphs from the `noto-sans-mono-bitmap` crate,
which embeds bitmap renderings of **Noto Sans Mono**. Noto Sans Mono is
copyright the Noto Project Authors and licensed under the **SIL Open Font
License, Version 1.1**. Because the shipped kernel image contains those glyph
bitmaps, the OFL notice travels with it:

> Copyright The Noto Project Authors (https://github.com/notofonts/latin-greek-cyrillic)
>
> This Font Software is licensed under the SIL Open Font License, Version 1.1.
> This license is available with a FAQ at https://openfontlicense.org

The full OFL-1.1 text is available at the URL above. Under the OFL the font may
be bundled and redistributed with software; it may not be sold on its own, and
the reserved font name "Noto" is not used for any modified version.
