/*
 * Flat C bridge over the Sony Camera Remote SDK (CrSDK, C++ with async callbacks).
 *
 * The CrSDK exposes abstract C++ classes (ICrCameraObjectInfo, IDeviceCallback)
 * and delivers connection / download events through virtual callbacks on
 * SDK-owned threads. Rust can't bind that directly, so this bridge wraps a single
 * camera session behind an opaque handle and exposes a flat, synchronous C API:
 * connect blocks until OnConnected, capture blocks until OnCompleteDownload.
 *
 * Every function here is meant to be called from ONE dedicated OS thread (the Rust
 * "sony-sdk" actor thread); the bridge does no locking across cameras. The SDK's
 * own callback threads only touch a session's atomics / condition variables, which
 * are internally synchronised.
 *
 * String fields are UTF-8. Returned image buffers are malloc'd and must be freed
 * with sn_free().
 */
#ifndef TOUCAN_SONY_BRIDGE_H
#define TOUCAN_SONY_BRIDGE_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#define SN_MAX_MODEL      128
#define SN_MAX_ID         256
#define SN_MAX_CONN        32
#define SN_MAX_DEVICES     32
#define SN_MAX_PARAMS     256
#define SN_MAX_OPTIONS    512

/* Return codes shared with the Rust side. */
#define SN_OK               0
#define SN_ERR            (-1)
#define SN_NOT_READY        2   /* live view frame not available yet */
#define SN_ERR_AF           3   /* sn_capture: autofocus could not lock, no shot */

/* sn_connect: the camera is not on the bus (no CrError covers this). */
#define SN_CONNECT_NOT_FOUND 0xFFFFFFFFu

typedef struct SnDeviceInfo {
    char model[SN_MAX_MODEL];   /* e.g. "ILCE-7M4" */
    char id[SN_MAX_ID];         /* native id: hex of the SDK device id bytes */
    char conn_type[SN_MAX_CONN];/* "USB" / "IP" */
} SnDeviceInfo;

/* One camera property, raw values only — labels are decoded on the Rust side. */
typedef struct SnParam {
    uint32_t code;                    /* CrDevicePropertyCode */
    uint64_t current;                 /* GetCurrentValue() (raw, zero-extended) */
    int32_t  writable;                /* IsSetEnableCurrentValue() */
    uint32_t value_type;              /* CrDataType */
    int32_t  num_options;             /* entries used in `options` */
    int64_t  options[SN_MAX_OPTIONS]; /* possible values (raw, zero-extended) */
} SnParam;

/* --- SDK lifecycle (call once each) --- */
int  sn_init(void);
void sn_release(void);

/* Enumerate connected cameras. `time_in_sec` is the SDK enumeration floor: pass 3
 * for a thorough scan (discovering a cold / freshly plugged body), 1 for a fast
 * refresh of already-known cameras. Returns count (>=0) or SN_ERR. */
int  sn_list_devices(SnDeviceInfo* out, int capacity, int time_in_sec);

/* Open a session and block until connected. Returns an opaque handle, or NULL on
 * failure — in which case *err (if non-NULL) carries the CrError explaining why:
 * SN_CONNECT_NOT_FOUND when the body is not on the bus, CrError_Connect_TimeOut
 * (0x8208) when it is there but refuses the session, etc. */
void* sn_connect(const char* native_id, uint32_t* err);

/* Close a session and free the handle. */
void  sn_disconnect(void* cam);

/* 1 while the session is usable, 0 once the SDK reported the device gone
 * (OnDisconnected). A dead handle answers CrError_Api_InvalidCalled to every call. */
int   sn_is_alive(void* cam);

/* Fill `out` with the device's properties. Returns count (>=0) or SN_ERR. */
int  sn_get_parameters(void* cam, SnParam* out, int capacity);

/* Set one property; the bridge looks up the value type. Returns SN_OK or a CrError. */
int  sn_set_parameter(void* cam, uint32_t code, uint64_t value);

/* Toggle ISO Auto. enable!=0 selects ISO AUTO; enable==0 leaves auto by setting
 * the lowest concrete ISO offered by the body. Returns SN_OK or a CrError. */
int  sn_set_iso_auto(void* cam, int enable);

/* Grab one live-view JPEG. On SN_OK, *out is a malloc'd buffer of *size bytes. */
int  sn_get_live_view(void* cam, uint8_t** out, uint32_t* size);

/* Shoot one still and return its JPEG bytes (malloc'd). Blocks until downloaded. */
int  sn_capture(void* cam, uint8_t** out, uint32_t* size);

/* Free a buffer returned by sn_get_live_view / sn_capture. */
void sn_free(uint8_t* p);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* TOUCAN_SONY_BRIDGE_H */
