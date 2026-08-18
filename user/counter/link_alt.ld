/* The SAME counter program at a second disjoint base inside the user window.
 * The self-test battery schedules two unyielding compute tasks against each
 * other to prove sustained round-robin; the same source linked twice is the
 * honest way to get two such programs without duplicating code. 0x540000 +
 * the parser's 64-page budget stays inside USER_IMAGE_END (0x600000) and
 * clear of the primary counter at 0x500000. */
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R+X */
    rodata PT_LOAD FLAGS(4); /* R   */
    data   PT_LOAD FLAGS(6); /* R+W */
}

SECTIONS
{
    . = 0x540000;
    .text : { *(.text .text.*) } :text
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    .bss (NOLOAD) : { *(.bss .bss.*) *(COMMON) } :data
    /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
