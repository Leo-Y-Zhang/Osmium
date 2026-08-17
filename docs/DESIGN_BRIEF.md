# Design Brief — Osmium console

**Date:** 2026-08-17 · **PRD:** [PRD.md](PRD.md) · **App Flow:** [APP_FLOW.md](APP_FLOW.md)

## Intent

Dense and calm. Osmium's console should read like a well-kept instrument panel: a
lot of true information, arranged so the eye lands on the one line that matters
without being pulled anywhere else. Every pixel it draws is either a fact or the
structure that makes a fact findable.

**What it must never feel like:** a wall of noise. Not a stream of undifferentiated
log lines at one weight in one colour; not a screen that flashes, blinks, animates or
decorates; not a machine performing being a machine. If a screenshot of the console
makes a reader squint to find where the output starts, the design has failed
regardless of how correct the kernel underneath is.

The second intent, harder to name and more important: **it should look like it is
telling the truth.** Restraint is doing real work here. A console that reports 1 MiB
of heap in a plain grey line is more credible than one that reports it in glowing
green inside a box, and this project's entire pitch is that its claims are checkable.
The visual language should not oversell what the engineering understates.

## Who is looking at it

Three people, in three quite different states.

- **The author, mid-iteration**, who has run `cargo xtask run` for the fortieth time
  today and is scanning for one changed line in the boot output. They are not
  reading; they are pattern-matching against what the screen looked like a minute
  ago. Layout stability between boots matters more to them than beauty: if a line
  moves, they should be able to trust that something changed.
- **A reader seeing it for the first time**, in a screenshot in a README, deciding in
  about four seconds whether this is a real operating system or a `println` in a
  loop. They will read the banner and nothing else. The banner has to carry the
  weight: real resolution, real memory figures, real boot time.
- **Someone reading a panic**, who is now in the worst state a user of this software
  can be in: something has gone wrong, they cannot interact, and the only thing they
  can do is read. They need the message, the location, and to be told what to do, in
  that order, at a size they can read across a desk.

## Precedents

- **A well-configured `dmesg`.** What it gets right is *rank*: subsystem names are
  visually separable from the messages that follow them, so the eye can scan the left
  column alone and find the subsystem it cares about. Osmium's boot lines borrow the
  structure — a fixed-width label column, then the message — and not the density.
- **The OpenBSD boot sequence.** What it gets right is *refusing to editorialise*. It
  lists what it found, in one voice, with no progress theatre and no reassurance. The
  reader is trusted to know what a line means. Osmium's initialisation lines take the
  same posture: `heap: 1 MiB at 0x444444440000` and nothing more.
- **A Kernel Panic on a well-behaved Unix, or a modern Rust backtrace.** What both get
  right is *putting the actionable thing where the eye lands first* — the message
  before the machine state — with the raw detail below for whoever wants it. Osmium's
  panic screen orders itself the same way: heading, message, location, then registers.

## Anti-patterns for this project

Specific enough to enforce in review.

- **No green-on-black.** The bright-green-on-pure-black terminal is a costume, and it
  says "hacker aesthetic" where this project needs to say "measured engineering". It
  is also, at typical phosphor-green values, worse to read for long stretches than a
  neutral grey. Green appears in exactly one place — a passing self-test — and never
  as body text.
- **Nothing blinks.** Not the caret, not an error, not an alert. A blinking element
  takes attention continuously and returns information once; on a screen someone
  stares at for minutes, it is a tax. The caret is a solid block, drawn and left
  alone.
- **No ASCII-art logo, no box-drawing frames around the banner.** They add pixels and
  no facts, they break at every resolution the firmware might negotiate, and they are
  the single most common way a hobby OS screenshot announces itself as a toy.
- **No progress bars, spinners or percentages.** Nothing here takes long enough to
  need one, so one would be decoration pretending to be feedback.
- **No full-screen colour flood on panic.** A red screen with light text on it is both
  alarming and hard to read, and the pairing fails contrast (see below). The panic
  screen keeps the normal surface and uses one red band. The signal comes from
  structure, not from saturation.
- **No sixteen-colour ANSI free-for-all.** The console understands a deliberately
  restricted ANSI set that maps onto the six roles below. A full palette would let any
  future code paint anything, and the intent above would erode one commit at a time.
- **No marketing voice.** The banner states facts and nothing else. It does not
  describe itself as blazing, minimal or secure, and it does not congratulate the
  reader on booting it. The PRD makes the claims, CI proves them, and the screen
  reports numbers.

## Type

One family, because there is one and it is the right one.

- **Family:** Noto Sans Mono, as pre-rasterised bitmaps from the
  `noto-sans-mono-bitmap` crate. No font parsing, no hinting engine, no rasteriser at
  runtime; the glyphs arrive as anti-aliased coverage bitmaps and the console blends
  them against the surface colour. This is the "least machinery" pillar applied to
  type: a TrueType renderer in the kernel would be several thousand lines of
  attack surface to make the letter A slightly rounder.
- **Body:** `RasterHeight::Size16`, `FontWeight::Regular`. Everything the shell prints,
  every initialisation line, all command output.
- **Banner and the panic heading:** `RasterHeight::Size24`, `FontWeight::Regular`. Two
  sizes total. A third would be a decision no one can defend.
- **Weight** carries emphasis where emphasis is needed — the prompt string, the
  subsystem labels in the boot lines — rather than colour doing that job alone.
- **Advance width is read from the font crate**, via its raster-width accessor, never
  hard-coded. A hard-coded character cell is the same class of bug as a hard-coded
  pixel format, and it fails the same way: silently, on the firmware that was not
  tested.
- **Line length:** the console wraps at the column count the resolution allows, and
  output is written to sit comfortably under about 100 columns so it does not wrap on
  the smallest resolution a firmware is likely to negotiate. Wrapped, never truncated:
  a truncated hexadecimal address is worse than a wrapped one.
- **Leading:** two pixels below the raster height, so 18-pixel rows for body text. Dense
  without lines touching.

## Colour

Seven roles, and the list is closed: a new colour requires a new row here, with its
contrast measured, before it is added to the code. Values are given because a role
without a value cannot be contrast-checked, and every pair below has been checked
with the sRGB relative-luminance formula rather than estimated by eye.

| Role | Value | Used for | Contrast on surface |
|---|---|---|---|
| **Surface** | `#101014` | The background, everywhere, including the panic screen | n/a |
| **Text** | `#d8d8d8` | All body text, all command output | **13.32:1** |
| **Muted** | `#8a8a95` | Subsystem labels, units, secondary detail in `sysinfo` and `mem` | **5.56:1** |
| **Accent** | `#4fc3e8` | The prompt string, the banner's product name, values worth finding in a table | **9.31:1** |
| **Warning** | `#e8b44f` | Degraded-but-working conditions only: the no-framebuffer fallback notice, dropped keystrokes, a self-test phase deliberately skipped | **10.01:1** |
| **Danger** | `#e84f4f` | The panic band, the `error:` prefix, a `FAILED:` self-test phase | **5.12:1** |
| **OK** | `#7bc47f` | A passing self-test phase, and nothing else | **9.08:1** |

Surface, text, accent, warning and danger are implemented in `kernel/src/framebuffer.rs`
and the values above are that file's values. Muted and OK are specified here and land
with the milestones that need them; when they do, the values above are the values.

Notes that are binding, not advisory:

- **Surface is near-black, not black.** `#101014` has a trace of blue in it, which
  reads as deliberate rather than as an absent signal, and it takes the edge off the
  contrast with light text during a long session. Pure `#000000` is not used.
- **The 4.5:1 floor genuinely applies**, and it is not decorative here. The danger
  role at 5.12:1 is the tightest pairing on screen and it clears the floor; the muted
  role at 5.56:1 is next. Nothing below 4.5:1 ships. There is no "it is only a debug
  console" exemption, because the debug console is the entire interface.
- **One forbidden pairing, stated explicitly:** text `#d8d8d8` on danger `#e84f4f` is
  **2.6:1** and must never appear. Where the danger colour is used as a fill, which
  is only ever the panic band, the text on it is the surface colour `#101014`, which
  is **5.12:1**. This is why the panic screen has a band and not a flooded
  background: the flooded version cannot be made to pass.
- **Colour is never the only signal.** Every role above is paired with a textual
  marker: `error:`, `FAILED:`, `PASSED:`, `KERNEL PANIC`. The serial log carries the
  same words with no colour at all, and CI reads the serial log, so if the words ever
  stopped carrying the meaning, the boot gate would be the first thing to notice.

## Spacing and layout

- **Scale:** multiples of 4 pixels. Only 4, 8, 12 and 24 are used.
- **Margins:** a 4-pixel border on all four sides of the text area, preserved by
  scrolling; the banner block is inset a further 8 pixels. Text never begins at pixel
  zero, because a character touching the bezel reads as a rendering fault rather than
  as a layout choice.
- **Structure:** a single vertical stack, one line at a time, growing downwards and
  scrolling when it reaches the bottom. There is no grid, no column layout, no panel.
- **The boot block** is: banner at Size16-over-Size24, one blank line, the
  initialisation lines with a fixed-width label column, one blank line, then the
  prompt. Those two blank lines are the whole of the layout system and they are what
  stops the screen becoming the wall of noise the intent forbids.
- **Scrolling** moves the character grid up by one row in RAM and redraws; content
  does not reflow, and the prompt stays on the bottom line.

## Components touched

- **`framebuffer::PixelWriter`** — new, and the only thing that writes pixels. Every
  other component draws through it.
- **`console::Console`** — new: the character grid, glyph blending, scrolling, the
  restricted ANSI handling and the colour roles above.
- **The panic renderer reuses `Console`'s glyph path rather than duplicating it.** A
  second renderer that exists only because the first one might be locked would be a
  near-duplicate component, which is a defect. Instead the panic path recovers the
  console (the machine is already stopping, and no other code will run again) and
  draws through the same code. That recovery is an unsafe operation with a stated
  invariant; it is listed in the [TDD](TDD.md) unsafe inventory and must not be
  copied anywhere else.
- **The serial writer** carries the same text with the colour stripped. It is not a
  second design; it is the same content with one dimension removed, and the fact that
  it survives that removal is the accessibility argument above.

## States

Most of the template's interaction states do not exist on a text console, and saying
which and why is more useful than inventing them.

| State | Osmium's form |
|---|---|
| **Hover** | Does not exist. There is no pointer. |
| **Focus** | One focus point, always: the caret at the prompt. A solid block in the accent colour, drawn at the cursor position, never blinking, never hidden. There is nothing to tab between and nothing that can take focus away. |
| **Active** | The instant a character is echoed at the caret. No press state, because there is no widget being pressed. |
| **Disabled** | Does not exist. Every command is always available; a command that cannot do its job says so in words when it is run, rather than being greyed out in advance. |
| **Loading** | Effectively does not exist: every command completes far inside a frame. The boot sequence is the one genuinely progressive thing, and it shows progress by printing the subsystem it just brought up, not by animating. |
| **Error** | The `error:` prefix in the danger colour, on the surface, followed by what was wrong and what to do. Inline, beneath the command, and the prompt returns immediately. |
| **Panic** | The one full-screen state: a danger-filled band across the top with `KERNEL PANIC` in surface-coloured Size24 text, then the message, then the location, then the registers in muted colour, then the instruction to power-cycle. |

## Accessibility floor — non-negotiable

Adapted honestly to a bare-metal target. Where an item cannot apply, it says so and
says why, rather than being quietly ticked.

- **Contrast 4.5:1 for body text, 3:1 for large text and boundaries** — applies in
  full, checked above with real values, tightest pair 5.12:1.
- **Full keyboard operation, visible focus, sensible order** — applies in full, and is
  trivially satisfied because the keyboard is the only input and there is one focus
  point. The caret is always drawn.
- **Touch targets >= 44px** — not applicable. There is no touch input and no pointer
  of any kind.
- **Colour is never the only signal** — applies in full, enforced by the textual
  markers above, and continuously verified as a side effect of CI reading the
  colourless serial log.
- **Respects `prefers-reduced-motion`** — no equivalent setting exists at this level,
  so the stronger form is adopted instead: **nothing moves.** No animation, no blink,
  no transition. There is nothing for such a preference to switch off.
- **Works at 200% zoom** — no zoom exists. The equivalent constraint is that the
  console must remain legible at every resolution the firmware might hand over, from
  the smallest upwards, which is covered next.
- **The serial console is a genuine alternative interface**, carrying identical
  content to the screen in plain text, where a reader's own assistive technology
  works.

## Responsive

The "breakpoints" here are the framebuffer geometries the firmware negotiates, and
they are not under our control: BIOS VBE and UEFI GOP hand over different
resolutions, strides and pixel formats, which is precisely why both are boot-tested.

**What changes with resolution:** the number of columns and rows, computed at
initialisation from the reported width, height and the font's advance width; where
the text wraps; how many lines fit before scrolling starts.

**What does not change, at any resolution:** the two type sizes, the margins, the
spacing scale, the six colour roles, the wrap-never-truncate rule, and the order of
information in the banner and the panic screen. A reader who has seen one Osmium
screen has seen them all.

**The degenerate case is designed, not discovered:** when the firmware provides no
framebuffer at all, there is no screen to lay out. The console falls back to
serial-only at the conventional 80 columns, logs that it has done so, and the shell
remains fully usable. This is a supported configuration, not an error state.

## Done means

- [ ] Matches the intent: a screenshot reads as dense and calm, and a reader can find
      where the boot output ends and the prompt begins without looking twice.
- [ ] Every state above is implemented, including the empty prompt, the inline error,
      the no-framebuffer fallback and the panic screen.
- [ ] Contrast checked with the real values in this document, at the real rendered
      colours, including the forbidden text-on-danger pairing being absent.
- [ ] The keyboard path is walked end to end: type, correct with backspace, move
      within the line, recall from history, submit, and clear with Escape.
- [ ] Checked on **both firmwares**, at the resolutions each negotiates, and with the
      framebuffer absent.
- [ ] Checked at the pinned minimal RAM figure, since that is the configuration CI
      certifies and therefore the one the claims are about.
- [ ] The serial log, read on its own with no screen, still tells a complete story.
