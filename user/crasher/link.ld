/* Links the crasher at the SAME base as hello — deliberately: since M9 every
 * program owns its address space, and running the fault-isolation demo pair
 * (crasher + hello) at identical virtual addresses exercises that isolation
 * at the same time. Same explicit PHDRS arrangement as the other programs. */
ENTRY(_start)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R+X */
    rodata PT_LOAD FLAGS(4); /* R   */
    data   PT_LOAD FLAGS(6); /* R+W */
}

SECTIONS
{
    . = 0x400000;
    .text : { *(.text .text.*) } :text
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) } :rodata
    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    .bss (NOLOAD) : { *(.bss .bss.*) *(COMMON) } :data
    /DISCARD/ : { *(.eh_frame*) *(.comment) *(.note*) }
}
