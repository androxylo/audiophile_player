// Kernel-mode header wrapper for bindgen (WDK 10.0.26100, KMDF 1.31, ACX 1.1)
#define _AMD64_ 1
#define AMD64 1
#define _WIN64 1
#define NTDDI_VERSION 0x0A00000C
#define _WIN32_WINNT 0x0A00
#define WINVER 0x0A00
#define WINNT 1
#define KMDF_VERSION_MAJOR 1
#define KMDF_VERSION_MINOR 31
#define KMDF_MINIMUM_VERSION_REQUIRED 31
#define ACX_VERSION_MAJOR 1
#define ACX_VERSION_MINOR 1
#define ACX_MINIMUM_VERSION_REQUIRED 1
#define POOL_NX_OPTIN_AUTO 1
#define NOBITMAP 1

#include <wdm.h>
#include <ntddk.h>
#include <windef.h>
#include <mmsystem.h>
#include <mmreg.h>
#include <ks.h>
#include <ksmedia.h>
#include <wdf.h>
#include <acx.h>

// Exported ntoskrnl routine returning the 15-byte ASCII process image filename
// (e.g. "TIDALPlayer.ex" or "audiodg.exe") used for hard zero-mixer enforcement.
NTKERNELAPI UCHAR* NTAPI PsGetProcessImageFileName(PEPROCESS Process);
