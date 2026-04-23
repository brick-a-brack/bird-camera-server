#pragma once
#include <stddef.h>
#include <stdint.h>

#define WC_MAX_STR     256
#define WC_MAX_DEVICES  32

typedef struct {
    char unique_id[WC_MAX_STR];
    char name[WC_MAX_STR];
} WcDeviceInfo;

// List available video capture devices.
// Writes up to `capacity` entries into `out`. Returns the count written.
int wc_list_devices(WcDeviceInfo *out, int capacity);

// Open an AVCaptureSession for the given uniqueID.
// Returns an opaque handle, or NULL on failure.
void *wc_open_session(const char *unique_id);

// Stop and release a capture session.
void wc_close_session(void *handle);

// Copy the latest captured frame as JPEG into a heap-allocated buffer.
// The caller must release the buffer with wc_free_frame.
// Returns 0 on success, -1 on error.
int wc_capture_frame(void *handle, uint8_t **out_data, size_t *out_size);

// Free a buffer returned by wc_capture_frame.
void wc_free_frame(uint8_t *data);
