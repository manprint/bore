#include <mimalloc.h>

/* Return freed memory to the OS at once instead of after mimalloc's delay.
 * `set_default` leaves an explicit MIMALLOC_PURGE_DELAY in the environment in
 * charge. */
void bore_alloc_configure(void) {
  mi_option_set_default(mi_option_purge_delay, 0);
}
