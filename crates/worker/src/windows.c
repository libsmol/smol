#include <windows.h>

/* dl_nt.obj, which defines these in the official build, stays out for its DllMain */
HMODULE PyWin_DLLhModule = NULL;
const char *PyWin_DLLVersionString = SMOL_WINVER;

/* _ssl was built against an OpenSSL with compression, while this one has none,
   so SSL_get_current_compression returns NULL and _ssl never calls this */
int COMP_get_type(const void *method) { return 0; }

/* _ctypes was built against libffi's DLL, so it reaches each function and type through __imp_ */
#define FUNCTION(name) extern int name(); void *__imp_##name = (void *)name;
#define TYPE(name) extern char ffi_type_##name; void *__imp_ffi_type_##name = &ffi_type_##name;

FUNCTION(ffi_call)
FUNCTION(ffi_prep_cif)
FUNCTION(ffi_prep_cif_var)
FUNCTION(ffi_prep_closure)
FUNCTION(ffi_prep_closure_loc)
FUNCTION(ffi_closure_alloc)
FUNCTION(ffi_closure_free)

TYPE(void)
TYPE(uint8)
TYPE(sint8)
TYPE(uint16)
TYPE(sint16)
TYPE(uint32)
TYPE(sint32)
TYPE(uint64)
TYPE(sint64)
TYPE(float)
TYPE(double)
TYPE(longdouble)
TYPE(pointer)
