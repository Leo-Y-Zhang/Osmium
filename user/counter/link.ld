/* Links the counter program at the UPPER half of Osmium's user image window
 * (hello owns the base), so the two programs' pages are disjoint and both fit
 * inside the single 2 MiB window the page-table audit knows. Same explicit
 * PHDRS arrangement as hello's script: one page-aligned PT_LOAD per
 * permission class, ELF file headers kept out of the load segments. */
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R+X */
    rodata PT_LOAD FLAGS(4); /* R   */
    data   PT_LOAD FLAGS(6); /* R+W */
}

SECTIONS
{
    . = 0x500000;
    .text : { *(.text .text.*) } :text
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    .bss (NOLOAD) : { *(.bss .bss.*) *(COMMON) } :data
    /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
